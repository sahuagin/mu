//! Prompt-time kx document-index hints.
//!
//! Claude Code has a `UserPromptSubmit` hook that runs `agent kx recall` on the
//! user's prompt and injects a small "possibly relevant docs" block. mu does not
//! have frontend hooks at that layer, so the agent loop owns the equivalent as a
//! transient rope span: rank the current user intent through `agent kx recall`,
//! inject the compact result immediately after the last user span, and never add
//! it to conversation history. The hint is best-effort sugar, not load-bearing.
//!
//! This is deliberately opt-in (`[recall].kx = true`) because kx recall may call
//! the configured embedder. It is also byte-capped and memoized by the loop per
//! user intent so a tool round does not re-run recall or mutate the prompt.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::context::rope::{RetainedRope, RetentionClass, Span, SpanKind};

/// Stable rope span id for the injected kx hint.
pub const KX_HINT_SPAN_ID: &str = "kx-hint";

/// Hard byte ceiling on the rendered hint.
pub const KX_HINT_MAX_BYTES: usize = 1200;

const KX_HINT_TIMEOUT: Duration = Duration::from_secs(3);

const DESC_MAX_CHARS: usize = 160;
const PATH_MAX_CHARS: usize = 180;
const TAGS_MAX_CHARS: usize = 120;

#[derive(Clone)]
pub struct KxHints {
    binary_path: PathBuf,
    limit: usize,
    min_score: f32,
    max_bytes: usize,
    warned_about_missing_binary: Arc<AtomicBool>,
}

impl std::fmt::Debug for KxHints {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KxHints")
            .field("binary_path", &self.binary_path)
            .field("limit", &self.limit)
            .field("min_score", &self.min_score)
            .field("max_bytes", &self.max_bytes)
            .finish()
    }
}

impl KxHints {
    pub fn new(binary_path: impl Into<PathBuf>) -> Self {
        Self {
            binary_path: binary_path.into(),
            limit: 4,
            min_score: 0.60,
            max_bytes: KX_HINT_MAX_BYTES,
            warned_about_missing_binary: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn default_binary() -> Self {
        let path = dirs::home_dir()
            .map(|h| h.join(".local").join("bin").join("agent"))
            .unwrap_or_else(|| PathBuf::from("agent"));
        Self::new(path)
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit.max(1);
        self
    }

    pub fn with_min_score(mut self, min_score: f32) -> Self {
        self.min_score = min_score;
        self
    }

    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes.max(1);
        self
    }

    /// Run `agent kx recall` for `intent` and render a compact hint. All
    /// failures degrade to no hint: prompt-time recall must never block the turn
    /// with a user-visible error.
    pub fn render_for_intent(&self, intent: &str) -> Option<String> {
        if intent.trim().is_empty() {
            return None;
        }

        let mut command = Command::new(&self.binary_path);
        command
            .arg("kx")
            .arg("recall")
            .arg(intent)
            .arg("--json")
            .arg("--k")
            .arg(self.limit.to_string())
            .arg("--min-score")
            .arg(format!("{:.3}", self.min_score))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Defense in depth for the ambient-billing incident class: kx recall
        // resolves its embedder key from ~/.config/agent/config.toml, so it never
        // needs metered provider creds in the env. Scrub every ANTHROPIC* var,
        // OPENROUTER_API_KEY, and the Bedrock/Vertex selectors so an ambient key
        // cannot route paid calls through the subprocess.
        for (key, _) in std::env::vars() {
            if key.starts_with("ANTHROPIC")
                || key == "OPENROUTER_API_KEY"
                || key == "CLAUDE_CODE_USE_BEDROCK"
                || key == "CLAUDE_CODE_USE_VERTEX"
            {
                command.env_remove(key);
            }
        }
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if !self
                    .warned_about_missing_binary
                    .swap(true, Ordering::Relaxed)
                {
                    tracing::warn!(
                        binary = %self.binary_path.display(),
                        "KxHints: agent CLI not found; prompt-time kx recall disabled",
                    );
                }
                return None;
            }
            Err(e) => {
                tracing::warn!(
                    binary = %self.binary_path.display(),
                    error = %e,
                    "KxHints: failed to spawn agent CLI",
                );
                return None;
            }
        };

        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if start.elapsed() < KX_HINT_TIMEOUT => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    tracing::warn!(
                        binary = %self.binary_path.display(),
                        timeout_ms = KX_HINT_TIMEOUT.as_millis(),
                        "KxHints: agent kx recall timed out; omitting prompt hint",
                    );
                    return None;
                }
                Err(e) => {
                    let _ = child.kill();
                    tracing::warn!(
                        binary = %self.binary_path.display(),
                        error = %e,
                        "KxHints: failed while waiting for agent kx recall",
                    );
                    return None;
                }
            }
        }

        let output = match child.wait_with_output() {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(
                    binary = %self.binary_path.display(),
                    error = %e,
                    "KxHints: failed to collect agent kx recall output",
                );
                return None;
            }
        };

        if !output.status.success() {
            let stderr_excerpt: String = String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(200)
                .collect();
            tracing::warn!(
                binary = %self.binary_path.display(),
                status = ?output.status.code(),
                stderr = %stderr_excerpt,
                "KxHints: agent kx recall exited non-zero",
            );
            return None;
        }

        let parsed: KxRecallOutput = match serde_json::from_slice(&output.stdout) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    binary = %self.binary_path.display(),
                    error = %e,
                    "KxHints: agent kx recall returned non-JSON",
                );
                return None;
            }
        };
        if let Some(error) = parsed.error.as_deref() {
            tracing::debug!(error, "KxHints: agent kx recall reported no usable result");
        }
        format_hint(&parsed.results, self.max_bytes)
    }
}

