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
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    /// Position in the config.
    pub index: usize,
    /// Label for logs (`name` from the config, else `webhooks[<index>]`);
    /// the URL is never logged.
    pub name: String,
    pub url: String,
    pub events: Vec<String>,
    /// Agent globs; empty matches every session.
    pub agents: Vec<String>,
    /// Task globs; empty matches every session.
    pub tasks: Vec<String>,
    /// Event types sent for every session regardless of `agents`/`tasks`.
    pub escalate: Vec<String>,
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
        let name = hook
            .name
            .clone()
            .unwrap_or_else(|| format!("webhooks[{index}]"));
        let Some(url) = get(&hook.url_env).filter(|u| !u.is_empty()) else {
            tracing::warn!(webhook = %name, env = %hook.url_env, "webhook disabled: URL env var not set");
            return None;
        };
        let secret = hook.hmac_secret_env.as_deref().and_then(&get);
        let headers = hook
            .headers
            .iter()
            .filter_map(|(header, env)| match get(env) {
                Some(v) => Some((header.clone(), v)),
                None => {
                    tracing::warn!(webhook = %name, header = %header, env = %env, "webhook header env var not set");
                    None
                }
            })
            .collect();
        Some(Self {
            index,
            name,
            url,
            events: hook.events.clone(),
            agents: hook.agents.clone(),
            tasks: hook.tasks.clone(),
            escalate: hook.escalate.clone(),
            secret,
            headers,
            timeout: Duration::from_secs(hook.timeout_secs.max(1)),
            max_attempts: hook.max_attempts.max(1),
            backoff: Duration::from_secs(1),
        })
    }

    /// A plain target: the given events of every session.
    pub fn new(url: impl Into<String>, events: &[&str]) -> Self {
        Self {
            index: 0,
            name: "webhooks[0]".into(),
            url: url.into(),
            events: events.iter().map(|e| e.to_string()).collect(),
            agents: Vec::new(),
            tasks: Vec::new(),
            escalate: Vec::new(),
            secret: None,
            headers: Vec::new(),
            timeout: Duration::from_secs(10),
            max_attempts: 5,
            backoff: Duration::from_secs(1),
        }
    }

    /// Whether this target gets `event`: an escalated type always; otherwise
    /// a listed type from a session matching the `agents` and `tasks` filters.
    /// A filter never matches a session without that label.
    pub fn wants(&self, event: &Event) -> bool {
        if event.kind.is_delta() {
            return false;
        }
        let kind = event.kind.name();
        if self.escalate.iter().any(|e| e == kind) {
            return true;
        }
        self.events.iter().any(|e| e == kind)
            && matches_any(&self.agents, event.agent.as_deref())
            && matches_any(&self.tasks, event.task.as_deref())
    }
}

fn matches_any(patterns: &[String], value: Option<&str>) -> bool {
    if patterns.is_empty() {
        return true;
    }
    value.is_some_and(|v| patterns.iter().any(|p| glob(p, v)))
}

/// `*` matches any run of characters, `?` one character; the rest literally.
pub fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Every configured webhook whose URL env var is set.
pub fn resolve_all(config: &Config, get: impl Fn(&str) -> Option<String>) -> Vec<Target> {
    config
        .webhooks
        .iter()
        .enumerate()
        .filter_map(|(i, h)| Target::resolve(i, h, &get))
        .collect()
}

/// Handle to the running dispatcher.
#[derive(Clone)]
pub struct Webhooks {
    reload: mpsc::UnboundedSender<Vec<Target>>,
}

impl Webhooks {
    /// Switch to a new set of targets between two events: unchanged targets
    /// keep their queue, removed ones finish what they have queued.
    pub fn replace(&self, targets: Vec<Target>) {
        let _ = self.reload.send(targets);
    }
}

struct Worker {
    target: Target,
    queue: mpsc::Sender<Value>,
}

impl Worker {
    fn start(target: Target) -> Self {
        let (queue, mut rx) = mpsc::channel::<Value>(QUEUE);
        let t = target.clone();
        tokio::spawn(async move {
            let client = match reqwest::Client::builder().timeout(t.timeout).build() {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(webhook = %t.name, error = %e, "cannot build http client");
                    return;
                }
            };
            // Ends once the dispatcher drops the worker and the queue is empty.
            while let Some(payload) = rx.recv().await {
                deliver(&client, &t, &payload).await;
            }
        });
        Self { target, queue }
    }
}

