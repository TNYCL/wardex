//! Spawning agent processes, optionally inside a transient systemd scope, and
//! reading their resource usage. See `docs/drivers.md`, "systemd scopes".

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::{Child, Command};

#[derive(Debug, Clone)]
pub struct Launcher {
    scope: bool,
    scope_command: PathBuf,
}

pub struct LaunchRequest<'a> {
    /// Unit name without the `.scope` suffix. Must be unique.
    pub unit: &'a str,
    pub slice: Option<&'a str>,
    pub program: &'a str,
    pub args: &'a [String],
    pub cwd: &'a Path,
}

pub struct Spawned {
    pub child: Child,
    pub pid: Option<u32>,
    /// Full unit name (`<unit>.scope`) when running in a scope.
    pub unit: Option<String>,
}

impl Launcher {
    /// Use systemd scopes when requested and available.
    pub fn new(systemd_scope: bool) -> Self {
        let scope = systemd_scope && systemd_user_available();
        if systemd_scope && !scope {
            tracing::warn!("systemd user manager not available; sessions run without scopes");
        }
        Self {
            scope,
            scope_command: "systemctl".into(),
        }
    }

    /// Spawn processes directly, without systemd.
    pub fn direct() -> Self {
        Self {
            scope: false,
            scope_command: "systemctl".into(),
        }
    }

    /// Inject a scope controller for temporary-process tests. Process launches
    /// remain direct; the injected controller only receives recorded unit ids.
    pub fn with_scope_command(program: PathBuf) -> Self {
        Self {
            scope: false,
            scope_command: program,
        }
    }

    /// Reap all descendants after the session leader exits. Verify the scope
    /// is inactive before a new generation can start alongside it.
    pub async fn cleanup_unit(
        &self,
        unit: &str,
        grace: std::time::Duration,
    ) -> std::io::Result<()> {
        self.scope_call(&["--user", "kill", "--signal=SIGTERM", unit])
            .await?;
        let deadline = tokio::time::Instant::now() + grace;
        while self.unit_active(unit).await? {
            if tokio::time::Instant::now() >= deadline {
                self.scope_call(&["--user", "kill", "--signal=SIGKILL", unit])
                    .await?;
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
                while self.unit_active(unit).await? {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(std::io::Error::other(
                            "scope still active after SIGKILL; stop its descendants before resuming",
                        ));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        Ok(())
    }

    async fn scope_call(&self, args: &[&str]) -> std::io::Result<std::process::Output> {
        let mut command = Command::new(&self.scope_command);
        command.args(args).kill_on_drop(true);
        tokio::time::timeout(std::time::Duration::from_secs(2), command.output())
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "scope controller timed out")
            })?
    }

    async fn unit_active(&self, unit: &str) -> std::io::Result<bool> {
        let output = self
            .scope_call(&[
                "--user",
                "show",
                "--property=LoadState,ActiveState,TasksCurrent",
                unit,
            ])
            .await?;
        let state = String::from_utf8_lossy(&output.stdout);
        if state
            .lines()
            .find_map(|line| line.strip_prefix("TasksCurrent="))
            .and_then(|tasks| tasks.parse::<u64>().ok())
            .is_some_and(|tasks| tasks > 0)
        {
            return Ok(true);
        }
        if state.lines().any(|line| {
            matches!(
                line,
                "LoadState=not-found" | "ActiveState=inactive" | "ActiveState=failed"
            )
        }) {
            return Ok(false);
        }
        if output.status.success() && state.lines().any(|line| line.starts_with("ActiveState=")) {
            return Ok(true);
        }
        Err(std::io::Error::other(
            "cannot verify scope state; inspect the user systemd manager before resuming",
        ))
    }

    pub fn uses_scope(&self) -> bool {
        self.scope
    }

