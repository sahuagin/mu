# Delegation Ledger

Append-only record of sub-agent delegations. The point of this file is
not bookkeeping — it's evidence for the routing policy. Patterns
across rows tell us when to delegate, to whom, and what kind of spec
makes a delegation likely to succeed.

## Failure-mode taxonomy

| code | name                       | meaning                                                                                          | fix path                                                                          |
|------|----------------------------|--------------------------------------------------------------------------------------------------|-----------------------------------------------------------------------------------|
| SA   | Spec Ambiguity             | we under-specified; sub-agent made a reasonable choice we disagree with                          | patch spec template / future-spec invariants                                       |
| SC   | Sub-agent Comprehension    | spec was clear; sub-agent paraphrased / reordered / dropped a constraint                          | tighten delegation prompt phrasing                                                 |
| WC   | Workspace Cleanup          | sub-agent modified or restored files unrelated to its task ("helpfully" cleaning up)             | add explicit "don't touch files you didn't author" rule to delegation prompt      |
| CM   | Capability Mismatch        | task is the kind this sub-agent reliably gets wrong (rust async lifetimes, send bounds, etc.)    | update routing rule; route this category elsewhere                                 |
| IC   | Iteration Cap              | sub-agent ran out of iterations mid-implementation                                                | break the spec into smaller pieces; or re-route to a backend with a higher cap     |
| BL   | Blocked (returned cleanly) | sub-agent recognized it couldn't continue and exited with `status: "blocked"` and a useful note  | this is the WORKING case for a hard task — extend with the unblocking info         |
| TR   | Transport / response truncation | sub-agent's wrapper exits non-zero because the response stream was cut mid-envelope (network issue, SSE EOF), even though the work itself is complete | inspect workspace; verify independently; merge if good. Doesn't necessarily indicate a problem with the work or the agent. |
| —    | Success                    | result matches the spec; verification passed; no rework                                          | note what made the spec / prompt / routing work, so we repeat it                   |

A delegation can have multiple failure modes (e.g., SA + SC). Record
all that apply.

## Ledger