#[derive(Debug, Deserialize)]
struct KxRecallOutput {
    #[serde(default)]
    results: Vec<KxHit>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct KxHit {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub score: Option<f64>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub tags: Option<String>,
    #[serde(default)]
    pub date: Option<String>,
}

pub fn format_hint(hits: &[KxHit], max_bytes: usize) -> Option<String> {
    let mut out = String::from(
        "[kx hints — relevant indexed docs for this turn; open/read if useful, ignore if not]",
    );
    let mut entries = 0usize;
    for hit in hits {
        let desc = hit.description.as_deref().unwrap_or("").trim();
        let path = hit.path.as_deref().unwrap_or("").trim();
        let tags = hit.tags.as_deref().unwrap_or("").trim();
        let date = hit.date.as_deref().unwrap_or("").trim();
        let score = hit
            .score
            .map(|s| format!("{}", (s * 100.0).round() as i64))
            .unwrap_or_else(|| "?".to_string());
        let mut meta = String::new();
        if !date.is_empty() {
            meta.push_str(date);
        }
        if !tags.is_empty() {
            if !meta.is_empty() {
                meta.push_str(" · ");
            }
            meta.push_str(&truncate(tags, TAGS_MAX_CHARS));
        }
        let line = format!(
            "\n• [{score}] {} — {}{}{}",
            truncate(&hit.name, DESC_MAX_CHARS),
            truncate(desc, DESC_MAX_CHARS),
            if path.is_empty() {
                String::new()
            } else {
                format!(" — {}", truncate(path, PATH_MAX_CHARS))
            },
            if meta.is_empty() {
                String::new()
            } else {
                format!(" ({meta})")
            }
        );
        if out.len() + line.len() > max_bytes {
            break;
        }
        out.push_str(&line);
        entries += 1;
    }
    (entries > 0).then_some(out)
}

fn truncate(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in s.chars().enumerate() {
        if idx >= max_chars {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

pub fn with_kx_hint_after_last_user(rope: &RetainedRope, hint: &str) -> RetainedRope {
    let spans = rope.spans();
    let Some(pos) = spans.iter().rposition(|s| matches!(s.kind, SpanKind::User)) else {
        return rope.clone();
    };
    let mut out: Vec<Span> = Vec::with_capacity(spans.len() + 1);
    out.extend_from_slice(&spans[..=pos]);
    out.push(Span::new(
        KX_HINT_SPAN_ID,
        SpanKind::User,
        hint,
        RetentionClass::Hot,
    ));
    out.extend_from_slice(&spans[pos + 1..]);
    RetainedRope::from_spans(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentMessage, ToolSpec};
    use crate::context::assemble_rope;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    fn hit(name: &str) -> KxHit {
        KxHit {
            name: name.to_string(),
            description: Some("a useful design note".to_string()),
            score: Some(0.82),
            path: Some("/repo/experiments/note.md".to_string()),
            tags: Some("repo:mu,topic:experiments".to_string()),
            date: Some("2026-07-01".to_string()),
        }
    }

    #[test]
    fn format_hint_renders_compact_docs() {
        let hint = format_hint(&[hit("note")], KX_HINT_MAX_BYTES).expect("hint");
        assert!(hint.starts_with("[kx hints"));
        assert!(hint.contains("• [82] note — a useful design note"));
        assert!(hint.contains("/repo/experiments/note.md"));
    }

    #[test]
    fn format_hint_none_when_empty_or_too_small() {
        assert_eq!(format_hint(&[], KX_HINT_MAX_BYTES), None);
        assert_eq!(format_hint(&[hit("note")], 10), None);
    }

    #[test]
    fn inserts_after_last_user_span() {
        let rope = assemble_rope(
            Some("system"),
            &[AgentMessage::User {
                content: "hello".into(),
            }],
            &[ToolSpec::new("read", "read", json!({}))],
        );
        let rope = with_kx_hint_after_last_user(&rope, "kx docs");
        let ids: Vec<&str> = rope.spans().iter().map(|s| s.id()).collect();
        let user_pos = rope
            .spans()
            .iter()
            .position(|s| matches!(s.kind, SpanKind::User) && s.id() == "msg-0-user")
            .unwrap();
        assert_eq!(ids[user_pos + 1], KX_HINT_SPAN_ID);
    }

    #[test]
    fn render_for_intent_invokes_agent_kx_recall_json() {
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("mu-kx-hint-test-{pid}"));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("agent");
        std::fs::write(
            &script,
            r#"#!/bin/sh
printf '%s\n' "{\"results\":[{\"name\":\"doc\",\"description\":\"desc\",\"score\":0.91,\"path\":\"/tmp/doc.md\",\"tags\":\"topic:test\",\"date\":\"2026-07-02\"}]}"
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let hint = KxHints::new(&script)
            .with_limit(2)
            .with_min_score(0.5)
            .render_for_intent("test")
            .expect("hint");
        assert!(hint.contains("doc"));
        assert!(hint.contains("/tmp/doc.md"));
    }
}
