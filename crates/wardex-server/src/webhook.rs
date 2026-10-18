//! Webhook dispatcher: POSTs notable events to configured URLs with retry,
//! backoff and an optional HMAC-SHA256 signature. URLs, header values and
//! secrets come from env vars and are never logged.

use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use wardex_core::config::WebhookConfig;
use wardex_core::manager::Manager;
use wardex_core::{Config, Event};

const QUEUE: usize = 4096;
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// A webhook with its env values resolved.
#[derive(Clone)]
pub struct Target {
    /// Position in the config, used in logs instead of the URL.
    pub index: usize,
    pub url: String,
    pub events: Vec<String>,
    pub secret: Option<String>,
    pub headers: Vec<(String, String)>,
    pub timeout: Duration,
    pub max_attempts: u32,
    /// First retry delay; doubles per attempt.
    pub backoff: Duration,
}

impl Target {
    pub fn resolve(
        index: usize,
        hook: &WebhookConfig,
        get: impl Fn(&str) -> Option<String>,
    ) -> Option<Self> {
        let Some(url) = get(&hook.url_env).filter(|u| !u.is_empty()) else {
            tracing::warn!(webhook = index, env = %hook.url_env, "webhook disabled: URL env var not set");
            return None;
        };
        let secret = hook.hmac_secret_env.as_deref().and_then(&get);
        let headers = hook
            .headers
            .iter()
            .filter_map(|(name, env)| match get(env) {
                Some(v) => Some((name.clone(), v)),
                None => {
                    tracing::warn!(webhook = index, header = %name, env = %env, "webhook header env var not set");
                    None
                }
            })
            .collect();
        Some(Self {
            index,
            url,
            events: hook.events.clone(),
            secret,
            headers,
            timeout: Duration::from_secs(hook.timeout_secs.max(1)),
            max_attempts: hook.max_attempts.max(1),
            backoff: Duration::from_secs(1),
        })
    }

    fn wants(&self, event: &Event) -> bool {
        !event.kind.is_delta() && self.events.iter().any(|e| e == event.kind.name())
    }
}

/// Start one delivery worker per configured webhook.
pub fn spawn(config: &Config, manager: &Manager) {
    let targets: Vec<Target> = config
        .webhooks
        .iter()
        .enumerate()
        .filter_map(|(i, h)| Target::resolve(i, h, |k| std::env::var(k).ok()))
        .collect();
    for target in targets {
        spawn_target(target, manager.clone());
    }
}

pub fn spawn_target(target: Target, manager: Manager) {
    let (tx, mut rx) = mpsc::channel::<Value>(QUEUE);
    let mut events = manager.subscribe();
    let filter = target.clone();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    if !filter.wants(&event) {
                        continue;
                    }
                    let session = manager.status(&event.session_id).await.ok().map(|s| {
                        json!({
                            "id": s.session.id,
                            "state": s.session.state,
                            "driver": s.session.driver,
                            "agent": s.session.agent,
                            "task": s.session.task,
                            "cwd": s.session.cwd,
                            "driver_session_id": s.session.driver_session_id,
                            "pending_questions": s.pending.len(),
                        })
                    });
                    let payload = json!({"event": event, "session": session});
                    if tx.try_send(payload).is_err() {
                        tracing::warn!(webhook = filter.index, "webhook queue full; event dropped");
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(
                        webhook = filter.index,
                        skipped = n,
                        "webhook subscriber lagged"
                    );
                }
                Err(RecvError::Closed) => return,
            }
        }
    });
    tokio::spawn(async move {
        let client = match reqwest::Client::builder().timeout(target.timeout).build() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(webhook = target.index, error = %e, "cannot build http client");
                return;
            }
        };
        while let Some(payload) = rx.recv().await {
            deliver(&client, &target, &payload).await;
        }
    });
}

/// `sha256=<hex>` HMAC of the body.
pub fn signature(secret: &str, body: &[u8]) -> String {
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

async fn deliver(client: &reqwest::Client, target: &Target, payload: &Value) -> bool {
    let body = serde_json::to_vec(payload).unwrap_or_default();
    let event_type = payload["event"]["type"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let delivery = format!(
        "{}-{}",
        payload["event"]["session_id"].as_str().unwrap_or_default(),
        payload["event"]["seq"]
    );
    let mut delay = target.backoff;
    for attempt in 1..=target.max_attempts {
        let mut req = client
            .post(&target.url)
            .header("content-type", "application/json")
            .header("user-agent", concat!("wardex/", env!("CARGO_PKG_VERSION")))
            .header("x-wardex-event", &event_type)
            .header("x-wardex-delivery", &delivery);
        if let Some(secret) = &target.secret {
            req = req.header("x-wardex-signature", signature(secret, &body));
        }
        for (name, value) in &target.headers {
            req = req.header(name, value);
        }
        let retry = match req.body(body.clone()).send().await {
            Ok(resp) if resp.status().is_success() => {
                tracing::debug!(webhook = target.index, event = %event_type, attempt, "webhook delivered");
                return true;
            }
            Ok(resp) => {
                let status = resp.status();
                let retryable = status.is_server_error() || status == 408 || status == 429;
                tracing::warn!(webhook = target.index, event = %event_type, attempt, status = status.as_u16(), "webhook rejected");
                retryable
            }
            Err(e) => {
                // reqwest errors can contain the URL; log only the kind.
                let kind = if e.is_timeout() {
                    "timeout"
                } else if e.is_connect() {
                    "connect"
                } else {
                    "request"
                };
                tracing::warn!(webhook = target.index, event = %event_type, attempt, error = kind, "webhook failed");
                true
            }
        };
        if !retry || attempt == target.max_attempts {
            break;
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(MAX_BACKOFF);
    }
    tracing::error!(webhook = target.index, event = %event_type, "webhook gave up");
    false
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn signature_is_hex_hmac() {
        // RFC 4231 test case 2.
        assert_eq!(
            signature("Jefe", b"what do ya want for nothing?"),
            "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn resolve_reads_env_and_skips_missing_url() {
        let mut hook = WebhookConfig {
            url_env: "HOOK_URL".into(),
            hmac_secret_env: Some("HOOK_SECRET".into()),
            ..Default::default()
        };
        hook.headers = BTreeMap::from([("Authorization".into(), "HOOK_AUTH".into())]);
        let env = |k: &str| match k {
            "HOOK_URL" => Some("http://127.0.0.1:1/x".to_string()),
            "HOOK_SECRET" => Some("s".to_string()),
            "HOOK_AUTH" => Some("Bearer t".to_string()),
            _ => None,
        };
        let t = Target::resolve(0, &hook, env).unwrap();
        assert_eq!(t.secret.as_deref(), Some("s"));
        assert_eq!(
            t.headers,
            [("Authorization".to_string(), "Bearer t".to_string())]
        );
        assert!(Target::resolve(0, &hook, |_| None).is_none());
    }
}
