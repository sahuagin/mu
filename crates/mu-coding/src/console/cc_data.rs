//! mu-cc-sessions-console-lqqt.1: scanner seam for Claude Code sessions.
//!
//! The mu console's native index reads mu's own event-log JSONL
//! (`console::data::scan_sessions`). This module is its sibling for the
//! *other* fleet: claude-code's session transcripts under
//! `~/.claude-personal/projects/<project-dir>/<session-uuid>.jsonl`. It
//! projects those wrapper-shaped logs into the same [`SessionSummary`]
//! rows so both corpora render in one table — "one pane of glass."
//!
//! The map of the cc wrapper schema is the proven DuckDB decode at
//! `~/src/claude-personal/scripts/t4c_tool_inventory.sql`
//! (`read_ndjson_objects` + `message.content[*]` unnest); this is its
//! Rust translation. Real transcripts are messier than that decode
//! lets on — each file mixes `assistant`/`user` message envelopes with
//! `attachment`/`queue-operation`/`last-prompt`/`summary` metadata
//! envelopes. We process only the two message types and skip the rest.
//!
//! INVARIANT (cc transcript ownership): this module is strictly
//! READ-ONLY over `~/.claude-personal`. It opens files for reading and
//! never creates, writes, or appends. Marks for cc sessions live in a
//! task_log sidecar (sibling bead .3), never in the transcript itself.
//!
//! Parsing is defensive: a transcript with an unknown shape must
//! skip-and-count, never panic. A line that fails to parse marks its
//! file malformed; a file or dir entry that can't be read bumps the
//! skipped-entries counter; neither aborts the scan.
//!
//! mu-y5hz (cc-schema edge cases) — applied consistently to BOTH
//! projections in this module (the index scanner [`scan_cc_sessions`] and
//! the detail-view reader [`read_cc_transcript`]):
//!
//! - **Sidechain (subagent) turns.** cc tags every envelope with a
//!   top-level `isSidechain` bool; subagent turns carry `true`. They are
//!   the agent loop's nested work, not the parent operator's, so they are
//!   EXCLUDED from every parent-session rollup — the index counts/usage
//!   (`scan_cc_sessions`) AND the cost-tab summed total (`read_cc_transcript`
//!   → `render_cc_cost`) — and surfaced via a `sidechain_entries` counter
//!   rather than silently dropped. The detail transcript still RENDERS
//!   each sidechain line (flagged, not vanished). (Terrain note,
//!   2026-06-06: on the current cc format these turns live in a
//!   `subagents/agent-*.jsonl` subdirectory that the scanner does not
//!   descend into, so the inline exclusion is defensive — it guards
//!   older inline-format transcripts and any future re-inlining, and is
//!   a no-op on today's main session files.)
//! - **Model switching.** A session can switch models mid-run. The index
//!   keeps last-model-wins for the `model` column (the most recent
//!   *non-sidechain* assistant model), and ALSO reports the count of
//!   distinct models via `SessionSummary::models_seen` so a switch isn't
//!   invisible.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use mu_core::agent::Usage;

use super::data::{ScanResult, SessionSummary};
use super::time::parse_rfc3339_ms;

/// The default claude-code projects root. `None` if the home dir can't
/// be resolved (mirrors [`crate::serve::default_events_dir`]). Opt-in:
/// the console only scans cc sessions when explicitly asked.
pub fn default_cc_projects_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude-personal/projects"))
}

/// Scan `projects_dir` for claude-code session transcripts and project
/// each into a [`SessionSummary`] with `provider = "claude-code"` and
/// `daemon_id` carrying the project-dir name. Best-effort: returns
/// whatever it could read, counting malformed files and skipped
/// entries rather than failing.
pub(crate) fn scan_cc_sessions(projects_dir: &Path) -> ScanResult {
    let mut result = ScanResult::default();
    let Ok(projects) = std::fs::read_dir(projects_dir) else {
        // No projects dir (or unreadable) — nothing to add, not an error.
        return result;
    };
    for project in projects.flatten() {
        let Ok(ft) = project.file_type() else {
            result.skipped_entries += 1;
            continue;
        };
        if !ft.is_dir() {
            continue;
        }
        let project_dir = project.file_name().to_string_lossy().to_string();
        let Ok(files) = std::fs::read_dir(project.path()) else {
            result.skipped_entries += 1;
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            // The cc session id IS the filename stem (a UUID).
            let session_id = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => {
                    result.skipped_entries += 1;
                    continue;
                }
            };
            match scan_one_file(&path, &project_dir, &session_id) {
                Ok(Some((summary, malformed_lines))) => {
                    if malformed_lines > 0 {
                        result.malformed_files += 1;
                    }
                    result.sessions.push(summary);
                }
                // File parsed but held no message turns (pure metadata):
                // not a session, silently ignored.
                Ok(None) => {}
                // Couldn't read the file at all.
                Err(_) => result.malformed_files += 1,
            }
        }
    }
    result
        .sessions
        .sort_by_key(|s| std::cmp::Reverse(s.last_activity_unix_ms.unwrap_or(0)));
    result
}

