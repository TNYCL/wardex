//! End-to-end: the real `wardex` binary as daemon and CLI, with a shell
//! script replaying a recorded Claude session as the agent. Everything lives
//! in a temporary directory; no systemd scopes, no real agent CLIs.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_wardex");

struct Env {
    dir: tempfile::TempDir,
    config: PathBuf,
}

impl Env {
    fn new(fixture: &str) -> Self {
        Self::with_config(fixture, "")
    }

    /// `extra` is appended to the config (e.g. `[[webhooks]]` tables).
    fn with_config(fixture: &str, extra: &str) -> Self {
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
env_file = "{dir}/env"
tick_ms = 100

[resources]
systemd_scope = false

[drivers.claude]
bin = "{script}"

# Never start a real agent CLI from tests.
[drivers.codex]
bin = "{script}"
{extra}
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
        for key in [
            "WARDEX_SOCKET",
            "WARDEX_DATABASE",
            "WARDEX_TOKEN",
            "WARDEX_HTTP_LISTEN",
            "WARDEX_ALLOWED_HOSTS",
            "WARDEX_CLAUDE_BIN",
            "WARDEX_CODEX_BIN",
            "WARDEX_SYSTEMD_SCOPE",
        ] {
            c.env_remove(key);
        }
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
        let mut child = self
            .cmd(&["daemon"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_until(
            || {
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "daemon exited on start"
                );
                self.run(&["list"]).status.success()
            },
            "daemon readiness",
        );
        child
    }

    fn stop_daemon(&self, mut child: Child) {
        let out = self.run(&["daemon", "stop"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(wait_for_exit(&mut child).success());
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

fn wait_for_exit(child: &mut Child) -> ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if start.elapsed() >= Duration::from_secs(15) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("temporary daemon did not exit");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn assert_rejected(mut child: Child, message: &str) {
    assert!(!wait_for_exit(&mut child).success());
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains(message), "{err}");
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

fn duplicate_daemon_preserves_live_session(socket: &str) {
    let env = Env::new("question");
    let mut daemon = env.start_daemon();
    let out = env.json(&["run", "--driver", "claude", "--prompt", "ask me"]);
    let id = out["session"]["id"].as_str().unwrap();
    env.wait_state(id, "waiting_input");
    let pending = env.json(&["pending"]);
    let before = env.json(&["status", id]);
    let events = env.json(&["tail", id, "--since-seq", "0"]);
    assert_eq!(pending.as_array().unwrap().len(), 1);
    assert_eq!(
        events.as_array().unwrap().last().unwrap()["seq"],
        before["last_seq"]
    );

    let socket = env.dir.path().join(socket);
    let second = env
        .cmd(&["--socket", socket.to_str().unwrap(), "daemon"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    assert_rejected(second, "another daemon is already using this database");

    assert!(daemon.try_wait().unwrap().is_none());
    let after = env.json(&["status", id]);
    for field in [
        "state",
        "live",
        "pending",
        "last_seq",
        "resume_pending",
        "pid",
    ] {
        assert_eq!(after[field], before[field], "changed {field}");
    }
    // These read persisted rows, so in-memory state cannot hide recovery writes.
    assert_eq!(env.json(&["pending"]), pending);
    assert_eq!(env.json(&["tail", id, "--since-seq", "0"]), events);
    env.json(&["answer", id, "blue"]);
    env.wait_state(id, "done");
    assert_eq!(env.json(&["summary", id])["last_result"], "blue");
    let events = env.json(&["tail", id, "--since-seq", "0"]);
    assert!(
        events
            .as_array()
            .unwrap()
            .windows(2)
            .all(|e| e[0]["seq"].as_u64().unwrap() < e[1]["seq"].as_u64().unwrap())
    );
    env.stop_daemon(daemon);
}

#[test]
fn duplicate_daemon_on_same_socket_preserves_live_session() {
    duplicate_daemon_preserves_live_session("wardex.sock");
}

#[test]
fn duplicate_daemon_on_different_socket_preserves_live_session() {
    duplicate_daemon_preserves_live_session("other.sock");
}

#[test]
fn concurrent_daemons_have_one_database_owner() {
    let env = Env::new("question");
    let sockets = ["first.sock", "second.sock"].map(|name| env.dir.path().join(name));
    let barrier = std::sync::Barrier::new(2);
    let mut daemons = std::thread::scope(|scope| {
        let starts: Vec<_> = sockets
            .iter()
            .map(|socket| {
                let env = &env;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    env.cmd(&["--socket", socket.to_str().unwrap(), "daemon"])
                        .stdout(Stdio::null())
                        .stderr(Stdio::piped())
                        .spawn()
                        .unwrap()
                })
            })
            .collect();
        starts
            .into_iter()
            .map(|start| start.join().unwrap())
            .collect::<Vec<_>>()
    });
    let mut loser = None;
    wait_until(
        || {
            loser = daemons
                .iter_mut()
                .position(|child| child.try_wait().unwrap().is_some());
            loser.is_some()
        },
        "one daemon to be rejected",
    );
    let loser = loser.unwrap();
    assert_rejected(
        daemons.remove(loser),
        "another daemon is already using this database",
    );
    assert!(!sockets[loser].exists());
    let mut winner = daemons.pop().unwrap();
    let socket = sockets[1 - loser].to_str().unwrap();
    wait_until(|| sockets[1 - loser].exists(), "winning daemon socket");
    assert!(winner.try_wait().unwrap().is_none());
    assert_eq!(
        env.json(&["--socket", socket, "list"]),
        serde_json::json!([])
    );
    env.json(&["--socket", socket, "daemon", "stop"]);
    assert!(wait_for_exit(&mut winner).success());
}

#[test]
fn live_socket_is_rejected_before_opening_another_database() {
    let env = Env::new("question");
    let daemon = env.start_daemon();
    let second = Env::new("question");
    let socket = env.dir.path().join("wardex.sock");
    let child = second
        .cmd(&["--socket", socket.to_str().unwrap(), "daemon"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    assert_rejected(child, "another daemon is already listening on");
    assert!(!second.dir.path().join("state/wardex.db").exists());
    assert_eq!(env.json(&["list"]), serde_json::json!([]));
    env.stop_daemon(daemon);
}

#[test]
fn database_lock_is_released_after_shutdown() {
    let env = Env::new("question");
    let daemon = env.start_daemon();
    env.stop_daemon(daemon);
    assert!(env.dir.path().join("state/wardex.db.lock").exists());
    let daemon = env.start_daemon();
    assert_eq!(env.json(&["list"]), serde_json::json!([]));
    env.stop_daemon(daemon);
}

#[test]
fn database_lock_is_released_after_crash_and_sessions_are_recovered() {
    let env = Env::new("question");
    let mut daemon = env.start_daemon();
    let out = env.json(&["run", "--driver", "claude", "--prompt", "ask me"]);
    let id = out["session"]["id"].as_str().unwrap();
    env.wait_state(id, "waiting_input");
    assert_eq!(env.json(&["pending"]).as_array().unwrap().len(), 1);
    let before = env.json(&["status", id]);
    // Crash only this test's temporary daemon, leaving a stale socket and live rows.
    daemon.kill().unwrap();
    assert!(!wait_for_exit(&mut daemon).success());
    assert!(env.dir.path().join("wardex.sock").exists());

    let daemon = env.start_daemon();
    let recovered = env.json(&["status", id]);
    assert_eq!(recovered["state"], "orphaned");
    assert_eq!(recovered["live"], false);
    // Automatic resume is disabled, so startup clears its pending flag.
    assert_eq!(recovered["resume_pending"], false);
    assert_eq!(
        recovered["last_seq"].as_u64().unwrap(),
        before["last_seq"].as_u64().unwrap() + 2
    );
    assert!(env.json(&["pending"]).as_array().unwrap().is_empty());
    env.json(&["resume", id, "ask again"]);
    env.wait_state(id, "waiting_input");
    env.json(&["answer", id, "blue"]);
    env.wait_state(id, "done");
    env.stop_daemon(daemon);
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

/// `wardex mcp --stdio` as an MCP client would spawn it (e.g. over ssh):
/// tools forward to the daemon; without a daemon they report it.
#[test]
fn mcp_over_stdio() {
    use rmcp::ServiceExt;
    use rmcp::model::{CallToolRequestParams, CallToolResult};
    use rmcp::transport::TokioChildProcess;

    fn text(r: &CallToolResult) -> &str {
        &r.content[0].as_text().unwrap().text
    }

    let env = Env::new("question");
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let cmd = tokio::process::Command::from(env.cmd(&["mcp", "--stdio"]));
    let client = rt
        .block_on(().serve(TokioChildProcess::new(cmd).unwrap()))
        .expect("mcp handshake");
    let call = |tool: &str, args: Value| {
        let Value::Object(args) = args else { panic!() };
        rt.block_on(
            client.call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(args)),
        )
        .unwrap()
    };

    assert_eq!(rt.block_on(client.list_all_tools()).unwrap().len(), 13);
    let r = call("overview", serde_json::json!({}));
    assert_eq!(r.is_error, Some(true));
    assert!(text(&r).starts_with("unavailable:"), "{}", text(&r));

    let daemon = env.start_daemon();
    let cwd = env.dir.path().to_string_lossy().into_owned();
    let r = call(
        "run",
        serde_json::json!({"driver": "claude", "cwd": cwd, "prompt": "ask", "agent": "ssh-bot"}),
    );
    let id = serde_json::from_str::<Value>(text(&r)).unwrap()["session"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    env.wait_state(&id, "waiting_input");
    let pending: Value =
        serde_json::from_str(text(&call("pending_questions", serde_json::json!({})))).unwrap();
    let qid = pending[0]["question_id"].as_str().unwrap();
    let r = call(
        "answer",
        serde_json::json!({"id": id, "question_id": qid, "text": "blue"}),
    );
    assert_ne!(r.is_error, Some(true), "{}", text(&r));
    // Long-poll until the turn is done (the wait happens in the daemon).
    let mut since = 0;
    let mut done = false;
    for _ in 0..20 {
        let r = call(
            "tail",
            serde_json::json!({"id": id, "since_seq": since, "wait_secs": 5}),
        );
        for e in serde_json::from_str::<Value>(text(&r))
            .unwrap()
            .as_array()
            .unwrap()
        {
            since = e["seq"].as_u64().unwrap();
            done |= e["type"] == "done";
        }
        if done {
            break;
        }
    }
    assert!(done);
    env.wait_state(&id, "done");
    let summary: Value =
        serde_json::from_str(text(&call("summary", serde_json::json!({"id": id})))).unwrap();
    assert_eq!(summary["last_result"], "blue");
    assert_eq!(summary["agent"], "ssh-bot");

    // The CLI's `tail --wait` returns once the wait is over.
    let out = env.run(&["--json", "tail", &id, "--since-seq", "1000", "--wait", "1"]);
    assert!(out.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap(),
        serde_json::json!([])
    );

    rt.block_on(client.cancel()).unwrap();
    env.stop_daemon(daemon);
}

/// A tiny HTTP receiver on a thread: records each request body's event type.
fn hook_receiver() -> (String, std::sync::mpsc::Receiver<String>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut len = 0;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; len];
            let _ = reader.read_exact(&mut body);
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            if let Ok(v) = serde_json::from_slice::<Value>(&body) {
                let _ = tx.send(v["event"]["type"].as_str().unwrap_or("").to_string());
            }
        }
    });
    (url, rx)
}

/// Webhooks come from the env file; `daemon reload` and SIGHUP apply it
/// without a restart. `daemon stop` returns only once the daemon exited.
#[test]
fn reload_applies_env_file_webhooks() {
    let env = Env::with_config(
        "tool",
        "[[webhooks]]\nname = \"bot\"\nurl_env = \"TEST_HOOK_URL\"\nevents = [\"done\"]\n",
    );
    let (url, hooks) = hook_receiver();
    let mut daemon = env.start_daemon();
    let cwd = env.dir.path().to_string_lossy().into_owned();
    let run = || {
        let id = String::from_utf8(
            env.run(&["run", "--driver", "claude", "--cwd", &cwd, "--prompt", "go"])
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        env.wait_state(&id, "done");
        id
    };

    // No URL yet: nothing is sent.
    run();
    assert!(hooks.recv_timeout(Duration::from_millis(500)).is_err());

    std::fs::write(
        env.dir.path().join("env"),
        format!("# test\nTEST_HOOK_URL={url}\n"),
    )
    .unwrap();
    let reloaded = env.json(&["daemon", "reload"]);
    assert_eq!(reloaded["webhooks_active"], 1);
    assert_eq!(reloaded["restart_needed"], serde_json::json!([]));
    run();
    assert_eq!(hooks.recv_timeout(Duration::from_secs(10)).unwrap(), "done");

    // SIGHUP does the same: with the URL gone, nothing is sent any more.
    std::fs::write(env.dir.path().join("env"), "# no hooks\n").unwrap();
    let status = Command::new("kill")
        .args(["-HUP", &daemon.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        daemon.try_wait().unwrap().is_none(),
        "SIGHUP must not stop the daemon"
    );
    run();
    assert!(hooks.recv_timeout(Duration::from_millis(500)).is_err());

    // A changed section that cannot be applied live is reported.
    let config = std::fs::read_to_string(&env.config).unwrap();
    std::fs::write(
        &env.config,
        config.replace("tick_ms = 100", "tick_ms = 200"),
    )
    .unwrap();
    let reloaded = env.json(&["daemon", "reload"]);
    assert_eq!(reloaded["restart_needed"], serde_json::json!(["daemon"]));

    let out = env.run(&["daemon", "stop"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "daemon stopped\n");
    // Already exited when `stop` returned.
    let start = Instant::now();
    while daemon.try_wait().unwrap().is_none() {
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "daemon still running after stop"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `daemon restart` comes back in the same process with history intact.
#[test]
fn restart_keeps_the_process_and_history() {
    let env = Env::new("tool");
    let mut daemon = env.start_daemon();
    let before = env.json(&["list", "-a"]);
    let cwd = env.dir.path().to_string_lossy().into_owned();
    let out = env.run(&["run", "--driver", "claude", "--cwd", &cwd, "--prompt", "go"]);
    let id = String::from_utf8(out.stdout).unwrap().trim().to_string();
    env.wait_state(&id, "done");
    assert!(before.as_array().unwrap().is_empty());

    let out = env.run(&["daemon", "restart"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "daemon restarted\n");
    assert!(
        daemon.try_wait().unwrap().is_none(),
        "same process keeps running"
    );
    assert_eq!(env.json(&["list", "-a"])[0]["id"], id.as_str());
    env.stop_daemon(daemon);
}

/// Full tool input/output: left out of `tail` by default, shown with
/// `--full` and by `tool-output`.
#[test]
fn tool_output_is_stored_in_full() {
    let env = Env::new("tool");
    let daemon = env.start_daemon();
    let cwd = env.dir.path().to_string_lossy().into_owned();
    let out = env.run(&["run", "--driver", "claude", "--cwd", &cwd, "--prompt", "go"]);
    let id = String::from_utf8(out.stdout).unwrap().trim().to_string();
    env.wait_state(&id, "done");

    let events = env.json(&["tail", &id, "-n", "100"]);
    let tools: Vec<&Value> = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "tool_call")
        .collect();
    assert!(!tools.is_empty());
    assert!(
        tools
            .iter()
            .all(|e| e["payload"].get("output").is_none() && e["payload"].get("input").is_none())
    );
    let full = env.json(&["tail", &id, "-n", "100", "--full"]);
    let done = full
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "tool_call" && e["payload"]["status"] != "started")
        .unwrap();
    let output = done["payload"]["output"].as_str().unwrap();
    assert!(!output.is_empty());

    let seq = done["seq"].as_u64().unwrap().to_string();
    let tool = env.json(&["tool-output", &id, &seq]);
    assert_eq!(tool["output"], output);
    assert!(tool["input"].as_str().is_some_and(|i| i.starts_with('{')));
    assert!(tool["started_seq"].as_u64().unwrap() < tool["finished_seq"].as_u64().unwrap());
    let human = env.run(&["tool-output", &id, &seq]);
    assert!(String::from_utf8_lossy(&human.stdout).contains("--- output"));
    let bad = env.run(&["tool-output", &id, "1"]);
    assert!(!bad.status.success());

    let summary = env.json(&["summary", &id]);
    assert!(summary["last_turn_ms"].as_i64().unwrap() >= 0);
    let ended_turn = summary["last_turn_ended_ms"].as_i64().unwrap();
    assert_eq!(
        summary["duration_ms"].as_i64().unwrap(),
        ended_turn - summary["created_ms"].as_i64().unwrap()
    );
    env.stop_daemon(daemon);
}
