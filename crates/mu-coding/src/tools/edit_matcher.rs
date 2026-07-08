//! Staged fallback matching for the `edit` tool. See bead mu-t731, spec mu-022.
//!
//! Models — local coder models especially — frequently reproduce file content
//! with small whitespace drift (trailing spaces dropped, a block re-indented).
//! With exact-only matching those edits fail and the model burns turns
//! re-reading and retrying. This module keeps exact matching authoritative and
//! adds two *line-granularity* fallback stages that only engage when the
//! previous stage found nothing:
//!
//! 1. **Exact** — byte-for-byte substring (the original behavior).
//! 2. **Trailing-whitespace-insensitive** — lines compared with
//!    `trim_end()`; catches trailing-space/CR drift.
//! 3. **Uniform indent shift** — every non-blank line differs by the *same*
//!    added or removed leading prefix; `new_string` is re-indented by that
//!    prefix so the replacement lands at the file's real indentation.
//!
//! Fallback stages match whole lines only — never sub-line regions — so a
//! fuzzy match can't splice into the middle of a line it merely resembles.
//! Ambiguity at any stage is the caller's problem to refuse; this module just
//! reports every non-overlapping match at the first stage that has any.

/// Which stage produced the match. Reported to the model so it learns its
/// `old_string` drifted from the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchStage {
    Exact,
    TrailingWs,
    IndentShift,
}

impl MatchStage {
    /// Short human/model-facing label used in tool output.
    pub fn label(&self) -> &'static str {
        match self {
            MatchStage::Exact => "exact",
            MatchStage::TrailingWs => "whitespace-tolerant",
            MatchStage::IndentShift => "indent-shift",
        }
    }
}

/// One match: a byte span of the original contents and the exact replacement
/// text to splice into it (already re-indented for indent-shift matches).
#[derive(Debug, Clone)]
pub struct MatchSpan {
    pub start: usize,
    pub end: usize,
    pub replacement: String,
}

/// All matches found at the first stage that produced any.
#[derive(Debug, Clone)]
pub struct StagedMatches {
    pub stage: MatchStage,
    pub spans: Vec<MatchSpan>,
    /// Human-readable indent description (only for `IndentShift`), e.g.
    /// `right by "    "` — quoted so tabs/spaces are visible.
    pub indent_note: Option<String>,
}

/// Byte layout of one line: `[start, content_end)` excludes the line
/// terminator; `next_start` is where the following line begins (past `\n`).
struct LineSpan {
    start: usize,
    content_end: usize,
    next_start: usize,
}

fn index_lines(contents: &str) -> Vec<LineSpan> {
    let bytes = contents.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    while start <= bytes.len() {
        let rel_nl = bytes[start..].iter().position(|&b| b == b'\n');
        match rel_nl {
            Some(rel) => {
                lines.push(LineSpan {
                    start,
                    content_end: start + rel,
                    next_start: start + rel + 1,
                });
                start += rel + 1;
            }
            None => {
                // Final line without a terminator (possibly empty only when
                // the file is empty; a trailing "\n" yields this empty tail
                // line too, which is a real, matchable empty last line).
                lines.push(LineSpan {
                    start,
                    content_end: bytes.len(),
                    next_start: bytes.len(),
                });
                break;
            }
        }
    }
    lines
}

/// Split a pattern into comparison lines. A trailing `\n` means "the match
/// consumes the last matched line's terminator too"; we track that separately
/// instead of keeping a phantom empty line.
fn pattern_lines(pattern: &str) -> (Vec<&str>, bool) {
    let mut lines: Vec<&str> = pattern.split('\n').collect();
    let trailing_newline = lines.len() > 1 && lines.last() == Some(&"");
    if trailing_newline {
        lines.pop();
    }
    (lines, trailing_newline)
}

fn leading_ws(s: &str) -> &str {
    &s[..s.len() - s.trim_start().len()]
}

/// The uniform indent transformation between pattern and file.
#[derive(Debug, Clone, PartialEq, Eq)]
enum IndentDelta {
    Added(String),
    Removed(String),
}

