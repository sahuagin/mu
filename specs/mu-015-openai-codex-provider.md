# Spec: OpenAI-via-Codex provider (subprocess wrapper)

| field      | value                                          |
| ---------- | ---------------------------------------------- |
| spec_id    | mu-015                                         |
| status     | ready                                          |
| created    | 2026-05-10                                     |
| updated    | 2026-05-10                                     |
| authors    | tcovert + claude-personal (claude-opus-4.7)    |
| supersedes | none                                           |

## Why

Second provider. Per AGENTS.md "no third-party-OAuth-token-holding"
rule, OpenAI's OAuth (Pro account) requires a subprocess wrapper.
We delegate to `pi --provider openai-codex` (the same invocation
agent-router uses) and capture stdout as the assistant's response.

After this lands, `mu serve --provider openai-codex` runs an agent
backed by GPT-5.5 (or whichever Codex model is configured), using
the user's OpenAI Pro OAuth.

Crucial v1 limitation: **no streaming, no tool support.** pi's
print-mode is one-shot text. Tool calls don't round-trip through
this path. For the "delegate that does a self-contained task in
one turn" use case (the user's stated MVP — see flywheel
ecosystem context), this is fine. For multi-turn tool-using
sessions, use Anthropic.

## Scope

- **In:**
  - `crates/mu-ai/src/providers/openai_codex.rs` — `OpenaiCodexProvider`
    struct implementing `Provider`. Spawns
    `pi --provider openai-codex --model <m> -p <prompt> --thinking medium`,
    captures stdout, returns it as a single TextDelta + Done.
  - `crates/mu-ai/src/lib.rs` — `pub use providers::OpenaiCodexProvider`.
  - `crates/mu-ai/src/providers/mod.rs` — `pub mod openai_codex;` +
    `pub use openai_codex::OpenaiCodexProvider`.
  - `crates/mu-coding/src/serve/factory.rs` — add `"openai-codex"`
    arm to `build_provider`.
  - Unit tests covering: prompt-flattening from message vec, tool-
    less stream behavior, error path when pi exits non-zero.
  - Live integration test gated on `MU_LIVE_OPENAI_CODEX=1`.

- **Out:**
  - Streaming. v1 buffers the whole response and returns it at once.
    Future spec can add streaming if pi gains a streaming mode.
  - Tool support. v1's request side has no way to send tools to pi,
    and the response side has no way to parse tool_use from text.
    Future spec when pi (or some replacement) supports it.
  - Direct OpenAI API + key. Per the no-token-holding rule, that's
    a separate guarded path. Could spec later if user wants.
  - Per-message system prompt. v1 uses pi's defaults.

## Invariants

- **INV-1 (CONVENTIONS apply).**
- **INV-2 (no token holding).** Provider holds NO OpenAI tokens.
  pi handles all OAuth via `~/.pi/agent/auth.json`. mu spawns pi
  as a subprocess; tokens never enter mu's address space.
- **INV-3 (pi binary lookup).** Provider locates pi via, in order:
  (a) `MU_PI_BINARY` env var (override for tests / unusual installs);
  (b) `PATH` lookup for `pi`. Fail clearly if neither resolves.
- **INV-4 (cancel honored).** When `cancel_rx` fires mid-call, the
  subprocess is killed. Same shape as the file-tool tools' cancel
  handling.
- **INV-5 (errors as ProviderError, not `is_error`).** Provider
  errors surface via `ProviderError::Other`. (Tools use
  `is_error: true` because they're inside the loop's normal flow;
  Provider errors are out-of-band — the loop terminates with
  `Outcome::Error`.)
- **INV-6 (file size).** Module under 500 lines including tests.

## Interfaces

