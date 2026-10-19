//! Codex rollout store: `<root>/sessions/YYYY/MM/DD/rollout-*-<id>.jsonl`
//! (older ones `.jsonl.zst`), archived ones under `<root>/archived_sessions`.
//! Read only; Codex's SQLite state is never opened. Format notes:
//! `docs/external-sessions.md`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde_json::Value;
use wardex_core::driver::{cap_tool_text, summarize};
use wardex_core::external::{
    ExternalError, ExternalFilter, ExternalSession, ExternalSource, FORMAT_VERSION_UNTESTED,
    History, HistoryKind, HistoryTool, Liveness, Result, check_transcript, is_uuid, relative_hint,
    version_untested,
};
use wardex_core::time::parse_rfc3339_ms;
use wardex_core::{DriverKind, ToolStatus};

use crate::claude_store::system_ms;

const RESULT_SUMMARY: usize = 300;
const INPUT_SUMMARY: usize = 200;
/// `sessions/YYYY/MM/DD/` plus some slack.
const MAX_DEPTH: usize = 5;
/// The first line (`session_meta`) can hold long base instructions.
const HEADER_BYTES: u64 = 4 * 1024 * 1024;

type Cache = HashMap<PathBuf, (SystemTime, u64, ExternalSession)>;
type Titles = Option<(SystemTime, HashMap<String, String>)>;

pub struct CodexStore {
    root: PathBuf,
    max_file_bytes: u64,
    cache: Mutex<Cache>,
    titles: Mutex<Titles>,
}

/// The thread id in a rollout file name, and whether it is compressed.
fn rollout_id(path: &Path) -> Option<(String, bool)> {
    let name = path.file_name()?.to_str()?;
    let (stem, compressed) = match name.strip_suffix(".jsonl.zst") {
        Some(stem) => (stem, true),
        None => (name.strip_suffix(".jsonl")?, false),
    };
    if !stem.starts_with("rollout-") {
        return None;
    }
    // `get`, not indexing: a multi-byte character at the cut must not panic.
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    is_uuid(id).then(|| (id.to_string(), compressed))
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() && depth < MAX_DEPTH {
            walk(&path, depth + 1, out);
        } else if kind.is_file() && rollout_id(&path).is_some() {
            out.push(path);
        }
    }
}

impl CodexStore {
    pub fn new(root: PathBuf, max_file_bytes: u64) -> Self {
        Self {
            root,
            max_file_bytes,
            cache: Mutex::new(HashMap::new()),
            titles: Mutex::new(None),
        }
    }

    fn rollouts(&self) -> Vec<(PathBuf, bool)> {
        let mut out = Vec::new();
        let mut live = Vec::new();
        walk(&self.root.join("sessions"), 0, &mut live);
        out.extend(live.into_iter().map(|p| (p, false)));
        let mut archived = Vec::new();
        walk(&self.root.join("archived_sessions"), 0, &mut archived);
        out.extend(archived.into_iter().map(|p| (p, true)));
        out
    }

    fn io(&self, path: &Path) -> impl Fn(std::io::Error) -> ExternalError {
        let hint = relative_hint(&self.root, path);
        move |source| ExternalError::Io {
            path: hint.clone(),
            source,
        }
    }

    /// A reader over the (decompressed) rollout, limited to
    /// `max_file_bytes` of content.
    fn open(&self, path: &Path, limit: u64) -> Result<Box<dyn Read>> {
        let file = std::fs::File::open(path).map_err(self.io(path))?;
        let reader: Box<dyn Read> = if rollout_id(path).is_some_and(|(_, z)| z) {
            let decoder = ruzstd::decoding::StreamingDecoder::new(file).map_err(|e| {
                ExternalError::Invalid(format!(
                    "{}: invalid zstd data: {e}",
                    relative_hint(&self.root, path)
                ))
            })?;
            Box::new(decoder)
        } else {
            Box::new(file)
        };
        Ok(Box::new(reader.take(limit)))
    }

