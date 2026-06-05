//! Tier-1 tool-result ingestion hygiene (mu-2e0h).
//!
//! Deterministic, model-free cruft removal applied to every tool
//! result at the execute_tools seam — BEFORE the content enters the
//! provider context, the durable event log, and the wire. Filtering
//! once at that seam keeps all three views identical (the
//! event-log-decides diagnostic discipline depends on the log being
//! the truth of what the model saw).
//!
//! Rules, applied in order:
//!   1. Strip ANSI escape sequences (CSI / OSC / two-char escapes) —
//!      colorized tool output differs only in styling, which is pure
//!      token waste and defeats rule 2's duplicate detection.
//!   2. Cap pathological line lengths at [`MAX_LINE_CHARS`] chars
//!      (minified bundles, base64 blobs) with a `… [+N more chars]`
//!      marker.
//!   3. Collapse runs of >= [`REPEAT_COLLAPSE_THRESHOLD`] identical
//!      consecutive lines to the first occurrence plus
//!      `[line repeated N more times]` (progress spinners, repeated
//!      warnings, blank-line runs).
//!   4. Truncate past [`MAX_LINES`] processed lines with
//!      `[truncated, N more lines]`.
//!
//! The economics (from the bead): a raw 50K-token result in an opus
//! context pays cache-write once plus cache-read RENT on every
//! subsequent call; cruft never earns that rent back. Tier 1 is
//! verbatim-where-it-matters — it never paraphrases; it only removes
//! styling, repetition, and pathological excess, each with an
//! explicit marker. Tier 2 (sidecar distillation for oversized
//! results) is a separate, model-bearing feature and intentionally
//! NOT here.
//!
//! The filter is idempotent (filter(filter(x)) == filter(x)) — safe
//! to re-apply during rehydration or replay. Clean content passes
//! through with zero allocation beyond the borrow check.

use std::borrow::Cow;

/// Lines longer than this (in chars) are capped with a marker.
const MAX_LINE_CHARS: usize = 2000;

/// Runs of identical consecutive lines at or above this length are
/// collapsed. 3 keeps natural doubles (e.g. a repeated word on two
/// lines) untouched.
const REPEAT_COLLAPSE_THRESHOLD: usize = 3;

/// Results longer than this many (post-processing) lines are
/// truncated with a marker.
const MAX_LINES: usize = 1000;

/// Split into '\n'-separated segments, KEEPING any '\r' as line
/// content so CRLF endings survive filtering verbatim (the dirty path
/// reassembles with '\n', which restores '\r\n' for lines that kept
/// their '\r'). The trailing empty segment of newline-terminated
/// content is dropped; callers restore the final newline explicitly.
/// Both `is_clean` and the rewrite path MUST traverse via this
/// helper — a divergence here is exactly the clean/dirty disagreement
/// the fast path must never have.
fn line_iter(content: &str) -> std::str::Split<'_, char> {
    content.strip_suffix('\n').unwrap_or(content).split('\n')
}

/// Apply the tier-1 filter. Returns `Cow::Borrowed` when the content
/// is already clean — the common case costs one scan and no
/// allocation.
pub fn filter_tool_result(content: &str) -> Cow<'_, str> {
    if is_clean(content) {
        return Cow::Borrowed(content);
    }

    // Rule 1: strip ANSI first so styling differences don't defeat
    // duplicate detection below.
    let stripped = strip_ansi(content);

    // Rules 2+3: per-line cap, then collapse runs of identical lines.
    let mut out: Vec<String> = Vec::new();
    let mut run_line: Option<&str> = None;
    let mut run_count: usize = 0;

    fn flush_run(out: &mut Vec<String>, line: Option<&str>, count: usize) {
        let Some(line) = line else { return };
        if count >= REPEAT_COLLAPSE_THRESHOLD {
            out.push(cap_line(line));
            out.push(format!("[line repeated {} more times]", count - 1));
        } else {
            for _ in 0..count {
                out.push(cap_line(line));
            }
        }
    }

    for line in line_iter(&stripped) {
        match run_line {
            Some(prev) if prev == line => run_count += 1,
            _ => {
                flush_run(&mut out, run_line, run_count);
                run_line = Some(line);
                run_count = 1;
            }
        }
    }
    flush_run(&mut out, run_line, run_count);

    // Rule 4: total truncation on processed lines. The marker line is
    // INCLUDED in the MAX_LINES budget so the output never exceeds it
    // — otherwise a second pass would truncate again (idempotency).
    let mut truncated = false;
    if out.len() > MAX_LINES {
        let remaining = out.len() - (MAX_LINES - 1);
        out.truncate(MAX_LINES - 1);
        out.push(format!("[truncated, {remaining} more lines]"));
        truncated = true;
    }

    let mut result = out.join("\n");
    // Preserve a trailing newline if the original had one — except
    // after truncation, where the marker ends the content.
    if !truncated && stripped.ends_with('\n') && !result.ends_with('\n') {
        result.push('\n');
    }
    Cow::Owned(result)
}