/// Fold one transcript file into a [`SessionSummary`]. Returns
/// `Ok(None)` when the file is readable but contains no `user`/
/// `assistant` message envelopes (so it isn't a real session). The
/// `usize` is the count of lines that failed to parse as JSON.
fn scan_one_file(
    path: &Path,
    project_dir: &str,
    session_id: &str,
) -> std::io::Result<Option<(SessionSummary, usize)>> {
    let text = std::fs::read_to_string(path)?;
    let mut acc = CcAccumulator::default();
    let mut malformed_lines = 0usize;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) => acc.ingest(&v),
            Err(_) => malformed_lines += 1,
        }
    }
    if !acc.saw_message {
        return Ok(None);
    }
    Ok(Some((
        acc.into_summary(project_dir, session_id),
        malformed_lines,
    )))
}

/// Running fold over one transcript's lines.
#[derive(Default)]
struct CcAccumulator {
    saw_message: bool,
    /// Last non-sidechain assistant model seen (last-model-wins column).
    model: Option<String>,
    /// Distinct non-sidechain assistant models seen across the session.
    /// `model` above keeps the last for the column; this captures how
    /// many appeared so a mid-run model switch isn't invisible (mu-y5hz).
    models: BTreeSet<String>,
    last_activity_unix_ms: Option<u64>,
    ask_count: u32,
    assistant_count: u32,
    tool_call_count: u32,
    usage: Option<Usage>,
    /// mu-y5hz: subagent (isSidechain:true) message turns excluded from
    /// the rollups above, surfaced rather than silently dropped.
    sidechain_entries: u32,
}

impl CcAccumulator {
    fn ingest(&mut self, v: &serde_json::Value) {
        // Every envelope can carry a timestamp; track the latest across
        // all types — including sidechain turns — so sort order reflects
        // real session liveness. last_activity is a max, not a sum, so a
        // subagent turn can't inflate it the way it would a rollup count.
        if let Some(ts) = v.get("timestamp").and_then(|t| t.as_str()) {
            if let Some(ms) = parse_rfc3339_ms(ts) {
                self.last_activity_unix_ms =
                    Some(self.last_activity_unix_ms.map_or(ms, |cur| cur.max(ms)));
            }
        }
        // mu-y5hz policy (a): subagent (isSidechain:true) message turns
        // are the agent loop's nested work, not the parent operator's, so
        // they're EXCLUDED from this session's ask/assistant/tool/usage
        // rollups (and from model detection) and counted separately. Only
        // user/assistant sidechain envelopes are tallied — metadata
        // envelopes never count toward rollups regardless of sidechain.
        // A pure-sidechain file therefore never sets `saw_message` and is
        // not projected as a session (correct: subagent transcripts are
        // not standalone operator sessions).
        if is_sidechain(v) {
            if matches!(
                v.get("type").and_then(|t| t.as_str()),
                Some("user") | Some("assistant")
            ) {
                self.sidechain_entries = self.sidechain_entries.saturating_add(1);
            }
            return;
        }
        match v.get("type").and_then(|t| t.as_str()) {
            Some("user") => self.ingest_user(v),
            Some("assistant") => self.ingest_assistant(v),
            // attachment / queue-operation / last-prompt / summary / …:
            // not message turns — deliberately ignored, not malformed.
            _ => {}
        }
    }

    fn ingest_user(&mut self, v: &serde_json::Value) {
        let content = v.get("message").and_then(|m| m.get("content"));
        // A user envelope is a real "ask" when its content is a bare
        // string (the typed prompt) or a block-array with at least one
        // non-`tool_result` block. Tool-result-only user envelopes are
        // the agent loop feeding results back, not operator asks.
        let is_ask = match content {
            Some(serde_json::Value::String(_)) => true,
            Some(serde_json::Value::Array(blocks)) => blocks
                .iter()
                .any(|b| b.get("type").and_then(|t| t.as_str()) != Some("tool_result")),
            _ => false,
        };
        if is_ask {
            self.saw_message = true;
            self.ask_count = self.ask_count.saturating_add(1);
        } else if content.is_some() {
            // Tool-result envelope still counts as a real session turn.
            self.saw_message = true;
        }
    }

