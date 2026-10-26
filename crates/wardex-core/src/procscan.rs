//! Liveness of external sessions from `/proc` and the CLIs' own registry
//! files. Detection only: no process is ever signalled or traced, no file is
//! locked and no CLI daemon is contacted. Only processes of this user are
//! looked at. The `/proc` root is injectable for tests.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::DriverKind;
use crate::external::{ExternalSession, LiveEvidence, Liveness, normalize_cwd};

/// How deep the parent chain is followed to find excluded ancestors.
const MAX_ANCESTRY: usize = 64;

#[derive(Debug, Clone)]
pub struct ProbeConfig {
    /// Usually `/proc`.
    pub proc_root: PathBuf,
    /// Claude's root, for its live-session registry (`<root>/sessions`).
    pub claude_root: Option<PathBuf>,
    /// A transcript written this recently counts as `recent`.
    pub quiet_secs: u64,
}

#[derive(Debug, Clone)]
struct Process {
    ppid: u32,
    start_time: u64,
    argv: Vec<String>,
    cwd: Option<PathBuf>,
}

/// One look at the process table, reused for every session of a listing.
#[derive(Debug, Clone)]
pub struct ProcSnapshot {
    /// `Err` when `/proc` cannot be listed.
    procs: Result<HashMap<u32, Process>, String>,
    /// Claude `sessionId` -> (pid, procStart).
    registry: HashMap<String, (u32, String)>,
    quiet_secs: u64,
}

impl ProcSnapshot {
    /// Read the process table and registries. Processes in `exclude` (this
    /// daemon's own agents) and their descendants are left out.
    pub fn take(config: &ProbeConfig, exclude: &HashSet<u32>) -> Self {
        let procs = read_procs(&config.proc_root).map(|mut procs| {
            let excluded: HashSet<u32> = procs
                .keys()
                .copied()
                .filter(|pid| descends_from(&procs, *pid, exclude))
                .collect();
            procs.retain(|pid, _| !excluded.contains(pid));
            procs
        });
        let registry = config
            .claude_root
            .as_deref()
            .map(read_claude_registry)
            .unwrap_or_default();
        Self {
            procs,
            registry,
            quiet_secs: config.quiet_secs,
        }
    }

    /// The liveness of `session` at `now_ms`.
    pub fn liveness(&self, session: &ExternalSession, now_ms: i64) -> Liveness {
        let procs = match &self.procs {
            Ok(p) => p,
            Err(reason) => {
                return Liveness::Unknown {
                    reason: reason.clone(),
                };
            }
        };
        let age_secs = (now_ms - session.updated_ms).max(0) as u64 / 1000;
        let fresh = age_secs <= self.quiet_secs;
        let id = session.native_id.as_str();
        match session.driver {
            DriverKind::Claude => {
                if let Some((pid, start)) = self.registry.get(id)
                    && procs
                        .get(pid)
                        .is_some_and(|p| p.start_time.to_string() == *start)
                {
                    return Liveness::Live {
                        pid: *pid,
                        evidence: LiveEvidence::Registry,
                    };
                }
                if let Some(pid) = find(procs, |p| claude_args_name(&p.argv, id)) {
                    return Liveness::Live {
                        pid,
                        evidence: LiveEvidence::Cmdline,
                    };
                }
                if fresh {
                    return Liveness::Recent {
                        modified_secs_ago: age_secs,
                        pid: None,
                    };
                }
            }
            DriverKind::Codex => {
                let codex = |p: &Process| is_named(&p.argv, "codex");
                if let Some(pid) = find(procs, |p| codex(p) && p.argv.iter().any(|a| a == id)) {
                    return Liveness::Live {
                        pid,
                        evidence: LiveEvidence::Cmdline,
                    };
                }
                let cwd = normalize_cwd(&session.cwd);
                let in_cwd = find(procs, |p| codex(p) && p.cwd.as_deref() == Some(&*cwd));
                match (in_cwd, fresh) {
                    (Some(pid), true) => {
                        return Liveness::Live {
                            pid,
                            evidence: LiveEvidence::CwdRecent,
                        };
                    }
                    (pid @ Some(_), false) | (pid @ None, true) => {
                        return Liveness::Recent {
                            modified_secs_ago: age_secs,
                            pid,
                        };
                    }
                    (None, false) => {}
                }
            }
            // No external session store is read for these drivers.
            DriverKind::Grok | DriverKind::Agy => {}
        }
        Liveness::Inactive
    }
}

