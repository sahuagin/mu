//! Claude-code JSONL → mu event conversion.
//!
//! Ported from scripts/import-claude-history.py (the trusted parser).

use serde_json::Value;

use crate::types::*;

/// Convert a full claude-code session (list of parsed JSON values) to mu events.
pub fn convert_session(cc_events: &[Value], session_id: &str) -> Vec<MuEvent> {
    let mut out = Vec::with_capacity(cc_events.len() * 2);
    let mut next_id: u64 = 1;

    let first_envelope = cc_events
        .iter()
        .find(|e| matches!(e.get("type").and_then(|t| t.as_str()), Some("user" | "assistant")));

    let model = first_envelope
        .and_then(|e| e.get("message"))
        .and_then(|m| m.get("model"))
        .and_then(|m| m.as_str())
        .unwrap_or("unknown");

    let version = first_envelope
        .and_then(|e| e.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let cwd = first_envelope
        .and_then(|e| e.get("cwd"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let entrypoint = first_envelope
        .and_then(|e| e.get("entrypoint"))
        .and_then(|v| v.as_str())
        .unwrap_or("cli");

    let ts = first_envelope
        .and_then(|e| e.get("timestamp"))
        .and_then(|t| t.as_str())
        .map(iso_to_unix_ms)
        .unwrap_or(0);

    out.push(MuEvent {
        id: next_id,
        timestamp_unix_ms: ts,
        actor: Actor::System,
        payload: Payload::SessionCreated {
            provider_kind: "anthropic_api".into(),
            model: model.into(),
        },
    });
    next_id += 1;

    out.push(MuEvent {
        id: next_id,
        timestamp_unix_ms: ts,
        actor: Actor::System,
        payload: Payload::Callout {
            category: "import".into(),
            title: "Imported from claude-code".into(),
            body: serde_json::json!({
                "source": "claude-code",
                "version": version,
                "entrypoint": entrypoint,
                "cwd": cwd,
                "original_session_id": session_id,
                "event_count": cc_events.len(),
            }),
        },
    });
    next_id += 1;

    for cc_event in cc_events {
        let converted = convert_one_event(cc_event, next_id);
        let count = converted.len() as u64;
        out.extend(converted);
        next_id += count;
    }

    let last_ts = out.last().map(|e| e.timestamp_unix_ms).unwrap_or(0);
    out.push(MuEvent {
        id: next_id,
        timestamp_unix_ms: last_ts,
        actor: Actor::System,
        payload: Payload::SessionClosed,
    });

    out
}

/// Convert a single claude-code event. May produce 0..N mu events.
pub fn convert_one_event(cc_event: &Value, start_id: u64) -> Vec<MuEvent> {
    let cc_type = cc_event
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("");

    match cc_type {
        "user" => convert_user_event(cc_event, start_id),
        "assistant" => convert_assistant_event(cc_event, start_id),
        "system" | "attachment" | "permission-mode" | "ai-title" | "queue-operation" => {
            convert_metadata_event(cc_event, start_id)
        }
        _ => Vec::new(),
    }
}

fn convert_user_event(cc_event: &Value, start_id: u64) -> Vec<MuEvent> {
    let ts = event_ts(cc_event);
    let msg = cc_event.get("message").cloned().unwrap_or(Value::Null);
    let content = msg.get("content").cloned().unwrap_or(Value::Null);
    let mut out = Vec::new();
    let mut next_id = start_id;

    let is_tool_result = cc_event.get("sourceToolAssistantUUID").is_some();

    if is_tool_result {
        if let Some(blocks) = content.as_array() {
            for block in blocks {
                if block.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                    let result_content = extract_tool_result_content(block);
                    let call_id = block
                        .get("tool_use_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let is_error = block
                        .get("is_error")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);

                    out.push(MuEvent {
                        id: next_id,
                        timestamp_unix_ms: ts,
                        actor: Actor::Tool {
                            name: "unknown".into(),
                        },
                        payload: Payload::ToolResult {
                            call_id,
                            content: truncate(&result_content, 65536),
                            is_error,
                        },
                    });
                    next_id += 1;
                }
            }
        } else if let Some(result_str) = cc_event.get("toolUseResult").and_then(|v| v.as_str()) {
            out.push(MuEvent {
                id: next_id,
                timestamp_unix_ms: ts,
                actor: Actor::Tool {
                    name: "unknown".into(),
                },
                payload: Payload::ToolResult {
                    call_id: String::new(),
                    content: truncate(result_str, 65536),
                    is_error: false,
                },
            });
        }
    } else {
        let text = extract_text_from_content(&content);
        if !text.trim().is_empty() {
            out.push(MuEvent {
                id: next_id,
                timestamp_unix_ms: ts,
                actor: Actor::User,
                payload: Payload::UserMessage { content: text },
            });
        }
    }

    out
}

fn convert_assistant_event(cc_event: &Value, start_id: u64) -> Vec<MuEvent> {
    let ts = event_ts(cc_event);
    let msg = cc_event.get("message").cloned().unwrap_or(Value::Null);
    let content = msg
        .get("content")
        .cloned()
        .unwrap_or(Value::Array(Vec::new()));
    let mut out = Vec::new();
    let mut next_id = start_id;

    let blocks = content.as_array().cloned().unwrap_or_else(|| {
        vec![serde_json::json!({"type": "text", "text": content.as_str().unwrap_or("")})]
    });

    let model = msg
        .get("model")
        .and_then(|m| m.as_str())
        .or_else(|| cc_event.get("model").and_then(|m| m.as_str()))
        .unwrap_or("unknown");

    let stop_reason = msg
        .get("stop_reason")
        .and_then(|s| s.as_str())
        .unwrap_or("end_turn");

    let usage = msg.get("usage").and_then(parse_usage);

    // Thinking blocks → audit events
    for block in &blocks {
        let btype = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if btype == "thinking" || btype == "redacted_thinking" {
            out.push(MuEvent {
                id: next_id,
                timestamp_unix_ms: ts,
                actor: Actor::Agent,
                payload: Payload::AuditEvent {
                    subtype: "thinking".into(),
                    body: block.clone(),
                },
            });
            next_id += 1;
        }
    }

    // Tool use blocks → tool_call events
    for block in &blocks {
        if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
            let call_id = block
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = block
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let arguments = block.get("input").cloned().unwrap_or(Value::Object(Default::default()));

            out.push(MuEvent {
                id: next_id,
                timestamp_unix_ms: ts,
                actor: Actor::Agent,
                payload: Payload::ToolCall {
                    call_id,
                    name,
                    arguments,
                },
            });
            next_id += 1;
        }
    }

    // Text content → assistant_message
    let text = extract_text_from_blocks(&blocks);
    let has_tool_calls = blocks
        .iter()
        .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"));
    let has_thinking = blocks
        .iter()
        .any(|b| matches!(b.get("type").and_then(|t| t.as_str()), Some("thinking" | "redacted_thinking")));

    if !text.trim().is_empty() || (!has_tool_calls && !has_thinking) {
        out.push(MuEvent {
            id: next_id,
            timestamp_unix_ms: ts,
            actor: Actor::Agent,
            payload: Payload::AssistantMessage {
                content: text,
                stop_reason: stop_reason.into(),
                model: model.into(),
                usage,
            },
        });
    }

    out
}

fn convert_metadata_event(cc_event: &Value, start_id: u64) -> Vec<MuEvent> {
    let ts = event_ts(cc_event);
    let cc_type = cc_event
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("unknown");

    let mut body = serde_json::Map::new();
    body.insert("source_type".into(), Value::String(cc_type.into()));

    match cc_type {
        "system" => {
            if let Some(sub) = cc_event.get("subtype").and_then(|s| s.as_str()) {
                body.insert("subtype".into(), Value::String(sub.into()));
            }
            if let Some(content) = cc_event.get("content").and_then(|c| c.as_str()) {
                body.insert("content".into(), Value::String(truncate(content, 4096)));
            }
        }
        "attachment" => {
            if let Some(att) = cc_event.get("attachment") {
                if let Some(atype) = att.get("type").and_then(|t| t.as_str()) {
                    body.insert("attachment_type".into(), Value::String(atype.into()));
                }
                if let Some(hook) = att.get("hookName").and_then(|h| h.as_str()) {
                    body.insert("hook_name".into(), Value::String(hook.into()));
                }
                if let Some(hook_ev) = att.get("hookEvent").and_then(|h| h.as_str()) {
                    body.insert("hook_event".into(), Value::String(hook_ev.into()));
                }
                let content_str = att
                    .get("content")
                    .map(|c| match c.as_str() {
                        Some(s) => truncate(s, 4096),
                        None => truncate(&c.to_string(), 4096),
                    })
                    .unwrap_or_default();
                body.insert("content".into(), Value::String(content_str));
            }
        }
        "permission-mode" => {
            if let Some(mode) = cc_event.get("permissionMode").and_then(|m| m.as_str()) {
                body.insert("permission_mode".into(), Value::String(mode.into()));
            }
        }
        "ai-title" => {
            if let Some(title) = cc_event.get("aiTitle").and_then(|t| t.as_str()) {
                body.insert("title".into(), Value::String(title.into()));
            }
        }
        _ => {}
    }

    vec![MuEvent {
        id: start_id,
        timestamp_unix_ms: ts,
        actor: Actor::System,
        payload: Payload::AuditEvent {
            subtype: cc_type.into(),
            body: Value::Object(body),
        },
    }]
}

// ─── Helpers ─────────────────────────────────────────────────────────

fn event_ts(cc_event: &Value) -> u64 {
    cc_event
        .get("timestamp")
        .and_then(|t| t.as_str())
        .map(iso_to_unix_ms)
        .unwrap_or(0)
}

fn iso_to_unix_ms(ts: &str) -> u64 {
    // Parse ISO 8601 with timezone. chrono would be cleaner but
    // keeping deps minimal — this handles the common shapes:
    //   2026-05-26T02:17:50.558Z
    //   2026-05-26T02:17:50.558+00:00
    let ts = ts.trim();
    let ts = if let Some(stripped) = ts.strip_suffix('Z') {
        stripped
    } else if ts.len() > 6 && &ts[ts.len() - 6..ts.len() - 5] == "+" {
        &ts[..ts.len() - 6]
    } else {
        ts
    };

    // Split at 'T'
    let parts: Vec<&str> = ts.splitn(2, 'T').collect();
    if parts.len() != 2 {
        return 0;
    }

    let date_parts: Vec<&str> = parts[0].splitn(3, '-').collect();
    if date_parts.len() != 3 {
        return 0;
    }

    let time_and_frac = parts[1];
    let (time_str, frac_ms) = if let Some(dot_pos) = time_and_frac.find('.') {
        let frac_str = &time_and_frac[dot_pos + 1..];
        let ms: u64 = match frac_str.len() {
            1 => frac_str.parse::<u64>().unwrap_or(0) * 100,
            2 => frac_str.parse::<u64>().unwrap_or(0) * 10,
            3 => frac_str.parse::<u64>().unwrap_or(0),
            _ => frac_str[..3].parse::<u64>().unwrap_or(0),
        };
        (&time_and_frac[..dot_pos], ms)
    } else {
        (time_and_frac, 0u64)
    };

    let time_parts: Vec<&str> = time_str.splitn(3, ':').collect();
    if time_parts.len() != 3 {
        return 0;
    }

    let year: i64 = date_parts[0].parse().unwrap_or(0);
    let month: i64 = date_parts[1].parse().unwrap_or(0);
    let day: i64 = date_parts[2].parse().unwrap_or(0);
    let hour: i64 = time_parts[0].parse().unwrap_or(0);
    let min: i64 = time_parts[1].parse().unwrap_or(0);
    let sec: i64 = time_parts[2].parse().unwrap_or(0);

    // Days from epoch (simplified — no leap second, good enough for ms precision)
    let days = days_from_epoch(year, month, day);
    let secs = days * 86400 + hour * 3600 + min * 60 + sec;
    (secs as u64) * 1000 + frac_ms
}

fn days_from_epoch(year: i64, month: i64, day: i64) -> i64 {
    // Adjusted month for March-start year
    let m = if month > 2 { month - 3 } else { month + 9 };
    let y = if month > 2 { year } else { year - 1 };
    let era = y / 400;
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn extract_text_from_content(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => extract_text_from_blocks(blocks),
        _ => String::new(),
    }
}

fn extract_text_from_blocks(blocks: &[Value]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
        }
    }
    parts.join("\n")
}