    fn ingest_assistant(&mut self, v: &serde_json::Value) {
        self.saw_message = true;
        self.assistant_count = self.assistant_count.saturating_add(1);
        let Some(message) = v.get("message") else {
            return;
        };
        if let Some(model) = message.get("model").and_then(|m| m.as_str()) {
            if !model.is_empty() {
                // last-model-wins for the index column …
                self.model = Some(model.to_string());
                // … and record distinctness for models_seen (mu-y5hz).
                self.models.insert(model.to_string());
            }
        }
        if let Some(blocks) = message.get("content").and_then(|c| c.as_array()) {
            for b in blocks {
                if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    self.tool_call_count = self.tool_call_count.saturating_add(1);
                }
            }
        }
        if let Some(u) = message.get("usage") {
            let turn = parse_usage(u);
            self.usage = Some(self.usage.map_or(turn, |cur| cur + turn));
        }
    }

    fn into_summary(self, project_dir: &str, session_id: &str) -> SessionSummary {
        SessionSummary {
            daemon_id: project_dir.to_string(),
            session_id: session_id.to_string(),
            provider: Some("claude-code".to_string()),
            model: self.model,
            // Distinct assistant models, saturated into the u8 surface.
            // 0 when no assistant turn carried a model (last-wins `model`
            // is then None too); ≥2 flags a mid-run model switch.
            models_seen: u8::try_from(self.models.len()).unwrap_or(u8::MAX),
            last_activity_unix_ms: self.last_activity_unix_ms,
            ask_count: self.ask_count,
            // cc has no ContextAssembly event; an assistant turn is the
            // closest analog of a model call, so it fills that column.
            context_assembly_count: self.assistant_count,
            tool_call_count: self.tool_call_count,
            // Per-turn usage summed across the session — the cc analog of
            // mu's cumulative_usage(). cache_read inflates across turns
            // (re-read each call), same as the native cost view's sum.
            usage: self.usage,
            // Marks for cc sessions live in a task_log sidecar (bead .3);
            // we never read OperatorMark events out of cc transcripts.
            mark: None,
            // mu-y5hz: subagent turns excluded from the rollups above.
            sidechain_entries: self.sidechain_entries,
        }
    }
}

/// mu-y5hz: is this envelope a subagent (sidechain) turn? cc tags every
/// envelope with a top-level `isSidechain` bool; a missing/false field
/// means a parent-session turn. Shared by the index scanner and the
/// detail-view reader so both treat sidechain identically.
fn is_sidechain(v: &serde_json::Value) -> bool {
    v.get("isSidechain")
        .and_then(|s| s.as_bool())
        .unwrap_or(false)
}

/// Project one cc `message.usage` object into [`Usage`]. Missing fields
/// default to 0/None; the nested `cache_creation.ephemeral_{5m,1h}`
/// breakdown maps onto the 5m/1h tier columns when present.
fn parse_usage(u: &serde_json::Value) -> Usage {
    let u64_of = |key: &str| u.get(key).and_then(|x| x.as_u64());
    let creation = u.get("cache_creation");
    let tier = |key: &str| creation.and_then(|c| c.get(key)).and_then(|x| x.as_u64());
    Usage {
        input_tokens: u64_of("input_tokens").unwrap_or(0),
        output_tokens: u64_of("output_tokens").unwrap_or(0),
        cache_read_input_tokens: u64_of("cache_read_input_tokens"),
        cache_creation_input_tokens: u64_of("cache_creation_input_tokens"),
        cache_creation_5m_input_tokens: tier("ephemeral_5m_input_tokens"),
        cache_creation_1h_input_tokens: tier("ephemeral_1h_input_tokens"),
        reasoning_tokens: u64_of("reasoning_tokens"),
    }
}

// ── mu-cc-sessions-console-lqqt.2: full-transcript reader ──────────────
//
// Where `scan_cc_sessions` folds a whole file down to one index row, the
// detail view needs every line back as a renderable entry. The reader
// stays defensive to the same standard as the scanner: a line that fails
// to parse is counted, never fatal; an envelope shape we don't recognize
// degrades to a `Meta` entry whose raw JSON is preserved for drilldown.
// It is strictly READ-ONLY over `~/.claude-personal` — it opens the file
// for reading and never writes.

/// Which transcript lane a cc envelope renders into. Maps onto the same
/// `role-{user,assistant,tool}` CSS the native transcript uses; `Meta`
/// is the catch-all for non-message envelopes (summary/attachment/…) and
/// unknown shapes, so unrecognized lines stay visible rather than vanish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CcRole {
    User,
    Assistant,
    Tool,
    Meta,
}

impl CcRole {
    /// The CSS role token (the native transcript styles `user`,
    /// `assistant`, `tool`; `meta` falls through to the default block
    /// style — still rendered, just without a colored left border).
    pub(crate) fn css(self) -> &'static str {
        match self {
            CcRole::User => "user",
            CcRole::Assistant => "assistant",
            CcRole::Tool => "tool",
            CcRole::Meta => "meta",
        }
    }
}

/// One parsed transcript line, ready to render. `text` is the
/// human-readable projection of the content blocks; `raw` is the
/// pretty-printed JSON for the raw-drilldown (events) tab. `usage` /
/// `model` are populated only for assistant turns.
#[derive(Debug)]
pub(crate) struct CcEntry {
    /// 1-based index among successfully-parsed lines; the anchor id.
    pub(crate) seq: usize,
    pub(crate) timestamp_unix_ms: Option<u64>,
    /// The envelope `type` field verbatim (or `"<no type>"`).
    pub(crate) envelope_type: String,
    pub(crate) role: CcRole,
    pub(crate) text: String,
    pub(crate) raw: String,
    pub(crate) usage: Option<Usage>,
    pub(crate) model: Option<String>,
    /// mu-y5hz: this line is a subagent (isSidechain:true) turn. The
    /// transcript still renders it (flagged, not dropped), but the cost
    /// tab excludes its usage from the summed total — the same exclusion
    /// the index scanner applies (policy a).
    pub(crate) is_sidechain: bool,
}

