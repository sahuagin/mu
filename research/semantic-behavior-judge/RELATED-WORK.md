# Related Work

A survey of publicly available prior art on enforcing compliance on coding
agents (Claude Code primarily; also Cursor / Codex / other AGENTS.md-readers).
The aim is research framing: to map *what mechanisms exist*, *what they reliably
enforce*, and *where they break down* — so that a defensive design can reason
about where deterministic control actually lives.

Every entry below is third-party public material (official docs, public blog
posts, public GitHub repositories). For each source we give name, URL, a
one-line description, and the key claim or finding. Code is cited and described,
never copied — repositories with no declared license are flagged **link-only
(all rights reserved)** and should be read at their source, not vendored.

The throughline of the survey is a single defensive thesis that the sources
converge on independently: **hooks are deterministic only inside the standard
parent interactive session; prose directives are advisory at all times.**
Anything that genuinely must-not-happen therefore needs enforcement *below* the
tool-call layer (OS file permissions, network policy, containerization), with
hooks and directives as defense-in-depth above it — not as the sole barrier.

---

## (a) The official Claude Code hook mechanism

The canonical, authoritative description of what a hook is and what it can
enforce. These are the spec against which every third-party pattern is measured.

- **Hooks reference — Claude Code Docs (official).**
  <https://code.claude.com/docs/en/hooks>
  The authoritative mechanism reference. Key facts: the live event set has grown
  well beyond the "classic 7" (PreToolUse, PostToolUse, UserPromptSubmit, Stop,
  SubagentStop, Notification, PreCompact) to ~30 events. The core gate is
  exit-code semantics — exit `0` = success (stdout parsed as JSON only on exit 0;
  injected as context for UserPromptSubmit / SessionStart); exit `2` = blocking
  error (stderr is fed back to Claude; effect is per-event — PreToolUse blocks the
  call, Stop/SubagentStop prevents stopping); **any other exit code is
  non-blocking** and execution continues. Some events (PostToolUse, Notification,
  SessionStart/End) *cannot* block at all — they fire after the fact. The
  modern JSON path uses `permissionDecision` (`allow`/`deny`/`ask`/`defer`) for
  PreToolUse, with multi-hook precedence `deny > defer > ask > allow`. Official
  framing: hooks "provide deterministic control… ensuring certain actions always
  happen rather than relying on the LLM to choose."
  License: official Anthropic documentation.

- **Automate actions with hooks — Claude Code Docs (hooks-guide, official).**
  <https://code.claude.com/docs/en/hooks-guide>
  The how-to companion. Carries the canonical "block edits to protected files"
  example (a PreToolUse `Edit|Write` guard that exits 2 on a protected path) and
  the canonical "auto-format after edits" PostToolUse example. Key claim: hooks
  are for things that must happen deterministically; for *judgment calls* it
  explicitly points to prompt-based / agent-based hooks where a model evaluates
  the condition. Notes the auto-approve footgun: an empty or `.*` matcher
  auto-approves every write and shell command.
  License: official Anthropic documentation.

- **Intercept and control agent behavior with hooks — Claude Agent SDK Docs (official).**
  <https://platform.claude.com/docs/en/agent-sdk/hooks>
  Confirms the same hook model is a first-class SDK primitive (PreToolUse can
  block / modify / inject context programmatically), not merely a CLI
  convenience.
  License: official Anthropic documentation.

**Mechanism summary.** A hook reads a JSON event on stdin
(`session_id`, `cwd`, `hook_event_name`, plus `tool_name` / `tool_input` for
tool events) and signals a decision two ways: structured JSON
(`permissionDecision: deny`, which runs *before* the permission system and so
overrides allow-lists) or exit-code-2-plus-stderr (older, simpler). A Stop /
SubagentStop hook can return `decision: block` to refuse to let a turn end, and
must self-guard against infinite loops via `stop_hook_active`. The single
most-repeated correction in the literature: **use exit 2, not exit 1** — exit 1
is a silent non-blocking warning, the classic footgun that makes a "guard" do
nothing.