fn extract_tool_result_content(block: &Value) -> String {
    let content = block.get("content").cloned().unwrap_or(Value::Null);
    match content {
        Value::String(s) => s,
        Value::Array(arr) => arr
            .iter()
            .filter_map(|b| {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    b.get("text").and_then(|t| t.as_str()).map(String::from)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => content.to_string(),
    }
}

fn parse_usage(usage_val: &Value) -> Option<Usage> {
    let obj = usage_val.as_object()?;
    Some(Usage {
        input_tokens: obj.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        output_tokens: obj.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        cache_creation_input_tokens: obj
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cache_read_input_tokens: obj
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    })
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        s[..max].to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_to_unix_ms_basic() {
        // 2026-01-01T00:00:00.000Z
        let ms = iso_to_unix_ms("2026-01-01T00:00:00.000Z");
        assert_eq!(ms, 1767225600000);
    }

    #[test]
    fn iso_to_unix_ms_with_fraction() {
        let ms = iso_to_unix_ms("2026-05-26T02:17:50.558Z");
        assert!(ms > 1767225600000); // after 2026-01-01
        assert_eq!(ms % 1000, 558);
    }

    #[test]
    fn convert_user_message() {
        let ev = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": "hello world"},
            "timestamp": "2026-05-26T00:00:00.000Z",
            "uuid": "abc",
        });
        let result = convert_one_event(&ev, 1);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].payload.kind(), "user_message");
    }

    #[test]
    fn convert_assistant_with_tool_use() {
        let ev = serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "model": "claude-opus-4-7",
                "content": [
                    {"type": "text", "text": "Let me check."},
                    {"type": "tool_use", "id": "tc1", "name": "Bash", "input": {"command": "ls"}}
                ],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 1000, "output_tokens": 50, "cache_read_input_tokens": 500, "cache_creation_input_tokens": 0}
            },
            "timestamp": "2026-05-26T00:00:01.000Z",
        });
        let result = convert_one_event(&ev, 1);
        assert_eq!(result.len(), 2); // tool_call + assistant_message
        let kinds: Vec<&str> = result.iter().map(|e| e.payload.kind()).collect();
        assert!(kinds.contains(&"tool_call"));
        assert!(kinds.contains(&"assistant_message"));
    }
}
