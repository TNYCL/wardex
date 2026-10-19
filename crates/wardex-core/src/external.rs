//! External sessions: Claude Code and Codex sessions started outside Wardex,
//! read from the CLIs' own stores. Wardex only reads those stores; it never
//! writes, locks or deletes anything in them. See `docs/external-sessions.md`.
//!
//! The format readers live in `wardex-drivers` (they are driver specific);
//! this module holds the normalized model, the paging of history and the
//! checks every reader shares.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::{DriverKind, SessionId, ToolStatus};

#[derive(Debug, thiserror::Error)]
pub enum ExternalError {
    #[error("no external session matches {0:?}")]
    NotFound(String),
    #[error("{0:?} matches several external sessions; use more characters or a driver prefix")]
    Ambiguous(String),
    #[error("{0}")]
    Invalid(String),
    #[error("cannot read {path}: {source}")]
    Io {
        /// Relative to the store root.
        path: String,
        source: std::io::Error,
    },
}

pub type Result<T, E = ExternalError> = std::result::Result<T, E>;

/// A session found in a CLI's store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExternalSession {
    pub driver: DriverKind,
    /// Claude `sessionId` / Codex thread id.
    pub native_id: String,
    /// The session's starting working directory.
    pub cwd: PathBuf,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub created_ms: Option<i64>,
    /// Modification time of the transcript.
    pub updated_ms: i64,
    #[serde(default)]
    pub cli_version: Option<String>,
    /// Claude `entrypoint` / Codex `originator`.
    #[serde(default)]
    pub entrypoint: Option<String>,
    /// Codex `forked_from_id`.
    #[serde(default)]
    pub forked_from: Option<String>,
    /// Claude `continued-in`: the conversation went on under this id.
    #[serde(default)]
    pub continued_in: Option<String>,
    pub size_bytes: u64,
    #[serde(default)]
    pub compressed: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub subagent: bool,
    /// The Wardex session bound to this native id in this daemon's database.
    #[serde(default)]
    pub managed_by: Option<SessionId>,
    pub liveness: Liveness,
    /// Transcript path relative to the store root.
    pub path_hint: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Absolute transcript path; never sent to clients.
    #[serde(skip)]
    pub path: PathBuf,
}

impl ExternalSession {
    /// `claude:<id>` / `codex:<id>`.
    pub fn qualified_id(&self) -> String {
        format!("{}:{}", self.driver.as_str(), self.native_id)
    }
}

/// Whether a CLI process has the session open. Only detected: nothing is
/// ever signalled or locked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Liveness {
    /// Hard evidence of a process that has the session open.
    Live {
        pid: u32,
        evidence: LiveEvidence,
    },
    /// No hard evidence, but the transcript was written within
    /// `external.quiet_secs`, or a CLI process runs in the session's cwd.
    Recent {
        modified_secs_ago: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pid: Option<u32>,
    },
    Inactive,
    /// `/proc` could not be read.
    Unknown {
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveEvidence {
    /// The CLI's own live-session registry, verified against `/proc`.
    Registry,
    /// The session id in a CLI process's arguments.
    Cmdline,
    /// A CLI process in the session's cwd and a freshly written transcript.
    CwdRecent,
}

impl Liveness {
    pub fn not_probed() -> Self {
        Self::Unknown {
            reason: "not probed".into(),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Live { .. } => "live",
            Self::Recent { .. } => "recent",
            Self::Inactive => "inactive",
            Self::Unknown { .. } => "unknown",
        }
    }

    pub fn is_live(&self) -> bool {
        matches!(self, Self::Live { .. })
    }
}

/// Which external sessions to list.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExternalFilter {
    #[serde(default)]
    pub driver: Option<DriverKind>,
    /// Only sessions whose starting cwd is this (absolute) directory.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// With `cwd`: also sessions in directories below it.
    #[serde(default)]
    pub under: bool,
    /// Only sessions whose transcript changed at or after this time.
    #[serde(default)]
    pub since_ms: Option<i64>,
    #[serde(default)]
    pub limit: Option<usize>,
    /// Also sessions this daemon already manages.
    #[serde(default)]
    pub include_managed: bool,
    /// Also archived sessions (Codex).
    #[serde(default)]
    pub archived: bool,
    /// Also subagent threads (Codex).
    #[serde(default)]
    pub include_subagents: bool,
}

