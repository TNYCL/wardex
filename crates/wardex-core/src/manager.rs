//! The session manager: starts sessions through drivers, numbers and
//! persists their events, tracks live state, answers questions, detects
//! stuck sessions and applies retention.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};

use crate::driver::{Driver, DriverCommand, DriverError, DriverEvent, READONLY_PREFIX, SpawnSpec};
use crate::process::{self, Launcher};
use crate::store::{QuestionRow, Store, StoreError};
use crate::time::now_ms;
use crate::{
    Answer, Config, DriverKind, Event, EventKind, GitState, RunSpec, SessionId, SessionRecord,
    SessionState, StuckReason, ToolStatus, git,
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
    pub ended_ms: Option<i64>,
    pub duration_ms: i64,
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

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
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
    tx: broadcast::Sender<Event>,
    shutting_down: AtomicBool,
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
                tx,
                shutting_down: AtomicBool::new(false),
            }),
        }
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

    /// Persist and broadcast an event for a live session.
    fn emit_live(&self, id: &SessionId, kind: EventKind) -> Option<Event> {
        let mut live = self.live();
        let entry = live.get_mut(id)?;
        let event = make_event(&mut entry.record, kind);
        self.publish(&event, Some(&entry.record));
        Some(event)
    }

    /// Persist and broadcast an event for a session that is not live.
    fn emit_record(&self, record: &mut SessionRecord, kind: EventKind) -> Event {
        let event = make_event(record, kind);
        self.publish(&event, Some(record));
        event
    }

    fn publish(&self, event: &Event, record: Option<&SessionRecord>) {
        if !event.kind.is_delta() {
            self.inner.store.append_event(event.clone());
            if let Some(r) = record {
                self.inner.store.queue_session_update(r.clone());
            }
        }
        let _ = self.inner.tx.send(event.clone());
    }

    /// Resolve a full id or unique prefix.
    pub async fn resolve(&self, id: &str) -> Result<SessionId> {
        let id = id.trim();
        if id.is_empty() {
            return Err(ManagerError::Invalid("empty session id".into()));
        }
        let matches = self.inner.store.find_sessions(id, 2).await?;
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
            if let Some(unit) = &record.unit {
                Launcher::kill_unit(unit).await;
            }
            record.last_seq = record
                .last_seq
                .max(self.inner.store.max_seq(&record.id).await?);
            record.resume_pending = true;
            record.resume_with_prompt = record.state != SessionState::Idle;
            record.state = SessionState::Orphaned;
            record.ended_ms = Some(now_ms());
            record.pid = None;
            record.last_error =
                Some("orphaned: the daemon stopped while the session was live".into());
            self.emit_record(
                &mut record,
                EventKind::Error {
                    message: "orphaned: the daemon stopped while the session was live".into(),
                },
            );
            self.emit_record(
                &mut record,
                EventKind::Ended {
                    state: SessionState::Orphaned,
                    exit_code: None,
                },
            );
            self.inner.store.queue_cancel(&record.id);
            ids.push(record.id.clone());
        }
        self.inner.store.flush().await;
        Ok(ids)
    }

    pub async fn run(&self, spec: RunSpec) -> Result<RunOutcome> {
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
        };
        self.inner.store.insert_session(&record).await?;
        if let Err(e) = self.spawn_process(&mut record, None) {
            record.state = SessionState::Error;
            record.ended_ms = Some(now_ms());
            record.last_error = Some(e.to_string());
            self.emit_record(
                &mut record,
                EventKind::Error {
                    message: e.to_string(),
                },
            );
            self.inner.store.flush().await;
            return Err(e);
        }
        self.deliver(&id, spec.prompt)?;
        let session = self.record(&id).await?;
        Ok(RunOutcome { session, warnings })
    }

    /// Spawn a process for `record` and register it as live.
    fn spawn_process(&self, record: &mut SessionRecord, resume: Option<String>) -> Result<()> {
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
        record.updated_ms = now_ms();
        self.inner.store.queue_session_update(record.clone());

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
            interrupt_requested: false,
            cpu_sample: None,
            cpu_percent: None,
            rss_bytes: None,
        };
        self.live().insert(record.id.clone(), live);
        tracing::info!(session = %record.id, driver = record.driver.as_str(), pid = ?record.pid, "session process started");

        let this = self.clone();
        let id = record.id.clone();
        let mut events = handle.events;
        tokio::spawn(async move {
            while let Some(ev) = events.recv().await {
                match ev {
                    DriverEvent::Event(kind) => this.on_event(&id, kind).await,
                    DriverEvent::Exited { code, stderr_tail } => {
                        this.on_exit(&id, code, stderr_tail).await;
                        return;
                    }
                }
            }
            this.on_exit(&id, None, Vec::new()).await;
        });
        Ok(())
    }

    /// Send a message to a live session.
    fn deliver(&self, id: &SessionId, text: String) -> Result<()> {
        let mut live = self.live();
        let entry = live
            .get_mut(id)
            .ok_or_else(|| ManagerError::Conflict(format!("session {id} is not live")))?;
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
        self.publish(&event, Some(&entry.record));
        Ok(())
    }

    async fn on_event(&self, id: &SessionId, kind: EventKind) {
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

        {
            let mut live = self.live();
            let Some(entry) = live.get_mut(id) else {
                return;
            };
            entry.last_activity_ms = now;
            entry.stuck_flagged = false;
            let kind = match kind {
                EventKind::Started {
                    driver_session_id,
                    model,
                } => {
                    entry.started = true;
                    if driver_session_id.is_some() {
                        entry.record.driver_session_id = driver_session_id.clone();
                    }
                    if model.is_some() {
                        entry.record.model = model.clone();
                    }
                    EventKind::Started {
                        driver_session_id,
                        model,
                    }
                }
                EventKind::ToolCall {
                    call_id,
                    name,
                    input_summary,
                    status,
                    result_summary,
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
                    self.inner.store.queue_question(QuestionRow {
                        session_id: id.clone(),
                        question_id,
                        kind: pending.kind().into(),
                        body: serde_json::to_value(&k).unwrap_or_default(),
                        asked_ms: now,
                        answered_ms: None,
                        answer: None,
                        cancelled: false,
                    });
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
            self.publish(&event, Some(&entry.record));
        }
    }

    async fn on_exit(&self, id: &SessionId, code: Option<i32>, stderr_tail: Vec<String>) {
        let Some(entry) = self.live().remove(id) else {
            return;
        };
        let mut record = entry.record;
        let state = if self.inner.shutting_down.load(Ordering::SeqCst) {
            record.resume_pending = true;
            record.resume_with_prompt = entry.turn_active || !entry.pending.is_empty();
            SessionState::Orphaned
        } else if entry.stop_requested {
            SessionState::Stopped
        } else if entry.idle_exit || (code == Some(0) && !entry.turn_active) {
            SessionState::Done
        } else {
            SessionState::Error
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
            self.emit_record(&mut record, EventKind::Error { message });
        }
        record.state = state;
        record.ended_ms = Some(now_ms());
        record.pid = None;
        self.emit_record(
            &mut record,
            EventKind::Ended {
                state,
                exit_code: code,
            },
        );
        self.inner.store.queue_cancel(id);
        tracing::info!(session = %id, state = state.as_str(), ?code, "session process ended");
        if let Some(unit) = &record.unit {
            // Clean up anything the agent left running in its scope.
            Launcher::kill_unit(unit).await;
        }
        if record.remove_worktree
            && matches!(state, SessionState::Done | SessionState::Stopped)
            && let Some(origin) = &record.origin_cwd
            && let Some(root) = git::repo_root(origin).await
            && let Err(e) = git::remove_worktree(&root, &record.cwd).await
        {
            tracing::warn!(session = %id, error = %e, "worktree kept");
        }
    }

    pub async fn list(&self, all: bool) -> Result<Vec<SessionView>> {
        let records = self.inner.store.list_sessions(all, 500).await?;
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
        store.flush().await;
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

    /// Persisted events after `since_seq`. Waits for queued events first.
    pub async fn events(&self, id: &SessionId, since_seq: u64, limit: i64) -> Result<Vec<Event>> {
        self.record(id).await?;
        self.inner.store.flush().await;
        Ok(self.inner.store.events(id, since_seq, limit).await?)
    }

    pub async fn last_events(&self, id: &SessionId, n: i64) -> Result<Vec<Event>> {
        self.record(id).await?;
        self.inner.store.flush().await;
        Ok(self.inner.store.last_events(id, n).await?)
    }

    pub async fn summary(&self, id: &SessionId) -> Result<SessionSummary> {
        let record = self.record(id).await?;
        let store = &self.inner.store;
        store.flush().await;
        let event_counts: BTreeMap<String, i64> =
            store.event_counts(id).await?.into_iter().collect();
        let tool_counts: BTreeMap<String, i64> = store.tool_counts(id).await?.into_iter().collect();
        let last_done = store.last_event_of(id, "done").await?;
        let last_note = match last_done.as_ref().map(|e| &e.kind) {
            Some(EventKind::Done { note, .. }) => note.clone(),
            _ => None,
        };
        let git = git::state(&record.cwd).await;
        let end = record.ended_ms.unwrap_or_else(now_ms);
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
        let qid = {
            let mut live = self.live();
            let entry = live
                .get_mut(id)
                .ok_or_else(|| ManagerError::Conflict(format!("session {id} is not live")))?;
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
            let pending = entry.pending.remove(idx);
            entry
                .commands
                .send(DriverCommand::Answer {
                    question_id: pending.question_id.clone(),
                    answer: answer.clone(),
                })
                .map_err(|_| ManagerError::Conflict(format!("session {id} is shutting down")))?;
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
            self.publish(&event, Some(&entry.record));
            self.inner.store.queue_answer(
                id,
                &pending.question_id,
                serde_json::to_value(&answer).unwrap_or_default(),
                now_ms(),
            );
            pending.question_id
        };
        Ok(qid)
    }

    /// Send a follow-up message. A session that is not live is resumed with
    /// the message. Returns whether it was resumed.
    pub async fn send(&self, id: &SessionId, text: String) -> Result<bool> {
        if text.trim().is_empty() {
            return Err(ManagerError::Invalid("message is empty".into()));
        }
        let live = self.live().contains_key(id);
        if live {
            self.deliver(id, text)?;
            Ok(false)
        } else {
            self.resume(id, Some(text)).await?;
            Ok(true)
        }
    }

    /// Resume a session that is not live, optionally with a message.
    pub async fn resume(&self, id: &SessionId, message: Option<String>) -> Result<SessionRecord> {
        if self.live().contains_key(id) {
            return Err(ManagerError::Conflict(format!(
                "session {id} is already live"
            )));
        }
        let mut record = self.record(id).await?;
        let resume_id = record.driver_session_id.clone().ok_or_else(|| {
            ManagerError::Conflict(format!(
                "session {id} cannot be resumed: the agent never reported a session id"
            ))
        })?;
        record.last_seq = record.last_seq.max(self.inner.store.max_seq(id).await?);
        record.last_error = None;
        record.resume_pending = false;
        record.resume_with_prompt = false;
        self.spawn_process(&mut record, Some(resume_id))?;
        if let Some(text) = message {
            self.deliver(id, text)?;
        }
        self.record(id).await
    }

    /// Stop a live session: close stdin, then kill after the grace period.
    pub async fn stop(&self, id: &SessionId) -> Result<()> {
        let grace = Duration::from_secs(self.inner.config.sessions.stop_grace_secs);
        {
            let mut live = self.live();
            let entry = live
                .get_mut(id)
                .ok_or_else(|| ManagerError::Conflict(format!("session {id} is not live")))?;
            entry.stop_requested = true;
            let _ = entry.commands.send(DriverCommand::Shutdown);
        }
        let this = self.clone();
        let id = id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            let cmd = this.live().get(&id).map(|l| l.commands.clone());
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
    /// Returns the resumed ids.
    pub async fn auto_resume(&self) -> Result<Vec<SessionId>> {
        let pending = self.inner.store.pending_resumes().await?;
        let mut resumed = Vec::new();
        for mut record in pending {
            let enabled = self.inner.config.sessions.auto_resume_orphaned;
            if !enabled || record.driver_session_id.is_none() {
                record.resume_pending = false;
                record.resume_with_prompt = false;
                self.inner.store.update_session(&record).await?;
                continue;
            }
            let message = record
                .resume_with_prompt
                .then(|| self.inner.config.sessions.resume_prompt.clone());
            match self.resume(&record.id, message).await {
                Ok(_) => resumed.push(record.id.clone()),
                Err(e) => {
                    tracing::error!(session = %record.id, error = %e, "automatic resume failed");
                    record.resume_pending = false;
                    self.inner.store.update_session(&record).await?;
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
        self.inner.store.flush().await;
        Ok(self.inner.store.open_questions(None).await?)
    }

    pub fn live_ids(&self) -> Vec<SessionId> {
        self.live().keys().cloned().collect()
    }

    /// Stop every live session and mark it orphaned so it can be resumed by
    /// the next daemon. Waits up to `timeout` for processes to exit.
    pub async fn shutdown(&self, timeout: Duration) {
        self.inner.shutting_down.store(true, Ordering::SeqCst);
        let commands: Vec<_> = self.live().values().map(|l| l.commands.clone()).collect();
        for cmd in &commands {
            let _ = cmd.send(DriverCommand::Shutdown);
        }
        let deadline = tokio::time::Instant::now() + timeout;
        while !self.live().is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let commands: Vec<_> = self.live().values().map(|l| l.commands.clone()).collect();
        for cmd in &commands {
            let _ = cmd.send(DriverCommand::Kill);
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !self.live().is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.inner.store.flush().await;
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
                if l.turn_active
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
                if s.idle_shutdown_secs > 0
                    && !l.turn_active
                    && !l.idle_exit
                    && !l.stop_requested
                    && l.pending.is_empty()
                    && l.record.state == SessionState::Idle
                    && let Some(since) = l.idle_since_ms
                    && (now - since).max(0) as u64 / 1000 >= s.idle_shutdown_secs
                {
                    l.idle_exit = true;
                    let _ = l.commands.send(DriverCommand::Shutdown);
                }
            }
        }
        for (id, kind) in to_emit {
            self.emit_live(&id, kind);
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
