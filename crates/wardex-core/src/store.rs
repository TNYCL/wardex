//! SQLite persistence. Events are written by a single writer task in batches;
//! everything else goes through the pool directly.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};
use tokio::sync::{mpsc, oneshot};

use crate::{DriverKind, Event, SessionId, SessionRecord, SessionState};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("cannot create {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("corrupt row: {0}")]
    Corrupt(String),
}

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// A question or approval as stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionRow {
    pub session_id: SessionId,
    pub question_id: String,
    /// `question` or `approval`.
    pub kind: String,
    /// The `EventKind` payload that asked it, as JSON.
    pub body: serde_json::Value,
    pub asked_ms: i64,
    pub answered_ms: Option<i64>,
    pub answer: Option<serde_json::Value>,
    pub cancelled: bool,
}

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
    path: PathBuf,
    writer: mpsc::UnboundedSender<WriterMsg>,
}

enum WriterMsg {
    Item(Item),
    Flush(oneshot::Sender<()>),
}

enum Item {
    Event(Box<Event>),
    Session(Box<SessionRecord>),
    Question(Box<QuestionRow>),
    Answer {
        session: SessionId,
        question_id: String,
        answer: serde_json::Value,
        at_ms: i64,
    },
    Cancel(SessionId),
}

const BATCH: usize = 256;