impl ExternalFilter {
    /// The conditions a store checks itself: time, archive and subagent.
    /// The cwd is checked by the caller on normalized paths.
    pub fn admits(&self, session: &ExternalSession) -> bool {
        (self.archived || !session.archived)
            && (self.include_subagents || !session.subagent)
            && self.since_ms.is_none_or(|t| session.updated_ms >= t)
    }
}

/// How much of a tool call `history` returns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolDetail {
    /// No tool items.
    None,
    /// Name, input and result summaries.
    #[default]
    Summary,
    /// Also the full input and output (each capped at 256 KiB).
    Full,
}

impl ToolDetail {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "summary" => Some(Self::Summary),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

pub const DEFAULT_HISTORY_TURNS: usize = 20;
pub const DEFAULT_HISTORY_BYTES: usize = 64 * 1024;
const MAX_HISTORY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryQuery {
    /// Number of turns (prompts) per page.
    pub turns: usize,
    /// Only items before this cursor (a page's `next_before`).
    pub before: Option<String>,
    pub tools: ToolDetail,
    /// Byte budget of the page's items; the oldest are dropped first.
    pub max_bytes: usize,
    /// Mask secrets on a best-effort basis.
    pub redact: bool,
}

impl Default for HistoryQuery {
    fn default() -> Self {
        Self {
            turns: DEFAULT_HISTORY_TURNS,
            before: None,
            tools: ToolDetail::Summary,
            max_bytes: DEFAULT_HISTORY_BYTES,
            redact: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryKind {
    Prompt,
    Message,
    ToolCall,
    Compaction,
}

/// One normalized history entry. Built from an allowlist of fields; raw
/// transcript lines are never returned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryItem {
    /// Position in the conversation, from 1. Stable across pages.
    pub seq: u64,
    /// Number of prompts so far (0 before the first prompt).
    pub turn: u64,
    #[serde(default)]
    pub ts_ms: Option<i64>,
    pub kind: HistoryKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<HistoryTool>,
    /// Claude message uuid / Codex line number.
    #[serde(rename = "ref")]
    pub reference: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryTool {
    pub name: String,
    pub input_summary: String,
    /// `started` when no result was recorded (e.g. an interrupted turn).
    pub status: ToolStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

/// A whole conversation as a reader produced it.
#[derive(Debug, Clone, Default)]
pub struct History {
    /// Chronological; `seq` from 1, tool items with full input/output.
    pub items: Vec<HistoryItem>,
    pub warnings: Vec<String>,
}

impl History {
    /// Append an item, numbering it and counting turns.
    pub fn push(&mut self, ts_ms: Option<i64>, kind: HistoryKind, reference: String) -> usize {
        let turn = self.items.last().map_or(0, |i| i.turn) + u64::from(kind == HistoryKind::Prompt);
        self.items.push(HistoryItem {
            seq: self.items.len() as u64 + 1,
            turn,
            ts_ms,
            kind,
            text: None,
            tool: None,
            reference,
        });
        self.items.len() - 1
    }

    pub fn push_text(
        &mut self,
        ts_ms: Option<i64>,
        kind: HistoryKind,
        reference: String,
        text: String,
    ) {
        let i = self.push(ts_ms, kind, reference);
        self.items[i].text = Some(text);
    }

    pub fn warn(&mut self, warning: impl Into<String>) {
        let warning = warning.into();
        if !self.warnings.contains(&warning) {
            self.warnings.push(warning);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryPage {
    pub session: ExternalSession,
    pub items: Vec<HistoryItem>,
    /// Cursor for the previous (older) page, if there is one.
    pub next_before: Option<String>,
    /// Items were dropped or cut to stay within `max_bytes`.
    pub truncated: bool,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// One page of `history` out of a whole conversation.
pub fn page(session: ExternalSession, history: History, q: &HistoryQuery) -> Result<HistoryPage> {
    let before = match &q.before {
        Some(c) => Some(
            c.trim()
                .parse::<u64>()
                .map_err(|_| ExternalError::Invalid(format!("invalid history cursor {c:?}")))?,
        ),
        None => None,
    };
    let mut items: Vec<HistoryItem> = history
        .items
        .into_iter()
        .filter(|i| before.is_none_or(|b| i.seq < b))
        .filter(|i| q.tools != ToolDetail::None || i.kind != HistoryKind::ToolCall)
        .collect();
    let first_seq = items.first().map(|i| i.seq);
    // The last `turns` turns.
    let turns = q.turns.max(1);
    let mut seen = 0;
    let mut current = None;
    let mut start = items.len();
    for (idx, item) in items.iter().enumerate().rev() {
        if current != Some(item.turn) {
            if seen == turns {
                break;
            }
            seen += 1;
            current = Some(item.turn);
        }
        start = idx;
    }
    let mut items = items.split_off(start);
    for item in &mut items {
        if let Some(tool) = &mut item.tool
            && q.tools == ToolDetail::Summary
        {
            tool.input = None;
            tool.output = None;
        }
        if q.redact {
            redact_item(item);
        }
    }
    let budget = q.max_bytes.clamp(1024, MAX_HISTORY_BYTES);
    let sizes: Vec<usize> = items.iter().map(item_size).collect();
    let mut total: usize = sizes.iter().sum();
    let mut drop = 0;
    while total > budget && drop + 1 < items.len() {
        total -= sizes[drop];
        drop += 1;
    }
    let mut truncated = drop > 0;
    items.drain(..drop);
    if total > budget
        && let Some(item) = items.first_mut()
    {
        shrink_item(item, budget);
        truncated = true;
    }
    let next_before = match (items.first(), first_seq) {
        (Some(first), Some(oldest)) if first.seq > oldest => Some(first.seq.to_string()),
        _ => None,
    };
    Ok(HistoryPage {
        session,
        items,
        next_before,
        truncated,
        warnings: history.warnings,
    })
}

fn item_size(item: &HistoryItem) -> usize {
    serde_json::to_string(item).map_or(0, |s| s.len())
}

/// Cut the longest texts of a single item that alone exceeds the budget.
fn shrink_item(item: &mut HistoryItem, budget: usize) {
    let keep = budget / 2;
    let cut = |s: &mut String| {
        if s.len() > keep {
            let mut end = keep;
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            let more = s.len() - end;
            s.truncate(end);
            s.push_str(&format!("\n[wardex: {more} more bytes not shown]"));
        }
    };
    if let Some(text) = &mut item.text {
        cut(text);
    }
    if let Some(tool) = &mut item.tool {
        for s in [&mut tool.input, &mut tool.output].into_iter().flatten() {
            cut(s);
        }
    }
}

fn redact_item(item: &mut HistoryItem) {
    let apply = |s: &mut String| {
        if let Cow::Owned(r) = redact(s) {
            *s = r;
        }
    };
    if let Some(text) = &mut item.text {
        apply(text);
    }
    if let Some(tool) = &mut item.tool {
        apply(&mut tool.input_summary);
        for s in [&mut tool.result_summary, &mut tool.input, &mut tool.output]
            .into_iter()
            .flatten()
        {
            apply(s);
        }
    }
}

const REDACTED: &str = "[REDACTED]";

static PEM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
    )
    .expect("valid regex")
});
static AUTH_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b((?:proxy-)?authorization["']?\s*[:=]\s*["']?)(?:(?:bearer|basic|token|digest)\s+)?[^\s"',;]+"#)
        .expect("valid regex")
});
static ENV_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\b([A-Z0-9_]*(?:KEY|TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIALS?)[A-Z0-9_]*\s*=\s*)("[^"\n]*"|'[^'\n]*'|[^\s"']+)"#)
        .expect("valid regex")
});
static FIELD_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(["']?(?:api[_-]?key|access[_-]?token|refresh[_-]?token|auth[_-]?token|client[_-]?secret|secret|password|passwd|token)["']?\s*:\s*)("[^"\n]*"|'[^'\n]*')"#)
        .expect("valid regex")
});
static TOKENS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\bsk-[A-Za-z0-9_-]{20,}",
        r"|\bgh[pousr]_[A-Za-z0-9]{30,}",
        r"|\bgithub_pat_[A-Za-z0-9_]{30,}",
        r"|\bglpat-[A-Za-z0-9_-]{20,}",
        r"|\bxox[abprs]-[A-Za-z0-9-]{10,}",
        r"|\bAKIA[0-9A-Z]{16}\b",
        r"|\bAIza[0-9A-Za-z_-]{35}",
        r"|\beyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
        r"|(?i:\bbearer\s+)[A-Za-z0-9._~+/-]{16,}=*",
    ))
    .expect("valid regex")
});

