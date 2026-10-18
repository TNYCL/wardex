//! End-to-end: the real `wardex` binary as daemon and CLI, with a shell
//! script replaying a recorded Claude session as the agent. Everything lives
//! in a temporary directory; no systemd scopes, no real agent CLIs.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_wardex");

struct Env {
    dir: tempfile::TempDir,
    config: PathBuf,
}

impl Env {
    fn new(fixture: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let script = replay_script(fixture, dir.path());
        let config = dir.path().join("wardex.toml");
        std::fs::write(
            &config,
            format!(
                r#"
[daemon]
socket = "{dir}/wardex.sock"
database = "{dir}/state/wardex.db"
tick_ms = 100

[resources]
systemd_scope = false

[drivers.claude]
bin = "{script}"

# Never start a real agent CLI from tests.
[drivers.codex]
bin = "{script}"
"#,
                dir = dir.path().display(),
                script = script.display()
            ),
        )
        .unwrap();
        Self { dir, config }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.arg("--config").arg(&self.config).args(args);
        c.env_remove("WARDEX_SOCKET").env_remove("WARDEX_TOKEN");
        c.current_dir(self.dir.path());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let out = self.run(&all);
        assert!(
            out.status.success(),
            "wardex {args:?} failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn start_daemon(&self) -> Child {
        let child = self
            .cmd(&["daemon"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let sock = self.dir.path().join("wardex.sock");
        wait_until(|| sock.exists(), "daemon socket");
        child
    }

    fn stop_daemon(&self, mut child: Child) {
        let out = self.run(&["daemon", "stop"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let start = Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "daemon did not exit"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_state(&self, id: &str, state: &str) {
        wait_until(
            || self.json(&["status", id])["state"] == state,
            &format!("state {state}"),
        );
    }
}

fn wait_until(mut f: impl FnMut() -> bool, what: &str) {
    let start = Instant::now();
    while !f() {
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn replay_script(fixture: &str, dir: &Path) -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../wardex-drivers/tests/fixtures/claude")
        .join(format!("{fixture}.jsonl"));
    let mut script = String::from("#!/bin/sh\n");
    for line in std::fs::read_to_string(path).unwrap().lines() {
        let rec: Value = serde_json::from_str(line).unwrap();
        match rec["dir"].as_str().unwrap() {
            "out" => {
                let l = rec["msg"].to_string().replace('\'', "'\\''");
                script.push_str(&format!("printf '%s\\n' '{l}'\n"));
            }
            "in" => script.push_str("IFS= read -r _ || exit 0\n"),
            "exit" => script.push_str(&format!("exit {}\n", rec["msg"].as_i64().unwrap_or(0))),
            _ => {}
        }
    }
    let file = dir.join("fake-claude.sh");
    std::fs::write(&file, script).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
    file
}

#[test]
fn question_flow_survives_a_daemon_restart() {
    let env = Env::new("question");
    let daemon = env.start_daemon();

    let out = env.json(&[
        "run", "--driver", "claude", "--prompt", "ask me", "--agent", "bot-a", "--task", "t1",
    ]);
    let id = out["session"]["id"].as_str().unwrap().to_string();
    env.wait_state(&id, "waiting_input");

    // Follow the session in the background; it ends with the session.
    let tail = env
        .cmd(&["tail", &id, "-f"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let pending = env.json(&["pending"]);
    assert_eq!(pending.as_array().unwrap().len(), 1);
    let human = env.run(&["status", &id[..4]]);
    assert!(String::from_utf8_lossy(&human.stdout).contains("pending"));
    let overview = env.json(&["overview"]);
    assert_eq!(overview["counts"]["waiting_input"], 1);
    assert_eq!(overview["live"][0]["id"], id.as_str());
    let human = env.run(&["overview"]);
    let human = String::from_utf8_lossy(&human.stdout);
    assert!(human.contains("1 pending question"), "{human}");

    let ans = env.run(&["answer", &id, "blue"]);
    assert!(
        ans.status.success(),
        "{}",
        String::from_utf8_lossy(&ans.stderr)
    );
    env.wait_state(&id, "done");

    let tail = tail.wait_with_output().unwrap();
    let tail_text = String::from_utf8_lossy(&tail.stdout);
    for kind in ["question", "answered", "done", "ended"] {
        assert!(
            tail_text.contains(kind),
            "tail -f missing {kind}:\n{tail_text}"
        );
    }

    let summary = env.json(&["summary", &id]);
    assert_eq!(summary["last_result"], "blue");
    assert_eq!(summary["agent"], "bot-a");
    assert!(env.json(&["pending"]).as_array().unwrap().is_empty());
    assert!(env.json(&["list"]).as_array().unwrap().is_empty());
    assert_eq!(env.json(&["list", "--all"]).as_array().unwrap().len(), 1);

    env.stop_daemon(daemon);
    // Without a daemon the CLI explains what is wrong.
    let out = env.run(&["list"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("wardex daemon"));

    // History survives a restart.
    let daemon = env.start_daemon();
    let list = env.json(&["list", "--all"]);
    assert_eq!(list[0]["id"], id.as_str());
    assert_eq!(list[0]["state"], "done");
    let events = env.json(&["tail", &id, "--since-seq", "0"]);
    assert!(events.as_array().unwrap().len() >= 7);
    env.stop_daemon(daemon);
}

#[test]
fn errors_are_reported() {
    let env = Env::new("partial");
    let daemon = env.start_daemon();
    let out = env.run(&["status", "nope"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no session matches"));
    let out = env.run(&["--json", "answer", "nope", "x"]);
    assert!(!out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["code"], "not_found");
    let out = env.run(&[
        "run",
        "--driver",
        "claude",
        "--prompt",
        "x",
        "--slice",
        "nope.slice",
    ]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("resources.slices"));
    env.stop_daemon(daemon);
}

#[test]
fn run_prints_only_the_id_on_stdout() {
    let env = Env::new("partial");
    let daemon = env.start_daemon();
    let first = env.run(&["run", "--driver", "claude", "--prompt", "a"]);
    let second = env.run(&["run", "--driver", "claude", "--prompt", "b"]);
    let id1 = String::from_utf8_lossy(&first.stdout).trim().to_string();
    let id2 = String::from_utf8_lossy(&second.stdout).trim().to_string();
    assert_eq!(id1.len(), 8, "{id1:?}");
    assert_eq!(id2.len(), 8, "{id2:?}");
    // The same-cwd warning goes to stderr.
    let err = String::from_utf8_lossy(&second.stderr);
    if env.json(&["status", &id1])["live"] == true {
        assert!(err.contains("warning:"), "{err}");
    }
    env.stop_daemon(daemon);
}
