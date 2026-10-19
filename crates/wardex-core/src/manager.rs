//! The session manager: starts sessions through drivers, numbers and
//! persists their events, tracks live state, answers questions, detects
//! stuck sessions and applies retention.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};

use crate::config::SessionsConfig;
use crate::driver::{Driver, DriverCommand, DriverError, DriverEvent, READONLY_PREFIX, SpawnSpec};
use crate::process::{self, Launcher};
use crate::store::{Persistence, PublicationPermit, QuestionRow, Store, StoreError};
use crate::time::now_ms;
use crate::{
    Answer, Config, DriverKind, EndReason, Event, EventKind, GitState, RunSpec, SessionId,
    SessionRecord, SessionState, StuckReason, ToolStatus, git,
};

const BROADCAST_CAPACITY: usize = 8192;
const LIVE_STATES: [SessionState; 4] = [
    SessionState::Starting,
    SessionState::Running,
    SessionState::Idle,
    SessionState::WaitingInput,
];

#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    #[error("no session matches {0:?}")]
    NotFound(String),
    #[error("{0:?} matches several sessions; use more characters")]
    Ambiguous(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{message}: {}; no turns were interrupted. {}", join_ids(.sessions), drain_next_steps(.sessions, .cleanup_failed).join(" "))]
    DrainBlocked {
        message: String,
        sessions: Vec<SessionId>,
        closed_sessions: Vec<SessionId>,
        cleanup_failed: Vec<SessionId>,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Driver(#[from] DriverError),
}

pub type Result<T, E = ManagerError> = std::result::Result<T, E>;

/// How a caller answers a pending question or approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerInput {
    /// One answer for a single-question request, or a decision for an
    /// approval (`allow`/`yes`/`accept` or `deny`/`no`/`decline`).
    Text(String),
    /// Answers per question item id.
    Items(BTreeMap<String, String>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunOutcome {
    pub session: SessionRecord,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrainReport {
    pub closed_sessions: Vec<SessionId>,
    pub interrupted_sessions: Vec<SessionId>,
    pub persisted: bool,
    pub warnings: Vec<String>,
}

pub const DRAIN_NEXT_STEPS: [&str; 3] = [
    "1) Use wardex answer <id> or wardex stop <id> for blocked sessions.",
    "2) Retry wardex daemon restart --drain N.",
    "3) Or use wardex daemon restart --drain N --force to interrupt them explicitly.",
];

pub fn drain_next_steps(sessions: &[SessionId], cleanup_failed: &[SessionId]) -> [String; 3] {
    let mut steps = DRAIN_NEXT_STEPS.map(String::from);
    if !cleanup_failed.is_empty() {
        steps[0] = format!(
            "1) Use wardex resume <id> to retry scope cleanup for cleanup_failed sessions: {}.",
            join_ids(cleanup_failed)
        );
        if sessions.iter().any(|id| !cleanup_failed.contains(id)) {
            steps[0].push_str(
                " Use wardex answer <id> or wardex stop <id> only for the other blocked sessions.",
            );
        }
    }
    steps
}

fn join_ids(ids: &[SessionId]) -> String {
    ids.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionView {
    #[serde(flatten)]
    pub session: SessionRecord,
    pub live: bool,
    pub pending_questions: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingView {
    pub question_id: String,
    /// `question` or `approval`.
    pub kind: String,
    pub asked_ms: i64,
    pub request: EventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStatus {
    #[serde(flatten)]
    pub session: SessionRecord,
    pub live: bool,
    pub rss_bytes: Option<u64>,
    pub cpu_percent: Option<f64>,
    pub pending: Vec<PendingView>,
    pub running_tools: Vec<String>,
    pub last_activity_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub driver: DriverKind,
    pub state: SessionState,
    pub agent: Option<String>,
    pub task: Option<String>,
    pub model: Option<String>,
    pub cwd: PathBuf,
    pub created_ms: i64,
    /// When the agent process ended. Stays null while the process is alive,
    /// including `idle` sessions waiting for their next message.
    pub ended_ms: Option<i64>,
    /// From creation to the end of the last turn; while a turn runs (or none
    /// has ended yet), to now.
    pub duration_ms: i64,
    /// When the last turn ended (`done` or `error`).
    pub last_turn_ended_ms: Option<i64>,
    /// How long the last turn took, from its prompt to its end.
    pub last_turn_ms: Option<i64>,
    pub prompts: i64,
    pub turns: i64,
    pub event_counts: BTreeMap<String, i64>,
    pub tool_counts: BTreeMap<String, i64>,
    pub last_result: Option<String>,
    pub last_note: Option<String>,
    pub last_error: Option<String>,
    pub git: Option<GitState>,
}

/// Everything a client needs at a glance: live sessions with their state,
/// questions, resources and latest events, plus recently ended sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Overview {
    pub now_ms: i64,
    /// Live sessions per state.
    pub counts: BTreeMap<String, usize>,
    pub pending_questions: usize,
    pub live: Vec<OverviewSession>,
    pub recently_ended: Vec<EndedSession>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverviewSession {
    pub id: SessionId,
    pub driver: DriverKind,
    pub state: SessionState,
    pub agent: Option<String>,
    pub task: Option<String>,
    pub model: Option<String>,
    pub cwd: PathBuf,
    pub branch: Option<String>,
    pub readonly: bool,
    pub created_ms: i64,
    pub duration_ms: i64,
    pub last_activity_ms: i64,
    /// Since when the session has been idle (no turn, no question).
    pub idle_since_ms: Option<i64>,
    /// Time left until an idle session's process is closed
    /// (`sessions.idle_shutdown_secs`); `send` resumes it afterwards.
    pub shutdown_in_ms: Option<i64>,
    pub last_seq: u64,
    pub rss_bytes: Option<u64>,
    pub cpu_percent: Option<f64>,
    pub running_tools: Vec<String>,
    pub pending: Vec<PendingView>,
    pub latest: Vec<EventBrief>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndedSession {
    pub id: SessionId,
    pub driver: DriverKind,
    pub state: SessionState,
    pub agent: Option<String>,
    pub task: Option<String>,
    pub ended_ms: Option<i64>,
    pub last_seq: u64,
    /// Truncated; `summary` has the full text.
    pub last_result: Option<String>,
    pub last_error: Option<String>,
    #[serde(default)]
    pub cleanup_failed: bool,
    #[serde(default)]
    pub ended_reason: Option<EndReason>,
}

/// One-line form of an event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventBrief {
    pub seq: u64,
    pub ts_ms: i64,
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
}

impl From<&Event> for EventBrief {
    fn from(e: &Event) -> Self {
        Self {
            seq: e.seq,
            ts_ms: e.ts_ms,
            kind: e.kind.name().into(),
            text: truncate(&e.kind.describe(), OVERVIEW_TEXT_LIMIT),
        }
    }
}

const OVERVIEW_TEXT_LIMIT: usize = 300;
/// How far back `overview` lists ended sessions.
const OVERVIEW_ENDED_WINDOW_MS: i64 = 24 * 3600 * 1000;
const OVERVIEW_ENDED_LIMIT: i64 = 20;

/// `idle_since_ms` of a session that is really idle.
fn idle_since(l: &Live) -> Option<i64> {
    (l.record.state == SessionState::Idle && !l.turn_active && l.pending.is_empty())
        .then_some(l.idle_since_ms)
        .flatten()
}

/// A live session that is idle (no turn, no open question) and not closing.
#[derive(Debug, Clone)]
struct IdleCandidate {
    id: SessionId,
    idle_since_ms: i64,
}

/// Which idle sessions supervision closes now, and why. Longest idle first:
/// those idle for `idle_shutdown_secs`, then those beyond `max_idle_live`,
/// then, while available memory is below `min_mem_available_mb`, one more,
/// unless another idle close is happening now or a memory close is still
/// waiting for its process to exit.
fn idle_closures(
    mut idle: Vec<IdleCandidate>,
    now: i64,
    s: &SessionsConfig,
    mem_available_mb: Option<u64>,
    memory_close_pending: bool,
) -> Vec<(SessionId, EndReason)> {
    idle.sort_by(|a, b| (a.idle_since_ms, &a.id).cmp(&(b.idle_since_ms, &b.id)));
    let shutdown_ms = secs_to_ms(s.idle_shutdown_secs);
    let (expired, mut kept): (Vec<_>, Vec<_>) = idle
        .into_iter()
        .partition(|c| shutdown_ms > 0 && now - c.idle_since_ms >= shutdown_ms);
    let mut out: Vec<_> = expired
        .into_iter()
        .map(|c| (c.id, EndReason::IdleShutdown))
        .collect();
    let max = s.max_idle_live as usize;
    if max > 0 && kept.len() > max {
        let excess = kept.len() - max;
        out.extend(kept.drain(..excess).map(|c| (c.id, EndReason::IdleEvicted)));
    }
    let low_memory = s.min_mem_available_mb > 0
        && mem_available_mb.is_some_and(|mb| mb < s.min_mem_available_mb);
    if low_memory
        && out.is_empty()
        && !memory_close_pending
        && let Some(oldest) = kept.into_iter().next()
    {
        out.push((oldest.id, EndReason::MemoryPressure));
    }
    out
}

/// Seconds from the config as milliseconds, saturating instead of wrapping.
fn secs_to_ms(secs: u64) -> i64 {
    i64::try_from(secs).unwrap_or(i64::MAX).saturating_mul(1000)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// One tool call with its full input and output (each capped at
/// `driver::TOOL_TEXT_LIMIT`). `status` is empty while the tool runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolOutput {
    pub call_id: String,
    pub name: String,
    pub started_seq: Option<u64>,
    pub finished_seq: Option<u64>,
    pub status: Option<ToolStatus>,
    pub input_summary: String,
    pub result_summary: Option<String>,
    pub input: Option<String>,
    pub output: Option<String>,
}

#[derive(Clone)]
pub struct Manager {
    inner: Arc<Inner>,
}

struct Inner {
    config: Config,
    store: Store,
    drivers: HashMap<DriverKind, Arc<dyn Driver>>,
    launcher: Launcher,
    live: Mutex<HashMap<SessionId, Live>>,
    records: Mutex<HashMap<SessionId, SessionRecord>>,
    read_warning: Mutex<Option<String>>,
    cleanup_failed: Mutex<HashMap<SessionId, String>>,
    starting: Mutex<HashSet<SessionId>>,
    tx: broadcast::Sender<Event>,
    shutting_down: AtomicBool,
    draining: AtomicBool,
    started_ms: i64,
}

struct Live {
    record: SessionRecord,
    commands: mpsc::UnboundedSender<DriverCommand>,
    pending: Vec<Pending>,
    /// call_id -> tool name
    running_tools: HashMap<String, String>,
    turn_active: bool,
    /// The agent reported `started`.
    started: bool,
    first_prompt_sent: bool,
    last_activity_ms: i64,
    idle_since_ms: Option<i64>,
    stuck_flagged: bool,
    stop_requested: bool,
    idle_exit: bool,
    /// Why supervision closed this idle process (with `idle_exit`).
    idle_reason: Option<EndReason>,
    drain_exit: bool,
    /// EOF cannot be undone when a drain is refused. Keep this separate
    /// from the exit classification flags, which the reservation resets.
    stdin_closed: bool,
    interrupt_requested: bool,
    cpu_sample: Option<(u64, i64)>,
    cpu_percent: Option<f64>,
    rss_bytes: Option<u64>,
}

struct Pending {
    question_id: String,
    request: EventKind,
    asked_ms: i64,
    reminded: bool,
    stuck: bool,
}

/// Held across async database reads and released on errors or cancellation.
struct StartReservation<'a> {
    inner: &'a Inner,
    id: SessionId,
}

impl Drop for StartReservation<'_> {
    fn drop(&mut self) {
        self.inner
            .starting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

struct DrainReservation<'a>(&'a Inner);

impl Drop for DrainReservation<'_> {
    fn drop(&mut self) {
        for entry in self
            .0
            .live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values_mut()
        {
            if entry.drain_exit {
                entry.drain_exit = false;
                entry.idle_exit = false;
            }
        }
        self.0.draining.store(false, Ordering::SeqCst);
    }
}

impl Pending {
    fn kind(&self) -> &'static str {
        match self.request {
            EventKind::Approval { .. } => "approval",
            _ => "question",
        }
    }
}

impl Live {
    fn recompute_state(&mut self) {
        let state = if !self.pending.is_empty() {
            SessionState::WaitingInput
        } else if self.turn_active {
            SessionState::Running
        } else if !self.started {
            SessionState::Starting
        } else {
            SessionState::Idle
        };
        self.record.state = state;
    }
}

fn make_event(record: &mut SessionRecord, kind: EventKind) -> Event {
    let now = now_ms();
    // Deltas share the sequence number of the last persisted event; they are
    // only ever delivered live.
    if !kind.is_delta() {
        record.last_seq += 1;
    }
    record.updated_ms = now;
    Event {
        session_id: record.id.clone(),
        seq: record.last_seq,
        ts_ms: now,
        agent: record.agent.clone(),
        task: record.task.clone(),
        driver: record.driver,
        kind,
    }
}

impl Manager {
    pub fn new(
        config: Config,
        store: Store,
        drivers: Vec<Arc<dyn Driver>>,
        launcher: Launcher,
    ) -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        let drivers = drivers.into_iter().map(|d| (d.kind(), d)).collect();
        Self {
            inner: Arc::new(Inner {
                config,
                store,
                drivers,
                launcher,
                live: Mutex::new(HashMap::new()),
                records: Mutex::new(HashMap::new()),
                read_warning: Mutex::new(None),
                cleanup_failed: Mutex::new(HashMap::new()),
                starting: Mutex::new(HashSet::new()),
                tx,
                shutting_down: AtomicBool::new(false),
                draining: AtomicBool::new(false),
                started_ms: now_ms(),
            }),
        }
    }

    /// When this manager (the daemon) started.
    pub fn started_ms(&self) -> i64 {
        self.inner.started_ms
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    /// Live events of all sessions, including message deltas.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.tx.subscribe()
    }

    fn live(&self) -> MutexGuard<'_, HashMap<SessionId, Live>> {
        self.inner.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn reserve_start(&self, id: &SessionId) -> Result<StartReservation<'_>> {
        let mut starting = self
            .inner
            .starting
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.accepting_work()?;
        if self.live().contains_key(id) || !starting.insert(id.clone()) {
            return Err(ManagerError::Conflict(format!(
                "session {id} is already live or starting"
            )));
        }
        Ok(StartReservation {
            inner: &self.inner,
            id: id.clone(),
        })
    }

    fn accepting_work(&self) -> Result<()> {
        if self.inner.draining.load(Ordering::SeqCst)
            || self.inner.shutting_down.load(Ordering::SeqCst)
        {
            return Err(ManagerError::Conflict(
                "the daemon is draining or shutting down".into(),
            ));
        }
        Ok(())
    }

    fn cache(&self, record: &SessionRecord) {
        self.inner
            .records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(record.id.clone(), record.clone());
    }

    pub fn persistence(&self) -> Persistence {
        let mut persistence = self.inner.store.persistence();
        if let Some(warning) = self
            .inner
            .read_warning
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            persistence.persisted = false;
            persistence.warnings.push(warning);
        }
        persistence
    }

    async fn finish_action(&self) {
        if let Err(error) = self.inner.store.flush().await {
            tracing::warn!(%error, "action applied but persistence is pending; do not resend it");
        }
    }

    fn publish_reserved(&self, event: &Event, record: &SessionRecord, permit: PublicationPermit) {
        self.cache(record);
        self.inner
            .store
            .publish_reserved(event.clone(), record.clone(), permit);
        let _ = self.inner.tx.send(event.clone());
    }

    /// Supervision notices use nonblocking admission and retry on later ticks.
    fn emit_live(&self, id: &SessionId, kind: EventKind) -> Option<Event> {
        let permit = match self.inner.store.reserve_publication() {
            Ok(permit) => permit,
            Err(error) => {
                tracing::warn!(%error, %id, "notice deferred because persistence is full");
                return None;
            }
        };
        let mut live = self.live();
        let entry = live.get_mut(id)?;
        let event = make_event(&mut entry.record, kind);
        self.publish_reserved(&event, &entry.record, permit);
        Some(event)
    }

    fn emit_record(&self, record: &mut SessionRecord, kind: EventKind) -> Result<Event> {
        let permit = self.inner.store.reserve_publication()?;
        let event = make_event(record, kind);
        self.publish_reserved(&event, record, permit);
        Ok(event)
    }

    /// Resolve a full id or unique prefix.
    pub async fn resolve(&self, id: &str) -> Result<SessionId> {
        let id = id.trim();
        if id.is_empty() {
            return Err(ManagerError::Invalid("empty session id".into()));
        }
        let mut matches: Vec<_> = self
            .inner
            .records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .filter(|key| key.as_str().starts_with(id))
            .cloned()
            .collect();
        if let Some(exact) = matches.iter().find(|known| known.as_str() == id) {
            // Management of a known live/cached id must not wait for SQLite.
            return Ok(exact.clone());
        }
        match self.inner.store.find_sessions(id, 2).await {
            Ok(stored) => {
                *self
                    .inner
                    .read_warning
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = None;
                matches.extend(stored);
            }
            Err(error) if !matches.is_empty() => {
                *self
                    .inner
                    .read_warning
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(format!(
                    "Last database read failed; using cached sessions: {error}"
                ));
                tracing::warn!(%error, "resolving session from memory")
            }
            Err(error) => return Err(error.into()),
        }
        matches.sort();
        matches.dedup();
        match matches.len() {
            0 => Err(ManagerError::NotFound(id.into())),
            1 => Ok(matches
                .into_iter()
                .next()
                .unwrap_or_else(|| SessionId(id.into()))),
            _ => {
                if matches.iter().any(|m| m.as_str() == id) {
                    Ok(SessionId(id.into()))
                } else {
                    Err(ManagerError::Ambiguous(id.into()))
                }
            }
        }
    }

    async fn record(&self, id: &SessionId) -> Result<SessionRecord> {
        if let Some(l) = self.live().get(id) {
            return Ok(l.record.clone());
        }
        if let Some(record) = self
            .inner
            .records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
        {
            return Ok(record);
        }
        self.inner
            .store
            .get_session(id)
            .await?
            .ok_or_else(|| ManagerError::NotFound(id.to_string()))
    }

    /// Mark sessions that were still live when the daemon last stopped (a
    /// crash or kill) as orphaned, killing whatever is left in their scopes so
    /// a resume never runs next to a stray process. Returns their ids.
    pub async fn recover(&self) -> Result<Vec<SessionId>> {
        let stale = self.inner.store.sessions_in_states(&LIVE_STATES).await?;
        let mut ids = Vec::new();
        for mut record in stale {
            let cleanup_error = self.clean_record_scope(&mut record).await.err();
            record.last_seq = record
                .last_seq
                .max(self.inner.store.max_seq(&record.id).await?);
            record.resume_pending = true;
            record.resume_with_prompt = record.state != SessionState::Idle;
            record.state = SessionState::Orphaned;
            record.ended_ms = Some(now_ms());
            record.ended_reason = Some(EndReason::DaemonLost);
            record.pid = None;
            let message = cleanup_error
                .map(|error| error.to_string())
                .unwrap_or_else(|| {
                    "orphaned: the daemon stopped while the session was live".into()
                });
            record.last_error = Some(message.clone());
            self.emit_record(&mut record, EventKind::Error { message })?;
            self.emit_record(
                &mut record,
                EventKind::Ended {
                    state: SessionState::Orphaned,
                    exit_code: None,
                    reason: Some(EndReason::DaemonLost),
                },
            )?;
            ids.push(record.id.clone());
        }
        for mut record in self.inner.store.pending_resumes().await? {
            if ids.contains(&record.id) {
                continue;
            }
            // A previous failure requires an explicit operator retry, even
            // if the scope happens to become available on this daemon start.
            if record.cleanup_failed {
                let message = record.last_error.clone().unwrap_or_else(|| format!(
                    "cleanup_failed for session {}; use wardex resume <id> to retry scope cleanup",
                    record.id
                ));
                self.inner
                    .cleanup_failed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(record.id.clone(), message);
                self.cache(&record);
                tracing::warn!(session = %record.id, "cleanup_failed session requires explicit resume; continuing recovery");
                continue;
            }
            if record.unit.is_some() {
                if let Err(error) = self.clean_record_scope(&mut record).await {
                    tracing::warn!(session = %record.id, %error, "scope cleanup deferred; continuing recovery");
                }
                self.inner.store.update_session(&record).await?;
                self.cache(&record);
            }
        }
        self.inner.store.flush().await?;
        Ok(ids)
    }

    pub async fn run(&self, spec: RunSpec) -> Result<RunOutcome> {
        self.accepting_work()?;
        self.inner.store.check_admission(None)?;
        let driver = spec
            .driver
            .ok_or_else(|| ManagerError::Invalid("driver is required".into()))?;
        if !self.inner.drivers.contains_key(&driver) {
            return Err(ManagerError::Invalid(format!(
                "driver {} is not available",
                driver.as_str()
            )));
        }
        if spec.prompt.trim().is_empty() {
            return Err(ManagerError::Invalid("prompt is empty".into()));
        }
        if !spec.cwd.is_absolute() || !spec.cwd.is_dir() {
            return Err(ManagerError::Invalid(format!(
                "cwd {} is not an existing absolute directory",
                spec.cwd.display()
            )));
        }
        if let Some(slice) = &spec.slice
            && !self.inner.config.resources.slices.contains(slice)
        {
            return Err(ManagerError::Invalid(format!(
                "slice {slice:?} is not listed in resources.slices"
            )));
        }
        if spec.branch.is_some() && spec.worktree.is_none() {
            return Err(ManagerError::Invalid("--branch requires --worktree".into()));
        }

        let mut id = SessionId::generate();
        while self.inner.store.session_exists(id.as_str()).await? {
            id = SessionId::generate();
        }
        let _reservation = self.reserve_start(&id)?;

        let (cwd, origin_cwd) = match &spec.worktree {
            Some(name) => {
                let root = git::repo_root(&spec.cwd).await.ok_or_else(|| {
                    ManagerError::Invalid(format!(
                        "{} is not inside a git repository",
                        spec.cwd.display()
                    ))
                })?;
                let path = git::add_worktree(
                    &root,
                    &self.inner.config.sessions.worktree_dir,
                    name,
                    spec.branch.as_deref(),
                )
                .await
                .map_err(ManagerError::Invalid)?;
                (path, Some(spec.cwd.clone()))
            }
            None => (spec.cwd.clone(), None),
        };

        let mut warnings = Vec::new();
        for l in self.live().values() {
            if l.record.cwd == cwd {
                warnings.push(format!(
                    "session {} is also running in {}",
                    l.record.id,
                    cwd.display()
                ));
            }
        }

        let drivers_cfg = &self.inner.config.drivers;
        let (default_model, default_reasoning) = match driver {
            DriverKind::Claude => (
                drivers_cfg.claude.default_model.clone(),
                drivers_cfg.claude.default_reasoning.clone(),
            ),
            DriverKind::Codex => (
                drivers_cfg.codex.default_model.clone(),
                drivers_cfg.codex.default_reasoning.clone(),
            ),
        };
        let now = now_ms();
        let mut record = SessionRecord {
            id: id.clone(),
            driver,
            driver_session_id: None,
            state: SessionState::Starting,
            agent: spec.agent.clone(),
            task: spec.task.clone(),
            model: spec.model.clone().or(default_model),
            reasoning: spec.reasoning.clone().or(default_reasoning),
            cwd,
            origin_cwd,
            worktree: spec.worktree.clone(),
            branch: spec.branch.clone(),
            remove_worktree: spec.remove_worktree,
            readonly: spec.readonly,
            slice: spec.slice.clone(),
            unit: None,
            pid: None,
            created_ms: now,
            updated_ms: now,
            ended_ms: None,
            last_seq: 0,
            last_result: None,
            last_error: None,
            spawn_count: 0,
            resume_pending: false,
            resume_with_prompt: false,
            cleanup_failed: false,
            ended_reason: None,
        };
        let initial_publication = self.inner.store.reserve_publication()?;
        self.inner.store.insert_session(&record).await?;
        if let Err(e) = self.spawn_process(&mut record, None) {
            record.state = SessionState::Error;
            record.ended_ms = Some(now_ms());
            record.ended_reason = Some(EndReason::Error);
            record.last_error = Some(e.to_string());
            let event = make_event(
                &mut record,
                EventKind::Error {
                    message: e.to_string(),
                },
            );
            self.publish_reserved(&event, &record, initial_publication);
            self.finish_action().await;
            return Err(e);
        }
        self.deliver(&id, spec.prompt, true, Some(initial_publication))?;
        self.finish_action().await;
        let session = self.record(&id).await?;
        Ok(RunOutcome { session, warnings })
    }

    /// Spawn a process for `record` and register it as live.
    fn spawn_process(&self, record: &mut SessionRecord, resume: Option<String>) -> Result<()> {
        // Serialize process registration with the daemon shutdown gate.
        let _starting = self
            .inner
            .starting
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.inner.shutting_down.load(Ordering::SeqCst) {
            return Err(ManagerError::Conflict("the daemon is shutting down".into()));
        }
        let publication = self.inner.store.reserve_publication()?;
        let driver = self
            .inner
            .drivers
            .get(&record.driver)
            .ok_or_else(|| ManagerError::Invalid("driver not available".into()))?
            .clone();
        record.spawn_count += 1;
        let spec = SpawnSpec {
            session_id: record.id.clone(),
            cwd: record.cwd.clone(),
            model: record.model.clone(),
            reasoning: record.reasoning.clone(),
            readonly: record.readonly,
            resume,
            slice: record.slice.clone(),
            unit: format!("wardex-{}-{}", record.id, record.spawn_count),
        };
        let handle = driver.spawn(spec, &self.inner.launcher)?;
        record.pid = handle.pid;
        record.unit = handle.unit.clone();
        record.state = SessionState::Starting;
        record.ended_ms = None;
        record.ended_reason = None;
        record.updated_ms = now_ms();
        self.cache(record);
        self.inner
            .store
            .session_reserved(record.clone(), publication);

        let now = now_ms();
        let live = Live {
            record: record.clone(),
            commands: handle.commands,
            pending: Vec::new(),
            running_tools: HashMap::new(),
            turn_active: false,
            started: false,
            first_prompt_sent: false,
            last_activity_ms: now,
            idle_since_ms: Some(now),
            stuck_flagged: false,
            stop_requested: false,
            idle_exit: false,
            idle_reason: None,
            drain_exit: false,
            stdin_closed: false,
            interrupt_requested: false,
            cpu_sample: None,
            cpu_percent: None,
            rss_bytes: None,
        };
        self.live().insert(record.id.clone(), live);
        tracing::info!(session = %record.id, driver = record.driver.as_str(), pid = ?record.pid, "session process started");

        let this = self.clone();
        let id = record.id.clone();
        let generation = record.spawn_count;
        let mut events = handle.events;
        tokio::spawn(async move {
            while let Some(ev) = events.recv().await {
                match ev {
                    DriverEvent::Event(kind) => this.on_event(&id, generation, kind).await,
                    DriverEvent::Exited { code, stderr_tail } => {
                        this.on_exit(&id, generation, code, stderr_tail).await;
                        return;
                    }
                }
            }
            this.on_exit(&id, generation, None, Vec::new()).await;
        });
        Ok(())
    }

    /// Send a message to a live session.
    fn deliver(
        &self,
        id: &SessionId,
        text: String,
        starting: bool,
        publication: Option<PublicationPermit>,
    ) -> Result<()> {
        let mut live = self.live();
        if !starting {
            self.accepting_work()?;
        }
        let publication = match publication {
            Some(permit) => permit,
            None => self.inner.store.reserve_publication()?,
        };
        let entry = live
            .get_mut(id)
            .ok_or_else(|| ManagerError::Conflict(format!("session {id} is not live")))?;
        if entry.stop_requested || entry.idle_exit || entry.drain_exit || entry.stdin_closed {
            return Err(ManagerError::Conflict(format!(
                "session {id} is shutting down; retry after it exits"
            )));
        }
        let payload = if entry.record.readonly && !entry.first_prompt_sent {
            format!("{READONLY_PREFIX}{text}")
        } else {
            text.clone()
        };
        entry
            .commands
            .send(DriverCommand::Send(payload))
            .map_err(|_| ManagerError::Conflict(format!("session {id} is shutting down")))?;
        entry.first_prompt_sent = true;
        entry.turn_active = true;
        entry.idle_since_ms = None;
        entry.last_activity_ms = now_ms();
        entry.stuck_flagged = false;
        entry.recompute_state();
        let event = make_event(&mut entry.record, EventKind::Prompt { text });
        self.publish_reserved(&event, &entry.record, publication);
        Ok(())
    }

    async fn on_event(&self, id: &SessionId, generation: u32, kind: EventKind) {
        let now = now_ms();
        // Done needs the git state, which is read outside the lock.
        let kind = match kind {
            EventKind::Done {
                result,
                interrupted,
                note,
                git: _,
            } => {
                let cwd = self.live().get(id).map(|l| l.record.cwd.clone());
                let git = match cwd {
                    Some(cwd) => git::state(&cwd).await,
                    None => None,
                };
                EventKind::Done {
                    result,
                    interrupted,
                    note,
                    git,
                }
            }
            other => other,
        };

        let publication = if kind.is_delta() {
            None
        } else {
            match self.inner.store.reserve_wait().await {
                Ok(permit) => Some(permit),
                Err(error) => {
                    tracing::error!(%error, session = %id, "writer unavailable; driver event cannot be retained");
                    return;
                }
            }
        };
        {
            let mut live = self.live();
            let Some(entry) = live.get_mut(id) else {
                return;
            };
            if entry.record.spawn_count != generation {
                return;
            }
            entry.last_activity_ms = now;
            entry.stuck_flagged = false;
            let kind = match kind {
                EventKind::Started {
                    driver_session_id,
                    model,
                    reasoning,
                } => {
                    entry.started = true;
                    if driver_session_id.is_some() {
                        entry.record.driver_session_id = driver_session_id.clone();
                    }
                    if model.is_some() {
                        entry.record.model = model.clone();
                    }
                    // An explicit effort wins; otherwise record what the CLI uses.
                    if entry.record.reasoning.is_none() && reasoning.is_some() {
                        entry.record.reasoning = reasoning.clone();
                    }
                    EventKind::Started {
                        driver_session_id,
                        model,
                        reasoning,
                    }
                }
                EventKind::ToolCall {
                    call_id,
                    name,
                    input_summary,
                    status,
                    result_summary,
                    input,
                    output,
                } => {
                    match status {
                        ToolStatus::Started => {
                            entry.running_tools.insert(call_id.clone(), name.clone());
                        }
                        ToolStatus::Completed | ToolStatus::Failed => {
                            entry.running_tools.remove(&call_id);
                        }
                    }
                    EventKind::ToolCall {
                        call_id,
                        name,
                        input_summary,
                        status,
                        result_summary,
                        input,
                        output,
                    }
                }
                k @ (EventKind::Question { .. } | EventKind::Approval { .. }) => {
                    let question_id = match &k {
                        EventKind::Question { question_id, .. } => question_id.clone(),
                        EventKind::Approval { request_id, .. } => request_id.clone(),
                        _ => unreachable!(),
                    };
                    let pending = Pending {
                        question_id: question_id.clone(),
                        request: k.clone(),
                        asked_ms: now,
                        reminded: false,
                        stuck: false,
                    };
                    // Queued under the lock: ordered before any answer.
                    entry.pending.push(pending);
                    k
                }
                EventKind::Done {
                    result,
                    interrupted,
                    note,
                    git,
                } => {
                    let interrupted = interrupted || entry.interrupt_requested;
                    entry.interrupt_requested = false;
                    entry.turn_active = false;
                    entry.running_tools.clear();
                    entry.idle_since_ms = Some(now);
                    if result.is_some() {
                        entry.record.last_result = result.clone();
                    }
                    EventKind::Done {
                        result,
                        interrupted,
                        note,
                        git,
                    }
                }
                EventKind::Error { message } => {
                    // Drivers report errors when a turn ends with a failure.
                    entry.turn_active = false;
                    entry.running_tools.clear();
                    entry.idle_since_ms = Some(now);
                    entry.record.last_error = Some(message.clone());
                    EventKind::Error { message }
                }
                other => other,
            };
            entry.recompute_state();
            let event = make_event(&mut entry.record, kind);
            if let Some(publication) = publication {
                self.publish_reserved(&event, &entry.record, publication);
            } else {
                let _ = self.inner.tx.send(event);
            }
        }
    }

    async fn clean_scope(&self, id: &SessionId, unit: &str) -> Result<()> {
        let result = self
            .inner
            .launcher
            .cleanup_unit(
                unit,
                Duration::from_secs(self.inner.config.sessions.stop_grace_secs),
            )
            .await;
        let mut failed = self
            .inner
            .cleanup_failed
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match result {
            Ok(()) => {
                failed.remove(id);
                Ok(())
            }
            Err(error) => {
                let message = format!(
                    "cleanup_failed for session {id}: {error}; inspect the user systemd manager and old scope, then use wardex resume <id> to retry cleanup; no new process was started"
                );
                failed.insert(id.clone(), message.clone());
                tracing::warn!(session = %id, %error, "scope cleanup failed; automatic resume is blocked");
                Err(ManagerError::Conflict(message))
            }
        }
    }

    /// Update the snapshot even on failure so callers can persist the
    /// quarantine without losing the unit needed for an explicit retry.
    async fn clean_record_scope(&self, record: &mut SessionRecord) -> Result<()> {
        let Some(unit) = &record.unit else {
            if record.cleanup_failed {
                return Err(ManagerError::Conflict(format!(
                    "cleanup_failed for session {} has no recorded scope; inspect its stored unit before resuming",
                    record.id
                )));
            }
            return Ok(());
        };
        let result = self.clean_scope(&record.id, unit).await;
        record.updated_ms = now_ms();
        match &result {
            Ok(()) => {
                record.unit = None;
                if record.cleanup_failed {
                    record.last_error = None;
                }
                record.cleanup_failed = false;
            }
            Err(error) => {
                record.cleanup_failed = true;
                record.last_error = Some(error.to_string());
            }
        }
        result
    }

    async fn on_exit(
        &self,
        id: &SessionId,
        generation: u32,
        code: Option<i32>,
        stderr_tail: Vec<String>,
    ) {
        let unit = self
            .live()
            .get(id)
            .filter(|l| l.record.spawn_count == generation)
            .and_then(|l| l.record.unit.clone());
        let cleanup_error = match unit {
            Some(unit) => self.clean_scope(id, &unit).await.err(),
            None => None,
        };
        let publication = match self.inner.store.reserve_wait().await {
            Ok(permit) => permit,
            Err(error) => {
                tracing::error!(%error, session = %id, "writer unavailable during process exit");
                return;
            }
        };
        let mut publications = Vec::new();
        let record = {
            let mut live = self.live();
            let Some(entry) = live
                .get_mut(id)
                .filter(|l| l.record.spawn_count == generation)
            else {
                return;
            };
            let mut record = entry.record.clone();
            let shutting_down = self.inner.shutting_down.load(Ordering::SeqCst);
            let (state, reason) = if entry.stop_requested && !shutting_down {
                (SessionState::Stopped, EndReason::Stopped)
            } else if entry.drain_exit || shutting_down {
                record.resume_pending = true;
                record.resume_with_prompt =
                    record.resume_with_prompt || entry.turn_active || !entry.pending.is_empty();
                let reason = if entry.drain_exit {
                    EndReason::Drain
                } else {
                    EndReason::Shutdown
                };
                (SessionState::Orphaned, reason)
            } else if entry.idle_exit || (code == Some(0) && !entry.turn_active) {
                let reason = entry.idle_reason.unwrap_or(EndReason::Done);
                (SessionState::Done, reason)
            } else {
                (SessionState::Error, EndReason::Error)
            };
            if state == SessionState::Error {
                let mut message = match code {
                    Some(c) => format!("agent process exited unexpectedly with code {c}"),
                    None => "agent process exited unexpectedly".to_string(),
                };
                let tail: Vec<&String> = stderr_tail.iter().rev().take(5).rev().collect();
                if !tail.is_empty() {
                    message.push_str(": ");
                    message.push_str(
                        &tail
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(" | "),
                    );
                }
                record.last_error = Some(message.clone());
                let event = make_event(&mut record, EventKind::Error { message });
                publications.push(event);
            }
            record.state = state;
            record.ended_ms = Some(now_ms());
            record.ended_reason = Some(reason);
            record.pid = None;
            if let Some(error) = &cleanup_error {
                record.state = SessionState::Orphaned;
                record.resume_pending = true;
                record.cleanup_failed = true;
                record.last_error = Some(error.to_string());
            } else {
                record.cleanup_failed = false;
                record.unit = None;
            }
            let ended_state = record.state;
            let event = make_event(
                &mut record,
                EventKind::Ended {
                    state: ended_state,
                    exit_code: code,
                    reason: Some(reason),
                },
            );
            self.cache(&record);
            publications.push(event);
            self.inner
                .store
                .exit_reserved(publications.clone(), record.clone(), publication);
            for event in publications {
                let _ = self.inner.tx.send(event);
            }
            live.remove(id);
            record
        };
        let reason = record.ended_reason;
        tracing::info!(session = %id, state = record.state.as_str(), reason = reason.map(EndReason::as_str), ?code, "session process ended and scope reaped");
        // An idle session closed to save resources is unfinished work: keep
        // its worktree so `send` can continue it.
        if cleanup_error.is_none()
            && record.remove_worktree
            && matches!(reason, Some(EndReason::Done | EndReason::Stopped))
            && let Some(origin) = &record.origin_cwd
            && let Some(root) = git::repo_root(origin).await
            && let Err(e) = git::remove_worktree(&root, &record.cwd).await
        {
            tracing::warn!(session = %id, error = %e, "worktree kept");
        }
    }

    pub async fn list(&self, all: bool) -> Result<Vec<SessionView>> {
        let records = match self.inner.store.list_sessions(all, 500).await {
            Ok(records) => {
                *self
                    .inner
                    .read_warning
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = None;
                records
            }
            Err(error) => {
                *self
                    .inner
                    .read_warning
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(format!(
                    "Last database read failed; showing only cached sessions: {error}"
                ));
                tracing::warn!(%error, "listing cached sessions while database is unavailable");
                Vec::new()
            }
        };
        let mut records: HashMap<_, _> = records
            .into_iter()
            .map(|record| (record.id.clone(), record))
            .collect();
        records.extend(
            self.inner
                .records
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        );
        let records = records
            .into_values()
            .filter(|r| all || !r.state.is_terminal());
        let live = self.live();
        let mut out: Vec<SessionView> = records
            .into_iter()
            .map(|r| match live.get(&r.id) {
                Some(l) => SessionView {
                    session: l.record.clone(),
                    live: true,
                    pending_questions: l.pending.len(),
                },
                None => SessionView {
                    session: r,
                    live: false,
                    pending_questions: 0,
                },
            })
            .collect();
        // Sessions started after the list query.
        for (id, l) in live.iter() {
            if !out.iter().any(|v| &v.session.id == id) {
                out.push(SessionView {
                    session: l.record.clone(),
                    live: true,
                    pending_questions: l.pending.len(),
                });
            }
        }
        out.sort_by_key(|v| std::cmp::Reverse(v.session.created_ms));
        Ok(out)
    }

    pub async fn status(&self, id: &SessionId) -> Result<SessionStatus> {
        if let Some(l) = self.live().get(id) {
            return Ok(SessionStatus {
                session: l.record.clone(),
                live: true,
                rss_bytes: l.rss_bytes,
                cpu_percent: l.cpu_percent,
                pending: l
                    .pending
                    .iter()
                    .map(|p| PendingView {
                        question_id: p.question_id.clone(),
                        kind: p.kind().into(),
                        asked_ms: p.asked_ms,
                        request: p.request.clone(),
                    })
                    .collect(),
                running_tools: l.running_tools.values().cloned().collect(),
                last_activity_ms: Some(l.last_activity_ms),
            });
        }
        let session = self.record(id).await?;
        Ok(SessionStatus {
            session,
            live: false,
            rss_bytes: None,
            cpu_percent: None,
            pending: Vec::new(),
            running_tools: Vec::new(),
            last_activity_ms: None,
        })
    }

    /// Live sessions with their latest `events` events each, plus sessions
    /// that ended in the last 24 hours.
    pub async fn overview(&self, events: i64) -> Result<Overview> {
        let now = now_ms();
        let idle_shutdown_ms = secs_to_ms(self.inner.config.sessions.idle_shutdown_secs);
        let snapshot: Vec<OverviewSession> = self
            .live()
            .values()
            .map(|l| OverviewSession {
                id: l.record.id.clone(),
                driver: l.record.driver,
                state: l.record.state,
                agent: l.record.agent.clone(),
                task: l.record.task.clone(),
                model: l.record.model.clone(),
                cwd: l.record.cwd.clone(),
                branch: l.record.branch.clone(),
                readonly: l.record.readonly,
                created_ms: l.record.created_ms,
                duration_ms: now - l.record.created_ms,
                last_activity_ms: l.last_activity_ms,
                idle_since_ms: idle_since(l),
                shutdown_in_ms: idle_since(l)
                    .filter(|_| idle_shutdown_ms > 0)
                    .map(|since| (idle_shutdown_ms - (now - since)).max(0)),
                last_seq: l.record.last_seq,
                rss_bytes: l.rss_bytes,
                cpu_percent: l.cpu_percent,
                running_tools: l.running_tools.values().cloned().collect(),
                pending: l
                    .pending
                    .iter()
                    .map(|p| PendingView {
                        question_id: p.question_id.clone(),
                        kind: p.kind().into(),
                        asked_ms: p.asked_ms,
                        request: p.request.clone(),
                    })
                    .collect(),
                latest: Vec::new(),
            })
            .collect();
        let store = &self.inner.store;
        store.flush().await?;
        let mut live = snapshot;
        if events > 0 {
            for s in &mut live {
                s.latest = store
                    .last_events(&s.id, events)
                    .await?
                    .iter()
                    .map(EventBrief::from)
                    .collect();
            }
        }
        live.sort_by_key(|s| s.created_ms);
        let mut counts = BTreeMap::new();
        for s in &live {
            *counts.entry(s.state.as_str().to_string()).or_insert(0) += 1;
        }
        let pending_questions = live.iter().map(|s| s.pending.len()).sum();
        let recently_ended = store
            .ended_since(now - OVERVIEW_ENDED_WINDOW_MS, OVERVIEW_ENDED_LIMIT)
            .await?
            .into_iter()
            .filter(|r| !live.iter().any(|s| s.id == r.id))
            .map(|r| EndedSession {
                id: r.id,
                driver: r.driver,
                state: r.state,
                agent: r.agent,
                task: r.task,
                ended_ms: r.ended_ms,
                last_seq: r.last_seq,
                last_result: r.last_result.map(|t| truncate(&t, OVERVIEW_TEXT_LIMIT)),
                last_error: r.last_error,
                cleanup_failed: r.cleanup_failed,
                ended_reason: r.ended_reason,
            })
            .collect();
        Ok(Overview {
            now_ms: now,
            counts,
            pending_questions,
            live,
            recently_ended,
        })
    }

    /// Wait up to `timeout` until the session has a persisted event after
    /// `after_seq`. Returns whether one arrived.
    pub async fn wait_for_event(
        &self,
        id: &SessionId,
        after_seq: u64,
        timeout: Duration,
    ) -> Result<bool> {
        let mut rx = self.subscribe();
        if self.record(id).await?.last_seq > after_seq {
            return Ok(true);
        }
        let wait = async {
            loop {
                match rx.recv().await {
                    Ok(e) if &e.session_id == id && !e.kind.is_delta() && e.seq > after_seq => {
                        return true;
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if let Ok(r) = self.record(id).await
                            && r.last_seq > after_seq
                        {
                            return true;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return false,
                }
            }
        };
        Ok(tokio::time::timeout(timeout, wait).await.unwrap_or(false))
    }

    /// Full input and output of the tool call that event `seq` belongs to.
    pub async fn tool_output(&self, id: &SessionId, seq: u64) -> Result<ToolOutput> {
        self.record(id).await?;
        let store = &self.inner.store;
        store.flush_session(id).await?;
        let call_id = match store.event_at(id, seq).await?.map(|e| e.kind) {
            Some(EventKind::ToolCall { call_id, .. }) => call_id,
            Some(_) => {
                return Err(ManagerError::Invalid(format!(
                    "event {seq} is not a tool_call"
                )));
            }
            None => return Err(ManagerError::NotFound(format!("{id} event {seq}"))),
        };
        let mut out = ToolOutput {
            call_id: call_id.clone(),
            ..Default::default()
        };
        for e in store.tool_call_events(id, &call_id).await? {
            if let EventKind::ToolCall {
                name,
                input_summary,
                status,
                result_summary,
                input,
                output,
                ..
            } = e.kind
            {
                out.name = name;
                if status == ToolStatus::Started {
                    out.started_seq = Some(e.seq);
                    out.input_summary = input_summary;
                    out.input = input;
                } else {
                    out.finished_seq = Some(e.seq);
                    out.status = Some(status);
                    out.result_summary = result_summary;
                    out.output = output;
                }
            }
        }
        Ok(out)
    }

    /// Persisted events after `since_seq`. Waits for queued events first.
    pub async fn events(&self, id: &SessionId, since_seq: u64, limit: i64) -> Result<Vec<Event>> {
        self.record(id).await?;
        self.inner.store.flush_session(id).await?;
        Ok(self.inner.store.events(id, since_seq, limit).await?)
    }

    pub async fn last_events(&self, id: &SessionId, n: i64) -> Result<Vec<Event>> {
        self.record(id).await?;
        self.inner.store.flush_session(id).await?;
        Ok(self.inner.store.last_events(id, n).await?)
    }

    pub async fn summary(&self, id: &SessionId) -> Result<SessionSummary> {
        let record = self.record(id).await?;
        let store = &self.inner.store;
        store.flush_session(id).await?;
        let event_counts: BTreeMap<String, i64> =
            store.event_counts(id).await?.into_iter().collect();
        let tool_counts: BTreeMap<String, i64> = store.tool_counts(id).await?.into_iter().collect();
        let last_done = store.last_event_of(id, "done").await?;
        let last_note = match last_done.as_ref().map(|e| &e.kind) {
            Some(EventKind::Done { note, .. }) => note.clone(),
            _ => None,
        };
        let git = git::state(&record.cwd).await;
        let last_end = match (last_done, store.last_event_of(id, "error").await?) {
            (Some(d), Some(e)) => Some(if d.seq > e.seq { d } else { e }),
            (d, e) => d.or(e),
        };
        let last_turn_ended_ms = last_end.as_ref().map(|e| e.ts_ms);
        let last_turn_ms = match &last_end {
            Some(end) => store
                .last_event_before(id, "prompt", end.seq)
                .await?
                .map(|p| end.ts_ms - p.ts_ms),
            None => None,
        };
        let in_turn = matches!(
            record.state,
            SessionState::Starting | SessionState::Running | SessionState::WaitingInput
        );
        let end = match (in_turn, last_turn_ended_ms) {
            (false, Some(t)) => t,
            _ => record.ended_ms.unwrap_or_else(now_ms),
        };
        Ok(SessionSummary {
            id: record.id.clone(),
            driver: record.driver,
            state: record.state,
            agent: record.agent.clone(),
            task: record.task.clone(),
            model: record.model.clone(),
            cwd: record.cwd.clone(),
            created_ms: record.created_ms,
            ended_ms: record.ended_ms,
            duration_ms: end - record.created_ms,
            last_turn_ended_ms,
            last_turn_ms,
            prompts: event_counts.get("prompt").copied().unwrap_or(0),
            turns: event_counts.get("done").copied().unwrap_or(0)
                + event_counts.get("error").copied().unwrap_or(0),
            event_counts,
            tool_counts,
            last_result: record.last_result.clone(),
            last_note,
            last_error: record.last_error.clone(),
            git,
        })
    }

    /// Answer the given (or the oldest) pending question of a session.
    pub async fn answer(
        &self,
        id: &SessionId,
        question_id: Option<&str>,
        input: AnswerInput,
    ) -> Result<String> {
        let publication = self.inner.store.reserve_publication()?;
        let qid = {
            let mut live = self.live();
            let entry = live
                .get_mut(id)
                .ok_or_else(|| ManagerError::Conflict(format!("session {id} is not live")))?;
            if entry.stop_requested || entry.idle_exit || entry.drain_exit || entry.stdin_closed {
                return Err(ManagerError::Conflict(format!(
                    "session {id} is shutting down"
                )));
            }
            let idx = match question_id {
                Some(q) => entry
                    .pending
                    .iter()
                    .position(|p| p.question_id == q)
                    .ok_or_else(|| ManagerError::NotFound(format!("question {q}")))?,
                None => {
                    if entry.pending.is_empty() {
                        return Err(ManagerError::Conflict(format!(
                            "session {id} has no pending question"
                        )));
                    }
                    0
                }
            };
            let answer = build_answer(&entry.pending[idx].request, input)?;
            let pending = &entry.pending[idx];
            entry
                .commands
                .send(DriverCommand::Answer {
                    question_id: pending.question_id.clone(),
                    answer: answer.clone(),
                })
                .map_err(|_| ManagerError::Conflict(format!("session {id} is shutting down")))?;
            let pending = entry.pending.remove(idx);
            entry.turn_active = true;
            entry.last_activity_ms = now_ms();
            entry.stuck_flagged = false;
            entry.recompute_state();
            let event = make_event(
                &mut entry.record,
                EventKind::Answered {
                    question_id: pending.question_id.clone(),
                    answer: answer.clone(),
                },
            );
            self.publish_reserved(&event, &entry.record, publication);
            pending.question_id
        };
        self.finish_action().await;
        Ok(qid)
    }

    /// Send a follow-up message. A session that is not live is resumed with
    /// the message. Returns whether it was resumed.
    pub async fn send(&self, id: &SessionId, text: String) -> Result<bool> {
        self.accepting_work()?;
        self.inner.store.check_admission(Some(id))?;
        if text.trim().is_empty() {
            return Err(ManagerError::Invalid("message is empty".into()));
        }
        let live = self.live().contains_key(id);
        if live {
            self.deliver(id, text, false, None)?;
            self.finish_action().await;
            Ok(false)
        } else {
            if self.record(id).await?.cleanup_failed {
                return Err(ManagerError::Conflict(format!(
                    "cleanup_failed for session {id}; use wardex resume <id> explicitly to retry scope cleanup before sending; no message was sent"
                )));
            }
            self.resume(id, Some(text)).await?;
            Ok(true)
        }
    }

    /// Resume a session that is not live, optionally with a message.
    pub async fn resume(&self, id: &SessionId, message: Option<String>) -> Result<SessionRecord> {
        let _reservation = self.reserve_start(id)?;
        self.inner.store.check_admission(Some(id))?;
        self.inner.store.flush_session(id).await?;
        let mut record = self.record(id).await?;
        if record.unit.is_some() || record.cleanup_failed {
            let cleanup = self.clean_record_scope(&mut record).await;
            if cleanup.is_err() {
                record.state = SessionState::Orphaned;
                record.resume_pending = true;
                record.pid = None;
                record.ended_ms.get_or_insert_with(now_ms);
            }
            self.inner.store.update_session(&record).await?;
            self.cache(&record);
            cleanup?;
        }
        if !record.cwd.is_dir() {
            let hint = if record.worktree.is_some() {
                " (its worktree was removed)"
            } else {
                ""
            };
            return Err(ManagerError::Conflict(format!(
                "session {id} cannot be resumed: its working directory {} no longer exists{hint}",
                record.cwd.display()
            )));
        }
        let resume_id = record.driver_session_id.clone().ok_or_else(|| {
            ManagerError::Conflict(format!(
                "session {id} cannot be resumed: the agent never reported a session id"
            ))
        })?;
        record.last_seq = record.last_seq.max(self.inner.store.max_seq(id).await?);
        record.last_error = None;
        record.resume_pending = false;
        record.resume_with_prompt = false;
        let publication = if message.is_some() {
            Some(self.inner.store.reserve_publication()?)
        } else {
            None
        };
        self.spawn_process(&mut record, Some(resume_id))?;
        if let Some(text) = message {
            self.deliver(id, text, true, publication)?;
        }
        self.finish_action().await;
        self.record(id).await
    }

    /// Stop a live session: close stdin, then kill after the grace period.
    pub async fn stop(&self, id: &SessionId) -> Result<()> {
        let grace = Duration::from_secs(self.inner.config.sessions.stop_grace_secs);
        let generation = {
            let mut live = self.live();
            let entry = live
                .get_mut(id)
                .ok_or_else(|| ManagerError::Conflict(format!("session {id} is not live")))?;
            entry.stop_requested = true;
            let _ = entry.commands.send(DriverCommand::Shutdown);
            entry.record.spawn_count
        };
        let this = self.clone();
        let id = id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            let cmd = this
                .live()
                .get(&id)
                .filter(|l| l.record.spawn_count == generation)
                .map(|l| l.commands.clone());
            if let Some(cmd) = cmd {
                let _ = cmd.send(DriverCommand::Kill);
            }
        });
        Ok(())
    }

    /// Interrupt the running turn of a live session.
    pub async fn interrupt(&self, id: &SessionId) -> Result<()> {
        let mut live = self.live();
        let entry = live
            .get_mut(id)
            .ok_or_else(|| ManagerError::Conflict(format!("session {id} is not live")))?;
        if !entry.turn_active {
            return Err(ManagerError::Conflict(format!(
                "session {id} has no running turn"
            )));
        }
        entry.interrupt_requested = true;
        let _ = entry.commands.send(DriverCommand::Interrupt);
        Ok(())
    }

    /// Resume sessions orphaned by the previous daemon when
    /// `sessions.auto_resume_orphaned` is on; otherwise just clear their
    /// pending flag. Sessions that were mid-turn get `sessions.resume_prompt`.
    /// Sessions that were idle are left for `send` to resume unless
    /// `sessions.auto_resume_idle` is on. Returns the resumed ids.
    pub async fn auto_resume(&self) -> Result<Vec<SessionId>> {
        let pending = self.inner.store.pending_resumes().await?;
        let mut resumed = Vec::new();
        for mut record in pending {
            if record.cleanup_failed {
                continue;
            }
            let sessions = &self.inner.config.sessions;
            // Idle sessions come back on demand (`send`) unless configured.
            let wanted = sessions.auto_resume_orphaned
                && (record.resume_with_prompt || sessions.auto_resume_idle);
            if !wanted || record.driver_session_id.is_none() {
                record.resume_pending = false;
                record.resume_with_prompt = false;
                self.inner.store.update_session(&record).await?;
                self.cache(&record);
                continue;
            }
            let message = record
                .resume_with_prompt
                .then(|| self.inner.config.sessions.resume_prompt.clone());
            match self.resume(&record.id, message).await {
                Ok(_) => resumed.push(record.id.clone()),
                Err(e @ ManagerError::Store(_)) => return Err(e),
                Err(e) => {
                    tracing::error!(session = %record.id, error = %e, "automatic resume failed");
                    record = self.record(&record.id).await?;
                    if record.cleanup_failed {
                        continue;
                    }
                    record.resume_pending = false;
                    self.inner.store.update_session(&record).await?;
                    self.cache(&record);
                }
            }
        }
        Ok(resumed)
    }

    /// Wait until no live session has a running turn (sessions waiting for
    /// an answer do not count). Returns false on timeout.
    pub async fn wait_for_turns(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let busy = self
                .live()
                .values()
                .any(|l| l.turn_active && l.pending.is_empty());
            if !busy {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn pending_questions(&self) -> Result<Vec<QuestionRow>> {
        self.inner.store.flush().await?;
        Ok(self.inner.store.open_questions(None).await?)
    }

    pub fn live_ids(&self) -> Vec<SessionId> {
        self.live().keys().cloned().collect()
    }

    fn drain_blocked(
        &self,
        message: &str,
        mut sessions: Vec<SessionId>,
        closed_sessions: Vec<SessionId>,
    ) -> ManagerError {
        let mut cleanup_failed: Vec<_> = self
            .inner
            .cleanup_failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        cleanup_failed.sort();
        sessions.extend(cleanup_failed.iter().cloned());
        sessions.sort();
        sessions.dedup();
        ManagerError::DrainBlocked {
            message: message.into(),
            sessions,
            closed_sessions,
            cleanup_failed,
        }
    }

    /// Default drain preserves live turns/questions on timeout. Once the
    /// leader exits, its scope must also be empty before drain succeeds.
    pub async fn drain(&self, timeout: Duration, grace: Duration) -> Result<DrainReport> {
        let _reservation = {
            let _starting = self
                .inner
                .starting
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            self.accepting_work()?;
            self.inner.draining.store(true, Ordering::SeqCst);
            DrainReservation(&self.inner)
        };
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let mut busy: Vec<_> = self
                .inner
                .starting
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .cloned()
                .collect();
            busy.extend(
                self.live()
                    .values()
                    .filter(|l| l.turn_active || !l.pending.is_empty() || !l.started)
                    .map(|l| l.record.id.clone()),
            );
            busy.sort();
            busy.dedup();
            if busy.is_empty() {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(self.drain_blocked(
                    "drain timed out waiting for starts, turns or answers",
                    busy,
                    Vec::new(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if !self
            .inner
            .cleanup_failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
        {
            return Err(self.drain_blocked(
                "drain refused: cleanup_failed scopes remain unverified",
                Vec::new(),
                Vec::new(),
            ));
        }
        // Never close a safely resumable idle leader before its id/state is
        // durable. --force is the explicit escape if SQLite is unavailable.
        self.inner.store.flush().await?;
        let candidates = {
            let mut live = self.live();
            let mut blocked: Vec<_> = live
                .values()
                .filter(|l| {
                    l.turn_active
                        || !l.pending.is_empty()
                        || !l.started
                        || l.record.driver_session_id.is_none()
                })
                .map(|l| l.record.id.clone())
                .collect();
            if !blocked.is_empty() {
                blocked.sort();
                return Err(self.drain_blocked(
                    "a session has no resume id or is no longer idle; use wardex stop <id> if it never reports a resume id",
                    blocked, Vec::new(),
                ));
            }
            let ids: Vec<_> = live.keys().cloned().collect();
            for entry in live.values_mut() {
                entry.drain_exit = true;
                entry.idle_exit = true;
                entry.stdin_closed = true;
                let _ = entry.commands.send(DriverCommand::Drain);
            }
            ids
        };
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            let mut remaining = self.live_ids();
            remaining.extend(
                self.inner
                    .cleanup_failed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .keys()
                    .cloned(),
            );
            remaining.sort();
            remaining.dedup();
            if remaining.is_empty() {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                let mut closed_sessions: Vec<_> = candidates
                    .iter()
                    .filter(|id| !remaining.contains(id))
                    .cloned()
                    .collect();
                closed_sessions.sort();
                return Err(self.drain_blocked(
                    "drain refused: a leader with closed stdin has not exited or cleanup_failed scopes remain unverified",
                    remaining, closed_sessions,
                ));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        self.finish_action().await;
        self.inner.shutting_down.store(true, Ordering::SeqCst);
        let persistence = self.persistence();
        let mut closed_sessions = candidates;
        closed_sessions.sort();
        Ok(DrainReport {
            closed_sessions,
            interrupted_sessions: Vec::new(),
            persisted: persistence.persisted,
            warnings: persistence.warnings,
        })
    }

    /// Shutdown is a control operation: database failure can never leave a
    /// daemon half-shut-down and still listening. Report uncommitted state.
    pub async fn shutdown(&self, timeout: Duration) -> Result<DrainReport> {
        self.shutdown_with_note(timeout, "Daemon shutdown interrupted this turn")
            .await
    }

    pub async fn force_shutdown(&self, timeout: Duration) -> Result<DrainReport> {
        self.shutdown_with_note(
            timeout,
            "Daemon restart --drain --force interrupted this turn",
        )
        .await
    }

    async fn shutdown_with_note(&self, timeout: Duration, note: &str) -> Result<DrainReport> {
        {
            let _starting = self
                .inner
                .starting
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            self.inner.shutting_down.store(true, Ordering::SeqCst);
        }
        let (candidates, mut interrupted_sessions) = {
            let mut live = self.live();
            let mut interrupted = Vec::new();
            for entry in live.values_mut() {
                if entry.turn_active || !entry.pending.is_empty() || !entry.started {
                    interrupted.push(entry.record.id.clone());
                    entry.record.resume_with_prompt = true;
                    entry.record.last_error = Some(note.into());
                    self.cache(&entry.record);
                    if let Ok(publication) = self.inner.store.reserve_publication() {
                        let event = make_event(
                            &mut entry.record,
                            EventKind::Done {
                                result: None,
                                interrupted: true,
                                note: Some(note.into()),
                                git: None,
                            },
                        );
                        self.publish_reserved(&event, &entry.record, publication);
                    }
                }
                let _ = entry.commands.send(DriverCommand::Shutdown);
            }
            (live.keys().cloned().collect::<Vec<_>>(), interrupted)
        };
        let deadline = tokio::time::Instant::now() + timeout;
        while !self.live().is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let commands: Vec<_> = self.live().values().map(|l| l.commands.clone()).collect();
        for command in commands {
            let _ = command.send(DriverCommand::Kill);
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !self.live().is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let flush =
            tokio::time::timeout(Duration::from_millis(250), self.inner.store.flush()).await;
        let mut persistence = self.persistence();
        if !matches!(flush, Ok(Ok(()))) {
            persistence.persisted = false;
            if persistence.warnings.is_empty() {
                persistence.warnings.push("Shutdown applied but state is not committed; inspect database access before restarting; do not resend prior commands".into());
            }
        }
        let mut remaining = self.live_ids();
        {
            let failures = self
                .inner
                .cleanup_failed
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            remaining.extend(failures.keys().cloned());
            persistence.warnings.extend(failures.values().cloned());
        }
        remaining.sort();
        remaining.dedup();
        if !remaining.is_empty() {
            persistence.warnings.push(format!(
                "Shutdown requested; still reaping sessions: {}",
                join_ids(&remaining)
            ));
        }
        let mut closed_sessions: Vec<_> = candidates
            .into_iter()
            .filter(|id| !remaining.contains(id))
            .collect();
        closed_sessions.sort();
        interrupted_sessions.sort();
        Ok(DrainReport {
            closed_sessions,
            interrupted_sessions,
            persisted: persistence.persisted,
            warnings: persistence.warnings,
        })
    }

    /// Run the supervision loop forever: stuck detection, reminders, idle
    /// shutdown, resource sampling and retention.
    pub async fn supervise(&self) {
        let tick = Duration::from_millis(self.inner.config.daemon.tick_ms);
        let mut last_prune = 0i64;
        loop {
            tokio::time::sleep(tick).await;
            self.tick(now_ms());
            let now = now_ms();
            if now - last_prune > 3_600_000 {
                last_prune = now;
                let r = &self.inner.config.retention;
                match self
                    .inner
                    .store
                    .prune(r.events_days, r.max_db_mb, now)
                    .await
                {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(deleted = n, "pruned old events"),
                    Err(e) => tracing::error!(error = %e, "retention failed"),
                }
            }
        }
    }

    /// One supervision step.
    pub fn tick(&self, now: i64) {
        let mem_available_mb = (self.inner.config.sessions.min_mem_available_mb > 0)
            .then(process::mem_available_mb)
            .flatten();
        self.tick_with_memory(now, mem_available_mb);
    }

    /// One supervision step with the system's available memory (MiB) given
    /// by the caller instead of read from `/proc/meminfo`.
    pub fn tick_with_memory(&self, now: i64, mem_available_mb: Option<u64>) {
        let t = &self.inner.config.timeouts;
        let s = &self.inner.config.sessions;
        let pids: Vec<(SessionId, u32)> = self
            .live()
            .iter()
            .filter_map(|(id, l)| l.record.pid.map(|p| (id.clone(), p)))
            .collect();
        let samples: Vec<(SessionId, Option<process::Usage>)> = pids
            .into_iter()
            .map(|(id, pid)| (id, process::sample(pid)))
            .collect();

        let mut to_emit: Vec<(SessionId, EventKind)> = Vec::new();
        {
            let mut live = self.live();
            for (id, usage) in samples {
                let Some(l) = live.get_mut(&id) else { continue };
                if let Some(u) = usage {
                    l.rss_bytes = Some(u.rss_bytes);
                    if let Some((prev_cpu, prev_ms)) = l.cpu_sample {
                        let wall_us = ((now - prev_ms).max(1) as f64) * 1000.0;
                        let pct = (u.cpu_usec.saturating_sub(prev_cpu) as f64) / wall_us * 100.0;
                        l.cpu_percent = Some((pct * 10.0).round() / 10.0);
                        if t.busy_cpu_percent > 0.0 && pct >= t.busy_cpu_percent && l.turn_active {
                            l.last_activity_ms = now;
                            l.stuck_flagged = false;
                        }
                    }
                    l.cpu_sample = Some((u.cpu_usec, now));
                }
            }
            for (id, l) in live.iter_mut() {
                for p in l.pending.iter_mut() {
                    let waited = (now - p.asked_ms).max(0) as u64 / 1000;
                    if !p.reminded && waited >= t.question_reminder_secs {
                        p.reminded = true;
                        to_emit.push((
                            id.clone(),
                            EventKind::Reminder {
                                question_id: p.question_id.clone(),
                                waited_secs: waited,
                            },
                        ));
                    }
                    if !p.stuck && waited >= t.question_stuck_secs {
                        p.stuck = true;
                        to_emit.push((
                            id.clone(),
                            EventKind::Stuck {
                                reason: StuckReason::WaitingInput,
                                idle_secs: waited,
                            },
                        ));
                    }
                }
                let silent = (now - l.last_activity_ms).max(0) as u64 / 1000;
                if t.idle_stuck_secs > 0
                    && l.turn_active
                    && l.pending.is_empty()
                    && l.running_tools.is_empty()
                    && !l.stuck_flagged
                    && silent >= t.idle_stuck_secs
                {
                    l.stuck_flagged = true;
                    to_emit.push((
                        id.clone(),
                        EventKind::Stuck {
                            reason: StuckReason::NoOutput,
                            idle_secs: silent,
                        },
                    ));
                }
            }
            let idle: Vec<IdleCandidate> = live
                .values()
                .filter(|l| !l.idle_exit && !l.stop_requested && !l.drain_exit && !l.stdin_closed)
                .filter_map(|l| {
                    idle_since(l).map(|since| IdleCandidate {
                        id: l.record.id.clone(),
                        idle_since_ms: since,
                    })
                })
                .collect();
            let memory_close_pending = live
                .values()
                .any(|l| l.idle_exit && l.idle_reason == Some(EndReason::MemoryPressure));
            for (id, reason) in idle_closures(idle, now, s, mem_available_mb, memory_close_pending)
            {
                let Some(l) = live.get_mut(&id) else { continue };
                if reason != EndReason::IdleShutdown {
                    tracing::info!(session = %id, reason = reason.as_str(), ?mem_available_mb, "closing idle session");
                }
                l.idle_exit = true;
                l.idle_reason = Some(reason);
                let _ = l.commands.send(DriverCommand::Shutdown);
            }
        }
        for (id, kind) in to_emit {
            if self.emit_live(&id, kind.clone()).is_none()
                && let Some(entry) = self.live().get_mut(&id)
            {
                match kind {
                    EventKind::Reminder { question_id, .. } => {
                        if let Some(pending) = entry
                            .pending
                            .iter_mut()
                            .find(|p| p.question_id == question_id)
                        {
                            pending.reminded = false;
                        }
                    }
                    EventKind::Stuck {
                        reason: StuckReason::WaitingInput,
                        ..
                    } => {
                        for pending in &mut entry.pending {
                            pending.stuck = false;
                        }
                    }
                    EventKind::Stuck { .. } => entry.stuck_flagged = false,
                    _ => {}
                }
            }
        }
    }
}

fn build_answer(request: &EventKind, input: AnswerInput) -> Result<Answer> {
    match request {
        EventKind::Approval { .. } => {
            let text = match input {
                AnswerInput::Text(t) => t,
                AnswerInput::Items(_) => {
                    return Err(ManagerError::Invalid(
                        "an approval takes a single decision (allow or deny)".into(),
                    ));
                }
            };
            let allow = match text.trim().to_lowercase().as_str() {
                "allow" | "yes" | "y" | "accept" | "approve" | "ok" => true,
                "deny" | "no" | "n" | "decline" | "reject" => false,
                other => {
                    return Err(ManagerError::Invalid(format!(
                        "unknown decision {other:?}; use allow or deny"
                    )));
                }
            };
            Ok(Answer::Approval {
                allow,
                message: None,
            })
        }
        EventKind::Question { items, .. } => {
            let mut answers = BTreeMap::new();
            match input {
                AnswerInput::Text(text) => {
                    if items.len() != 1 {
                        return Err(ManagerError::Invalid(format!(
                            "this request has {} questions; answer each item by id",
                            items.len()
                        )));
                    }
                    answers.insert(items[0].id.clone(), vec![text]);
                }
                AnswerInput::Items(map) => {
                    for item in items {
                        let text = map.get(&item.id).ok_or_else(|| {
                            ManagerError::Invalid(format!("missing answer for {:?}", item.id))
                        })?;
                        answers.insert(item.id.clone(), vec![text.clone()]);
                    }
                }
            }
            Ok(Answer::Question { items: answers })
        }
        _ => Err(ManagerError::Invalid("not a question".into())),
    }
}

/// Helper for tests and tools: absolute form of a path.
pub fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failed_drain_resets_both_exit_flags_without_clearing_idle_shutdown() {
        struct PassiveDriver;
        impl Driver for PassiveDriver {
            fn kind(&self) -> DriverKind {
                DriverKind::Claude
            }
            fn spawn(
                &self,
                _: SpawnSpec,
                _: &Launcher,
            ) -> std::result::Result<crate::driver::DriverHandle, DriverError> {
                let (commands, mut rx) = mpsc::unbounded_channel();
                let (tx, events) = mpsc::unbounded_channel();
                tokio::spawn(async move {
                    while rx.recv().await.is_some() {}
                    drop(tx);
                });
                Ok(crate::driver::DriverHandle {
                    pid: None,
                    unit: None,
                    commands,
                    events,
                })
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("w.db")).await.unwrap();
        let manager = Manager::new(
            Config::default(),
            store,
            vec![Arc::new(PassiveDriver)],
            Launcher::direct(),
        );
        let spec = RunSpec {
            driver: Some(DriverKind::Claude),
            prompt: "hello".into(),
            cwd: dir.path().into(),
            ..Default::default()
        };
        let drained = manager.run(spec.clone()).await.unwrap().session.id;
        let idle = manager.run(spec).await.unwrap().session.id;
        {
            let mut live = manager.live();
            let entry = live.get_mut(&drained).unwrap();
            entry.drain_exit = true;
            entry.idle_exit = true;
            entry.stdin_closed = true;
            live.get_mut(&idle).unwrap().idle_exit = true;
        }
        manager.inner.draining.store(true, Ordering::SeqCst);
        drop(DrainReservation(&manager.inner));
        let live = manager.live();
        assert!(!live[&drained].drain_exit);
        assert!(
            !live[&drained].idle_exit,
            "refused drain left idle_exit set"
        );
        assert!(
            live[&drained].stdin_closed,
            "closed stdin became writable after a refused drain"
        );
        assert!(
            live[&idle].idle_exit,
            "drain cleared an independent idle shutdown"
        );
        assert!(!manager.inner.draining.load(Ordering::SeqCst));
    }

    fn candidates(idle_since: &[(&str, i64)]) -> Vec<IdleCandidate> {
        idle_since
            .iter()
            .map(|(id, since)| IdleCandidate {
                id: SessionId((*id).into()),
                idle_since_ms: *since,
            })
            .collect()
    }

    fn closures(
        idle_since: &[(&str, i64)],
        s: &SessionsConfig,
        mem: Option<u64>,
        pending: bool,
    ) -> Vec<(String, EndReason)> {
        idle_closures(candidates(idle_since), 100_000, s, mem, pending)
            .into_iter()
            .map(|(id, reason)| (id.0, reason))
            .collect()
    }

    fn limits(shutdown: u64, max: u32, min_mem: u64) -> SessionsConfig {
        SessionsConfig {
            idle_shutdown_secs: shutdown,
            max_idle_live: max,
            min_mem_available_mb: min_mem,
            ..SessionsConfig::default()
        }
    }

    #[test]
    fn idle_shutdown_closes_sessions_idle_long_enough() {
        let s = limits(10, 0, 0);
        assert_eq!(
            closures(
                &[("new", 95_000), ("old", 90_000), ("older", 1)],
                &s,
                None,
                false
            ),
            [
                ("older".into(), EndReason::IdleShutdown),
                ("old".into(), EndReason::IdleShutdown)
            ]
        );
        assert!(closures(&[("old", 1)], &limits(0, 0, 0), None, false).is_empty());
    }

    #[test]
    fn idle_limit_closes_the_longest_idle_beyond_it() {
        let idle = [("c", 30), ("a", 10), ("d", 40), ("b", 20)];
        assert_eq!(
            closures(&idle, &limits(0, 2, 0), None, false),
            [
                ("a".into(), EndReason::IdleEvicted),
                ("b".into(), EndReason::IdleEvicted)
            ]
        );
        assert!(closures(&idle, &limits(0, 4, 0), None, false).is_empty());
        assert!(closures(&idle, &limits(0, 0, 0), None, false).is_empty());
        // Sessions closed for idleness do not count against the limit.
        assert_eq!(
            closures(
                &[("a", 1), ("b", 99_000), ("c", 99_500)],
                &limits(10, 2, 0),
                None,
                false
            ),
            [("a".into(), EndReason::IdleShutdown)]
        );
    }

    #[test]
    fn low_memory_closes_one_idle_session_at_a_time() {
        let s = limits(0, 0, 1024);
        let idle = [("b", 20), ("a", 10)];
        assert_eq!(
            closures(&idle, &s, Some(512), false),
            [("a".into(), EndReason::MemoryPressure)]
        );
        // Not while the previous one is still exiting, not with enough or
        // unknown memory, not when disabled, and never without idle sessions.
        assert!(closures(&idle, &s, Some(512), true).is_empty());
        assert!(closures(&idle, &s, Some(1024), false).is_empty());
        assert!(closures(&idle, &s, None, false).is_empty());
        assert!(closures(&idle, &limits(0, 0, 0), Some(1), false).is_empty());
        assert!(closures(&[], &s, Some(1), false).is_empty());
        // Other closes this tick free memory first.
        assert_eq!(
            closures(
                &[("a", 10), ("b", 20), ("c", 30)],
                &limits(0, 2, 1024),
                Some(1),
                false
            ),
            [("a".into(), EndReason::IdleEvicted)]
        );
    }

    #[test]
    fn mixed_drain_blockers_get_distinct_cleanup_and_live_instructions() {
        let cleanup = SessionId("cleanup".into());
        let live = SessionId("live".into());
        let steps = drain_next_steps(&[cleanup.clone(), live], &[cleanup]);
        assert!(steps[0].contains("resume <id>"));
        assert!(steps[0].contains("cleanup_failed sessions: cleanup"));
        assert!(steps[0].contains("stop <id> only for the other blocked sessions"));
        assert!(steps[2].contains("--force"));
    }

    #[tokio::test]
    async fn drain_waits_for_existing_start_reservations_until_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("w.db")).await.unwrap();
        let manager = Manager::new(Config::default(), store, Vec::new(), Launcher::direct());
        let reservation = manager
            .reserve_start(&SessionId("starting".into()))
            .unwrap();
        let other = manager.clone();
        let drain = tokio::spawn(async move {
            other
                .drain(Duration::from_secs(1), Duration::from_millis(50))
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !drain.is_finished(),
            "drain rejected a start before its deadline"
        );
        drop(reservation);
        assert!(drain.await.unwrap().unwrap().closed_sessions.is_empty());
    }
}