    /// Spawn with piped stdin/stdout/stderr. With a scope, `systemd-run`
    /// execs the program, so the returned PID is the agent's PID.
    pub fn spawn(&self, req: &LaunchRequest<'_>) -> std::io::Result<Spawned> {
        let mut cmd = if self.scope {
            let mut c = Command::new("systemd-run");
            c.args([
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--expand-environment=no",
            ]);
            c.arg(format!("--unit={}", req.unit));
            if let Some(slice) = req.slice {
                c.arg(format!("--slice={slice}"));
            }
            c.arg("--").arg(req.program).args(req.args);
            c
        } else {
            let mut c = Command::new(req.program);
            c.args(req.args);
            c
        };
        cmd.current_dir(req.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = cmd.spawn()?;
        let pid = child.id();
        Ok(Spawned {
            child,
            pid,
            unit: self.scope.then(|| format!("{}.scope", req.unit)),
        })
    }

    /// Kill every process in a session's scope (best effort).
    pub async fn kill_unit(unit: &str) {
        let _ = Command::new("systemctl")
            .args(["--user", "kill", "--signal=SIGKILL", unit])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
}

fn systemd_user_available() -> bool {
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return false;
    };
    let runtime = PathBuf::from(runtime);
    let manager = runtime.join("systemd/private").exists() || runtime.join("bus").exists();
    manager && which("systemd-run")
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

/// Resource usage of a session process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    /// Resident memory in bytes (whole scope when scoped, else the process).
    pub rss_bytes: u64,
    /// Total CPU time in microseconds.
    pub cpu_usec: u64,
    /// Measured from the systemd scope's cgroup.
    pub scoped: bool,
}

/// Sample resource usage for a PID. Uses the process's cgroup when it is a
/// Wardex scope, so child processes (tools, builds) are included.
pub fn sample(pid: u32) -> Option<Usage> {
    if let Some(usage) = sample_cgroup(pid) {
        return Some(usage);
    }
    sample_proc(pid)
}

fn sample_cgroup(pid: u32) -> Option<Usage> {
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let path = cgroup
        .lines()
        .find_map(|l| l.strip_prefix("0::"))?
        .trim()
        .to_string();
    let leaf = path.rsplit('/').next()?;
    if !(leaf.starts_with("wardex-") && leaf.ends_with(".scope")) {
        return None;
    }
    let base = PathBuf::from("/sys/fs/cgroup").join(path.trim_start_matches('/'));
    let rss_bytes = std::fs::read_to_string(base.join("memory.current"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let cpu_usec = std::fs::read_to_string(base.join("cpu.stat"))
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("usage_usec "))?
        .trim()
        .parse()
        .ok()?;
    Some(Usage {
        rss_bytes,
        cpu_usec,
        scoped: true,
    })
}

fn sample_proc(pid: u32) -> Option<Usage> {
    let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    let rss_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesised command name; utime and stime are the
    // 14th and 15th fields overall.
    let after = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = after.split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    // Clock ticks are 100 Hz on Linux unless the kernel was built otherwise.
    Some(Usage {
        rss_bytes: rss_pages * 4096,
        cpu_usec: (utime + stime) * 10_000,
        scoped: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_own_process() {
        let usage = sample(std::process::id()).expect("usage of own process");
        assert!(usage.rss_bytes > 0);
    }

    /// Needs a systemd user manager; run with
    /// `cargo test -p wardex-core -- --ignored systemd`. The transient scope
    /// is collected when the process ends, so nothing is left behind.
    #[tokio::test]
    #[ignore]
    async fn systemd_scope_spawn_and_sample() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let launcher = Launcher::new(true);
        assert!(launcher.uses_scope(), "no systemd user manager");
        let unit = format!("wardex-selftest-{}", std::process::id());
        let mut spawned = launcher
            .spawn(&LaunchRequest {
                unit: &unit,
                slice: None,
                program: "sh",
                args: &["-c".into(), "echo pid=$$; exec cat".into()],
                cwd: Path::new("/"),
            })
            .unwrap();
        assert_eq!(spawned.unit.as_deref(), Some(&*format!("{unit}.scope")));
        let pid = spawned.pid.unwrap();
        let mut stdout = BufReader::new(spawned.child.stdout.take().unwrap()).lines();
        // `$$` reaches the shell unexpanded by systemd-run, and the PID we
        // hold is the agent's own.
        assert_eq!(
            stdout.next_line().await.unwrap(),
            Some(format!("pid={pid}"))
        );
        let mut stdin = spawned.child.stdin.take().unwrap();
        stdin.write_all(b"ping\n").await.unwrap();
        assert_eq!(stdout.next_line().await.unwrap().as_deref(), Some("ping"));
        let usage = sample(pid).unwrap();
        assert!(usage.scoped && usage.rss_bytes > 0);
        Launcher::kill_unit(&format!("{unit}.scope")).await;
        let status = spawned.child.wait().await.unwrap();
        assert!(!status.success());
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let out = std::process::Command::new("systemctl")
            .args([
                "--user",
                "list-units",
                "--all",
                "--no-legend",
                &format!("{unit}.scope"),
            ])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&out.stdout).trim().is_empty(),
            "scope left behind"
        );
    }

    #[tokio::test]
    async fn direct_spawn_pipes_stdio() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let launcher = Launcher::direct();
        let mut spawned = launcher
            .spawn(&LaunchRequest {
                unit: "wardex-test",
                slice: None,
                program: "cat",
                args: &[],
                cwd: Path::new("/"),
            })
            .unwrap();
        assert!(spawned.unit.is_none());
        let mut stdin = spawned.child.stdin.take().unwrap();
        let mut stdout = BufReader::new(spawned.child.stdout.take().unwrap()).lines();
        stdin.write_all(b"ping\n").await.unwrap();
        assert_eq!(stdout.next_line().await.unwrap().as_deref(), Some("ping"));
        drop(stdin);
        assert!(spawned.child.wait().await.unwrap().success());
    }
}