/// The lowest matching pid, so results do not depend on hash order.
fn find(procs: &HashMap<u32, Process>, pred: impl Fn(&Process) -> bool) -> Option<u32> {
    procs
        .iter()
        .filter(|(_, p)| pred(p))
        .map(|(pid, _)| *pid)
        .min()
}

fn descends_from(procs: &HashMap<u32, Process>, pid: u32, roots: &HashSet<u32>) -> bool {
    let mut current = pid;
    for _ in 0..MAX_ANCESTRY {
        if roots.contains(&current) {
            return true;
        }
        match procs.get(&current) {
            Some(p) if p.ppid != current && p.ppid != 0 => current = p.ppid,
            _ => return false,
        }
    }
    false
}

/// Whether one of the first arguments (the program, an interpreter's
/// script) is named like `name`.
fn is_named(argv: &[String], name: &str) -> bool {
    argv.iter().take(3).any(|a| {
        Path::new(a)
            .file_name()
            .and_then(|f| f.to_str())
            .is_some_and(|f| f.contains(name))
    })
}

/// `--resume <id>`, `-r <id>`, `--session-id <id>` or their `=` forms.
fn claude_args_name(argv: &[String], id: &str) -> bool {
    argv.windows(2)
        .any(|w| matches!(w[0].as_str(), "--resume" | "-r" | "--session-id") && w[1] == id)
        || argv.iter().any(|a| {
            a.strip_prefix("--resume=")
                .or_else(|| a.strip_prefix("--session-id="))
                == Some(id)
        })
}

fn read_procs(root: &Path) -> Result<HashMap<u32, Process>, String> {
    use std::os::unix::fs::MetadataExt;
    let entries =
        std::fs::read_dir(root).map_err(|e| format!("cannot list {}: {e}", root.display()))?;
    let uid = crate::external::own_uid();
    let mut out = HashMap::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let dir = entry.path();
        // Only this user's processes; others are none of Wardex's business.
        let Ok(meta) = std::fs::metadata(&dir) else {
            continue;
        };
        if uid.is_some_and(|u| u != meta.uid()) {
            continue;
        }
        let Some((ppid, start_time)) = std::fs::read_to_string(dir.join("stat"))
            .ok()
            .and_then(|s| parse_stat(&s))
        else {
            continue;
        };
        let argv = std::fs::read(dir.join("cmdline"))
            .map(|raw| {
                raw.split(|b| *b == 0)
                    .filter(|a| !a.is_empty())
                    .map(|a| String::from_utf8_lossy(a).into_owned())
                    .collect()
            })
            .unwrap_or_default();
        let cwd = std::fs::read_link(dir.join("cwd")).ok();
        out.insert(
            pid,
            Process {
                ppid,
                start_time,
                argv,
                cwd,
            },
        );
    }
    Ok(out)
}

/// `(ppid, starttime)` from `/proc/<pid>/stat`. The command name may contain
/// spaces and parentheses, so fields are counted after the last `)`.
fn parse_stat(stat: &str) -> Option<(u32, u64)> {
    let (_, rest) = stat.rsplit_once(')')?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // rest starts at field 3 (state); ppid is field 4, starttime field 22.
    let ppid = fields.get(1)?.parse().ok()?;
    let start = fields.get(19)?.parse().ok()?;
    Some((ppid, start))
}

