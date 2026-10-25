//! Durable MCP Events state. Only committed, minimal lifecycle records enter
//! delivery; the live broadcast is deliberately not the delivery source.

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Row, SqliteConnection};

use crate::config::McpEventsConfig;
use crate::store::{Result, Store, StoreError};
use crate::{Event, EventKind};

pub const EVENT_NAMES: &[&str] = &[
    "wardex.question",
    "wardex.approval",
    "wardex.done",
    "wardex.ended",
    "wardex.error",
    "wardex.reminder",
    "wardex.stuck",
    "wardex.resolved",
];

/// Changing this non-secret generation revokes all earlier owner subscriptions.
pub fn owner_principal(config: &McpEventsConfig) -> String {
    format!("wardex-owner:{}", config.authorization_epoch)
}

/// Explicit scope: labels are filters, never authentication or authorization.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Arguments {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub all_sessions: Option<bool>,
}

impl Arguments {
    pub fn validate(&self) -> bool {
        (self.all_sessions == Some(true)
            || self.session_id.is_some()
            || self.agent.is_some()
            || self.task.is_some())
            && [&self.session_id, &self.agent, &self.task]
                .into_iter()
                .all(|v| v.as_ref().is_none_or(|s| !s.is_empty() && s.len() <= 256))
    }

    pub fn matches(&self, data: &Value) -> bool {
        self.session_id
            .as_ref()
            .is_none_or(|s| data["session_id"] == *s)
            && [&self.agent, &self.task]
                .into_iter()
                .zip(["agent", "task"])
                .all(|(pattern, field)| {
                    pattern.as_ref().is_none_or(|p| {
                        data[field]
                            .as_str()
                            .is_some_and(|s| crate::glob::glob(p, s))
                    })
                })
    }
}

/// Contains sensitive callback and signing material; deliberately no Debug.
#[derive(Clone)]
pub struct Subscription {
    pub id: String,
    pub principal: String,
    pub name: String,
    pub arguments: Arguments,
    pub callback: String,
    pub secret: String,
    pub previous_secret: Option<String>,
    pub rotate_until_ms: i64,
    pub expires_ms: i64,
    pub verified_ms: i64,
    pub active: bool,
    pub version: i64,
    pub created_ms: i64,
}

pub struct Delivery {
    pub subscription: Subscription,
    pub event_id: String,
    pub position: i64,
    pub body: String,
    pub attempts: u32,
    pub lease_token: String,
    pub created_ms: i64,
}