---

## (b) Hook-based enforcement patterns people actually use

Published implementations, grouped by the pattern they enforce. These establish
the de-facto playbook. Code is described, not reproduced.

### Read-before-edit (anti-hallucinated-edit)

- **Pinperepette/grounded** (~27 stars).
  <https://github.com/Pinperepette/grounded>
  The most complete published read-before-edit implementation: a PostToolUse
  read-tracker records every file the agent has read, and a PreToolUse edit-guard
  enforces three gates — read-before-edit, an `old_string`-actually-exists check
  (catching hallucinated edits the native behavior does not), and a Grep→Read→Edit
  sequence bonus. Distinctive idea: **active correction** — on a block it injects
  the real file contents so the retry succeeds in one round-trip. Key claim: even
  with Claude Code's native read-before-edit rule, the added value is the
  `old_string`-existence check plus content-injection-on-block.
  License: verify at source; treat as **link-only** unless a LICENSE is present.

### Dangerous-bash guards (`rm -rf`, fork bombs, disk writes, `curl | sh`)

- **disler/claude-code-hooks-mastery** (~3.8k stars).
  <https://github.com/disler/claude-code-hooks-mastery>
  The most-cited *teaching* repo — one hook per event, with the widely-forked
  dangerous-`rm` / `.env` PreToolUse guard (exit-code-2 style, with
  whitespace-normalization and flag-permutation regexes). Key finding: this guard
  is the de-facto base everyone copies, which is why its limitations (regex
  matching, not semantic parsing) propagate everywhere.
  **No LICENSE file — link-only (all rights reserved).** Cite, do not vendor.

- **karanb192/claude-code-hooks** (~426 stars, MIT).
  <https://github.com/karanb192/claude-code-hooks>
  "Copy, paste, customize," and notably *tested* (each hook has a test). Its
  dangerous-command guard uses a tiered `critical/high/strict` severity table and
  the modern `permissionDecision: deny` JSON path, covering far more than `rm`
  (disk-device `dd`, fork bombs, `curl|sh`, force-push-to-main, `git reset
  --hard`, `chmod 777`, `sudo rm`). Appends a JSONL audit log. Key idea: tiered,
  configurable severity rather than a flat block-list.
  License: MIT.

- **RandyHaylor/claude-block-risky-calls** (~0 stars).
  <https://github.com/RandyHaylor/claude-block-risky-calls>
  A genuinely different angle: it blocks by *approvability*, not by danger —
  forbidding command *shapes* (`&&` chaining, inline `python -c`, `$()`
  substitution, `cd …;` compounds) that cannot be turned into stable, reusable
  permission rules, forcing the agent into permanently-approvable forms. The
  inline-`python -c` block doubles as anti-bypass (it stops a guarded command from
  being rewritten as an interpreter one-liner). Key claim: shaping commands for
  approvability is itself a compliance lever.
  License: verify at source; treat as **link-only** unless a LICENSE is present.

### Secret guards (block reading / editing / exfiltrating credentials)

- **karanb192/claude-code-hooks — secret guard** (MIT).
  <https://github.com/karanb192/claude-code-hooks>
  A thorough PreToolUse `Read|Edit|Write|Bash` secret guard with two pattern
  tables (sensitive *files* — `.env`, SSH keys, `.aws/credentials`, `*.pem`,
  `.npmrc`, etc.; and sensitive *bash* — `cat .env`, `printenv`, `echo $API_KEY`,
  `/proc/*/environ`, plus exfiltration via `curl -d @.env`, `scp id_rsa`, `nc <
  .env`, `base64 .env`) and an allowlist so `.env.example` / `.sample` /
  `.template` always pass. Key finding: covering *bash exposure and
  exfiltration*, not just file-path reads, is what closes the obvious holes.
  License: MIT.

