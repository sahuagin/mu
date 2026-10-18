//! mu-09z5y part 3, increment 5a: the redacted view of a session log, built
//! at its seam; increment 5b integrates it (`mu export --redacted`, the
//! Python binding, the resume carry of `sensitive_calls`).
//! Design: `specs/architecture/command-text-redaction.md`, section 4.
//!
//! [`redacted_view`] is a projection: it takes a session's events and
//! returns a copy in which every tool call judged sensitive has its
//! arguments WITHHELD, whole, in every place they appear:
//!
//! - the `ToolCall` event's `arguments`;
//! - the matching tool-call block (same `call_id`) in the
//!   `AssistantMessageEvent` that issued it;
//! - tool-call blocks in a `ContinuationSeeded` copy of a predecessor's
//!   conversation;
//! - an `ErrorInvalidMessage`'s `raw_message`, which holds a provider's raw
//!   reply.
//!
//! The withheld value is `{"withheld": true, "rules": [...], "hmac": "..."}`.
//! `hmac` is HMAC-SHA256 of the original arguments' JSON under a key kept
//! outside the log ([`RedactionKey`]), so identical commands match across
//! sessions and machines sharing the key, without the value being
//! recoverable by guessing the way a bare hash of a short secret would be.
//!
//! A call is sensitive if it was tagged when recorded (its `ToolCall`
//! event's `sensitive`, increment 4a, or, for a call inherited by a
//! resumed session, the seed's `sensitive_calls` carried from the
//! predecessor's log) OR the scanner passed in fires on it now. Rescanning
//! covers logs written before tagging existed, seeds written before the
//! carry, and rules added since. It only ever adds: a stored tag is never
//! dropped. A seed written before the carry has only the rescan, so a call
//! its predecessor tagged under a rule since retired is copied there.
//!
//! The primary log is never touched. Tool RESULTS are not redacted yet
//! (increment 7), nor user messages.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;

use crate::agent::types::{AgentMessage, AssistantMessage, ContentBlock, ToolArgs};
use crate::event_log::{EventPayload, SessionEvent};
use crate::secret_scan::SecretScanner;

/// Shortest key accepted, in bytes: the HMAC-SHA256 output size.
pub const MIN_KEY_BYTES: usize = 32;

/// The HMAC key for withheld values. Read from a file the operator creates;
/// mu never generates or writes one (one key is shared by every machine
/// whose redacted views should correlate, so where it comes from is the
/// operator's call).
#[derive(Clone)]
pub struct RedactionKey(Vec<u8>);

impl std::fmt::Debug for RedactionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RedactionKey(..)")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error(
        "redaction key {path}: not found. Create one (the same file on every machine whose \
         exports should correlate), owner-only from the start: \
         `(umask 077 && openssl rand -hex 32 > '{path}')`"
    )]
    Missing { path: String },
    #[error("redaction key {path}: {err}")]
    Read { path: String, err: std::io::Error },
    #[error(
        "redaction key {path}: mode {mode:o} lets group or others read it; `chmod 600 '{path}'`, \
         and replace the key (on every machine sharing it) if anyone else could have read it"
    )]
    Permissions { path: String, mode: u32 },
    #[error("redaction key {path}: {len} bytes, need at least {MIN_KEY_BYTES}")]
    TooShort { path: String, len: usize },
}

impl RedactionKey {
    /// The standard place: `<config dir>/mu/redaction.key`, next to
    /// `config.toml` and `secret_scan.toml`.
    pub fn default_path() -> Option<std::path::PathBuf> {
        dirs::config_dir().map(|dir| dir.join("mu").join("redaction.key"))
    }

