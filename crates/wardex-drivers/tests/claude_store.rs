//! Claude Code transcript reader against synthetic fixtures shaped like
//! Claude Code 2.1.30x transcripts (`tests/fixtures/claude/store`).

use std::path::{Path, PathBuf};

use wardex_core::external::{
    ExternalFilter, ExternalSource, FORMAT_VERSION_UNTESTED, HistoryKind, HistoryQuery, ToolDetail,
    page,
};
use wardex_core::{DriverKind, ToolStatus};
use wardex_drivers::ClaudeStore;
use wardex_drivers::claude_store::parse_history;

const S1: &str = "11111111-1111-4111-8111-111111111111";
const S2: &str = "22222222-2222-4222-8222-222222222222";
const S3: &str = "33333333-3333-4333-8333-333333333333";

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude/store")
}

fn store() -> ClaudeStore {
    ClaudeStore::new(root(), 16 * 1024 * 1024)
}

fn find(id: &str) -> wardex_core::external::ExternalSession {
    let mut found = store().find(id).unwrap();
    assert_eq!(found.len(), 1, "{id}");
    found.remove(0)
}

#[test]
fn discovers_top_level_transcripts_only() {
    let mut sessions = store().discover(&ExternalFilter::default()).unwrap();
    sessions.sort_by(|a, b| a.native_id.cmp(&b.native_id));
    let ids: Vec<&str> = sessions.iter().map(|s| s.native_id.as_str()).collect();
    // No subagent transcript, no `notes.jsonl`, no memory files.
    assert_eq!(ids, [S1, S2, S3]);
    assert!(sessions.iter().all(|s| s.driver == DriverKind::Claude));
}

#[test]
fn session_metadata() {
    let s = find(S1);
    assert_eq!(s.cwd, Path::new("/work/app"));
    // custom-title wins over ai-title.
    assert_eq!(s.title.as_deref(), Some("Health check"));
    assert_eq!(s.cli_version.as_deref(), Some("2.1.300"));
    assert_eq!(s.entrypoint.as_deref(), Some("cli"));
    assert_eq!(s.created_ms, Some(1_790_848_800_000));
    assert_eq!(
        s.path_hint,
        format!("projects/-work-app/{S1}.jsonl"),
        "relative to the root"
    );
    assert!(s.warnings.is_empty(), "{:?}", s.warnings);
    let json = serde_json::to_value(&s).unwrap();
    assert!(json.get("path").is_none(), "absolute paths are not sent");
    assert_eq!(json["liveness"]["state"], "unknown");
}

#[test]
fn first_cwd_wins_and_the_directory_name_is_ignored() {
    // Both live in `-work-app-b`: the encoding is lossy.
    let b = find(S2);
    assert_eq!(b.cwd, Path::new("/work/app-b"), "not the later /sub cwd");
    let nested = find(S3);
    assert_eq!(nested.cwd, Path::new("/work/app/b"));
    assert_eq!(nested.title.as_deref(), Some("Directory listing"));
}

#[test]
fn continued_in_and_untested_versions_warn() {
    let s = find(S2);
    assert_eq!(
        s.continued_in.as_deref(),
        Some("44444444-4444-4444-8444-444444444444")
    );
    assert_eq!(s.entrypoint.as_deref(), Some("claude-desktop"));
    assert!(s.warnings.iter().any(|w| w == FORMAT_VERSION_UNTESTED));
    assert!(s.warnings.iter().any(|w| w.starts_with("continued_in")));
}

#[test]
fn prefix_lookup() {
    assert_eq!(store().find("1111").unwrap().len(), 1);
    assert_eq!(store().find("").unwrap().len(), 3);
    assert!(store().find("9999").unwrap().is_empty());
}

#[test]
fn active_branch_history() {
    let s = find(S1);
    let h = store().read_history(&s).unwrap();
    let kinds: Vec<HistoryKind> = h.items.iter().map(|i| i.kind).collect();
    use HistoryKind::*;
    assert_eq!(
        kinds,
        [
            Prompt, Message, ToolCall, Message, Prompt, Compaction, Prompt, ToolCall, Message
        ]
    );
    let texts: Vec<&str> = h.items.iter().filter_map(|i| i.text.as_deref()).collect();
    assert_eq!(texts[0], "Add a health check endpoint");
    assert_eq!(texts[2], "Added GET /healthz returning 200.");
    assert_eq!(texts[3], "Now add a test");
    assert_eq!(texts[5], "Run the tests");
    // The rewound reply, the sidechain, meta lines and the compact summary
    // are not part of the active branch.
    let all = serde_json::to_string(&h.items).unwrap();
    for absent in [
        "Old answer that was rewound",
        "SIDECHAIN-TEXT",
        "Synthetic meta message",
        "SYNTHETIC-COMPACT-SUMMARY",
        "SUBAGENT-PROMPT",
    ] {
        assert!(!all.contains(absent), "{absent}");
    }
    let turns: Vec<u64> = h.items.iter().map(|i| i.turn).collect();
    assert_eq!(turns, [1, 1, 1, 1, 2, 2, 3, 3, 3]);
    let seqs: Vec<u64> = h.items.iter().map(|i| i.seq).collect();
    assert_eq!(seqs, (1..=9).collect::<Vec<_>>());
    assert_eq!(h.items[0].reference, "u1");
    assert_eq!(h.items[0].ts_ms, Some(1_790_848_800_000));
    assert!(h.warnings.is_empty(), "{:?}", h.warnings);
}

