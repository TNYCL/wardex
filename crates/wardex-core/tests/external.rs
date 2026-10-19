//! External sessions through the manager: discovery, history and their
//! gates, against an in-memory store and a fake `/proc`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use wardex_core::external::{
    ExternalFilter, ExternalSession, ExternalSource, History, HistoryKind, HistoryQuery, Liveness,
    Result as ExternalResult,
};
use wardex_core::manager::{ExternalSetup, Manager, ManagerError};
use wardex_core::process::Launcher;
use wardex_core::store::Store;
use wardex_core::time::now_ms;
use wardex_core::{Config, DriverKind, SessionId, SessionRecord, SessionState};

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
}

impl Harness {
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
    let manager = Manager::with_external(
        config,
        store,
        Vec::new(),
        Launcher::direct(),
        ExternalSetup {
            sources: vec![claude, codex],
            proc_root: dir.path().join("proc"),
        },
    );
    Harness { manager, dir }
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
