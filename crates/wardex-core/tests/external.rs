//! External sessions through the manager: discovery, history, adopt and
//! their gates, against an in-memory store, a fake driver and a fake `/proc`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use wardex_core::driver::{
    Driver, DriverCommand, DriverError, DriverEvent, DriverHandle, SpawnSpec,
};
use wardex_core::{EndReason, EventKind, ListFilter};

use wardex_core::external::{
    ExternalFilter, ExternalSession, ExternalSource, History, HistoryKind, HistoryQuery, Liveness,
    Result as ExternalResult,
};
use wardex_core::manager::{ExternalSetup, Manager, ManagerError};
use wardex_core::process::Launcher;
use wardex_core::store::Store;
use wardex_core::time::now_ms;
use wardex_core::{
    AdoptSpec, Config, DriverKind, SessionId, SessionOrigin, SessionRecord, SessionState,
};

const C1: &str = "11111111-1111-4111-8111-111111111111";
const C2: &str = "11111111-2222-4222-8222-222222222222";
const X1: &str = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a01";

/// Sessions held in memory; `root` is a temp dir with Claude's registry.
struct FakeSource {
    kind: DriverKind,
    root: PathBuf,
    sessions: Mutex<Vec<ExternalSession>>,
}

impl ExternalSource for FakeSource {
    fn kind(&self) -> DriverKind {
        self.kind
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn discover(&self, filter: &ExternalFilter) -> ExternalResult<Vec<ExternalSession>> {
        Ok(self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .filter(|s| filter.admits(s))
            .cloned()
            .collect())
    }

    fn find(&self, prefix: &str) -> ExternalResult<Vec<ExternalSession>> {
        Ok(self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.native_id.starts_with(prefix))
            .cloned()
            .collect())
    }

    fn read_history(&self, session: &ExternalSession) -> ExternalResult<History> {
        let mut h = History::default();
        h.push_text(
            None,
            HistoryKind::Prompt,
            "r1".into(),
            format!("hello from {}", session.native_id),
        );
        h.push_text(
            None,
            HistoryKind::Message,
            "r2".into(),
            "TOKEN=abcdefghijkl".into(),
        );
        Ok(h)
    }
}

/// Records its spawn specs; reports the resume id and echoes messages.
struct FakeDriver {
    kind: DriverKind,
    spawns: Arc<Mutex<Vec<SpawnSpec>>>,
}

impl Driver for FakeDriver {
    fn kind(&self) -> DriverKind {
        self.kind
    }

    fn spawn(
        &self,
        spec: SpawnSpec,
        _: &Launcher,
    ) -> std::result::Result<DriverHandle, DriverError> {
        self.spawns.lock().unwrap().push(spec.clone());
        let (commands, mut rx) = mpsc::unbounded_channel();
        let (tx, events) = mpsc::unbounded_channel();
        let id = match (&spec.resume, spec.fork) {
            (Some(source), true) => Some(format!("fork-of-{source}")),
            (resume, _) => resume.clone(),
        };
        tokio::spawn(async move {
            let _ = tx.send(DriverEvent::Event(EventKind::Started {
                driver_session_id: id,
                model: None,
                reasoning: None,
            }));
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    DriverCommand::Send(text) => {
                        let _ = tx.send(DriverEvent::Event(EventKind::Done {
                            result: Some(format!("echo {text}")),
                            interrupted: false,
                            note: None,
                            git: None,
                        }));
                    }
                    DriverCommand::Shutdown | DriverCommand::Drain | DriverCommand::Kill => break,
                    _ => {}
                }
            }
            let _ = tx.send(DriverEvent::Exited {
                code: Some(0),
                stderr_tail: Vec::new(),
                interrupted: false,
            });
        });
        Ok(DriverHandle {
            pid: None,
            unit: None,
            driver_session_id: None,
            commands,
            events,
        })
    }
}

