//! Human-readable output. `--json` bypasses all of this.

use std::fmt::Write as _;

use serde_json::Value;
use wardex_core::external::{ExternalSession, HistoryItem, HistoryKind, HistoryPage, Liveness};
use wardex_core::manager::{Overview, SessionStatus, SessionSummary, SessionView};
use wardex_core::store::QuestionRow;
use wardex_core::time::{clock, human_duration, now_ms};
use wardex_core::{EndReason, Event, SessionState};

fn short_path(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && s.starts_with(&home) => format!("~{}", &s[home.len()..]),
        _ => s,
    }
}

fn opt(s: &Option<String>) -> &str {
    s.as_deref().unwrap_or("-")
}

/// `done (idle_shutdown)`: the state, plus why it ended when that says more.
fn state_with_reason(state: SessionState, reason: Option<EndReason>) -> String {
    match reason.filter(|r| r.as_str() != state.as_str()) {
        Some(r) => format!("{} ({})", state.as_str(), r.as_str()),
        None => state.as_str().to_string(),
    }
}

pub fn list(views: &[SessionView]) -> String {
    if views.is_empty() {
        return "no sessions\n".into();
    }
    let mut out = format!(
        "{:<9} {:<29} {:<7} {:<12} {:<14} {:>6} {:>6}  {}\n",
        "ID", "STATE", "DRIVER", "AGENT", "TASK", "AGE", "ENDED", "CWD"
    );
    let now = now_ms();
    for v in views {
        let s = &v.session;
        let mut state = state_with_reason(s.state, s.ended_reason);
        if v.pending_questions > 0 {
            state = format!("{state}({})", v.pending_questions);
        }
        if s.cleanup_failed {
            state = "cleanup_failed".into();
        }
        let ended = s
            .ended_ms
            .map(|t| human_duration(now - t))
            .unwrap_or_else(|| "-".into());
        let _ = writeln!(
            out,
            "{:<9} {:<29} {:<7} {:<12} {:<14} {:>6} {:>6}  {}",
            s.id,
            state,
            s.driver.as_str(),
            truncate(opt(&s.agent), 12),
            truncate(opt(&s.task), 14),
            human_duration(now - s.created_ms),
            ended,
            short_path(&s.cwd)
        );
    }
    out
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n - 1).collect();
        t.push('…');
        t
    }
}

pub fn status(st: &SessionStatus) -> String {
    let s = &st.session;
    let mut out = String::new();
    let _ = writeln!(out, "session   {}", s.id);
    let _ = writeln!(
        out,
        "state     {}{}",
        state_with_reason(s.state, s.ended_reason),
        if st.live { " (live)" } else { "" }
    );
    let _ = writeln!(
        out,
        "driver    {} (model {})",
        s.driver.as_str(),
        opt(&s.model)
    );
    if let Some(r) = &s.reasoning {
        let _ = writeln!(out, "reasoning {r}");
    }
    let _ = writeln!(out, "agent     {}", opt(&s.agent));
    let _ = writeln!(out, "task      {}", opt(&s.task));
    let _ = writeln!(out, "cwd       {}", short_path(&s.cwd));
    if let Some(w) = &s.worktree {
        let _ = writeln!(out, "worktree  {w} (branch {})", opt(&s.branch));
    }
    if s.readonly {
        let _ = writeln!(out, "mode      read-only");
    }
    let end = s.ended_ms.unwrap_or_else(now_ms);
    let _ = writeln!(out, "age       {}", human_duration(end - s.created_ms));
    let _ = writeln!(out, "resume id {}", opt(&s.driver_session_id));
    if let Some(pid) = s.pid {
        let _ = writeln!(
            out,
            "pid       {pid}{}",
            s.unit
                .as_ref()
                .map(|u| format!(" ({u})"))
                .unwrap_or_default()
        );
    }
    if let Some(rss) = st.rss_bytes {
        let _ = writeln!(
            out,
            "resources {:.0} MiB, {} cpu",
            rss as f64 / 1_048_576.0,
            st.cpu_percent
                .map(|c| format!("{c:.1}%"))
                .unwrap_or_else(|| "-".into())
        );
    }
    if let Some(last) = st.last_activity_ms {
        let _ = writeln!(out, "activity  {} ago", human_duration(now_ms() - last));
    }
    if !st.running_tools.is_empty() {
        let _ = writeln!(out, "running   {}", st.running_tools.join(", "));
    }
    for p in &st.pending {
        let _ = writeln!(
            out,
            "pending   {} ({}, {} ago): {}",
            p.question_id,
            p.kind,
            human_duration(now_ms() - p.asked_ms),
            p.request.describe()
        );
    }
    if let Some(r) = &s.last_result {
        let _ = writeln!(out, "result    {}", wardex_core::driver::summarize(r, 300));
    }
    if let Some(e) = &s.last_error {
        let _ = writeln!(out, "error     {e}");
    }
    if s.cleanup_failed {
        let _ = writeln!(
            out,
            "warning   cleanup_failed; old scope {}; use wardex resume {} to retry scope cleanup",
            opt(&s.unit),
            s.id
        );
    }
    out
}