impl Store {
    /// Open (and create) the database and start the event writer. Must be
    /// called inside a Tokio runtime.
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| StoreError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
        }
        let url = format!("sqlite://{}", path.display());
        let options = SqliteConnectOptions::from_str(&url)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(10))
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;

        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(writer_loop(pool.clone(), rx));
        Ok(Self {
            pool,
            path: path.to_path_buf(),
            writer: tx,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Queue an event for persistence. Deltas must not be passed here.
    pub fn append_event(&self, event: Event) {
        let _ = self
            .writer
            .send(WriterMsg::Item(Item::Event(Box::new(event))));
    }

    /// Queue a new question row (ordered with events).
    pub fn queue_question(&self, q: QuestionRow) {
        let _ = self
            .writer
            .send(WriterMsg::Item(Item::Question(Box::new(q))));
    }

    /// Queue an answer for a question row (ordered with its insert).
    pub fn queue_answer(
        &self,
        session: &SessionId,
        question_id: &str,
        answer: serde_json::Value,
        at_ms: i64,
    ) {
        let _ = self.writer.send(WriterMsg::Item(Item::Answer {
            session: session.clone(),
            question_id: question_id.to_string(),
            answer,
            at_ms,
        }));
    }

    /// Queue cancelling all open questions of a session.
    pub fn queue_cancel(&self, session: &SessionId) {
        let _ = self
            .writer
            .send(WriterMsg::Item(Item::Cancel(session.clone())));
    }

    /// Queue a session row update. Applied in order with queued events, so a
    /// later snapshot never gets overwritten by an earlier one.
    pub fn queue_session_update(&self, record: SessionRecord) {
        let _ = self
            .writer
            .send(WriterMsg::Item(Item::Session(Box::new(record))));
    }

    /// Wait until every event queued so far is committed.
    pub async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        if self.writer.send(WriterMsg::Flush(tx)).is_ok() {
            let _ = rx.await;
        }
    }

    pub async fn insert_session(&self, s: &SessionRecord) -> Result<()> {
        sqlx::query(
            "INSERT INTO sessions (id, driver, driver_session_id, state, agent, task, model,
                reasoning, cwd, origin_cwd, worktree, branch, remove_worktree, readonly, slice,
                unit, pid, created_ms, updated_ms, ended_ms, last_seq, last_result, last_error,
                spawn_count, resume_pending, resume_with_prompt)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(s.id.as_str())
        .bind(s.driver.as_str())
        .bind(&s.driver_session_id)
        .bind(s.state.as_str())
        .bind(&s.agent)
        .bind(&s.task)
        .bind(&s.model)
        .bind(&s.reasoning)
        .bind(s.cwd.to_string_lossy().into_owned())
        .bind(
            s.origin_cwd
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
        )
        .bind(&s.worktree)
        .bind(&s.branch)
        .bind(s.remove_worktree)
        .bind(s.readonly)
        .bind(&s.slice)
        .bind(&s.unit)
        .bind(s.pid.map(i64::from))
        .bind(s.created_ms)
        .bind(s.updated_ms)
        .bind(s.ended_ms)
        .bind(s.last_seq as i64)
        .bind(&s.last_result)
        .bind(&s.last_error)
        .bind(i64::from(s.spawn_count))
        .bind(s.resume_pending)
        .bind(s.resume_with_prompt)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Orphaned sessions waiting for an automatic resume, oldest first.
    pub async fn pending_resumes(&self) -> Result<Vec<SessionRecord>> {
        let rows = sqlx::query(
            "SELECT * FROM sessions WHERE state = 'orphaned' AND resume_pending = 1
             ORDER BY created_ms",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(session_from_row).collect()
    }

    /// Write the mutable columns of a session row.
    pub async fn update_session(&self, s: &SessionRecord) -> Result<()> {
        update_session_query(s).execute(&self.pool).await?;
        Ok(())
    }

    /// Highest persisted event sequence number of a session.
    pub async fn max_seq(&self, id: &SessionId) -> Result<u64> {
        let row = sqlx::query("SELECT COALESCE(MAX(seq), 0) AS m FROM events WHERE session_id = ?")
            .bind(id.as_str())
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<i64, _>("m") as u64)
    }

    pub async fn session_exists(&self, id: &str) -> Result<bool> {
        let row = sqlx::query("SELECT 1 FROM sessions WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    pub async fn get_session(&self, id: &SessionId) -> Result<Option<SessionRecord>> {
        let row = sqlx::query("SELECT * FROM sessions WHERE id = ?")
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| session_from_row(&r)).transpose()
    }

    /// Session ids starting with `prefix` (at most `limit`).
    pub async fn find_sessions(&self, prefix: &str, limit: i64) -> Result<Vec<SessionId>> {
        let escaped = prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let rows = sqlx::query("SELECT id FROM sessions WHERE id LIKE ? ESCAPE '\\' LIMIT ?")
            .bind(format!("{escaped}%"))
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .iter()
            .map(|r| SessionId(r.get::<String, _>("id")))
            .collect())
    }

    /// Sessions, newest first. Without `all`, only non-terminal ones.
    pub async fn list_sessions(&self, all: bool, limit: i64) -> Result<Vec<SessionRecord>> {
        let sql = if all {
            "SELECT * FROM sessions ORDER BY created_ms DESC LIMIT ?"
        } else {
            "SELECT * FROM sessions WHERE state IN ('starting','running','idle','waiting_input')
             ORDER BY created_ms DESC LIMIT ?"
        };
        let rows = sqlx::query(sql).bind(limit).fetch_all(&self.pool).await?;
        rows.iter().map(session_from_row).collect()
    }

    /// Sessions that ended at or after `since_ms`, most recent first.
    pub async fn ended_since(&self, since_ms: i64, limit: i64) -> Result<Vec<SessionRecord>> {
        let rows = sqlx::query(
            "SELECT * FROM sessions WHERE ended_ms >= ? ORDER BY ended_ms DESC LIMIT ?",
        )
        .bind(since_ms)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(session_from_row).collect()
    }

    pub async fn sessions_in_states(&self, states: &[SessionState]) -> Result<Vec<SessionRecord>> {
        let mut out = Vec::new();
        for state in states {
            let rows = sqlx::query("SELECT * FROM sessions WHERE state = ?")
                .bind(state.as_str())
                .fetch_all(&self.pool)
                .await?;
            for row in &rows {
                out.push(session_from_row(row)?);
            }
        }
        Ok(out)
    }

    /// Persisted events with `seq > since_seq`, oldest first.
    pub async fn events(&self, id: &SessionId, since_seq: u64, limit: i64) -> Result<Vec<Event>> {
        let rows = sqlx::query(
            "SELECT body FROM events WHERE session_id = ? AND seq > ? ORDER BY seq LIMIT ?",
        )
        .bind(id.as_str())
        .bind(since_seq as i64)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let body: String = r.get("body");
                serde_json::from_str(&body).map_err(|e| StoreError::Corrupt(e.to_string()))
            })
            .collect()
    }

    /// The last `n` persisted events, oldest first.
    pub async fn last_events(&self, id: &SessionId, n: i64) -> Result<Vec<Event>> {
        let rows =
            sqlx::query("SELECT body FROM events WHERE session_id = ? ORDER BY seq DESC LIMIT ?")
                .bind(id.as_str())
                .bind(n)
                .fetch_all(&self.pool)
                .await?;
        let mut events: Vec<Event> = rows
            .iter()
            .map(|r| {
                let body: String = r.get("body");
                serde_json::from_str(&body).map_err(|e| StoreError::Corrupt(e.to_string()))
            })
            .collect::<Result<_>>()?;
        events.reverse();
        Ok(events)
    }

    /// Event counts per type.
    pub async fn event_counts(&self, id: &SessionId) -> Result<Vec<(String, i64)>> {
        let rows = sqlx::query(
            "SELECT kind, COUNT(*) AS n FROM events WHERE session_id = ? GROUP BY kind ORDER BY kind",
        )
        .bind(id.as_str())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| (r.get::<String, _>("kind"), r.get::<i64, _>("n")))
            .collect())
    }

    /// Finished tool calls per tool name.
    pub async fn tool_counts(&self, id: &SessionId) -> Result<Vec<(String, i64)>> {
        let rows = sqlx::query(
            "SELECT json_extract(body, '$.payload.name') AS name, COUNT(*) AS n FROM events
             WHERE session_id = ? AND kind = 'tool_call'
               AND json_extract(body, '$.payload.status') != 'started'
             GROUP BY name ORDER BY n DESC",
        )
        .bind(id.as_str())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    r.get::<Option<String>, _>("name").unwrap_or_default(),
                    r.get::<i64, _>("n"),
                )
            })
            .collect())
    }

    /// The most recent persisted event of a type.
    pub async fn last_event_of(&self, id: &SessionId, kind: &str) -> Result<Option<Event>> {
        let row = sqlx::query(
            "SELECT body FROM events WHERE session_id = ? AND kind = ? ORDER BY seq DESC LIMIT 1",
        )
        .bind(id.as_str())
        .bind(kind)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| {
            let body: String = r.get("body");
            serde_json::from_str(&body).map_err(|e| StoreError::Corrupt(e.to_string()))
        })
        .transpose()
    }

    /// The last event of `kind` with `seq < before_seq`.
    pub async fn last_event_before(
        &self,
        id: &SessionId,
        kind: &str,
        before_seq: u64,
    ) -> Result<Option<Event>> {
        let row = sqlx::query(
            "SELECT body FROM events WHERE session_id = ? AND kind = ? AND seq < ?
             ORDER BY seq DESC LIMIT 1",
        )
        .bind(id.as_str())
        .bind(kind)
        .bind(before_seq as i64)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| decode_body(&r)).transpose()
    }

    /// One event by sequence number.
    pub async fn event_at(&self, id: &SessionId, seq: u64) -> Result<Option<Event>> {
        let row = sqlx::query("SELECT body FROM events WHERE session_id = ? AND seq = ?")
            .bind(id.as_str())
            .bind(seq as i64)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| decode_body(&r)).transpose()
    }

    /// All `tool_call` events of one tool call, oldest first.
    pub async fn tool_call_events(&self, id: &SessionId, call_id: &str) -> Result<Vec<Event>> {
        let rows = sqlx::query(
            "SELECT body FROM events WHERE session_id = ? AND kind = 'tool_call'
             AND json_extract(body, '$.payload.call_id') = ? ORDER BY seq",
        )
        .bind(id.as_str())
        .bind(call_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(decode_body).collect()
    }

    pub async fn insert_question(&self, q: &QuestionRow) -> Result<()> {
        insert_question_query(q).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn answer_question(
        &self,
        session: &SessionId,
        question_id: &str,
        answer: &serde_json::Value,
        at_ms: i64,
    ) -> Result<()> {
        answer_question_query(session, question_id, answer, at_ms)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Mark all open questions of a session as cancelled.
    pub async fn cancel_questions(&self, session: &SessionId) -> Result<()> {
        cancel_questions_query(session).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn open_questions(&self, session: Option<&SessionId>) -> Result<Vec<QuestionRow>> {
        let rows = match session {
            Some(id) => {
                sqlx::query(
                    "SELECT * FROM questions WHERE session_id = ? AND answered_ms IS NULL
                     AND cancelled = 0 ORDER BY asked_ms",
                )
                .bind(id.as_str())
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "SELECT * FROM questions WHERE answered_ms IS NULL AND cancelled = 0
                     ORDER BY asked_ms",
                )
                .fetch_all(&self.pool)
                .await?
            }
        };
        rows.iter().map(question_from_row).collect()
    }

    /// Apply retention: drop events of ended sessions older than
    /// `events_days`, then the oldest of them while the file is above
    /// `max_db_mb`. Returns the number of deleted events.
    pub async fn prune(&self, events_days: u64, max_db_mb: u64, now_ms: i64) -> Result<u64> {
        let mut deleted = 0;
        if events_days > 0 {
            let cutoff = now_ms - (events_days as i64) * 86_400_000;
            let r = sqlx::query(
                "DELETE FROM events WHERE ts_ms < ? AND session_id IN
                    (SELECT id FROM sessions WHERE ended_ms IS NOT NULL)",
            )
            .bind(cutoff)
            .execute(&self.pool)
            .await?;
            deleted += r.rows_affected();
        }
        if max_db_mb > 0 {
            let limit = max_db_mb * 1024 * 1024;
            for _ in 0..20 {
                if self.size_bytes().await? <= limit {
                    break;
                }
                let r = sqlx::query(
                    "DELETE FROM events WHERE (session_id, seq) IN
                        (SELECT e.session_id, e.seq FROM events e
                         JOIN sessions s ON s.id = e.session_id
                         WHERE s.ended_ms IS NOT NULL ORDER BY e.ts_ms LIMIT 10000)",
                )
                .execute(&self.pool)
                .await?;
                if r.rows_affected() == 0 {
                    break;
                }
                deleted += r.rows_affected();
            }
        }
        if deleted > 0 {
            sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
                .execute(&self.pool)
                .await
                .ok();
        }
        Ok(deleted)
    }

    /// Bytes in use by the database (excluding free pages that SQLite reuses).
    pub async fn size_bytes(&self) -> Result<u64> {
        let row = sqlx::query(
            "SELECT (p.page_count - f.freelist_count) * s.page_size AS used
             FROM pragma_page_count() p, pragma_freelist_count() f, pragma_page_size() s",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get::<i64, _>("used").max(0) as u64)
    }

    pub async fn close(&self) {
        self.flush().await;
        self.pool.close().await;
    }
}

async fn writer_loop(pool: SqlitePool, mut rx: mpsc::UnboundedReceiver<WriterMsg>) {
    let mut batch: Vec<Item> = Vec::with_capacity(BATCH);
    let mut waiters: Vec<oneshot::Sender<()>> = Vec::new();
    while let Some(msg) = rx.recv().await {
        push(&mut batch, &mut waiters, msg);
        while batch.len() < BATCH {
            match rx.try_recv() {
                Ok(msg) => push(&mut batch, &mut waiters, msg),
                Err(_) => break,
            }
        }
        if !batch.is_empty() {
            if let Err(e) = write_batch(&pool, &batch).await {
                tracing::error!(error = %e, count = batch.len(), "failed to persist events");
            }
            batch.clear();
        }
        for w in waiters.drain(..) {
            let _ = w.send(());
        }
    }
}

fn push(batch: &mut Vec<Item>, waiters: &mut Vec<oneshot::Sender<()>>, msg: WriterMsg) {
    match msg {
        WriterMsg::Item(item) => batch.push(item),
        WriterMsg::Flush(tx) => waiters.push(tx),
    }
}

async fn write_batch(pool: &SqlitePool, batch: &[Item]) -> Result<()> {
    let mut tx = pool.begin().await?;
    for item in batch {
        match item {
            Item::Event(ev) => {
                let body =
                    serde_json::to_string(ev).map_err(|e| StoreError::Corrupt(e.to_string()))?;
                sqlx::query(
                    "INSERT OR REPLACE INTO events (session_id, seq, ts_ms, kind, body)
                     VALUES (?, ?, ?, ?, ?)",
                )
                .bind(ev.session_id.as_str())
                .bind(ev.seq as i64)
                .bind(ev.ts_ms)
                .bind(ev.kind.name())
                .bind(body)
                .execute(&mut *tx)
                .await?;
            }
            Item::Session(s) => {
                update_session_query(s).execute(&mut *tx).await?;
            }
            Item::Question(q) => {
                insert_question_query(q).execute(&mut *tx).await?;
            }
            Item::Answer {
                session,
                question_id,
                answer,
                at_ms,
            } => {
                answer_question_query(session, question_id, answer, *at_ms)
                    .execute(&mut *tx)
                    .await?;
            }
            Item::Cancel(session) => {
                cancel_questions_query(session).execute(&mut *tx).await?;
            }
        }
    }
    tx.commit().await?;
    Ok(())
}

type Query<'a> = sqlx::query::Query<'a, sqlx::Sqlite, sqlx::sqlite::SqliteArguments>;

fn insert_question_query(q: &QuestionRow) -> Query<'_> {
    sqlx::query(
        "INSERT OR REPLACE INTO questions (session_id, question_id, kind, body, asked_ms,
            answered_ms, answer, cancelled) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(q.session_id.as_str())
    .bind(&q.question_id)
    .bind(&q.kind)
    .bind(q.body.to_string())
    .bind(q.asked_ms)
    .bind(q.answered_ms)
    .bind(q.answer.as_ref().map(|a| a.to_string()))
    .bind(q.cancelled)
}

fn answer_question_query<'a>(
    session: &'a SessionId,
    question_id: &'a str,
    answer: &serde_json::Value,
    at_ms: i64,
) -> Query<'a> {
    sqlx::query(
        "UPDATE questions SET answered_ms = ?, answer = ? WHERE session_id = ? AND question_id = ?",
    )
    .bind(at_ms)
    .bind(answer.to_string())
    .bind(session.as_str())
    .bind(question_id)
}

