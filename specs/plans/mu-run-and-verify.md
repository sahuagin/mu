# Scope: a run-and-verify affordance for mu (`mu-lg8j1`)

*Design memo, 2026-09-02, session 442e5925. From the phase-2 harness
experiment (`mu-316wl`, battery-1 writeup PR #583). Scopes the single
feature that battery-1 identified as the largest capability lever; it does
not implement it. Architecture facts below were read from current main
(`301d9690`).*

## The problem this solves

Battery-1 ran the same model (qwen3.8-27b-nvfp4) on the same one-shot
"build a playable Minecraft-style game" prompt through three harnesses.
DeepSeek Harness produced a playable game 3/3; mu produced 1/3. The
difference was not the model, the prompt, or the request envelope — it was
that DeepSeek Harness, mid-run, *ran its own artifact against a real
runtime* (it improvised a headless browser with its general shell, ran the
game, and iterated on what came back), while mu's models verified against a
hand-built Node stub of the DOM. A stub answers every call with a live
object, so it is blind to exactly the runtime-contract bugs that crashed
mu's artifacts: `Cannot read properties of null (reading 'getContext')`,
`Cannot create property 'fillStyle' on string 'grass_top'`, a function
referenced but never defined. Every one of those surfaces the instant the
page loads in a real browser, as an uncaught exception.

So mu needs a first-class, cheap way for a model to run an artifact in its
real runtime and get the runtime's output back into the loop, instead of
improvising one (which costs a dozen turns) or settling for a stub (which
misses the bugs).

## Architecture the design must fit (verified on main)

- **Tool results are text.** `ToolResult.content` is a `String`
  (`mu-core/src/agent/tool.rs:361`). A tool hands the model text, nothing
  else, today.
- **mu has no image path at all.** `ContentBlock` is `Text | ToolCall |
  Thinking` — no `Image` variant (`mu-core/src/agent/types.rs`). mu cannot
  send an image to any model regardless of the model's capability. (The
  operator confirms image handling has never been implemented and has not
  constrained the work so far.)
- **The model and the serve CAN take images, though.** qwen3.8-27b is an
  image-text-to-text model, and the deployed nvfp4 vLLM serve accepts
  `image_url` content (probed 2026-09-02: it described a test pixel). So
  image feedback is grounded, not speculative — it is blocked only by mu's
  own missing plumbing, and that plumbing is a separable piece of work.
- **Side effects are already gated.** `ToolPolicy` classifies a tool's
  `SideEffects` (`ReadOnly | Mutating`) and carries a permission posture;
  `bash` is `Mutating` and runs under the `--bash-yolo` / allowlist / prompt
  gate. A runner tool is `Mutating` and reuses this gate — no new permission
  machinery.
- **Subprocess pattern exists.** `bash.rs` spawns via
  `tokio::process::Command` with a timeout (60s default) and a 64KB output
  cap. The runner is the same shape with a different, structured payload.
- **Tools register by name** in `serve/factory.rs` (a match arm returning
  `Arc<dyn Tool>`), and are opted into per session via `--tools`.

The load-bearing consequence: **the biggest lever is deliverable with text
alone.** The bugs that beat mu in battery-1 are thrown exceptions, which are
text. A text-only runner that captures console output, uncaught exceptions,
and exit status recovers most of the gap without touching mu's missing image
support. Image feedback adds the ability to catch *visual* bugs that do not
throw (renders-but-invisible, HUD off-screen); that is real but almost
certainly the smaller, later increment, and the acceptance battery will
measure exactly how much of the gap text alone closes.

## The tool

A `verify` tool (name TBD — `verify` / `run` / `preview`). Spec:

- **Input**: `artifact` (path to run), `kind` (`auto | node | python |
  web | command`), optional `timeout_secs`, and for `web` an optional
  `settle_secs` and a small list of scripted `input_events`
  (click/keypress) so a game or app can be exercised, not just booted.
- **Behavior by kind**:
  - `node` / `python`: run the file, capture stdout + stderr + exit code,
    return them as a structured text summary. (Thin wrapper over the
    `bash.rs` spawn.)
  - `command`: run a shell command and capture output — overlaps `bash`;
    include only if `verify` is granted where `bash` is not, otherwise omit.
  - `web`: launch a headless browser (see backend), load the artifact,
    collect `console.log`/`console.error`, uncaught exceptions, and page
    errors; wait `settle_secs`; dispatch any `input_events`; confirm
    `requestAnimationFrame` is ticking; write a screenshot to disk and
    return its path. **Return text**: boot status, the list of
    errors/exceptions (the gold), whether it animated and responded to
    input, timeout/exit, and the screenshot path.
- **Output**: a text `ToolResult` — a PASS/FAIL line plus the error list and
  key signals, truncated to the existing 64KB cap.
- **Policy**: `SideEffects::Mutating`, gated like `bash`. Not in the default
  toolset; opt in via `--tools` until proven (as `final_answer` was before
  it earned default-on).

## The web backend

Reuse the rig this experiment already proved (memory `3f16261a`): Google's
chrome-for-testing with WebGL via SwiftShader (CPU-only, so it never
contends with an inference serve), driven over the DevTools protocol. The
probe logic — boot, console/exception capture, rAF tick, input response,
screenshot — exists as `phase2/tools/t3-probe.py` and is the reference
implementation to port. The tool needs a Chrome binary; recommend a
`[verify] chrome_path` config knob with a bootstrap-on-first-use fallback
(the chrome-for-testing tarball is a user-level, no-root install). Document
the dependency; it is the one external requirement.

## Phasing

- **Phase 1 — node/python/command runner.** Text stdout/stderr/exit back to
  the model. Thin wrapper over the existing spawn. Ships the run-and-verify
  loop for non-browser artifacts immediately.
- **Phase 2 — web runner.** Headless Chrome + console/exception capture +
  screenshot-to-disk, text summary to the model. This is the battery-1
  lever. The bulk of the design work is here (CDP integration, the Chrome
  dependency).
- **Phase 3 — image feedback (separate track, `nice-to-have`).** Add
  `ContentBlock::Image`, teach the openai-chat provider to serialize image
  content (the serve already accepts `image_url`), let `ToolResult` carry an
  image, and pass the screenshot the Phase-2 runner already captures back to
  the model. Worth doing because the model can use it, but gated behind mu's
  broader image support and expected to be the smaller increment. Its own
  bead.

## Acceptance

Rerun the battery-1 Minecraft task with mu carrying the Phase-2 `verify`
tool, n=3, same lane. Measure T3-playable rate against battery-1's 1/3.
Target: move toward DeepSeek Harness's 3/3. The grading rig already exists
(`t3-probe` / `batch-grade` / `resolve-artifact` + CDP Chrome on the box).
Secondary check from the transcript: the model calls `verify`, receives the
runtime exceptions, and fixes them — the loop closing is the mechanism, and
the delta between the Phase-2 (text) result and any later Phase-3 (image)
result quantifies how much of the gap needs vision.

## Decisions for the operator

- Tool name: `verify` vs `run` vs `preview`.
- Effects gate: reuse `--bash-yolo`, or a dedicated `--verify` permission so
  a session can run-and-verify without granting full shell?
- Chrome: a configured `chrome_path`, bootstrap-on-first-use, or both.
- Default-on for `mu ask` eventually, or opt-in indefinitely?
- Phase 3 priority: schedule the image track now, or leave it as a
  `nice-to-have` bead until the Phase-2 acceptance shows whether text-only
  verification is enough.
