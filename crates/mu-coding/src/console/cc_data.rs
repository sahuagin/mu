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

use std::path::{Path, PathBuf};

use mu_core::agent::Usage;

use super::data::{ScanResult, SessionSummary};

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
    model: Option<String>,
    last_activity_unix_ms: Option<u64>,
    ask_count: u32,
    assistant_count: u32,
    tool_call_count: u32,
    usage: Option<Usage>,
}

impl CcAccumulator {
    fn ingest(&mut self, v: &serde_json::Value) {
        // Every envelope can carry a timestamp; track the latest across
        // all types so sort order reflects real last activity.
        if let Some(ts) = v.get("timestamp").and_then(|t| t.as_str()) {
            if let Some(ms) = parse_rfc3339_ms(ts) {
                self.last_activity_unix_ms =
                    Some(self.last_activity_unix_ms.map_or(ms, |cur| cur.max(ms)));
            }
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
                self.model = Some(model.to_string());
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
        }
    }
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

/// Parse an RFC 3339 / ISO 8601 UTC timestamp (`YYYY-MM-DDTHH:MM:SS`,
/// optional `.fff` fraction, optional `Z`) into epoch milliseconds.
/// Returns `None` on any malformation — cc always emits `…Z` UTC, so we
/// only handle the `Z`/no-offset case and ignore non-UTC offsets rather
/// than mis-parse them. Dependency-free (no chrono in this crate).
fn parse_rfc3339_ms(s: &str) -> Option<u64> {
    let s = s.trim();
    let (date, rest) = s.split_once('T')?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    if d.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // Strip a trailing 'Z'; reject explicit non-UTC offsets to avoid
    // silently dropping them.
    let time = rest.strip_suffix('Z').unwrap_or(rest);
    if time.contains('+') || time.contains('-') {
        return None;
    }

    let (hms, frac) = match time.split_once('.') {
        Some((hms, frac)) => (hms, Some(frac)),
        None => (time, None),
    };
    let mut t = hms.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let minute: i64 = t.next()?.parse().ok()?;
    let second: i64 = t.next()?.parse().ok()?;
    if t.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    // Fraction → milliseconds (first 3 digits, zero-padded).
    let millis: i64 = match frac {
        Some(f) => {
            let digits: String = f.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                return None;
            }
            let mut ms = digits;
            ms.truncate(3);
            while ms.len() < 3 {
                ms.push('0');
            }
            ms.parse().ok()?
        }
        None => 0,
    };

    let days = days_from_civil(year, month, day);
    let total_ms = (((days * 24 + hour) * 60 + minute) * 60 + second) * 1000 + millis;
    u64::try_from(total_ms).ok()
}

/// Days since the Unix epoch (1970-01-01) for a civil date, via Howard
/// Hinnant's `days_from_civil`. Valid for the proleptic Gregorian
/// calendar; cc timestamps are always well past 1970.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
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
    fn parses_rfc3339_to_epoch_ms() {
        // 1970-01-01T00:00:00Z == 0.
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        // Known epoch: 2026-06-06T07:31:19.771Z.
        // days_from_civil(2026,6,6) computed by the same algorithm.
        let days = days_from_civil(2026, 6, 6);
        let expect = (((days * 24 + 7) * 60 + 31) * 60 + 19) * 1000 + 771;
        assert_eq!(
            parse_rfc3339_ms("2026-06-06T07:31:19.771Z"),
            Some(expect as u64)
        );
        // Without fractional seconds, without Z.
        assert_eq!(
            parse_rfc3339_ms("2026-06-06T07:31:19"),
            Some(((days_from_civil(2026, 6, 6) * 24 + 7) * 60 + 31) as u64 * 60_000 + 19_000)
        );
    }

    #[test]
    fn rejects_malformed_timestamps() {
        assert_eq!(parse_rfc3339_ms("not-a-date"), None);
        assert_eq!(parse_rfc3339_ms("2026-13-01T00:00:00Z"), None); // bad month
        assert_eq!(parse_rfc3339_ms("2026-06-06T25:00:00Z"), None); // bad hour
        assert_eq!(parse_rfc3339_ms(""), None);
        // Non-UTC offset: refused rather than mis-parsed.
        assert_eq!(parse_rfc3339_ms("2026-06-06T07:31:19+02:00"), None);
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
}