fn cancel_questions_query(session: &SessionId) -> Query<'_> {
    sqlx::query("UPDATE questions SET cancelled = 1 WHERE session_id = ? AND answered_ms IS NULL")
        .bind(session.as_str())
}

fn update_session_query(
    s: &SessionRecord,
) -> sqlx::query::Query<'_, sqlx::Sqlite, sqlx::sqlite::SqliteArguments> {
    sqlx::query(
        "UPDATE sessions SET driver_session_id = ?, state = ?, model = ?, unit = ?, pid = ?,
            updated_ms = ?, ended_ms = ?, last_seq = ?, last_result = ?, last_error = ?,
            spawn_count = ?, resume_pending = ?, resume_with_prompt = ?
         WHERE id = ?",
    )
    .bind(&s.driver_session_id)
    .bind(s.state.as_str())
    .bind(&s.model)
    .bind(&s.unit)
    .bind(s.pid.map(i64::from))
    .bind(s.updated_ms)
    .bind(s.ended_ms)
    .bind(s.last_seq as i64)
    .bind(&s.last_result)
    .bind(&s.last_error)
    .bind(i64::from(s.spawn_count))
    .bind(s.resume_pending)
    .bind(s.resume_with_prompt)
    .bind(s.id.as_str())
}

fn opt_path(v: Option<String>) -> Option<PathBuf> {
    v.map(PathBuf::from)
}