/// Mask well-known secret shapes. Best effort only: a transcript can hold
/// anything a user pasted or a tool printed.
pub fn redact(text: &str) -> Cow<'_, str> {
    let mut out = Cow::Borrowed(text);
    let replace = |out: &mut Cow<'_, str>, re: &Regex, keep_prefix: bool| {
        if re.is_match(out) {
            let replaced = re
                .replace_all(out, |c: &regex::Captures<'_>| {
                    if keep_prefix {
                        format!("{}{REDACTED}", &c[1])
                    } else {
                        REDACTED.to_string()
                    }
                })
                .into_owned();
            *out = Cow::Owned(replaced);
        }
    };
    replace(&mut out, &PEM, false);
    replace(&mut out, &AUTH_HEADER, true);
    replace(&mut out, &ENV_ASSIGNMENT, true);
    replace(&mut out, &FIELD_ASSIGNMENT, true);
    replace(&mut out, &TOKENS, false);
    out
}

/// Tested and observed CLI versions per driver (inclusive).
const TESTED_VERSIONS: [(DriverKind, &str, &str); 2] = [
    (DriverKind::Claude, "2.1.295", "2.1.302"),
    (DriverKind::Codex, "0.162.1", "0.163.0"),
];

/// Whether `version` is outside the range the readers were tested with.
pub fn version_untested(driver: DriverKind, version: &str) -> bool {
    let parse = |v: &str| -> Option<Vec<u64>> {
        v.trim()
            .split(['-', '+', ' '])
            .next()?
            .split('.')
            .map(|p| p.parse().ok())
            .collect()
    };
    let Some(v) = parse(version) else {
        return true;
    };
    TESTED_VERSIONS
        .iter()
        .find(|(d, ..)| *d == driver)
        .is_none_or(|(_, lo, hi)| {
            parse(lo).is_some_and(|lo| v < lo) || parse(hi).is_some_and(|hi| v > hi)
        })
}

