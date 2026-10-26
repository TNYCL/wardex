//! Optional MCP Events webhook profile. The daemon owns subscriptions and
//! delivery, including when MCP requests arrive through a short-lived stdio proxy.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use wardex_core::manager::Manager;
use wardex_core::mcp_events::{
    Arguments, EVENT_NAMES, Subscription, owner_principal, parse_cursor, timestamp,
};
use wardex_core::store::{Store, StoreError};
use wardex_core::time::now_ms;

use crate::api::Response;
use crate::callback::{self, Post, Transport};

// Wardex's existing one-token, owner-only-socket trust boundary. Never derive
// this from labels, untrusted _meta, or a claimed platform user identifier.
const VERIFICATION_CACHE_MS: i64 = 300_000;
static VERIFICATION_RATE: Mutex<Option<(Instant, u32)>> = Mutex::new(None);

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Webhook {
    pub mode: String,
    pub url: String,
    pub secret: String,
}
impl std::fmt::Debug for Webhook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Webhook { destination and secret redacted }")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Subscribe {
    pub name: String,
    pub arguments: Arguments,
    pub delivery: Webhook,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub ttl_ms: Option<u64>,
    #[serde(default)]
    pub max_age_ms: Option<u64>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Destination {
    pub mode: String,
    pub url: String,
}
impl std::fmt::Debug for Destination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Destination { redacted }")
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unsubscribe {
    pub name: String,
    pub arguments: Arguments,
    pub delivery: Destination,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct List {
    #[serde(default)]
    pub cursor: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    List(List),
    Subscribe(Subscribe),
    Unsubscribe(Unsubscribe),
}

fn error(code: i32, message: &str, reason: Option<&str>) -> Response {
    let mut response = Response::err(&code.to_string(), message);
    response.data = reason.map(|r| json!({"reason":r}));
    response
}
fn invalid() -> Response {
    error(-32602, "Invalid MCP Events parameters", None)
}
fn storage(error: StoreError) -> Response {
    let message = if matches!(error, StoreError::McpEventPermissions) {
        "MCP Events requires owner-only database and parent directory permissions"
    } else {
        "MCP Events persistence unavailable"
    };
    self::error(-32603, message, None)
}

fn subscription_id(principal: &str, name: &str, arguments: &Arguments, url: &str) -> String {
    let identity = serde_json::to_vec(&(principal, url, name, arguments)).unwrap_or_default();
    format!("sub_{}", hex::encode(Sha256::digest(identity)))
}

fn definitions() -> Value {
    let input = json!({"type":"object","properties":{
        "session_id":{"type":"string","minLength":1,"maxLength":256},
        "agent":{"type":"string","minLength":1,"maxLength":256},
        "task":{"type":"string","minLength":1,"maxLength":256},
        "all_sessions":{"type":"boolean","default":false}
    },"anyOf":[{"required":["session_id"]},{"required":["agent"]},{"required":["task"]},
       {"required":["all_sessions"],"properties":{"all_sessions":{"const":true}}}],"additionalProperties":false});
    let descriptions = [
        "A session asks a question. Fetch pending_questions and answer only the matching request occurrence.",
        "A session requests approval. Fetch its current request; delivery never grants approval.",
        "A turn finished. The process can remain alive and idle; fetch summary for the result.",
        "A session process ended, including idle shutdown, drain, stop and errors.",
        "A session or turn reported an error. Fetch current status for details.",
        "A previously pending input request needs attention. Recheck current pending questions.",
        "A running session needs attention. Fetch current state before acting.",
        "A native input request was resolved or cancelled. It no longer accepts an answer.",
    ];
    let events:Vec<_>=EVENT_NAMES.iter().zip(descriptions).map(|(name,description)| {
        let mut required=vec!["schema_version","session_id","seq","driver","agent","task"];
        if *name=="wardex.question" {required.extend(["request_id","request_seq","question_count"]);}
        if *name=="wardex.approval" {required.extend(["request_id","request_seq","approval_kind"]);}
        let payload=json!({"type":"object","properties":{
            "schema_version":{"const":1},"session_id":{"type":"string"},"seq":{"type":"integer","minimum":1},
            "driver":{"type":"string","enum":["claude","codex","grok"]},"agent":{"type":["string","null"]},"task":{"type":["string","null"]},
            "request_id":{"type":"string"},"request_seq":{"type":"integer"},"question_count":{"type":"integer"},
            "approval_kind":{"type":"string"},"interrupted":{"type":"boolean"},"state":{"type":"string"},
            "reason":{"type":["string","null"]},"exit_code":{"type":["integer","null"]},
            "waited_secs":{"type":"integer"},"idle_secs":{"type":"integer"}
        },"required":required,"additionalProperties":false});
        json!({"name":name,"description":description,"delivery":["webhook"],"inputSchema":input,"payloadSchema":payload})
    }).collect();
    json!({"events":events})
}

pub async fn handle(manager: &Manager, request: Request) -> Response {
    let transport = callback::transport(Duration::from_secs(
        manager.config().mcp_events.timeout_secs,
    ));
    handle_with(manager, request, transport).await
}

async fn handle_with(manager: &Manager, request: Request, transport: Transport) -> Response {
    if !manager.config().mcp_events.enabled {
        return error(-32601, "MCP Events is disabled", None);
    }
    match request {
        Request::List(p) => {
            if p.cursor.is_some() {
                invalid()
            } else {
                Response::ok(definitions())
            }
        }
        Request::Unsubscribe(p) => {
            if !EVENT_NAMES.contains(&p.name.as_str())
                || !p.arguments.validate()
                || p.delivery.mode != "webhook"
            {
                return invalid();
            }
            let url = match callback::validate_url(&p.delivery.url) {
                Ok(u) => u,
                Err(_) => return invalid(),
            };
            let id = subscription_id(
                &owner_principal(&manager.config().mcp_events),
                &p.name,
                &p.arguments,
                url.as_str(),
            );
            match manager
                .store()
                .unsubscribe_event(&owner_principal(&manager.config().mcp_events), &id)
                .await
            {
                Ok(()) => Response::ok(json!({})),
                Err(e) => storage(e),
            }
        }
        Request::Subscribe(p) => subscribe(manager, p, transport).await,
    }
}

async fn subscribe(manager: &Manager, p: Subscribe, transport: Transport) -> Response {
    let config = &manager.config().mcp_events;
    if !EVENT_NAMES.contains(&p.name.as_str())
        || !p.arguments.validate()
        || p.delivery.mode != "webhook"
        || callback::signing_key(&p.delivery.secret).is_none()
    {
        return invalid();
    }
    let url = match callback::validate_url(&p.delivery.url) {
        Ok(u) => u,
        Err(_) => return invalid(),
    };
    if let Err(e) = manager.store().check_event_permissions() {
        return storage(e);
    }
    let id = subscription_id(
        &owner_principal(&manager.config().mcp_events),
        &p.name,
        &p.arguments,
        url.as_str(),
    );
    let replay = match p.cursor.as_deref() {
        None => None,
        Some(v) => match parse_cursor(&id, v) {
            Some(n) => Some(n),
            None => return invalid(),
        },
    };
    if let Err(e) = manager.store().flush().await {
        return storage(e);
    }
    let old = match manager
        .store()
        .event_subscription(&owner_principal(&manager.config().mcp_events), &id)
        .await
    {
        Ok(v) => v,
        Err(e) => return storage(e),
    };
    let now = now_ms();
    let cached = old.as_ref().is_some_and(|s| {
        s.active
            && s.expires_ms > now
            && s.secret == p.delivery.secret
            && now - s.verified_ms < VERIFICATION_CACHE_MS
    });
    if !cached {
        let permitted = {
            let mut rate = VERIFICATION_RATE.lock().unwrap_or_else(|e| e.into_inner());
            let (start, count) = rate.get_or_insert((Instant::now(), 0));
            if start.elapsed() >= Duration::from_secs(60) {
                *start = Instant::now();
                *count = 0;
            }
            if *count >= 32 {
                false
            } else {
                *count += 1;
                true
            }
        };
        if !permitted {
            return error(-32025, "Callback verification rate limit reached", None);
        }
        let challenge = uuid_challenge();
        let body = json!({"type":"verification","challenge":challenge})
            .to_string()
            .into_bytes();
        let verification_id = format!("verification_{challenge}");
        let Some(headers) = callback::headers(
            &verification_id,
            &id,
            now / 1000,
            &body,
            &p.delivery.secret,
            None,
        ) else {
            return invalid();
        };
        let reply = transport(Post {
            verification: true,
            url: url.to_string(),
            body,
            headers,
        })
        .await;
        let reason = match reply {
            Ok(reply) if (200..300).contains(&reply.status) => {
                let echoed = serde_json::from_slice::<Value>(&reply.body)
                    .ok()
                    .and_then(|v| v["challenge"].as_str().map(str::to_owned));
                if !echoed.as_ref().is_some_and(|s| {
                    crate::http::constant_time_eq(s.as_bytes(), challenge.as_bytes())
                }) {
                    Some("challenge_failed")
                } else {
                    None
                }
            }
            Ok(reply) => Some(if reply.status >= 500 {
                "http_5xx"
            } else {
                "http_4xx"
            }),
            Err(reason) => Some(reason),
        };
        if let Some(reason) = reason {
            return error(-32015, "CallbackEndpointError", Some(reason));
        }
    }
    let ttl = p
        .ttl_ms
        .unwrap_or(config.ttl_secs * 1000)
        .clamp(60_000, config.ttl_secs * 1000);
    let sub = Subscription {
        id: id.clone(),
        principal: owner_principal(config),
        name: p.name,
        arguments: p.arguments,
        callback: url.to_string(),
        secret: p.delivery.secret,
        previous_secret: None,
        rotate_until_ms: 0,
        expires_ms: now + ttl as i64,
        verified_ms: if cached {
            old.as_ref().map_or(now, |s| s.verified_ms)
        } else {
            now
        },
        active: true,
        version: old.as_ref().map_or(1, |s| s.version),
        created_ms: old.as_ref().map_or(now, |s| s.created_ms),
    };
    match manager
        .store()
        .activate_event_subscription(
            &sub,
            old.as_ref().map(|s| s.version),
            replay,
            p.max_age_ms.map(|n| n.min(i64::MAX as u64 / 2) as i64),
            config,
        )
        .await
    {
        Ok(Some((cursor, truncated))) => Response::ok(
            json!({"id":id,"refreshBefore":timestamp(sub.expires_ms),"cursor":cursor,"truncated":truncated}),
        ),
        Ok(None) => error(
            -32000,
            "Subscription changed concurrently; retry discovery and subscribe",
            None,
        ),
        Err(StoreError::McpEventLimit) => error(-32025, "Event subscription limit reached", None),
        Err(StoreError::McpEventCursor) => invalid(),
        Err(e) => storage(e),
    }
}

fn uuid_challenge() -> String {
    // Uuid v4 supplies 122 unpredictable bits; verification values are never logged.
    uuid::Uuid::new_v4().to_string()
}

pub struct Dispatcher {
    cancel: CancellationToken,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Dispatcher {
    pub async fn shutdown(self) {
        self.cancel.cancel();
        for task in self.tasks {
            let _ = task.await;
        }
    }
}

pub async fn spawn(manager: Manager) -> Result<Dispatcher, StoreError> {
    let config = manager.config().mcp_events.clone();
    let cancel = CancellationToken::new();
    let mut tasks = Vec::new();
    if config.enabled {
        manager.store().check_event_permissions()?;
        manager
            .store()
            .revoke_event_principals(&owner_principal(&config))
            .await?;
        manager.store().recover_event_deliveries().await?;
        for _ in 0..4 {
            let store = manager.store().clone();
            let options = config.clone();
            let cancel = cancel.clone();
            let transport = callback::transport(Duration::from_secs(options.timeout_secs));
            tasks.push(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _=cancel.cancelled()=>break,
                        result=dispatch_one(&store,&options,&transport)=> {
                            if let Err(reason)=result {tracing::warn!(reason,"MCP Events worker deferred");}
                        }
                    }
                    tokio::select! {_=cancel.cancelled()=>break,_=tokio::time::sleep(Duration::from_millis(100))=>{}}
                }
            }));
        }
        let store = manager.store().clone();
        let cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                if store.prune_event_deliveries(now_ms()-(config.retention_days*86_400_000) as i64).await.is_err() {
                    tracing::warn!("MCP Events retention deferred");
                }
                tokio::select! {_=cancel.cancelled()=>break,_=tokio::time::sleep(Duration::from_secs(60))=>{}}
            }
        }));
    }
    Ok(Dispatcher { cancel, tasks })
}