impl IndentDelta {
    fn apply(&self, line: &str) -> String {
        if line.trim().is_empty() {
            // Never manufacture whitespace-only lines.
            return line.to_owned();
        }
        match self {
            IndentDelta::Added(prefix) => format!("{prefix}{line}"),
            IndentDelta::Removed(prefix) => line
                .strip_prefix(prefix.as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| line.to_owned()),
        }
    }

    fn describe(&self) -> String {
        match self {
            IndentDelta::Added(p) => format!("right by {:?}", p),
            IndentDelta::Removed(p) => format!("left by {:?}", p),
        }
    }
}

/// Does the window starting at `file_lines[at]` match the pattern under
/// trailing-whitespace tolerance?
fn window_matches_trailing_ws(
    contents: &str,
    file_lines: &[LineSpan],
    at: usize,
    pat: &[&str],
) -> bool {
    pat.iter().enumerate().all(|(j, pl)| {
        let fl = &contents[file_lines[at + j].start..file_lines[at + j].content_end];
        fl.trim_end() == pl.trim_end()
    })
}

/// Does the window match under a *uniform* indent shift? Returns the delta.
fn window_indent_delta(
    contents: &str,
    file_lines: &[LineSpan],
    at: usize,
    pat: &[&str],
) -> Option<IndentDelta> {
    let mut delta: Option<IndentDelta> = None;
    for (j, pl) in pat.iter().enumerate() {
        let fl = &contents[file_lines[at + j].start..file_lines[at + j].content_end];
        let (fl, pl) = (fl.trim_end(), pl.trim_end());
        if pl.trim().is_empty() {
            if !fl.trim().is_empty() {
                return None;
            }
            continue; // blank lines match regardless of indentation
        }
        if fl.trim_start() != pl.trim_start() {
            return None;
        }
        let (fi, pi) = (leading_ws(fl), leading_ws(pl));
        let line_delta = if fi.len() >= pi.len() {
            let prefix = fi.strip_suffix(pi)?;
            IndentDelta::Added(prefix.to_owned())
        } else {
            let prefix = pi.strip_suffix(fi)?;
            IndentDelta::Removed(prefix.to_owned())
        };
        match &delta {
            None => delta = Some(line_delta),
            Some(d) if *d == line_delta => {}
            Some(_) => return None, // non-uniform shift — refuse
        }
    }
    // A window of only blank lines has no computable delta; treat as no match
    // (stage 2 would have caught a genuinely identical blank window).
    delta.filter(|d| !matches!(d, IndentDelta::Added(p) | IndentDelta::Removed(p) if p.is_empty()))
}

/// End byte of a window match: the last line's content end, plus its
/// terminator when the pattern demanded a trailing newline.
fn window_end(file_lines: &[LineSpan], at: usize, pat_len: usize, trailing_newline: bool) -> usize {
    let last = &file_lines[at + pat_len - 1];
    if trailing_newline {
        last.next_start
    } else {
        last.content_end
    }
}

fn reindent(new_string: &str, delta: &IndentDelta) -> String {
    let (lines, trailing_newline) = pattern_lines(new_string);
    let mut out: Vec<String> = lines.iter().map(|l| delta.apply(l)).collect();
    if trailing_newline {
        out.push(String::new());
    }
    out.join("\n")
}