pub const FORMAT_VERSION_UNTESTED: &str = "format_version_untested";

/// A session's cwd in comparable form: symlinks resolved when it exists.
pub fn normalize_cwd(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.components().collect())
}

/// Whether `cwd` is `want` (or below it, with `under`). Both normalized.
pub fn cwd_matches(cwd: &Path, want: &Path, under: bool) -> bool {
    if under {
        cwd.starts_with(want)
    } else {
        cwd == want
    }
}

/// The effective uid of this process (the owner of `/proc/self`).
pub fn own_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").ok().map(|m| m.uid())
}

/// `path` relative to `root`, for messages and events (never an absolute
/// home path).
pub fn relative_hint(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Check a transcript before reading it: a regular file (not a symlink)
/// below `root`, owned by this user and at most `max_bytes` long.
pub fn check_transcript(root: &Path, path: &Path, max_bytes: u64) -> Result<std::fs::Metadata> {
    use std::os::unix::fs::MetadataExt;
    let hint = relative_hint(root, path);
    let io = |source| ExternalError::Io {
        path: hint.clone(),
        source,
    };
    let meta = std::fs::symlink_metadata(path).map_err(io)?;
    if !meta.file_type().is_file() {
        return Err(ExternalError::Invalid(format!(
            "{hint} is not a regular file"
        )));
    }
    let root = std::fs::canonicalize(root).map_err(io)?;
    let real = std::fs::canonicalize(path).map_err(io)?;
    if !real.starts_with(&root) {
        return Err(ExternalError::Invalid(format!(
            "{hint} is outside the store root"
        )));
    }
    if own_uid().is_some_and(|uid| uid != meta.uid()) {
        return Err(ExternalError::Invalid(format!(
            "{hint} belongs to another user"
        )));
    }
    if meta.len() > max_bytes {
        return Err(ExternalError::Invalid(format!(
            "{hint} is {} MiB, above external.max_file_mb",
            meta.len() / (1024 * 1024)
        )));
    }
    Ok(meta)
}

/// Whether `s` looks like a UUID (8-4-4-4-12 hex digits).
pub fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// A store of external sessions of one CLI.
pub trait ExternalSource: Send + Sync + 'static {
    fn kind(&self) -> DriverKind;

    /// The store root (e.g. `~/.claude`).
    fn root(&self) -> &Path;

    /// Sessions that pass `filter.admits` (cwd is checked by the caller).
    fn discover(&self, filter: &ExternalFilter) -> Result<Vec<ExternalSession>>;

    /// Sessions whose native id starts with `prefix`, archived and subagent
    /// sessions included.
    fn find(&self, prefix: &str) -> Result<Vec<ExternalSession>>;

    /// The whole conversation of a session returned by `discover` or `find`.
    fn read_history(&self, session: &ExternalSession) -> Result<History>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> ExternalSession {
        ExternalSession {
            driver: DriverKind::Claude,
            native_id: "00000000-0000-4000-8000-000000000001".into(),
            cwd: "/work/app".into(),
            title: None,
            created_ms: None,
            updated_ms: 0,
            cli_version: None,
            entrypoint: None,
            forked_from: None,
            continued_in: None,
            size_bytes: 0,
            compressed: false,
            archived: false,
            subagent: false,
            managed_by: None,
            liveness: Liveness::Inactive,
            path_hint: String::new(),
            warnings: Vec::new(),
            path: PathBuf::new(),
        }
    }

    /// Three turns: prompt, tool call, message each.
    fn history() -> History {
        let mut h = History::default();
        for t in 1..=3 {
            h.push_text(
                None,
                HistoryKind::Prompt,
                format!("p{t}"),
                format!("prompt {t}"),
            );
            let i = h.push(None, HistoryKind::ToolCall, format!("t{t}"));
            h.items[i].tool = Some(HistoryTool {
                name: "Bash".into(),
                input_summary: "ls".into(),
                status: ToolStatus::Completed,
                result_summary: Some("ok".into()),
                input: Some("{\"command\":\"ls\"}".into()),
                output: Some("ok".into()),
            });
            h.push_text(
                None,
                HistoryKind::Message,
                format!("m{t}"),
                format!("answer {t}"),
            );
        }
        h
    }

    #[test]
    fn pages_by_turn_with_a_cursor() {
        let q = HistoryQuery {
            turns: 2,
            ..Default::default()
        };
        let p = page(session(), history(), &q).unwrap();
        let seqs: Vec<u64> = p.items.iter().map(|i| i.seq).collect();
        assert_eq!(seqs, [4, 5, 6, 7, 8, 9]);
        assert_eq!(p.next_before.as_deref(), Some("4"));
        assert!(!p.truncated);
        // Summaries only by default.
        assert!(p.items[1].tool.as_ref().unwrap().output.is_none());
        let older = page(
            session(),
            history(),
            &HistoryQuery {
                before: p.next_before.clone(),
                ..q.clone()
            },
        )
        .unwrap();
        let seqs: Vec<u64> = older.items.iter().map(|i| i.seq).collect();
        assert_eq!(seqs, [1, 2, 3]);
        assert_eq!(older.next_before, None);
        assert!(
            page(
                session(),
                history(),
                &HistoryQuery {
                    before: Some("x".into()),
                    ..q
                }
            )
            .is_err()
        );
    }

    #[test]
    fn tool_detail_levels() {
        let none = HistoryQuery {
            tools: ToolDetail::None,
            ..Default::default()
        };
        let p = page(session(), history(), &none).unwrap();
        assert!(p.items.iter().all(|i| i.kind != HistoryKind::ToolCall));
        let full = HistoryQuery {
            tools: ToolDetail::Full,
            ..Default::default()
        };
        let p = page(session(), history(), &full).unwrap();
        let tool = p.items[1].tool.as_ref().unwrap();
        assert_eq!(tool.output.as_deref(), Some("ok"));
        assert_eq!(p.items.len(), 9);
    }

    #[test]
    fn byte_budget_drops_the_oldest_items() {
        let mut h = History::default();
        for t in 0..20 {
            h.push_text(None, HistoryKind::Prompt, format!("p{t}"), "x".repeat(200));
        }
        let q = HistoryQuery {
            max_bytes: 1024,
            ..Default::default()
        };
        let p = page(session(), h, &q).unwrap();
        assert!(p.truncated);
        assert!(p.items.len() < 20 && !p.items.is_empty());
        assert_eq!(p.items.last().unwrap().seq, 20);
        assert_eq!(
            p.next_before,
            Some(p.items[0].seq.to_string()),
            "the dropped items are on the next page"
        );
        let total: usize = p.items.iter().map(item_size).sum();
        assert!(total <= 1024);

        // A single huge item is cut instead.
        let mut h = History::default();
        h.push_text(None, HistoryKind::Message, "m".into(), "y".repeat(10_000));
        let p = page(session(), h, &q).unwrap();
        assert!(p.truncated);
        assert!(p.items[0].text.as_ref().unwrap().len() < 1024);
    }

    #[test]
    fn redacts_common_secret_shapes() {
        // Token-shaped inputs are assembled at runtime so secret scanners
        // do not flag this file.
        let gh = format!("gh{}_{}", "p", "x".repeat(36));
        let anthropic = format!("sk-{}-{}", "ant", "x".repeat(24));
        let aws = format!("AK{}{}", "IA", "X".repeat(16));
        let pem = format!(
            "-----BEGIN OPENSSH {k} KEY-----\nb3BlbnNzaA==\n-----END OPENSSH {k} KEY-----",
            k = "PRIVATE"
        );
        let cases = [
            (format!("export GITHUB_TOKEN={gh}"), "xxxxxxxx".to_string()),
            ("API_KEY=\"hunter2-secret\" next".into(), "hunter2".into()),
            (
                "curl -H 'Authorization: Bearer abc.def.ghi'".into(),
                "abc.def".into(),
            ),
            ("{\"password\": \"swordfish\"}".into(), "swordfish".into()),
            (format!("key {anthropic} end"), "sk-ant".into()),
            (format!("plain {gh} text"), gh.clone()),
            (format!("aws {aws}"), aws.clone()),
            (pem, "b3BlbnNzaA".into()),
        ];
        for (input, secret) in cases {
            let out = redact(&input);
            assert!(!out.contains(&secret), "{input:?} -> {out:?}");
            assert!(out.contains(REDACTED), "{input:?} -> {out:?}");
        }
        // Ordinary text is left alone and not copied.
        let plain = "fn monkey(key: &str) -> Token { primary_key = 3 }";
        assert!(matches!(redact(plain), Cow::Borrowed(_)));
        assert!(redact("API_KEY=x").starts_with("API_KEY="));
    }

    #[test]
    fn redaction_applies_to_tool_text() {
        let mut h = History::default();
        let i = h.push(None, HistoryKind::ToolCall, "t".into());
        h.items[i].tool = Some(HistoryTool {
            name: "Bash".into(),
            input_summary: "env".into(),
            status: ToolStatus::Completed,
            result_summary: Some("DB_PASSWORD=letmein".into()),
            input: None,
            output: Some("DB_PASSWORD=letmein\nHOME=/x".into()),
        });
        let q = HistoryQuery {
            tools: ToolDetail::Full,
            ..Default::default()
        };
        let p = page(session(), h.clone(), &q).unwrap();
        let json = serde_json::to_string(&p).unwrap();
        assert!(!json.contains("letmein"), "{json}");
        let raw = page(session(), h, &HistoryQuery { redact: false, ..q }).unwrap();
        assert!(serde_json::to_string(&raw).unwrap().contains("letmein"));
    }

    #[test]
    fn version_ranges() {
        assert!(!version_untested(DriverKind::Claude, "2.1.302"));
        assert!(!version_untested(DriverKind::Claude, "2.1.295"));
        assert!(version_untested(DriverKind::Claude, "2.1.303"));
        assert!(version_untested(DriverKind::Claude, "2.0.9"));
        assert!(!version_untested(DriverKind::Codex, "0.163.0"));
        assert!(!version_untested(DriverKind::Codex, "0.162.1-alpha"));
        assert!(version_untested(DriverKind::Codex, "0.170.0"));
        assert!(version_untested(DriverKind::Codex, "garbage"));
    }

    #[test]
    fn cwd_matching_and_ids() {
        let a = Path::new("/work/app");
        assert!(cwd_matches(a, a, false));
        assert!(!cwd_matches(Path::new("/work/app/.worktree/x"), a, false));
        assert!(cwd_matches(Path::new("/work/app/.worktree/x"), a, true));
        assert!(!cwd_matches(Path::new("/work/app-b"), a, true));
        assert_eq!(
            normalize_cwd(Path::new("/nonexistent/dir/")),
            Path::new("/nonexistent/dir")
        );
        assert!(is_uuid("0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"));
        assert!(!is_uuid("0190a1b2c3d47e5f8a9b0c1d2e3f4a5b"));
        assert!(!is_uuid("../../etc/passwd"));
    }

    #[test]
    fn transcript_checks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let file = root.join("a.jsonl");
        std::fs::write(&file, "{}\n").unwrap();
        assert!(check_transcript(&root, &file, 1024).is_ok());
        assert!(check_transcript(&root, &file, 1).is_err());
        let outside = dir.path().join("outside.jsonl");
        std::fs::write(&outside, "{}\n").unwrap();
        let link = root.join("link.jsonl");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let err = check_transcript(&root, &link, 1024).unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");
        assert!(check_transcript(&root, &outside, 1024).is_err());
    }
}
