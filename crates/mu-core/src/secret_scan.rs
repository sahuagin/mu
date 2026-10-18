//! mu-09z5y part 3: a best-effort scanner for credentials in command text.
//! Design: `specs/architecture/command-text-redaction.md`. The daemon
//! [`install`]s one at startup and tags each recorded `ToolCall` with
//! [`SecretScanner::tag_tool_call`].
//!
//! It answers ONE question, "does this text look like it carries a
//! credential, and by which rules?", and returns rule hits, NEVER the
//! matched text. What to do with a hit (withhold the whole field in a copy
//! of the log) is the caller's business. The primary log is never edited.
//!
//! Progress, not perfection: a miss is possible, and the archive replay
//! measures how many. False positives cost little (something gets withheld
//! that didn't need to be). Detectors, cheapest first:
//!
//! - **secret-named assignments:** `NAME=value`, `export NAME=value` where
//!   NAME passes [`is_secret_name`];
//! - **auth flags and headers:** `-H 'Authorization: …'`, `curl -u user:pw`,
//!   `--password=…`, `--token …`, `mysql -p…`;
//! - **URLs, parsed rather than pattern-matched** (the `url` crate): userinfo,
//!   secret-named query or fragment keys (decoded, so `%74oken` is `token`),
//!   and decoded parameter values that are themselves URLs (depth-capped);
//! - **known credential formats:** the rules file (`config/secret_scan.default.toml`
//!   plus an optional operator layer), run over the text and over each
//!   URL's percent-decoded form;
//! - **high entropy:** long mixed-case random-looking tokens, lower confidence.
//!
//! A value that is a variable reference (`$X`, `${X}`, `$(…)`) is not a
//! literal secret and doesn't count.
//!
//! The names these detectors look for (secret words, auth parts, file-holding
//! suffixes, the client commands whose `-u`/`-p` carry passwords) are config:
//! the rules file's `[names]` section, which an operator layer can replace.
//!
//! ## Known gaps
//!
//! The assignment and flag detectors read shell syntax with regexes, which
//! approximate the grammar. For ASSIGNMENTS, review found and fixed many
//! corners: quoting, concatenated pieces, ANSI-C `$'…'`, `${X:-fallback}`,
//! `$(…)literal`. The FLAG detector (`--password=…`, `--token …`) only looks
//! at the first character of the value, so a concatenated or ANSI-C value
//! (`--password=""hunter2`, `--password="$P"hunter2`, `--password=$'hunter2'`)
//! is missed. Other remaining gaps:
//! - nested command substitutions (`$(a $(b))`) and arithmetic `$((…))`;
//! - escaped quotes inside values (`'it'\''s'`);
//! - array assignments (`X=(a b)`) and assignments built from variable
//!   names;
//! - a lowercase secret-named keyword argument in code (`f(x, password=v)`,
//!   inside a call opened on the same line) whose value isn't a quoted
//!   literal is skipped, because most are not secrets;
//! - a secret passed some other way (stdin, a heredoc, a file) in a shape
//!   the known-format and entropy rules don't recognize.
//!
//! The follow-up is to read assignments and flags from a real shell parse
//! (the tree-sitter decomposer in mu-gj0fd; tracked as mu-2fh93) instead of
//! regexes. Until then,
//! the archive replay is how misses get measured.

use std::collections::BTreeSet;
use std::path::Path;

use regex::Regex;
use serde::Deserialize;

/// A rule that fired. Carries no part of the scanned text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Hit {
    /// Stable rule id, e.g. `secret-assignment`, `url-userinfo`, `github-token`.
    pub rule: String,
    pub kind: HitKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HitKind {
    Assignment,
    AuthFlag,
    UrlCredential,
    KnownFormat,
    HighEntropy,
}

/// Secret-pattern check on a NAME (an environment variable, a query or
/// fragment key, a flag), using the `[names]` lists of the rules file. Errs toward "secret": a false
/// positive only withholds something, a false negative leaks it.
///
/// It uses the [`active`] scanner's lists: the operator-layered ones once
/// the daemon has [`install`]ed its scanner, the built-in ones before
/// that. Callers are bash's strict-mode environment and the exec-context
/// snapshot.
pub fn is_secret_name(name: &str) -> bool {
    active().names.is_secret(name)
}

static INSTALLED: std::sync::OnceLock<SecretScanner> = std::sync::OnceLock::new();

/// Make `scanner` (normally [`SecretScanner::load_default`]) the process's
/// scanner, used from then on by [`active`] and [`is_secret_name`]. The
/// daemon calls it once at startup, before any session records anything.
/// Returns false, keeping the first, if a scanner was already installed:
/// the rules don't change under a running process.
#[must_use]
pub fn install(scanner: SecretScanner) -> bool {
    INSTALLED.set(scanner).is_ok()
}

/// The installed scanner, or the built-in rules when none was installed
/// (tests, tools that never start a daemon). The fallback is logged once
/// per process, so a path that should have installed the operator's rules
/// and didn't is visible.
pub fn active() -> &'static SecretScanner {
    INSTALLED.get().unwrap_or_else(|| {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            tracing::warn!(
                "secret_scan: no scanner installed in this process; using the built-in rules \
                 only (an operator secret_scan.toml is not applied)"
            );
        });
        built_in_scanner()
    })
}

fn built_in_scanner() -> &'static SecretScanner {
    static BUILT_IN: std::sync::OnceLock<SecretScanner> = std::sync::OnceLock::new();
    BUILT_IN.get_or_init(|| {
        SecretScanner::from_layers(&[("built-in", BUILT_IN_RULES)])
            .expect("built-in secret_scan rules must compile")
    })
}

/// The name lists the detectors use, from the rules file's `[names]`
/// section (no compiled defaults; invariant 6).
///
/// A name is secret if it IS, or ENDS IN `_`+, one of `secret_words`; if one
/// of its `_`-separated parts is in `auth_parts` or ends in one of
/// `auth_part_suffixes`; or if it is one of `known_secret_names`.
/// Case-insensitive; `-` counts as `_`. A secret-named variable holds a FILE
/// location when its name ends in one of `file_suffixes`.
#[derive(Debug, Clone)]
struct Names {
    secret_words: Vec<String>,
    auth_parts: Vec<String>,
    auth_part_suffixes: Vec<String>,
    known_secret_names: Vec<String>,
    file_suffixes: Vec<String>,
}

impl Names {
    fn normalize(name: &str) -> String {
        name.to_ascii_uppercase().replace('-', "_")
    }

    fn is_secret(&self, name: &str) -> bool {
        let upper = Self::normalize(name);
        self.secret_words
            .iter()
            .any(|w| upper == *w || upper.ends_with(&format!("_{w}")))
            || upper.split('_').any(|part| {
                self.auth_parts.iter().any(|a| a == part)
                    || self
                        .auth_part_suffixes
                        .iter()
                        .any(|s| part.ends_with(s.as_str()))
            })
            || self.known_secret_names.contains(&upper)
    }

    /// Whether a secret-named variable is one that holds a FILE location
    /// rather than the secret itself, so a path value names where a secret is
    /// stored: `MU_SSH_KEY=$HOME/.ssh/id` was 79 of 97 assignment hits in the
    /// archive replay. A POSITIVE list of suffixes, so every other secret
    /// name (`TOKEN`, `API_KEY`, `SSH_PASSWORD`, ...) keeps its path-looking
    /// values: `TOKEN='/abc/def'` counts.
    fn names_a_file(&self, name: &str) -> bool {
        // a leading `_` lets a suffix like `_SSH_KEY` match `SSH_KEY` itself
        let upper = format!("_{}", Self::normalize(name));
        self.file_suffixes
            .iter()
            .any(|suffix| upper.ends_with(suffix.as_str()))
    }
}

// ---- rules config ----------------------------------------------------------

