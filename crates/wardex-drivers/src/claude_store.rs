//! Claude Code transcript store: `<root>/projects/<encoded-cwd>/<uuid>.jsonl`.
//! Read only. Format notes: `docs/external-sessions.md`.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
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

use crate::claude::{content_text, input_summary};

const RESULT_SUMMARY: usize = 300;
/// The header (first `cwd`, version, entrypoint) is in the first lines;
/// give up after this much.
const HEADER_LINES: usize = 200;
const HEADER_BYTES: u64 = 1024 * 1024;
/// Titles and `continued-in` are appended near the end.
const TAIL_BYTES: u64 = 64 * 1024;

type Cache = HashMap<PathBuf, (SystemTime, u64, ExternalSession)>;

pub struct ClaudeStore {
    root: PathBuf,
    max_file_bytes: u64,
    cache: Mutex<Cache>,
}

impl ClaudeStore {
    pub fn new(root: PathBuf, max_file_bytes: u64) -> Self {
        Self {
            root,
            max_file_bytes,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// `<root>/projects/*/<uuid>.jsonl`, without symlinked directories and
    /// without the per-session side directories (subagents, tool results).
    fn transcripts(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(projects) = std::fs::read_dir(self.root.join("projects")) else {
            return out;
        };
        for project in projects.flatten() {
            if !project.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let Ok(files) = std::fs::read_dir(project.path()) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                let id_ok = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(is_uuid);
                if id_ok
                    && path.extension().is_some_and(|e| e == "jsonl")
                    && file.file_type().is_ok_and(|t| t.is_file())
                {
                    out.push(path);
                }
            }
        }
        out
    }

    /// The session behind a transcript, from the cache while the file is
    /// unchanged. `None` for files that hold no conversation.
    fn session(&self, path: &Path) -> Result<Option<ExternalSession>> {
        let meta = check_transcript(&self.root, path, self.max_file_bytes)?;
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((t, len, s)) = cache.get(path)
            && *t == mtime
            && *len == meta.len()
        {
            return Ok(Some(s.clone()));
        }
        drop(cache);
        let Some(session) = self.read_session(path, &meta, mtime)? else {
            return Ok(None);
        };
        cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.insert(path.to_path_buf(), (mtime, meta.len(), session.clone()));
        Ok(Some(session))
    }

    fn io(&self, path: &Path) -> impl Fn(std::io::Error) -> ExternalError {
        let hint = relative_hint(&self.root, path);
        move |source| ExternalError::Io {
            path: hint.clone(),
            source,
        }
    }

    fn read_session(
        &self,
        path: &Path,
        meta: &std::fs::Metadata,
        mtime: SystemTime,
    ) -> Result<Option<ExternalSession>> {
        let native_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let mut file = std::fs::File::open(path).map_err(self.io(path))?;
        let (mut cwd, mut version, mut entrypoint, mut created) = (None, None, None, None);
        let reader = BufReader::new((&mut file).take(HEADER_BYTES));
        for line in reader.lines().take(HEADER_LINES) {
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if created.is_none() {
                created = v["timestamp"].as_str().and_then(parse_rfc3339_ms);
            }
            if cwd.is_none() {
                cwd = v["cwd"].as_str().map(PathBuf::from);
            }
            if version.is_none() {
                version = v["version"].as_str().map(String::from);
            }
            if entrypoint.is_none() {
                entrypoint = v["entrypoint"].as_str().map(String::from);
            }
            if cwd.is_some() && version.is_some() && entrypoint.is_some() {
                break;
            }
        }
        let Some(cwd) = cwd else {
            return Ok(None);
        };
        // Titles and hand-overs are appended; the last one wins.
        let start = meta.len().saturating_sub(TAIL_BYTES);
        file.seek(SeekFrom::Start(start)).map_err(self.io(path))?;
        let mut tail = Vec::new();
        file.take(TAIL_BYTES)
            .read_to_end(&mut tail)
            .map_err(self.io(path))?;
        let tail = String::from_utf8_lossy(&tail);
        let (mut custom, mut ai, mut continued_in) = (None, None, None);
        for line in tail.lines().skip(usize::from(start > 0)) {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            match v["type"].as_str().unwrap_or_default() {
                "custom-title" => custom = v["customTitle"].as_str().map(String::from),
                "ai-title" => ai = v["aiTitle"].as_str().map(String::from),
                "continued-in" => {
                    continued_in = v["continuedInSessionId"].as_str().map(String::from)
                }
                _ => {}
            }
        }
        let mut warnings = Vec::new();
        if version
            .as_deref()
            .is_some_and(|v| version_untested(DriverKind::Claude, v))
        {
            warnings.push(FORMAT_VERSION_UNTESTED.to_string());
        }
        if let Some(next) = &continued_in {
            warnings.push(format!("continued_in {next}"));
        }
        Ok(Some(ExternalSession {
            driver: DriverKind::Claude,
            native_id,
            cwd,
            title: custom.or(ai).filter(|t| !t.trim().is_empty()),
            created_ms: created,
            updated_ms: system_ms(mtime),
            cli_version: version,
            entrypoint,
            forked_from: None,
            continued_in,
            size_bytes: meta.len(),
            compressed: false,
            archived: false,
            subagent: false,
            managed_by: None,
            liveness: Liveness::not_probed(),
            path_hint: relative_hint(&self.root, path),
            warnings,
            path: path.to_path_buf(),
        }))
    }
}

pub(crate) fn system_ms(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl ExternalSource for ClaudeStore {
    fn kind(&self) -> DriverKind {
        DriverKind::Claude
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn discover(&self, filter: &ExternalFilter) -> Result<Vec<ExternalSession>> {
        let mut out = Vec::new();
        for path in self.transcripts() {
            // Cheap time filter before reading anything.
            if let Some(since) = filter.since_ms
                && std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| system_ms(t) < since)
            {
                continue;
            }
            match self.session(&path) {
                Ok(Some(s)) if filter.admits(&s) => out.push(s),
                Ok(_) => {}
                Err(error) => tracing::debug!(%error, "skipping claude transcript"),
            }
        }
        Ok(out)
    }

    fn find(&self, prefix: &str) -> Result<Vec<ExternalSession>> {
        let mut out = Vec::new();
        for path in self.transcripts() {
            let matches = path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|id| id.starts_with(prefix));
            if matches && let Some(s) = self.session(&path)? {
                out.push(s);
            }
        }
        Ok(out)
    }

    fn read_history(&self, session: &ExternalSession) -> Result<History> {
        check_transcript(&self.root, &session.path, self.max_file_bytes)?;
        let text = std::fs::read(&session.path).map_err(self.io(&session.path))?;
        Ok(parse_history(&String::from_utf8_lossy(&text)))
    }
}

struct Node {
    value: Value,
    parent: Option<String>,
    sidechain: bool,
}

/// The active branch of a transcript as normalized history.
pub fn parse_history(text: &str) -> History {
    let mut history = History::default();
    let complete = text.ends_with('\n');
    let lines: Vec<&str> = text.lines().collect();
    let mut nodes: Vec<Node> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut children: HashMap<String, Vec<usize>> = HashMap::new();
    let mut leaf_marker: Option<String> = None;
    let mut malformed = 0;
    for (n, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) if n + 1 == lines.len() && !complete => {
                history.warn("partial_last_line");
                continue;
            }
            Err(_) => {
                malformed += 1;
                continue;
            }
        };
        if value["type"] == "last-prompt"
            && let Some(leaf) = value["leafUuid"].as_str()
        {
            leaf_marker = Some(leaf.to_string());
        }
        let Some(uuid) = value["uuid"].as_str().map(String::from) else {
            continue;
        };
        // A compaction boundary starts a new chain; its logical parent
        // continues the conversation.
        let parent = value["parentUuid"]
            .as_str()
            .or(value["logicalParentUuid"].as_str())
            .map(String::from);
        let sidechain = value["isSidechain"].as_bool().unwrap_or(false);
        if let Some(p) = &parent {
            children.entry(p.clone()).or_default().push(nodes.len());
        }
        index.insert(uuid, nodes.len());
        nodes.push(Node {
            value,
            parent,
            sidechain,
        });
    }
    if malformed > 0 {
        history.warn(format!("skipped {malformed} malformed lines"));
    }
    let start = match leaf_marker.as_ref().and_then(|l| index.get(l)) {
        Some(&i) => Some(i),
        None => {
            history.warn("no_leaf_marker");
            nodes.iter().rposition(|n| {
                !n.sidechain && matches!(n.value["type"].as_str(), Some("user" | "assistant"))
            })
        }
    };
    let Some(mut leaf) = start else {
        return history;
    };
    // The newest line in each node's subtree (children follow their parent
    // in the file).
    let mut latest: Vec<usize> = (0..nodes.len()).collect();
    for i in (0..nodes.len()).rev() {
        let uuid = nodes[i].value["uuid"].as_str().unwrap_or_default();
        if let Some(kids) = children.get(uuid) {
            for &k in kids {
                if k > i && !nodes[k].sidechain {
                    latest[i] = latest[i].max(latest[k]);
                }
            }
        }
    }
    // Follow the child with the newest activity down to the real leaf: the
    // marker may point at the last prompt, before the replies. Not simply
    // the last child: a parallel tool call's result hangs off its own call
    // and ends there, while the conversation goes on from the last call.
    for _ in 0..nodes.len() {
        let uuid = nodes[leaf].value["uuid"].as_str().unwrap_or_default();
        match children.get(uuid).and_then(|kids| {
            kids.iter()
                .filter(|&&k| k > leaf && !nodes[k].sidechain)
                .max_by_key(|&&k| latest[k])
        }) {
            Some(&next) => leaf = next,
            None => break,
        }
    }
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = Some(leaf);
    while let Some(i) = cursor {
        if !seen.insert(i) {
            history.warn("cycle in parent chain");
            break;
        }
        chain.push(i);
        cursor = nodes[i].parent.as_ref().and_then(|p| index.get(p)).copied();
    }
    chain.reverse();

    // Results of parallel tool calls hang off their own `tool_use` line, so
    // all but the last are off the active branch: match them by id.
    let mut results: HashMap<&str, &Value> = HashMap::new();
    for node in nodes
        .iter()
        .filter(|n| !n.sidechain && n.value["type"] == "user")
    {
        for block in node.value["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if block["type"] == "tool_result"
                && let Some(id) = block["tool_use_id"].as_str()
            {
                results.entry(id).or_insert(block);
            }
        }
    }

    let mut version = None;
    for i in chain {
        let node = &nodes[i];
        let v = &node.value;
        if let Some(ver) = v["version"].as_str() {
            version = Some(ver.to_string());
        }
        if node.sidechain {
            continue;
        }
        let ts = v["timestamp"].as_str().and_then(parse_rfc3339_ms);
        let reference = v["uuid"].as_str().unwrap_or_default().to_string();
        match v["type"].as_str().unwrap_or_default() {
            "user" => {
                if v["isMeta"].as_bool() == Some(true)
                    || v["isCompactSummary"].as_bool() == Some(true)
                {
                    continue;
                }
                let content = &v["message"]["content"];
                let mut texts = Vec::new();
                match content {
                    Value::String(s) => texts.push(s.clone()),
                    // Tool results are matched with their tool_use below.
                    Value::Array(blocks) => texts.extend(
                        blocks
                            .iter()
                            .filter(|b| b["type"] == "text")
                            .map(|b| b["text"].as_str().unwrap_or_default().to_string()),
                    ),
                    _ => {}
                }
                let text = texts.join("\n");
                if !text.trim().is_empty() {
                    history.push_text(ts, HistoryKind::Prompt, reference, text);
                }
            }
            "assistant" => {
                for block in v["message"]["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str().unwrap_or_default() {
                        "text" => {
                            let text = block["text"].as_str().unwrap_or_default();
                            if !text.trim().is_empty() {
                                history.push_text(
                                    ts,
                                    HistoryKind::Message,
                                    reference.clone(),
                                    text.to_string(),
                                );
                            }
                        }
                        "tool_use" => {
                            let name = block["name"].as_str().unwrap_or_default().to_string();
                            let input = &block["input"];
                            let result = block["id"].as_str().and_then(|id| results.get(id));
                            let output = result.map(|r| content_text(&r["content"]));
                            let status = match result {
                                None => ToolStatus::Started,
                                Some(r) if r["is_error"].as_bool() == Some(true) => {
                                    ToolStatus::Failed
                                }
                                Some(_) => ToolStatus::Completed,
                            };
                            let item = history.push(ts, HistoryKind::ToolCall, reference.clone());
                            history.items[item].tool = Some(HistoryTool {
                                input_summary: input_summary(&name, input),
                                name,
                                status,
                                result_summary: output
                                    .as_deref()
                                    .map(|o| summarize(o, RESULT_SUMMARY)),
                                input: Some(cap_tool_text(&input.to_string())),
                                output: output.as_deref().map(cap_tool_text),
                            });
                        }
                        _ => {}
                    }
                }
            }
            "system" if v["subtype"] == "compact_boundary" => {
                history.push_text(
                    ts,
                    HistoryKind::Compaction,
                    reference,
                    "conversation compacted".into(),
                );
            }
            _ => {}
        }
    }
    if version
        .as_deref()
        .is_some_and(|v| version_untested(DriverKind::Claude, v))
    {
        history.warn(FORMAT_VERSION_UNTESTED);
    }
    history
}