    /// Latest `thread_name` per thread id from `session_index.jsonl`.
    fn title(&self, id: &str) -> Option<String> {
        let path = self.root.join("session_index.jsonl");
        let meta = std::fs::symlink_metadata(&path).ok()?;
        if !meta.file_type().is_file() || meta.len() > self.max_file_bytes {
            return None;
        }
        let mtime = meta.modified().ok()?;
        let mut titles = self.titles.lock().unwrap_or_else(|e| e.into_inner());
        if titles.as_ref().is_none_or(|(t, _)| *t != mtime) {
            let mut map = HashMap::new();
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            for line in text.lines() {
                let Ok(v) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                if let (Some(id), Some(name)) = (v["id"].as_str(), v["thread_name"].as_str()) {
                    map.insert(id.to_string(), name.to_string());
                }
            }
            *titles = Some((mtime, map));
        }
        titles
            .as_ref()
            .and_then(|(_, m)| m.get(id).cloned())
            .filter(|t| !t.trim().is_empty())
    }

    fn session(&self, path: &Path, archived: bool) -> Result<Option<ExternalSession>> {
        let meta = check_transcript(&self.root, path, self.max_file_bytes)?;
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let (native_id, compressed) =
            rollout_id(path).ok_or_else(|| ExternalError::Invalid("not a rollout file".into()))?;
        let cached = {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            cache
                .get(path)
                .filter(|(t, len, _)| *t == mtime && *len == meta.len())
                .map(|(_, _, s)| s.clone())
        };
        let mut session = match cached {
            Some(s) => s,
            None => {
                let mut first = String::new();
                BufReader::new(self.open(path, HEADER_BYTES)?)
                    .read_line(&mut first)
                    .map_err(self.io(path))?;
                let Ok(line) = serde_json::from_str::<Value>(&first) else {
                    return Ok(None);
                };
                if line["type"] != "session_meta" {
                    return Ok(None);
                }
                let p = &line["payload"];
                let Some(cwd) = p["cwd"].as_str() else {
                    return Ok(None);
                };
                let cli_version = p["cli_version"].as_str().map(String::from);
                let mut warnings = Vec::new();
                if cli_version
                    .as_deref()
                    .is_some_and(|v| version_untested(DriverKind::Codex, v))
                {
                    warnings.push(FORMAT_VERSION_UNTESTED.to_string());
                }
                let source = &p["source"];
                let subagent = source.get("subagent").is_some() || source == "subagent";
                let s = ExternalSession {
                    driver: DriverKind::Codex,
                    native_id: native_id.clone(),
                    cwd: PathBuf::from(cwd),
                    title: None,
                    created_ms: p["timestamp"]
                        .as_str()
                        .or(line["timestamp"].as_str())
                        .and_then(parse_rfc3339_ms),
                    updated_ms: system_ms(mtime),
                    cli_version,
                    entrypoint: p["originator"].as_str().map(String::from),
                    forked_from: p["forked_from_id"].as_str().map(String::from),
                    continued_in: None,
                    size_bytes: meta.len(),
                    compressed,
                    archived,
                    subagent,
                    managed_by: None,
                    liveness: Liveness::not_probed(),
                    path_hint: relative_hint(&self.root, path),
                    warnings,
                    path: path.to_path_buf(),
                };
                self.cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(path.to_path_buf(), (mtime, meta.len(), s.clone()));
                s
            }
        };
        session.title = self.title(&native_id);
        Ok(Some(session))
    }
}

impl ExternalSource for CodexStore {
    fn kind(&self) -> DriverKind {
        DriverKind::Codex
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn discover(&self, filter: &ExternalFilter) -> Result<Vec<ExternalSession>> {
        let mut out = Vec::new();
        for (path, archived) in self.rollouts() {
            if archived && !filter.archived {
                continue;
            }
            if let Some(since) = filter.since_ms
                && std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| system_ms(t) < since)
            {
                continue;
            }
            match self.session(&path, archived) {
                Ok(Some(s)) if filter.admits(&s) => out.push(s),
                Ok(_) => {}
                Err(error) => tracing::debug!(%error, "skipping codex rollout"),
            }
        }
        Ok(out)
    }

    fn find(&self, prefix: &str) -> Result<Vec<ExternalSession>> {
        let mut out = Vec::new();
        for (path, archived) in self.rollouts() {
            let matches = rollout_id(&path).is_some_and(|(id, _)| id.starts_with(prefix));
            if matches && let Some(s) = self.session(&path, archived)? {
                out.push(s);
            }
        }
        Ok(out)
    }

    fn read_history(&self, session: &ExternalSession) -> Result<History> {
        check_transcript(&self.root, &session.path, self.max_file_bytes)?;
        let mut bytes = Vec::new();
        self.open(&session.path, self.max_file_bytes + 1)?
            .read_to_end(&mut bytes)
            .map_err(self.io(&session.path))?;
        if bytes.len() as u64 > self.max_file_bytes {
            return Err(ExternalError::Invalid(format!(
                "{} is larger than external.max_file_mb when decompressed",
                session.path_hint
            )));
        }
        Ok(parse_history(&String::from_utf8_lossy(&bytes)))
    }
}