    /// Read the key at `path`. The file's bytes are the key, less trailing
    /// whitespace (so `openssl rand -hex 32 > file` output works). Errors, never a
    /// fallback: a missing, unreadable, group- or world-readable, or short
    /// key file.
    pub fn load(path: &Path) -> Result<Self, KeyError> {
        use std::io::Read;
        let name = path.display().to_string();
        // One open: the mode is checked on, and the bytes read from, the
        // same file, so the path can't be swapped between the two.
        let mut file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(KeyError::Missing { path: name })
            }
            Err(err) => return Err(KeyError::Read { path: name, err }),
        };
        let read_err = |err| KeyError::Read {
            path: name.clone(),
            err,
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = file.metadata().map_err(read_err)?.permissions().mode();
            if mode & 0o077 != 0 {
                return Err(KeyError::Permissions {
                    path: name,
                    mode: mode & 0o777,
                });
            }
        }
        #[cfg(not(unix))]
        tracing::warn!(
            "redaction key {name}: file permissions aren't checked on this platform; keep it \
             readable by its owner only"
        );
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(read_err)?;
        let len = bytes
            .iter()
            .rposition(|b| !b.is_ascii_whitespace())
            .map_or(0, |i| i + 1);
        if len < MIN_KEY_BYTES {
            return Err(KeyError::TooShort { path: name, len });
        }
        Ok(Self(bytes[..len].to_vec()))
    }

    /// A key from bytes, for tests and embedding callers.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, KeyError> {
        if bytes.len() < MIN_KEY_BYTES {
            return Err(KeyError::TooShort {
                path: "<bytes>".to_owned(),
                len: bytes.len(),
            });
        }
        Ok(Self(bytes.to_vec()))
    }

    fn hmac_hex(&self, text: &str) -> String {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.0).expect("HMAC-SHA256 takes any key length");
        mac.update(text.as_bytes());
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn withheld(&self, rules: &BTreeSet<String>, original: &str) -> Value {
        json!({
            "withheld": true,
            "rules": rules.iter().collect::<Vec<_>>(),
            "hmac": self.hmac_hex(original),
        })
    }
}

/// The redacted copy of `events` (see the module doc). `events` is left as
/// it is.
pub fn redacted_view(
    events: &[SessionEvent],
    scanner: &SecretScanner,
    key: &RedactionKey,
) -> Vec<SessionEvent> {
    // Pass 1: the rules for every call id, from the stored tag and a fresh
    // scan of the recorded ToolCall. The AssistantMessageEvent that issued a
    // call comes BEFORE its ToolCall event, hence two passes.
    let mut rules_by_call: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for ev in events {
        match &ev.payload {
            EventPayload::ToolCall {
                call_id,
                name,
                arguments,
                sensitive,
                ..
            } => {
                let rules = rules_by_call.entry(call_id.clone()).or_default();
                rules.extend(sensitive.iter().cloned());
                rules.extend(scanner.tag_tool_call(name, arguments));
            }
            EventPayload::ContinuationSeeded {
                sensitive_calls, ..
            } => {
                for (call_id, tags) in sensitive_calls {
                    rules_by_call
                        .entry(call_id.clone())
                        .or_default()
                        .extend(tags.iter().cloned());
                }
            }
            _ => {}
        }
    }
    let rules_for = |call_id: &str, name: &str, arguments: &Value| -> BTreeSet<String> {
        let mut rules = rules_by_call.get(call_id).cloned().unwrap_or_default();
        rules.extend(scanner.tag_tool_call(name, arguments));
        rules
    };
    let redact_message = |message: &mut AssistantMessage| {
        for block in &mut message.content {
            if let ContentBlock::ToolCall(call) = block {
                let rules = rules_for(&call.id, &call.name, call.arguments.as_value());
                if !rules.is_empty() {
                    let original = call.arguments.as_value().to_string();
                    call.arguments = ToolArgs::new(key.withheld(&rules, &original))
                        .expect("the withheld object holds no floats");
                }
            }
        }
    };

    events
        .iter()
        .map(|ev| {
            let mut ev = ev.clone();
            match &mut ev.payload {
                EventPayload::ToolCall {
                    call_id,
                    name,
                    arguments,
                    ..
                } => {
                    let rules = rules_for(call_id, name, arguments);
                    if !rules.is_empty() {
                        *arguments = key.withheld(&rules, &arguments.to_string());
                    }
                }
                EventPayload::AssistantMessageEvent { message } => redact_message(message),
                EventPayload::ContinuationSeeded { messages, .. } => {
                    for m in messages {
                        if let AgentMessage::Assistant(message) = m {
                            redact_message(message);
                        }
                    }
                }
                EventPayload::ErrorInvalidMessage { raw_message, .. } => {
                    let rules = raw_reply_rules(scanner, raw_message);
                    if !rules.is_empty() {
                        *raw_message = key.withheld(&rules, raw_message).to_string();
                    }
                }
                _ => {}
            }
            ev
        })
        .collect()
}