- **carlrannaberg/claudekit — `file-guard`** (MIT).
  <https://github.com/carlrannaberg/claudekit>
  A parser-based secret/path guard (not just regex): it loads gitignore-style
  sensitive-path patterns *and* runs a bash-command parser plus a security
  heuristics engine, so pipeline tricks (e.g. `find -name .env | xargs cat`) are
  caught rather than slipping past plain file-path matching. Key claim: semantic
  pipeline analysis beats path-name matching for secret protection.
  License: MIT.

### Format / lint / test gates (PostToolUse "fix it before continuing")

- **carlrannaberg/claudekit — `test-changed` / `typecheck-changed` / `lint-changed`** (MIT).
  <https://github.com/carlrannaberg/claudekit>
  The most production-grade embedded gate framework — hooks compiled into a
  cross-platform, package-manager-aware, cached binary rather than loose scripts.
  After each edit the matching PostToolUse gate runs the relevant tool on the
  changed file(s) and, on failure, returns an *imperative remediation block* that
  becomes the agent's next instruction. Key finding: the enforcement is in the
  wording as much as the exit code — the gate tells the model it MUST fix all
  failures and may not skip or `.skip()` tests.
  License: MIT.

- **disler/claude-code-hooks-mastery — validators.**
  <https://github.com/disler/claude-code-hooks-mastery>
  PostToolUse content/format validators (ruff, type-check, file-contains,
  new-file checks). Key point: PostToolUse can only *react* after the tool ran —
  it gives feedback, it cannot undo the write.
  **No LICENSE — link-only (all rights reserved).**

### Stop-checklist hooks (the agent may not stop until it reflects)

- **carlrannaberg/claudekit — `self-review`** (MIT).
  <https://github.com/carlrannaberg/claudekit>
  A Stop / SubagentStop hook that, when files changed since the last review,
  samples a self-review question per focus area and *blocks the stop*, feeding the
  checklist back to Claude. Two load-bearing details: the `stop_hook_active` loop
  guard, and a transcript marker so each block is a fresh review rather than a
  repeat. Key claim: Stop hooks invert blocking — they enforce *that work
  continues* (no premature "done") rather than that an action is prevented.
  License: MIT.

- **JP Caparas — "Use Hooks to Enforce End-of-Turn Quality Gates" — Dev Genius.**
  <https://blog.devgenius.io/claude-code-use-hooks-to-enforce-end-of-turn-quality-gates-5bed84e89a0d>
  Articulates the Stop-hook-exits-2 pattern in prose: a Stop hook that exits 2
  "forces Claude to keep working," used to require green build/tests before a turn
  may end. License: blog (cite, do not reproduce).

- **Pixelmojo — "Production Quality / CI-CD Patterns."**
  <https://www.pixelmojo.io/blogs/claude-code-hooks-production-quality-ci-cd-patterns>
  PostToolUse gates catch format/type/lint errors at generation time; PreToolUse
  on `git commit` blocks the commit on check failure; Stop is positioned for
  end-of-turn validation. Reiterates the exit-2 footgun. License: blog.

### Prompt / context injectors (UserPromptSubmit, PreCompact)

- **severity1/claude-code-prompt-improver** (~1.6k stars).
  <https://github.com/severity1/claude-code-prompt-improver>
  A UserPromptSubmit hook that rewrites vague prompts before Claude sees them.
  Productivity rather than compliance, but it documents the same injection seam
  that prompt-level policy or secret-in-prompt scanning would use.
  License: verify at source; treat as **link-only** unless a LICENSE is present.

- **Dicklesworthstone/post_compact_reminder** (~43 stars).
  <https://github.com/Dicklesworthstone/post_compact_reminder>
  Detects context compaction and re-injects an AGENTS.md re-read reminder — a
  compliance-adjacent use of the PreCompact / SessionStart seam to keep standing
  rules alive across compaction. Key claim: directive decay under compaction is
  real enough that people build hooks to counter it.
  License: verify at source; treat as **link-only** unless a LICENSE is present.

