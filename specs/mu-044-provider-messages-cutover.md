# mu-044 — Provider messages cutover

| field          | value                                                              |
| -------------- | ------------------------------------------------------------------ |
| spec_id        | mu-044                                                             |
| status         | proposed                                                           |
| created        | 2026-05-21                                                         |
| authors        | tcovert + claude (cooperating sessions)                            |
| supersedes     | mu-fb0 (partial — see §History)                                    |
| tracking bead  | mu-yqeq                                                            |
| related specs  | `architecture/event-sourced-context.md`, `architecture/capability-delegation.md`, `architecture/mu-capability-substrate.md`, `architecture/session-lifecycle.md` |

## Why

The agent loop assembles a `RetainedRope` per turn, renders it through a `ProviderRenderer` into `ProviderMessages`, annotates cache boundaries via a `CacheStrategy`, emits a `ContextAssembly` event — and then **discards** the projection. The actual provider call at `crates/mu-core/src/agent/loop_/mod.rs:818` is `let _ = projection;`, and `provider.stream()` is invoked with the un-projected `&[AgentMessage]` at `:847`.

The result: compaction, cache annotation, and the rope's pointer-set discipline have **zero runtime effect on what the model sees today**. The render+annotate machinery is theater around an inert load-bearing call site.

This spec closes that gap: cut over the live agent-loop path so it actually sends the projected, cache-annotated `ProviderMessages` to the provider.

## History