/// Fast pre-scan: true when no rule would change the content, so the
/// caller gets a borrow back. Line counting and per-line checks in
/// one pass.
fn is_clean(content: &str) -> bool {
    if content.contains('\x1b') {
        return false;
    }
    let mut lines = 0usize;
    let mut prev: Option<&str> = None;
    let mut run = 0usize;
    for line in line_iter(content) {
        lines += 1;
        if lines > MAX_LINES {
            return false;
        }
        if line.chars().nth(MAX_LINE_CHARS).is_some() {
            return false;
        }
        if prev == Some(line) {
            run += 1;
            if run + 1 >= REPEAT_COLLAPSE_THRESHOLD {
                return false;
            }
        } else {
            run = 0;
        }
        prev = Some(line);
    }
    true
}

/// Headroom reserved for the cap marker so a capped line (content +
/// marker) stays under MAX_LINE_CHARS — otherwise a second pass would
/// cap the capped line again (idempotency).
const CAP_MARKER_RESERVE: usize = 32;

/// Cap one over-long line (char-boundary safe). Triggers past
/// MAX_LINE_CHARS; keeps MAX_LINE_CHARS - CAP_MARKER_RESERVE chars so
/// the result, marker included, is comfortably under the trigger.
fn cap_line(line: &str) -> String {
    if line.chars().nth(MAX_LINE_CHARS).is_none() {
        return line.to_string();
    }
    let keep = MAX_LINE_CHARS - CAP_MARKER_RESERVE;
    let byte_idx = line
        .char_indices()
        .nth(keep)
        .map(|(b, _)| b)
        .unwrap_or(line.len());
    let over = line[byte_idx..].chars().count();
    format!("{}… [+{} more chars]", &line[..byte_idx], over)
}

