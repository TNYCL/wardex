//! HTTP endpoint: `/healthz` and the MCP endpoint `/mcp`. Only served when a
//! token is configured, because tunnels forward remote traffic to localhost
//! (see `deploy/tunnel.md`). `/mcp` always requires the bearer token.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use wardex_core::config::HttpConfig;
use wardex_core::manager::Manager;

use crate::mcp::WardexMcp;

const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

#[derive(Clone)]
struct AppState {
    manager: Manager,
    token: Arc<str>,
    limiter: Arc<FailureLimiter>,
}

/// The HTTP app for the given token: `/healthz` and `/mcp`.
pub fn router(manager: Manager, token: &str, shutdown: CancellationToken) -> Router {
    let http = manager.config().http.clone();
    let state = AppState {
        manager: manager.clone(),
        token: token.into(),
        limiter: Arc::new(FailureLimiter::new(http.auth_failures_per_minute)),
    };
    let mcp = StreamableHttpService::new(
        move || Ok(WardexMcp::new(manager.clone())),
        Arc::new(LocalSessionManager::default()),
        mcp_config(&http, shutdown),
    );
    let mut protected = Router::new().nest_service("/mcp", mcp);
    if state.manager.config().mcp_events.enabled {
        protected = protected.layer(middleware::from_fn(crate::mcp_compat::http_capabilities));
    }
    let protected = protected.layer(middleware::from_fn_with_state(state.clone(), require_token));
    Router::new()
        .route("/healthz", get(healthz))
        .merge(protected)
        .with_state(state)
}

/// Stateless Streamable HTTP with plain JSON responses: every tool call is
/// one request, nothing to lose across daemon restarts.
fn mcp_config(http: &HttpConfig, shutdown: CancellationToken) -> StreamableHttpServerConfig {
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_cancellation_token(shutdown);
    if http.allowed_hosts.iter().any(|h| h == "*") {
        config.disable_allowed_hosts()
    } else {
        config.with_allowed_hosts(
            LOOPBACK_HOSTS
                .iter()
                .map(|h| h.to_string())
                .chain(http.allowed_hosts.iter().cloned()),
        )
    }
}

pub async fn serve(
    addr: SocketAddr,
    manager: Manager,
    token: String,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let cancel = CancellationToken::new();
    let app = router(manager, &token, cancel.clone());
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "http listening (/healthz, /mcp)");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            while shutdown.changed().await.is_ok() {
                if *shutdown.borrow() {
                    break;
                }
            }
            cancel.cancel();
        })
        .await
}

/// Liveness without a token; the session count only with one.
async fn healthz(State(state): State<AppState>, headers: HeaderMap) -> Json<Value> {
    let mut body = json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
    });
    if authorized(&headers, &state.token) {
        body["live_sessions"] = json!(state.manager.live_ids().len());
    }
    Json(body)
}

async fn require_token(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if authorized(req.headers(), &state.token) {
        return next.run(req).await;
    }
    if !state.limiter.allow() {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "too many unauthorized requests\n",
        )
            .into_response();
    }
    tracing::debug!("unauthorized http request");
    let mut resp = (StatusCode::UNAUTHORIZED, "bearer token required\n").into_response();
    resp.headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    resp
}

/// Fixed one-minute window over failed authentications. Behind a tunnel every
/// request comes from localhost, so the limit is global rather than per IP.
struct FailureLimiter {
    per_minute: u32,
    window: Mutex<(Instant, u32)>,
}

impl FailureLimiter {
    fn new(per_minute: u32) -> Self {
        Self {
            per_minute,
            window: Mutex::new((Instant::now(), 0)),
        }
    }

    /// Count a failure; false once the window's budget is spent.
    fn allow(&self) -> bool {
        if self.per_minute == 0 {
            return true;
        }
        let mut w = self.window.lock().unwrap_or_else(|e| e.into_inner());
        if w.0.elapsed() >= Duration::from_secs(60) {
            *w = (Instant::now(), 0);
        }
        w.1 = w.1.saturating_add(1);
        w.1 <= self.per_minute
    }
}

/// Whether the request carries `Authorization: Bearer <token>`. Compares in
/// constant time.
pub fn authorized(headers: &HeaderMap, token: &str) -> bool {
    let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(given) = value.strip_prefix("Bearer ") else {
        return false;
    };
    constant_time_eq(given.trim().as_bytes(), token.as_bytes())
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(x ^ y);
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

    #[test]
    fn failure_limiter() {
        let l = FailureLimiter::new(2);
        assert!(l.allow());
        assert!(l.allow());
        assert!(!l.allow());
        assert!(FailureLimiter::new(0).allow());
    }
}