pub fn timestamp(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .unwrap_or_default()
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn cursor(id: &str, position: i64) -> String {
    format!("v1:{id}:{position}")
}

pub fn parse_cursor(id: &str, value: &str) -> Option<i64> {
    let position = value
        .strip_prefix(&format!("v1:{id}:"))?
        .parse::<i64>()
        .ok()?;
    (position >= 0).then_some(position)
}

fn safe_data(event: &Event) -> Option<Value> {
    if !EVENT_NAMES.contains(&format!("wardex.{}", event.kind.name()).as_str()) {
        return None;
    }
    let mut data = json!({"schema_version":1,"session_id":event.session_id,"seq":event.seq,
        "driver":event.driver,"agent":event.agent,"task":event.task});
    // No prompt, final answer, command, error text, cwd or native session id.
    match &event.kind {
        EventKind::Question { question_id, items } => {
            data["request_id"] = json!(question_id);
            data["request_seq"] = json!(event.seq);
            data["question_count"] = json!(items.len());
        }
        EventKind::Approval {
            request_id, kind, ..
        } => {
            data["request_id"] = json!(request_id);
            data["request_seq"] = json!(event.seq);
            data["approval_kind"] = json!(kind);
        }
        EventKind::Resolved { question_id } => data["request_id"] = json!(question_id),
        EventKind::Done { interrupted, .. } => data["interrupted"] = json!(interrupted),
        EventKind::Ended {
            state,
            reason,
            exit_code,
        } => {
            data["state"] = json!(state);
            data["reason"] = json!(reason);
            data["exit_code"] = json!(exit_code);
        }
        EventKind::Reminder {
            question_id,
            waited_secs,
        } => {
            data["request_id"] = json!(question_id);
            data["waited_secs"] = json!(waited_secs);
        }
        EventKind::Stuck { reason, idle_secs } => {
            data["reason"] = json!(reason);
            data["idle_secs"] = json!(idle_secs);
        }
        _ => {}
    }
    Some(data)
}

fn decode(row: &sqlx::sqlite::SqliteRow) -> Result<Subscription> {
    Ok(Subscription {
        id: row.try_get("id")?,
        principal: row.try_get("principal")?,
        name: row.try_get("name")?,
        arguments: serde_json::from_str(row.try_get("arguments")?)
            .map_err(|_| StoreError::Corrupt("invalid event arguments".into()))?,
        callback: row.try_get("callback")?,
        secret: row.try_get("secret")?,
        previous_secret: row.try_get("previous_secret")?,
        rotate_until_ms: row.try_get("rotate_until_ms")?,
        expires_ms: row.try_get("expires_ms")?,
        verified_ms: row.try_get("verified_ms")?,
        active: row.try_get("active")?,
        version: row.try_get("version")?,
        created_ms: row.try_get("created_ms")?,
    })
}

async fn watermark(conn: &mut SqliteConnection, id: &str) -> Result<i64> {
    let head: i64 = sqlx::query_scalar(
        "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name='mcp_event_journal'),0)",
    )
    .fetch_one(&mut *conn)
    .await?;
    let pending: Option<i64> = sqlx::query_scalar("SELECT MIN(position) FROM mcp_outbox WHERE subscription_id=? AND state IN ('pending','inflight')")
        .bind(id).fetch_one(conn).await?;
    Ok(pending.map_or(head, |p| (p - 1).max(0)))
}

async fn enqueue(
    conn: &mut SqliteConnection,
    sub: &Subscription,
    position: i64,
    event_id: &str,
    ms: i64,
    data: Value,
    now: i64,
) -> Result<()> {
    let safe = watermark(conn, &sub.id).await?.min(position - 1).max(0);
    let mut wire_data = data;
    for label in ["agent", "task"] {
        if let Some(text) = wire_data[label].as_str() {
            wire_data[label] = json!(text.chars().take(256).collect::<String>());
        }
    }
    let body = json!({"eventId":event_id,"name":sub.name,"timestamp":timestamp(ms),"data":wire_data,"cursor":cursor(&sub.id,safe)}).to_string();
    sqlx::query("INSERT OR IGNORE INTO mcp_outbox (subscription_id,position,event_id,body,created_ms,next_attempt_ms) VALUES (?,?,?,?,?,?)")
        .bind(&sub.id).bind(position).bind(event_id).bind(body).bind(now).bind(now).execute(conn).await?;
    Ok(())
}

/// Called in the exact transaction that persists the original event.
pub(crate) async fn journal(
    conn: &mut SqliteConnection,
    event: &Event,
    config: &McpEventsConfig,
) -> Result<()> {
    if !config.enabled {
        return Ok(());
    }
    let Some(data) = safe_data(event) else {
        return Ok(());
    };
    let name = format!("wardex.{}", event.kind.name());
    let id = format!("wardex_{}_{}", event.session_id.as_str(), event.seq);
    let result = sqlx::query(
        "INSERT OR IGNORE INTO mcp_event_journal(event_id,name,occurred_ms,data) VALUES (?,?,?,?)",
    )
    .bind(&id)
    .bind(&name)
    .bind(event.ts_ms)
    .bind(data.to_string())
    .execute(&mut *conn)
    .await?;
    if result.rows_affected() == 0 {
        return Ok(());
    }
    let position = result.last_insert_rowid();
    let now = crate::time::now_ms();
    let rows =
        sqlx::query("SELECT * FROM mcp_subscriptions WHERE active=1 AND expires_ms>? AND name=? AND principal=?")
            .bind(now).bind(&name).bind(owner_principal(config)).fetch_all(&mut *conn)
            .await?;
    for row in rows {
        let sub = decode(&row)?;
        if !sub.arguments.matches(&data) {
            continue;
        }
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mcp_outbox WHERE subscription_id=? AND state IN ('pending','inflight')")
            .bind(&sub.id).fetch_one(&mut *conn).await?;
        if count >= i64::from(config.max_pending) {
            tracing::warn!(subscription=%sub.id, "MCP Events subscription suspended: queue_full");
            sqlx::query("UPDATE mcp_subscriptions SET active=0,last_error='queue_full' WHERE id=?")
                .bind(&sub.id)
                .execute(&mut *conn)
                .await?;
        } else {
            enqueue(conn, &sub, position, &id, event.ts_ms, data.clone(), now).await?;
        }
    }
    Ok(())
}

impl Store {
    /// Events persist callback secrets. Refuse unsafe existing permissions;
    /// operators decide how to secure their deployment and backups.
    pub fn check_event_permissions(&self) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        for path in [
            self.path().to_path_buf(),
            self.path()
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .to_path_buf(),
        ] {
            let metadata = std::fs::metadata(&path).map_err(|source| StoreError::Io {
                path: path.clone(),
                source,
            })?;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(StoreError::McpEventPermissions);
            }
        }
        Ok(())
    }

    pub fn configure_mcp_events(&self, config: McpEventsConfig) {
        *self.event_config.lock().unwrap_or_else(|e| e.into_inner()) = config;
    }

    pub async fn event_subscription(
        &self,
        principal: &str,
        id: &str,
    ) -> Result<Option<Subscription>> {
        sqlx::query("SELECT * FROM mcp_subscriptions WHERE id=? AND principal=?")
            .bind(id)
            .bind(principal)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(decode)
            .transpose()
    }

    /// Commit only after callback verification; failed refreshes preserve the old secret.
    pub async fn activate_event_subscription(
        &self,
        sub: &Subscription,
        expected: Option<i64>,
        replay: Option<i64>,
        max_age_ms: Option<i64>,
        config: &McpEventsConfig,
    ) -> Result<Option<(String, bool)>> {
        let mut tx = self.pool.begin().await?;
        let current = sqlx::query("SELECT * FROM mcp_subscriptions WHERE id=? AND principal=?")
            .bind(&sub.id)
            .bind(&sub.principal)
            .fetch_optional(&mut *tx)
            .await?;
        let old = current.as_ref().map(decode).transpose()?;
        if old.as_ref().map(|s| s.version) != expected {
            return Ok(None);
        }
        let now = crate::time::now_ms();
        if old.is_none() {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM mcp_subscriptions WHERE principal=? AND expires_ms>?",
            )
            .bind(&sub.principal)
            .bind(now)
            .fetch_one(&mut *tx)
            .await?;
            if count >= i64::from(config.max_subscriptions) {
                return Err(StoreError::McpEventLimit);
            }
        }
        let was_live = old.as_ref().is_some_and(|s| s.active && s.expires_ms > now);
        let previous = old
            .as_ref()
            .filter(|s| s.secret != sub.secret)
            .map(|s| s.secret.clone());
        sqlx::query("INSERT INTO mcp_subscriptions(id,principal,name,arguments,callback,secret,previous_secret,rotate_until_ms,expires_ms,verified_ms,active,version,created_ms) VALUES (?,?,?,?,?,?,?,?,?,?,1,1,?) ON CONFLICT(id) DO UPDATE SET secret=excluded.secret,previous_secret=CASE WHEN mcp_subscriptions.secret<>excluded.secret THEN mcp_subscriptions.secret ELSE mcp_subscriptions.previous_secret END,rotate_until_ms=CASE WHEN mcp_subscriptions.secret<>excluded.secret THEN excluded.rotate_until_ms ELSE mcp_subscriptions.rotate_until_ms END,expires_ms=excluded.expires_ms,verified_ms=excluded.verified_ms,active=1,version=mcp_subscriptions.version+1,last_error=NULL")
            .bind(&sub.id).bind(&sub.principal).bind(&sub.name).bind(serde_json::to_string(&sub.arguments).unwrap_or_default())
            .bind(&sub.callback).bind(&sub.secret).bind(previous).bind(now+300_000).bind(sub.expires_ms).bind(sub.verified_ms).bind(sub.created_ms)
            .execute(&mut *tx).await?;
        let head: i64 = sqlx::query_scalar(
            "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name='mcp_event_journal'),0)",
        )
        .fetch_one(&mut *tx)
        .await?;
        if !was_live {
            // A lapsed subscription starts at the requested cursor (null = now).
            // Invalidate old claims; an already sent HTTP request cannot be recalled.
            sqlx::query("UPDATE mcp_outbox SET state='cancelled',lease_token=NULL WHERE subscription_id=? AND state IN ('pending','inflight')")
                .bind(&sub.id).execute(&mut *tx).await?;
        }
        let mut truncated = false;
        if let Some(position) = replay.filter(|_| !was_live) {
            if position > head {
                return Err(StoreError::McpEventCursor);
            }
            let oldest: Option<i64> =
                sqlx::query_scalar("SELECT MIN(position) FROM mcp_event_journal")
                    .fetch_one(&mut *tx)
                    .await?;
            let floor = now - max_age_ms.unwrap_or(i64::MAX / 2).min(i64::MAX / 2);
            truncated = oldest.map_or(position < head, |p| position < p - 1);
            let rows = sqlx::query("SELECT position,event_id,name,occurred_ms,data FROM mcp_event_journal WHERE position>? AND name=? ORDER BY position")
                .bind(position).bind(&sub.name).fetch_all(&mut *tx).await?;
            let mut count = 0;
            for row in rows {
                let data: Value = serde_json::from_str(row.try_get("data")?)
                    .map_err(|_| StoreError::Corrupt("invalid event data".into()))?;
                if !sub.arguments.matches(&data) {
                    continue;
                }
                let ms: i64 = row.try_get("occurred_ms")?;
                if ms < floor || count >= config.max_pending {
                    truncated = true;
                    continue;
                }
                let event_id: String = row.try_get("event_id")?;
                // Explicit replay can recover an event previously acknowledged or abandoned.
                sqlx::query("DELETE FROM mcp_outbox WHERE subscription_id=? AND event_id=? AND state<>'inflight'")
                    .bind(&sub.id).bind(&event_id).execute(&mut *tx).await?;
                enqueue(
                    &mut tx,
                    sub,
                    row.try_get("position")?,
                    &event_id,
                    ms,
                    data,
                    now,
                )
                .await?;
                count += 1;
            }
        }
        let safe = watermark(&mut tx, &sub.id).await?;
        tx.commit().await?;
        Ok(Some((cursor(&sub.id, safe), truncated)))
    }

    pub async fn unsubscribe_event(&self, principal: &str, id: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE mcp_subscriptions SET active=0,expires_ms=0,version=version+1,secret='',previous_secret=NULL,rotate_until_ms=0 WHERE id=? AND principal=?")
            .bind(id).bind(principal).execute(&mut *tx).await?;
        sqlx::query("UPDATE mcp_outbox SET state='cancelled' WHERE subscription_id=? AND EXISTS(SELECT 1 FROM mcp_subscriptions WHERE id=? AND principal=?)")
            .bind(id).bind(id).bind(principal).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Safe diagnostics: never return callback destinations or signing material.
    pub async fn event_delivery_health(&self, principal: &str) -> Result<Value> {
        let rows=sqlx::query("SELECT s.id,s.name,s.active,s.expires_ms,s.last_error,(SELECT COUNT(*) FROM mcp_outbox o WHERE o.subscription_id=s.id AND o.state IN ('pending','inflight')) AS pending FROM mcp_subscriptions s WHERE s.principal=? ORDER BY s.id LIMIT 1024")
            .bind(principal).fetch_all(&self.pool).await?;
        let mut subscriptions = Vec::new();
        for row in rows {
            let expires_ms: i64 = row.try_get("expires_ms")?;
            subscriptions.push(json!({"id":row.try_get::<String,_>("id")?,"name":row.try_get::<String,_>("name")?,
                "active":row.try_get::<bool,_>("active")? && expires_ms>crate::time::now_ms(),"expires_at":timestamp(expires_ms),
                "pending":row.try_get::<i64,_>("pending")?,"last_error":row.try_get::<Option<String>,_>("last_error")?}));
        }
        Ok(json!({"subscriptions":subscriptions}))
    }

    /// Called under the daemon's exclusive database lock before workers start.
    pub async fn revoke_event_principals(&self, principal: &str) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE mcp_outbox SET state='cancelled',lease_token=NULL,last_error='access_revoked' WHERE subscription_id IN (SELECT id FROM mcp_subscriptions WHERE principal<>?) AND state IN ('pending','inflight')")
            .bind(principal).execute(&mut *tx).await?;
        sqlx::query("UPDATE mcp_subscriptions SET active=0,expires_ms=0,secret='',previous_secret=NULL,version=version+1,last_error='access_revoked' WHERE principal<>? AND (active<>0 OR secret<>'')")
            .bind(principal).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn recover_event_deliveries(&self) -> Result<()> {
        sqlx::query("UPDATE mcp_outbox SET state='pending',lease_until_ms=NULL,lease_token=NULL WHERE state='inflight'").execute(&self.pool).await?;
        Ok(())
    }

    pub async fn claim_event_delivery(&self, now: i64, lease_ms: i64) -> Result<Option<Delivery>> {
        sqlx::query("UPDATE mcp_outbox SET state='pending',lease_until_ms=NULL,lease_token=NULL WHERE state='inflight' AND lease_until_ms<=?")
            .bind(now).execute(&self.pool).await?;
        let lease_token = uuid::Uuid::new_v4().to_string();
        let row = sqlx::query("UPDATE mcp_outbox SET state='inflight',lease_until_ms=?,lease_token=?,attempts=attempts+1 WHERE (subscription_id,event_id)=(SELECT o.subscription_id,o.event_id FROM mcp_outbox o JOIN mcp_subscriptions s ON s.id=o.subscription_id WHERE o.state='pending' AND o.next_attempt_ms<=? AND s.active=1 AND s.expires_ms>? AND NOT EXISTS(SELECT 1 FROM mcp_outbox prior WHERE prior.subscription_id=o.subscription_id AND (prior.state='inflight' OR (prior.state='pending' AND prior.position<o.position))) ORDER BY o.position LIMIT 1) RETURNING *")
            .bind(now+lease_ms).bind(&lease_token).bind(now).bind(now).fetch_optional(&self.pool).await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let id: String = row.try_get("subscription_id")?;
        let Some(subrow) =
            sqlx::query("SELECT * FROM mcp_subscriptions WHERE id=? AND active=1 AND expires_ms>?")
                .bind(&id)
                .bind(now)
                .fetch_optional(&self.pool)
                .await?
        else {
            return Ok(None);
        };
        Ok(Some(Delivery {
            subscription: decode(&subrow)?,
            event_id: row.try_get("event_id")?,
            position: row.try_get("position")?,
            body: row.try_get("body")?,
            attempts: row.try_get::<i64, _>("attempts")? as u32,
            lease_token,
            created_ms: row.try_get("created_ms")?,
        }))
    }

    pub async fn finish_event_delivery(
        &self,
        delivery: &Delivery,
        state: &str,
        next: i64,
        reason: Option<&str>,
        suspend: bool,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let result=sqlx::query("UPDATE mcp_outbox SET state=?,next_attempt_ms=?,lease_until_ms=NULL,lease_token=NULL,last_error=? WHERE subscription_id=? AND event_id=? AND state='inflight' AND lease_token=?")
            .bind(state).bind(next).bind(reason).bind(&delivery.subscription.id).bind(&delivery.event_id).bind(&delivery.lease_token).execute(&mut *tx).await?;
        if suspend && result.rows_affected() == 1 {
            sqlx::query(
                "UPDATE mcp_subscriptions SET active=0,last_error=? WHERE id=? AND version=?",
            )
            .bind(reason)
            .bind(&delivery.subscription.id)
            .bind(delivery.subscription.version)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn prune_event_deliveries(&self, before: i64) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        // Expired subscriptions are recoverable within retention, not forever.
        sqlx::query("UPDATE mcp_outbox SET state='abandoned',last_error='expired' WHERE created_ms<? AND subscription_id IN (SELECT id FROM mcp_subscriptions WHERE expires_ms<?) AND state IN ('pending','inflight')")
            .bind(before).bind(before).execute(&mut *tx).await?;
        // Pending bodies are self-contained and survive journal pruning.
        sqlx::query("DELETE FROM mcp_event_journal WHERE occurred_ms<?")
            .bind(before)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM mcp_outbox WHERE created_ms<? AND state IN ('delivered','cancelled','abandoned')")
            .bind(before).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM mcp_subscriptions WHERE expires_ms<? AND NOT EXISTS(SELECT 1 FROM mcp_outbox WHERE subscription_id=mcp_subscriptions.id AND state IN ('pending','inflight'))")
            .bind(before).execute(&mut *tx).await?;
        sqlx::query("UPDATE mcp_subscriptions SET previous_secret=NULL WHERE rotate_until_ms<?")
            .bind(crate::time::now_ms())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DriverKind, SessionId};

    fn config() -> McpEventsConfig {
        McpEventsConfig {
            enabled: true,
            ..Default::default()
        }
    }
    fn sub(id: &str, name: &str, expires_ms: i64) -> Subscription {
        Subscription {
            id: id.into(),
            principal: "wardex-owner:0".into(),
            name: name.into(),
            arguments: Arguments {
                session_id: Some("session-test".into()),
                ..Default::default()
            },
            callback: "https://receiver.example/callback".into(),
            secret: "synthetic-test-material".into(),
            previous_secret: None,
            rotate_until_ms: 0,
            expires_ms,
            verified_ms: crate::time::now_ms(),
            active: true,
            version: 1,
            created_ms: crate::time::now_ms(),
        }
    }
    fn event(seq: u64) -> Event {
        Event {
            session_id: SessionId("session-test".into()),
            seq,
            ts_ms: crate::time::now_ms(),
            agent: None,
            task: None,
            driver: DriverKind::Codex,
            kind: EventKind::Done {
                result: Some("sensitive final text".into()),
                interrupted: false,
                note: None,
                git: None,
            },
        }
    }
    async fn store(dir: &tempfile::TempDir) -> Store {
        let store = Store::open(&dir.path().join("events.db")).await.unwrap();
        store.configure_mcp_events(config());
        store
    }

    #[tokio::test]
    async fn committed_delivery_survives_reopen_and_ack_crash_with_stable_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir).await;
        let subscription = sub("sub_test", "wardex.done", crate::time::now_ms() + 60_000);
        store
            .activate_event_subscription(&subscription, None, None, None, &config())
            .await
            .unwrap()
            .unwrap();
        store.append_event(event(1)).unwrap();
        store.flush().await.unwrap();
        let first = store
            .claim_event_delivery(crate::time::now_ms(), 60_000)
            .await
            .unwrap()
            .unwrap();
        assert!(!first.body.contains("sensitive final text"));
        let body = first.body.clone();
        let id = first.event_id.clone();
        // Simulate receiver 2xx followed by process loss before recording ACK.
        store.close().await.unwrap();
        drop(store);
        let recovered = Store::open(&dir.path().join("events.db")).await.unwrap();
        recovered.configure_mcp_events(config());
        recovered.recover_event_deliveries().await.unwrap();
        let second = recovered
            .claim_event_delivery(crate::time::now_ms(), 60_000)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.body, body);
        assert_eq!(second.event_id, id);
        assert_eq!(second.attempts, 2);
        recovered
            .finish_event_delivery(&second, "delivered", 0, None, false)
            .await
            .unwrap();
        assert!(
            recovered
                .claim_event_delivery(crate::time::now_ms(), 60_000)
                .await
                .unwrap()
                .is_none()
        );
        recovered.close().await.unwrap();
    }

    #[tokio::test]
    async fn event_and_outbox_roll_back_together_and_duplicate_publication_is_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir).await;
        let subscription = sub("sub_test", "wardex.done", crate::time::now_ms() + 60_000);
        store
            .activate_event_subscription(&subscription, None, None, None, &config())
            .await
            .unwrap();
        sqlx::query("CREATE TRIGGER fail_delivery BEFORE INSERT ON mcp_outbox BEGIN SELECT RAISE(ABORT,'synthetic delivery constraint'); END").execute(&store.pool).await.unwrap();
        store.append_event(event(1)).unwrap();
        assert!(store.flush().await.is_err());
        for (table, query) in [
            ("events", "SELECT COUNT(*) FROM events"),
            (
                "mcp_event_journal",
                "SELECT COUNT(*) FROM mcp_event_journal",
            ),
            ("mcp_outbox", "SELECT COUNT(*) FROM mcp_outbox"),
        ] {
            let count: i64 = sqlx::query_scalar(query)
                .fetch_one(&store.pool)
                .await
                .unwrap();
            assert_eq!(count, 0, "{table}");
        }
        sqlx::query("DROP TRIGGER fail_delivery")
            .execute(&store.pool)
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if store.flush().await.is_ok() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap();
        store.append_event(event(1)).unwrap();
        store.flush().await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mcp_outbox")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn scope_expiration_queue_limit_and_unsubscribe_are_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir).await;
        let mut options = config();
        options.max_pending = 1;
        store.configure_mcp_events(options.clone());
        let subscription = sub("sub_test", "wardex.done", crate::time::now_ms() + 60_000);
        store
            .activate_event_subscription(&subscription, None, None, None, &options)
            .await
            .unwrap();
        let mut other = event(1);
        other.session_id = SessionId("other".into());
        store.append_event(other).unwrap();
        store.append_event(event(1)).unwrap();
        store.append_event(event(2)).unwrap();
        store.flush().await.unwrap();
        let row =
            sqlx::query("SELECT active,last_error FROM mcp_subscriptions WHERE id='sub_test'")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert!(!row.get::<bool, _>("active"));
        assert_eq!(row.get::<String, _>("last_error"), "queue_full");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mcp_outbox")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
        store
            .unsubscribe_event("other-owner", "sub_test")
            .await
            .unwrap();
        assert_ne!(
            store
                .event_subscription("wardex-owner:0", "sub_test")
                .await
                .unwrap()
                .unwrap()
                .secret,
            ""
        );
        store
            .unsubscribe_event("wardex-owner:0", "sub_test")
            .await
            .unwrap();
        store
            .unsubscribe_event("wardex-owner:0", "sub_test")
            .await
            .unwrap();
        assert_eq!(
            store
                .event_subscription("wardex-owner:0", "sub_test")
                .await
                .unwrap()
                .unwrap()
                .secret,
            ""
        );
        assert!(
            store
                .claim_event_delivery(crate::time::now_ms(), 60_000)
                .await
                .unwrap()
                .is_none()
        );
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn replay_cursor_is_scoped_and_retention_does_not_reset_global_head() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir).await;
        store.append_event(event(1)).unwrap();
        store.flush().await.unwrap();
        let subscription = sub("sub_test", "wardex.done", crate::time::now_ms() + 60_000);
        let (current, truncated) = store
            .activate_event_subscription(&subscription, None, None, None, &config())
            .await
            .unwrap()
            .unwrap();
        assert!(!truncated);
        assert_eq!(parse_cursor("sub_test", &current), Some(1));
        assert_eq!(parse_cursor("other", &current), None);
        assert!(
            store
                .claim_event_delivery(crate::time::now_ms(), 60_000)
                .await
                .unwrap()
                .is_none()
        );
        store
            .unsubscribe_event("wardex-owner:0", "sub_test")
            .await
            .unwrap();
        let old = store
            .event_subscription("wardex-owner:0", "sub_test")
            .await
            .unwrap()
            .unwrap();
        store
            .activate_event_subscription(&subscription, Some(old.version), Some(0), None, &config())
            .await
            .unwrap();
        let delivery = store
            .claim_event_delivery(crate::time::now_ms(), 60_000)
            .await
            .unwrap()
            .unwrap();
        store
            .finish_event_delivery(&delivery, "delivered", 0, None, false)
            .await
            .unwrap();
        store
            .prune_event_deliveries(crate::time::now_ms() + 1)
            .await
            .unwrap();
        store
            .unsubscribe_event("wardex-owner:0", "sub_test")
            .await
            .unwrap();
        let old = store
            .event_subscription("wardex-owner:0", "sub_test")
            .await
            .unwrap()
            .unwrap();
        let (_, truncated) = store
            .activate_event_subscription(&subscription, Some(old.version), Some(0), None, &config())
            .await
            .unwrap()
            .unwrap();
        assert!(truncated);
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn expired_claims_are_fenced_and_authorization_epoch_revokes_old_queues() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir).await;
        let subscription = sub("sub_test", "wardex.done", crate::time::now_ms() + 60_000);
        store
            .activate_event_subscription(&subscription, None, None, None, &config())
            .await
            .unwrap();
        store.append_event(event(1)).unwrap();
        store.flush().await.unwrap();
        let first = store
            .claim_event_delivery(crate::time::now_ms(), 0)
            .await
            .unwrap()
            .unwrap();
        let second = store
            .claim_event_delivery(crate::time::now_ms() + 1, 60_000)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(first.lease_token, second.lease_token);
        store
            .finish_event_delivery(&first, "delivered", 0, None, false)
            .await
            .unwrap();
        let state: String = sqlx::query_scalar("SELECT state FROM mcp_outbox")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(state, "inflight");
        let mut options = config();
        options.authorization_epoch = 1;
        store
            .revoke_event_principals(&owner_principal(&options))
            .await
            .unwrap();
        store
            .finish_event_delivery(&second, "delivered", 0, None, false)
            .await
            .unwrap();
        let sub = store
            .event_subscription("wardex-owner:0", "sub_test")
            .await
            .unwrap()
            .unwrap();
        assert!(!sub.active);
        assert!(sub.secret.is_empty());
        let state: String = sqlx::query_scalar("SELECT state FROM mcp_outbox")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(state, "cancelled");
        assert!(
            store
                .claim_event_delivery(crate::time::now_ms(), 60_000)
                .await
                .unwrap()
                .is_none()
        );
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn finite_expiration_and_null_resubscribe_do_not_replay_old_pending_events() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir).await;
        let mut subscription = sub("sub_test", "wardex.done", crate::time::now_ms() + 60_000);
        store
            .activate_event_subscription(&subscription, None, None, None, &config())
            .await
            .unwrap();
        store.append_event(event(1)).unwrap();
        store.flush().await.unwrap();
        sqlx::query("UPDATE mcp_subscriptions SET expires_ms=0")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(
            store
                .claim_event_delivery(crate::time::now_ms(), 60_000)
                .await
                .unwrap()
                .is_none()
        );
        let old = store
            .event_subscription("wardex-owner:0", "sub_test")
            .await
            .unwrap()
            .unwrap();
        subscription.expires_ms = crate::time::now_ms() + 60_000;
        store
            .activate_event_subscription(&subscription, Some(old.version), None, None, &config())
            .await
            .unwrap();
        assert!(
            store
                .claim_event_delivery(crate::time::now_ms(), 60_000)
                .await
                .unwrap()
                .is_none()
        );
        let health = store.event_delivery_health("wardex-owner:0").await.unwrap();
        assert!(!health.to_string().contains(&subscription.callback));
        assert!(!health.to_string().contains(&subscription.secret));
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn additive_migration_preserves_legacy_rows_and_old_migrator_requires_rollback_plan() {
        let dir = tempfile::tempdir().unwrap();
        let migrations = dir.path().join("legacy-migrations");
        std::fs::create_dir(&migrations).unwrap();
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        for entry in std::fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            if !name.to_string_lossy().starts_with("0006_") {
                std::fs::copy(entry.path(), migrations.join(name)).unwrap();
            }
        }
        let legacy = sqlx::migrate::Migrator::new(migrations).await.unwrap();
        let path = dir.path().join("legacy.db");
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let pool = sqlx::SqlitePool::connect_with(options).await.unwrap();
        legacy.run(&pool).await.unwrap();
        sqlx::query("INSERT INTO sessions(id,driver,state,cwd,created_ms,updated_ms) VALUES ('legacy','codex','stopped','/example',1,1)").execute(&pool).await.unwrap();
        pool.close().await;
        let upgraded = Store::open(&path).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE id='legacy'")
            .fetch_one(&upgraded.pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert!(matches!(
            legacy.run(&upgraded.pool).await,
            Err(sqlx::migrate::MigrateError::VersionMissing(6))
        ));
        upgraded.close().await.unwrap();
    }
}