| spec_id | attempt | sub-agent           | outcome   | iters | wall_time | files_changed_match | tests_passed | failure_modes | lesson |
|---------|---------|---------------------|-----------|-------|-----------|---------------------|--------------|---------------|--------|
| mu-001  | 1       | codex-oauth/gpt-5.5 | success   | ?     | ~?m       | yes (2/2)           | 11/11        | —             | spec was tight enough that §Invariants / §OOC blocks caught the predictable mistakes (extra derives, wrong rename_all). Reusable: §OOC section is load-bearing for mechanical translation tasks. |
| mu-002  | 1       | codex-oauth/gpt-5.5 | success†  | ?     | ~?m       | yes (2/2)           | 19/19        | WC            | Spec implementation was correct (concurrent tokio code with mpsc + writer task, all 8 §Behaviors covered). BUT pi-rust ran `jj restore specs/delegations.md` mid-task, wiping an unrelated file I had created in the working copy. Lesson: when the working copy contains files from a parallel claude-code session, the sub-agent will "clean them up." Spec said "don't touch other files"; sub-agent interpreted that as "restore other files to their parent state." Different operation, same intent — but it's destructive to in-flight work. See `delegations/mu-002-attempt-1-postmortem.md`. |
| mu-003a | 1       | codex-oauth/gpt-5.5 | success‡  | ?     | ~?m       | 7/5 (5 expected + Cargo.toml + Cargo.lock — flagged) | 29/29        | —             | **WC fix held.** Explicit "Workspace hygiene" section in the prompt prevented the failure mode from mu-002 attempt 1. gpt-pro reported "Working copy was clean at start" — it noticed and respected the parallel-session files. Bonus: gpt-pro found a spec inconsistency (provider.rs uses `futures::stream::BoxStream` but `mu-core/Cargo.toml` didn't list `futures` as a per-crate dep, only the workspace did), added the dep, and EXPLICITLY flagged the deviation in `notes` instead of hiding it. This is the behavior we want — surface inconsistencies, don't paper over them. The 7-vs-5 file deviation isn't a failure; it's a spec lint we should fold back. |
| mu-004a | 1       | codex-oauth/gpt-5.5 | success‡  | ?     | ~?m       | 4/3 (3 expected + Cargo.lock — flagged) | 41/41        | —             | **WC fix held a third time** (mu-002 → mu-003a → mu-004a). Solid evidence the prompt-level fix is reliable; can probably stop adding "test if WC holds" as a watch item. **NEW positive datapoint**: gpt-5.5 noticed the spec's §Interfaces sketch had `expect("mutex poisoned")` in non-test code, which violates INV-6, and chose to map poisoning to `ProviderError::Other` instead. *Treated invariants as normative, sketches as illustrative.* That's the right reading of a spec — and exactly the behavior the "if spec/prompt disagree, surface the deviation" routing rule is meant to encourage. Also: deps were minimal (only `mu-core` added); other deps were already in the linter-modified `Cargo.toml`. |
| mu-007  | 1       | codex-oauth/gpt-5.5 | success   | ?     | ~?m       | yes (3/3)           | 70/70        | —             | **WC fix held a 4th time.** Notably: gpt-5.5 explicitly reported "README.md modified by another session. I did not touch README.md and left it alone" — which is exactly right (claude was updating README in parallel). Pattern is now reliable across 4 delegations. **NEW positive datapoint**: gpt-5.5 implemented `Tool::execute` *desugared* (returning `Pin<Box<dyn Future>>` directly) rather than adding `async-trait` as a new direct dep on mu-coding. That's a genuinely smart minimization — gpt-pro found a way to satisfy the trait without a new dep. Reusable insight: when a spec implies a dep (§Interfaces uses `#[async_trait]`), implementers can sometimes find a desugared path that avoids the dep. |
| mu-008a | 1       | codex-oauth/gpt-5.5 | success   | ?     | ~?m       | yes (1/1)           | 75/75        | —             | **First delegation through `scripts/delegate.sh`** (workspace isolation). Ran in `.delegations/mu-008a-attempt-1/` on bookmark `delegate/mu-008a-attempt-1`. The new CONVENTIONS.md-referencing prompt was ~70 lines vs the prior ~130; gpt-pro followed it cleanly with no clarification questions. WC fix not relevant in the new model — the delegate's workspace structurally CAN'T see parallel-session files because they're not in its checkout. **The infra refactor paid off immediately.** |
| mu-011  | 1       | codex-oauth/gpt-5.5 | success†† | ?     | ~?m       | yes (2/2)           | 32/32 mu-coding | TR            | **NEW failure mode discovered: TR (Transport / response truncation).** Wrapper exited 1 with `API error: SSE error: unexpected EOF reading chunk size`, but the work was complete: 7 tests added, all passing, code structurally correct. Recovery: `cd .delegations/<spec>-attempt-N`, run `cargo nextest run`, observe everything passing, merge as usual. **Lesson:** when `scripts/delegate.sh` shows non-zero exit, ALWAYS inspect the workspace before retrying. The diff in the workspace is the source of truth, not the wrapper's exit code. Recovery cost: ~2 minutes. |

`†` = "spec passed acceptance, but operational issue captured separately."
`‡` = "spec passed acceptance, but a benign deviation (correct call, prompt-level inconsistency to fix)."
`††` = "spec passed acceptance, but the wrapper reported failure; the work itself was complete and verified independently."

Columns:
- **iters**: iteration count if known (codex-oauth doesn't always
  surface this; leave `?` if not in the output envelope).
- **wall_time**: rough order of magnitude. Helps spot "fast and right"
  vs "slow and right" vs "fast but garbage."
- **files_changed_match**: did the diff touch only the files the spec
  said it would? `yes (N/M)` means N of M expected files, no extras.
  `no` with notes if it touched things it shouldn't have.
- **tests_passed**: P/T or just `pass` if everything green.
- **failure_modes**: zero or more codes from the taxonomy table.
- **lesson**: one sentence. What does this row teach us about
  delegation in general?

## Per-attempt post-mortems

When `failure_modes` is non-empty, also create a short markdown file
under `specs/delegations/` named
`<spec_id>-attempt-<N>-postmortem.md`. The post-mortem has four sections:

1. **What we expected** — restate the spec acceptance.
2. **What we got** — actual diff stat / output envelope.
3. **Why it diverged** — failure mode(s) plus root cause.
4. **What we changed** — spec patch, prompt patch, or routing change,
   with a link to the commit that made the change.

Successes don't need a post-mortem unless something surprising
happened (e.g., "succeeded but in 2x expected wall time" is worth
flagging).

## Routing implications (live)

This section is updated as patterns emerge. Each rule cites the rows
it's based on so it can be revisited when the evidence changes.

- **Delegation prompts MUST include a "don't touch files you didn't
  create" rule.** Evidence: mu-002 attempt 1 (WC), mu-003a attempt 1
  (WC fix held). The standard "don't touch any other file" wording is
  insufficient — sub-agents may interpret restoring an unrelated file
  as a benign "clean up your workspace" action. The explicit
  Workspace Hygiene section added to mu-003a's prompt prevented
  recurrence; keep this section in every future delegation prompt
  and reference the post-mortem.

- **When the spec and the prompt are inconsistent, sub-agents should
  flag the deviation explicitly** rather than silently fix or refuse.
  Evidence: mu-003a attempt 1 (futures dep), mu-004a attempt 1
  (mutex-poison expect). gpt-pro's `notes` explicitly called out
  both deviations and proposed/took the right call. Reinforce by
  including a sentence in every delegation prompt: "If the spec and
  this prompt disagree, make the call your judgment supports and
  surface the deviation in `notes`."

- **Treat §Invariants as normative; treat §Interfaces sketches as
  illustrative.** Evidence: mu-004a attempt 1. The spec's interface
  block had `expect("mutex poisoned")` in non-test code. INV-6 says
  no expect outside tests. gpt-5.5 followed the invariant, mapped
  poisoning to a proper error variant. This is the right reading.
  Future spec authors: be aware the interface block is a strong
  hint but not a contract; the contract is invariants + behaviors.
  Future delegation prompts can make this explicit if needed.

- **A non-zero exit from `scripts/delegate.sh` does NOT mean the
  delegation failed.** Evidence: mu-011 attempt 1. The wrapper
  reported exit 1 due to a transport-side SSE truncation, but the
  delegate's actual work was complete and correct. **Always inspect
  the workspace and run verification before deciding the work is
  bad.** If the diff looks right and tests pass, merge as usual.
  Only retry if the workspace itself is incomplete or wrong.

## Cross-references

- Memory `2da785e5` — current `agent-router` routing policy. Updated
  when this ledger surfaces enough evidence.
- AGENTS.md — multi-agent build flow section.
- task_log entries tagged `mu,delegation`.