```rust
pub struct OpenaiCodexProvider {
    model: String,
    thinking: String,  // "minimal" | "low" | "medium" | "high" | "xhigh"
}

impl OpenaiCodexProvider {
    pub fn new(model: String) -> Self {
        Self {
            model,
            thinking: "medium".to_string(),
        }
    }

    pub fn with_thinking(mut self, thinking: String) -> Self {
        self.thinking = thinking;
        self
    }
}

#[async_trait]
impl Provider for OpenaiCodexProvider {
    async fn stream(
        &self,
        messages: &[AgentMessage],
        _tools: &[ToolSpec],  // v1: ignored
        cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let prompt = flatten_messages(messages);
        let pi_binary = locate_pi()?;
        let response = spawn_and_wait(&pi_binary, &self.model, &self.thinking, &prompt, cancel_rx).await?;

        // v1: emit one TextDelta + Done.
        let text = response.clone();
        let done = AssistantMessage {
            content: vec![ContentBlock::Text { text }],
            stop_reason: StopReason::EndTurn,
        };
        Ok(Box::pin(futures::stream::iter(vec![
            ProviderEvent::TextDelta(response),
            ProviderEvent::Done(done),
        ])))
    }
}

fn flatten_messages(messages: &[AgentMessage]) -> String {
    let mut out = String::new();
    for m in messages {
        match m {
            AgentMessage::User { content } => out.push_str(&format!("User: {content}\n\n")),
            AgentMessage::Assistant(a) => {
                let text: String = a.content.iter().filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                }).collect::<Vec<_>>().join("");
                if !text.is_empty() {
                    out.push_str(&format!("Assistant: {text}\n\n"));
                }
            }
            AgentMessage::ToolResult { content, .. } => {
                // v1: tool results flatten as user-side text, lossy
                out.push_str(&format!("[tool result] {content}\n\n"));
            }
        }
    }
    out
}

fn locate_pi() -> Result<String, ProviderError> {
    if let Ok(p) = std::env::var("MU_PI_BINARY") {
        if !p.is_empty() {
            return Ok(p);
        }
    }
    // Use which-style lookup. On FreeBSD, `which pi` works.
    use std::process::Command;
    let out = Command::new("which").arg("pi").output()
        .map_err(|e| ProviderError::Other(format!("which lookup failed: {e}")))?;
    if !out.status.success() {
        return Err(ProviderError::Other("pi binary not found in PATH".into()));
    }
    let path = String::from_utf8(out.stdout)
        .map_err(|_| ProviderError::Other("pi path not utf-8".into()))?
        .trim().to_string();
    if path.is_empty() {
        return Err(ProviderError::Other("pi binary not found in PATH".into()));
    }
    Ok(path)
}

async fn spawn_and_wait(
    pi: &str, model: &str, thinking: &str, prompt: &str,
    cancel_rx: oneshot::Receiver<()>,
) -> Result<String, ProviderError> {
    let mut child = tokio::process::Command::new(pi)
        .arg("--provider").arg("openai-codex")
        .arg("--model").arg(model)
        .arg("-p").arg(prompt)
        .arg("--thinking").arg(thinking)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| ProviderError::Other(format!("spawn pi: {e}")))?;

    let wait_fut = async { child.wait_with_output().await };

    tokio::select! {
        out = wait_fut => {
            let out = out.map_err(|e| ProviderError::Other(format!("wait: {e}")))?;
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                return Err(ProviderError::Other(format!("pi exited {}: {stderr}", out.status)));
            }
            String::from_utf8(out.stdout).map_err(|_| ProviderError::Other("pi stdout not utf-8".into()))
        }
        _ = cancel_rx => {
            // Cancel: child is moved already by wait_with_output. Best-effort:
            // we can't kill from here, but the task ending will eventually
            // reap. Document v1 limitation: cancel doesn't kill the subprocess
            // immediately.
            Err(ProviderError::Other("cancelled".into()))
        }
    }
}
```

## Behaviors

1. **B-1 (flatten user message):** `flatten_messages([User{"hi"}])`
   → `"User: hi\n\n"`.
2. **B-2 (flatten multi-turn):** User → Assistant → User flattens
   to three blocks in order, each suffixed with `\n\n`.
3. **B-3 (flatten tool result as user-side):** ToolResult with
   content "foo" flattens as `"[tool result] foo\n\n"`. (Lossy v1
   shape.)
4. **B-4 (locate_pi from env var):** With `MU_PI_BINARY=/some/path`
   set, `locate_pi()` returns that path without consulting PATH.
   With `MU_PI_BINARY=""` empty, falls back to PATH.
5. **B-5 (locate_pi from PATH):** With no env var, `locate_pi()`
   returns `which pi` output. Test only runs on systems where pi
   is in PATH.
6. **B-6 (locate_pi failure):** With `MU_PI_BINARY=""` and a `PATH`
   that doesn't contain pi (use `MU_PI_BINARY=/nonexistent` if env
   override is the only path tested), returns
   `Err(ProviderError::Other("pi binary not found"))`.
7. **B-7 (live API smoke):** Gated on `MU_LIVE_OPENAI_CODEX=1`.
   Build provider with model="gpt-5.5", send a single user message
   "Reply with the single word 'hello' and nothing else." Drain
   stream, assert final Done's text contains "hello".

## Acceptance

- New file: `crates/mu-ai/src/providers/openai_codex.rs`.
- Modified: `crates/mu-ai/src/lib.rs`, `crates/mu-ai/src/providers/mod.rs`,
  `crates/mu-coding/src/serve/factory.rs`.
- `cargo build` clean.
- `cargo nextest run` passes — every existing test plus B-1..B-6
  (B-7 skipped without env var).
- With `MU_LIVE_OPENAI_CODEX=1` and OpenAI Pro auth configured: B-7
  also passes.
- Module under 500 lines.

## Out-of-circuit warnings

- **OOC-1:** `tokio::process::Command::wait_with_output` consumes
  the child. Once we await it inside `tokio::select!`, we can't
  send a kill signal from another arm. The cancel arm in v1
  returns an error but doesn't actively kill; the subprocess will
  finish naturally and its output is dropped. Future spec can use
  `Child::id()` + `Child::start_kill()` for active termination.
- **OOC-2:** pi's stderr can be noisy (warnings about model
  lookups, etc.) but the response is on stdout. Tests should
  capture both but only assert on stdout.
- **OOC-3:** Don't try to add streaming via parsing pi's stdout
  line-by-line. pi may emit progress messages that aren't part of
  the response. v1 reads the FULL stdout after the process exits
  and treats it as the response.

## Prior work

- mu-006 — AnthropicProvider (HTTP-based; structurally different).
- agent-router script — established pi invocation shape.

## Changelog

- 2026-05-10 — initial draft (claude-personal).