pub fn event(e: &Event) -> String {
    format!(
        "[{:>4}] {} {:<9} {}",
        e.seq,
        clock(e.ts_ms),
        e.kind.name(),
        e.kind.describe()
    )
}

pub fn summary(s: &SessionSummary) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "session   {} ({}, {})",
        s.id,
        s.driver.as_str(),
        s.state.as_str()
    );
    let _ = writeln!(out, "agent     {} / task {}", opt(&s.agent), opt(&s.task));
    let _ = writeln!(out, "cwd       {}", short_path(&s.cwd));
    let mut duration = human_duration(s.duration_ms);
    if let Some(t) = s.last_turn_ms {
        duration.push_str(&format!(" (last turn {})", human_duration(t)));
    }
    let _ = writeln!(out, "duration  {duration}");
    let _ = writeln!(out, "turns     {} (prompts {})", s.turns, s.prompts);
    if !s.tool_counts.is_empty() {
        let tools: Vec<String> = s
            .tool_counts
            .iter()
            .map(|(k, v)| format!("{k}×{v}"))
            .collect();
        let _ = writeln!(out, "tools     {}", tools.join(", "));
    }
    if let Some(g) = &s.git {
        let _ = writeln!(
            out,
            "git       branch {}, {} changed file(s)",
            g.branch.as_deref().unwrap_or("-"),
            g.changed_files.len()
        );
        for f in g.changed_files.iter().take(20) {
            let _ = writeln!(out, "            {f}");
        }
    }
    if let Some(n) = &s.last_note {
        let _ = writeln!(out, "note      {n}");
    }
    if let Some(r) = &s.last_result {
        let _ = writeln!(out, "result    {r}");
    }
    if let Some(e) = &s.last_error {
        let _ = writeln!(out, "error     {e}");
    }
    out
}

pub fn pending(rows: &[QuestionRow]) -> String {
    if rows.is_empty() {
        return "no pending questions\n".into();
    }
    let mut out = String::new();
    for q in rows {
        let request: Option<wardex_core::EventKind> = serde_json::from_value(q.body.clone()).ok();
        let _ = writeln!(
            out,
            "{} {} ({}, {} ago): {}",
            q.session_id,
            q.question_id,
            q.kind,
            human_duration(now_ms() - q.asked_ms),
            request.map(|r| r.describe()).unwrap_or_default()
        );
    }
    out
}