/// Find matches at the first stage that yields any. Returns `None` when all
/// three stages come up empty.
pub fn find_staged_matches(
    contents: &str,
    old_string: &str,
    new_string: &str,
) -> Option<StagedMatches> {
    // Stage 1: exact substring — authoritative, sub-line capable.
    let exact: Vec<MatchSpan> = contents
        .match_indices(old_string)
        .map(|(start, m)| MatchSpan {
            start,
            end: start + m.len(),
            replacement: new_string.to_owned(),
        })
        .collect();
    if !exact.is_empty() {
        // match_indices is non-overlapping-from-the-left already for our
        // purposes; keep only non-overlapping spans in order.
        let mut spans = Vec::new();
        let mut watermark = 0;
        for s in exact {
            if s.start >= watermark {
                watermark = s.end;
                spans.push(s);
            }
        }
        return Some(StagedMatches {
            stage: MatchStage::Exact,
            spans,
            indent_note: None,
        });
    }

    let (pat, trailing_newline) = pattern_lines(old_string);
    if pat.is_empty() {
        return None;
    }
    let file_lines = index_lines(contents);
    if file_lines.len() < pat.len() {
        return None;
    }
    let last_window = file_lines.len() - pat.len();

    // Stage 2: trailing-whitespace-insensitive, whole-line windows.
    let mut spans = Vec::new();
    let mut i = 0;
    while i <= last_window {
        if window_matches_trailing_ws(contents, &file_lines, i, &pat) {
            spans.push(MatchSpan {
                start: file_lines[i].start,
                end: window_end(&file_lines, i, pat.len(), trailing_newline),
                replacement: new_string.to_owned(),
            });
            i += pat.len();
        } else {
            i += 1;
        }
    }
    if !spans.is_empty() {
        return Some(StagedMatches {
            stage: MatchStage::TrailingWs,
            spans,
            indent_note: None,
        });
    }

    // Stage 3: uniform indent shift; new_string re-indented per match.
    let mut spans = Vec::new();
    let mut note = None;
    let mut i = 0;
    while i <= last_window {
        if let Some(delta) = window_indent_delta(contents, &file_lines, i, &pat) {
            note.get_or_insert_with(|| delta.describe());
            spans.push(MatchSpan {
                start: file_lines[i].start,
                end: window_end(&file_lines, i, pat.len(), trailing_newline),
                replacement: reindent(new_string, &delta),
            });
            i += pat.len();
        } else {
            i += 1;
        }
    }
    if !spans.is_empty() {
        return Some(StagedMatches {
            stage: MatchStage::IndentShift,
            spans,
            indent_note: note,
        });
    }

    None
}

/// Splice replacements into `contents`. `spans` must be ascending and
/// non-overlapping (guaranteed by `find_staged_matches`).
pub fn apply_spans(contents: &str, spans: &[MatchSpan]) -> String {
    let mut out = String::with_capacity(contents.len());
    let mut cursor = 0;
    for s in spans {
        out.push_str(&contents[cursor..s.start]);
        out.push_str(&s.replacement);
        cursor = s.end;
    }
    out.push_str(&contents[cursor..]);
    out
}