const BUILT_IN_RULES: &str = include_str!("../config/secret_scan.default.toml");

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RulesFile {
    #[serde(default)]
    format: Vec<FormatRuleSpec>,
    #[serde(default)]
    entropy: Option<EntropySpec>,
    #[serde(default)]
    names: Option<NamesSpec>,
    #[serde(default)]
    tagging: Option<TaggingSpec>,
}

/// `[tagging]`: which tool calls get a `sensitive` tag when they're
/// recorded. A later layer's list replaces the earlier one.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaggingSpec {
    tools: Option<Vec<String>>,
}

/// `[names]`: every field optional per layer; a later layer's list REPLACES
/// the earlier one. The built-in layer must set them all.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct NamesSpec {
    secret_words: Option<Vec<String>>,
    auth_parts: Option<Vec<String>>,
    auth_part_suffixes: Option<Vec<String>>,
    known_secret_names: Option<Vec<String>>,
    file_suffixes: Option<Vec<String>>,
    http_clients: Option<Vec<String>>,
    mysql_clients: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FormatRuleSpec {
    id: String,
    #[serde(default)]
    pattern: Option<String>,
    #[serde(default = "yes")]
    enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct EntropySpec {
    enabled: Option<bool>,
    min_len: Option<usize>,
    min_run: Option<usize>,
    min_bits_per_char: Option<f64>,
    /// Known random-but-not-secret ID formats (tool-call and message ids),
    /// removed before the entropy check. A later layer's list REPLACES the
    /// earlier one.
    ignore: Option<Vec<String>>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone)]
struct Entropy {
    enabled: bool,
    min_len: usize,
    min_run: usize,
    min_bits_per_char: f64,
    ignore: Vec<Regex>,
}

/// A rules file that can't be used, and why: the scanner refuses to start
/// on a bad operator rule rather than silently dropping it (invariant 7).
#[derive(Debug, thiserror::Error)]
pub enum RulesError {
    #[error("secret_scan rules {source_name}: cannot read: {err}")]
    Read {
        source_name: String,
        err: std::io::Error,
    },
    #[error("secret_scan rules {source_name}: not valid TOML for the rules schema: {err}")]
    Parse {
        source_name: String,
        err: toml::de::Error,
    },
    #[error("secret_scan rules {source_name}: rule `{id}` has an invalid pattern: {err}")]
    Pattern {
        source_name: String,
        id: String,
        err: regex::Error,
    },
    #[error("secret_scan rules {source_name}: rule `{id}` adds a new rule without a pattern")]
    MissingPattern { source_name: String, id: String },
    #[error("secret_scan rules {source_name}: [tagging] tools: {problem}")]
    Tagging {
        source_name: String,
        problem: String,
    },
    #[error("secret_scan rules {source_name}: [names] {field}: {problem}")]
    Names {
        source_name: String,
        field: &'static str,
        problem: String,
    },
    #[error(
        "secret_scan rules {source_name}: [entropy] {field} = {value} is out of range ({expected})"
    )]
    Entropy {
        source_name: String,
        field: &'static str,
        value: String,
        expected: String,
    },
}

/// The scanner: compiled rules, ready to run.
#[derive(Debug, Clone)]
pub struct SecretScanner {
    formats: Vec<(String, Regex)>,
    entropy: Entropy,
    names: Names,
    auth_flags: Vec<(&'static str, Regex)>,
    tag_tools: Vec<String>,
}

impl SecretScanner {
    /// The built-in rules only.
    pub fn built_in() -> Self {
        built_in_scanner().clone()
    }

    /// [`is_secret_name`] with this scanner's (possibly operator-layered)
    /// name lists.
    pub fn is_secret_name(&self, name: &str) -> bool {
        self.names.is_secret(name)
    }

    /// [`SecretScanner::load`] with the operator file at its standard
    /// place, `<config dir>/mu/secret_scan.toml` (`~/.config/mu` on Linux
    /// and FreeBSD, the same directory as `config.toml`).
    pub fn load_default() -> Result<Self, RulesError> {
        let path = dirs::config_dir().map(|dir| dir.join("mu").join("secret_scan.toml"));
        Self::load(path.as_deref())
    }

    /// The rule ids that fire on a tool call's arguments, sorted and
    /// deduplicated, for the `sensitive` field of its `ToolCall` event.
    /// Empty when nothing fires or when `tool` isn't on the rules file's
    /// `[tagging] tools` list. Every string in the arguments is scanned,
    /// not only `command`. Never returns any part of the text.
    pub fn tag_tool_call(&self, tool: &str, arguments: &serde_json::Value) -> Vec<String> {
        if !self.tag_tools.iter().any(|t| t == tool) {
            return Vec::new();
        }
        fn strings<'a>(v: &'a serde_json::Value, out: &mut Vec<&'a str>) {
            match v {
                serde_json::Value::String(s) => out.push(s),
                serde_json::Value::Array(items) => items.iter().for_each(|i| strings(i, out)),
                serde_json::Value::Object(map) => map.values().for_each(|i| strings(i, out)),
                _ => {}
            }
        }
        let mut texts = Vec::new();
        strings(arguments, &mut texts);
        let rules: BTreeSet<String> = texts
            .into_iter()
            .flat_map(|t| self.scan(t))
            .map(|hit| hit.rule)
            .collect();
        rules.into_iter().collect()
    }