/// A rollout as normalized history. Uses the `item_completed` events of
/// current versions; rollouts without them fall back to response items.
pub fn parse_history(text: &str) -> History {
    let mut history = History::default();
    let complete = text.ends_with('\n');
    let lines: Vec<&str> = text.lines().collect();
    let mut parsed = Vec::new();
    let mut malformed = 0;
    for (n, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(v) => parsed.push((n + 1, v)),
            Err(_) if n + 1 == lines.len() && !complete => history.warn("partial_last_line"),
            Err(_) => malformed += 1,
        }
    }
    if malformed > 0 {
        history.warn(format!("skipped {malformed} malformed lines"));
    }
    if let Some(version) = parsed
        .iter()
        .find(|(_, v)| v["type"] == "session_meta")
        .and_then(|(_, v)| v["payload"]["cli_version"].as_str())
        && version_untested(DriverKind::Codex, version)
    {
        history.warn(FORMAT_VERSION_UNTESTED);
    }
    let items = parsed
        .iter()
        .any(|(_, v)| v["type"] == "event_msg" && v["payload"]["type"] == "item_completed");
    if items {
        for (n, v) in &parsed {
            if v["type"] == "event_msg" && v["payload"]["type"] == "item_completed" {
                let ts = v["payload"]["completed_at_ms"]
                    .as_i64()
                    .or_else(|| v["timestamp"].as_str().and_then(parse_rfc3339_ms));
                completed_item(&mut history, &v["payload"]["item"], ts, n.to_string());
            }
        }
    } else {
        history.warn("legacy_format");
        legacy_items(&mut history, &parsed);
    }
    history
}

fn texts(content: &Value, kind: &str) -> String {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| {
            b["type"]
                .as_str()
                .is_some_and(|t| t.eq_ignore_ascii_case(kind))
        })
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `bash -lc "<script>"` shows as the script.
fn command_text(command: &Value) -> String {
    match command {
        Value::String(s) => s.clone(),
        Value::Array(argv) => {
            let argv: Vec<&str> = argv.iter().filter_map(|a| a.as_str()).collect();
            match argv.as_slice() {
                [_, flag, script] if matches!(*flag, "-c" | "-lc") => script.to_string(),
                _ => argv.join(" "),
            }
        }
        _ => String::new(),
    }
}

