//! Process lifecycle tests use only temporary shell processes.

use std::time::Duration;

use wardex_core::driver::{DriverCommand, DriverEvent};
use wardex_core::process::{LaunchRequest, Launcher};
use wardex_drivers::runner::{Output, Protocol, spawn_with};

struct EmptyProtocol;

impl Protocol for EmptyProtocol {
    fn on_line(&mut self, _line: &str) -> Output {
        Output::default()
    }

    fn on_command(&mut self, _command: DriverCommand) -> Output {
        Output::default()
    }
}

async fn drain_without_implicit_kill(close_stdout_first: bool) {
    let dir = tempfile::tempdir().unwrap();
    let mut script = String::new();
    if !close_stdout_first {
        script.push_str("while IFS= read -r _; do :; done\n");
    }
    script
        .push_str("exec 1>&-\ntouch stdout-closed\nwhile [ ! -e release ]; do sleep 0.02; done\n");
    let script_path = dir.path().join("agent.sh");
    std::fs::write(&script_path, script).unwrap();
    let mut handle = spawn_with(
        &Launcher::direct(),
        &LaunchRequest {
            unit: "wardex-test-drain",
            slice: None,
            program: "sh",
            args: &[script_path.to_string_lossy().into_owned()],
            cwd: dir.path(),
            env: &[],
        },
        EmptyProtocol,
    )
    .unwrap();
    if close_stdout_first {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !dir.path().join("stdout-closed").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }
    handle.commands.send(DriverCommand::Drain).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !dir.path().join("stdout-closed").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    // Cross the runner's normal five-second exit deadline. Always clean up
    // before asserting, including when an implicit kill regresses.
    let premature_exit =
        tokio::time::timeout(Duration::from_millis(5200), handle.events.recv()).await;
    let _ = handle.commands.send(DriverCommand::Kill);
    if premature_exit.is_err() {
        let event = tokio::time::timeout(Duration::from_secs(2), handle.events.recv())
            .await
            .unwrap();
        assert!(matches!(event, Some(DriverEvent::Exited { .. })));
    }
    assert!(
        premature_exit.is_err(),
        "drain implicitly killed a process after stdout EOF: {premature_exit:?}"
    );
}

#[tokio::test]
async fn drain_does_not_kill_after_stdout_eof() {
    drain_without_implicit_kill(false).await;
}

#[tokio::test]
async fn drain_is_honored_when_stdout_already_closed() {
    drain_without_implicit_kill(true).await;
}