    /// Built-in rules, then the operator file at `operator` if it exists:
    /// a `[[format]]` with an existing id replaces it (or disables it with
    /// `enabled = false`), a new id adds a rule, and `[entropy]` fields
    /// override. A missing file is fine; an unreadable or invalid one is an
    /// error.
    pub fn load(operator: Option<&Path>) -> Result<Self, RulesError> {
        let Some(path) = operator else {
            return Ok(Self::built_in());
        };
        let name = path.display().to_string();
        // Only "not found" means there is no operator layer. Any other
        // error (permission denied, a directory in the way) is reported:
        // `Path::exists` would have turned those into a silent fallback.
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::built_in()),
            Err(err) => {
                return Err(RulesError::Read {
                    source_name: name,
                    err,
                })
            }
        };
        Self::from_layers(&[("built-in", BUILT_IN_RULES), (&name, &text)])
    }

    fn from_layers(layers: &[(&str, &str)]) -> Result<Self, RulesError> {
        let mut specs: Vec<(String, FormatRuleSpec)> = Vec::new();
        // No compiled defaults: the built-in TOML (always the first layer)
        // must set every [entropy] field, so the file is the one source.
        let mut entropy_enabled: Option<bool> = None;
        let mut entropy_min_len: Option<usize> = None;
        let mut entropy_min_run: Option<usize> = None;
        let mut entropy_ignore: Vec<Regex> = Vec::new();
        let mut entropy_bits: Option<f64> = None;
        let mut names: [Option<(String, Vec<String>)>; 7] = Default::default();
        let mut tag_tools: Option<(String, Vec<String>)> = None;
        for (source_name, text) in layers {
            let file: RulesFile = toml::from_str(text).map_err(|err| RulesError::Parse {
                source_name: (*source_name).to_owned(),
                err,
            })?;
            for rule in file.format {
                match specs.iter_mut().find(|(_, s)| s.id == rule.id) {
                    Some(existing) => {
                        let pattern = rule.pattern.clone().or(existing.1.pattern.clone());
                        *existing = (
                            (*source_name).to_owned(),
                            FormatRuleSpec { pattern, ..rule },
                        );
                    }
                    None => specs.push(((*source_name).to_owned(), rule)),
                }
            }
            if let Some(e) = file.entropy {
                // A threshold that can never be met (NaN, infinity, below 0,
                // or above what a token from the entropy alphabet can carry:
                // log2 of its size) or a zero length would quietly disable or
                // distort a detector that still claims to be on.
                if let Some(bits) = e.min_bits_per_char {
                    let ceiling = max_bits_per_char();
                    if !(bits.is_finite() && (0.0..=ceiling).contains(&bits)) {
                        return Err(RulesError::Entropy {
                            source_name: (*source_name).to_owned(),
                            field: "min_bits_per_char",
                            value: bits.to_string(),
                            expected: format!(
                                "a finite number from 0 to {ceiling:.3}, the most a token \
                                 from the {ENTROPY_ALPHABET_SIZE}-character alphabet can carry"
                            ),
                        });
                    }
                }
                for (field, value) in [("min_len", e.min_len), ("min_run", e.min_run)] {
                    if value == Some(0) {
                        return Err(RulesError::Entropy {
                            source_name: (*source_name).to_owned(),
                            field,
                            value: "0".to_owned(),
                            expected: "at least 1".to_owned(),
                        });
                    }
                }
                entropy_enabled = e.enabled.or(entropy_enabled);
                entropy_min_len = e.min_len.or(entropy_min_len);
                entropy_min_run = e.min_run.or(entropy_min_run);
                if let Some(list) = e.ignore {
                    entropy_ignore = list
                        .iter()
                        .map(|pattern| {
                            Regex::new(pattern).map_err(|err| RulesError::Pattern {
                                source_name: (*source_name).to_owned(),
                                id: "entropy.ignore".to_owned(),
                                err,
                            })
                        })
                        .collect::<Result<_, _>>()?;
                }
                entropy_bits = e.min_bits_per_char.or(entropy_bits);
            }
            if let Some(list) = file.tagging.and_then(|t| t.tools) {
                tag_tools = Some(((*source_name).to_owned(), list));
            }
            if let Some(n) = file.names {
                let fields = [
                    n.secret_words,
                    n.auth_parts,
                    n.auth_part_suffixes,
                    n.known_secret_names,
                    n.file_suffixes,
                    n.http_clients,
                    n.mysql_clients,
                ];
                for (slot, list) in names.iter_mut().zip(fields) {
                    if let Some(list) = list {
                        *slot = Some(((*source_name).to_owned(), list));
                    }
                }
            }
        }
        let [secret_words, auth_parts, auth_part_suffixes, known_secret_names, file_suffixes, http_clients, mysql_clients] =
            names;
        let secret_words = name_list(secret_words, "secret_words", true, false)?;
        let auth_parts = name_list(auth_parts, "auth_parts", false, false)?;
        let auth_part_suffixes = name_list(auth_part_suffixes, "auth_part_suffixes", false, false)?;
        let known_secret_names = name_list(known_secret_names, "known_secret_names", false, false)?;
        let file_suffixes = name_list(file_suffixes, "file_suffixes", false, false)?
            .into_iter()
            .map(|s| {
                if s.starts_with('_') {
                    s
                } else {
                    format!("_{s}")
                }
            })
            .collect();
        // client names stay as written (they are command names, not
        // variables), and an empty list would silently turn a detector off
        // where the client lists came from, for a compile error below
        let clients_source = [&http_clients, &mysql_clients]
            .iter()
            .filter_map(|slot| slot.as_ref().map(|(source, _)| source.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        let http_clients = name_list(http_clients, "http_clients", true, true)?;
        let mysql_clients = name_list(mysql_clients, "mysql_clients", true, true)?;
        let names = Names {
            secret_words,
            auth_parts,
            auth_part_suffixes,
            known_secret_names,
            file_suffixes,
        };
        let auth_flags = build_auth_flags(&http_clients, &mysql_clients).map_err(|(id, err)| {
            RulesError::Pattern {
                source_name: format!("{clients_source} ([names] client lists)"),
                id: id.to_owned(),
                err,
            }
        })?;
        let (Some(enabled), Some(min_len), Some(min_run), Some(min_bits_per_char)) = (
            entropy_enabled,
            entropy_min_len,
            entropy_min_run,
            entropy_bits,
        ) else {
            return Err(RulesError::Entropy {
                source_name: "built-in".to_owned(),
                field: "enabled/min_len/min_run/min_bits_per_char",
                value: "unset".to_owned(),
                expected: "all four set in config/secret_scan.default.toml".to_owned(),
            });
        };
        let entropy = Entropy {
            enabled,
            min_len,
            min_run,
            min_bits_per_char,
            ignore: entropy_ignore,
        };
        let mut formats = Vec::new();
        for (source_name, spec) in specs.into_iter().filter(|(_, s)| s.enabled) {
            let pattern = spec.pattern.ok_or_else(|| RulesError::MissingPattern {
                source_name: source_name.clone(),
                id: spec.id.clone(),
            })?;
            let re = Regex::new(&pattern).map_err(|err| RulesError::Pattern {
                source_name: source_name.clone(),
                id: spec.id.clone(),
                err,
            })?;
            formats.push((spec.id, re));
        }
        // no compiled default; an empty list would turn tagging off without
        // a word, and a blank entry names no tool
        let tag_tools = match tag_tools {
            None => {
                return Err(RulesError::Tagging {
                    source_name: "built-in".to_owned(),
                    problem: "missing (the built-in rules must set it)".to_owned(),
                })
            }
            Some((source_name, list))
                if list.is_empty() || list.iter().any(|t| t.trim().is_empty()) =>
            {
                return Err(RulesError::Tagging {
                    source_name,
                    problem: "empty, or a blank entry (it would turn tagging off)".to_owned(),
                })
            }
            Some((_, list)) => list,
        };
        Ok(Self {
            formats,
            entropy,
            names,
            auth_flags,
            tag_tools,
        })
    }

    /// Every rule that fires on `text`, deduplicated and sorted. Never the
    /// matched text.
    pub fn scan(&self, text: &str) -> Vec<Hit> {
        let mut hits = BTreeSet::new();
        self.scan_assignments_at(text, 0, &mut hits);
        // `;&|` and newlines inside quotes, and backslash-newline
        // continuations, aren't command boundaries: without this,
        // `curl 'https://h/?a=1&b=2' -u bob:pw` ended the curl command at `&`
        let masked = mask_quoted_boundaries(text);
        for (id, re) in &self.auth_flags {
            if re.is_match(&masked) {
                hits.insert(hit(id, HitKind::AuthFlag));
            }
        }
        for candidate in url_candidates(text) {
            self.scan_url(candidate, 0, &mut hits);
        }
        self.scan_formats(text, &mut hits);
        if self.entropy.enabled {
            scan_entropy(text, &self.entropy, &mut hits);
        }
        hits.into_iter().collect()
    }

    /// Assignments in `text`, and inside each `$(...)` in it (an outer
    /// `x=$(token=… cmd)` takes the whole substitution as its value, so the inner
    /// assignment would otherwise never be examined). Depth-capped.
    fn scan_assignments_at(&self, text: &str, depth: usize, hits: &mut BTreeSet<Hit>) {
        static SUBST: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        if depth < 3 {
            let subst =
                SUBST.get_or_init(|| Regex::new(r"\$\(([^)]*)\)").expect("substitution regex"));
            for inner in subst.captures_iter(text) {
                self.scan_assignments_at(&inner[1], depth + 1, hits);
            }
        }
        for c in assignment_re().captures_iter(text) {
            // Lowercase `key=` / `password=` in commands were mostly keyword
            // arguments in code inside heredocs (`sorted(rows, key=...)`), the
            // archive replay showed. Skip a lowercase name only in that
            // position, right after `(` or `,`, and only when the value
            // isn't a quoted string literal: `key=lambda r: …` and
            // `password=args.password` are skipped, `password="hunter2"` is
            // not. A lowercase shell variable (`export password=...`,
            // `token=x ./client`) still counts.
            let name = &c[1];
            let whole = c.get(0).expect("match");
            let name_start = whole.start() + whole.as_str().find(name).unwrap_or(0);
            // spaces and tabs only: a `,` ending the previous LINE
            // (`echo hello,\npassword=x ./client`) isn't an argument list
            let before = text[..name_start].trim_end_matches([' ', '\t']);
            // `f(` (an identifier right before the paren) or `,` is a call's
            // argument list; a `(` after whitespace, an operator, `$` or at the
            // start opens a subshell or command substitution, which is shell.
            let call_paren = before.strip_suffix('(').is_some_and(ends_in_identifier);
            // a `,` counts only inside such a call, still open on this line:
            // `f(x, password=…)`, not `env LABEL=hello, password=… ./client`
            let call_comma = before.ends_with(',') && in_open_call(before);
            let value = &c[2];
            // the value OPENS with a non-empty quoted string; what follows
            // the closing quote doesn't matter (the capture can include an
            // argument separator: `password="x", host=…` captures `"x",`)
            // (ANSI-C `$'…'` included)
            let quoted_literal = ['"', '\''].iter().any(|&q| {
                let value = value.strip_prefix("$'").map_or(value, |_| &value[1..]);
                value
                    .strip_prefix(q)
                    .and_then(|rest| rest.find(q))
                    .is_some_and(|close| close > 0)
            });
            let keyword_arg = !name.chars().any(|ch| ch.is_ascii_uppercase())
                && (call_paren || call_comma)
                && !quoted_literal;
            if !keyword_arg
                && self.names.is_secret(name)
                && has_literal(&c[2])
                && !(self.names.names_a_file(name) && is_path(&c[2]))
            {
                hits.insert(hit("secret-assignment", HitKind::Assignment));
            }
        }
    }

    fn scan_formats(&self, text: &str, hits: &mut BTreeSet<Hit>) {
        for (id, re) in &self.formats {
            if re.is_match(text) {
                hits.insert(hit(id, HitKind::KnownFormat));
            }
        }
    }

    fn scan_url(&self, candidate: &str, depth: usize, hits: &mut BTreeSet<Hit>) {
        // Known formats also run over the percent-decoded URL, so an encoded
        // webhook or token can't hide behind %XX.
        let decoded = percent_encoding::percent_decode_str(candidate).decode_utf8_lossy();
        if decoded != candidate {
            self.scan_formats(&decoded, hits);
        }
        let Ok(url) = url::Url::parse(candidate) else {
            // Unparseable but userinfo-shaped (`scheme://x:y@host`): count it.
            if let Some(rest) = candidate.split_once("://").map(|(_, r)| r) {
                let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
                if authority.contains('@')
                    && authority.split('@').next().is_some_and(|u| u.contains(':'))
                {
                    hits.insert(hit("url-userinfo", HitKind::UrlCredential));
                }
            }
            return;
        };
        let web = matches!(url.scheme(), "http" | "https" | "ftp" | "ftps");
        // a password is always a credential; a bare username is one on web
        // schemes (https://TOKEN@host), not on ssh (ssh://git@host)
        if url.password().is_some() || (web && !url.username().is_empty()) {
            hits.insert(hit("url-userinfo", HitKind::UrlCredential));
        }
        let mut pairs: Vec<(String, String)> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        if let Some(fragment) = url.fragment() {
            pairs.extend(
                url::form_urlencoded::parse(fragment.as_bytes())
                    .map(|(k, v)| (k.into_owned(), v.into_owned())),
            );
        }
        for (key, value) in pairs {
            if self.names.is_secret(&key) && !value.is_empty() {
                hits.insert(hit("url-secret-param", HitKind::UrlCredential));
            }
            if depth < 3 {
                for nested in url_candidates(&value) {
                    self.scan_url(nested, depth + 1, hits);
                }
            }
        }
    }
}

fn hit(rule: &str, kind: HitKind) -> Hit {
    Hit {
        rule: rule.to_owned(),
        kind,
    }
}

/// Whether an assignment's value (a whole shell word, possibly several
/// quoted and unquoted pieces the shell joins: `''hunter2`, `"$P"hunter2`)
/// puts any LITERAL text in the variable, as opposed to only references
/// (`$X`, `${X}`, `$(...)`, `` `...` ``) or nothing at all.
///
/// Single-quoted pieces, and bash's ANSI-C `$'...'`, are literal whatever
/// they contain, because single quotes stop expansion: `'$hunter2'` is a
/// password, not a variable. In double-quoted and unquoted pieces (and
/// locale `$"..."`), references are removed and anything left is literal.
fn has_literal(value: &str) -> bool {
    static PIECES: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static REFS: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let pieces = PIECES.get_or_init(|| {
        Regex::new(r#"\$'([^']*)'?|'([^']*)'?|\$?"([^"]*)"?|([^'"]+)"#).expect("pieces regex")
    });
    static FALLBACKS: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let fallbacks = FALLBACKS.get_or_init(|| {
        Regex::new(r#"\$\{[A-Za-z_][A-Za-z0-9_]*:?[-=+?](?P<operand>[^}]*)\}?"#)
            .expect("fallbacks regex")
    });
    let refs = REFS.get_or_init(|| {
        // unclosed forms too: the value regex stops at a space, so
        // `$(gh auth token)` arrives as `$(gh`
        Regex::new(
            r#"\$\{[^}]*\}?|\$\([^)]*\)?|`[^`]*`?|\$[A-Za-z_][A-Za-z0-9_]*|\$[0-9@*#?$!-]|\$"#,
        )
        .expect("refs regex")
    });
    pieces.captures_iter(value).any(|c| {
        if let Some(quoted) = c.get(1).or_else(|| c.get(2)) {
            return !quoted.as_str().is_empty();
        }
        let expanding = c.get(3).or_else(|| c.get(4)).map_or("", |m| m.as_str());
        // ${X:-fallback} (and :=, -, =, :+, +, :?, ?) puts the operand's
        // text in the command: keep the operand, drop the rest, then strip
        // references from what's left.
        let operands = fallbacks.replace_all(expanding, "$operand");
        !refs.replace_all(&operands, "").is_empty()
    })
}

fn assignment_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        // the value is the WHOLE shell word: any run of quoted pieces,
        // $(...) and ${...} (which may contain spaces or `)`), and unquoted
        // text, which the shell concatenates
        Regex::new(
            r#"(?:^|[\s;&|(])([A-Za-z_][A-Za-z0-9_]*)=((?:"[^"]*"|\$'[^']*'|'[^']*'|\$\([^)]*\)|\$\{[^}]*\}|[^\s;&|)'"])*)"#,
        )
            .expect("assignment regex")
    })
}