/// `s` with JSON string escapes undone (`\\n`, `\\"`, `\\uXXXX` with surrogate
/// pairs, ...), without parsing: for text that isn't valid JSON. A
/// malformed escape is kept as written.
fn unescape_json_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(c @ ('"' | '\\' | '/')) => out.push(c),
            // `\uXXXX`, including a surrogate pair; anything malformed
            // is kept as written
            Some('u') => {
                let hex = |chars: &mut std::str::Chars<'_>| -> Option<u32> {
                    let digits: String = chars.by_ref().take(4).collect();
                    (digits.len() == 4)
                        .then(|| u32::from_str_radix(&digits, 16).ok())
                        .flatten()
                };
                let mut ahead = chars.clone();
                let decoded = hex(&mut ahead).and_then(|hi| {
                    if (0xD800..0xDC00).contains(&hi) {
                        let mut pair = ahead.clone();
                        (pair.next() == Some('\\') && pair.next() == Some('u'))
                            .then(|| hex(&mut pair))
                            .flatten()
                            .filter(|lo| (0xDC00..0xE000).contains(lo))
                            .and_then(|lo| {
                                ahead = pair;
                                char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00))
                            })
                    } else {
                        char::from_u32(hi)
                    }
                });
                match decoded {
                    Some(c) => {
                        out.push(c);
                        chars = ahead;
                    }
                    None => {
                        out.push(ch);
                        out.push('u');
                    }
                }
            }
            Some(other) => {
                out.push(ch);
                out.push(other);
            }
            None => out.push(ch),
        }
    }
    out
}

/// The rules that fire on a provider's raw reply: on the text as it is,
/// and, when it parses as JSON, on every decoded string in it. JSON escapes
/// the quotes inside a command (`-H \"X-Api-Key: …\"`), which the
/// shell-syntax rules can't read through; decoded, the string is the
/// command again. A string that is itself JSON (tool arguments are often
/// sent JSON-encoded) is decoded in turn, to a fixed depth.
///
/// A reply that doesn't parse (it's truncated at 64 KiB, or invalid, which
/// is why it was recorded) is also scanned with its JSON string escapes
/// (`\uXXXX` included) undone, twice over for doubly encoded arguments, so
/// a header inside it is read as written.
fn raw_reply_rules(scanner: &SecretScanner, raw: &str) -> BTreeSet<String> {
    fn leaves(v: &Value, depth: usize, out: &mut Vec<String>) {
        match v {
            Value::String(s) => {
                out.push(s.clone());
                if depth < 3 {
                    if let Ok(inner @ (Value::Object(_) | Value::Array(_))) =
                        serde_json::from_str::<Value>(s)
                    {
                        leaves(&inner, depth + 1, out);
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|i| leaves(i, depth, out)),
            Value::Object(map) => map.values().for_each(|i| leaves(i, depth, out)),
            _ => {}
        }
    }
    let once = unescape_json_escapes(raw);
    let twice = unescape_json_escapes(&once);
    let mut texts = vec![raw.to_owned(), once, twice];
    if let Ok(v) = serde_json::from_str::<Value>(raw) {
        leaves(&v, 0, &mut texts);
    }
    texts
        .iter()
        .flat_map(|t| scanner.scan(t))
        .map(|h| h.rule)
        .collect()
}

/// The `sensitive` tags of the tool calls in `inherited` (a resumed
/// session's seed messages), from the predecessor's `events`: each call's
/// `ToolCall` tag, plus what the predecessor's own seed carried, so the
/// tags follow a chain of resumes. For the seed's `sensitive_calls`; only
/// tagged calls appear.
pub fn inherited_tags(
    events: &[SessionEvent],
    inherited: &[AgentMessage],
) -> BTreeMap<String, Vec<String>> {
    let wanted: BTreeSet<&str> = inherited
        .iter()
        .filter_map(|m| match m {
            AgentMessage::Assistant(message) => Some(message),
            _ => None,
        })
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call.id.as_str()),
            _ => None,
        })
        .collect();
    let mut tags: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut add = |call_id: &str, rules: &[String]| {
        if wanted.contains(call_id) && !rules.is_empty() {
            tags.entry(call_id.to_owned())
                .or_default()
                .extend(rules.iter().cloned());
        }
    };
    for ev in events {
        match &ev.payload {
            EventPayload::ToolCall {
                call_id, sensitive, ..
            } => add(call_id, sensitive),
            EventPayload::ContinuationSeeded {
                sensitive_calls, ..
            } => {
                for (call_id, rules) in sensitive_calls {
                    add(call_id, rules);
                }
            }
            _ => {}
        }
    }
    tags.into_iter()
        .map(|(id, rules)| (id, rules.into_iter().collect()))
        .collect()
}