pub fn overview(o: &Overview) -> String {
    let mut out = String::new();
    let counts: Vec<String> = o.counts.iter().map(|(k, v)| format!("{v} {k}")).collect();
    let _ = writeln!(
        out,
        "{} live ({}), {} pending question(s)",
        o.live.len(),
        if counts.is_empty() {
            "-".into()
        } else {
            counts.join(", ")
        },
        o.pending_questions
    );
    for s in &o.live {
        let mut res = String::new();
        if let Some(rss) = s.rss_bytes {
            let _ = write!(res, " {:.0}MB", rss as f64 / 1_048_576.0);
        }
        if let Some(cpu) = s.cpu_percent {
            let _ = write!(res, " {cpu:.0}%cpu");
        }
        let _ = writeln!(
            out,
            "\n{} {} {} agent={} task={} age={} quiet={}{}{}  {}",
            s.id,
            s.state.as_str(),
            s.driver.as_str(),
            opt(&s.agent),
            opt(&s.task),
            human_duration(s.duration_ms),
            human_duration(o.now_ms - s.last_activity_ms),
            match (s.idle_since_ms, s.shutdown_in_ms) {
                (Some(since), Some(left)) => format!(
                    " idle={} closes_in={}",
                    human_duration(o.now_ms - since),
                    human_duration(left)
                ),
                (Some(since), None) => format!(" idle={}", human_duration(o.now_ms - since)),
                _ => String::new(),
            },
            res,
            short_path(&s.cwd)
        );
        if !s.running_tools.is_empty() {
            let _ = writeln!(out, "  tools: {}", s.running_tools.join(", "));
        }
        for p in &s.pending {
            let _ = writeln!(
                out,
                "  ? {} ({}): {}",
                p.question_id,
                p.kind,
                p.request.describe()
            );
        }
        for e in &s.latest {
            let _ = writeln!(out, "  {} #{} {}", clock(e.ts_ms), e.seq, e.text);
        }
    }
    if !o.recently_ended.is_empty() {
        let _ = writeln!(out, "\nended in the last 24h:");
        for s in &o.recently_ended {
            let ago = s
                .ended_ms
                .map(|t| human_duration(o.now_ms - t))
                .unwrap_or_else(|| "-".into());
            let text = s.last_error.as_ref().or(s.last_result.as_ref());
            let _ = writeln!(
                out,
                "{} {} {} agent={} task={} {} ago: {}",
                s.id,
                state_with_reason(s.state, s.ended_reason),
                s.driver.as_str(),
                opt(&s.agent),
                opt(&s.task),
                ago,
                truncate(text.map(String::as_str).unwrap_or("-"), 100).replace('\n', " ")
            );
        }
    }
    out
}

fn liveness(l: &Liveness) -> String {
    match l {
        Liveness::Live { pid, .. } => format!("live({pid})"),
        other => other.as_str().to_string(),
    }
}

pub fn external_list(sessions: &[ExternalSession]) -> String {
    if sessions.is_empty() {
        return "no external sessions\n".into();
    }
    let mut out = format!(
        "{:<9} {:<7} {:<16} {:>7}  {:<30} {}\n",
        "ID", "DRIVER", "STATE", "UPDATED", "CWD", "TITLE"
    );
    let now = now_ms();
    for s in sessions {
        let state = match &s.managed_by {
            Some(id) => format!("managed:{id}"),
            None => liveness(&s.liveness),
        };
        let mut title = s.title.clone().unwrap_or_else(|| "-".into());
        if !s.warnings.is_empty() {
            title.push_str(&format!(" [{}]", s.warnings.join("; ")));
        }
        let _ = writeln!(
            out,
            "{:<9} {:<7} {:<16} {:>7}  {:<30} {}",
            s.native_id.chars().take(8).collect::<String>(),
            s.driver.as_str(),
            state,
            human_duration(now - s.updated_ms),
            truncate(&short_path(&s.cwd), 30),
            truncate(&title, 80)
        );
    }
    out
}

fn history_header(page: &HistoryPage) -> String {
    let s = &page.session;
    let mut out = format!(
        "{} {} {}",
        s.qualified_id(),
        liveness(&s.liveness),
        short_path(&s.cwd)
    );
    if let Some(t) = &s.title {
        out.push_str(&format!(" \u{2014} {t}"));
    }
    out
}

