//! HTTP endpoint. Only served when a token is configured, because tunnels
//! forward remote traffic to localhost (see `deploy/tunnel.md`). Phase 1
//! serves `/healthz`; the MCP endpoint joins it in Phase 2.

use std::net::SocketAddr;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::sync::watch;
use wardex_core::manager::Manager;

#[derive(Clone)]
struct AppState {
    manager: Manager,
}

pub async fn serve(
    addr: SocketAddr,
    manager: Manager,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let app = Router::new()
        .route("/healthz", get(healthz))
        .with_state(AppState { manager });
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "http listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            while shutdown.changed().await.is_ok() {
                if *shutdown.borrow() {
                    break;
                }
            }
        })
        .await
}

async fn healthz(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "live_sessions": state.manager.live_ids().len(),
    }))
}

/// Whether the request carries `Authorization: Bearer <token>`. Compares in
/// constant time.
pub fn authorized(headers: &HeaderMap, token: &str) -> bool {
    let Some(value) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(given) = value.strip_prefix("Bearer ") else {
        return false;
    };
    constant_time_eq(given.trim().as_bytes(), token.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_token_check() {
        let mut h = HeaderMap::new();
        assert!(!authorized(&h, "secret"));
        h.insert("authorization", "Bearer secret".parse().unwrap());
        assert!(authorized(&h, "secret"));
        assert!(!authorized(&h, "secret2"));
        assert!(!authorized(&h, "secre"));
        h.insert("authorization", "Basic secret".parse().unwrap());
        assert!(!authorized(&h, "secret"));
    }
}
