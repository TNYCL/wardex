//! SQLite persistence. Events are written by a single writer task in batches;
//! everything else goes through the pool directly.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc};

use crate::{
    DriverKind, EndReason, Event, EventKind, ListFilter, SessionId, SessionOrigin, SessionRecord,
    SessionState,
};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(
        "database error: {0}; inspect database access and disk space; use list/status or stop/interrupt, and daemon stop without --drain to exit"
    )]
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
    #[error(
        "persistence writer is retrying: {0}; inspect database access and disk space; use list/status and stop/interrupt, or daemon stop without --drain to exit"
    )]
    WriterFailed(String),
    #[error(
        "persistence writer is unavailable; inspect the daemon logs and use daemon stop without --drain before restarting"
    )]
    WriterStopped,
    #[error(
        "persistence queue is full ({0} writes); new work was not applied; wait for database recovery or use stop/interrupt"
    )]
    QueueFull(usize),
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
    writer: mpsc::Sender<QueuedItem>,
    state: Arc<Mutex<WriterState>>,
    changed: Arc<Notify>,
    capacity: Arc<Semaphore>,
}

#[derive(Default)]
struct WriterState {
    next_seq: u64,
    pending: BTreeMap<u64, SessionId>,
    quarantined: HashSet<SessionId>,
    error: Option<String>,
    global_failure: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Persistence {
    pub persisted: bool,
    pub pending_writes: usize,
    pub quarantined_sessions: Vec<SessionId>,
    pub warnings: Vec<String>,
}

struct QueuedItem {
    seq: u64,
    item: Item,
    _permit: OwnedSemaphorePermit,
}

pub(crate) struct PublicationPermit {
    channel: mpsc::OwnedPermit<QueuedItem>,
    budget: OwnedSemaphorePermit,
}

enum Item {
    Event(Box<Event>),
    Exit {
        events: Vec<Event>,
        record: Box<SessionRecord>,
    },
    Publish {
        event: Box<Event>,
        record: Box<SessionRecord>,
    },
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
const QUEUE_LIMIT: usize = 4096;
const RETRY_MIN: Duration = Duration::from_millis(25);
const RETRY_MAX: Duration = Duration::from_secs(2);

impl Item {
    fn session(&self) -> &SessionId {
        match self {
            Self::Event(event) | Self::Publish { event, .. } => &event.session_id,
            Self::Exit { record, .. } => &record.id,
            Self::Session(record) => &record.id,
            Self::Question(question) => &question.session_id,
            Self::Answer { session, .. } | Self::Cancel(session) => session,
        }
    }
}

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

        let (tx, rx) = mpsc::channel(QUEUE_LIMIT);
        let state = Arc::new(Mutex::new(WriterState::default()));
        let changed = Arc::new(Notify::new());
        let capacity = Arc::new(Semaphore::new(QUEUE_LIMIT));
        tokio::spawn(writer_loop(
            pool.clone(),
            rx,
            state.clone(),
            changed.clone(),
        ));
        Ok(Self {
            pool,
            path: path.to_path_buf(),
            writer: tx,
            state,
            changed,
            capacity,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Queue an event for persistence. Deltas must not be passed here.
    pub fn append_event(&self, event: Event) -> Result<()> {
        self.enqueue(Item::Event(Box::new(event)))
    }

    /// Queue a new question row (ordered with events).
    pub fn queue_question(&self, q: QuestionRow) -> Result<()> {
        self.enqueue(Item::Question(Box::new(q)))
    }

    /// Queue an answer for a question row (ordered with its insert).
    pub fn queue_answer(
        &self,
        session: &SessionId,
        question_id: &str,
        answer: serde_json::Value,
        at_ms: i64,
    ) -> Result<()> {
        self.enqueue(Item::Answer {
            session: session.clone(),
            question_id: question_id.to_string(),
            answer,
            at_ms,
        })
    }

    /// Queue cancelling all open questions of a session.
    pub fn queue_cancel(&self, session: &SessionId) -> Result<()> {
        self.enqueue(Item::Cancel(session.clone()))
    }

    /// Queue a session row update. Applied in order with queued events, so a
    /// later snapshot never gets overwritten by an earlier one.
    pub fn queue_session_update(&self, record: SessionRecord) -> Result<()> {
        self.enqueue(Item::Session(Box::new(record)))
    }

    pub fn persistence(&self) -> Persistence {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut warnings = Vec::new();
        if let Some(error) = &state.error {
            warnings.push(StoreError::WriterFailed(error.clone()).to_string());
        }
        if self.writer.is_closed() {
            warnings.push(StoreError::WriterStopped.to_string());
        }
        if self.pool.is_closed() {
            warnings.push(StoreError::Db(sqlx::Error::PoolClosed).to_string());
        }
        let mut quarantined_sessions: Vec<_> = state.quarantined.iter().cloned().collect();
        quarantined_sessions.sort();
        Persistence {
            persisted: state.pending.is_empty() && warnings.is_empty(),
            pending_writes: state.pending.len(),
            quarantined_sessions,
            warnings,
        }
    }

    /// Reject new work before side effects while the whole database is down,
    /// this session is quarantined, or the bounded backlog has no room.
    pub fn check_admission(&self, session: Option<&SessionId>) -> Result<()> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if self.writer.is_closed() {
            return Err(StoreError::WriterStopped);
        }
        if self.pool.is_closed() {
            return Err(StoreError::Db(sqlx::Error::PoolClosed));
        }
        if state.global_failure || session.is_some_and(|id| state.quarantined.contains(id)) {
            return Err(StoreError::WriterFailed(
                state.error.clone().unwrap_or_default(),
            ));
        }
        if self.capacity.available_permits() == 0 {
            return Err(StoreError::QueueFull(QUEUE_LIMIT));
        }
        Ok(())
    }

    pub(crate) fn reserve_publication(&self) -> Result<PublicationPermit> {
        let budget = self
            .capacity
            .clone()
            .try_acquire_owned()
            .map_err(|_| StoreError::QueueFull(QUEUE_LIMIT))?;
        let channel = self
            .writer
            .clone()
            .try_reserve_owned()
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => StoreError::QueueFull(QUEUE_LIMIT),
                mpsc::error::TrySendError::Closed(_) => StoreError::WriterStopped,
            })?;
        Ok(PublicationPermit { channel, budget })
    }