fn session(kind: DriverKind, id: &str, cwd: &str, updated_ms: i64) -> ExternalSession {
    ExternalSession {
        driver: kind,
        native_id: id.into(),
        cwd: cwd.into(),
        title: Some(format!("title {id}")),
        created_ms: None,
        updated_ms,
        cli_version: None,
        entrypoint: None,
        forked_from: None,
        continued_in: None,
        size_bytes: 1,
        compressed: false,
        archived: false,
        subagent: false,
        managed_by: None,
        liveness: Liveness::not_probed(),
        path_hint: format!("projects/x/{id}.jsonl"),
        warnings: Vec::new(),
        path: PathBuf::new(),
    }
}

struct Harness {
    manager: Manager,
    dir: tempfile::TempDir,
    claude: Arc<FakeSource>,
    spawns: Arc<Mutex<Vec<SpawnSpec>>>,
}

impl Harness {
    fn touch(&self, id: &str) {
        for s in self.claude.sessions.lock().unwrap().iter_mut() {
            if s.native_id == id {
                s.updated_ms = now_ms();
            }
        }
    }

    async fn wait_state(&self, id: &SessionId, state: SessionState) {
        for _ in 0..200 {
            if self.manager.status(id).await.unwrap().session.state == state {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{id} never reached {state:?}");
    }

    fn proc_dir(&self) -> PathBuf {
        self.dir.path().join("proc")
    }

    /// A live Claude process registered for `id`.
    fn claude_live(&self, pid: u32, id: &str) {
        let p = self.proc_dir().join(pid.to_string());
        std::fs::create_dir_all(&p).unwrap();
        let fields: Vec<String> = (3..=22)
            .map(|i| if i == 22 { "777".into() } else { "1".into() })
            .collect();
        std::fs::write(
            p.join("stat"),
            format!("{pid} (claude) {}\n", fields.join(" ")),
        )
        .unwrap();
        std::fs::write(p.join("cmdline"), "claude\0").unwrap();
        std::fs::write(
            self.dir.path().join(format!("claude/sessions/{pid}.json")),
            format!(r#"{{"pid":{pid},"sessionId":"{id}","procStart":"777"}}"#),
        )
        .unwrap();
    }
}

async fn harness_with(config: Config) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("proc")).unwrap();
    std::fs::create_dir_all(dir.path().join("claude/sessions")).unwrap();
    for d in ["work/app/.worktree/fix", "work/other"] {
        std::fs::create_dir_all(dir.path().join(d)).unwrap();
    }
    let at = |p: &str| dir.path().join(p).to_string_lossy().into_owned();
    let old = now_ms() - 3_600_000;
    let claude = Arc::new(FakeSource {
        kind: DriverKind::Claude,
        root: dir.path().join("claude"),
        sessions: Mutex::new(vec![
            session(DriverKind::Claude, C1, &at("work/app"), old),
            session(
                DriverKind::Claude,
                C2,
                &at("work/app/.worktree/fix"),
                old - 1000,
            ),
        ]),
    });
    let codex = Arc::new(FakeSource {
        kind: DriverKind::Codex,
        root: dir.path().join("codex"),
        sessions: Mutex::new(vec![{
            let mut s = session(DriverKind::Codex, X1, &at("work/other"), old + 1000);
            s.entrypoint = Some("wardex".into());
            s
        }]),
    });
    let store = Store::open(&dir.path().join("w.db")).await.unwrap();
    let spawns = Arc::new(Mutex::new(Vec::new()));
    let drivers: Vec<Arc<dyn Driver>> = [DriverKind::Claude, DriverKind::Codex]
        .into_iter()
        .map(|kind| {
            Arc::new(FakeDriver {
                kind,
                spawns: spawns.clone(),
            }) as Arc<dyn Driver>
        })
        .collect();
    let manager = Manager::with_external(
        config,
        store,
        drivers,
        Launcher::direct(),
        ExternalSetup {
            sources: vec![claude.clone(), codex],
            proc_root: dir.path().join("proc"),
        },
    );
    Harness {
        manager,
        dir,
        claude,
        spawns,
    }
}

fn enabled() -> Config {
    let mut config = Config::default();
    config.external.enabled = true;
    config
}

async fn harness() -> Harness {
    harness_with(enabled()).await
}

fn ids(sessions: &[ExternalSession]) -> Vec<&str> {
    sessions.iter().map(|s| s.native_id.as_str()).collect()
}

#[tokio::test]
async fn disabled_by_default_and_gated_for_mcp() {
    let h = harness_with(Config::default()).await;
    let err = h
        .manager
        .external_list(ExternalFilter::default(), false)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ManagerError::Invalid(ref m) if m.contains("external.enabled")),
        "{err}"
    );
    let h = harness().await;
    assert!(
        h.manager
            .external_list(ExternalFilter::default(), false)
            .await
            .is_ok()
    );
    let err = h
        .manager
        .history(C1, None, HistoryQuery::default(), true)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("external.mcp"), "{err}");
    let mut config = enabled();
    config.external.mcp = true;
    let h = harness_with(config).await;
    assert!(
        h.manager
            .history(C1, None, HistoryQuery::default(), true)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn lists_newest_first_with_cwd_filters() {
    let h = harness().await;
    let all = h
        .manager
        .external_list(ExternalFilter::default(), false)
        .await
        .unwrap();
    assert_eq!(ids(&all), [X1, C1, C2]);
    assert!(
        all.iter().all(|s| s.liveness == Liveness::Inactive),
        "{all:?}"
    );
    // Codex threads another Wardex daemon started are flagged.
    assert!(all[0].warnings.iter().any(|w| w.contains("Wardex daemon")));

    let app = h.dir.path().join("work/app");
    let exact = ExternalFilter {
        cwd: Some(app.clone()),
        ..Default::default()
    };
    let only = h.manager.external_list(exact.clone(), false).await.unwrap();
    assert_eq!(ids(&only), [C1]);
    let under = ExternalFilter {
        under: true,
        ..exact
    };
    assert_eq!(
        ids(&h.manager.external_list(under, false).await.unwrap()),
        [C1, C2]
    );
    let claude = ExternalFilter {
        driver: Some(DriverKind::Claude),
        limit: Some(1),
        ..Default::default()
    };
    assert_eq!(
        ids(&h.manager.external_list(claude, false).await.unwrap()),
        [C1]
    );
}

#[tokio::test]
async fn cwd_allow_limits_what_is_visible() {
    let mut config = enabled();
    config.external.cwd_allow = vec!["*/work/app".into()];
    let h = harness_with(config).await;
    let all = h
        .manager
        .external_list(ExternalFilter::default(), false)
        .await
        .unwrap();
    assert_eq!(ids(&all), [C1]);
    // Hidden sessions cannot be read either.
    assert!(matches!(
        h.manager
            .history(X1, None, HistoryQuery::default(), false)
            .await,
        Err(ManagerError::NotFound(_))
    ));
}

#[tokio::test]
async fn managed_sessions_are_marked_and_hidden_by_default() {
    let h = harness().await;
    let now = now_ms();
    let record = SessionRecord {
        id: SessionId("abcd0001".into()),
        driver: DriverKind::Claude,
        driver_session_id: Some(C1.into()),
        state: SessionState::Done,
        agent: None,
        task: None,
        model: None,
        reasoning: None,
        cwd: h.dir.path().join("work/app"),
        origin_cwd: None,
        worktree: None,
        branch: None,
        remove_worktree: false,
        readonly: false,
        slice: None,
        unit: None,
        pid: None,
        created_ms: now,
        updated_ms: now,
        ended_ms: Some(now),
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
    };
    h.manager.store().insert_session(&record).await.unwrap();
    let all = h
        .manager
        .external_list(ExternalFilter::default(), false)
        .await
        .unwrap();
    assert_eq!(ids(&all), [X1, C2]);
    let with = ExternalFilter {
        include_managed: true,
        ..Default::default()
    };
    let all = h.manager.external_list(with, false).await.unwrap();
    let c1 = all.iter().find(|s| s.native_id == C1).unwrap();
    assert_eq!(c1.managed_by, Some(SessionId("abcd0001".into())));

    // `history` takes the Wardex id too.
    let page = h
        .manager
        .history("abcd0001", None, HistoryQuery::default(), false)
        .await
        .unwrap();
    assert_eq!(page.session.native_id, C1);
    assert_eq!(
        page.items[0].text.as_deref(),
        Some(&*format!("hello from {C1}"))
    );
}

#[tokio::test]
async fn id_resolution() {
    let h = harness().await;
    let m = &h.manager;
    let short = m.resolve_external("1111", None).await.unwrap_err();
    assert!(short.to_string().contains("at least 8"), "{short}");
    assert!(matches!(
        m.resolve_external("11111111", None).await,
        Err(ManagerError::Ambiguous(_))
    ));
    assert_eq!(
        m.resolve_external("11111111-2", None)
            .await
            .unwrap()
            .native_id,
        C2
    );
    assert_eq!(
        m.resolve_external(&format!("claude:{C1}"), None)
            .await
            .unwrap()
            .native_id,
        C1
    );
    assert!(matches!(
        m.resolve_external(&format!("codex:{C1}"), None).await,
        Err(ManagerError::NotFound(_))
    ));
    assert!(
        m.resolve_external(&format!("claude:{C1}"), Some(DriverKind::Codex))
            .await
            .is_err()
    );
    assert!(matches!(
        m.resolve_external("../../etc/passwd", None).await,
        Err(ManagerError::NotFound(_))
    ));
    assert_eq!(
        m.resolve_external("0199a1b2", Some(DriverKind::Codex))
            .await
            .unwrap()
            .native_id,
        X1
    );
}

#[tokio::test]
async fn history_is_redacted_and_flags_live_sessions() {
    let h = harness().await;
    h.claude_live(4242, C1);
    let page = h
        .manager
        .history(C1, None, HistoryQuery::default(), false)
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.items[1].text.as_deref(), Some("TOKEN=[REDACTED]"));
    assert!(
        page.session.liveness.is_live(),
        "{:?}",
        page.session.liveness
    );
    assert!(page.warnings.iter().any(|w| w == "live"));

    let mut config = enabled();
    config.external.redact = false;
    let raw = harness_with(config).await;
    let page = raw
        .manager
        .history(C1, None, HistoryQuery::default(), false)
        .await
        .unwrap();
    assert_eq!(page.items[1].text.as_deref(), Some("TOKEN=abcdefghijkl"));
}