fn ends_in_identifier(text: &str) -> bool {
    text.chars()
        .last()
        .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Whether the end of `text` is inside a call's argument list opened on the
/// same line: walking back from the end, the first `(` not closed by a `)`
/// has an identifier right before it (`f(`, not a subshell's `(`).
/// Brackets inside quotes aren't told apart; like the rest of this module,
/// an approximation (mu-2fh93).
fn in_open_call(text: &str) -> bool {
    let line = text.rsplit('\n').next().unwrap_or(text);
    let mut depth = 0usize;
    for (i, ch) in line.char_indices().rev() {
        match ch {
            ')' => depth += 1,
            '(' if depth == 0 => return ends_in_identifier(&line[..i]),
            '(' => depth -= 1,
            _ => {}
        }
    }
    false
}

/// A filesystem path with at least two components (`~/.ssh/id`,
/// `/etc/gcp/sa.json`, `$HOME/.ssh/id`), quoted or not. One component
/// (`/hunter2`) is too little to tell a path from a value that starts with
/// a slash.
fn is_path(value: &str) -> bool {
    static LEADING_VAR: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let v = value.trim_matches(|c| c == '"' || c == '\'');
    // `$HOME/.ssh/id`, `${XDG_CONFIG_HOME}/x`: a path under a variable
    let v = LEADING_VAR
        .get_or_init(|| Regex::new(r"^\$\{?[A-Za-z_][A-Za-z0-9_]*\}?").expect("leading var regex"))
        .replace(v, "");
    let rest = ["~/", "../", "./", "/"]
        .iter()
        .find_map(|prefix| v.strip_prefix(prefix));
    !v.contains(char::is_whitespace) && rest.is_some_and(|r| r.trim_end_matches('/').contains('/'))
}

/// The auth-flag and auth-header patterns. The `-u`/`--user` and `-p`
/// patterns are scoped to the configured client commands (`[names]`
/// `http_clients`, `mysql_clients`): elsewhere those flags mean something
/// else (`date -u +%H:%M` looked like user:pw, 37 of 44 replay hits).
/// The client name may be quoted (`"curl"`) and must be followed by
/// whitespace, so the `http` of an `http://` URL isn't the HTTPie command.
/// Client names are escaped, but a large enough list can still exceed the
/// regex size limit, so a compile failure is returned, naming the rule.
fn build_auth_flags(
    http_clients: &[String],
    mysql_clients: &[String],
) -> Result<Vec<(&'static str, Regex)>, (&'static str, regex::Error)> {
    let alt = |names: &[String]| {
        names
            .iter()
            .map(|n| regex::escape(n))
            .collect::<Vec<_>>()
            .join("|")
    };
    [
        // an auth header given to curl/wget/httpie, or written inline
        (
            "auth-header",
            r#"(?i)(?:authorization|proxy-authorization)\s*:\s*(?:bearer|basic|token|digest)\s+[^\s'"$]"#.to_owned(),
        ),
        (
            "auth-header",
            r#"(?i)(?:^|\s)(?:-H|--header)[\s=]*['"]?\s*(?:cookie|x-api-key|x-auth-token|api-key|x-access-token)\s*:\s*[^\s'"$]"#.to_owned(),
        ),
        // curl -u user:pw / --user user:pw (a literal password)
        (
            "basic-auth-flag",
            format!(
                r#"(?:^|[\s;&|(])['"]?(?:\S*/)?(?:{})['"]?\s(?:[^;&|\n]*\s)?(?:-u|--user)[\s=]+(?:'[^'\s:]+:[^'\s]|"?[^\s:'"$]+:[^\s'"$])"#,
                alt(http_clients)
            ),
        ),
        // --password=x, --token x, --api-key x (not --token-file, not $VAR)
        (
            "secret-flag",
            r#"(?i)(?:^|\s)--?(?:password|passwd|pass|token|api-key|apikey|secret|access-token|auth-token|client-secret)(?:=|\s+)(?:'[^'\s]|"?[^\s'"$-])"#.to_owned(),
        ),
        // mysql/mariadb -pSECRET (attached; -p alone prompts)
        (
            "mysql-password",
            format!(
                r#"(?:^|[\s;&|(])['"]?(?:\S*/)?(?:{})['"]?\s(?:[^;&|\n]*\s)?-p(?:'[^'\s]|"[^"\s$]|[^\s'"$])"#,
                alt(mysql_clients)
            ),
        ),
    ]
    .into_iter()
    .map(|(id, p)| Regex::new(&p).map(|re| (id, re)).map_err(|err| (id, err)))
    .collect()
}

/// `text` with the shell command-boundary characters `;&|` and newlines
/// that sit inside single or double quotes, or are backslash-escaped
/// (`a=1\&b=2`), replaced by `_`, and each backslash-newline continuation
/// replaced by a space, so a pattern that stops
/// at a command boundary doesn't stop inside an argument. Regex-level
/// quoting, with the same limits as the rest of this module (mu-2fh93).
fn mask_quoted_boundaries(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut quote: Option<char> = None;
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            // an escape outside single quotes: keep `\"` from closing a
            // quote, turn a line continuation into a space, and make an
            // escaped `;&|` a literal
            (q, '\\') if q != Some('\'') => match chars.next() {
                Some('\n') => out.push(' '),
                Some(';' | '&' | '|') => {
                    out.push(ch);
                    out.push('_');
                }
                Some(next) => {
                    out.push(ch);
                    out.push(next);
                }
                None => out.push(ch),
            },
            (None, '\'' | '"') => {
                quote = Some(ch);
                out.push(ch);
            }
            (Some(q), _) if ch == q => {
                quote = None;
                out.push(ch);
            }
            (Some(_), ';' | '&' | '|' | '\n') => out.push('_'),
            _ => out.push(ch),
        }
    }
    out
}