#[derive(Debug, thiserror::Error)]
pub enum RedactLogError {
    #[error("reading event log {path}: {err}")]
    Read { path: String, err: std::io::Error },
    #[error(
        "event log {path}: {count} line(s) don't parse as events; refusing to produce a \
         redacted view (a dropped ToolCall line takes its tag with it, which could let the \
         same call's copy elsewhere through)"
    )]
    Malformed { path: String, count: usize },
}

/// [`redacted_view`] of the session log at `path`. Refuses, producing
/// nothing, if any line doesn't parse as an event: a dropped `ToolCall`
/// line takes its stored tag with it, and the call's copy in the assistant
/// message would then rest on the rescan alone.
pub fn redact_log(
    path: &Path,
    scanner: &SecretScanner,
    key: &RedactionKey,
) -> Result<Vec<SessionEvent>, RedactLogError> {
    let name = || path.display().to_string();
    let (log, malformed) = crate::event_log::SessionEventLog::from_jsonl(path)
        .map_err(|err| RedactLogError::Read { path: name(), err })?;
    if malformed > 0 {
        return Err(RedactLogError::Malformed {
            path: name(),
            count: malformed,
        });
    }
    Ok(redacted_view(&log.snapshot(), scanner, key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::types::{StopReason, ToolCall};
    use crate::event_log::EventActor;

    const SECRET_CMD: &str = "curl -H 'Authorization: Bearer abc123' https://example.com";

    fn key() -> RedactionKey {
        RedactionKey::from_bytes(&[7u8; 32]).expect("key")
    }

    fn ev(id: u64, payload: EventPayload) -> SessionEvent {
        SessionEvent {
            id,
            session_id: "s".into(),
            parent_event_ids: vec![],
            timestamp_unix_ms: id,
            actor: EventActor::Agent,
            payload,
        }
    }

    fn assistant(call_id: &str, command: &str) -> AssistantMessage {
        AssistantMessage {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: call_id.into(),
                name: "bash".into(),
                arguments: ToolArgs::new(json!({ "command": command })).expect("args"),
            })],
            stop_reason: StopReason::ToolUse,
            usage: None,
        }
    }

    fn tool_call(call_id: &str, command: &str, sensitive: &[&str]) -> EventPayload {
        EventPayload::ToolCall {
            call_id: call_id.into(),
            name: "bash".into(),
            arguments: json!({ "command": command }),
            cwd: None,
            sensitive: sensitive.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn session(command: &str, sensitive: &[&str]) -> Vec<SessionEvent> {
        vec![
            ev(
                1,
                EventPayload::AssistantMessageEvent {
                    message: assistant("c1", command),
                },
            ),
            ev(2, tool_call("c1", command, sensitive)),
        ]
    }

    fn withheld_parts(v: &Value) -> (Vec<String>, String) {
        assert_eq!(v["withheld"], true, "{v}");
        let rules = v["rules"]
            .as_array()
            .expect("rules")
            .iter()
            .map(|r| r.as_str().expect("rule").to_owned())
            .collect();
        (rules, v["hmac"].as_str().expect("hmac").to_owned())
    }

    fn block_args(e: &SessionEvent) -> Value {
        let EventPayload::AssistantMessageEvent { message } = &e.payload else {
            panic!("expected assistant message")
        };
        let ContentBlock::ToolCall(call) = &message.content[0] else {
            panic!("expected tool call block")
        };
        call.arguments.as_value().clone()
    }

    fn call_args(e: &SessionEvent) -> Value {
        let EventPayload::ToolCall { arguments, .. } = &e.payload else {
            panic!("expected tool call")
        };
        arguments.clone()
    }

    #[test]
    fn a_tagged_call_is_withheld_in_both_places_with_one_hmac() {
        let events = session(SECRET_CMD, &["auth-header"]);
        let view = redacted_view(&events, &SecretScanner::built_in(), &key());
        let (rules_a, hmac_a) = withheld_parts(&block_args(&view[0]));
        let (rules_b, hmac_b) = withheld_parts(&call_args(&view[1]));
        assert_eq!(rules_a, vec!["auth-header"]);
        assert_eq!(rules_b, vec!["auth-header"]);
        assert_eq!(hmac_a, hmac_b, "the same text correlates");
        assert_eq!(hmac_a.len(), 64);
        let text = serde_json::to_string(&view).expect("serialize");
        assert!(!text.contains("abc123"), "{text}");
        // the input is untouched: the log stays exact
        assert_eq!(call_args(&events[1])["command"], SECRET_CMD);
        assert_eq!(block_args(&events[0])["command"], SECRET_CMD);
    }

    #[test]
    fn an_untagged_old_log_is_rescanned_and_a_stored_tag_is_kept() {
        // written before tagging: no stored tag, the scanner catches it
        let view = redacted_view(
            &session(SECRET_CMD, &[]),
            &SecretScanner::built_in(),
            &key(),
        );
        assert_eq!(withheld_parts(&call_args(&view[1])).0, vec!["auth-header"]);
        assert_eq!(withheld_parts(&block_args(&view[0])).0, vec!["auth-header"]);
        // a stored tag the current rules no longer fire on is still honored
        let view = redacted_view(
            &session("echo ordinary", &["retired-rule"]),
            &SecretScanner::built_in(),
            &key(),
        );
        assert_eq!(withheld_parts(&call_args(&view[1])).0, vec!["retired-rule"]);
        assert_eq!(
            withheld_parts(&block_args(&view[0])).0,
            vec!["retired-rule"]
        );
    }

    #[test]
    fn clean_calls_and_other_events_pass_through_unchanged() {
        let mut events = session("ls -la", &[]);
        events.push(ev(
            3,
            EventPayload::UserMessage {
                content: "hello".into(),
            },
        ));
        let view = redacted_view(&events, &SecretScanner::built_in(), &key());
        assert_eq!(view, events);
    }

    #[test]
    fn continuation_copies_and_raw_provider_replies_are_covered() {
        let events = vec![
            ev(
                1,
                EventPayload::ContinuationSeeded {
                    predecessor_session_id: "p".into(),
                    branched_at_event_id: None,
                    messages: vec![AgentMessage::Assistant(assistant("old", SECRET_CMD))],
                    sensitive_calls: Default::default(),
                },
            ),
            ev(
                2,
                EventPayload::ErrorInvalidMessage {
                    provider: "x".into(),
                    raw_message: format!("{{\"input\":\"{SECRET_CMD}\"}}"),
                    validation_error: "bad".into(),
                },
            ),
        ];
        let view = redacted_view(&events, &SecretScanner::built_in(), &key());
        let text = serde_json::to_string(&view).expect("serialize");
        assert!(!text.contains("abc123"), "{text}");
        let EventPayload::ErrorInvalidMessage { raw_message, .. } = &view[1].payload else {
            panic!("expected error event")
        };
        let raw: Value = serde_json::from_str(raw_message).expect("withheld json");
        assert_eq!(withheld_parts(&raw).0, vec!["auth-header"]);
    }

    #[test]
    fn inherited_tags_follow_a_chain_of_resumes_and_only_the_inherited_calls() {
        // the predecessor: its own tagged call, an untagged one, a tagged
        // call that isn't inherited, and what ITS seed carried from further up
        let predecessor = vec![
            ev(
                1,
                EventPayload::ContinuationSeeded {
                    predecessor_session_id: "grandparent".into(),
                    branched_at_event_id: None,
                    messages: vec![],
                    sensitive_calls: [("g1".to_owned(), vec!["jwt".to_owned()])].into(),
                },
            ),
            ev(2, tool_call("p1", "x", &["auth-header"])),
            ev(3, tool_call("p2", "x", &[])),
            ev(4, tool_call("p3", "x", &["github-token"])),
        ];
        let inherited: Vec<AgentMessage> = ["g1", "p1", "p2"]
            .into_iter()
            .map(|id| AgentMessage::Assistant(assistant(id, "x")))
            .collect();
        let tags = inherited_tags(&predecessor, &inherited);
        assert_eq!(
            tags,
            [
                ("g1".to_owned(), vec!["jwt".to_owned()]),
                ("p1".to_owned(), vec!["auth-header".to_owned()]),
            ]
            .into()
        );
    }

    #[test]
    fn seed_tags_round_trip_and_are_left_out_when_empty() {
        let seed = EventPayload::ContinuationSeeded {
            predecessor_session_id: "p".into(),
            branched_at_event_id: None,
            messages: vec![],
            sensitive_calls: Default::default(),
        };
        let v = serde_json::to_value(&seed).expect("serialize");
        assert!(v.get("sensitive_calls").is_none(), "{v}");
        let tagged = EventPayload::ContinuationSeeded {
            predecessor_session_id: "p".into(),
            branched_at_event_id: None,
            messages: vec![],
            sensitive_calls: [("c1".to_owned(), vec!["jwt".to_owned()])].into(),
        };
        let v = serde_json::to_value(&tagged).expect("serialize");
        assert_eq!(v["sensitive_calls"], json!({ "c1": ["jwt"] }));
        let back: EventPayload = serde_json::from_value(v).expect("round trip");
        assert_eq!(back, tagged);
    }

    #[test]
    fn a_raw_reply_is_scanned_decoded_so_escaped_quotes_dont_hide_a_header() {
        // as a provider sends it: JSON, the command's double quotes escaped,
        // and (OpenAI style) the arguments themselves a JSON-encoded string
        let command = r#"curl -H "X-Api-Key: 12345" https://example.com"#;
        let anthropic = serde_json::to_string(&json!({
            "content": [{ "type": "tool_use", "input": { "command": command } }]
        }))
        .expect("json");
        let openai = serde_json::to_string(&json!({
            "tool_calls": [{ "function": {
                "arguments": serde_json::to_string(&json!({ "command": command })).expect("args")
            }}]
        }))
        .expect("json");
        assert!(
            SecretScanner::built_in().scan(&anthropic).is_empty(),
            "the raw text alone hides it, which is the point of decoding"
        );
        for raw in [anthropic, openai] {
            let events = vec![ev(
                1,
                EventPayload::ErrorInvalidMessage {
                    provider: "x".into(),
                    raw_message: raw.clone(),
                    validation_error: "bad".into(),
                },
            )];
            let view = redacted_view(&events, &SecretScanner::built_in(), &key());
            let EventPayload::ErrorInvalidMessage { raw_message, .. } = &view[0].payload else {
                panic!("expected error event")
            };
            assert!(!raw_message.contains("12345"), "{raw_message}");
            let withheld: Value = serde_json::from_str(raw_message).expect("withheld json");
            assert_eq!(withheld_parts(&withheld).0, vec!["auth-header"], "{raw}");
        }
        // truncated (no longer JSON), singly and doubly encoded: the escapes
        // are undone without a parse
        let anthropic_cut =
            serde_json::to_string(&json!({ "input": { "command": command } })).expect("json");
        let openai_cut = serde_json::to_string(&json!({
            "arguments": serde_json::to_string(&json!({ "command": command })).expect("args")
        }))
        .expect("json");
        for whole in [anthropic_cut, openai_cut] {
            let cut = &whole[..whole.len() - 2];
            assert!(serde_json::from_str::<Value>(cut).is_err(), "{cut}");
            let rules = raw_reply_rules(&SecretScanner::built_in(), cut);
            assert!(rules.contains("auth-header"), "{cut}: {rules:?}");
        }
        // `\u` escapes, in a reply that doesn't parse
        let escaped = r#"{"command":"curl -H \u0022X-Api-Key: 12345\u0022 https://example.com"#;
        let rules = raw_reply_rules(&SecretScanner::built_in(), escaped);
        assert!(rules.contains("auth-header"), "{rules:?}");
        assert_eq!(
            unescape_json_escapes(r"a\ud83d\ude00b\u00zz"),
            "a\u{1F600}b\\u00zz"
        );
        // a reply that isn't JSON is still scanned as text
        let rules = raw_reply_rules(&SecretScanner::built_in(), &format!("oops {command}"));
        assert!(rules.contains("auth-header"), "{rules:?}");
    }

    #[test]
    fn the_hmac_depends_on_the_key() {
        let events = session(SECRET_CMD, &["auth-header"]);
        let other = RedactionKey::from_bytes(&[9u8; 32]).expect("key");
        let a = withheld_parts(&call_args(
            &redacted_view(&events, &SecretScanner::built_in(), &key())[1],
        ))
        .1;
        let b = withheld_parts(&call_args(
            &redacted_view(&events, &SecretScanner::built_in(), &other)[1],
        ))
        .1;
        assert_ne!(a, b);
    }

    #[test]
    fn a_log_with_an_unparseable_line_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("s.jsonl");
        let good = serde_json::to_string(&session(SECRET_CMD, &[])[0]).expect("row");
        std::fs::write(&path, format!("{good}\n{{\"not\": \"an event\"}}\n")).expect("write");
        let err = redact_log(&path, &SecretScanner::built_in(), &key()).expect_err("refused");
        assert!(
            matches!(err, RedactLogError::Malformed { count: 1, .. }),
            "{err}"
        );
        std::fs::write(&path, format!("{good}\n")).expect("write");
        let view = redact_log(&path, &SecretScanner::built_in(), &key()).expect("clean log");
        assert_eq!(view.len(), 1);
    }

    #[test]
    fn key_files_fail_loud() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("redaction.key");
        assert!(matches!(
            RedactionKey::load(&path),
            Err(KeyError::Missing { .. })
        ));
        std::fs::write(&path, "short\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        }
        assert!(matches!(
            RedactionKey::load(&path),
            Err(KeyError::TooShort { len: 5, .. })
        ));
        std::fs::write(&path, format!("{}\n", "ab".repeat(32))).expect("write");
        let k = RedactionKey::load(&path).expect("good key");
        assert_eq!(k.0.len(), 64, "trailing newline trimmed");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("chmod");
            assert!(matches!(
                RedactionKey::load(&path),
                Err(KeyError::Permissions { mode: 0o640, .. })
            ));
        }
    }
}