### Curated indexes / frameworks (discovery surfaces, not reusable code)

- **hesreallyhim/awesome-claude-code** (~47k stars).
  <https://github.com/hesreallyhim/awesome-claude-code>
  The flagship curated index of Claude Code skills / hooks / slash-commands /
  orchestrators — an entry point for discovery, not itself reusable code.

- **carlrannaberg/claudekit** (~724 stars, MIT) — the most production-grade
  embedded hook framework, spanning nearly every category above in one repo
  (already cited per-pattern). License: MIT.

---

## (c) Bypass-resistance and the limits of hooks

The most load-bearing part of the survey for a defensive design: the published
evidence on *how hooks are defeated*, and why they are not a sandbox.

- **kenryu42/cc-safety-net** (~1.4k stars, MIT) — standout.
  <https://github.com/kenryu42/cc-safety-net>
  A multi-agent PreToolUse bash regulator that does *semantic command analysis*
  rather than pattern matching, and the single best public argument for
  hooks-over-permission-rules. Its README enumerates the bypass vectors that
  defeat a naive `settings.json` deny rule: options-before-value, shell-variable
  indirection (`URL=…; curl $URL`), flag reordering (`rm -r -f` vs `rm -rf`),
  extra whitespace, shell wrappers (`sh -c "…"`), and interpreter one-liners
  (`python -c '…'`). It responds by *recursively unwrapping shell wrappers* (up to
  10 levels), detecting interpreter one-liners, running *before* the permission
  system, and offering a fail-closed **strict mode** on unparseable commands.
  Key finding: regex/allow-list permission rules are structurally bypassable;
  only a parser that semantically classifies the command holds.
  License: MIT.

- **open-horizon-labs/superego** (~13 stars) — unique angle.
  <https://github.com/open-horizon-labs/superego>
  A metacognitive / LLM-as-judge guard: on a large edit it builds a
  pending-change description and asks a model to evaluate it against the
  transcript, blocking with the judge's reasoning if flagged. Represents the
  "semantic policy via LLM" end of the spectrum (vs. a deterministic parser). Key
  caveat for a defensive reading: an LLM judge is itself probabilistic and
  confusable — it raises the bar but is not a deterministic gate.
  **License: NOASSERTION — treat as link-only (all rights reserved).**