/// Remove ANSI escape sequences: CSI (`ESC [ … final`), OSC
/// (`ESC ] … BEL` or `ESC ] … ESC \`), and two-character escapes
/// (`ESC x`). Unterminated sequences at end-of-input are dropped.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            // CSI: ESC [ params/intermediates… final byte @–~
            Some('[') => {
                chars.next();
                for c2 in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c2) {
                        break;
                    }
                }
            }
            // OSC: ESC ] … terminated by BEL or ESC \
            Some(']') => {
                chars.next();
                while let Some(c2) = chars.next() {
                    if c2 == '\x07' {
                        break;
                    }
                    if c2 == '\x1b' {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            // Two-char escape (ESC c, ESC =, …) — drop both.
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_content_borrows_unchanged() {
        let s = "ordinary output\nwith two lines\n";
        match filter_tool_result(s) {
            Cow::Borrowed(b) => assert_eq!(b, s),
            Cow::Owned(_) => panic!("clean content must not allocate"),
        }
    }

    #[test]
    fn ansi_stripped() {
        let s = "\x1b[31mred\x1b[0m plain \x1b]0;title\x07tail";
        assert_eq!(filter_tool_result(s), "red plain tail");
    }

    #[test]
    fn long_line_capped() {
        let long = "x".repeat(MAX_LINE_CHARS + 50);
        let out = filter_tool_result(&long);
        // 50 over the trigger + the reserve headroom both get cut.
        assert!(out.contains(&format!("… [+{} more chars]", 50 + CAP_MARKER_RESERVE)));
        // Capped line must sit UNDER the trigger so it never re-caps.
        assert!(out.chars().count() < MAX_LINE_CHARS);
    }

    #[test]
    fn long_line_cap_is_char_boundary_safe() {
        // Multibyte chars straddling the cap must not split.
        let long = "é".repeat(MAX_LINE_CHARS + 7);
        let out = filter_tool_result(&long);
        assert!(out.contains(&format!("… [+{} more chars]", 7 + CAP_MARKER_RESERVE)));
    }

    #[test]
    fn repeated_lines_collapsed() {
        let s = "warming up\nspam\nspam\nspam\nspam\ndone";
        let out = filter_tool_result(s);
        assert_eq!(out, "warming up\nspam\n[line repeated 3 more times]\ndone");
    }

    #[test]
    fn natural_doubles_kept() {
        let s = "twice\ntwice\nend";
        match filter_tool_result(s) {
            Cow::Borrowed(b) => assert_eq!(b, s),
            Cow::Owned(o) => panic!("double below threshold must pass through, got {o:?}"),
        }
    }

    #[test]
    fn blank_line_runs_collapse() {
        let s = "a\n\n\n\n\nb";
        let out = filter_tool_result(s);
        assert_eq!(out, "a\n\n[line repeated 3 more times]\nb");
    }

    #[test]
    fn dump_truncated() {
        let s: String = (0..MAX_LINES + 25).map(|i| format!("line {i}\n")).collect();
        let out = filter_tool_result(&s);
        // Marker is inside the MAX_LINES budget: keep MAX_LINES - 1
        // content lines, so 26 (25 over + 1 displaced) are gone.
        assert!(
            out.ends_with("[truncated, 26 more lines]"),
            "got tail: {:?}",
            &out[out.len().saturating_sub(40)..]
        );
        assert_eq!(out.lines().count(), MAX_LINES);
    }

    #[test]
    fn collapse_runs_before_truncation_counts() {
        // A huge run of identical lines collapses to 2 lines, so a
        // result that LOOKS over-long by raw lines can survive whole.
        let mut s = String::new();
        for _ in 0..(MAX_LINES * 2) {
            s.push_str("same\n");
        }
        s.push_str("tail");
        let out = filter_tool_result(&s);
        assert_eq!(
            out,
            format!(
                "same\n[line repeated {} more times]\ntail",
                MAX_LINES * 2 - 1
            )
        );
    }

    #[test]
    fn idempotent() {
        let mixed = format!(
            "\x1b[1mtitle\x1b[0m\n{}\n{}\n{}",
            "dup\n".repeat(10),
            "y".repeat(MAX_LINE_CHARS + 9),
            (0..MAX_LINES + 5)
                .map(|i| format!("l{i}\n"))
                .collect::<String>(),
        );
        let once = filter_tool_result(&mixed).into_owned();
        let twice = filter_tool_result(&once).into_owned();
        assert_eq!(
            once, twice,
            "filter must be idempotent (rehydration re-applies)"
        );
    }

    #[test]
    fn trailing_newline_preserved() {
        let s = "\x1b[2mdim\x1b[0m line\n";
        assert_eq!(filter_tool_result(s), "dim line\n");
    }

    #[test]
    fn unterminated_csi_dropped() {
        let s = "before\x1b[31";
        assert_eq!(filter_tool_result(s), "before");
    }

    #[test]
    fn crlf_clean_content_borrows_unchanged() {
        let s = "windows line one\r\nwindows line two\r\n";
        match filter_tool_result(s) {
            Cow::Borrowed(b) => assert_eq!(b, s),
            Cow::Owned(o) => panic!("clean CRLF must pass through, got {o:?}"),
        }
    }

    #[test]
    fn crlf_endings_survive_a_triggered_rewrite() {
        // One ANSI code triggers the rewrite path; the untouched CRLF
        // lines must keep their \r — silently normalizing the whole
        // result would mutate verbatim content and diverge from the
        // clean path (verifier finding, 2026-06-05).
        let s = "keep1\r\nkeep2\r\n\x1b[31mred\x1b[0m\r\nkeep3\r\n";
        let out = filter_tool_result(s);
        assert_eq!(out, "keep1\r\nkeep2\r\nred\r\nkeep3\r\n");
    }

    #[test]
    fn crlf_duplicate_runs_collapse_consistently() {
        // CRLF duplicates compare equal to each other (\r is part of
        // the line content on both sides), so collapse still fires.
        let s = "spam\r\nspam\r\nspam\r\nspam\r\ntail\r\n";
        let out = filter_tool_result(s);
        assert_eq!(out, "spam\r\n[line repeated 3 more times]\ntail\r\n");
    }

    #[test]
    fn empty_and_newline_only_content_pass_through() {
        for s in ["", "\n", "\r\n"] {
            match filter_tool_result(s) {
                Cow::Borrowed(b) => assert_eq!(b, s),
                Cow::Owned(o) => panic!("{s:?} must pass through, got {o:?}"),
            }
        }
    }
}
