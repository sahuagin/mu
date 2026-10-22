//! mu-09z5y part 3, increment 2: a best-effort scanner for credentials in
//! command text. Design: `specs/architecture/command-text-redaction.md`.
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
/// fragment key, a flag). Errs toward "secret": a false positive only
/// withholds something, a false negative leaks it.
///
/// Matches a name that IS, or ENDS IN `_`+, one of KEY / TOKEN / SECRET /
/// PASSWORD / PASSWD / PASS / CREDENTIAL(S) / COOKIE; any name containing
/// AUTH; and every `AWS_*` name. Case-insensitive; `-` counts as `_`, so a
/// query key `api-key` or a flag `--auth-token` match as well.
pub fn is_secret_name(name: &str) -> bool {
    const WORDS: &[&str] = &[
        "KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "PASS",
        "CREDENTIAL",
        "CREDENTIALS",
        "COOKIE",
    ];
    let upper = name.to_ascii_uppercase().replace('-', "_");
    WORDS
        .iter()
        .any(|w| upper == *w || upper.ends_with(&format!("_{w}")))
        || upper.contains("AUTH")
        || upper.starts_with("AWS_")
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
    min_bits_per_char: Option<f64>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone)]
struct Entropy {
    enabled: bool,
    min_len: usize,
    min_bits_per_char: f64,
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
}

impl SecretScanner {
    /// The built-in rules only.
    pub fn built_in() -> Self {
        Self::from_layers(&[("built-in", BUILT_IN_RULES)])
            .expect("built-in secret_scan rules must compile")
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
        let mut entropy_bits: Option<f64> = None;
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
                if e.min_len == Some(0) {
                    return Err(RulesError::Entropy {
                        source_name: (*source_name).to_owned(),
                        field: "min_len",
                        value: "0".to_owned(),
                        expected: "at least 1".to_owned(),
                    });
                }
                entropy_enabled = e.enabled.or(entropy_enabled);
                entropy_min_len = e.min_len.or(entropy_min_len);
                entropy_bits = e.min_bits_per_char.or(entropy_bits);
            }
        }
        let (Some(enabled), Some(min_len), Some(min_bits_per_char)) =
            (entropy_enabled, entropy_min_len, entropy_bits)
        else {
            return Err(RulesError::Entropy {
                source_name: "built-in".to_owned(),
                field: "enabled/min_len/min_bits_per_char",
                value: "unset".to_owned(),
                expected: "all three set in config/secret_scan.default.toml".to_owned(),
            });
        };
        let entropy = Entropy {
            enabled,
            min_len,
            min_bits_per_char,
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
        Ok(Self { formats, entropy })
    }

    /// Every rule that fires on `text`, deduplicated and sorted. Never the
    /// matched text.
    pub fn scan(&self, text: &str) -> Vec<Hit> {
        let mut hits = BTreeSet::new();
        scan_assignments(text, &mut hits);
        scan_auth_flags(text, &mut hits);
        for candidate in url_candidates(text) {
            self.scan_url(candidate, 0, &mut hits);
        }
        self.scan_formats(text, &mut hits);
        if self.entropy.enabled {
            scan_entropy(text, &self.entropy, &mut hits);
        }
        hits.into_iter().collect()
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
            if is_secret_name(&key) && !value.is_empty() {
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

fn scan_assignments(text: &str, hits: &mut BTreeSet<Hit>) {
    for c in assignment_re().captures_iter(text) {
        if is_secret_name(&c[1]) && has_literal(&c[2]) {
            hits.insert(hit("secret-assignment", HitKind::Assignment));
        }
    }
}

fn scan_auth_flags(text: &str, hits: &mut BTreeSet<Hit>) {
    static RES: std::sync::OnceLock<Vec<(&'static str, Regex)>> = std::sync::OnceLock::new();
    let res = RES.get_or_init(|| {
        [
            // an auth header given to curl/wget/httpie, or written inline
            (
                "auth-header",
                r#"(?i)(?:authorization|proxy-authorization)\s*:\s*(?:bearer|basic|token|digest)\s+[^\s'"$]"#,
            ),
            (
                "auth-header",
                r#"(?i)(?:^|\s)(?:-H|--header)[\s=]*['"]?\s*(?:cookie|x-api-key|x-auth-token|api-key|x-access-token)\s*:\s*[^\s'"$]"#,
            ),
            // curl -u user:pw / --user user:pw (a literal password)
            (
                "basic-auth-flag",
                r#"(?:^|\s)(?:-u|--user)[\s=]+(?:'[^'\s:]+:[^'\s]|"?[^\s:'"$]+:[^\s'"$])"#,
            ),
            // --password=x, --token x, --api-key x (not --token-file, not $VAR)
            (
                "secret-flag",
                r#"(?i)(?:^|\s)--?(?:password|passwd|pass|token|api-key|apikey|secret|access-token|auth-token|client-secret)(?:=|\s+)(?:'[^'\s]|"?[^\s'"$-])"#,
            ),
            // mysql/mariadb -pSECRET (attached; -p alone prompts)
            (
                "mysql-password",
                r#"(?:^|[\s;&|])(?:mysql|mariadb|mysqldump)\b[^;&|\n]*\s-p(?:'[^'\s]|"[^"\s$]|[^\s'"$])"#,
            ),
        ]
        .into_iter()
        .map(|(id, p)| (id, Regex::new(p).expect("auth flag regex")))
        .collect()
    });
    for (id, re) in res {
        if re.is_match(text) {
            hits.insert(hit(id, HitKind::AuthFlag));
        }
    }
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
    for token in text.split(|c: char| !is_token_char(c)) {
        if token.len() < rule.min_len {
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
            "AWS_PROFILE",
            "PROXY_AUTHORIZATION",
            "SESSION_COOKIE",
            "token",
            "access_token",
        ] {
            assert!(is_secret_name(n), "{n}");
        }
        for n in [
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
    }

    #[test]
    fn auth_flags_and_headers() {
        fires(
            r#"curl -H "Authorization: Bearer abc.def" https://api.x/y"#,
            "auth-header",
        );
        fires("curl -H 'X-Api-Key: 12345' https://api.x/y", "auth-header");
        fires("curl -u bob:hunter2 https://x/y", "basic-auth-flag");
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