fn decode_body(row: &sqlx::sqlite::SqliteRow) -> Result<Event> {
    let body: String = row.get("body");
    serde_json::from_str(&body).map_err(|e| StoreError::Corrupt(e.to_string()))
}

fn session_from_row(r: &sqlx::sqlite::SqliteRow) -> Result<SessionRecord> {
    let driver: String = r.get("driver");
    let state: String = r.get("state");
    Ok(SessionRecord {
        id: SessionId(r.get("id")),
        driver: DriverKind::parse(&driver)
            .ok_or_else(|| StoreError::Corrupt(format!("driver {driver}")))?,
        driver_session_id: r.get("driver_session_id"),
        state: SessionState::parse(&state)
            .ok_or_else(|| StoreError::Corrupt(format!("state {state}")))?,
        agent: r.get("agent"),
        task: r.get("task"),
        model: r.get("model"),
        reasoning: r.get("reasoning"),
        cwd: PathBuf::from(r.get::<String, _>("cwd")),
        origin_cwd: opt_path(r.get("origin_cwd")),
        worktree: r.get("worktree"),
        branch: r.get("branch"),
        remove_worktree: r.get("remove_worktree"),
        readonly: r.get("readonly"),
        slice: r.get("slice"),
        unit: r.get("unit"),
        pid: r.get::<Option<i64>, _>("pid").map(|p| p as u32),
        created_ms: r.get("created_ms"),
        updated_ms: r.get("updated_ms"),
        ended_ms: r.get("ended_ms"),
        last_seq: r.get::<i64, _>("last_seq") as u64,
        last_result: r.get("last_result"),
        last_error: r.get("last_error"),
        spawn_count: r.get::<i64, _>("spawn_count") as u32,
        resume_pending: r.get("resume_pending"),
        resume_with_prompt: r.get("resume_with_prompt"),
    })
}

