//! The daemon: wires the store, drivers and manager together and serves the
//! local socket, HTTP and webhooks until a signal or a `shutdown` request.
//! SIGHUP or a `reload` request re-reads the config and env files and
//! applies the webhook settings without a restart.

use std::fs::{File, OpenOptions};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fs2::FileExt;
use serde_json::json;
use tokio::signal::unix::{SignalKind, signal};
use wardex_core::Config;
use wardex_core::config::{ConfigError, Env};
use wardex_core::driver::Driver;
use wardex_core::external::ExternalSource;
use wardex_core::manager::{ExternalSetup, Manager};
use wardex_core::process::Launcher;
use wardex_core::store::{Store, StoreError};
use wardex_drivers::{ClaudeDriver, ClaudeStore, CodexDriver, CodexStore, GrokDriver};

use crate::api::Response;
use crate::socket::Control;
use crate::{http, socket, webhook};

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Socket(#[from] socket::SocketError),
    #[error(transparent)]
    Manager(#[from] wardex_core::manager::ManagerError),
    #[error("another daemon is already using this database: {0}")]
    DatabaseInUse(PathBuf),
    #[error("invalid daemon.http_listen {0:?}")]
    Listen(String),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// How the daemon ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Stopped,
    /// A `restart` was requested: the caller starts the daemon again.
    Restart,
}

/// Where the config comes from, so `reload` reads the same files.
#[derive(Debug, Clone, Default)]
pub struct Source {
    /// `--config`; `None` uses the default path.
    pub config_path: Option<PathBuf>,
    /// `--socket`, which wins over the files.
    pub socket: Option<String>,
}

impl Source {
    pub fn load(&self) -> Result<(Config, Env), ConfigError> {
        let (mut config, env) = Config::load_with_env(self.config_path.as_deref())?;
        if let Some(socket) = &self.socket {
            config.daemon.socket = socket.clone();
        }
        Ok((config, env))
    }
}

/// Drivers for every supported CLI.
pub fn drivers(config: &Config) -> Vec<Arc<dyn Driver>> {
    vec![
        Arc::new(ClaudeDriver::new(config.drivers.claude.clone())),
        Arc::new(CodexDriver::new(config.drivers.codex.clone())),
        Arc::new(GrokDriver::new(config.drivers.grok.clone())),
    ]
}

/// Readers of the CLIs' own session stores (used only with
/// `external.enabled`).
pub fn external_sources(config: &Config) -> Vec<Arc<dyn ExternalSource>> {
    let e = &config.external;
    vec![
        Arc::new(ClaudeStore::new(e.claude_root(), e.max_file_bytes())),
        Arc::new(CodexStore::new(e.codex_root(), e.max_file_bytes())),
    ]
}

pub async fn run(source: Source) -> Result<Exit, DaemonError> {
    let (config, env) = source.load()?;
    let launcher = Launcher::new(config.resources.systemd_scope);
    run_with(source, config, env, drivers, launcher).await
}

/// Run with custom drivers (tests).
pub async fn run_with(
    source: Source,
    config: Config,
    env: Env,
    make_drivers: impl Fn(&Config) -> Vec<Arc<dyn Driver>>,
    launcher: Launcher,
) -> Result<Exit, DaemonError> {
    let socket_path = config.socket_path();
    let db_path = config.database_path();
    let _database_lock = lock_database(&db_path)?;
    let listener = socket::bind(&socket_path).await?;
    let store = Store::open(&db_path).await?;
    let manager = Manager::with_external(
        config.clone(),
        store.clone(),
        make_drivers(&config),
        launcher.clone(),
        ExternalSetup {
            sources: external_sources(&config),
            ..Default::default()
        },
    );

    if config.mcp_events.enabled {
        store.check_event_permissions()?;
    }
    let orphaned = manager.recover().await?;
    if !orphaned.is_empty() {
        tracing::warn!(
            count = orphaned.len(),
            "marked sessions from the previous run as orphaned"
        );
    }

    let resumed = manager.auto_resume().await?;
    if !resumed.is_empty() {
        tracing::info!(count = resumed.len(), "resumed orphaned sessions");
    }
    tracing::info!(
        socket = %socket_path.display(),
        database = %db_path.display(),
        systemd_scope = launcher.uses_scope(),
        "wardex daemon started"
    );
    let (control, mut reload_rx) = Control::new();
    let shutdown_tx = control.shutdown.clone();
    let shutdown_rx = shutdown_tx.subscribe();
    tokio::spawn(socket::serve(listener, manager.clone(), control.clone()));
    {
        let m = manager.clone();
        tokio::spawn(async move { m.supervise().await });
    }
    let event_dispatcher = crate::mcp_events::spawn(manager.clone()).await?;
    let webhooks = webhook::spawn(
        webhook::resolve_all(&config, |k| env.get(k)),
        manager.clone(),
    );
    let token = config.token_from(&env);

    {
        let mut hup = signal(SignalKind::hangup())?;
        let mut running = config.clone();
        let token = token.clone();
        tokio::spawn(async move {
            loop {
                let reply = tokio::select! {
                    Some(tx) = reload_rx.recv() => Some(tx),
                    Some(()) = hup.recv() => {
                        tracing::info!("SIGHUP received");
                        None
                    }
                    else => return,
                };
                let resp = reload(&source, &mut running, &token, &webhooks);
                if let Some(tx) = reply {
                    let _ = tx.send(resp);
                }
            }
        });
    }

    if let Some(token) = token {
        let addr: SocketAddr = config
            .daemon
            .http_listen
            .parse()
            .map_err(|_| DaemonError::Listen(config.daemon.http_listen.clone()))?;
        let m = manager.clone();
        let rx = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(e) = http::serve(addr, m, token, rx).await {
                tracing::error!(error = %e, "http server failed");
            }
        });
    } else {
        tracing::warn!(
            env = %config.auth.token_env,
            "HTTP disabled: no token set (the HTTP/MCP endpoint always requires one)"
        );
    }

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut rx = shutdown_rx.clone();
    tokio::select! {
        _ = term.recv() => tracing::info!("SIGTERM received"),
        _ = int.recv() => tracing::info!("SIGINT received"),
        _ = async { while rx.changed().await.is_ok() { if *rx.borrow() { break; } } } => {
            tracing::info!("shutdown requested");
        }
    }
    let _ = shutdown_tx.send(true);
    let grace = Duration::from_secs(config.sessions.stop_grace_secs + 5);
    let shutdown_result = manager.shutdown(grace).await;
    let _ = std::fs::remove_file(&socket_path);
    event_dispatcher.shutdown().await;
    let close_result = store.close().await;
    match shutdown_result {
        Ok(report) => {
            for warning in report.warnings {
                tracing::warn!(%warning, "shutdown persistence or cleanup warning");
            }
            if !report.interrupted_sessions.is_empty() {
                tracing::warn!(sessions = ?report.interrupted_sessions, "shutdown interrupted sessions");
            }
        }
        Err(error) => tracing::warn!(%error, "shutdown completed with a control error"),
    }
    if let Err(error) = close_result {
        tracing::warn!(%error, "daemon exiting with uncommitted state; inspect database access before restart");
    }
    let exit = if control.restart_requested() {
        tracing::info!("wardex daemon stopped; restarting");
        Exit::Restart
    } else {
        tracing::info!("wardex daemon stopped");
        Exit::Stopped
    };
    Ok(exit)
}

