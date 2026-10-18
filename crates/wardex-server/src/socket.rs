//! Local API over a Unix socket: one JSON request line per connection, then
//! one JSON response line (or, for `tail --follow`, a stream of event lines).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::watch;
use wardex_core::manager::Manager;

use crate::api::{self, Request, Response, StreamItem};

#[derive(Debug, thiserror::Error)]
pub enum SocketError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("another daemon is already listening on {0}")]
    InUse(PathBuf),
}

/// Bind the socket, replacing a stale socket file. Only the owner can
/// connect (mode 0600).
pub async fn bind(path: &Path) -> Result<UnixListener, SocketError> {
    let io = |source| SocketError::Io {
        path: path.to_path_buf(),
        source,
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    if path.exists() {
        if UnixStream::connect(path).await.is_ok() {
            return Err(SocketError::InUse(path.to_path_buf()));
        }
        std::fs::remove_file(path).map_err(io)?;
    }
    let listener = UnixListener::bind(path).map_err(io)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(io)?;
    Ok(listener)
}

/// Serve until `shutdown` turns true. A `shutdown` request sets it.
pub async fn serve(listener: UnixListener, manager: Manager, shutdown_tx: watch::Sender<bool>) {
    let mut shutdown = shutdown_tx.subscribe();
    loop {
        tokio::select! {
            conn = listener.accept() => {
                match conn {
                    Ok((stream, _)) => {
                        let m = manager.clone();
                        let tx = shutdown_tx.clone();
                        let stop = shutdown_tx.subscribe();
                        tokio::spawn(async move {
                            if let Err(e) = connection(stream, m, tx, stop).await {
                                tracing::debug!(error = %e, "api connection ended");
                            }
                        });
                    }
                    Err(e) => tracing::warn!(error = %e, "accept failed"),
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return;
                }
            }
        }
    }
}

async fn connection(
    stream: UnixStream,
    manager: Manager,
    shutdown_tx: watch::Sender<bool>,
    mut stop: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let Some(line) = lines.next_line().await? else {
        return Ok(());
    };
    let req: Request = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            return send(
                &mut write,
                &Response::err("invalid", format!("bad request: {e}")),
            )
            .await;
        }
    };
    match req {
        Request::Tail {
            id,
            since_seq,
            last,
            follow: true,
            deltas,
        } => {
            // Subscribe before reading history so nothing falls in between.
            let mut rx = manager.subscribe();
            let id = match manager.resolve(&id).await {
                Ok(id) => id,
                Err(e) => return send(&mut write, &Response::from(e)).await,
            };
            let initial = match api::initial_events(&manager, &id, since_seq, last).await {
                Ok(ev) => ev,
                Err(e) => return send(&mut write, &Response::from(e)).await,
            };
            let mut last_seq = 0;
            for event in initial {
                last_seq = event.seq;
                let end = api::ends_stream(&event) && !manager.live_ids().contains(&id);
                send(&mut write, &StreamItem { event }).await?;
                if end {
                    return Ok(());
                }
            }
            let status = manager.status(&id).await;
            if matches!(status, Ok(s) if !s.live) {
                return Ok(());
            }
            loop {
                tokio::select! {
                    ev = rx.recv() => match ev {
                        Ok(event) => {
                            if event.session_id != id {
                                continue;
                            }
                            if event.kind.is_delta() {
                                if deltas {
                                    send(&mut write, &StreamItem { event }).await?;
                                }
                                continue;
                            }
                            if event.seq <= last_seq {
                                continue;
                            }
                            last_seq = event.seq;
                            let end = api::ends_stream(&event);
                            send(&mut write, &StreamItem { event }).await?;
                            if end {
                                return Ok(());
                            }
                        }
                        Err(RecvError::Lagged(n)) => {
                            tracing::warn!(skipped = n, "tail subscriber lagged");
                        }
                        Err(RecvError::Closed) => return Ok(()),
                    },
                    _ = stop.changed() => return Ok(()),
                }
            }
        }
        Request::Shutdown => {
            send(&mut write, &api::handle(&manager, Request::Shutdown).await).await?;
            let _ = shutdown_tx.send(true);
            Ok(())
        }
        req => send(&mut write, &api::handle(&manager, req).await).await,
    }
}

async fn send<T: serde::Serialize>(
    w: &mut tokio::net::unix::OwnedWriteHalf,
    value: &T,
) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await
}

type ResponseLines = tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>;

/// Client side of the socket API.
pub struct Client {
    path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot reach the wardex daemon at {path}: {source} (is `wardex daemon` running?)")]
    Connect {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("connection error: {0}")]
    Io(#[from] std::io::Error),
    #[error("bad response from daemon: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("the daemon closed the connection")]
    Closed,
}

impl Client {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    async fn connect(&self, req: &Request) -> Result<ResponseLines, ClientError> {
        let stream =
            UnixStream::connect(&self.path)
                .await
                .map_err(|source| ClientError::Connect {
                    path: self.path.clone(),
                    source,
                })?;
        let (read, mut write) = stream.into_split();
        let mut line = serde_json::to_vec(req)?;
        line.push(b'\n');
        write.write_all(&line).await?;
        write.flush().await?;
        // The server reads a single request line; closing our write half
        // only signals that nothing more is coming.
        drop(write);
        Ok(BufReader::new(read).lines())
    }

    /// Send a request and read its single response.
    pub async fn request(&self, req: &Request) -> Result<Response, ClientError> {
        let mut lines = self.connect(req).await?;
        let line = lines.next_line().await?.ok_or(ClientError::Closed)?;
        Ok(serde_json::from_str(&line)?)
    }

    /// Send a streaming request; calls `on_item` for each event until the
    /// stream ends. Returns an error response if the daemon sent one.
    pub async fn stream(
        &self,
        req: &Request,
        mut on_item: impl FnMut(StreamItem),
    ) -> Result<Option<Response>, ClientError> {
        let mut lines = self.connect(req).await?;
        while let Some(line) = lines.next_line().await? {
            let value: serde_json::Value = serde_json::from_str(&line)?;
            if value.get("event").is_some() {
                on_item(serde_json::from_value(value)?);
            } else {
                return Ok(Some(serde_json::from_value(value)?));
            }
        }
        Ok(None)
    }
}
