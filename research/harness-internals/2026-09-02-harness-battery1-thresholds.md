# Past the threshold: can a harness add capability, not just subtract it

*Research writeup, 2026-09-02 (bead `mu-316wl`, phase 2, session
442e5925). Phase 1 (`mu-nd9p2`, merged writeup
`2026-08-31-harness-matrix-qwen27b.md`) measured what the harness is worth
on self-contained tasks that fit one completion budget, and found it worth
everything at binary cliffs and nothing on healthy lanes: every healthy
(arm × model) cell scored 100%, so the task set stopped discriminating.
That left the operator's original question unanswered — a task that fits one
budget cannot show a harness **adding** capability, only avoiding
subtracting it. Phase 2 builds tasks past a capability threshold. This is
battery 1: output longer than one completion budget. Same model
(`qwen3.8-27b-nvfp4` on the .143 vLLM lane, MTP k=2, non-thinking chat
template), same lane, same prompt, three harnesses, n=3. Every request was
captured on the wire; artifacts were graded by rendering them in a real
headless browser. Raw captures, per-run transcripts, and the grading tools
are on the bead and in scratchpad `phase2/`.*

## The task and why it crosses a threshold

The prompt is one of two one-shot mega-prompts from a public video that ran
`qwen3.8:27b` across three harnesses (James Layne / augmentedmind,
youtube.com/watch?v=sSySOPGNdjw). It asks for a fully playable
Minecraft-style voxel game — infinite chunked terrain, biomes, caves,
mining and placing, crafting, mobs, day/night, voxel lighting — as a single
self-contained HTML file using Three.js, "built in one continuous pass, do
not stop to ask questions." It is 3,810 words.

A complete answer is 80–115 KB of code, roughly 20–30k output tokens. That
is the threshold: it does not fit one 32k completion budget with any room to
spare, and the correctness bar is not "does it parse" but "does it run in a
browser." Phase 1's tasks were answerable in a few hundred tokens and scored
by a regex; this one can only be scored by executing it.

## How the artifacts were graded

Grading is tiered so most of it is mechanical:

- **T0 — exists**: a single HTML file, complete (closing tag present), of
  plausible size.
- **T1 — parses**: extract the inline scripts, `node --check` them. Catches
  truncation, which is the failure this battery predicts.
- **T2 — loads**: the page loads far enough to start.
- **T3 — runs**: render in a real headless browser and probe it — boots with
  no uncaught error, the canvas is non-blank and changes frame to frame
  (`requestAnimationFrame` actually ticking), and it responds to input
  (WASD + mouse produce pixel changes).

T3 needs real WebGL, which headless Firefox refuses everywhere it was tried
(the jail and a real Linux box, with every pref and env override). The rig
that works: Google's chrome-for-testing installed at user level on the GPU
box, WebGL via SwiftShader (CPU-only, so it never contends with the
inference serves), driven over the DevTools protocol through an SSH tunnel
by the jail's Playwright client. WebGL canvases without `preserveDrawingBuffer`
read back blank through `getImageData`, so the render/animation/input checks
compare full-page screenshot hashes instead. The rig details are in memory
`3f16261a`.

## Result

Thinking-off, which is the like-for-like comparison because the other two
harnesses run non-thinking:

| harness | completed | T3 playable | how it failed |
|---|---|---|---|
| DeepSeek Harness | 3/3 | **3/3** | — boots, animates, input-responsive all three |
| Claude Code | 1/3 | 1/3 | two wall-clock timeouts; only the finished run plays |
| mu | 3/3 (all via `final_answer`) | 1/3 | completes now, but two of three crash on a runtime bug |

The mu thinking-**on** cells are a separate row: 0/2. Both timed out at 2400s
mid-code with no `final_answer` — the model spent its budget reasoning and
never converged on a long output. On phase 1's short tasks, thinking was the
integrity switch (9/9 honest with it vs 4/9 without). On a long-output task
it is a starvation liability. The lever that follows is a reasoning budget
that leaves room for the output, not less reasoning globally. This is the
same overthinking-to-timeout shape reported for `deepseek-v4-pro` on the
review seats.

The crashes were not truncation and not logic errors a syntax check would
catch. They were runtime-contract bugs that surface the instant the page
loads: `Cannot read properties of null (reading 'getContext')` (a canvas
element the code queries but the markup never defines), `Cannot create
property 'fillStyle' on string 'grass_top'` (a texture table indexed as if
it held canvases when it held strings), `buildHotbarDOM is not defined` (a
function referenced but never written, left dangling when the run was
killed). Every one is caught by loading the page once.

## The mechanism: self-verification against the real runtime