fn completed_item(history: &mut History, item: &Value, ts: Option<i64>, reference: String) {
    let failed_status = item["status"].as_str() == Some("failed");
    let (name, input, output, failed) = match item["type"].as_str().unwrap_or_default() {
        "UserMessage" => {
            let text = texts(&item["content"], "text");
            if !text.trim().is_empty() {
                history.push_text(ts, HistoryKind::Prompt, reference, text);
            }
            return;
        }
        "AgentMessage" => {
            let text = texts(&item["content"], "text");
            if !text.trim().is_empty() {
                history.push_text(ts, HistoryKind::Message, reference, text);
            }
            return;
        }
        "ContextCompaction" => {
            history.push_text(
                ts,
                HistoryKind::Compaction,
                reference,
                "conversation compacted".into(),
            );
            return;
        }
        "CommandExecution" => (
            "shell".to_string(),
            command_text(&item["command"]),
            item["aggregated_output"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            failed_status || item["exit_code"].as_i64().is_some_and(|c| c != 0),
        ),
        "FileChange" => {
            let paths: Vec<String> = match &item["changes"] {
                Value::Object(map) => map.keys().cloned().collect(),
                Value::Array(list) => list
                    .iter()
                    .filter_map(|c| c["path"].as_str().map(String::from))
                    .collect(),
                _ => Vec::new(),
            };
            (
                "file_change".to_string(),
                paths.join(", "),
                item["status"].as_str().unwrap_or("completed").to_string(),
                failed_status,
            )
        }
        "McpToolCall" => {
            let result = &item["result"];
            let mut output = texts(&result["content"], "text");
            if output.is_empty() && !result.is_null() {
                output = result.to_string();
            }
            (
                format!(
                    "mcp:{}/{}",
                    item["server"].as_str().unwrap_or("?"),
                    item["tool"].as_str().unwrap_or("?")
                ),
                item["arguments"].to_string(),
                output,
                failed_status || result["isError"].as_bool() == Some(true),
            )
        }
        _ => return,
    };
    let full_input = match item["type"].as_str() {
        Some("FileChange") => item["changes"].to_string(),
        _ => input.clone(),
    };
    let i = history.push(ts, HistoryKind::ToolCall, reference);
    history.items[i].tool = Some(HistoryTool {
        name,
        input_summary: summarize(&input, INPUT_SUMMARY),
        status: if failed {
            ToolStatus::Failed
        } else {
            ToolStatus::Completed
        },
        result_summary: Some(summarize(&output, RESULT_SUMMARY)),
        input: Some(cap_tool_text(&full_input)),
        output: Some(cap_tool_text(&output)),
    });
}

/// Context Codex injects as user messages (`<environment_context>`, …).
fn injected_context(text: &str) -> bool {
    let t = text.trim();
    (t.starts_with('<') && t.ends_with('>')) || t.starts_with("# AGENTS.md")
}

fn legacy_items(history: &mut History, lines: &[(usize, Value)]) {
    let mut calls: HashMap<String, usize> = HashMap::new();
    for (n, v) in lines {
        if v["type"] != "response_item" {
            continue;
        }
        let p = &v["payload"];
        let ts = v["timestamp"].as_str().and_then(parse_rfc3339_ms);
        let reference = n.to_string();
        match (p["type"].as_str().unwrap_or_default(), p["role"].as_str()) {
            ("message", Some("user")) => {
                let text = texts(&p["content"], "input_text");
                if !text.trim().is_empty() && !injected_context(&text) {
                    history.push_text(ts, HistoryKind::Prompt, reference, text);
                }
            }
            ("message", Some("assistant")) => {
                let text = texts(&p["content"], "output_text");
                if !text.trim().is_empty() {
                    history.push_text(ts, HistoryKind::Message, reference, text);
                }
            }
            ("function_call" | "custom_tool_call", _) => {
                let name = p["name"].as_str().unwrap_or("tool").to_string();
                let input = p["arguments"]
                    .as_str()
                    .or(p["input"].as_str())
                    .unwrap_or_default()
                    .to_string();
                let i = history.push(ts, HistoryKind::ToolCall, reference);
                history.items[i].tool = Some(HistoryTool {
                    name,
                    input_summary: summarize(&input, INPUT_SUMMARY),
                    status: ToolStatus::Started,
                    result_summary: None,
                    input: Some(cap_tool_text(&input)),
                    output: None,
                });
                if let Some(id) = p["call_id"].as_str() {
                    calls.insert(id.to_string(), i);
                }
            }
            ("function_call_output" | "custom_tool_call_output", _) => {
                let Some(i) = p["call_id"].as_str().and_then(|id| calls.remove(id)) else {
                    continue;
                };
                let output = match &p["output"] {
                    Value::String(s) => s.clone(),
                    other => other["content"]
                        .as_str()
                        .map(String::from)
                        .unwrap_or_else(|| other.to_string()),
                };
                if let Some(tool) = &mut history.items[i].tool {
                    tool.status = ToolStatus::Completed;
                    tool.result_summary = Some(summarize(&output, RESULT_SUMMARY));
                    tool.output = Some(cap_tool_text(&output));
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollout_ids_from_odd_file_names_never_panic() {
        let id = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a01";
        let ok = format!("rollout-2026-10-01T10-00-00-{id}.jsonl");
        assert_eq!(rollout_id(Path::new(&ok)), Some((id.to_string(), false)));
        let zst = format!("rollout-x-{id}.jsonl.zst");
        assert_eq!(rollout_id(Path::new(&zst)), Some((id.to_string(), true)));
        // The 36-byte cut lands inside the two-byte `é`.
        let multibyte = format!("rollout-é{}.jsonl", "a".repeat(35));
        assert_eq!(rollout_id(Path::new(&multibyte)), None);
        let multibyte = format!("rollout-{}é{}.jsonl", "b".repeat(10), "c".repeat(35));
        assert_eq!(rollout_id(Path::new(&multibyte)), None);
        // Shorter than a uuid.
        assert_eq!(rollout_id(Path::new("rollout-x.jsonl")), None);
        assert_eq!(rollout_id(Path::new("rollout-.jsonl.zst")), None);
        assert_eq!(rollout_id(Path::new(&format!("other-{id}.jsonl"))), None);
    }
}