async fn dispatch_one(
    store: &Store,
    config: &wardex_core::config::McpEventsConfig,
    transport: &Transport,
) -> Result<(), &'static str> {
    let now = now_ms();
    let Some(delivery) = store
        .claim_event_delivery(now, ((config.timeout_secs + 30) * 1000) as i64)
        .await
        .map_err(|_| "storage")?
    else {
        return Ok(());
    };
    let previous = delivery
        .subscription
        .previous_secret
        .as_deref()
        .filter(|_| delivery.subscription.rotate_until_ms > now);
    let headers = callback::headers(
        &delivery.event_id,
        &delivery.subscription.id,
        now / 1000,
        delivery.body.as_bytes(),
        &delivery.subscription.secret,
        previous,
    );
    let result = match headers {
        Some(headers) if delivery.body.len() <= 262_144 => {
            transport(Post {
                verification: false,
                url: delivery.subscription.callback.clone(),
                body: delivery.body.as_bytes().to_vec(),
                headers,
            })
            .await
        }
        _ => Err("invalid_payload"),
    };
    let (state, reason, suspend, retry_after) = match result {
        Ok(reply) if (200..300).contains(&reply.status) => ("delivered", None, false, None),
        Ok(reply) if reply.status == 410 => ("abandoned", Some("gone"), true, None),
        Ok(reply) if reply.status == 413 => ("abandoned", Some("payload_too_large"), false, None),
        Ok(reply) if reply.status == 408 || reply.status == 429 || reply.status >= 500 => {
            ("pending", Some("http_transient"), false, reply.retry_after)
        }
        Ok(_) => ("abandoned", Some("http_permanent"), true, None),
        Err(reason)
            if ["invalid_payload", "non_public_address", "invalid_url"].contains(&reason) =>
        {
            ("abandoned", Some(reason), true, None)
        }
        Err(reason) => ("pending", Some(reason), false, None),
    };
    let exhausted = state == "pending"
        && (delivery.attempts >= config.max_attempts
            || now - delivery.created_ms >= (config.retry_window_secs * 1000) as i64);
    let state = if exhausted { "abandoned" } else { state };
    let delay = (1_u64 << delivery.attempts.saturating_sub(1).min(6))
        .min(60)
        .max(retry_after.unwrap_or(0).min(300));
    // Bounded deterministic jitter avoids synchronized retries without another RNG dependency.
    let jitter = delivery
        .event_id
        .bytes()
        .fold(0_u64, |a, b| a.wrapping_add(u64::from(b)))
        % 1000;
    store
        .finish_event_delivery(
            &delivery,
            state,
            now + (delay * 1000 + jitter) as i64,
            if exhausted {
                Some("retry_exhausted")
            } else {
                reason
            },
            suspend || exhausted,
        )
        .await
        .map_err(|_| "storage")?;
    if state == "abandoned" {
        tracing::warn!(subscription=%delivery.subscription.id,event=%delivery.event_id,reason=reason.unwrap_or("retry_exhausted"),"MCP event delivery abandoned");
    } else {
        tracing::debug!(subscription=%delivery.subscription.id,event=%delivery.event_id,state,"MCP event delivery recorded");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callback::Reply;
    use base64::Engine;
    use std::sync::{Arc, Mutex};
    use wardex_core::process::Launcher;
    use wardex_core::{Config, DriverKind, Event, EventKind, SessionId};

    async fn manager() -> (tempfile::TempDir, Manager) {
        let dir = tempfile::tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = Store::open(&dir.path().join("events.db")).await.unwrap();
        store.check_event_permissions().unwrap();
        let mut config = Config::default();
        config.mcp_events.enabled = true;
        (dir, Manager::new(config, store, vec![], Launcher::direct()))
    }
    fn request() -> Subscribe {
        Subscribe {
            name: "wardex.done".into(),
            arguments: Arguments {
                session_id: Some("test-session".into()),
                ..Default::default()
            },
            delivery: Webhook {
                mode: "webhook".into(),
                url: "https://receiver.example/callback".into(),
                secret: format!(
                    "whsec_{}",
                    base64::engine::general_purpose::STANDARD.encode([11_u8; 32])
                ),
            },
            cursor: None,
            ttl_ms: Some(120_000),
            max_age_ms: None,
        }
    }
    fn accepting(captured: Arc<Mutex<Vec<Post>>>) -> Transport {
        Arc::new(move |post| {
            let value: Value = serde_json::from_slice(&post.body).unwrap();
            let reply = if value["type"] == "verification" {
                json!({"challenge":value["challenge"]})
                    .to_string()
                    .into_bytes()
            } else {
                Vec::new()
            };
            captured.lock().unwrap().push(post);
            Box::pin(async move {
                Ok(Reply {
                    status: 200,
                    body: reply,
                    retry_after: None,
                })
            })
        })
    }
    async fn publish(manager: &Manager, seq: u64) {
        manager
            .store()
            .append_event(Event {
                session_id: SessionId("test-session".into()),
                seq,
                ts_ms: now_ms(),
                agent: None,
                task: None,
                driver: DriverKind::Codex,
                kind: EventKind::Done {
                    result: Some("private result".into()),
                    interrupted: false,
                    note: None,
                    git: None,
                },
            })
            .unwrap();
        manager.store().flush().await.unwrap();
    }

    #[tokio::test]
    async fn verified_idempotent_subscription_delivers_without_status_polling_and_stops() {
        let (_dir, manager) = manager().await;
        let captured = Arc::new(Mutex::new(Vec::new()));
        let transport = accepting(captured.clone());
        let first = handle_with(&manager, Request::Subscribe(request()), transport.clone()).await;
        assert!(first.ok, "{first:?}");
        let second = handle_with(&manager, Request::Subscribe(request()), transport.clone()).await;
        assert_eq!(
            first.data.as_ref().unwrap()["id"],
            second.data.as_ref().unwrap()["id"]
        );
        assert_eq!(captured.lock().unwrap().len(), 1, "cached challenge");
        publish(&manager, 1).await;
        dispatch_one(manager.store(), &manager.config().mcp_events, &transport)
            .await
            .unwrap();
        {
            let posts = captured.lock().unwrap();
            assert_eq!(posts.len(), 2);
            let event: Value = serde_json::from_slice(&posts[1].body).unwrap();
            assert_eq!(event["name"], "wardex.done");
            assert!(!String::from_utf8_lossy(&posts[1].body).contains("private result"));
            assert_eq!(
                posts[1].headers["webhook-id"].to_str().unwrap(),
                event["eventId"].as_str().unwrap()
            );
        }
        let p = request();
        let unsubscribe = Request::Unsubscribe(Unsubscribe {
            name: p.name,
            arguments: p.arguments,
            delivery: Destination {
                mode: p.delivery.mode,
                url: p.delivery.url,
            },
        });
        assert!(
            handle_with(&manager, unsubscribe, transport.clone())
                .await
                .ok
        );
        publish(&manager, 2).await;
        dispatch_one(manager.store(), &manager.config().mcp_events, &transport)
            .await
            .unwrap();
        assert_eq!(captured.lock().unwrap().len(), 2);
        manager.store().close().await.unwrap();
    }

    #[tokio::test]
    async fn failed_challenge_never_stores_secret_and_failed_rotation_preserves_active_subscription()
     {
        let (_dir, manager) = manager().await;
        let rejected: Transport = Arc::new(|_| {
            Box::pin(async {
                Ok(Reply {
                    status: 200,
                    body: br#"{"challenge":"wrong"}"#.to_vec(),
                    retry_after: None,
                })
            })
        });
        let failed = handle_with(&manager, Request::Subscribe(request()), rejected.clone()).await;
        assert_eq!(failed.code.as_deref(), Some("-32015"));
        let p = request();
        let id = subscription_id(
            &owner_principal(&manager.config().mcp_events),
            &p.name,
            &p.arguments,
            &p.delivery.url,
        );
        assert!(
            manager
                .store()
                .event_subscription(&owner_principal(&manager.config().mcp_events), &id)
                .await
                .unwrap()
                .is_none()
        );
        let captured = Arc::new(Mutex::new(Vec::new()));
        let accepting = accepting(captured);
        assert!(
            handle_with(&manager, Request::Subscribe(request()), accepting)
                .await
                .ok
        );
        let mut rotated = request();
        rotated.delivery.secret = format!(
            "whsec_{}",
            base64::engine::general_purpose::STANDARD.encode([12_u8; 32])
        );
        assert!(
            !handle_with(&manager, Request::Subscribe(rotated), rejected)
                .await
                .ok
        );
        assert_eq!(
            manager
                .store()
                .event_subscription(&owner_principal(&manager.config().mcp_events), &id)
                .await
                .unwrap()
                .unwrap()
                .secret,
            request().delivery.secret
        );
        manager.store().close().await.unwrap();
    }

    #[tokio::test]
    async fn transient_delivery_retries_same_bytes_and_permanent_failures_stop() {
        let (_dir, manager) = manager().await;
        let captured = Arc::new(Mutex::new(Vec::new()));
        assert!(
            handle_with(
                &manager,
                Request::Subscribe(request()),
                accepting(captured.clone())
            )
            .await
            .ok
        );
        publish(&manager, 1).await;
        let failure: Transport = Arc::new({
            let captured = captured.clone();
            move |post| {
                captured.lock().unwrap().push(post);
                Box::pin(async {
                    Ok(Reply {
                        status: 503,
                        body: Vec::new(),
                        retry_after: Some(1),
                    })
                })
            }
        });
        dispatch_one(manager.store(), &manager.config().mcp_events, &failure)
            .await
            .unwrap();
        manager.store().recover_event_deliveries().await.unwrap();
        // Advance the due time in the isolated database; no wall-clock sleep.
        let pool =
            sqlx::SqlitePool::connect(&format!("sqlite://{}", manager.store().path().display()))
                .await
                .unwrap();
        sqlx::query("UPDATE mcp_outbox SET next_attempt_ms=0")
            .execute(&pool)
            .await
            .unwrap();
        dispatch_one(
            manager.store(),
            &manager.config().mcp_events,
            &accepting(captured.clone()),
        )
        .await
        .unwrap();
        {
            let posts = captured.lock().unwrap();
            assert_eq!(posts.len(), 3);
            assert_eq!(posts[1].body, posts[2].body);
        }
        publish(&manager, 2).await;
        let gone: Transport = Arc::new(|_| {
            Box::pin(async {
                Ok(Reply {
                    status: 410,
                    body: Vec::new(),
                    retry_after: None,
                })
            })
        });
        dispatch_one(manager.store(), &manager.config().mcp_events, &gone)
            .await
            .unwrap();
        let p = request();
        let id = subscription_id(
            &owner_principal(&manager.config().mcp_events),
            &p.name,
            &p.arguments,
            &p.delivery.url,
        );
        assert!(
            !manager
                .store()
                .event_subscription(&owner_principal(&manager.config().mcp_events), &id)
                .await
                .unwrap()
                .unwrap()
                .active
        );
        pool.close().await;
        manager.store().close().await.unwrap();
    }

    #[test]
    fn debug_output_redacts_destinations_and_material() {
        let p = request();
        let debug = format!("{p:?}");
        assert!(!debug.contains(&p.delivery.url));
        assert!(!debug.contains(&p.delivery.secret));
    }

    #[tokio::test]
    async fn redirects_and_oversize_rejections_are_terminal_without_another_attempt() {
        for status in [302, 413] {
            let (_dir, manager) = manager().await;
            let captured = Arc::new(Mutex::new(Vec::new()));
            assert!(
                handle_with(
                    &manager,
                    Request::Subscribe(request()),
                    accepting(captured.clone())
                )
                .await
                .ok
            );
            publish(&manager, 1).await;
            let replies: Transport = Arc::new({
                let captured = captured.clone();
                move |post| {
                    captured.lock().unwrap().push(post);
                    Box::pin(async move {
                        Ok(Reply {
                            status,
                            body: Vec::new(),
                            retry_after: None,
                        })
                    })
                }
            });
            dispatch_one(manager.store(), &manager.config().mcp_events, &replies)
                .await
                .unwrap();
            dispatch_one(manager.store(), &manager.config().mcp_events, &replies)
                .await
                .unwrap();
            assert_eq!(
                captured.lock().unwrap().len(),
                2,
                "one verification and one application attempt"
            );
            let p = request();
            let owner = owner_principal(&manager.config().mcp_events);
            let id = subscription_id(&owner, &p.name, &p.arguments, &p.delivery.url);
            assert_eq!(
                manager
                    .store()
                    .event_subscription(&owner, &id)
                    .await
                    .unwrap()
                    .unwrap()
                    .active,
                status == 413
            );
            manager.store().close().await.unwrap();
        }
    }
}
