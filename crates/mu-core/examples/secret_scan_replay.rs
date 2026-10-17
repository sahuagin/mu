//! mu-09z5y part 3, increment 3: replay `secret_scan` over session logs and
//! report how often each rule fires. This is the baseline for measuring the
//! scanner's progress between versions.
//!
//! ```text
//! cargo run -p mu-core --example secret_scan_replay -- [--rules FILE] [--show RULE [N]] PATH...
//! ```
//!
//! Each PATH is a session log or a directory walked for `*.jsonl`. Shell
//! commands are taken from Claude Code transcripts (`tool_use` blocks named
//! `Bash` or `Monitor`, `input.command`) and mu event logs (`tool_call`
//! events named `bash` or `watch`, `arguments.command`). Each unique command
//! is scanned once.
//!
//! Files or directory entries that can't be read count as `unreadable`,
//! files that fail partway as `partial`, and tool-call lines that aren't
//! valid JSON as `malformed_lines`, so an incomplete scan shows in the
//! report.
//!
//! The report on stdout is COUNTS ONLY, never command text, so it is safe to
//! keep and compare. `--show RULE [N]` prints up to N (default 20) unique
//! commands that RULE fired on, to stderr, for judging false positives.
//! Those commands may contain real secrets: it is an explicit, interactive
//! choice, and the output should not be kept.

use std::collections::{BTreeMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};

use mu_core::secret_scan::SecretScanner;
use serde_json::{json, Value};

struct Args {
    rules: Option<PathBuf>,
    show: Option<(String, usize)>,
    paths: Vec<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut rules = None;
    let mut show = None;
    let mut paths = Vec::new();
    let mut it = std::env::args().skip(1).peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--rules" => rules = Some(PathBuf::from(it.next().ok_or("--rules needs a file")?)),
            "--show" => {
                let rule = it.next().ok_or("--show needs a rule id")?;
                let n = match it.peek().and_then(|s| s.parse::<usize>().ok()) {
                    Some(n) => {
                        it.next();
                        n
                    }
                    None => 20,
                };
                show = Some((rule, n));
            }
            "-h" | "--help" => return Err(String::new()),
            _ => paths.push(PathBuf::from(a)),
        }
    }
    if paths.is_empty() {
        return Err("no PATH given".to_owned());
    }
    Ok(Args { rules, show, paths })
}

/// Shell commands in one log line, if any (both log formats).
/// Returns false when a line that looks like a tool call isn't valid JSON,
/// so the caller can count it: a truncated record shouldn't vanish quietly.
fn commands_in(line: &str, out: &mut Vec<String>) -> bool {
    // cheap prefilter before parsing JSON
    if !(line.contains("\"tool_use\"") || line.contains("\"tool_call\"")) {
        return true;
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    // Claude Code transcript: message.content[] tool_use blocks
    if let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) {
        for b in blocks {
            let is_shell = b.get("type").and_then(Value::as_str) == Some("tool_use")
                && matches!(
                    b.get("name").and_then(Value::as_str),
                    Some("Bash" | "Monitor")
                );
            if let Some(cmd) = is_shell
                .then(|| b.pointer("/input/command").and_then(Value::as_str))
                .flatten()
            {
                out.push(cmd.to_owned());
            }
        }
    }
    // mu event log: payload.kind == tool_call
    if v.pointer("/payload/kind").and_then(Value::as_str) == Some("tool_call")
        && matches!(
            v.pointer("/payload/name").and_then(Value::as_str),
            Some("bash" | "watch")
        )
    {
        if let Some(cmd) = v
            .pointer("/payload/arguments/command")
            .and_then(Value::as_str)
        {
            out.push(cmd.to_owned());
        }
    }
    true
}

fn walk(path: &Path, files: &mut Vec<PathBuf>, unreadable: &mut usize) {
    if path.is_file() {
        files.push(path.to_path_buf());
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        *unreadable += 1;
        return;
    };
    for e in entries {
        let Ok(e) = e else {
            *unreadable += 1;
            continue;
        };
        let p = e.path();
        // the entry's own type, not its target's: a symlinked directory
        // isn't followed, so a link cycle can't recurse forever
        let Ok(file_type) = e.file_type() else {
            *unreadable += 1;
            continue;
        };
        if file_type.is_dir() {
            walk(&p, files, unreadable);
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            files.push(p);
        }
    }
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("secret_scan_replay: {e}");
            }
            eprintln!("usage: secret_scan_replay [--rules FILE] [--show RULE [N]] PATH...");
            std::process::exit(2);
        }
    };
    let scanner = match SecretScanner::load(args.rules.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("secret_scan_replay: {e}");
            std::process::exit(1);
        }
    };

    let mut files = Vec::new();
    let mut unreadable = 0usize;
    let mut partial = 0usize;
    let mut malformed = 0usize;
    for p in &args.paths {
        walk(p, &mut files, &mut unreadable);
    }

    let mut calls = 0usize;
    let mut seen: HashSet<String> = HashSet::new();
    let mut with_hit = 0usize;
    let mut per_rule: BTreeMap<String, usize> = BTreeMap::new();
    let mut per_kind: BTreeMap<String, usize> = BTreeMap::new();
    let mut shown = 0usize;
    let mut buf = Vec::new();
    for f in &files {
        let Ok(file) = std::fs::File::open(f) else {
            unreadable += 1;
            continue;
        };
        // Read raw lines: invalid UTF-8 is converted lossily rather than
        // ending the file, and a read error marks the file `partial` in the
        // report instead of silently dropping the rest of it.
        let mut reader = std::io::BufReader::new(file);
        let mut raw = Vec::new();
        loop {
            raw.clear();
            match reader.read_until(b'\n', &mut raw) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => {
                    partial += 1;
                    break;
                }
            }
            let line = String::from_utf8_lossy(&raw);
            buf.clear();
            if !commands_in(&line, &mut buf) {
                malformed += 1;
            }
            for cmd in buf.drain(..) {
                calls += 1;
                if !seen.insert(cmd.clone()) {
                    continue;
                }
                let hits = scanner.scan(&cmd);
                if hits.is_empty() {
                    continue;
                }
                with_hit += 1;
                let mut kinds = HashSet::new();
                for h in &hits {
                    *per_rule.entry(h.rule.clone()).or_default() += 1;
                    kinds.insert(format!("{:?}", h.kind));
                }
                for k in kinds {
                    *per_kind.entry(k).or_default() += 1;
                }
                if let Some((rule, n)) = &args.show {
                    if shown < *n && hits.iter().any(|h| &h.rule == rule) {
                        shown += 1;
                        eprintln!("--- {rule} #{shown}\n{cmd}");
                    }
                }
            }
        }
    }
    let unique = seen.len();
    let report = json!({
        "files": files.len(),
        "unreadable": unreadable,
        "partial": partial,
        "malformed_lines": malformed,
        "shell_calls": calls,
        "unique_commands": unique,
        "unique_with_any_hit": with_hit,
        "hit_rate": if unique == 0 { 0.0 } else { with_hit as f64 / unique as f64 },
        "unique_hits_per_rule": per_rule,
        "unique_hits_per_kind": per_kind,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("serialize report")
    );
}