#[test]
fn tool_calls_are_matched_with_their_results() {
    let h = store().read_history(&find(S1)).unwrap();
    let tools: Vec<_> = h.items.iter().filter_map(|i| i.tool.as_ref()).collect();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].name, "Bash");
    assert_eq!(tools[0].input_summary, "grep -rn health src/");
    assert_eq!(tools[0].status, ToolStatus::Completed);
    assert_eq!(
        tools[0].result_summary.as_deref(),
        Some("src/router.rs:12: // TODO health")
    );
    assert_eq!(tools[1].status, ToolStatus::Failed);
    assert_eq!(
        tools[1].output.as_deref(),
        Some("test failed: expected 200")
    );
}

#[test]
fn sensitive_fields_never_reach_the_output() {
    let s = find(S1);
    let h = store().read_history(&s).unwrap();
    let q = HistoryQuery {
        tools: ToolDetail::Full,
        ..Default::default()
    };
    let json = serde_json::to_string(&page(s, h, &q).unwrap()).unwrap();
    for absent in [
        "SYNTHETIC-THINKING",
        "SYNTHETIC-ORG-NAME",
        "credential_org",
        "attachment",
        "SYNTHETIC-OWNER",
        "ownerAccountUuid",
        "toolUseResult",
        "requestId",
        "xxxxxxxxxxxxxxxx",
    ] {
        assert!(!json.contains(absent), "{absent} leaked: {json}");
    }
    assert!(json.contains("API_KEY=[REDACTED]"), "{json}");
}

#[test]
fn partial_last_line_and_missing_leaf_marker() {
    let s = find(S2);
    let h = store().read_history(&s).unwrap();
    assert!(h.warnings.iter().any(|w| w == "partial_last_line"));
    assert!(h.warnings.iter().any(|w| w == "no_leaf_marker"));
    assert!(h.warnings.iter().any(|w| w == FORMAT_VERSION_UNTESTED));
    let texts: Vec<&str> = h.items.iter().filter_map(|i| i.text.as_deref()).collect();
    assert_eq!(texts, ["Explain the build", "It uses cargo."]);
}

#[test]
fn unknown_lines_and_types_are_tolerated() {
    let text = concat!(
        "{\"type\":\"brand-new-type\",\"uuid\":\"z0\",\"parentUuid\":null}\n",
        "not json at all\n",
        "{\"type\":\"user\",\"uuid\":\"z1\",\"parentUuid\":\"z0\",\"message\":{\"content\":\"hi\"}}\n",
        "{\"type\":\"assistant\",\"uuid\":\"z2\",\"parentUuid\":\"z1\",\"message\":{\"content\":[{\"type\":\"future_block\"},{\"type\":\"text\",\"text\":\"hello\"}]}}\n",
    );
    let h = parse_history(text);
    let texts: Vec<&str> = h.items.iter().filter_map(|i| i.text.as_deref()).collect();
    assert_eq!(texts, ["hi", "hello"]);
    assert!(h.warnings.iter().any(|w| w.contains("1 malformed")));
}

#[test]
fn rejects_symlinks_and_oversized_files() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("projects/-work-x");
    std::fs::create_dir_all(&project).unwrap();
    let real = dir.path().join("elsewhere.jsonl");
    std::fs::copy(
        root().join(format!("projects/-work-app-b/{S3}.jsonl")),
        &real,
    )
    .unwrap();
    std::os::unix::fs::symlink(&real, project.join(format!("{S3}.jsonl"))).unwrap();
    let store = ClaudeStore::new(dir.path().to_path_buf(), 1024 * 1024);
    assert!(
        store
            .discover(&ExternalFilter::default())
            .unwrap()
            .is_empty()
    );

    let small = ClaudeStore::new(root(), 64);
    assert!(
        small
            .discover(&ExternalFilter::default())
            .unwrap()
            .is_empty()
    );
    assert!(small.find(S1).is_err());
}

#[test]
fn since_filter_uses_the_file_time() {
    let filter = ExternalFilter {
        since_ms: Some(i64::MAX),
        ..Default::default()
    };
    assert!(store().discover(&filter).unwrap().is_empty());
}
