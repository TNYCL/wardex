//! Codex rollout reader against synthetic fixtures shaped like Codex 0.163.0
//! rollouts (`tests/fixtures/codex/store`).

use std::path::{Path, PathBuf};

use wardex_core::external::{
    ExternalFilter, ExternalSession, ExternalSource, FORMAT_VERSION_UNTESTED, HistoryKind,
    HistoryQuery, ToolDetail, page,
};
use wardex_core::{DriverKind, ToolStatus};
use wardex_drivers::CodexStore;

const A: &str = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a01";
const B: &str = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a02";
const C: &str = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a03";
const D: &str = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a04";
const E: &str = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a05";
const G: &str = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a07";

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex/store")
}

fn store() -> CodexStore {
    CodexStore::new(root(), 16 * 1024 * 1024)
}

fn find(id: &str) -> ExternalSession {
    let mut found = store().find(id).unwrap();
    assert_eq!(found.len(), 1, "{id}");
    found.remove(0)
}

fn ids(sessions: &[ExternalSession]) -> Vec<&str> {
    let mut ids: Vec<&str> = sessions.iter().map(|s| s.native_id.as_str()).collect();
    ids.sort();
    ids
}

#[test]
fn discovery_skips_archived_and_subagent_threads_by_default() {
    let all = store().discover(&ExternalFilter::default()).unwrap();
    assert_eq!(ids(&all), [A, B, C, G]);
    let more = store()
        .discover(&ExternalFilter {
            archived: true,
            include_subagents: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(ids(&more), [A, B, C, D, E, G]);
    assert!(more.iter().any(|s| s.archived && s.native_id == E));
    assert!(more.iter().any(|s| s.subagent && s.native_id == D));
    // `find` sees everything.
    assert_eq!(store().find("0199a1b2").unwrap().len(), 6);
}

#[test]
fn session_metadata() {
    let s = find(A);
    assert_eq!(s.driver, DriverKind::Codex);
    assert_eq!(s.cwd, Path::new("/work/svc"));
    assert_eq!(s.cli_version.as_deref(), Some("0.163.0"));
    assert_eq!(s.entrypoint.as_deref(), Some("codex-tui"));
    // The newest index entry wins.
    assert_eq!(s.title.as_deref(), Some("Fix flaky login test"));
    assert_eq!(s.created_ms, Some(1_790_848_800_000));
    assert!(s.path_hint.starts_with("sessions/2026/10/01/rollout-"));
    assert!(s.warnings.is_empty(), "{:?}", s.warnings);
    let json = serde_json::to_string(&s).unwrap();
    for absent in [
        "SYNTHETIC-CREATOR",
        "SYNTHETIC-BASE",
        "creator_",
        "base_instructions",
    ] {
        assert!(!json.contains(absent), "{absent}");
    }
}

#[test]
fn forked_rollouts_use_their_own_id() {
    let s = find(B);
    assert_eq!(s.native_id, B, "not the copied source session_meta");
    assert_eq!(s.forked_from.as_deref(), Some(A));
    assert_eq!(s.title.as_deref(), Some("Regression test"));
}

#[test]
fn wardex_originated_threads_and_untested_versions() {
    let s = find(C);
    assert_eq!(s.entrypoint.as_deref(), Some("wardex"));
    assert_eq!(s.cwd, Path::new("/work/svc/.worktree/fix"));
    assert!(s.warnings.iter().any(|w| w == FORMAT_VERSION_UNTESTED));
}

#[test]
fn history_from_completed_items() {
    let s = find(A);
    let h = store().read_history(&s).unwrap();
    use HistoryKind::*;
    let kinds: Vec<HistoryKind> = h.items.iter().map(|i| i.kind).collect();
    assert_eq!(
        kinds,
        [
            Prompt, Message, ToolCall, ToolCall, ToolCall, Message, Compaction, Prompt, Message
        ]
    );
    let tools: Vec<_> = h.items.iter().filter_map(|i| i.tool.as_ref()).collect();
    assert_eq!(tools[0].name, "shell");
    assert_eq!(tools[0].input_summary, "cargo test login");
    assert_eq!(tools[0].status, ToolStatus::Failed);
    assert_eq!(tools[1].name, "file_change");
    assert_eq!(tools[1].input_summary, "/work/svc/tests/login.rs");
    assert!(
        tools[1]
            .input
            .as_deref()
            .unwrap()
            .contains("wait_for_ready")
    );
    assert_eq!(tools[2].name, "mcp:docs/search");
    assert_eq!(
        tools[2].result_summary.as_deref(),
        Some("found retry::wait")
    );
    let turns: Vec<u64> = h.items.iter().map(|i| i.turn).collect();
    assert_eq!(turns, [1, 1, 1, 1, 1, 1, 1, 2, 2]);
    assert_eq!(h.items[0].ts_ms, Some(1_790_848_800_000));
    // References are line numbers.
    assert_eq!(h.items[0].reference, "5");
    assert!(h.warnings.is_empty(), "{:?}", h.warnings);
}

#[test]
fn sensitive_fields_never_reach_the_output() {
    let s = find(A);
    let h = store().read_history(&s).unwrap();
    let q = HistoryQuery {
        tools: ToolDetail::Full,
        ..Default::default()
    };
    let json = serde_json::to_string(&page(s, h, &q).unwrap()).unwrap();
    for absent in [
        "SYNTHETIC-DEVELOPER-MESSAGE",
        "SYNTHETIC-ENCRYPTED",
        "SYNTHETIC-REASONING",
        "SYNTHETIC-CREATOR",
        "SYNTHETIC-BASE",
        "encrypted_content",
        "xxxxxxxxxxxxxxxx",
    ] {
        assert!(!json.contains(absent), "{absent} leaked: {json}");
    }
    assert!(json.contains("TOKEN=[REDACTED]"), "{json}");
}

#[test]
fn partial_last_line() {
    let h = store().read_history(&find(B)).unwrap();
    assert!(h.warnings.iter().any(|w| w == "partial_last_line"));
    let texts: Vec<&str> = h.items.iter().filter_map(|i| i.text.as_deref()).collect();
    assert_eq!(texts.last(), Some(&"Also add a regression test"));
}

#[test]
fn legacy_rollouts_fall_back_to_response_items() {
    let h = store().read_history(&find(G)).unwrap();
    assert!(h.warnings.iter().any(|w| w == "legacy_format"));
    let kinds: Vec<HistoryKind> = h.items.iter().map(|i| i.kind).collect();
    assert_eq!(
        kinds,
        [
            HistoryKind::Prompt,
            HistoryKind::ToolCall,
            HistoryKind::Message
        ]
    );
    assert_eq!(h.items[0].text.as_deref(), Some("List the files"));
    let tool = h.items[1].tool.as_ref().unwrap();
    assert_eq!(tool.name, "shell");
    assert_eq!(tool.status, ToolStatus::Completed);
    assert_eq!(tool.output.as_deref(), Some("Cargo.toml\nsrc"));
}

#[test]
fn compressed_rollouts() {
    let dir = tempfile::tempdir().unwrap();
    let day = dir.path().join("sessions/2026/10/02");
    std::fs::create_dir_all(&day).unwrap();
    let plain = std::fs::read(store().find(A).unwrap()[0].path.clone()).unwrap();
    let packed = ruzstd::encoding::compress_to_vec(
        plain.as_slice(),
        ruzstd::encoding::CompressionLevel::Fastest,
    );
    let id = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a06";
    std::fs::write(
        day.join(format!("rollout-2026-10-02T09-00-00-{id}.jsonl.zst")),
        packed,
    )
    .unwrap();
    let store = CodexStore::new(dir.path().to_path_buf(), 16 * 1024 * 1024);
    let found = store.discover(&ExternalFilter::default()).unwrap();
    assert_eq!(found.len(), 1);
    let s = &found[0];
    assert_eq!(s.native_id, id);
    assert!(s.compressed);
    assert_eq!(s.cwd, Path::new("/work/svc"));
    let h = store.read_history(s).unwrap();
    assert_eq!(h.items.len(), 9);

    // The decompressed size counts against the limit too.
    let tight = CodexStore::new(dir.path().to_path_buf(), plain.len() as u64 - 1);
    let s = tight.find(id).unwrap().remove(0);
    assert!(tight.read_history(&s).is_err());
}