/// A `[names]` list after layering. `required`: an empty list is an error
/// (it would silently turn a detector off); `keep_case`: command names are
/// matched as written, variable-name lists are uppercased with `-` as `_`.
fn name_list(
    slot: Option<(String, Vec<String>)>,
    field: &'static str,
    required: bool,
    keep_case: bool,
) -> Result<Vec<String>, RulesError> {
    let Some((source_name, list)) = slot else {
        return Err(RulesError::Names {
            source_name: "built-in".to_owned(),
            field,
            problem: "missing (the built-in rules must set every [names] list)".to_owned(),
        });
    };
    if let Some(bad) = list.iter().find(|n| n.trim().is_empty()) {
        return Err(RulesError::Names {
            source_name,
            field,
            problem: format!("blank entry {bad:?}"),
        });
    }
    if required && list.is_empty() {
        return Err(RulesError::Names {
            source_name,
            field,
            problem: "empty (it would turn its detector off)".to_owned(),
        });
    }
    Ok(list
        .into_iter()
        .map(|n| if keep_case { n } else { Names::normalize(&n) })
        .collect())
}

/// Every `scheme://…` in `text`. A candidate ends at whitespace, a quote,
/// or where the NEXT URL's scheme begins, so a list like `a://x,b://u:p@y`
/// yields both URLs; otherwise the first would swallow the second and its
/// credentials would never be examined.
fn url_candidates(text: &str) -> Vec<&str> {
    let starts: Vec<usize> = text
        .match_indices("://")
        .filter_map(|(i, _)| {
            let scheme_start = text[..i]
                .char_indices()
                .rev()
                .take_while(|(_, c)| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
                .last()
                .map(|(j, _)| j)?;
            text[scheme_start..i]
                .starts_with(|c: char| c.is_ascii_alphabetic())
                .then_some(scheme_start)
        })
        .collect();
    let mut out = Vec::new();
    for (n, &start) in starts.iter().enumerate() {
        let hard_end = text[start..]
            .find(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | '<' | '>' | '`'))
            .map_or(text.len(), |e| start + e);
        let end = starts
            .get(n + 1)
            .map_or(hard_end, |&next| next.min(hard_end));
        let candidate = text[start..end].trim_end_matches([',', ';', ')', '.']);
        if candidate.len() > candidate.find("://").unwrap_or(0) + 3 {
            out.push(candidate);
        }
    }
    out
}

/// The characters a high-entropy token is made of: ASCII letters and digits
/// plus these. Defined once, because the entropy ceiling is derived from it.
const ENTROPY_EXTRA_CHARS: &[char] = &['+', '/', '=', '_', '-'];
const ENTROPY_ALPHABET_SIZE: usize = 26 + 26 + 10 + ENTROPY_EXTRA_CHARS.len();

/// The highest Shannon entropy a token from that alphabet can have,
/// log2(alphabet size). A threshold above it could never fire.
fn max_bits_per_char() -> f64 {
    (ENTROPY_ALPHABET_SIZE as f64).log2()
}