/// `<claude_root>/sessions/<pid>.json`: only `*.json`; the `*.key` files
/// next to them are never opened.
fn read_claude_registry(claude_root: &Path) -> HashMap<String, (u32, String)> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(claude_root.join("sessions")) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json")
            || !entry.file_type().is_ok_and(|t| t.is_file())
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let (Some(session), Some(pid)) = (v["sessionId"].as_str(), v["pid"].as_u64()) else {
            continue;
        };
        let start = match &v["procStart"] {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => continue,
        };
        out.insert(session.to_string(), (pid as u32, start));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external::Liveness;

    const ID: &str = "11111111-1111-4111-8111-111111111111";

    struct FakeProc {
        dir: tempfile::TempDir,
    }

    impl FakeProc {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join("proc")).unwrap();
            std::fs::create_dir_all(dir.path().join("claude/sessions")).unwrap();
            std::fs::create_dir_all(dir.path().join("work/app")).unwrap();
            Self { dir }
        }

        fn add(&self, pid: u32, ppid: u32, start: u64, argv: &[&str], cwd: Option<&str>) {
            let p = self.dir.path().join("proc").join(pid.to_string());
            std::fs::create_dir_all(&p).unwrap();
            let mut fields = vec!["S".to_string(), ppid.to_string()];
            fields.extend((5..22).map(|i| i.to_string()));
            fields.push(start.to_string());
            fields.extend(["100".into(), "200".into()]);
            std::fs::write(
                p.join("stat"),
                format!("{pid} (node (x) y) {}\n", fields.join(" ")),
            )
            .unwrap();
            let mut cmdline = argv.join("\0");
            cmdline.push('\0');
            std::fs::write(p.join("cmdline"), cmdline).unwrap();
            if let Some(cwd) = cwd {
                std::os::unix::fs::symlink(self.dir.path().join(cwd), p.join("cwd")).unwrap();
            }
        }

        fn register(&self, pid: u32, session: &str, proc_start: &str) {
            std::fs::write(
                self.dir.path().join(format!("claude/sessions/{pid}.json")),
                format!(
                    r#"{{"pid":{pid},"sessionId":"{session}","procStart":"{proc_start}","kind":"interactive"}}"#
                ),
            )
            .unwrap();
            // Must never be read.
            std::fs::write(
                self.dir.path().join(format!("claude/sessions/{pid}.key")),
                "not json",
            )
            .unwrap();
        }

        fn config(&self) -> ProbeConfig {
            ProbeConfig {
                proc_root: self.dir.path().join("proc"),
                claude_root: Some(self.dir.path().join("claude")),
                quiet_secs: 120,
            }
        }

        fn session(&self, driver: DriverKind, updated_ms: i64) -> ExternalSession {
            ExternalSession {
                driver,
                native_id: ID.into(),
                cwd: self.dir.path().join("work/app"),
                title: None,
                created_ms: None,
                updated_ms,
                cli_version: None,
                entrypoint: None,
                forked_from: None,
                continued_in: None,
                size_bytes: 0,
                compressed: false,
                archived: false,
                subagent: false,
                managed_by: None,
                liveness: Liveness::not_probed(),
                path_hint: String::new(),
                warnings: Vec::new(),
                path: PathBuf::new(),
            }
        }

        fn probe(&self, session: &ExternalSession, exclude: &[u32]) -> Liveness {
            let exclude = exclude.iter().copied().collect();
            ProcSnapshot::take(&self.config(), &exclude).liveness(session, NOW)
        }
    }

    const NOW: i64 = 10_000_000;
    const OLD: i64 = NOW - 3_600_000;

    #[test]
    fn stat_parsing_survives_odd_command_names() {
        let stat = "42 (a) b (c)) S 7 42 42 0 -1 4194560 1 0 0 0 0 0 0 0 20 0 1 0 98765 0 0";
        assert_eq!(parse_stat(stat), Some((7, 98765)));
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn claude_registry_with_matching_start_time_is_live() {
        let f = FakeProc::new();
        f.add(300, 1, 5555, &["claude"], Some("work/app"));
        f.register(300, ID, "5555");
        let s = f.session(DriverKind::Claude, OLD);
        assert_eq!(
            f.probe(&s, &[]),
            Liveness::Live {
                pid: 300,
                evidence: LiveEvidence::Registry
            }
        );
    }

    #[test]
    fn a_reused_pid_is_not_live() {
        let f = FakeProc::new();
        // The registered process is gone; another one got its pid.
        f.add(300, 1, 9999, &["bash"], None);
        f.register(300, ID, "5555");
        assert_eq!(
            f.probe(&f.session(DriverKind::Claude, OLD), &[]),
            Liveness::Inactive
        );
    }

    #[test]
    fn claude_resume_in_the_command_line_is_live() {
        for args in [
            vec!["claude", "--resume", ID],
            vec!["node", "/opt/claude/cli.js", "-r", ID],
            vec!["claude", "-p", "--session-id", ID],
            vec!["claude", &format!("--resume={ID}")],
        ] {
            let f = FakeProc::new();
            f.add(400, 1, 1, &args, None);
            assert_eq!(
                f.probe(&f.session(DriverKind::Claude, OLD), &[]),
                Liveness::Live {
                    pid: 400,
                    evidence: LiveEvidence::Cmdline
                },
                "{args:?}"
            );
        }
    }

    #[test]
    fn a_fresh_transcript_alone_is_recent() {
        let f = FakeProc::new();
        f.add(500, 1, 1, &["claude", "--resume", "someone-else"], None);
        assert_eq!(
            f.probe(&f.session(DriverKind::Claude, NOW - 30_000), &[]),
            Liveness::Recent {
                modified_secs_ago: 30,
                pid: None
            }
        );
        assert_eq!(
            f.probe(&f.session(DriverKind::Claude, OLD), &[]),
            Liveness::Inactive
        );
    }

    #[test]
    fn codex_thread_id_in_arguments_is_live() {
        let f = FakeProc::new();
        f.add(600, 1, 1, &["codex", "resume", ID], None);
        // A wardex CLI mentioning the id is not a codex process.
        f.add(601, 1, 1, &["wardex", "adopt", ID], None);
        assert_eq!(
            f.probe(&f.session(DriverKind::Codex, OLD), &[]),
            Liveness::Live {
                pid: 600,
                evidence: LiveEvidence::Cmdline
            }
        );
    }

    #[test]
    fn codex_cwd_and_fresh_rollout() {
        let f = FakeProc::new();
        f.add(700, 1, 1, &["/usr/bin/codex"], Some("work/app"));
        assert_eq!(
            f.probe(&f.session(DriverKind::Codex, NOW - 5_000), &[]),
            Liveness::Live {
                pid: 700,
                evidence: LiveEvidence::CwdRecent
            }
        );
        // Only one of the two signals: recent.
        assert_eq!(
            f.probe(&f.session(DriverKind::Codex, OLD), &[]),
            Liveness::Recent {
                modified_secs_ago: 3600,
                pid: Some(700)
            }
        );
        let g = FakeProc::new();
        g.add(701, 1, 1, &["codex"], None);
        assert!(matches!(
            g.probe(&g.session(DriverKind::Codex, NOW - 5_000), &[]),
            Liveness::Recent { pid: None, .. }
        ));
        assert_eq!(
            g.probe(&g.session(DriverKind::Codex, OLD), &[]),
            Liveness::Inactive
        );
    }

    #[test]
    fn own_processes_and_their_children_are_ignored() {
        let f = FakeProc::new();
        // A Wardex-run codex app-server and its child in the same cwd.
        f.add(800, 1, 1, &["codex", "app-server"], Some("work/app"));
        f.add(801, 800, 1, &["codex", "exec", ID], Some("work/app"));
        let s = f.session(DriverKind::Codex, NOW - 1_000);
        assert!(f.probe(&s, &[]).is_live());
        assert_eq!(
            f.probe(&s, &[800]),
            Liveness::Recent {
                modified_secs_ago: 1,
                pid: None
            }
        );
    }

    #[test]
    fn unreadable_proc_is_unknown() {
        let f = FakeProc::new();
        let mut config = f.config();
        config.proc_root = f.dir.path().join("missing");
        let snap = ProcSnapshot::take(&config, &HashSet::new());
        assert!(matches!(
            snap.liveness(&f.session(DriverKind::Claude, OLD), NOW),
            Liveness::Unknown { .. }
        ));
    }

    #[test]
    fn real_proc_lists_this_process() {
        let snap = ProcSnapshot::take(
            &ProbeConfig {
                proc_root: "/proc".into(),
                claude_root: None,
                quiet_secs: 0,
            },
            &HashSet::new(),
        );
        let procs = snap.procs.as_ref().unwrap();
        assert!(procs.contains_key(&std::process::id()));
    }
}