DeepSeek Harness went 3/3 because, mid-run, it built itself a test loop
against the real target. Its transcript shows it hunting for a browser,
`npm install`-ing puppeteer-core, downloading chrome-headless-shell (which
works under the linuxulator), running its own game, taking screenshots, and
reading those screenshots back (three `read_image` calls; a leftover
screenshot shows voxel terrain, a hotbar, and hearts mid-run). It then made
105 edits against what it actually saw. Its final claim — "no runtime errors,
stable over 15+ seconds" — was empirical.

mu's models tried the same idea and settled for less. On this battery a mu
run built a Node stub of the DOM and `THREE`, booted the game headlessly in
Node, and ran a few hundred frames against the stub. That validates control
flow and catches logic bugs, and it is genuinely more than nothing — but a
stub answers every `getContext` call with a live object and every texture
lookup with whatever the stub returns, so it cannot see a DOM-contract or
runtime-type mismatch. The exact bug class that crashed two of three mu
artifacts is the class a stub is blind to. Claude Code mostly ran out of
wall-clock before it verified at all.

So the harness feature that adds capability past this threshold is the
affordance and the budget to run the artifact in its real target and look at
the result. It is not model class (same model everywhere), not the prompt
(identical), and not envelope size (Claude Code's 37k-token envelope neither
helped nor was the fatal thing; termination and verification were). This is
the phase-2 result phase 1 could not reach: a self-contained task never
forces the model to close the loop against reality, so the difference stays
invisible until the artifact is something that has to actually run.

## Two things the fix and the toggle already showed

**A harness bug was a threshold, and removing it moved mu across.** The first
mu run on this battery, on the pre-fix binary, died mid-write at exactly
300s — the openai-chat provider never emitted `ToolCallDelta` events, so the
stall watchdog (`mu-197pd`) counted zero bytes during a long single `write`
call and killed a live, streaming connection (`mu-b82rr`, now merged). At
~60 tok/s a file over ~50 KB cannot be written in one tool call on a local
lane without tripping it. After the fix, mu completes 3/3 and delivers
through `final_answer`. The barrier was mu's own, and it is gone; the
remaining gap is verification, not completion.

**Artifacts land wherever the model decides.** No harness confines the
model's working directory, so a one-shot artifact went to at least three
different wrong places across the runs: a directory the model made itself
(`/home/claude/work`), a relative path that resolved to a sibling directory,
and a "cleaned-up" mirror of the ugly scratchpad path rooted at
`/home/claude/<id>/...` that the model created fresh and then cited in its
`final_answer`. Every file existed and was complete; the models' "delivered
and verified" claims were true but location-blind. This is invisible on
answer-only tasks and only appears when the task produces a deliverable file.
For the experiment it meant mu's per-run file attribution could not be kept
clean (shared write locations let later runs overwrite earlier ones), so mu
file grades here are arm-representative rather than clean per-run means; the
per-run outcomes are read from the wire and are reliable.

## What mu should absorb

The goal of this line is to make mu perform as well as or better than the
other harnesses on our models. Battery 1 points at one primary lever and two
supporting ones.

1. **Give the model a way to run an artifact against its real runtime, and
   the budget to iterate on what it sees.** This is the whole gap on this
   battery. DeepSeek Harness reached the real browser by improvising with a
   general shell; mu can do better than improvisation by making it a
   first-class, cheap affordance — a way to launch a headless browser (or the
   relevant runtime for other artifact types), capture output and a
   screenshot, and feed both back into the loop, without the model spending a
   dozen turns bootstrapping it. The verification loop, not the model, is what
   separated a crash from a playable game. This should be its own bead and its
   own battery: give mu the affordance, rerun this exact task, and measure
   whether mu's playable rate moves from 1/3 toward 3/3.

2. **Make the reasoning budget leave room for the output.** Thinking-on
   starved this task to 0/2 while thinking-off completed 3/3. mu should not
   answer this by disabling reasoning — phase 1 showed reasoning carries task
   integrity — but by bounding the thinking budget as a fraction of the
   completion budget so a long output is never crowded out. This is the same
   failure the DeepSeek-V4 review seats hit, so the fix generalizes.

3. **Confine the artifact.** A one-shot task should run in a working directory
   the model cannot casually escape, or mu should track and report the actual
   paths written, so a completed deliverable is always findable and a scoring
   or hand-off step never has to hunt for it.

The envelope-tax hypothesis — that mu's lean request would outperform Claude
Code's heavy one as tasks grow — was not what decided this battery. It is the
subject of battery 2 (context larger than comfortable, where compaction and
envelope interact), which is where request size should finally bite.