/// A whole cc transcript, parsed line-by-line for the detail view.
#[derive(Debug, Default)]
pub(crate) struct CcTranscript {
    pub(crate) entries: Vec<CcEntry>,
    /// Lines that failed to parse as JSON (surfaced, never fatal).
    pub(crate) malformed_lines: usize,
    /// Last non-empty `message.model` from a NON-sidechain assistant turn,
    /// for the header (mu-y5hz: sidechain models don't pollute it).
    pub(crate) model: Option<String>,
    /// Latest timestamp across all envelopes.
    pub(crate) last_activity_unix_ms: Option<u64>,
    /// mu-y5hz: count of subagent (isSidechain:true) message turns in this
    /// transcript. They stay rendered but are excluded from the cost-tab
    /// summed total; this surfaces how many, rather than dropping silently.
    pub(crate) sidechain_entries: usize,
}

/// Read one cc transcript file into renderable [`CcEntry`] rows. Returns
/// an `io::Error` only when the file itself can't be read; malformed
/// *lines* inside a readable file are counted, not surfaced as errors.
pub(crate) fn read_cc_transcript(path: &Path) -> std::io::Result<CcTranscript> {
    let text = std::fs::read_to_string(path)?;
    let mut out = CcTranscript::default();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let v = match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) => v,
            Err(_) => {
                out.malformed_lines += 1;
                continue;
            }
        };
        let ts = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(parse_rfc3339_ms);
        if let Some(ms) = ts {
            out.last_activity_unix_ms =
                Some(out.last_activity_unix_ms.map_or(ms, |cur| cur.max(ms)));
        }
        let envelope_type = v
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("<no type>")
            .to_string();
        let raw = serde_json::to_string_pretty(&v).unwrap_or_else(|_| line.to_string());
        let sidechain = is_sidechain(&v);
        let (role, body, usage, model) = project_entry(&envelope_type, &v);
        // mu-y5hz: a sidechain (subagent) model must not pollute the
        // header's last-model-wins; only parent turns update it. Count
        // sidechain message turns so the view can surface the exclusion.
        if let Some(m) = &model {
            if !sidechain {
                out.model = Some(m.clone());
            }
        }
        if sidechain && matches!(envelope_type.as_str(), "user" | "assistant") {
            out.sidechain_entries += 1;
        }
        out.entries.push(CcEntry {
            seq: out.entries.len() + 1,
            timestamp_unix_ms: ts,
            envelope_type,
            role,
            text: body,
            raw,
            usage,
            model,
            is_sidechain: sidechain,
        });
    }
    Ok(out)
}