fn adopt(id: &str) -> AdoptSpec {
    AdoptSpec {
        id: id.into(),
        agent: Some("bot".into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn adopt_creates_a_stopped_record_once() {
    let h = harness().await;
    let out = h.manager.adopt(adopt(C1), false).await.unwrap();
    assert!(out.adopted && !out.resumed);
    let s = &out.session;
    assert_eq!(s.state, SessionState::Stopped);
    assert_eq!(s.ended_reason, Some(EndReason::Adopted));
    assert_eq!(s.origin, SessionOrigin::Adopted);
    assert_eq!(s.driver_session_id.as_deref(), Some(C1));
    assert_eq!(s.cwd, h.dir.path().join("work/app"));
    assert_eq!(s.agent.as_deref(), Some("bot"));
    assert!(s.adopted_ms.is_some());
    assert_eq!(s.spawn_count, 0);
    assert!(
        out.warnings.iter().any(|w| w.contains("full permissions")),
        "{:?}",
        out.warnings
    );
    assert!(h.spawns.lock().unwrap().is_empty(), "no process is started");
    let events = h.manager.events(&s.id, 0, 10).await.unwrap();
    assert_eq!(events.len(), 1);
    match &events[0].kind {
        EventKind::Adopted {
            source_driver_session_id,
            forked,
            liveness,
            path_hint,
            ..
        } => {
            assert_eq!(source_driver_session_id, C1);
            assert!(!forked);
            assert_eq!(liveness, "inactive");
            assert_eq!(path_hint, &format!("projects/x/{C1}.jsonl"));
        }
        e => panic!("{e:?}"),
    }
    assert!(!events[0].kind.is_notable());

    // Idempotent.
    let again = h
        .manager
        .adopt(adopt(&format!("claude:{C1}")), false)
        .await
        .unwrap();
    assert!(!again.adopted);
    assert_eq!(again.session.id, s.id);
    let adopted = ListFilter {
        all: true,
        origin: Some(SessionOrigin::Adopted),
        ..Default::default()
    };
    let listed = h.manager.list_filtered(&adopted).await.unwrap();
    assert_eq!(listed.len(), 1);
    // Now managed: hidden from the external list.
    let ext = h
        .manager
        .external_list(ExternalFilter::default(), false)
        .await
        .unwrap();
    assert!(!ids(&ext).contains(&C1));
}

#[tokio::test]
async fn adopt_with_a_message_resumes_the_same_native_id() {
    let h = harness().await;
    let out = h
        .manager
        .adopt(
            AdoptSpec {
                message: Some("continue".into()),
                readonly: true,
                ..adopt(C2)
            },
            false,
        )
        .await
        .unwrap();
    assert!(out.adopted && out.resumed);
    assert!(!out.warnings.iter().any(|w| w.contains("full permissions")));
    let id = out.session.id.clone();
    h.wait_state(&id, SessionState::Idle).await;
    {
        let spawns = h.spawns.lock().unwrap();
        assert_eq!(spawns.len(), 1);
        assert_eq!(spawns[0].resume.as_deref(), Some(C2));
        assert!(spawns[0].readonly);
        assert_eq!(spawns[0].cwd, h.dir.path().join("work/app/.worktree/fix"));
    }
    let status = h.manager.status(&id).await.unwrap();
    assert_eq!(
        status
            .session
            .last_result
            .as_deref()
            .map(|r| r.contains("continue")),
        Some(true)
    );
    h.manager.stop(&id).await.unwrap();
    h.wait_state(&id, SessionState::Stopped).await;
    let stored = h.manager.store().get_session(&id).await.unwrap().unwrap();
    assert_eq!(stored.origin, SessionOrigin::Adopted, "origin is kept");
    assert_eq!(stored.ended_reason, Some(EndReason::Stopped));
    let kinds: Vec<&str> = h
        .manager
        .events(&id, 0, 100)
        .await
        .unwrap()
        .iter()
        .map(|e| e.kind.name())
        .collect();
    assert_eq!(kinds.first(), Some(&"adopted"));
}

#[tokio::test]
async fn adopt_refuses_live_and_recent_sessions() {
    let mut config = enabled();
    config.external.mcp = true;
    let h = harness_with(config).await;
    h.claude_live(4242, C1);
    let err = h.manager.adopt(adopt(C1), false).await.unwrap_err();
    assert!(
        matches!(err, ManagerError::Conflict(ref m) if m.contains("--fork") && m.contains("4242")),
        "{err}"
    );
    // --force does not override hard evidence.
    let forced = AdoptSpec {
        force: true,
        ..adopt(C1)
    };
    assert!(matches!(
        h.manager.adopt(forced, false).await,
        Err(ManagerError::Conflict(_))
    ));

    h.touch(C2);
    let err = h.manager.adopt(adopt(C2), false).await.unwrap_err();
    assert!(
        matches!(err, ManagerError::Conflict(ref m) if m.contains("--force")),
        "{err}"
    );
    let forced = AdoptSpec {
        force: true,
        ..adopt(C2)
    };
    let err = h.manager.adopt(forced.clone(), true).await.unwrap_err();
    assert!(
        err.to_string().contains("only available in the CLI"),
        "{err}"
    );
    assert!(h.manager.adopt(forced, false).await.unwrap().adopted);
    assert!(h.spawns.lock().unwrap().is_empty());
}

#[tokio::test]
async fn resume_checks_again_whether_the_session_is_open_elsewhere() {
    let h = harness().await;
    let id = h.manager.adopt(adopt(C1), false).await.unwrap().session.id;
    // The user reopened it in a terminal.
    h.claude_live(4343, C1);
    let err = h.manager.send(&id, "hello".into()).await.unwrap_err();
    assert!(
        matches!(err, ManagerError::Conflict(ref m) if m.contains("4343")),
        "{err}"
    );
    assert!(h.manager.resume(&id, None).await.is_err());
    assert!(h.spawns.lock().unwrap().is_empty());
    // Closed again: send resumes it.
    std::fs::remove_file(h.dir.path().join("claude/sessions/4343.json")).unwrap();
    assert!(h.manager.send(&id, "hello".into()).await.unwrap());
    assert_eq!(h.spawns.lock().unwrap()[0].resume.as_deref(), Some(C1));
}

#[tokio::test]
async fn adopt_needs_the_feature() {
    let h = harness_with(Config::default()).await;
    assert!(matches!(
        h.manager.adopt(adopt(C1), false).await,
        Err(ManagerError::Invalid(_))
    ));
}

#[tokio::test]
async fn fork_is_allowed_while_live_and_branches_on_first_run() {
    let h = harness().await;
    h.claude_live(4444, C1);
    let first = h.manager.adopt(adopt(C1), false).await;
    assert!(matches!(first, Err(ManagerError::Conflict(_))));
    let out = h
        .manager
        .adopt(
            AdoptSpec {
                fork: true,
                ..adopt(C1)
            },
            false,
        )
        .await
        .unwrap();
    assert!(out.adopted && !out.resumed);
    let s = out.session.clone();
    assert_eq!(s.origin, SessionOrigin::Forked);
    assert_eq!(s.forked_from.as_deref(), Some(C1));
    assert_eq!(
        s.driver_session_id, None,
        "the fork has no id until it runs"
    );
    assert!(
        out.warnings.iter().any(|w| w.contains("open elsewhere")),
        "{:?}",
        out.warnings
    );
    match &h.manager.events(&s.id, 0, 10).await.unwrap()[0].kind {
        EventKind::Adopted {
            forked, liveness, ..
        } => {
            assert!(forked);
            assert_eq!(liveness, "live");
        }
        e => panic!("{e:?}"),
    }
    // Waiting forks are resumable, and history shows the source meanwhile.
    let resumable = ListFilter {
        resumable: true,
        ..Default::default()
    };
    assert_eq!(h.manager.list_filtered(&resumable).await.unwrap().len(), 1);
    let page = h
        .manager
        .history(s.id.as_str(), None, HistoryQuery::default(), false)
        .await
        .unwrap();
    assert_eq!(page.session.native_id, C1);

    // The first send forks (the live source is only read).
    assert!(h.manager.send(&s.id, "branch off".into()).await.unwrap());
    h.wait_state(&s.id, SessionState::Idle).await;
    {
        let spawns = h.spawns.lock().unwrap();
        assert_eq!(spawns.len(), 1);
        assert!(spawns[0].fork);
        assert_eq!(spawns[0].resume.as_deref(), Some(C1));
    }
    let fork_id = format!("fork-of-{C1}");
    let status = h.manager.status(&s.id).await.unwrap();
    assert_eq!(status.session.driver_session_id.as_deref(), Some(&*fork_id));
    h.manager.stop(&s.id).await.unwrap();
    h.wait_state(&s.id, SessionState::Stopped).await;
    // The next resume continues the fork and does not fork again.
    assert!(h.manager.send(&s.id, "more".into()).await.unwrap());
    h.wait_state(&s.id, SessionState::Idle).await;
    let spawns = h.spawns.lock().unwrap();
    assert_eq!(spawns.len(), 2);
    assert!(!spawns[1].fork);
    assert_eq!(spawns[1].resume.as_deref(), Some(&*fork_id));
}

#[tokio::test]
async fn every_fork_is_a_new_session() {
    let h = harness().await;
    let fork = || AdoptSpec {
        fork: true,
        message: Some("go".into()),
        ..adopt(C2)
    };
    let a = h.manager.adopt(fork(), false).await.unwrap();
    let b = h.manager.adopt(fork(), false).await.unwrap();
    assert!(a.resumed && b.resumed);
    assert_ne!(a.session.id, b.session.id);
}
