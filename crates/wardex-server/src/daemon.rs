//! The daemon: wires the store, drivers and manager together and serves the
//! local socket, HTTP and webhooks until a signal or a `shutdown` request.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use wardex_core::Config;
use wardex_core::driver::Driver;
use wardex_core::manager::Manager;
use wardex_core::process::Launcher;
use wardex_core::store::{Store, StoreError};
use wardex_drivers::{ClaudeDriver, CodexDriver};

use crate::{http, socket, webhook};

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Socket(#[from] socket::SocketError),
    #[error(transparent)]
    Manager(#[from] wardex_core::manager::ManagerError),
    #[error("invalid daemon.http_listen {0:?}")]
    Listen(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Drivers for every supported CLI.
pub fn drivers(config: &Config) -> Vec<Arc<dyn Driver>> {
    vec![
        Arc::new(ClaudeDriver::new(config.drivers.claude.clone())),
        Arc::new(CodexDriver::new(config.drivers.codex.clone())),
    ]
}

pub async fn run(config: Config) -> Result<(), DaemonError> {
    let launcher = Launcher::new(config.resources.systemd_scope);
    run_with(config, drivers, launcher).await
}

/// Run with custom drivers (tests).
pub async fn run_with(
    config: Config,
    make_drivers: impl Fn(&Config) -> Vec<Arc<dyn Driver>>,
    launcher: Launcher,
) -> Result<(), DaemonError> {
    let socket_path = config.socket_path();
    let db_path = config.database_path();
    let store = Store::open(&db_path).await?;
    let manager = Manager::new(
        config.clone(),
        store.clone(),
        make_drivers(&config),
        launcher.clone(),
    );

    let orphaned = manager.recover().await?;
    if !orphaned.is_empty() {
        tracing::warn!(
            count = orphaned.len(),
            "marked sessions from the previous run as orphaned"
        );
    }

    let listener = socket::bind(&socket_path).await?;
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
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(socket::serve(
        listener,
        manager.clone(),
        shutdown_tx.clone(),
    ));
    {
        let m = manager.clone();
        tokio::spawn(async move { m.supervise().await });
    }
    webhook::spawn(&config, &manager);

    if config.token().is_some() {
        let addr: SocketAddr = config
            .daemon
            .http_listen
            .parse()
            .map_err(|_| DaemonError::Listen(config.daemon.http_listen.clone()))?;
        let m = manager.clone();
        let rx = shutdown_rx.clone();
        tokio::spawn(async move {
            if let Err(e) = http::serve(addr, m, rx).await {
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
    manager.shutdown(grace).await;
    let _ = std::fs::remove_file(&socket_path);
    store.close().await;
    tracing::info!("wardex daemon stopped");
    Ok(())
}