    fn send_reserved(&self, item: Item, permit: PublicationPermit) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.next_seq += 1;
        let seq = state.next_seq;
        state.pending.insert(seq, item.session().clone());
        permit.channel.send(QueuedItem {
            seq,
            item,
            _permit: permit.budget,
        });
    }

    fn enqueue(&self, item: Item) -> Result<()> {
        let permit = self.reserve_publication()?;
        self.send_reserved(item, permit);
        Ok(())
    }

    pub(crate) fn publish_reserved(
        &self,
        event: Event,
        record: SessionRecord,
        permit: PublicationPermit,
    ) {
        self.send_reserved(
            Item::Publish {
                event: Box::new(event),
                record: Box::new(record),
            },
            permit,
        );
    }

    pub(crate) fn session_reserved(&self, record: SessionRecord, permit: PublicationPermit) {
        self.send_reserved(Item::Session(Box::new(record)), permit);
    }

    pub(crate) async fn reserve_wait(&self) -> Result<PublicationPermit> {
        let budget = self
            .capacity
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| StoreError::WriterStopped)?;
        let channel = self
            .writer
            .clone()
            .reserve_owned()
            .await
            .map_err(|_| StoreError::WriterStopped)?;
        Ok(PublicationPermit { channel, budget })
    }

    pub(crate) fn exit_reserved(
        &self,
        events: Vec<Event>,
        record: SessionRecord,
        permit: PublicationPermit,
    ) {
        self.send_reserved(
            Item::Exit {
                events,
                record: Box::new(record),
            },
            permit,
        );
    }

    /// A barrier over accepted writes. Errors are returned promptly while
    /// background retries retain the writes; a later flush succeeds on repair.
    pub async fn flush(&self) -> Result<()> {
        self.flush_selected(None).await
    }

    pub async fn flush_session(&self, session: &SessionId) -> Result<()> {
        self.flush_selected(Some(session)).await
    }

    async fn flush_selected(&self, session: Option<&SessionId>) -> Result<()> {
        let target = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .next_seq;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if self.writer.is_closed() {
                    return Err(StoreError::WriterStopped);
                }
                if !state
                    .pending
                    .range(..=target)
                    .any(|(_, id)| session.is_none_or(|session| session == id))
                {
                    return Ok(());
                }
                if let Some(error) = &state.error
                    && (session.is_none()
                        || state.global_failure
                        || session.is_some_and(|id| state.quarantined.contains(id)))
                {
                    return Err(StoreError::WriterFailed(error.clone()));
                }
            }
            tokio::select! {
                _ = notified => {}
                _ = self.writer.closed() => return Err(StoreError::WriterStopped),
            }
        }
    }

    pub async fn insert_session(&self, s: &SessionRecord) -> Result<()> {
        sqlx::query(
            "INSERT INTO sessions (id, driver, driver_session_id, state, agent, task, model,
                reasoning, cwd, origin_cwd, worktree, branch, remove_worktree, readonly, slice,
                unit, pid, created_ms, updated_ms, ended_ms, last_seq, last_result, last_error,
                spawn_count, resume_pending, resume_with_prompt, cleanup_failed, ended_reason,
                origin, forked_from, adopted_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                ?, ?, ?)",
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
        .bind(s.cleanup_failed)
        .bind(s.ended_reason.map(EndReason::as_str))
        .bind(s.origin.as_str())
        .bind(&s.forked_from)
        .bind(s.adopted_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The newest session bound to this driver session id.
    pub async fn find_by_driver_session(
        &self,
        driver: DriverKind,
        driver_session_id: &str,
    ) -> Result<Option<SessionId>> {
        let row = sqlx::query(
            "SELECT id FROM sessions WHERE driver = ? AND driver_session_id = ?
             ORDER BY created_ms DESC, rowid DESC LIMIT 1",
        )
        .bind(driver.as_str())
        .bind(driver_session_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| SessionId(r.get("id"))))
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

    /// Wardex session per driver session id (the newest when several share
    /// one).
    pub async fn managed_driver_ids(&self) -> Result<HashMap<(DriverKind, String), SessionId>> {
        let rows = sqlx::query(
            "SELECT id, driver, driver_session_id FROM sessions
             WHERE driver_session_id IS NOT NULL ORDER BY created_ms, rowid",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut out = HashMap::new();
        for r in rows {
            let driver: String = r.get("driver");
            if let Some(kind) = DriverKind::parse(&driver) {
                out.insert(
                    (kind, r.get::<String, _>("driver_session_id")),
                    SessionId(r.get("id")),
                );
            }
        }
        Ok(out)
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
        let filter = ListFilter {
            all,
            ..Default::default()
        };
        self.list_sessions_matching(&filter, limit).await
    }

    /// Sessions passing `filter` (see [`ListFilter::matches`]; the working
    /// directory of `resumable` is not checked here), newest first; sessions
    /// created in the same millisecond newest-inserted first.
    pub async fn list_sessions_matching(
        &self,
        filter: &ListFilter,
        limit: i64,
    ) -> Result<Vec<SessionRecord>> {
        let mut q = sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT * FROM sessions WHERE ");
        if filter.ended || filter.resumable {
            q.push("state IN ('done','error','stopped','orphaned')");
        } else if filter.all {
            q.push("1");
        } else {
            q.push("state IN ('starting','running','idle','waiting_input')");
        }
        if filter.resumable {
            q.push(" AND driver_session_id IS NOT NULL AND cleanup_failed = 0");
        }
        if let Some(pattern) = &filter.agent {
            q.push(" AND agent GLOB ")
                .push_bind(crate::glob::to_sqlite(pattern));
        }
        if let Some(pattern) = &filter.task {
            q.push(" AND task GLOB ")
                .push_bind(crate::glob::to_sqlite(pattern));
        }
        if let Some(origin) = filter.origin {
            q.push(" AND origin = ").push_bind(origin.as_str());
        }
        // rowid breaks ties between sessions created in the same millisecond.
        q.push(" ORDER BY created_ms DESC, rowid DESC LIMIT ")
            .push_bind(limit);
        let rows = q.build().fetch_all(&self.pool).await?;
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

    pub async fn close(&self) -> Result<()> {
        let result = self.flush().await;
        self.pool.close().await;
        result
    }
}

async fn writer_loop(
    pool: SqlitePool,
    mut rx: mpsc::Receiver<QueuedItem>,
    state: Arc<Mutex<WriterState>>,
    changed: Arc<Notify>,
) {
    let mut ready = VecDeque::new();
    let mut quarantine: VecDeque<QueuedItem> = VecDeque::new();
    let mut backoff = RETRY_MIN;
    loop {
        if ready.is_empty() && quarantine.is_empty() {
            match rx.recv().await {
                Some(item) => ready.push_back(item),
                None => return,
            }
        }
        while let Ok(item) = rx.try_recv() {
            ready.push_back(item);
        }
        // Retry quarantined sessions first, preserving their original order.
        quarantine.append(&mut ready);
        std::mem::swap(&mut ready, &mut quarantine);
        let mut blocked = HashSet::new();
        let mut global_error = false;
        while !ready.is_empty() {
            let mut batch = Vec::new();
            while batch.len() < BATCH {
                let Some(item) = ready.pop_front() else { break };
                if blocked.contains(item.item.session()) {
                    quarantine.push_back(item);
                } else {
                    batch.push(item);
                }
            }
            if batch.is_empty() {
                continue;
            }
            let refs: Vec<_> = batch.iter().map(|q| &q.item).collect();
            match write_batch(&pool, &refs).await {
                Ok(()) => complete(&state, &changed, &batch),
                Err(error) if record_error(&error) => {
                    // Split a failed transaction. Isolate only the poisoned
                    // session, so unrelated records can still be committed.
                    let mut items = batch.into_iter();
                    while let Some(item) = items.next() {
                        if blocked.contains(item.item.session()) {
                            quarantine.push_back(item);
                            continue;
                        }
                        match write_batch(&pool, &[&item.item]).await {
                            Ok(()) => complete(&state, &changed, std::slice::from_ref(&item)),
                            Err(error) if record_error(&error) => {
                                let id = item.item.session().clone();
                                blocked.insert(id.clone());
                                let mut health = state.lock().unwrap_or_else(|e| e.into_inner());
                                health.error = Some(format!(
                                    "quarantined session {id}: {error}; fix the offending constraint or record; retries continue automatically"
                                ));
                                health.quarantined.insert(id);
                                drop(health);
                                quarantine.push_back(item);
                                changed.notify_waiters();
                            }
                            Err(error) => {
                                // A global failure can start while splitting a
                                // constraint batch. Retain the rest in order;
                                // do not misclassify an outage as poison data.
                                let mut health = state.lock().unwrap_or_else(|e| e.into_inner());
                                health.error = Some(error.to_string());
                                health.global_failure = true;
                                drop(health);
                                for rest in items.rev() {
                                    ready.push_front(rest);
                                }
                                ready.push_front(item);
                                changed.notify_waiters();
                                global_error = true;
                                break;
                            }
                        }
                    }
                    if global_error {
                        break;
                    }
                }
                Err(error) => {
                    let mut health = state.lock().unwrap_or_else(|e| e.into_inner());
                    health.error = Some(error.to_string());
                    health.global_failure = true;
                    drop(health);
                    changed.notify_waiters();
                    for item in batch.into_iter().rev() {
                        ready.push_front(item);
                    }
                    global_error = true;
                    break;
                }
            }
        }
        {
            let mut health = state.lock().unwrap_or_else(|e| e.into_inner());
            health.global_failure = global_error;
            health.quarantined = blocked;
            if !global_error && quarantine.is_empty() {
                health.error = None;
                backoff = RETRY_MIN;
            }
        }
        changed.notify_waiters();
        if global_error || !quarantine.is_empty() {
            tracing::warn!(
                pending = state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .pending
                    .len(),
                retry_ms = backoff.as_millis(),
                "persistence retries pending"
            );
            // Incoming writes wake admission, but do not reset exponential
            // backoff or turn a permanent error into a busy loop.
            let deadline = tokio::time::Instant::now() + backoff;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => break,
                    item = rx.recv() => match item {
                        Some(item) => ready.push_back(item),
                        None => return,
                    }
                }
            }
            backoff = (backoff * 2).min(RETRY_MAX);
        }
    }
}

fn complete(state: &Mutex<WriterState>, changed: &Notify, items: &[QueuedItem]) {
    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
    for item in items {
        state.pending.remove(&item.seq);
    }
    changed.notify_waiters();
}

fn record_error(error: &StoreError) -> bool {
    match error {
        StoreError::Corrupt(_) => true,
        StoreError::Db(sqlx::Error::Database(error)) => error
            .code()
            .and_then(|code| code.parse::<u32>().ok())
            .is_some_and(|code| matches!(code & 255, 18..=20)),
        _ => false,
    }
}

async fn write_batch(pool: &SqlitePool, batch: &[&Item]) -> Result<()> {
    let mut tx = pool.begin().await?;
    for item in batch {
        match *item {
            Item::Exit { events, record } => {
                for event in events {
                    insert_event_query(event)?.execute(&mut *tx).await?;
                }
                update_session_query(record).execute(&mut *tx).await?;
                cancel_questions_query(&record.id).execute(&mut *tx).await?;
            }
            Item::Publish { event, record } => {
                insert_event_query(event)?.execute(&mut *tx).await?;
                update_session_query(record).execute(&mut *tx).await?;
                match &event.kind {
                    kind @ (EventKind::Question { .. } | EventKind::Approval { .. }) => {
                        let (question_id, name) = match kind {
                            EventKind::Question { question_id, .. } => (question_id, "question"),
                            EventKind::Approval { request_id, .. } => (request_id, "approval"),
                            _ => unreachable!(),
                        };
                        let question = QuestionRow {
                            session_id: event.session_id.clone(),
                            question_id: question_id.clone(),
                            kind: name.into(),
                            body: serde_json::to_value(kind)
                                .map_err(|e| StoreError::Corrupt(e.to_string()))?,
                            asked_ms: event.ts_ms,
                            answered_ms: None,
                            answer: None,
                            cancelled: false,
                        };
                        insert_question_query(&question).execute(&mut *tx).await?;
                    }
                    EventKind::Answered {
                        question_id,
                        answer,
                    } => {
                        let answer = serde_json::to_value(answer)
                            .map_err(|e| StoreError::Corrupt(e.to_string()))?;
                        answer_question_query(&event.session_id, question_id, &answer, event.ts_ms)
                            .execute(&mut *tx)
                            .await?;
                    }
                    EventKind::Ended { .. } => {
                        cancel_questions_query(&event.session_id)
                            .execute(&mut *tx)
                            .await?;
                    }
                    _ => {}
                }
            }
            Item::Event(ev) => {
                insert_event_query(ev)?.execute(&mut *tx).await?;
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

fn insert_event_query(event: &Event) -> Result<Query<'_>> {
    let body = serde_json::to_string(event).map_err(|e| StoreError::Corrupt(e.to_string()))?;
    Ok(sqlx::query(
        "INSERT OR REPLACE INTO events (session_id, seq, ts_ms, kind, body) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(event.session_id.as_str())
    .bind(event.seq as i64)
    .bind(event.ts_ms)
    .bind(event.kind.name())
    .bind(body))
}

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
    // `reasoning` is currently insert-only. Updating driver-reported effort
    // is a separate persistence change, outside the lifecycle fixes.
    sqlx::query(
        "UPDATE sessions SET driver_session_id = ?, state = ?, model = ?, unit = ?, pid = ?,
            updated_ms = ?, ended_ms = ?, last_seq = ?, last_result = ?, last_error = ?,
            spawn_count = ?, resume_pending = ?, resume_with_prompt = ?, cleanup_failed = ?,
            ended_reason = ?
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
    .bind(s.cleanup_failed)
    .bind(s.ended_reason.map(EndReason::as_str))
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
        cleanup_failed: r.get("cleanup_failed"),
        // Unknown values (from a newer binary) read as unknown, not corrupt.
        ended_reason: r
            .get::<Option<String>, _>("ended_reason")
            .and_then(|v| EndReason::parse(&v)),
        // Unknown origins (from a newer binary) read as external, so resume
        // keeps checking for another writer.
        origin: SessionOrigin::parse(&r.get::<String, _>("origin"))
            .unwrap_or(SessionOrigin::Adopted),
        forked_from: r.get("forked_from"),
        adopted_ms: r.get("adopted_ms"),
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
            cleanup_failed: false,
            ended_reason: None,
            origin: SessionOrigin::Wardex,
            forked_from: None,
            adopted_ms: None,
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
            store
                .append_event(event(
                    "abcd1234",
                    1,
                    EventKind::Prompt { text: "hi".into() },
                ))
                .unwrap();
            store
                .append_event(event(
                    "abcd1234",
                    2,
                    EventKind::Done {
                        result: Some("ok".into()),
                        interrupted: false,
                        note: None,
                        git: None,
                    },
                ))
                .unwrap();
            s.state = SessionState::Done;
            s.last_seq = 2;
            s.ended_reason = Some(EndReason::IdleShutdown);
            store.update_session(&s).await.unwrap();
            store.close().await.unwrap();
        }
        let store = Store::open(&path).await.unwrap();
        let s = store
            .get_session(&SessionId("abcd1234".into()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(s.state, SessionState::Done);
        assert_eq!(s.last_seq, 2);
        assert_eq!(s.ended_reason, Some(EndReason::IdleShutdown));
        // A reason written by a newer binary reads as unknown, not corrupt.
        sqlx::query("UPDATE sessions SET ended_reason = 'future_reason'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            store
                .get_session(&s.id)
                .await
                .unwrap()
                .unwrap()
                .ended_reason,
            None
        );
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
    async fn migration_0005_defaults_existing_sessions_to_wardex() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.db");
        // A database as the previous release left it (migrations 1-4).
        let old = dir.path().join("migrations");
        std::fs::create_dir(&old).unwrap();
        for entry in std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/migrations")).unwrap()
        {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.as_str() < "0005" {
                std::fs::copy(entry.path(), old.join(name)).unwrap();
            }
        }
        {
            let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
                .unwrap()
                .create_if_missing(true);
            let pool = SqlitePoolOptions::new()
                .connect_with(options)
                .await
                .unwrap();
            sqlx::migrate::Migrator::new(old.as_path())
                .await
                .unwrap()
                .run(&pool)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO sessions (id, driver, driver_session_id, state, cwd, created_ms,
                    updated_ms) VALUES ('old00001', 'claude', 'native-1', 'done', '/w', 1, 1)",
            )
            .execute(&pool)
            .await
            .unwrap();
            pool.close().await;
        }
        let store = Store::open(&path).await.unwrap();
        let s = store
            .get_session(&SessionId("old00001".into()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(s.origin, SessionOrigin::Wardex);
        assert_eq!(s.forked_from, None);
        assert_eq!(s.adopted_ms, None);
        assert_eq!(
            store
                .find_by_driver_session(DriverKind::Claude, "native-1")
                .await
                .unwrap(),
            Some(s.id.clone())
        );
        assert_eq!(
            store
                .find_by_driver_session(DriverKind::Codex, "native-1")
                .await
                .unwrap(),
            None
        );
        let mut adopted = record("new00001", SessionState::Stopped);
        adopted.origin = SessionOrigin::Forked;
        adopted.forked_from = Some("native-2".into());
        adopted.adopted_ms = Some(5);
        store.insert_session(&adopted).await.unwrap();
        let back = store.get_session(&adopted.id).await.unwrap().unwrap();
        assert_eq!(back, adopted);
        let forked = ListFilter {
            all: true,
            origin: Some(SessionOrigin::Forked),
            ..Default::default()
        };
        let rows = store.list_sessions_matching(&forked, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, adopted.id);
    }

    #[tokio::test]
    async fn list_filters_run_in_sql() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("w.db")).await.unwrap();
        let rows = [
            (
                "s1",
                SessionState::Idle,
                Some("agent-a"),
                Some("fix-1"),
                true,
                false,
            ),
            (
                "s2",
                SessionState::Done,
                Some("agent-a"),
                Some("fix-2"),
                true,
                false,
            ),
            (
                "s3",
                SessionState::Stopped,
                Some("agent-b"),
                Some("[x]-3"),
                true,
                false,
            ),
            (
                "s4",
                SessionState::Error,
                Some("agent-a"),
                None,
                false,
                false,
            ),
            (
                "s5",
                SessionState::Orphaned,
                None,
                Some("fix-5"),
                true,
                true,
            ),
        ];
        for (i, (id, state, agent, task, resume_id, cleanup_failed)) in rows.iter().enumerate() {
            let mut r = record(id, *state);
            r.agent = agent.map(String::from);
            r.task = task.map(String::from);
            r.driver_session_id = resume_id.then(|| format!("d-{id}"));
            r.cleanup_failed = *cleanup_failed;
            r.created_ms = i as i64;
            store.insert_session(&r).await.unwrap();
        }
        let ids = |filter: ListFilter| {
            let store = store.clone();
            async move {
                store
                    .list_sessions_matching(&filter, 100)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|r| r.id.0)
                    .collect::<Vec<_>>()
            }
        };
        let f = |agent: Option<&str>, task: Option<&str>, all, ended, resumable| ListFilter {
            all,
            agent: agent.map(String::from),
            task: task.map(String::from),
            ended,
            resumable,
            origin: None,
        };
        assert_eq!(ids(f(None, None, false, false, false)).await, ["s1"]);
        assert_eq!(
            ids(f(None, None, true, false, false)).await,
            ["s5", "s4", "s3", "s2", "s1"]
        );
        assert_eq!(
            ids(f(None, None, false, true, false)).await,
            ["s5", "s4", "s3", "s2"]
        );
        assert_eq!(ids(f(None, None, false, false, true)).await, ["s3", "s2"]);
        assert_eq!(
            ids(f(Some("agent-a"), None, true, false, false)).await,
            ["s4", "s2", "s1"]
        );
        assert_eq!(ids(f(Some("*-b"), None, true, false, false)).await, ["s3"]);
        assert_eq!(
            ids(f(None, Some("fix-?"), true, false, false)).await,
            ["s5", "s2", "s1"]
        );
        assert_eq!(
            ids(f(Some("agent-a"), Some("fix-*"), false, true, false)).await,
            ["s2"]
        );
        // `[` is literal, as in webhook globs; a pattern never matches NULL.
        assert_eq!(
            ids(f(None, Some("[x]-*"), true, false, false)).await,
            ["s3"]
        );
        assert_eq!(
            ids(f(Some("*"), None, true, false, false)).await,
            ["s4", "s3", "s2", "s1"]
        );
        assert_eq!(
            store
                .list_sessions_matching(&f(None, None, true, false, false), 2)
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn sessions_created_in_the_same_millisecond_list_newest_inserted_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("w.db")).await.unwrap();
        // Ids sort neither with nor against insertion order.
        let inserted = ["s3", "s1", "s4", "s2", "s5"];
        for id in inserted {
            let mut r = record(id, SessionState::Done);
            r.created_ms = 7;
            store.insert_session(&r).await.unwrap();
        }
        let expected: Vec<_> = inserted.iter().rev().map(|id| id.to_string()).collect();
        for filter in [
            ListFilter {
                all: true,
                ..Default::default()
            },
            ListFilter {
                ended: true,
                agent: Some("a".into()),
                ..Default::default()
            },
        ] {
            let ids: Vec<_> = store
                .list_sessions_matching(&filter, 100)
                .await
                .unwrap()
                .into_iter()
                .map(|r| r.id.0)
                .collect();
            assert_eq!(ids, expected);
        }
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
    async fn poison_batch_is_split_and_retried_after_constraint_repair() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("w.db")).await.unwrap();
        let mut session = record("s1", SessionState::Running);
        store.insert_session(&session).await.unwrap();
        sqlx::query("CREATE TRIGGER reject_event BEFORE INSERT ON events WHEN NEW.session_id = 'missing' BEGIN SELECT RAISE(FAIL, 'injected batch failure'); END")
            .execute(&store.pool).await.unwrap();
        store
            .append_event(event(
                "s1",
                1,
                EventKind::Prompt {
                    text: "keep".into(),
                },
            ))
            .unwrap();
        session.state = SessionState::Idle;
        store.queue_session_update(session.clone()).unwrap();
        // This injected SQLite error rolls back the earlier event and snapshot.
        store
            .append_event(event(
                "missing",
                1,
                EventKind::Prompt {
                    text: "fail".into(),
                },
            ))
            .unwrap();
        let (first, second) = tokio::join!(store.flush(), store.flush());
        for result in [first, second, store.flush().await] {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("quarantined session"), "{error}");
            assert!(error.contains("automatically"), "{error}");
        }
        assert_eq!(store.events(&session.id, 0, 10).await.unwrap().len(), 1);
        assert_eq!(
            store.get_session(&session.id).await.unwrap().unwrap().state,
            SessionState::Idle
        );
        sqlx::query("DROP TRIGGER reject_event")
            .execute(&store.pool)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while store.flush().await.is_err() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert!(store.persistence().quarantined_sessions.is_empty());
        store.queue_session_update(session).unwrap();
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn closed_pool_is_reported_to_flush_waiters() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("w.db")).await.unwrap();
        store.pool.close().await;
        store
            .append_event(event(
                "s1",
                1,
                EventKind::Prompt {
                    text: "fail".into(),
                },
            ))
            .unwrap();
        let (first, second) = tokio::join!(store.flush(), store.flush());
        assert!(matches!(first, Err(StoreError::WriterFailed(_))));
        assert!(matches!(second, Err(StoreError::WriterFailed(_))));
        assert!(store.flush().await.is_err());
    }

    #[tokio::test]
    async fn stopped_writer_cannot_acknowledge_flush_or_enqueue() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("w.db")).await.unwrap();
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        store.writer = tx;
        assert!(matches!(
            store.flush().await,
            Err(StoreError::WriterStopped)
        ));
        assert!(matches!(
            store.queue_cancel(&SessionId("s1".into())),
            Err(StoreError::WriterStopped)
        ));
        assert!(matches!(
            store.close().await,
            Err(StoreError::WriterStopped)
        ));
    }

    #[tokio::test]
    async fn transient_writer_failure_recovers_without_losing_accepted_events() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("w.db")).await.unwrap();
        sqlx::query("CREATE TRIGGER reject_writes BEFORE INSERT ON events BEGIN SELECT RAISE(FAIL, 'temporary failure'); END").execute(&store.pool).await.unwrap();
        store
            .append_event(event(
                "s1",
                1,
                EventKind::Prompt {
                    text: "first".into(),
                },
            ))
            .unwrap();
        assert!(store.flush().await.is_err());
        let accepted_during_outage = store.append_event(event(
            "s1",
            2,
            EventKind::Prompt {
                text: "second".into(),
            },
        ));
        sqlx::query("DROP TRIGGER reject_writes")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(
            accepted_during_outage.is_ok(),
            "writer rejected all events during a temporary outage"
        );
        tokio::time::timeout(Duration::from_secs(3), async {
            while store.flush().await.is_err() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("writer did not recover in the background");
        assert_eq!(
            store
                .events(&SessionId("s1".into()), 0, 10)
                .await
                .unwrap()
                .len(),
            2
        );
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn poison_record_does_not_block_unrelated_events() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("w.db")).await.unwrap();
        sqlx::query("CREATE TRIGGER reject_poison BEFORE INSERT ON events WHEN NEW.session_id = 'poison' BEGIN SELECT RAISE(FAIL, 'poison record'); END").execute(&store.pool).await.unwrap();
        for id in ["good-before", "poison", "good-after"] {
            store
                .append_event(event(
                    id,
                    1,
                    EventKind::Prompt {
                        text: "keep".into(),
                    },
                ))
                .unwrap();
        }
        let _ = store.flush().await;
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let before = store
                    .events(&SessionId("good-before".into()), 0, 10)
                    .await
                    .unwrap();
                let after = store
                    .events(&SessionId("good-after".into()), 0, 10)
                    .await
                    .unwrap();
                if before.len() == 1 && after.len() == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("one poison record stalled unrelated events");
        sqlx::query("DROP TRIGGER reject_poison")
            .execute(&store.pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn writer_queue_has_a_finite_limit() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("w.db")).await.unwrap();
        let rejected = (1..=5000)
            .filter(|seq| {
                store
                    .append_event(event(
                        "s1",
                        *seq,
                        EventKind::Prompt {
                            text: "queued".into(),
                        },
                    ))
                    .is_err()
            })
            .count();
        assert!(rejected > 0, "the writer accepted an unbounded backlog");
    }

    #[tokio::test]
    async fn writer_disappearance_wakes_an_existing_flush() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("w.db")).await.unwrap();
        let (tx, rx) = mpsc::channel(1);
        store.writer = tx;
        store
            .append_event(event(
                "s1",
                1,
                EventKind::Prompt {
                    text: "queued".into(),
                },
            ))
            .unwrap();
        let other = store.clone();
        let flush = tokio::spawn(async move { other.flush().await });
        tokio::task::yield_now().await;
        drop(rx);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), flush)
                .await
                .unwrap()
                .unwrap(),
            Err(StoreError::WriterStopped)
        ));
        store.pool.close().await;
    }

    #[tokio::test]
    async fn quarantine_preserves_session_order_and_allows_other_session_flushes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("w.db")).await.unwrap();
        sqlx::query("CREATE TRIGGER reject_first BEFORE INSERT ON events WHEN NEW.session_id = 'blocked' AND NEW.seq = 1 BEGIN SELECT RAISE(FAIL, 'poison first event'); END").execute(&store.pool).await.unwrap();
        for (id, seq) in [("blocked", 1), ("blocked", 2), ("healthy", 1)] {
            store
                .append_event(event(
                    id,
                    seq,
                    EventKind::Prompt {
                        text: "queued".into(),
                    },
                ))
                .unwrap();
        }
        assert!(store.flush().await.is_err());
        store
            .flush_session(&SessionId("healthy".into()))
            .await
            .unwrap();
        assert_eq!(
            store
                .events(&SessionId("healthy".into()), 0, 10)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            store
                .events(&SessionId("blocked".into()), 0, 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .check_admission(Some(&SessionId("blocked".into())))
                .is_err()
        );
        assert!(
            store
                .check_admission(Some(&SessionId("healthy".into())))
                .is_ok()
        );
        sqlx::query("DROP TRIGGER reject_first")
            .execute(&store.pool)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while store.flush().await.is_err() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            store
                .events(&SessionId("blocked".into()), 0, 10)
                .await
                .unwrap()
                .iter()
                .map(|e| e.seq)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn global_database_outage_recovers_and_reopens_admission() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("w.db")).await.unwrap();
        store
            .insert_session(&record("s1", SessionState::Running))
            .await
            .unwrap();
        let mut connections = Vec::new();
        for _ in 0..4 {
            let mut connection = store.pool.acquire().await.unwrap();
            sqlx::query("PRAGMA busy_timeout = 1")
                .execute(&mut *connection)
                .await
                .unwrap();
            connections.push(connection);
        }
        drop(connections);
        let lock_pool = SqlitePool::connect(&format!("sqlite://{}", store.path().display()))
            .await
            .unwrap();
        let mut lock = lock_pool.begin().await.unwrap();
        sqlx::query("UPDATE sessions SET task = 'locked'")
            .execute(&mut *lock)
            .await
            .unwrap();
        store
            .append_event(event(
                "s1",
                1,
                EventKind::Prompt {
                    text: "first".into(),
                },
            ))
            .unwrap();
        assert!(store.flush().await.is_err());
        assert!(store.persistence().quarantined_sessions.is_empty());
        assert!(store.check_admission(None).is_err());
        // Agent output is retained during the outage, though new work is denied.
        store
            .append_event(event(
                "s1",
                2,
                EventKind::Prompt {
                    text: "second".into(),
                },
            ))
            .unwrap();
        lock.commit().await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while store.flush().await.is_err() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert!(store.check_admission(None).is_ok());
        assert_eq!(
            store
                .events(&SessionId("s1".into()), 0, 10)
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(store.persistence().persisted);
        store.close().await.unwrap();
        lock_pool.close().await;
    }

    #[tokio::test]
    async fn flush_waits_for_a_locked_database_and_preserves_the_batch() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("w.db")).await.unwrap();
        store
            .insert_session(&record("s1", SessionState::Running))
            .await
            .unwrap();
        let mut lock = store.pool.begin().await.unwrap();
        sqlx::query("UPDATE sessions SET task = 'locked'")
            .execute(&mut *lock)
            .await
            .unwrap();
        store
            .append_event(event(
                "s1",
                1,
                EventKind::Prompt {
                    text: "queued".into(),
                },
            ))
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), store.flush())
                .await
                .is_err()
        );
        lock.commit().await.unwrap();
        store.flush().await.unwrap();
        assert_eq!(
            store
                .events(&SessionId("s1".into()), 0, 10)
                .await
                .unwrap()
                .len(),
            1
        );
        store.close().await.unwrap();
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
            store
                .append_event(event("old", seq, EventKind::Prompt { text: "x".into() }))
                .unwrap();
            store
                .append_event(event("live", seq, EventKind::Prompt { text: "x".into() }))
                .unwrap();
        }
        store.flush().await.unwrap();
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