fn scan_entropy(text: &str, rule: &Entropy, hits: &mut BTreeSet<Hit>) {
    let is_token_char = |c: char| c.is_ascii_alphanumeric() || ENTROPY_EXTRA_CHARS.contains(&c);
    // known random-but-not-secret ids are removed first
    let mut text = std::borrow::Cow::Borrowed(text);
    for re in &rule.ignore {
        if re.is_match(&text) {
            text = std::borrow::Cow::Owned(re.replace_all(&text, " ").into_owned());
        }
    }
    for token in text.split(|c: char| !is_token_char(c)) {
        if token.len() < rule.min_len {
            continue;
        }
        // The separators `/ - _ =` are in the alphabet (base64 and url-safe
        // keys use them), but they also join paths, kebab-case names and
        // `VAR=/path/<uuid>` into one long token that looks random. A real key
        // has a long unbroken run; a path doesn't.
        let longest_run = token
            .split(['/', '-', '_', '='])
            .map(str::len)
            .max()
            .unwrap_or(0);
        if longest_run < rule.min_run {
            continue;
        }
        let upper = token.chars().any(|c| c.is_ascii_uppercase());
        let lower = token.chars().any(|c| c.is_ascii_lowercase());
        let digit = token.chars().any(|c| c.is_ascii_digit());
        let hex = token.chars().all(|c| c.is_ascii_hexdigit());
        if upper && lower && digit && !hex && shannon_bits_per_char(token) >= rule.min_bits_per_char
        {
            hits.insert(hit("high-entropy", HitKind::HighEntropy));
            return;
        }
    }
}