/// Start the dispatcher: one subscriber that routes each event to the
/// targets that want it, in order, with one delivery queue per target.
pub fn spawn(targets: Vec<Target>, manager: Manager) -> Webhooks {
    let (reload, mut reload_rx) = mpsc::unbounded_channel::<Vec<Target>>();
    let mut events = manager.subscribe();
    tokio::spawn(async move {
        let mut workers: Vec<Worker> = targets.into_iter().map(Worker::start).collect();
        loop {
            tokio::select! {
                biased;
                Some(targets) = reload_rx.recv() => {
                    workers = reconcile(workers, targets);
                    tracing::info!(webhooks = workers.len(), "webhooks reloaded");
                }
                received = events.recv() => match received {
                    Ok(event) => dispatch(&manager, &workers, event).await,
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "webhook dispatcher lagged");
                    }
                    Err(RecvError::Closed) => return,
                },
            }
        }
    });
    Webhooks { reload }
}

/// One target, every matching event (tests and simple setups).
pub fn spawn_target(target: Target, manager: Manager) -> Webhooks {
    spawn(vec![target], manager)
}

fn reconcile(mut old: Vec<Worker>, targets: Vec<Target>) -> Vec<Worker> {
    targets
        .into_iter()
        .map(|t| match old.iter().position(|w| w.target == t) {
            Some(i) => old.swap_remove(i),
            None => Worker::start(t),
        })
        .collect()
}

async fn dispatch(manager: &Manager, workers: &[Worker], mut event: Event) {
    let wanted: Vec<&Worker> = workers.iter().filter(|w| w.target.wants(&event)).collect();
    if wanted.is_empty() {
        return;
    }
    event.kind.strip_tool_text();
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
    for w in wanted {
        if w.queue.try_send(payload.clone()).is_err() {
            tracing::warn!(webhook = %w.target.name, "webhook queue full; event dropped");
        }
    }
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
                tracing::debug!(webhook = %target.name, event = %event_type, attempt, "webhook delivered");
                return true;
            }
            Ok(resp) => {
                let status = resp.status();
                let retryable = status.is_server_error() || status == 408 || status == 429;
                tracing::warn!(webhook = %target.name, event = %event_type, attempt, status = status.as_u16(), "webhook rejected");
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
                tracing::warn!(webhook = %target.name, event = %event_type, attempt, error = kind, "webhook failed");
                true
            }
        };
        if !retry || attempt == target.max_attempts {
            break;
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(MAX_BACKOFF);
    }
    tracing::error!(webhook = %target.name, event = %event_type, "webhook gave up");
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

    fn event(kind: &str, agent: Option<&str>, task: Option<&str>) -> Event {
        let kind = match kind {
            "done" => wardex_core::EventKind::Done {
                result: None,
                interrupted: false,
                note: None,
                git: None,
            },
            "error" => wardex_core::EventKind::Error {
                message: "boom".into(),
            },
            _ => wardex_core::EventKind::Prompt { text: "hi".into() },
        };
        Event {
            session_id: wardex_core::SessionId("s1".into()),
            seq: 1,
            ts_ms: 0,
            agent: agent.map(String::from),
            task: task.map(String::from),
            driver: wardex_core::DriverKind::Claude,
            kind,
        }
    }

    #[test]
    fn globs() {
        assert!(glob("bot-a", "bot-a"));
        assert!(!glob("bot-a", "bot-a2"));
        assert!(glob("fix-*", "fix-login"));
        assert!(glob("fix-*", "fix-"));
        assert!(!glob("fix-*", "feat-login"));
        assert!(glob("*-login", "fix-login"));
        assert!(glob("b?t", "bot"));
        assert!(!glob("b?t", "boot"));
        assert!(glob("*", ""));
        assert!(glob("a*b*c", "aXXbYYc"));
        assert!(!glob("a*b*c", "aXXbYY"));
    }

    #[test]
    fn filters_and_escalation() {
        let all = Target::new("u", &["done"]);
        assert!(all.wants(&event("done", Some("bot-a"), None)));
        assert!(all.wants(&event("done", None, None)));
        assert!(!all.wants(&event("error", Some("bot-a"), None)));

        let bot_a = Target {
            agents: vec!["bot-a".into()],
            ..Target::new("u", &["done", "error"])
        };
        assert!(bot_a.wants(&event("done", Some("bot-a"), None)));
        assert!(!bot_a.wants(&event("done", Some("bot-b"), None)));
        // A filter never matches a session without that label.
        assert!(!bot_a.wants(&event("done", None, None)));

        let fixes = Target {
            tasks: vec!["fix-*".into()],
            ..Target::new("u", &["done"])
        };
        assert!(fixes.wants(&event("done", Some("x"), Some("fix-login"))));
        assert!(!fixes.wants(&event("done", Some("x"), Some("feat"))));
        assert!(!fixes.wants(&event("done", Some("x"), None)));

        let oncall = Target {
            agents: vec!["oncall".into()],
            escalate: vec!["error".into()],
            ..Target::new("u", &["done"])
        };
        assert!(oncall.wants(&event("done", Some("oncall"), None)));
        assert!(!oncall.wants(&event("done", Some("bot-a"), None)));
        assert!(oncall.wants(&event("error", Some("bot-a"), None)));
        assert!(oncall.wants(&event("error", None, None)));
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