/// Keep this file open until the store is closed. Never unlink it: another
/// daemon must lock the same inode, including after a stop or crash.
fn lock_database(path: &Path) -> Result<File, DaemonError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    file.try_lock_exclusive().map_err(|e| {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            DaemonError::DatabaseInUse(path.to_path_buf())
        } else {
            DaemonError::Io(e)
        }
    })?;
    Ok(file)
}

/// Re-read the config and env files. Webhooks switch over at once; other
/// changed sections are reported, since they only apply after a restart.
fn reload(
    source: &Source,
    running: &mut Config,
    token: &Option<String>,
    webhooks: &webhook::Webhooks,
) -> Response {
    let (new, env) = match source.load() {
        Ok(loaded) => loaded,
        Err(e) => {
            tracing::warn!(error = %e, "reload failed; keeping the current config");
            return Response::err("invalid", e.to_string());
        }
    };
    let targets = webhook::resolve_all(&new, |k| env.get(k));
    let active = targets.len();
    webhooks.replace(targets);
    let mut restart_needed: Vec<&str> = [
        ("daemon", running.daemon != new.daemon),
        ("auth", running.auth != new.auth),
        ("http", running.http != new.http),
        ("sessions", running.sessions != new.sessions),
        ("timeouts", running.timeouts != new.timeouts),
        ("retention", running.retention != new.retention),
        ("drivers", running.drivers != new.drivers),
        ("resources", running.resources != new.resources),
        ("external", running.external != new.external),
        ("mcp_events", running.mcp_events != new.mcp_events),
    ]
    .into_iter()
    .filter_map(|(name, changed)| changed.then_some(name))
    .collect();
    if *token != new.token_from(&env) {
        restart_needed.push("token");
    }
    running.webhooks = new.webhooks.clone();
    tracing::info!(
        webhooks = new.webhooks.len(),
        active,
        env_vars = env.file_len(),
        restart_needed = ?restart_needed,
        "config reloaded"
    );
    Response::ok(json!({
        "webhooks": new.webhooks.len(),
        "webhooks_active": active,
        "restart_needed": restart_needed,
    }))
}
