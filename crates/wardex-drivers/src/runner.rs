//! Shared process loop: reads the agent's stdout line by line, feeds a
//! protocol state machine, writes its replies to stdin and forwards events.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc;
use wardex_core::EventKind;
use wardex_core::driver::{DriverCommand, DriverError, DriverEvent, DriverHandle};
use wardex_core::process::{LaunchRequest, Launcher};

const STDERR_TAIL: usize = 20;

/// What a protocol step produced.
#[derive(Debug, Default)]
pub struct Output {
    pub events: Vec<EventKind>,
    /// Lines to write to the agent's stdin (without trailing newline).
    pub writes: Vec<String>,
    /// Close stdin after writing (graceful end).
    pub close: bool,
}

impl Output {
    pub fn event(mut self, e: EventKind) -> Self {
        self.events.push(e);
        self
    }

    pub fn write(mut self, line: String) -> Self {
        self.writes.push(line);
        self
    }
}

/// A CLI protocol as a pure state machine.
pub trait Protocol: Send + 'static {
    /// Called once right after spawn.
    fn on_start(&mut self) -> Output {
        Output::default()
    }
    fn on_line(&mut self, line: &str) -> Output;
    /// `Shutdown`, `Drain` and `Kill` are handled by the runner.
    fn on_command(&mut self, cmd: DriverCommand) -> Output;
}

/// Spawn `program args` and drive it with `protocol`.
pub fn spawn_with<P: Protocol>(
    launcher: &Launcher,
    req: &LaunchRequest<'_>,
    protocol: P,
) -> Result<DriverHandle, DriverError> {
    let mut spawned = launcher.spawn(req).map_err(|source| DriverError::Spawn {
        program: req.program.to_string(),
        source,
    })?;
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    let stdin = spawned.child.stdin.take();
    let stdout = spawned.child.stdout.take();
    let stderr = spawned.child.stderr.take();
    let tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL)));
    if let Some(stderr) = stderr {
        let tail = tail.clone();
        let unit = req.unit.to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(unit = %unit, "agent stderr: {line}");
                let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
                if t.len() == STDERR_TAIL {
                    t.pop_front();
                }
                t.push_back(line);
            }
        });
    }
    let unit = spawned.unit.clone();
    let pid = spawned.pid;
    tokio::spawn(run_loop(
        protocol,
        spawned.child,
        stdin,
        stdout,
        cmd_rx,
        ev_tx,
        tail,
        unit.clone(),
    ));
    Ok(DriverHandle {
        pid,
        unit,
        commands: cmd_tx,
        events: ev_rx,
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_loop<P: Protocol>(
    mut protocol: P,
    mut child: Child,
    mut stdin: Option<ChildStdin>,
    stdout: Option<tokio::process::ChildStdout>,
    mut cmd_rx: mpsc::UnboundedReceiver<DriverCommand>,
    ev_tx: mpsc::UnboundedSender<DriverEvent>,
    tail: Arc<Mutex<VecDeque<String>>>,
    unit: Option<String>,
) {
    let emit = |out: &mut Output| {
        for e in out.events.drain(..) {
            let _ = ev_tx.send(DriverEvent::Event(e));
        }
    };
    let mut out = protocol.on_start();
    emit(&mut out);
    write_all(&mut stdin, &mut out).await;

    let mut lines = stdout.map(|s| BufReader::new(s).lines());
    let mut commands_open = true;
    let mut draining = false;
    loop {
        tokio::select! {
            line = async {
                match lines.as_mut() {
                    Some(l) => l.next_line().await,
                    None => Ok(None),
                }
            } => {
                match line {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let mut out = protocol.on_line(&line);
                        emit(&mut out);
                        write_all(&mut stdin, &mut out).await;
                    }
                    Ok(None) | Err(_) => break,
                }
            }
            cmd = cmd_rx.recv(), if commands_open => {
                match cmd {
                    Some(DriverCommand::Shutdown) => {
                        stdin = None;
                    }
                    Some(DriverCommand::Drain) => {
                        draining = true;
                        stdin = None;
                    }
                    Some(DriverCommand::Kill) => {
                        kill(&mut child, unit.as_deref()).await;
                    }
                    Some(cmd) => {
                        let mut out = protocol.on_command(cmd);
                        emit(&mut out);
                        write_all(&mut stdin, &mut out).await;
                    }
                    None => {
                        // The manager is gone; end the process gracefully.
                        commands_open = false;
                        stdin = None;
                    }
                }
            }
        }
    }

    // stdout closed: keep accepting drain/stop commands while reaping. A
    // drain must not turn EOF into an implicit SIGKILL five seconds later.
    drop(stdin);
    let mut deadline = (!draining).then(|| tokio::time::Instant::now() + Duration::from_secs(5));
    let status = loop {
        tokio::select! {
            status = child.wait() => break status.ok(),
            cmd = cmd_rx.recv(), if commands_open => {
                match cmd {
                    Some(DriverCommand::Drain) => deadline = None,
                    Some(DriverCommand::Kill) => kill(&mut child, unit.as_deref()).await,
                    None => commands_open = false,
                    _ => {}
                }
            }
            _ = async {
                match deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {
                kill(&mut child, unit.as_deref()).await;
                deadline = None;
            }
        }
    };
    // Let the stderr task drain.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let stderr_tail: Vec<String> = tail
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .cloned()
        .collect();
    let _ = ev_tx.send(DriverEvent::Exited {
        code: status.and_then(|s| s.code()),
        stderr_tail,
    });
}

async fn write_all(stdin: &mut Option<ChildStdin>, out: &mut Output) {
    if let Some(w) = stdin.as_mut() {
        for line in out.writes.drain(..) {
            let mut bytes = line.into_bytes();
            bytes.push(b'\n');
            if w.write_all(&bytes).await.is_err() || w.flush().await.is_err() {
                *stdin = None;
                break;
            }
        }
    } else {
        out.writes.clear();
    }
    if out.close {
        *stdin = None;
    }
}

async fn kill(child: &mut Child, unit: Option<&str>) {
    let _ = child.start_kill();
    if let Some(unit) = unit {
        Launcher::kill_unit(unit).await;
    }
}
