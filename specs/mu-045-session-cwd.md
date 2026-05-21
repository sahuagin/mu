# mu-045 — session-creation `cwd` for memory recall (mu-phl v0)

**Status**: Active — landed as part of mu-phl v0 Phase D (bead `mu-0bxv`).
Wire-protocol amendment per the repo-specific goal-protocol rule #9
("Provider trait or wire-protocol surface change without spec amendment").

**Scope**: Adds an optional `cwd: PathBuf` field to two existing JSON-RPC
requests (`CreateSessionRequest`, `DelegateSessionRequest`). Back-compat
preserved via `#[serde(default)]` — clients that don't send the field
get the pre-mu-phl behavior (the daemon falls back to its own process
cwd).

## Motivation

mu-phl v0 (plan: `~/.claude-personal/plans/happy-sprouting-sprout.md`)
wires the daemon-side session-start recall providers (`SubprocessRecallProvider`,
`ProjectFileRecallProvider`) into `build_and_register_session`. Each
provider's `recall(cwd, capability)` takes a working directory and uses
it to:

- Bias / scope the memory query (e.g., `agent memory context --cwd <cwd>`
  weights memories by cwd-signal terms).
- Resolve project-local file paths (`./CLAUDE.md`, `./AGENTS.md`).

Without the new field, the daemon would fall back to `std::env::current_dir()`
— which is wherever `mu serve` was launched from, typically not where
the operator is running `mu ask` from. Memory recall would key off the
wrong project. The amendment threads the invocation cwd through the
wire protocol so the daemon recalls for the right project.

## Wire-shape changes

### `CreateSessionRequest`

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    pub provider: ProviderSelector,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// mu-phl v0 (mu-045): operator's working directory at the time
    /// of session creation. Used by the daemon to scope recall
    /// providers (`agent memory context --cwd ...`, `./CLAUDE.md`
    /// resolution, etc.). None → daemon falls back to its own
    /// process cwd (back-compat with pre-mu-phl clients).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<std::path::PathBuf>,
}
```

### `DelegateSessionRequest`

Same field, same semantics. Children inherit nothing of the parent's
cwd by default — each session's cwd is independently determined from
its own request (or the daemon fallback). A future revision could add
parent-cwd inheritance as the default when the field is absent, but
v0 leaves them symmetric.

## Wire-shape NON-changes

- `AskSessionRequest`, `CancelSessionRequest`, `CloseSessionRequest`,
  `SessionStatsRequest`, etc. — unchanged. cwd is a session-creation
  concern; once the session exists, the recall snapshot is immutable
  for v0 (mu-phl scope; see plan §"Out of scope").
- `SessionInfo` / list+events responses — unchanged. cwd isn't surfaced
  outbound; the daemon stores it implicitly via the bundled
  `ProjectContext` on the session's `AgentConfig`.
- `ProviderSelector` — unchanged.

## Back-compat

- `#[serde(default)]` on both new fields. Existing clients (TUI, CLI,
  scripted callers) that don't send `cwd` continue to work. The daemon
  derives a fallback via `std::env::current_dir()` — same effective
  behavior as before mu-phl.
- `skip_serializing_if = "Option::is_none"` means an absent cwd doesn't
  appear in the serialized JSON — JSON shape stays identical for the
  None case.

## Client guidance

CLI entry points (`mu ask`, future `mu start`, etc.) **should** populate
`cwd: Some(std::env::current_dir()?)` so the daemon recalls for the
operator's actual project, not the daemon's process cwd. The mu binary
update for the ask path lands as Phase E (bead `mu-lfgh`).

Programmatic JSON-RPC callers may omit the field if they want the daemon
fallback (or send their own explicit cwd if they're building a
context-aware tool).

## Out of scope for this spec

- Mid-session cwd changes — a session's cwd is fixed at creation time
  in v0. If a session needs to "move" projects, create a new session.
- Cross-daemon cwd resolution — the cwd is from the operator's machine
  and is interpreted on the daemon side. Federated daemons (mu-040)
  would need to either reject `create_session` requests with foreign-
  filesystem cwds, or resolve them in the peer daemon's filesystem.
  Out of v0 scope.
- Capability gating on recall content — mu-ywr's filtered-vs-marked
  discovery is the seam; v0 ignores the `capability` parameter that
  `RecallProvider::recall` already takes.

## Provenance

Filed 2026-05-21 as part of the mu-phl v0 goal session (tracking bead
`mu-s1b2`). Implementation in commit landing this spec — see
`crates/mu-coding/src/serve/handlers/session.rs` for the daemon
wire-up.