/// A one-line orientation hint for total misses: the first file line whose
/// trimmed content equals the trimmed first non-blank pattern line.
pub fn closest_line_hint(contents: &str, old_string: &str) -> Option<String> {
    let probe = old_string.lines().map(str::trim).find(|l| !l.is_empty())?;
    for (n, line) in contents.lines().enumerate() {
        if line.trim() == probe {
            let mut shown: String = line.trim().chars().take(80).collect();
            if line.trim().chars().count() > 80 {
                shown.push('…');
            }
            return Some(format!("line {} looks close: `{}`", n + 1, shown));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_wins_over_fuzzy() {
        // Both an exact and a would-be indent-shifted candidate exist; only
        // the exact one is reported.
        let contents = "fn a() {}\n    fn a() {}\n";
        let m = find_staged_matches(contents, "fn a() {}", "fn b() {}").unwrap();
        assert_eq!(m.stage, MatchStage::Exact);
        // match_indices finds the unindented and the indented occurrence's
        // tail — both byte-exact matches.
        assert!(!m.spans.is_empty());
    }

    #[test]
    fn trailing_ws_drift_matches() {
        let contents = "let x = 1;   \nlet y = 2;\t\n";
        let m = find_staged_matches(contents, "let x = 1;\nlet y = 2;", "Z").unwrap();
        assert_eq!(m.stage, MatchStage::TrailingWs);
        assert_eq!(m.spans.len(), 1);
        assert_eq!(apply_spans(contents, &m.spans), "Z\n");
    }

    #[test]
    fn crlf_drift_matches() {
        let contents = "alpha\r\nbeta\r\n";
        let m = find_staged_matches(contents, "alpha\nbeta", "gamma").unwrap();
        assert_eq!(m.stage, MatchStage::TrailingWs);
        // The matched window's own trailing `\r` is part of the replaced
        // region — the rewrite normalizes it away. Untouched lines keep
        // their terminators as-is.
        assert_eq!(apply_spans(contents, &m.spans), "gamma\n");
    }

    #[test]
    fn indent_shift_added_reindents_replacement() {
        let contents = "    if x {\n        go();\n    }\n";
        let m = find_staged_matches(contents, "if x {\n    go();\n}", "if y {\n    stop();\n}")
            .unwrap();
        assert_eq!(m.stage, MatchStage::IndentShift);
        assert_eq!(m.spans.len(), 1);
        assert_eq!(
            apply_spans(contents, &m.spans),
            "    if y {\n        stop();\n    }\n"
        );
        assert!(m.indent_note.unwrap().starts_with("right by"));
    }

    #[test]
    fn indent_shift_removed_strips_replacement() {
        let contents = "if x {\n    go();\n}\n";
        let m = find_staged_matches(
            contents,
            "    if x {\n        go();\n    }",
            "    if y {\n        stop();\n    }",
        )
        .unwrap();
        assert_eq!(m.stage, MatchStage::IndentShift);
        assert_eq!(apply_spans(contents, &m.spans), "if y {\n    stop();\n}\n");
    }

    #[test]
    fn non_uniform_shift_refused() {
        // Line 1 would need +4 and line 2 would need +8 — not one shift.
        let contents = "    a\n        b\n";
        assert!(find_staged_matches(contents, "a\nb", "x").is_none());
    }

    #[test]
    fn blank_lines_match_any_indent() {
        let contents = "    a\n\n    b\n";
        let m = find_staged_matches(contents, "a\n\nb", "c").unwrap();
        assert_eq!(m.stage, MatchStage::IndentShift);
        assert_eq!(apply_spans(contents, &m.spans), "    c\n");
    }

    #[test]
    fn fallback_ambiguity_reports_all_spans() {
        // Two indent-shifted candidates with *different* deltas — both must
        // be reported so the caller can refuse the ambiguous edit.
        let contents = "  a\n  b\nsep\n    a\n    b\n";
        let m = find_staged_matches(contents, "a\nb", "y").unwrap();
        assert_eq!(m.stage, MatchStage::IndentShift);
        assert_eq!(m.spans.len(), 2);
    }

    #[test]
    fn trailing_newline_in_pattern_consumes_terminator() {
        let contents = "keep\ndrop me  \nkeep2\n";
        let m = find_staged_matches(contents, "drop me\n", "").unwrap();
        assert_eq!(m.stage, MatchStage::TrailingWs);
        assert_eq!(apply_spans(contents, &m.spans), "keep\nkeep2\n");
    }

    #[test]
    fn sub_line_fuzzy_matching_is_refused() {
        // "bar" appears inside a longer line; fallback stages must not match
        // line fragments.
        let contents = "foobarbaz\n";
        assert!(find_staged_matches(contents, "bar ", "qux").is_none());
    }

    #[test]
    fn total_miss_returns_none_and_hint_finds_candidate() {
        let contents = "fn alpha() {\n    body();\n}\n";
        assert!(find_staged_matches(contents, "fn beta() {", "x").is_none());
        let hint = closest_line_hint(contents, "body();\nmore();").unwrap();
        assert!(hint.contains("line 2"), "got: {hint}");
        assert!(hint.contains("body();"));
    }

    #[test]
    fn hint_absent_when_nothing_close() {
        assert!(closest_line_hint("aaa\n", "zzz").is_none());
    }

    #[test]
    fn empty_file_matches_nothing() {
        assert!(find_staged_matches("", "x", "y").is_none());
    }
}