fn tool_line(item: &HistoryItem) -> Option<String> {
    let t = item.tool.as_ref()?;
    let mut line = format!("{}: {}", t.name, t.input_summary);
    let status = match t.status {
        wardex_core::ToolStatus::Started => "no result",
        wardex_core::ToolStatus::Completed => "completed",
        wardex_core::ToolStatus::Failed => "failed",
    };
    line.push_str(&format!(" -> {status}"));
    if let Some(r) = t.result_summary.as_deref().filter(|r| !r.is_empty()) {
        line.push_str(&format!(": {r}"));
    }
    Some(line)
}

fn history_footer(page: &HistoryPage, out: &mut String) {
    if page.truncated {
        let _ = writeln!(out, "(cut to the byte budget)");
    }
    if let Some(before) = &page.next_before {
        let _ = writeln!(out, "(older turns: --before {before})");
    }
}

pub fn history_text(page: &HistoryPage) -> String {
    let mut out = format!("{}\n", history_header(page));
    for w in &page.warnings {
        let _ = writeln!(out, "warning: {w}");
    }
    let mut turn = None;
    for item in &page.items {
        if turn != Some(item.turn) {
            turn = Some(item.turn);
            let _ = writeln!(out, "--- turn {}", item.turn);
        }
        let text = item.text.as_deref().unwrap_or_default();
        match item.kind {
            HistoryKind::Prompt => {
                let _ = writeln!(out, "> {}", text.replace('\n', "\n  "));
            }
            HistoryKind::Message => {
                let _ = writeln!(out, "{text}");
            }
            HistoryKind::Compaction => {
                let _ = writeln!(out, "[{text}]");
            }
            HistoryKind::ToolCall => {
                if let Some(line) = tool_line(item) {
                    let _ = writeln!(out, "  $ {line}");
                }
                if let Some(t) = &item.tool {
                    for (label, body) in [("input", &t.input), ("output", &t.output)] {
                        if let Some(body) = body {
                            let _ = writeln!(out, "    --- {label}");
                            for l in body.lines() {
                                let _ = writeln!(out, "    {l}");
                            }
                        }
                    }
                }
            }
        }
    }
    history_footer(page, &mut out);
    out
}

/// A context summary another agent can take as a prompt.
pub fn history_markdown(page: &HistoryPage) -> String {
    let s = &page.session;
    let mut out = format!(
        "# Conversation context: {} session {}\n\n",
        s.driver.as_str(),
        s.native_id
    );
    let _ = writeln!(out, "- Working directory: `{}`", s.cwd.display());
    if let Some(t) = &s.title {
        let _ = writeln!(out, "- Title: {t}");
    }
    if let (Some(first), Some(last)) = (page.items.first(), page.items.last()) {
        let _ = writeln!(out, "- Turns shown: {} to {}", first.turn, last.turn);
    }
    if page.next_before.is_some() {
        let _ = writeln!(out, "- Earlier turns are omitted.");
    }
    if !page.warnings.is_empty() {
        let _ = writeln!(out, "- Warnings: {}", page.warnings.join(", "));
    }
    let mut turn = None;
    for item in &page.items {
        if turn != Some(item.turn) {
            turn = Some(item.turn);
            let _ = write!(out, "\n## Turn {}\n", item.turn);
        }
        let text = item.text.as_deref().unwrap_or_default();
        match item.kind {
            HistoryKind::Prompt => {
                let _ = write!(out, "\n**User:** {text}\n");
            }
            HistoryKind::Message => {
                let _ = write!(out, "\n**Assistant:** {text}\n");
            }
            HistoryKind::Compaction => {
                let _ = write!(out, "\n_(The conversation was compacted here.)_\n");
            }
            HistoryKind::ToolCall => {
                if let Some(line) = tool_line(item) {
                    let _ = write!(out, "\n- Tool {line}\n");
                }
            }
        }
    }
    out
}

pub fn json(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}