[mu-fb0](#) (closed 2026-05-14 in PR #16) claimed this work but its acceptance criterion — *"integration test confirms no regression in produced provider payloads"* — was satisfied by keeping the old `&[AgentMessage]` path live and adding the rope projection *alongside*. The render+annotate machinery was added; the projection was discarded; the bead closed green. mu-yqeq exists to clean up that slippage and finish the cutover.

A goal-protocol pre-flight survey on 2026-05-20 surfaced that the cutover, as originally framed ("2-3 iter cycles, swap parameter type"), is materially bigger because `ProviderMessage.content` is `String`-typed and today's `flatten_assistant` in `crates/mu-core/src/context/assembly.rs:118-170` discards tool-call structure (`ToolCall.id`, `ToolCall.arguments` JSON, `ContentBlock::Thinking` blocks) at rope ingestion time. The three real providers (Anthropic, OpenAI Codex, OpenRouter) all need that structure on the wire. This spec captures the resulting decomposition into seven implementation sub-beads under mu-yqeq.

## What this changes

Six concrete changes, landing across seven sub-beads:

1. **Type aliases for immutable content storage** — `Span.content`, `Span.id`, `ProviderMessage.content`, `ContentBlock::Text.text`, `ContentBlock::Thinking.text` all become aliases over `Arc<str>` (e.g., `SpanText`, `SpanId`, `MessageText`, `BlockText`). Each alias is per-conceptual-type so any one can be replaced with a newtype later without churning the rest. See §Structural decision.
2. **Encapsulation discipline on Span and ProviderMessage** — fields move from `pub` to `pub(crate)`; public accessor methods replace external direct-field access. Zero runtime cost (the compiler inlines); localizes future field-type evolution to the type's impl block. Landed together with the Arc<str> migration in mu-yqeq.2 (same files, same construction sites, same compiler-guided sweep). See §Encapsulation discipline.
3. **Parallel `blocks` field on `Span` and `ProviderMessage`** — `Option<Vec<ContentBlock>>`. `None` for non-assistant spans, `Some(...)` for assistant turns with tool calls. The existing flat `content` field remains authoritative for non-structural consumers; the new field is structural data for wire adapters.
4. **Provider trait signature change** — the existing `stream` method's `messages: &[AgentMessage]` parameter becomes `input: MessageInput<'_>`, a sealed enum (`#[non_exhaustive]`) with `Legacy(&[AgentMessage])` and `Projected(&ProviderMessages)` variants. Each provider's impl dispatches via `match`. No new method; no deprecation phase; uniform shape at the call boundary, internal dispatch per provider — the shmq pattern applied to the provider trait. (Design decision 2026-05-21 — see §History below.)
5. **Per-provider wire adapters** — each of the four providers (Faux, Anthropic, OpenAI Codex, OpenRouter) extends its `match` arm to handle `MessageInput::Projected` and produce byte-identical wire JSON to today's `Legacy` path. Existing wire-format regression tests are the pinning.
6. **Call-site cutover + legacy cache-annotation retirement** — `mod.rs:818` stops discarding the projection and passes `MessageInput::Projected(&projection)` to `provider.stream(...)`. The duplicate `cache_control: ephemeral` annotation in `crates/mu-ai/src/providers/anthropic.rs:232-240, 259-261` retires; `AnthropicCacheStrategy` becomes the sole source.

## Structural decision

The cutover requires the rope to carry structural tool-call data to the providers. Three options were on the table during pre-flight; **option (a) is locked**.

### Option (a) — Parallel `blocks` field (LOCKED)

`Span` and `ProviderMessage` gain a new field `blocks: Option<Vec<ContentBlock>>`. The existing flat `content` field continues to exist and remain authoritative for:

- `/context why` diffing
- `estimate_tokens` (`crates/mu-core/src/context/renderer.rs:236-242`)
- `summary_line` (`crates/mu-core/src/context/renderer.rs:275-283`)
- `RopeEvent::CompactionSummary` projection (`crates/mu-core/src/context/rope.rs:145-159`)
- OperatorView rendering (`crates/mu-core/src/context/renderer.rs:296-326`)
- Compaction policies (`crates/mu-core/src/context/compaction/heuristic.rs:106-192`)

The new `blocks` field is consumed by:

- Wire adapters in Phase C — each provider walks `blocks` directly to produce structured wire JSON instead of parsing the flat string.
- Future OperatorView consumers that want structural fidelity.

### Why option (a) wins

1. **Additive, non-breaking.** `blocks` defaults to `None`. Existing serde, comparison, and `&str` consumers continue to work unchanged. Each sub-bead lands as a green-test atomic commit.
2. **Content is load-bearing AND immutable.** 30+ call sites read `Span.content` — all read-only. The Arc-based storage (below) keeps every one of them working while making clones cheap.
3. **Per-provider opt-in.** Each Phase C provider extends its `match` to handle `MessageInput::Projected` independently. Until a provider's Phase C bead lands, its `Projected` arm returns an error; the call site continues to pass `Legacy` until Phase D. C-beads land in any order without breaking each other.
4. **Round-trip provability.** A Phase A test asserts `ProviderMessages.messages[i].blocks → reconstructed AssistantMessage.content` is byte-identical to the originating `AgentMessage`. This is the regression net for the cutover.

### Alternatives considered and rejected

- **Option (b) — enum on `Span.content`** (e.g., `enum SpanContent { Text(Arc<str>), Tool(...) }`): rejected because workspace-wide churn can't be one-bead-per-commit. Every read site needs a `match`.
- **Option (c) — string-grammar parser** (encode tool structure in flat content with a defined grammar): rejected because fragile — tool args are arbitrary JSON, embedded markers collide, `tool_use_id` recovery still requires out-of-band data.
- **Option (d) — lazy `content()` method** (replace `content` field with method that walks `blocks` on demand): a viable follow-on optimization if ingestion-time allocation ever shows up as hot in profiling. Out of scope for mu-yqeq; the eager-flat + eager-structured combination is sufficient and preserves all existing consumer APIs.

### Content storage: `Arc<str>` with per-type aliases

The cutover changes the storage type of all immutable content fields from `String` to `Arc<str>`, but **does not** introduce a single universal alias for all `Arc<str>` uses. Each conceptual type gets its own alias:

| Alias         | Underlying     | Used by                                                                       |
| ------------- | -------------- | ----------------------------------------------------------------------------- |
| `SpanText`    | `Arc<str>`     | `Span.content` (`crates/mu-core/src/context/rope.rs`)                         |
| `SpanId`      | `Arc<str>`     | `Span.id` (`crates/mu-core/src/context/rope.rs`)                              |
| `MessageText` | `Arc<str>`     | `ProviderMessage.content` (`crates/mu-core/src/context/renderer.rs`)          |
| `BlockText`   | `Arc<str>`     | `ContentBlock::Text { text }`, `ContentBlock::Thinking { text }` (`crates/mu-core/src/agent/types.rs`) |
| `ToolArgs`    | `Arc<Value>`   | `ToolCall.arguments` (`crates/mu-core/src/agent/types.rs`) — optional in A.pre; can defer |

**Rationale (per gpt-5.5's recommendation, 2026-05-20 session-2):** type aliases per conceptual type — not one universal alias — make individual types independently replaceable with newtypes later. If, for example, we later want `SpanId` to be a `struct SpanId(Arc<str>)` newtype for stronger typing (preventing accidental confusion between span IDs and arbitrary `Arc<str>` values), the change is local: the alias becomes a newtype, downstream code that uses `SpanId` stays correct, code that uses `SpanText` or `BlockText` is untouched. Conversely, if we'd used a single `MuStr = Arc<str>` alias everywhere, swapping any one site for a newtype would either drag the rest with it or create asymmetry.

Aliases also document intent in signatures. `fn span_for(id: SpanId) -> Span` reads as "this takes a span identifier" rather than "this takes an Arc<str>." Code-reading benefit is real.

**Why `Arc<str>` and not `String`:** the rope's content is constructed once at ingestion (in `assemble_rope` and `message_to_span` at `assembly.rs:118-170`) and never mutated afterward — verified: no `&mut span.content` in the codebase. The clone hot path (`compaction_baseline.rope.clone()` at `loop_/mod.rs:807-810`, `bg_compaction.start(...rope.clone()...)` at `:785-790`, `assemble_rope`/`append_messages_to_baseline` at `assembly.rs:106-116`) currently deep-copies every span's heap-allocated `String`. With `Arc<str>`, clones become 8-byte refcount bumps. The clones are load-bearing for compaction baselines and background tasks; the savings are real, not premature.

**Why also `ContentBlock::Text { text: Arc<str> }`:** this enables byte-sharing between the flat `content` projection and the structured `blocks` field. When `flatten_assistant` concatenates assistant blocks into the flat string, the chunks in `blocks` and the resulting flat content point at the same underlying bytes through Arc. Storing both views is byte-cheap.

**Why `ToolArgs` is optional in A.pre:** `serde_json::Value` clones recursively copy all internal strings. Wrapping in `Arc<Value>` would make tool-call argument clones cheap too, mirroring the `Arc<str>` rationale for text. The work is mechanical, and the bead can either include it or defer to a follow-on. The spec endorses inclusion; the bead body documents the optional split.

### Serde compatibility

`Arc<str>` and `Arc<Value>` need the `rc` feature on serde for round-trip serialization. The workspace `Cargo.toml` will be updated:

```toml
serde = { version = "...", features = ["rc"] }
```

This is opt-in; it doesn't change existing serde behavior for non-Arc types.

## Encapsulation discipline

In addition to the storage-shape changes, mu-yqeq.2 hardens encapsulation on the two types whose fields are most likely to evolve: `Span` and `ProviderMessage`. Fields move from `pub` to `pub(crate)`; public accessor methods replace direct field access from outside `mu_core::context`.

Rationale: in Rust, inline accessor methods compile to identical assembly as direct field access — the compiler sees through them. The only cost is lines of code; the benefit is that future evolution of the field type or storage strategy (lazy content materialization, alternate representations, validation, etc.) is localized to the type's `impl` block rather than spread across every external call site. The String → `Arc<str>` migration in this bead is the first evidence that these fields *do* evolve; encapsulating now prevents the next such migration from churning external call sites.

```rust
pub struct Span {
    pub(crate) id: SpanId,
    pub(crate) kind: SpanKind,
    pub(crate) content: SpanText,
    pub(crate) retention: RetentionClass,
    pub(crate) cacheable: bool,
    pub(crate) blocks: Option<Vec<ContentBlock>>,
}

impl Span {
    pub fn id(&self) -> &SpanId { &self.id }
    pub fn kind(&self) -> SpanKind { self.kind }
    pub fn content(&self) -> &str { &self.content }
    pub fn retention(&self) -> RetentionClass { self.retention }
    pub fn cacheable(&self) -> bool { self.cacheable }
    pub fn blocks(&self) -> Option<&[ContentBlock]> { self.blocks.as_deref() }
}
```

`ProviderMessage` follows the same pattern.

### What is NOT encapsulated

- **`ContentBlock`** — it's an enum, and `match` IS the abstraction layer. Adding `as_text()`-style accessors would compete with the natural match-based discrimination and make call sites awkward. Variant-destructuring stays the access pattern.
- **`ToolCall`** — fields are simple values, less likely to evolve. Encapsulation can be added later (same pattern) if pressure emerges.
- **Primitive newtype-style structs** (`CacheBoundary { message_index: usize }`, etc.) — overkill. Direct field access is fine.

The principle: encapsulate where evolution pressure is likely or where the field type might change. `Span.content` and `ProviderMessage.content` have already moved `String → Arc<str>` in this bead and might evolve again. Other fields stay public until their own evolution pressure surfaces.

### Internal construction unchanged

Inside `mu_core::context`, code can still construct via struct-literal syntax (`Span { id, kind, content, ... }`) because `pub(crate)` allows in-crate access. External crates (`mu_ai`, `mu_coding`, `mu_tui`) gain a small amount of friction — they must use accessor methods — and lose a corresponding amount of coupling to the struct's exact shape.

## Provider trait amendment

### Sealed-enum input + single `stream` method

```rust
/// What the agent loop hands to a provider. The variants represent
/// two stages of mu's rope-projection rollout: `Legacy` is the
/// pre-cutover un-projected `Vec<AgentMessage>` shape; `Projected`
/// is the rendered + cache-annotated rope output. Providers dispatch
/// internally via `match`.
///
/// `#[non_exhaustive]` reserves the right to add variants later
/// (e.g., a streaming pull-based shape) without breaking downstream
/// match arms — they fall through to `_ =>` and the provider can
/// error or no-op as appropriate.
#[non_exhaustive]
#[derive(Debug)]
pub enum MessageInput<'a> {
    /// Pre-cutover path. The agent loop's raw `Vec<AgentMessage>`,
    /// unchanged from how `stream()` consumed it before mu-yqeq.
    Legacy(&'a [AgentMessage]),

    /// Post-cutover path. The rope projection rendered through this
    /// provider's `ProviderRenderer` and annotated by its
    /// `CacheStrategy`. The `blocks` field on each `ProviderMessage`
    /// carries structured ContentBlocks for wire reconstruction.
    Projected(&'a ProviderMessages),
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// Open a streaming response. The signature is stable across the
    /// cutover; the `input` parameter changed from `&[AgentMessage]`
    /// to `MessageInput<'_>` in mu-yqeq.A. Each provider's impl
    /// matches on the variant and dispatches to its own internal
    /// translator.
    async fn stream(
        &self,
        system_prompt: Option<&str>,
        input: MessageInput<'_>,
        tools: &[ToolSpec],
        cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError>;
}
```

### Sample provider impl shape

Each provider's `impl Provider for ...` block has the shape:

```rust
#[async_trait]
impl Provider for AnthropicProvider {
    async fn stream(
        &self,
        system_prompt: Option<&str>,
        input: MessageInput<'_>,
        tools: &[ToolSpec],
        cancel_rx: oneshot::Receiver<()>,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        match input {
            // Pre-cutover path. Existing logic, unchanged.
            MessageInput::Legacy(msgs) => {
                let body = build_request_body(system_prompt, msgs, tools);
                self.send(body, cancel_rx).await
            }

            // Post-cutover path. Initially errors (mu-yqeq.A); the
            // provider's Phase C bead implements this arm and produces
            // byte-identical wire JSON to the Legacy path.
            MessageInput::Projected(pmsgs) => {
                Err(ProviderError::Other(
                    "anthropic: Projected input not yet supported; \
                     mu-yqeq.4 is the bead that implements this arm".into(),
                ))
            }
        }
    }
}
```

### Why one method, not two

- **Uniform boundary.** The call site at `mod.rs:847` calls `provider.stream(...)` with no knowledge of which input shape is in flight; it just constructs the variant. This is the shmq-pattern: control the boundary, make the message shape uniform, dispatch internally.
- **Compiler-enforced exhaustiveness.** Adding a new variant later (mu-yqeq.A.future) forces every provider's `match` to handle it (or explicitly fall through via `_ =>`). No silent omission. C++ shmq required convention; Rust enforces it.
- **No deprecation phase.** There's no second method to mark `#[deprecated]` and later remove. The migration is the gradual filling-in of `Projected` arms; the legacy `Legacy` arm stays as long as any caller might construct it (which today is mu-yqeq.A, .C, until D's call-site swap).
- **Cheap.** The enum is ~24 bytes on the stack (discriminant + the larger variant, which is `&ProviderMessages` ≈ 16 bytes including width). No heap allocation. No vtable indirection beyond what the trait already has. The C++ shmq concern about size disparity doesn't apply here because both variants are slice references, not inlined payloads.

### Test fakes

Test fakes that implement `Provider` (`crates/mu-core/src/agent/loop_/tests.rs:66, 1768`) update their `match` in mu-yqeq.A — initially only the `Legacy` arm matters (test call sites pass `Legacy`). When test coverage extends to the `Projected` path, the fakes extend their arms.

## Type changes — field-by-field

### `MessageInput` (`crates/mu-core/src/agent/provider.rs`) — NEW

The sealed-enum dispatch type added in mu-yqeq.A. Full definition in §"Provider trait amendment" above; reference here for completeness:

```rust
#[non_exhaustive]
pub enum MessageInput<'a> {
    Legacy(&'a [AgentMessage]),
    Projected(&'a ProviderMessages),
}
```

### `Span` (`crates/mu-core/src/context/rope.rs`)

```rust
pub type SpanText = Arc<str>;
pub type SpanId = Arc<str>;

pub struct Span {
    pub(crate) id: SpanId,                   // was pub String
    pub(crate) kind: SpanKind,               // visibility changed (was pub)
    pub(crate) content: SpanText,            // was pub String
    pub(crate) retention: RetentionClass,    // visibility changed
    pub(crate) cacheable: bool,              // visibility changed
    pub(crate) blocks: Option<Vec<ContentBlock>>,  // NEW (mu-yqeq.A)
}

// Public accessor methods (mu-yqeq.2):
impl Span {
    pub fn id(&self) -> &SpanId { &self.id }
    pub fn kind(&self) -> SpanKind { self.kind }
    pub fn content(&self) -> &str { &self.content }
    pub fn retention(&self) -> RetentionClass { self.retention }
    pub fn cacheable(&self) -> bool { self.cacheable }
    pub fn blocks(&self) -> Option<&[ContentBlock]> { self.blocks.as_deref() }
}
```

`blocks` is populated in `message_to_span` (`assembly.rs:118-170`) for `Assistant` and `ToolResult` variants and left `None` for all other span kinds.

### `ProviderMessage` (`crates/mu-core/src/context/renderer.rs`)

```rust
pub type MessageText = Arc<str>;

pub struct ProviderMessage {
    pub(crate) role: ProviderRole,                  // was pub
    pub(crate) content: MessageText,                // was pub String
    pub(crate) source_span_ids: Vec<SpanId>,        // element type was String
    pub(crate) cache_marker: Option<CacheMarker>,   // was pub
    pub(crate) blocks: Option<Vec<ContentBlock>>,   // NEW (mu-yqeq.A)
}

// Public accessor methods (mu-yqeq.2):
impl ProviderMessage {
    pub fn role(&self) -> ProviderRole { self.role }
    pub fn content(&self) -> &str { &self.content }
    pub fn source_span_ids(&self) -> &[SpanId] { &self.source_span_ids }
    pub fn cache_marker(&self) -> Option<CacheMarker> { self.cache_marker }
    pub fn blocks(&self) -> Option<&[ContentBlock]> { self.blocks.as_deref() }
}
```

### `ContentBlock` (`crates/mu-core/src/agent/types.rs`)

```rust
pub type BlockText = Arc<str>;
pub type ToolArgs = Arc<serde_json::Value>;  // optional in mu-yqeq.A.pre

pub enum ContentBlock {
    Text { text: BlockText },               // was String
    ToolCall(ToolCall),
    Thinking { text: BlockText },           // was String
}

pub struct ToolCall {
    pub id: Arc<str>,            // was String — alias TBD if it gets its own meaning
    pub name: Arc<str>,          // was String — alias TBD
    pub arguments: ToolArgs,     // was Value (optional Arc-wrap)
}
```

The aliases for `ToolCall.id` and `ToolCall.name` are deferred — they may end up needing their own conceptual names (e.g., `ToolCallId`, `ToolName`) if a future bead introduces stronger typing there. For mu-yqeq.A.pre, they are plain `Arc<str>` with a TBD comment marking the future alias work.

## Wire-equivalence requirement

Each Phase C bead (mu-yqeq.4 through mu-yqeq.7) must produce **byte-identical** wire JSON via the `MessageInput::Projected` match arm compared to today's `MessageInput::Legacy` path (which itself mirrors the pre-cutover `&[AgentMessage]` behavior). The pinning is:

- `crates/mu-ai/src/providers/anthropic_tests.rs` — Anthropic wire-format tests (lines 8, 18, 109, 160, 193, 218, 255)
- `crates/mu-ai/src/providers/openai_codex_tests.rs` — OpenAI Codex wire-format tests (lines 158, 182, 210, 241)
- `crates/mu-ai/src/providers/openrouter_tests.rs` — OpenRouter wire-format tests (lines 7, 17, 31, 61)

Each Phase C bead adds a new test that runs the SAME scenario through both paths (legacy `&[AgentMessage]` and new `&ProviderMessages`) and asserts byte-equal output. Specific cases that must be covered:

- Pure text turn
- Single tool call
- Consecutive tool results (Anthropic groups these into one user message; the new path must reconstruct that grouping)
- System prompt + tools (cache_control positions must match)
- Multi-turn with mixed text + tool calls

A failure of byte-equivalence on any pinned test fails the bead.

## Cache-annotation consolidation (Phase D)

Today the live-loop path tags two cache positions in `build_request_body` (`crates/mu-ai/src/providers/anthropic.rs:232-240, 259-261`):

1. The system block: `cache_control: ephemeral`
2. The last tool spec: `cache_control: ephemeral`

The rope-based path (`AnthropicCacheStrategy::boundaries` at `crates/mu-ai/src/context/anthropic.rs:111-130`) tags ONE position: the last span in the stable+cacheable prefix.

**Pre-condition for Phase D:** `AnthropicCacheStrategy` produces a cache prefix at least as inclusive as the live-loop annotation. If the rope strategy is strictly less inclusive (the doc-comment at `context/anthropic.rs:37-45` admits the strategies are not identical), Phase A or Phase D must extend `AnthropicCacheStrategy` before the legacy annotation can be safely removed.

**Phase D acceptance** includes an integration test measuring `cache_creation_input_tokens` on a recorded fixture against a pre-cutover baseline. The cutover must not reduce cache effectiveness.

### Phase D end-to-end smoke test

Beyond the cache-effectiveness check, Phase D must pass a recorded end-to-end smoke covering the behavioral surface that the cutover touches. The smoke:

- Replays a multi-turn session through the new path (`MessageInput::Projected`) end-to-end: text turn → tool call → tool result → assistant follow-up → enough additional turns to cross the configured compaction threshold and trigger a `CompactionAssembly`.
- Compares the resulting `AssistantMessage` outputs (and emitted events) against a pre-cutover recording of the same input sequence.
- Assertion shape: the new path produces materially equivalent agent behavior — tool calls fire at the same positions with the same names + arguments, tool results are consumed the same way, the compaction summary preserves the spans the pre-cutover recording's compaction preserved.

The smoke is not a byte-equality check on model output (model responses aren't deterministic), but on the *structural decisions* of the agent loop: which tools were invoked, in what order, against what inputs. Wire-format byte-equality is already covered by the per-provider parity tests from Phase C beads; this smoke covers the agent-loop side.

Recorded fixture lives at `tests/fixtures/cutover-smoke/<scenario>.jsonl` (the same fixture infrastructure useful for the sanitized-test-history work tracked separately as an epic-level bead). If the fixture infrastructure isn't yet in place when Phase D lands, the smoke can use an inline test fixture; the bead body documents which path was taken.

### Phase D rollback plan

mu is a single-binary single-user tool with no rollout / canary / feature-flag infrastructure. The revert plan is git/jj:

- If the cutover commit produces a regression visible in normal use, `jj abandon <commit>` (or `git revert <merge>` if the feature branch has merged to main) restores the prior behavior. The legacy `MessageInput::Legacy` arm remains in the enum and in every provider's `match`, so the only change required to revert is at the call site at `mod.rs:818` — flip `Projected` back to `Legacy`.
- Because `MessageInput::Legacy` is still reachable until a follow-on cleanup bead retires it (see §Open questions), the cost of revert is one-line at the call site, not a full re-implementation.
- This is intentional: Phase D doesn't burn the bridge to the legacy path. Burning the bridge is a separate, later decision.

## Thinking-block rule

`ContentBlock::Thinking { text: BlockText }` blocks track the model's reasoning output. The rope retains them via `Span.blocks` so `/context why` and operator-transcript projections can show them. **However, wire adapters MUST filter them out of provider input.** A future contributor "fixing" the empty-content drop in `flatten_assistant` (which today silently drops thinking) could accidentally pump thinking content back into provider input, which is wrong — thinking is an output channel, not an input.

The spec mandates: each Phase C provider's `MessageInput::Projected` arm iterates `blocks` and skips `ContentBlock::Thinking` variants when emitting provider input. A regression test in each Phase C bead asserts: input containing a Thinking block produces wire JSON that does not include the thinking text.

## Migration plan — sub-beads

Eight sub-beads under tracking bead `mu-yqeq`, tagged `goal:2026-05-20:mu-yqeq`. Dependency graph:

```
mu-yqeq.1   This spec (no code)                          [no deps]
   ↓
mu-yqeq.2   Arc<str> migration via per-type aliases       [blocks: .1]
            (SpanText, SpanId, MessageText, BlockText,
             optionally ToolArgs) PLUS encapsulation pass:
            Span and ProviderMessage fields → pub(crate);
            public accessor methods added; external
            crate call sites updated to method form.
   ↓
mu-yqeq.3   blocks: Option<Vec<ContentBlock>> on Span    [blocks: .2]
            and ProviderMessage; assemble_rope populates;
            renderer passes through. Provider trait sig
            changes to take MessageInput<'_>; all four
            providers' impls add match Legacy/Projected
            arms (Projected initially errors). let _ =
            projection at mod.rs:818 UNCHANGED.
   ↓
   ├─────────┬───────────┬──────────────┬──────────┐
   ↓         ↓           ↓              ↓          (any order — independent)
mu-yqeq.4  mu-yqeq.5  mu-yqeq.6     mu-yqeq.7
Anthropic  OpenAI-    OpenRouter    Faux
adapter    Codex      adapter       adapter
           adapter
   ↓         ↓           ↓              ↓
   └─────────┴───────────┴──────────────┘
                       ↓
mu-yqeq.8  Cutover at mod.rs:818 + retire legacy    [blocks: all C]
           build_request_body cache_control.
           Closes mu-yqeq.
```

Recommended Phase C order: **mu-yqeq.4 (Anthropic) first** — most wire-shape transformations (consecutive-tool-result grouping, system block hoisting, structured `tool_use` blocks). If the `blocks` field is insufficient for any provider, Anthropic surfaces it first.

## Risks and mitigations

### Wire-format drift between legacy and new paths (HIGHEST)

Anthropic's `translate_messages` (`crates/mu-ai/src/providers/anthropic.rs:129-169`) groups consecutive `ToolResult` messages into a single user message with multiple `tool_result` content blocks. The new ProviderMessages path emits one `ProviderMessage` per `ToolResult` span. Phase C2 must reconstruct grouping from the `ProviderMessages.messages` sequence.

**Mitigation:** parity-test pattern in each Phase C bead — run the same scenario through both paths, assert byte-equal wire JSON. The existing `b3_consecutive_tool_results_group_into_one_user_message` test (anthropic_tests.rs:109) is the canonical case.

### Cache annotation behavioral regression (HIGH)

Live-loop tags two positions; rope strategy tags one. If the rope strategy is strictly less inclusive, Phase D's deletion of the legacy annotation reduces caching effectiveness.

**Mitigation:** Phase A or Phase D verifies `AnthropicCacheStrategy.boundaries` produces a cache prefix at least as inclusive as the live-loop. If not, extend the strategy before deletion. Integration test measures `cache_creation_input_tokens` against fixture.

### ToolResult call_id recovery (MEDIUM)

`ContentBlock` has no `ToolResult` variant — tool results travel as `AgentMessage::ToolResult { call_id, content, is_error }`. The `call_id` is embedded in `Span.id` as `msg-{idx}-tool-result:{call_id}` (`assembly.rs:139`). Phase A includes a helper to parse `call_id` from a `SpanId`; Phase C wire adapters use this to bind tool results to tool calls.

**Mitigation:** ProviderMessage already carries `source_span_ids`; the helper `fn extract_call_id_from_span_id(&SpanId) -> Option<&str>` is documented in Phase A's bead and tested.

### Thinking content silently misrouted (MEDIUM)

`flatten_assistant` drops `Thinking` blocks today; all providers also drop them on input. A future "fix" could accidentally enable thinking-as-input.

**Mitigation:** spec rule above (rope tracks via blocks, wire adapters filter); each Phase C bead has a regression test asserting thinking blocks are stripped on the wire.

### Compaction baseline preserves `blocks` (MEDIUM)

`append_messages_to_baseline` (`assembly.rs:106-116`) calls `message_to_span` for appended messages. Background compaction (`loop_/mod.rs:743-762`) takes over the rope after its turn. Surviving (non-absorbed) spans must carry their `blocks` through compaction unchanged.

**Mitigation:** Phase A includes a test: assemble → compaction → assemble; assert kept spans retain their `blocks`. `HashAndSummaryPolicy`'s `CompactionSummary` spans have `blocks = None` correctly (their content is a summary string, not assistant output).

### Provider trait amendment ripple (LOW)

Two test fakes implement `Provider` (`crates/mu-core/src/agent/loop_/tests.rs:66, 1768`). They update their `match` in mu-yqeq.A — initially only the `Legacy` arm matters because test call sites pass `Legacy`. The `Projected` arm errors until any test exercising that path lands. No deprecation phase since there is no second method to deprecate; the migration is the gradual filling-in of `Projected` arms.

## Acceptance per phase

| Phase     | Bead       | Acceptance                                                                                             |
| --------- | ---------- | ------------------------------------------------------------------------------------------------------ |
| Spec      | mu-yqeq.1  | This spec lands; operator review on the structural lock and provider-trait amendment proposal.        |
| Arc + encap | mu-yqeq.2  | Type-alias migration AND encapsulation pass green at every commit. `cargo test --workspace` passes. Span and ProviderMessage fields are `pub(crate)`; public accessor methods exist; all external-crate call sites use methods (no `&msg.content` direct access outside `mu_core::context`). Provider wire-format tests pass byte-identically (no behavior change). |
| Blocks    | mu-yqeq.3  | `MessageInput` enum + `blocks` field added; Provider trait sig changes to take `MessageInput<'_>`; all four provider impls (+ test fakes) gain a `match` with `Legacy` (existing logic) and `Projected` (errors). `assemble_rope` and both renderers populate `blocks`. `let _ = projection;` at mod.rs:818 UNCHANGED. Round-trip test passes. Compaction round-trip test passes. |
| C2-C5     | mu-yqeq.4-7 | Each provider's `MessageInput::Projected` match arm produces byte-identical wire JSON via parity test (vs the same provider's `Legacy` arm). Existing provider tests untouched and passing. |
| Cutover   | mu-yqeq.8  | `let _ = projection;` removed; call site passes `MessageInput::Projected(&projection)`. Legacy `cache_control` annotation in `build_request_body` deleted. Integration test verifies `cache_creation_input_tokens` is not reduced vs baseline. **End-to-end smoke test passes** (multi-turn replay through new path vs pre-cutover recording — structural decisions equivalent; see §"Phase D end-to-end smoke test"). Rollback path documented (§"Phase D rollback plan" — `MessageInput::Legacy` remains reachable; revert is a one-line call-site flip). Closes mu-yqeq. |

## Open questions

- **`ToolArgs` inclusion in mu-yqeq.A.pre or follow-on?** Recommended: include in A.pre. If bead diff is large, A.pre can split into A.pre.1 (Span + ProviderMessage + ContentBlock text aliases) and A.pre.2 (`ToolArgs` + ToolCall.id/name aliases). Both still mechanical, both still atomic.
- **`ToolCall.id` and `ToolCall.name` aliases.** Deferred — may want stronger naming (e.g., `ToolCallId`, `ToolName`) if a future bead introduces type-level guarantees there. mu-yqeq.A.pre leaves them as `Arc<str>` with a TBD comment.
- **`MessageInput::Legacy` retirement timing.** After mu-yqeq.D, `Legacy` is unreachable from the production call site, but it remains in the enum (so existing tests/fakes that construct it still work). Removing the variant would be a follow-on cleanup bead: update every match arm and test, then delete the variant. The `#[non_exhaustive]` attribute on the enum means downstream code already had a fallback arm; the cleanup is mechanical but touches every provider impl plus tests.
- **Cache-parity measurement bead between C2 and D.** Optional follow-on: a `specs/measurements/cache-parity-2026-MM-DD.md` recording before/after `cache_creation_input_tokens` to document the no-regression property. The compaction measurement at `specs/measurements/compaction-2026-05-14.md` is the model.

## References

- Tracking bead: `mu-yqeq`. Sub-beads: `mu-yqeq.1` through `mu-yqeq.8` (this spec is mu-yqeq.1).
- Plan: `~/.claude-personal/plans/expressive-painting-puffin.md` (operator-side planning doc)
- Predecessor: `mu-fb0` (closed 2026-05-14 PR #16 — left the cutover deferred)
- Architecture: `specs/architecture/event-sourced-context.md`, `specs/architecture/capability-delegation.md`, `specs/architecture/session-lifecycle.md`
- Type-alias rationale: gpt-5.5 contribution during daemon ebccc9 session-2 (2026-05-20), reinforced by operator instruction 2026-05-21
- Source files referenced throughout:
  - `crates/mu-core/src/context/rope.rs` — `Span`
  - `crates/mu-core/src/context/renderer.rs` — `ProviderMessage`, `ProviderRenderer`
  - `crates/mu-core/src/context/assembly.rs` — `assemble_rope`, `flatten_assistant`, `message_to_span`
  - `crates/mu-core/src/agent/provider.rs` — `Provider` trait
  - `crates/mu-core/src/agent/types.rs` — `ContentBlock`, `ToolCall`, `AgentMessage`
  - `crates/mu-core/src/agent/loop_/mod.rs:818, 847` — the call site
  - `crates/mu-ai/src/context/anthropic.rs` — `AnthropicProviderRenderer`, `AnthropicCacheStrategy`
  - `crates/mu-ai/src/providers/{anthropic,openai_codex,openrouter}.rs` — provider impls
  - `crates/mu-ai/src/faux.rs` — `FauxProvider`