fn question_from_row(r: &sqlx::sqlite::SqliteRow) -> Result<QuestionRow> {
    let body: String = r.get("body");
    let answer: Option<String> = r.get("answer");
    Ok(QuestionRow {
        session_id: SessionId(r.get("session_id")),
        question_id: r.get("question_id"),
        kind: r.get("kind"),
        body: serde_json::from_str(&body).map_err(|e| StoreError::Corrupt(e.to_string()))?,
        asked_ms: r.get("asked_ms"),
        answered_ms: r.get("answered_ms"),
        answer: answer
            .map(|a| serde_json::from_str(&a))
            .transpose()
            .map_err(|e| StoreError::Corrupt(e.to_string()))?,
        cancelled: r.get("cancelled"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventKind;

    fn record(id: &str, state: SessionState) -> SessionRecord {
        SessionRecord {
            id: SessionId(id.into()),
            driver: DriverKind::Claude,
            driver_session_id: None,
            state,
            agent: Some("a".into()),
            task: None,
            model: None,
            reasoning: None,
            cwd: PathBuf::from("/work"),
            origin_cwd: None,
            worktree: None,
            branch: None,
            remove_worktree: false,
            readonly: false,
            slice: None,
            unit: None,
            pid: None,
            created_ms: 1,
            updated_ms: 1,
            ended_ms: None,
            last_seq: 0,
            last_result: None,
            last_error: None,
            spawn_count: 1,
            resume_pending: false,
            resume_with_prompt: false,
        }
    }

    fn event(id: &str, seq: u64, kind: EventKind) -> Event {
        Event {
            session_id: SessionId(id.into()),
            seq,
            ts_ms: seq as i64,
            agent: None,
            task: None,
            driver: DriverKind::Claude,
            kind,
        }
    }

    #[tokio::test]
    async fn sessions_and_events_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.db");
        {
            let store = Store::open(&path).await.unwrap();
            let mut s = record("abcd1234", SessionState::Running);
            store.insert_session(&s).await.unwrap();
            store.append_event(event(
                "abcd1234",
                1,
                EventKind::Prompt { text: "hi".into() },
            ));
            store.append_event(event(
                "abcd1234",
                2,
                EventKind::Done {
                    result: Some("ok".into()),
                    interrupted: false,
                    note: None,
                    git: None,
                },
            ));
            s.state = SessionState::Done;
            s.last_seq = 2;
            store.update_session(&s).await.unwrap();
            store.close().await;
        }
        let store = Store::open(&path).await.unwrap();
        let s = store
            .get_session(&SessionId("abcd1234".into()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(s.state, SessionState::Done);
        assert_eq!(s.last_seq, 2);
        let events = store.events(&s.id, 0, 100).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].kind.name(), "done");
        assert_eq!(store.events(&s.id, 1, 100).await.unwrap().len(), 1);
        assert_eq!(store.find_sessions("abc", 5).await.unwrap().len(), 1);
        assert!(store.find_sessions("zz", 5).await.unwrap().is_empty());
        assert!(store.list_sessions(false, 10).await.unwrap().is_empty());
        assert_eq!(store.list_sessions(true, 10).await.unwrap().len(), 1);
        let counts = store.event_counts(&s.id).await.unwrap();
        assert_eq!(counts, vec![("done".into(), 1), ("prompt".into(), 1)]);
    }

    #[tokio::test]
    async fn questions_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("w.db")).await.unwrap();
        store
            .insert_session(&record("s1", SessionState::WaitingInput))
            .await
            .unwrap();
        let q = QuestionRow {
            session_id: SessionId("s1".into()),
            question_id: "q1".into(),
            kind: "question".into(),
            body: serde_json::json!({"x": 1}),
            asked_ms: 5,
            answered_ms: None,
            answer: None,
            cancelled: false,
        };
        store.insert_question(&q).await.unwrap();
        assert_eq!(store.open_questions(None).await.unwrap().len(), 1);
        store
            .answer_question(&q.session_id, "q1", &serde_json::json!("blue"), 9)
            .await
            .unwrap();
        assert!(
            store
                .open_questions(Some(&q.session_id))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn prune_keeps_live_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("w.db")).await.unwrap();
        let mut ended = record("old", SessionState::Done);
        ended.ended_ms = Some(10);
        store.insert_session(&ended).await.unwrap();
        store
            .insert_session(&record("live", SessionState::Running))
            .await
            .unwrap();
        for seq in 1..=3 {
            store.append_event(event("old", seq, EventKind::Prompt { text: "x".into() }));
            store.append_event(event("live", seq, EventKind::Prompt { text: "x".into() }));
        }
        store.flush().await;
        let deleted = store.prune(1, 0, 10 * 86_400_000).await.unwrap();
        assert_eq!(deleted, 3);
        assert_eq!(
            store
                .events(&SessionId("live".into()), 0, 10)
                .await
                .unwrap()
                .len(),
            3
        );
    }
}