fn shannon_bits_per_char(s: &str) -> f64 {
    let mut counts = std::collections::HashMap::new();
    for b in s.bytes() {
        *counts.entry(b).or_insert(0usize) += 1;
    }
    let n = s.len() as f64;
    counts
        .values()
        .map(|&c| {
            let p = c as f64 / n;
            -p * p.log2()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(text: &str) -> Vec<String> {
        SecretScanner::built_in()
            .scan(text)
            .into_iter()
            .map(|h| h.rule)
            .collect()
    }

    fn fires(text: &str, rule: &str) {
        let got = rules(text);
        assert!(
            got.iter().any(|r| r == rule),
            "expected `{rule}` on {text:?}, got {got:?}"
        );
    }

    fn clean(text: &str) {
        let got = rules(text);
        assert!(got.is_empty(), "expected no hits on {text:?}, got {got:?}");
    }

    #[test]
    fn built_in_rules_compile() {
        let s = SecretScanner::built_in();
        assert!(s.formats.len() >= 10, "{}", s.formats.len());
    }

    #[test]
    fn secret_names() {
        for n in [
            "GH_TOKEN",
            "api_key",
            "api-key",
            "OPENROUTER_API_KEY",
            "DB_PASSWORD",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "PROXY_AUTHORIZATION",
            "PROXY_AUTHORISATION",
            "GH_AUTH",
            "OAUTH_TOKEN",
            "BASICAUTH",
            "SESSION_COOKIE",
            "token",
            "access_token",
        ] {
            assert!(is_secret_name(n), "{n}");
        }
        for n in [
            "AWS_PROFILE",
            "AWS_REGION",
            "author",
            "AUTHOR_NAME",
            "PATH",
            "MONKEY",
            "KEYBOARD",
            "PASSAGE",
            "page",
            "sort",
            "TOKENIZER",
        ] {
            assert!(!is_secret_name(n), "{n}");
        }
    }

    #[test]
    fn assignments() {
        fires("export GH_TOKEN=ghx123abc", "secret-assignment");
        fires("OPENAI_API_KEY='abc' python run.py", "secret-assignment");
        fires("cd x && DB_PASSWORD=hunter2 ./migrate", "secret-assignment");
        // references are not literal secrets
        // single quotes stop expansion: these are literal passwords
        fires("DB_PASSWORD='$hunter2' ./migrate", "secret-assignment");
        fires("export API_KEY='`notacommand`'", "secret-assignment");
        fires("DB_PASSWORD=$'hunter2' ./migrate", "secret-assignment");
        fires("DB_PASSWORD=$\"hunter2\" ./migrate", "secret-assignment");
        // the shell joins adjacent pieces into one value
        fires("DB_PASSWORD=''hunter2 ./migrate", "secret-assignment");
        fires("DB_PASSWORD=\"\"hunter2 ./migrate", "secret-assignment");
        fires(
            "DB_PASSWORD=\"$PREFIX\"hunter2 ./migrate",
            "secret-assignment",
        );
        fires("DB_PASSWORD=$(true)hunter2 ./migrate", "secret-assignment");
        fires("DB_PASSWORD=${X}hunter2 ./migrate", "secret-assignment");
        clean("X_TOKEN=$(cat ~/.tok) make");
        clean("GH_TOKEN=\"$A\"\"$B\" make");
        clean("X_TOKEN=${A}$(b) make");
        // a literal fallback inside ${...} is in the command
        fires(
            "DB_PASSWORD=${DB_PASSWORD:-hunter2} ./migrate",
            "secret-assignment",
        );
        fires(
            "DB_PASSWORD=${DB_PASSWORD:=hunter2} ./migrate",
            "secret-assignment",
        );
        fires("API_KEY=${API_KEY-abc123} run", "secret-assignment");
        clean("DB_PASSWORD=${DB_PASSWORD:-$FALLBACK} ./migrate");
        clean("DB_PASSWORD=${DB_PASSWORD:-} ./migrate");
        clean("X_TOKEN=`cat tok` make");
        clean("export GH_TOKEN=$(gh auth token)");
        clean("export GH_TOKEN=''");
        clean("GH_TOKEN=\"$GH_TOKEN\" make deploy");
        clean("export PATH=/usr/bin:$PATH");
        // keyword arguments in code and AWS configuration are not secrets
        clean("python3 - <<'EOF'\nrows.sort(key=lambda r: r[1])\nEOF");
        clean("python3 -c 'f(x, password=args.pw)'");
        // but lowercase shell variables count
        fires("export password=hunter2", "secret-assignment");
        fires("token=abc123 ./client", "secret-assignment");
        // a subshell or command substitution is shell, not a call
        fires("(password=hunter2 ./client)", "secret-assignment");
        // a `,` ending the previous line isn't an argument list
        fires(
            "echo hello,\npassword=hunter2 ./client",
            "secret-assignment",
        );
        // a quoted literal in keyword-argument position is still a secret
        fires(
            r#"python3 -c 'connect(host="db", password="hunter2")'"#,
            "secret-assignment",
        );
        fires(
            r#"python3 -c 'connect(password="hunter2", host="db")'"#,
            "secret-assignment",
        );
        clean(r#"python3 -c 'connect(host="db", password=args.password)'"#);
        clean(r#"python3 -c 'connect(password=args.password, host="db")'"#);
        // a comma alone isn't a call: this is shell, passing a password
        fires(
            "env LABEL=hello, password=hunter2 ./client",
            "secret-assignment",
        );
        fires("(echo a, password=hunter2 ./client)", "secret-assignment");
        clean("sorted(rows, key=lambda r: r[0])");
        clean("f(g(x), key=item.name)");
        fires("f(x, password=$'hunter2')", "secret-assignment");
        fires("x=$(token=abc123 ./login)", "secret-assignment");
        fires("SSH_PASSWORD='/abc/def' ./login", "secret-assignment");
        fires("CERT_PASSWORD=/etc/x/y ./sign", "secret-assignment");
        // only file-holding names get the path exemption
        fires("TOKEN='/abc/def' ./run", "secret-assignment");
        fires("API_KEY=/x/y ./run", "secret-assignment");
        clean("SSL_CERT_FILE=/etc/ssl/cert.pem curl https://x");
        clean("AWS_PROFILE=sandbox-admin terraform plan");
        // a path to a key file is not the key
        clean("MU_SSH_KEY=~/.ssh/id_ed25519 ./deploy.sh");
        clean("MU_SSH_KEY=\"$HOME/.ssh/id_ed25519\" ./deploy.sh");
        clean("MU_SSH_KEY=${HOME}/.ssh/id_ed25519 ./deploy.sh");
        clean("GOOGLE_APPLICATION_CREDENTIALS=/etc/gcp/sa.json gcloud auth");
        // a slash-led password is still a password
        fires("PASSWORD='/hunter2' ./login", "secret-assignment");
        fires("DB_PASSWORD=/srv/not/a/path ./migrate", "secret-assignment");
        fires("API_KEY=/x ./run", "secret-assignment");
    }

    #[test]
    fn auth_flags_and_headers() {
        fires(
            r#"curl -H "Authorization: Bearer abc.def" https://api.x/y"#,
            "auth-header",
        );
        fires("curl -H 'X-Api-Key: 12345' https://api.x/y", "auth-header");
        fires("curl -u bob:hunter2 https://x/y", "basic-auth-flag");
        // a quoted `&`, `;` or `|` is part of an argument, and a continued
        // line is the same command
        for cmd in [
            "curl 'https://example.com/?a=1&b=2' -u bob:hunter2",
            "curl \"https://example.com/?a=1;b=2|c\" -u bob:hunter2",
            "curl https://example.com \\\n    -u bob:hunter2",
            "mysql -h 'db;x' -phunter2",
            "curl https://example.com/?a=1\\&b=2 -u bob:hunter2",
            "\"curl\" -u bob:hunter2 https://example.com",
            "'wget' --user a:hunter2 https://example.com",
            "http -a x https://example.com -u bob:hunter2",
            "$(mysql -uroot -phunter2 db)",
            "(mysql -uroot -phunter2 db)",
            "curl \"https://example.com/?a=\\\"1&b=2\" -u bob:hunter2",
        ] {
            assert!(!rules(cmd).is_empty(), "{cmd}");
        }
        // an unquoted `;` still ends the curl command
        clean("curl https://example.com; date -u +%H:%M");
        // the `http` of a URL scheme isn't the HTTPie command
        clean("echo http://x -u 1000:1000");
        clean("docker run -u 1000:1000 https://x");
        fires(
            "/usr/bin/curl -u alice:hunter2 https://example.com",
            "basic-auth-flag",
        );
        fires(
            "/usr/local/bin/mysql -u root -phunter2 db",
            "mysql-password",
        );
        fires("tool --password=hunter2", "secret-flag");
        fires("tool --token abc123", "secret-flag");
        fires("mysql -u root -phunter2 db", "mysql-password");
        fires("mysql -u root -p'hunter2' db", "mysql-password");
        fires("mysql -u root -p\"hunter2\" db", "mysql-password");
        clean("mysql -u root -p\"$PW\" db");
        fires("tool --password '$literal1'", "secret-flag");
        fires("curl -u 'bob:$pw' https://x/y", "basic-auth-flag");
        clean(r#"curl -H "Authorization: Bearer $TOKEN" https://api.x/y"#);
        clean("tool --token-file ~/.tok");
        clean("mkdir -p build && ssh -p 22 host");
        clean("date -u +%H:%M:%S && docker run -u 1000:1000 img");
        clean("mysql -u root -p db");
    }

    /// Every URL shape part 2's review rounds found.
    #[test]
    fn urls() {
        fires("curl http://bob:hunter2@proxy:3128/", "url-userinfo");
        fires(
            "git clone https://TOKEN123@github.com/o/r.git",
            "url-userinfo",
        );
        fires("x https://host1,https://user:secret@host2/", "url-userinfo");
        fires("curl 'https://h/p?api_key=sk123&x=1'", "url-secret-param");
        fires("curl 'https://h/?%74oken=secret'", "url-secret-param");
        fires(
            "open https://app/cb#access_token=ya29abc",
            "url-secret-param",
        );
        fires(
            "curl 'https://app/cb?next=https%3A%2F%2Fh%2F%3Ftoken%3Dxyz'",
            "url-secret-param",
        );
        fires("psql postgres://u:p@db.local/app", "url-userinfo");
        clean("git clone ssh://git@github.com/o/r.git");
        clean("curl https://example.com/page?sort=asc&page=2");
        clean("cargo install --git https://github.com/sahuagin/agent_tools --rev f3a3ab4e6e5c x");
    }

    #[test]
    fn known_formats() {
        fires(
            "curl -X POST https://hooks.slack.com/services/T0123ABC/B0456DEF/abcDEF123ghi",
            "slack-webhook",
        );
        fires(
            "aws configure set aws_access_key_id AKIAABCDEFGHIJKLMNOP",
            "aws-access-key-id",
        );
        fires(
            "git push https://x:ghp_abcdefghijklmnopqrstuvwxyzABCDEFGHIJ@github.com/o/r",
            "github-token",
        );
        fires("echo sk-ant-api03-abcdefghijklmnopqrstuvwx", "api-key-sk");
        fires(
            "echo -----BEGIN OPENSSH PRIVATE KEY----- > k",
            "private-key-block",
        );
        // an encoded webhook still matches, via the decoded URL
        fires(
            "curl 'https://proxy/fwd?u=https%3A%2F%2Fhooks.slack.com%2Fservices%2FT1%2FB2%2Fsecret9'",
            "slack-webhook",
        );
    }

    #[test]
    fn entropy() {
        fires("echo Zx9QmT4vR8kLp2WnY7sB3dFh6JcA1eGu", "high-entropy");
        // a path with a UUID in it is not a key, even when a variable name
        // supplies the upper case (the archive replay's dominant false hit)
        clean("S=/compat/linux/tmp/claude-1003/-home-claude/1887a4a3-6585-467f-bd62-6e042756296b/scratchpad; ls $S");
        // tool-call and message ids are random but not secrets
        clean("tail /tmp/x/tasks/toolu_01AbCdEfGhIjKlMnOpQrStUv.output");
        clean("grep msg_01XyZaBcDeFgHiJkLmNoPqRs log.jsonl");
        clean("open https://claude.ai/code/session_01Pd9eqwv7uoU8hSsuMnuqZR");
        // but a session token of another shape still counts
        fires(
            "echo session_Zx9QmT4vR8kLp2WnY7sB3dFh6JcA1eGu",
            "high-entropy",
        );
        // hex (git SHAs, checksums), lowercase ids and paths don't count
        clean("git show 3f2a9c1b7e4d6f8a0b2c4e6f8a1b3c5d7e9f0a2b");
        clean("sha256 = 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08");
        clean("ls /home/claude/src/public_github/mu-mu-09z5y/crates/mu-core/src");
    }

    #[test]
    fn ordinary_commands_are_clean() {
        for c in [
            "cargo test -p mu-core --lib secret_scan",
            "jj describe -m \"event log: record cwd\"",
            "git log --oneline -5 && git status",
            "rg -n 'fn main' crates/",
            "bot-gh pr view 761 -R sahuagin/mu --json state",
            "find . -name '*.rs' -newermt 2026-10-01 | wc -l",
        ] {
            clean(c);
        }
    }

    #[test]
    fn tag_tool_call_scans_every_string_of_a_tagged_tool() {
        let s = SecretScanner::built_in();
        // every string leaf, nested too, deduplicated and sorted
        let args = serde_json::json!({
            "command": "curl -u bob:hunter2 https://x/y",
            "env": { "extra": ["export GH_TOKEN=hunter2", "curl -u a:b https://z"] },
            "timeout": 30,
        });
        assert_eq!(
            s.tag_tool_call("bash", &args),
            vec!["basic-auth-flag", "secret-assignment"]
        );
        assert_eq!(
            s.tag_tool_call(
                "watch",
                &serde_json::json!({ "command": "mysql -phunter2" })
            ),
            vec!["mysql-password"]
        );
        // clean, or not a tagged tool
        assert!(s
            .tag_tool_call("bash", &serde_json::json!({ "command": "ls -la" }))
            .is_empty());
        assert!(s.tag_tool_call("write", &args).is_empty());
    }

    #[test]
    fn hits_never_contain_the_text() {
        let hits = SecretScanner::built_in().scan("export GH_TOKEN=supersecretvalue123");
        let dbg = format!("{hits:?}");
        assert!(!dbg.contains("supersecretvalue123"), "{dbg}");
    }

    #[test]
    fn operator_layer_adds_replaces_disables_and_fails_loud() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret_scan.toml");
        std::fs::write(
            &path,
            r#"
            [[format]]
            id = "internal-token"
            pattern = 'itk_[a-z0-9]{12}'
            [[format]]
            id = "jwt"
            enabled = false
            [entropy]
            enabled = false
            "#,
        )
        .expect("write");
        let s = SecretScanner::load(Some(&path)).expect("load");
        let ids = |t: &str| s.scan(t).into_iter().map(|h| h.rule).collect::<Vec<_>>();
        assert!(ids("x itk_abcdef123456").contains(&"internal-token".to_owned()));
        assert!(ids("eyJhbGciOiJIUzI1NiIs.eyJzdWIiOiIxMjM0NTY3.SflKxwRJSMeKKF2QT4fw").is_empty());
        assert!(ids("Zx9QmT4vR8kLp2WnY7sB3dFh6JcA1eGu").is_empty());
        // a bad pattern is an error naming the file and rule, not a silent drop
        std::fs::write(&path, "[[format]]\nid = \"bad\"\npattern = '('\n").expect("write");
        let err = SecretScanner::load(Some(&path)).expect_err("bad pattern");
        let msg = err.to_string();
        assert!(
            msg.contains("bad") && msg.contains("secret_scan.toml"),
            "{msg}"
        );
        // [names] lists replace the built-in ones
        std::fs::write(
            &path,
            "[names]\nsecret_words = [\"pin\"]\nhttp_clients = [\"xh\"]\nfile_suffixes = [\"LOC\"]\n",
        )
        .expect("write");
        let s = SecretScanner::load(Some(&path)).expect("load names");
        assert!(s.is_secret_name("BANK_PIN") && !s.is_secret_name("API_KEY_X"));
        assert!(!s.is_secret_name("API_TOKEN") && s.is_secret_name("MU_AUTH"));
        assert!(!s.scan("BANK_PIN=1234 ./pay").is_empty());
        assert!(
            s.scan("PIN_LOC=/etc/x/y ./pay").is_empty(),
            "suffix gets its `_`"
        );
        assert!(!s.scan("xh -u bob:hunter2 example.com").is_empty());
        assert!(s.scan("curl -u bob:hunter2 example.com").is_empty());
        // the free function stays on the built-in lists
        assert!(is_secret_name("API_TOKEN") && !is_secret_name("BANK_PIN"));
        // an empty list that would turn a detector off, or a blank entry, is
        // an error naming the list
        for (body, field) in [
            ("secret_words = []", "secret_words"),
            ("http_clients = []", "http_clients"),
            ("mysql_clients = []", "mysql_clients"),
            ("auth_parts = [\" \"]", "auth_parts"),
        ] {
            std::fs::write(&path, format!("[names]\n{body}\n")).expect("write");
            let err = SecretScanner::load(Some(&path)).expect_err(body);
            assert!(
                matches!(&err, RulesError::Names { field: f, .. } if *f == field),
                "{body}: {err}"
            );
        }
        // an empty optional list is allowed: no known names beyond the rules
        std::fs::write(&path, "[names]\nknown_secret_names = []\n").expect("write");
        assert!(!SecretScanner::load(Some(&path))
            .expect("empty known list")
            .is_secret_name("AWS_ACCESS_KEY_ID"));
        // a client list too big to compile is an error naming the rule, not
        // a panic
        let huge: Vec<String> = (0..200_000).map(|i| format!("\"client{i}\"")).collect();
        std::fs::write(
            &path,
            format!("[names]\nhttp_clients = [{}]\n", huge.join(",")),
        )
        .expect("write");
        let err = SecretScanner::load(Some(&path)).expect_err("oversized client list");
        assert!(
            matches!(&err, RulesError::Pattern { id, .. } if id == "basic-auth-flag"),
            "{err}"
        );
        // [tagging] tools replaces the built-in list; empty or blank is an
        // error, since it would turn tagging off without a word
        std::fs::write(&path, "[tagging]\ntools = [\"run\"]\n").expect("write");
        let s = SecretScanner::load(Some(&path)).expect("load tagging");
        let args = serde_json::json!({ "command": "export GH_TOKEN=hunter2" });
        assert_eq!(s.tag_tool_call("run", &args), vec!["secret-assignment"]);
        assert!(s.tag_tool_call("bash", &args).is_empty());
        for body in ["tools = []", "tools = [\"\"]"] {
            std::fs::write(&path, format!("[tagging]\n{body}\n")).expect("write");
            let err = SecretScanner::load(Some(&path)).expect_err(body);
            assert!(matches!(err, RulesError::Tagging { .. }), "{body}: {err}");
        }
        // a missing operator file is fine
        assert!(SecretScanner::load(Some(&dir.path().join("absent.toml"))).is_ok());
        // an entropy threshold that can never be met is an error, not a
        // detector that's silently off
        for bad in ["nan", "inf", "-1.0", "6.5"] {
            std::fs::write(&path, format!("[entropy]\nmin_bits_per_char = {bad}\n"))
                .expect("write");
            let err = SecretScanner::load(Some(&path)).expect_err(bad);
            assert!(
                matches!(
                    err,
                    RulesError::Entropy {
                        field: "min_bits_per_char",
                        ..
                    }
                ),
                "{bad}: {err}"
            );
        }
        // the ceiling is what the alphabet can carry (log2 67 ≈ 6.07), not 8
        std::fs::write(&path, "[entropy]\nmin_bits_per_char = 7.0\n").expect("write");
        assert!(matches!(
            SecretScanner::load(Some(&path)).expect_err("7.0 can never fire"),
            RulesError::Entropy {
                field: "min_bits_per_char",
                ..
            }
        ));
        std::fs::write(&path, "[entropy]\nmin_bits_per_char = 6.0\n").expect("write");
        assert!(SecretScanner::load(Some(&path)).is_ok(), "6.0 is reachable");
        std::fs::write(&path, "[entropy]\nmin_len = 0\n").expect("write");
        let err = SecretScanner::load(Some(&path)).expect_err("min_len 0");
        assert!(
            matches!(
                err,
                RulesError::Entropy {
                    field: "min_len",
                    ..
                }
            ),
            "{err}"
        );
        std::fs::write(&path, "[entropy]\nmin_run = 0\n").expect("write");
        let err = SecretScanner::load(Some(&path)).expect_err("min_run 0");
        assert!(
            matches!(
                err,
                RulesError::Entropy {
                    field: "min_run",
                    ..
                }
            ),
            "{err}"
        );
        std::fs::write(&path, "[entropy]\nmin_len = 0\n").expect("write");
        let err = SecretScanner::load(Some(&path)).expect_err("min_len 0");
        assert!(
            matches!(
                err,
                RulesError::Entropy {
                    field: "min_len",
                    ..
                }
            ),
            "{err}"
        );
        // a misspelled top-level table is an error, not zero rules
        std::fs::write(&path, "[[formats]]\nid = \"x\"\npattern = 'x'\n").expect("write");
        let err = SecretScanner::load(Some(&path)).expect_err("unknown table");
        assert!(matches!(err, RulesError::Parse { .. }), "{err}");
        // an unreadable path is an error, not a silent fallback (a directory
        // where the file should be fails to read even as root)
        let err = SecretScanner::load(Some(dir.path())).expect_err("unreadable");
        assert!(matches!(err, RulesError::Read { .. }), "{err}");
    }
}