/// Map one envelope to `(role, rendered-text, usage, model)`. Assistant
/// and user message envelopes render their content blocks; everything
/// else is `Meta` (the raw JSON carries the detail in the events tab).
fn project_entry(
    envelope_type: &str,
    v: &serde_json::Value,
) -> (CcRole, String, Option<Usage>, Option<String>) {
    match envelope_type {
        "assistant" => {
            let message = v.get("message");
            let mut body = String::new();
            if let Some(content) = message.and_then(|m| m.get("content")) {
                render_content(content, &mut body);
            }
            let usage = message.and_then(|m| m.get("usage")).map(parse_usage);
            let model = message
                .and_then(|m| m.get("model"))
                .and_then(|m| m.as_str())
                .filter(|m| !m.is_empty())
                .map(|m| m.to_string());
            (CcRole::Assistant, body, usage, model)
        }
        "user" => {
            let content = v.get("message").and_then(|m| m.get("content"));
            // A user envelope whose content is entirely `tool_result`
            // blocks is the agent loop feeding results back, not an
            // operator ask — render it in the tool lane.
            let role = match content {
                Some(serde_json::Value::Array(blocks)) if !blocks.is_empty() => {
                    if blocks
                        .iter()
                        .all(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                    {
                        CcRole::Tool
                    } else {
                        CcRole::User
                    }
                }
                _ => CcRole::User,
            };
            let mut body = String::new();
            if let Some(content) = content {
                render_content(content, &mut body);
            }
            (role, body, None, None)
        }
        // summary / attachment / queue-operation / last-prompt / unknown:
        // keep the line visible as a Meta block; raw JSON has the detail.
        _ => (CcRole::Meta, String::new(), None, None),
    }
}

/// Render a `message.content` value (bare string or block array) into
/// `out`. Unknown shapes degrade to a labeled raw-JSON line rather than
/// disappearing.
fn render_content(content: &serde_json::Value, out: &mut String) {
    match content {
        serde_json::Value::String(s) => {
            out.push_str(s);
            out.push('\n');
        }
        serde_json::Value::Array(blocks) => {
            for b in blocks {
                render_block(b, out);
            }
        }
        serde_json::Value::Null => {}
        other => {
            out.push_str("[unknown content] ");
            out.push_str(&other.to_string());
            out.push('\n');
        }
    }
}

/// Render one content block. `text` / `thinking` / `tool_use` /
/// `tool_result` / `image` map to readable lines; any other `type` (or a
/// block with no `type`) degrades visibly to its raw JSON.
fn render_block(b: &serde_json::Value, out: &mut String) {
    let str_field = |key: &str| b.get(key).and_then(|x| x.as_str());
    match b.get("type").and_then(|t| t.as_str()) {
        Some("text") => {
            if let Some(t) = str_field("text") {
                out.push_str(t);
                out.push('\n');
            }
        }
        Some("thinking") => {
            out.push_str("[thinking] ");
            out.push_str(
                str_field("thinking")
                    .or_else(|| str_field("text"))
                    .unwrap_or(""),
            );
            out.push('\n');
        }
        Some("tool_use") => {
            let id = str_field("id").unwrap_or("");
            let name = str_field("name").unwrap_or("");
            let input = b
                .get("input")
                .map(|i| i.to_string())
                .unwrap_or_else(|| "null".to_string());
            out.push_str(&format!("[tool_use {id} {name}] {input}\n"));
        }
        Some("tool_result") => {
            let id = str_field("tool_use_id").unwrap_or("");
            let err = b.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
            let tag = if err {
                "tool_result ERR"
            } else {
                "tool_result"
            };
            out.push_str(&format!("[{tag} {id}] "));
            render_tool_result_content(b.get("content"), out);
            out.push('\n');
        }
        Some("image") => {
            out.push_str("[image]\n");
        }
        Some(other) => {
            out.push_str(&format!("[unknown block: {other}] {b}\n"));
        }
        None => {
            out.push_str(&format!("[block] {b}\n"));
        }
    }
}

/// A `tool_result.content` is either a bare string or an array of text
/// blocks. Anything else degrades to raw JSON.
fn render_tool_result_content(content: Option<&serde_json::Value>, out: &mut String) {
    match content {
        Some(serde_json::Value::String(s)) => out.push_str(s),
        Some(serde_json::Value::Array(parts)) => {
            for p in parts {
                if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                    out.push_str(t);
                } else {
                    out.push_str(&p.to_string());
                }
            }
        }
        Some(other) => out.push_str(&other.to_string()),
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write `lines` as a `.jsonl` under `<root>/<project>/<session>.jsonl`
    /// and return the root. Test-only fixture; the scanner itself never
    /// writes anything under a real cc projects dir.
    fn fixture(root: &Path, project: &str, session: &str, lines: &[&str]) {
        let dir = root.join(project);
        std::fs::create_dir_all(&dir).unwrap();
        let mut f = std::fs::File::create(dir.join(format!("{session}.jsonl"))).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("mu-cc-scan-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn projects_a_session_with_usage_and_counts() {
        let root = tmp("happy");
        fixture(
            &root,
            "-home-tcovert-src-mu",
            "sess-aaa",
            &[
                r#"{"type":"queue-operation","timestamp":"2026-06-06T07:31:19.000Z"}"#,
                r#"{"type":"user","timestamp":"2026-06-06T07:31:20.000Z","message":{"content":"hello there"}}"#,
                r#"{"type":"assistant","timestamp":"2026-06-06T07:31:25.500Z","message":{"model":"claude-opus-4-8","content":[{"type":"thinking","text":"hm"},{"type":"tool_use","id":"t1","name":"Bash"}],"usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":900,"cache_creation_input_tokens":40,"cache_creation":{"ephemeral_5m_input_tokens":10,"ephemeral_1h_input_tokens":30}}}}"#,
                r#"{"type":"user","timestamp":"2026-06-06T07:31:30.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
                r#"{"type":"assistant","timestamp":"2026-06-06T07:31:35.000Z","message":{"model":"claude-opus-4-8","content":[{"type":"text","text":"done"}],"usage":{"input_tokens":200,"output_tokens":20}}}"#,
            ],
        );

        let scan = scan_cc_sessions(&root);
        assert_eq!(scan.sessions.len(), 1, "one session expected");
        assert_eq!(scan.malformed_files, 0);
        let s = &scan.sessions[0];
        assert_eq!(s.daemon_id, "-home-tcovert-src-mu");
        assert_eq!(s.session_id, "sess-aaa");
        assert_eq!(s.provider.as_deref(), Some("claude-code"));
        assert_eq!(s.model.as_deref(), Some("claude-opus-4-8"));
        // Only the string-content user envelope is an ask; the
        // tool_result envelope is not.
        assert_eq!(s.ask_count, 1);
        assert_eq!(s.context_assembly_count, 2, "two assistant turns");
        assert_eq!(s.tool_call_count, 1, "one tool_use block");
        let u = s.usage.expect("usage summed");
        assert_eq!(u.input_tokens, 300);
        assert_eq!(u.output_tokens, 70);
        assert_eq!(u.cache_read_input_tokens, Some(900));
        assert_eq!(u.cache_creation_input_tokens, Some(40));
        assert_eq!(u.cache_creation_5m_input_tokens, Some(10));
        assert_eq!(u.cache_creation_1h_input_tokens, Some(30));
        // Latest timestamp across all envelopes wins.
        let expect_last = parse_rfc3339_ms("2026-06-06T07:31:35.000Z");
        assert_eq!(s.last_activity_unix_ms, expect_last);
        // The scanner is read-only — fixture is untouched on disk, but
        // crucially the scan never created/appended anything.
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skips_malformed_lines_without_panicking() {
        let root = tmp("malformed");
        fixture(
            &root,
            "proj",
            "sess-bad",
            &[
                r#"{"type":"user","message":{"content":"hi"}}"#,
                r#"this is not json at all"#,
                r#"{"type":"assistant","message":{"model":"m","content":[]"#, // truncated/invalid
                r#"{"type":"assistant","message":{"model":"claude-x","content":[],"usage":{"input_tokens":5,"output_tokens":1}}}"#,
            ],
        );
        let scan = scan_cc_sessions(&root);
        assert_eq!(scan.sessions.len(), 1);
        assert_eq!(scan.malformed_files, 1, "file had malformed lines");
        let s = &scan.sessions[0];
        assert_eq!(s.ask_count, 1);
        assert_eq!(s.model.as_deref(), Some("claude-x"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ignores_metadata_only_files() {
        let root = tmp("metaonly");
        fixture(
            &root,
            "proj",
            "sess-meta",
            &[
                r#"{"type":"attachment","timestamp":"2026-06-06T07:31:19.000Z"}"#,
                r#"{"type":"queue-operation"}"#,
                r#"{"type":"summary","summary":"x"}"#,
            ],
        );
        let scan = scan_cc_sessions(&root);
        assert!(
            scan.sessions.is_empty(),
            "metadata-only file is not a session"
        );
        assert_eq!(scan.malformed_files, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_projects_dir_is_empty_not_error() {
        let scan = scan_cc_sessions(Path::new("/nonexistent/cc/projects/xyz"));
        assert!(scan.sessions.is_empty());
        assert_eq!(scan.malformed_files, 0);
        assert_eq!(scan.skipped_entries, 0);
    }

    #[test]
    fn unreadable_jsonl_entry_counts_as_malformed_not_panic() {
        // A `.jsonl`-named entry that can't be read as a file exercises
        // the `Err` arm of `scan_one_file` (read_to_string fails). A
        // *directory* named `x.jsonl` is the portable way to force that:
        // the extension check passes, but reading it yields an error.
        let root = tmp("unreadable");
        std::fs::create_dir_all(root.join("proj").join("notafile.jsonl")).unwrap();
        // Plus a real session so the scan still produces useful output.
        fixture(
            &root,
            "proj",
            "good",
            &[r#"{"type":"user","message":{"content":"hi"}}"#],
        );
        let scan = scan_cc_sessions(&root);
        assert_eq!(scan.sessions.len(), 1, "the real session still scans");
        assert_eq!(scan.sessions[0].session_id, "good");
        assert_eq!(
            scan.malformed_files, 1,
            "the unreadable .jsonl entry is counted, scan does not panic"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Write `lines` to a single `.jsonl` and return its path, for the
    /// transcript-reader tests (which read one file, not a tree).
    fn transcript_fixture(tag: &str, lines: &[&str]) -> PathBuf {
        let root = tmp(tag);
        let path = root.join("session.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        path
    }

    #[test]
    fn reads_transcript_mapping_blocks_to_roles() {
        let path = transcript_fixture(
            "tx-happy",
            &[
                r#"{"type":"summary","summary":"prior session"}"#,
                r#"{"type":"user","timestamp":"2026-06-06T07:31:20.000Z","message":{"content":"hello there"}}"#,
                r#"{"type":"assistant","timestamp":"2026-06-06T07:31:25.500Z","message":{"model":"claude-opus-4-8","content":[{"type":"thinking","thinking":"hm"},{"type":"text","text":"working on it"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}],"usage":{"input_tokens":100,"output_tokens":50}}}"#,
                r#"{"type":"user","timestamp":"2026-06-06T07:31:30.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"file.txt"}]}}"#,
                r#"{"type":"assistant","timestamp":"2026-06-06T07:31:35.000Z","message":{"model":"claude-opus-4-8","content":[{"type":"text","text":"done"}],"usage":{"input_tokens":200,"output_tokens":20}}}"#,
            ],
        );
        let tx = read_cc_transcript(&path).unwrap();
        assert_eq!(tx.malformed_lines, 0);
        assert_eq!(tx.entries.len(), 5, "every line becomes an entry");
        assert_eq!(tx.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(
            tx.last_activity_unix_ms,
            parse_rfc3339_ms("2026-06-06T07:31:35.000Z")
        );
        // summary envelope -> Meta lane, raw preserved, no rendered text.
        assert_eq!(tx.entries[0].role, CcRole::Meta);
        assert_eq!(tx.entries[0].envelope_type, "summary");
        assert!(tx.entries[0].raw.contains("prior session"));
        // string-content user -> User lane.
        assert_eq!(tx.entries[1].role, CcRole::User);
        assert!(tx.entries[1].text.contains("hello there"));
        // assistant blocks render thinking/text/tool_use; usage captured.
        let a = &tx.entries[2];
        assert_eq!(a.role, CcRole::Assistant);
        assert!(a.text.contains("[thinking] hm"));
        assert!(a.text.contains("working on it"));
        assert!(a.text.contains("[tool_use t1 Bash]"));
        assert!(a.text.contains("\"command\":\"ls\""));
        assert_eq!(a.usage.as_ref().map(|u| u.input_tokens), Some(100));
        assert_eq!(a.model.as_deref(), Some("claude-opus-4-8"));
        // tool_result-only user envelope -> Tool lane.
        assert_eq!(tx.entries[3].role, CcRole::Tool);
        assert!(tx.entries[3].text.contains("[tool_result t1] file.txt"));
        // seq is 1-based and dense.
        assert_eq!(tx.entries[0].seq, 1);
        assert_eq!(tx.entries[4].seq, 5);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn unknown_shapes_degrade_visibly_not_panic() {
        let path = transcript_fixture(
            "tx-unknown",
            &[
                r#"not valid json"#,
                r#"{"type":"assistant","message":{"content":[{"type":"redacted_thinking","data":"xx"},{"foo":"bar"}]}}"#,
                r#"{"type":"some-future-envelope","payload":{"a":1}}"#,
            ],
        );
        let tx = read_cc_transcript(&path).unwrap();
        assert_eq!(tx.malformed_lines, 1, "the non-JSON line is counted");
        assert_eq!(tx.entries.len(), 2, "two parseable lines remain");
        // Unknown block type and type-less block both degrade to raw.
        let a = &tx.entries[0];
        assert_eq!(a.role, CcRole::Assistant);
        assert!(a.text.contains("[unknown block: redacted_thinking]"));
        assert!(a.text.contains("[block]"));
        assert!(a.text.contains("\"foo\":\"bar\""));
        // Unknown envelope -> Meta, raw retained.
        assert_eq!(tx.entries[1].role, CcRole::Meta);
        assert_eq!(tx.entries[1].envelope_type, "some-future-envelope");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn read_transcript_missing_file_is_err_not_panic() {
        let err = read_cc_transcript(Path::new("/nonexistent/cc/x.jsonl"));
        assert!(err.is_err(), "missing file surfaces as io::Error");
    }

    // ── mu-y5hz: sidechain exclusion + model-switch (scanner) ──────────

    #[test]
    fn excludes_sidechain_turns_from_rollups() {
        // mu-y5hz policy (a): isSidechain:true subagent turns must NOT
        // inflate the parent session's ask/assistant/tool/usage rollups
        // or its model detection; they're counted in sidechain_entries.
        let root = tmp("sidechain");
        fixture(
            &root,
            "proj",
            "sess-side",
            &[
                // Parent operator ask.
                r#"{"type":"user","isSidechain":false,"timestamp":"2026-06-06T07:31:20.000Z","message":{"content":"do the thing"}}"#,
                // Parent assistant: model A, one tool_use, usage.
                r#"{"type":"assistant","isSidechain":false,"timestamp":"2026-06-06T07:31:25.000Z","message":{"model":"claude-opus-4-8","content":[{"type":"tool_use","id":"t1","name":"Task"}],"usage":{"input_tokens":100,"output_tokens":50}}}"#,
                // Sidechain user ask — EXCLUDED, counted.
                r#"{"type":"user","isSidechain":true,"timestamp":"2026-06-06T07:31:26.000Z","message":{"content":"subagent prompt"}}"#,
                // Sidechain assistant: different model, 2 tool_use, big
                // usage — ALL excluded (must not touch model/models_seen/
                // tool_call_count/usage).
                r#"{"type":"assistant","isSidechain":true,"timestamp":"2026-06-06T07:31:28.000Z","message":{"model":"sub-agent-model","content":[{"type":"tool_use","id":"s1","name":"Grep"},{"type":"tool_use","id":"s2","name":"Read"}],"usage":{"input_tokens":9999,"output_tokens":9999}}}"#,
                // Parent assistant again: model A, more usage.
                r#"{"type":"assistant","isSidechain":false,"timestamp":"2026-06-06T07:31:35.000Z","message":{"model":"claude-opus-4-8","content":[{"type":"text","text":"done"}],"usage":{"input_tokens":200,"output_tokens":20}}}"#,
            ],
        );
        let scan = scan_cc_sessions(&root);
        assert_eq!(scan.sessions.len(), 1);
        let s = &scan.sessions[0];
        // Only the parent ask counts.
        assert_eq!(s.ask_count, 1, "sidechain ask excluded");
        // Only the two parent assistant turns.
        assert_eq!(s.context_assembly_count, 2, "sidechain assistant excluded");
        // Only the parent tool_use; the sidechain's two are excluded.
        assert_eq!(s.tool_call_count, 1, "sidechain tool_use excluded");
        // Usage summed over parent turns only (100+200 / 50+20).
        let u = s.usage.expect("usage summed from parent turns");
        assert_eq!(u.input_tokens, 300, "sidechain usage excluded");
        assert_eq!(u.output_tokens, 70, "sidechain usage excluded");
        // Model detection ignores the sidechain model entirely.
        assert_eq!(s.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(s.models_seen, 1, "sidechain model not counted");
        // Both sidechain message turns surfaced, not dropped.
        assert_eq!(s.sidechain_entries, 2);
        // last_activity still reflects the latest envelope overall
        // (a max, immune to sidechain inflation).
        assert_eq!(
            s.last_activity_unix_ms,
            parse_rfc3339_ms("2026-06-06T07:31:35.000Z")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn counts_distinct_models_when_switching() {
        // mu-y5hz policy (b): a session that switches models mid-run keeps
        // last-model-wins for the `model` column AND reports models_seen.
        let root = tmp("modelswitch");
        fixture(
            &root,
            "proj",
            "sess-switch",
            &[
                r#"{"type":"user","message":{"content":"hi"}}"#,
                r#"{"type":"assistant","message":{"model":"claude-opus-4-8","content":[],"usage":{"input_tokens":1,"output_tokens":1}}}"#,
                r#"{"type":"assistant","message":{"model":"claude-sonnet-4-6","content":[],"usage":{"input_tokens":1,"output_tokens":1}}}"#,
                // Back to the first model: dedup means models_seen stays 2,
                // and last-wins makes `model` opus again.
                r#"{"type":"assistant","message":{"model":"claude-opus-4-8","content":[],"usage":{"input_tokens":1,"output_tokens":1}}}"#,
            ],
        );
        let scan = scan_cc_sessions(&root);
        assert_eq!(scan.sessions.len(), 1);
        let s = &scan.sessions[0];
        assert_eq!(
            s.model.as_deref(),
            Some("claude-opus-4-8"),
            "last-model-wins"
        );
        assert_eq!(s.models_seen, 2, "two distinct models despite three turns");
        assert_eq!(s.sidechain_entries, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pure_sidechain_file_is_not_a_session() {
        // A file containing ONLY subagent (sidechain) turns is not a
        // standalone operator session — it must not project a row. (On the
        // current cc format these live in a `subagents/` subdir the scanner
        // doesn't descend into; this guards the inline case defensively.)
        let root = tmp("puresidechain");
        fixture(
            &root,
            "proj",
            "agent-sub",
            &[
                r#"{"type":"user","isSidechain":true,"message":{"content":"subagent prompt"}}"#,
                r#"{"type":"assistant","isSidechain":true,"message":{"model":"sub-agent-model","content":[{"type":"tool_use","id":"s1","name":"Read"}],"usage":{"input_tokens":5,"output_tokens":5}}}"#,
            ],
        );
        let scan = scan_cc_sessions(&root);
        assert!(
            scan.sessions.is_empty(),
            "a pure-sidechain transcript is not projected as a session"
        );
        assert_eq!(scan.malformed_files, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── mu-y5hz: sidechain handling in the detail-view reader ──────────

    #[test]
    fn reader_flags_sidechain_and_keeps_header_model_clean() {
        // The detail reader RENDERS every line (sidechain included) but
        // flags sidechain entries, counts them, and must not let a
        // sidechain model pollute the header's last-model-wins.
        let path = transcript_fixture(
            "tx-sidechain",
            &[
                r#"{"type":"user","isSidechain":false,"timestamp":"2026-06-06T07:31:20.000Z","message":{"content":"do the thing"}}"#,
                r#"{"type":"assistant","isSidechain":false,"timestamp":"2026-06-06T07:31:25.000Z","message":{"model":"claude-opus-4-8","content":[{"type":"text","text":"on it"}],"usage":{"input_tokens":100,"output_tokens":50}}}"#,
                // Sidechain user + assistant: rendered, flagged, counted;
                // the sidechain model must NOT become the header model.
                r#"{"type":"user","isSidechain":true,"timestamp":"2026-06-06T07:31:26.000Z","message":{"content":"subagent prompt"}}"#,
                r#"{"type":"assistant","isSidechain":true,"timestamp":"2026-06-06T07:31:28.000Z","message":{"model":"sub-agent-model","content":[{"type":"text","text":"sub work"}],"usage":{"input_tokens":9999,"output_tokens":9999}}}"#,
            ],
        );
        let tx = read_cc_transcript(&path).unwrap();
        // Every line is still rendered — nothing dropped.
        assert_eq!(
            tx.entries.len(),
            4,
            "all lines rendered, sidechain included"
        );
        // Two subagent message turns counted.
        assert_eq!(tx.sidechain_entries, 2);
        // Per-entry sidechain flags.
        assert!(!tx.entries[0].is_sidechain);
        assert!(!tx.entries[1].is_sidechain);
        assert!(tx.entries[2].is_sidechain);
        assert!(tx.entries[3].is_sidechain);
        // Header model is the PARENT model, not the sidechain's.
        assert_eq!(tx.model.as_deref(), Some("claude-opus-4-8"));
        // The sidechain assistant still carries its own usage/model on the
        // entry (so the cost tab can list-but-exclude it).
        assert_eq!(tx.entries[3].model.as_deref(), Some("sub-agent-model"));
        assert_eq!(
            tx.entries[3].usage.as_ref().map(|u| u.input_tokens),
            Some(9999)
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