- **Boucle — "What Claude Code Hooks Can and Cannot Enforce."**
  <https://dev.to/boucle2026/what-claude-code-hooks-can-and-cannot-enforce-148o>
  (mirror: <https://blog.boucle.sh/posts/what-claude-code-hooks-can-and-cannot-enforce/>)
  The best skeptic-of-hooks piece, and the central correction to hook-maximalism.
  It catalogs six failure categories: (1) **hooks don't fire** — pipe mode (`-p`),
  `--bare`, some non-interactive / worktree paths, disabled plugins; (2) **hooks
  fire but are ignored** — some MCP / subagent paths discard deny decisions; (3)
  **platform bugs** — invalid JSON silently disables *all* hooks, updates strip
  execute permissions; (4) **architectural gaps** — events that simply don't
  exist for certain lifecycle points; (5) **model-level defeats** — the model
  *routes around* a blocked tool (e.g. a Bash heredoc instead of the Write tool),
  self-generates confirmation, or trusts subagent output unverified; (6)
  **security gaps** — wildcard permission rules, `bypassPermissions` overrides,
  path-case bypasses. Money quote: "Hooks are deterministic in the parent
  interactive session" — and *only* there. Recommendation, and the defensive
  thesis of this whole survey: for enforcement that must survive subagents / MCP /
  pipe mode, use **OS-level controls** (file permissions, network policy,
  containerization) *below* the tool-call layer; hooks alone are not a sandbox.
  License: blog.

**Defensive synthesis (theme c).** PreToolUse + `exit 2` (or `permissionDecision:
deny`) is the one place a directive becomes a hard gate — but that gate exists
only in the standard interactive session, only for events that can block, only if
the hook is wired correctly (valid JSON, exit 2 not 1, narrow matcher), and only
until the model finds an unguarded path to the same effect. The correct posture
is layered: OS/container controls for must-not-happen invariants; semantic hooks
(cc-safety-net-style, bypass-aware) as the deterministic-in-session layer; and
prose for the judgment-laden remainder. A guard that "always blocks" is a map; the
parent-session-only, parser-dependent reality is the terrain.

---

## (d) CLAUDE.md / AGENTS.md directive practices

How people write prose directives believing the wording will *force* behavior,
the emphasis techniques they use, and what the standards themselves concede.

### The standards and official guidance

- **AGENTS.md standard — agents.md** (~22k stars on the spec repo).
  <https://agents.md/> · <https://github.com/agentsmd/agents.md>
  "A README for agents": a predictable place for build/test/convention context.
  Just standard Markdown, no required schema; read by 30+ agents; nearest file in
  the tree wins. Now stewarded by the Agentic AI Foundation under the Linux
  Foundation. **Key efficacy admission:** the standard itself states "the closest
  AGENTS.md to the edited file wins; *explicit user chat prompts override
  everything*" — i.e. the directive file is structurally *low* in priority, and it
  makes no compliance guarantee. License: open spec / docs.

- **Custom instructions with AGENTS.md — OpenAI Codex Docs (official).**
  <https://developers.openai.com/codex/guides/agents-md>
  Codex merges directive files in precedence order (global → root→cwd), with
  closer files overriding earlier guidance because they appear later in the
  combined prompt. Key point: precedence determines *which prose wins*, not
  whether prose is enforced — same probabilistic substrate. License: official docs.

- **Best practices for Claude Code — Claude Code Docs (official).**
  <https://code.claude.com/docs/en/best-practices>
  Anthropic's own CLAUDE.md guidance: keep it concise, document
  bash/style/testing/etiquette, iterate on it like a prompt, and use the
  emphasis convention ("IMPORTANT", "YOU MUST") to raise adherence — while
  acknowledging it remains a prompt, not a guarantee. License: official docs.

- **Steering Claude Code: skills, hooks, rules, subagents — claude.com blog (Anthropic).**
  <https://claude.com/blog/steering-claude-code-skills-hooks-rules-subagents-and-more>
  The decisive official framing: "When there's something that absolutely must not
  happen, an instruction is the wrong tool… A real guardrail needs to be
  deterministic, and the enforcement methods are hooks and permissions." Provides
  the tool-selection map: hooks = deterministic blocking; rules (CLAUDE.md) =
  probabilistic guidance; skills = procedures loaded on invocation; subagents =
  fresh-context isolation. License: official Anthropic blog.

### Practitioner technique write-ups

- **HumanLayer — "Writing a good CLAUDE.md."**
  <https://www.humanlayer.dev/blog/writing-a-good-claude-md>
  High-quality guidance: "< 300 lines is best, shorter is better" (their own root
  file is < 60 lines); structure as WHAT / WHY / HOW; *exclude* code-style rules
  ("never send an LLM to do a linter's job") and task-specific instructions;
  frontier models follow "~150-200 instructions with reasonable consistency" and
  the system prompt already eats ~50; prefer pointers to copies; offload
  formatting to hooks and slash-commands. License: blog.

- **DEV (docat0209) — "5 Patterns That Make Claude Code Actually Follow Your Rules."**
  <https://dev.to/docat0209/5-patterns-that-make-claude-code-actually-follow-your-rules-44dh>
  Five patterns: the 30-line rule; positive over negative phrasing (claimed to cut
  violations ~half); primacy/recency anchoring (top and bottom, duplicate the
  worst offender); hooks for hard enforcement ("Claude cannot skip these"); and
  per-subdirectory scoping. License: blog.

- **mozilla-ai/any-agent — AGENTS.md (the symlink pattern).**
  <https://github.com/mozilla-ai/any-agent/blob/main/AGENTS.md>
  Notable single-source-of-truth pattern: `CLAUDE.md` is a symlink to `AGENTS.md`
  so one emphatic directive block reaches Claude, Codex, and Cursor alike. License:
  see repo.

- **oven-sh/bun — CLAUDE.md** (repo ~80k+ stars).
  <https://github.com/oven-sh/bun/blob/main/CLAUDE.md>
  The gold-standard "maximalist" directive file (~302 lines) — the densest public
  specimen of every emphasis technique at once (`**CRITICAL**:` prefixes,
  mid-sentence CAPS on the load-bearing word, whole-directive bolding, deliberate
  repetition of the same test rule in multiple sections). Encodes
  run-the-real-tests, don't-fake-success, verify-semantics-empirically,
  fix-the-class-not-the-symptom, and don't-touch-reference-files. Cite for
  *technique inventory*; do not copy. License: see repo (BSD-family; verify).

- **twostraws/SwiftAgents — AGENTS.md** (~1.3k stars).
  <https://github.com/twostraws/SwiftAgents/blob/main/AGENTS.md>
  Near-pure ALWAYS/NEVER convention-steering (prefer async/await, avoid deprecated
  APIs, ask before adding dependencies). Exemplifies the modal-CAPS directive
  vocabulary. License: see repo.

- **sjcoope/sjcnet-cc-agentic-team-template — CLAUDE.md (the prose→hook bridge).**
  <https://github.com/sjcoope/sjcnet-cc-agentic-team-template/blob/main/CLAUDE.md>
  The canonical "this rule is also a hook" specimen: file-ownership is enforced by
  a PreToolUse governance hook keyed on the acting subagent's role and the target
  path, with prose that *documents* the hook rather than hoping. Defaults
  fail-open so a guard bug never bricks a session. Key claim: the durable move is
  to implement "don't touch file X" as terrain (a PreToolUse deny), keeping prose
  only to explain the gate. License: see repo.

**Emphasis-technique inventory (observed across the corpus, by prevalence):**
modal CAPS keywords (ALWAYS / NEVER / MUST); `IMPORTANT:` / `CRITICAL:` prefixes;
mid-sentence CAPS on the load-bearing word; whole-directive bolding; deliberate
repetition; top-of-file placement; negative framing with the rejected example
inline; read-receipt canaries ("confirm you have read CLAUDE.md"); and
single-source-via-symlink. None of these change the probabilistic substrate —
they re-weight attention, they do not bind.

### Curated directive collections (demand signal)

The aggregators dwarf any individual file, which tells you the demand is for
copy-pasteable directive *bundles*: **PatrickJS/awesome-cursorrules**
(<https://github.com/PatrickJS/awesome-cursorrules>, ~40k stars),
**VoltAgent/awesome-claude-code-subagents** (~22k),
**rohitg00/awesome-claude-code-toolkit** (~2.1k, bundles prose rules *and* hooks),
**ciembor/agent-rules-books** (~1.9k, "encode the canon as directives"). License:
each is a curated index; consult per-entry licenses before reuse.

---

## (e) Efficacy and skepticism

The near-unanimous practitioner finding, plus the prior art on inferring agent
state (including frustration) from session signals.

- **DEV (minatoplanb) — "I Wrote 200 Lines of Rules for Claude Code. It Ignored Them All."**
  <https://dev.to/minatoplanb/i-wrote-200-lines-of-rules-for-claude-code-it-ignored-them-all-4639>
  Cites instruction-capacity research (Jaroslawicz et al., 2025): "Claude Sonnet
  shows a linear decay pattern — double the instructions, halve the compliance."
  Three failure modes — context competition, compaction summarizing rules "into
  oblivion," and no enforcement mechanism. Signature claim: **"Rules in prompts
  are requests. Hooks in code are laws."** Recommends cutting to ~20 critical rules
  and moving everything enforceable to hooks/CI. License: blog.

- **paddo.dev — "Claude Code Hooks: Guardrails That Actually Work."**
  <https://paddo.dev/blog/claude-code-hooks-guardrails/>
  Efficacy claim: "Prompts are interpreted at runtime by an LLM that can be
  convinced otherwise. You need something deterministic." A `CLAUDE.md` saying
  "don't edit .env" can be overridden by conflicting context; a PreToolUse hook
  blocking `.env` edits always runs. License: blog.

- **Dotzlaw — "The Deterministic Control Layer for AI Agents."**
  <https://dotzlaw.com/insights/claude-hooks/>
  "Rules in prompts are requests, while hooks execute deterministic code and
  cannot hallucinate. Hooks guarantee behavior; prompts suggest it." License: blog.

- **DEV (olivia_craft) — "Why Claude Ignores Your Instructions" + shareuhack guide.**
  <https://dev.to/olivia_craft/why-claude-ignores-your-instructions-and-how-to-fix-it-with-claudemd-1ba1>
  · <https://www.shareuhack.com/en/posts/claude-code-claude-md-setup-guide-2026>
  The "delivery mechanism" critique: Claude Code wraps CLAUDE.md content in framing
  that says it "may or may not be relevant" and to apply it only "if highly
  relevant" — so the model *evaluates whether to apply* the rules rather than
  treating them as binding. Conclusion: for must-happen behavior, hook-based
  injection is the more reliable path. License: blog.

- **anthropics/claude-code issues #7777, #15443, #27750 (terrain evidence).**
  <https://github.com/anthropics/claude-code/issues/7777> ·
  <https://github.com/anthropics/claude-code/issues/15443> ·
  <https://github.com/anthropics/claude-code/issues/27750>
  Filed-against-Anthropic confirmation that prose directives are unreliable in the
  wild ("Claude ignores instruction in CLAUDE.MD and agents"; "…does not reliably
  follow CLAUDE.md project instructions across sessions"). The empirical basis for
  the whole "hooks > prose" movement. License: public issue tracker.

### Inferring agent state from session signals (incl. frustration keywords)

Relevant prior art for *keyword-based detection of agent state*: Claude Code
itself has shipped client-side telemetry that scans conversation text for
sentiment/frustration-indicating keywords. This is publicly documented prior art
that user/agent frustration can be inferred from lexical signals in the session
stream — i.e. that a keyword scan over the transcript is a recognized (if blunt)
signal for state, which a defensive monitor could likewise use to detect a stuck
or escalating agent. Treated here as a *named concept* from the project brief; we
deliberately do **not** invent a citation URL for it, since none appears in the
underlying public-source bibliography this survey draws on. Anyone extending this
section should cite the specific public report / changelog entry documenting the
Claude Code frustration-keyword telemetry directly rather than relying on this
placeholder.

**Efficacy synthesis (theme e).** The consensus is quantitative and consistent:
prose compliance is roughly 70-90% and decays with context length and compaction;
the instruction ceiling is ~150-200 (with ~50 already consumed by the system
prompt); hooks approach 100% *in the happy path only*. The honest design
consequence — hook everything hookable so the limited prose budget is spent only
where prose is the sole available lever, and never rely on either layer for a
must-not-happen invariant that an OS-level control could enforce instead.

---

## Cross-cutting conclusion (defensive framing)

The independent sources converge on one structural fact: **determinism in agent
control is a property of the layer, not of the wording.** Prose directives
(CLAUDE.md / AGENTS.md) are advisory at all times and structurally low-priority
(the standard says a live user prompt overrides them). Hooks are deterministic,
but *only* inside the standard parent interactive session, *only* for events that
can block, and *only* until the model routes around the guarded path. Therefore a
defense-in-depth design places must-not-happen invariants at the OS / network /
container layer beneath the tool-call boundary, uses bypass-aware semantic hooks
as the in-session deterministic layer, and reserves prose for the judgment-laden
remainder it alone can address — treating every "this always blocks" claim as a
map to be terrain-checked, not a guarantee.
