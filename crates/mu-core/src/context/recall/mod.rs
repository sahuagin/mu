//! Recall — substrate seam for memory-recall and project-context injection
//! at session start.
//!
//! [`RecallProvider`] abstracts the source of recallable items so v0 of
//! mu-phl (bead `mu-vm81`) can ship a subprocess-backed provider (shells
//! out to `~/.local/bin/agent memory context`) and a project-file provider
//! (reads CLAUDE.md / AGENTS.md hierarchy), while v1+ of the full mu-phl
//! event-pointer architecture can swap in an `EventLogRecallProvider` at
//! the same trait — no call-site changes downstream.
//!
//! The output is consumed by
//! [`super::assembly::assemble_rope_with_context`], which lands each
//! [`RecalledItem`] as a [`super::rope::SpanKind::MemoryInjection`] or
//! [`super::rope::SpanKind::FileLoad`] span in the stable cacheable
//! prefix of the rope.
//!
//! ## v0 scope
//!
//! - Trait + value types (this module). No concrete providers yet — those
//!   land in `subprocess.rs` and `project_files.rs` (beads `mu-3j32` and
//!   `mu-zj4e`).
//! - One blob per `Memory` recall call (parsing markdown into per-section
//!   sub-items is deferred to v1+).
//! - Session-start snapshot only; immutable for the session's lifetime.
//!   Mid-session re-recall is out of scope.
//!
//! See `~/.claude-personal/plans/happy-sprouting-sprout.md` for the full
//! v0 plan and the seams left open for v1+.

use std::path::{Path, PathBuf};

use crate::capability::Capability;
use crate::context::rope::{SpanId, SpanText};

/// One item produced by a [`RecallProvider`] — content + provenance + a
/// stable id derived from the content (used as the rope span id so
/// re-recalls dedupe and audit identifies what was injected).
#[derive(Debug, Clone)]
pub struct RecalledItem {
    /// What kind of recall produced this item. Drives the [`super::rope::SpanKind`]
    /// chosen when the assembler inserts it into the rope.
    pub source: RecallSource,
    /// Recall content. `SpanText` is the per-type alias from mu-yqeq.2
    /// (`pub type SpanText = Arc<str>` today). Using the alias rather
    /// than bare `Arc<str>` so that if `SpanText` later migrates to a
    /// rope-backed / lazy / chunked variant, [`RecalledItem`] follows
    /// automatically — that's the whole point of the per-type alias.
    pub content: SpanText,
    /// Stable id derived from `(source, content-hash)`.
    ///
    /// v0: literal content hash (or, for `ProjectFile`, hash of the
    /// canonical path). v1+ (full mu-phl event-pointer architecture):
    /// event-log pointer id. Same call sites, different population —
    /// the call site at [`super::assembly::assemble_rope_with_context`]
    /// doesn't change.
    pub stable_id: SpanId,
}

/// What kind of recall produced an item. Determines which
/// [`super::rope::SpanKind`] the assembler injects.
#[derive(Debug, Clone)]
pub enum RecallSource {
    /// Output of an `agent memory`-style query — typically markdown text
    /// with multiple sections (Feedback / User / Project / Reference).
    /// v0: subprocess to `~/.local/bin/agent memory context`. v1+:
    /// query against mu's own event-pointer index. Maps to
    /// [`super::rope::SpanKind::MemoryInjection`].
    Memory,
    /// A project context file (CLAUDE.md, AGENTS.md, etc.) read at
    /// session start. Maps to [`super::rope::SpanKind::FileLoad`].
    /// Path is absolute (canonicalized).
    ProjectFile { path: PathBuf },
}

/// Bundle of recalled items handed to
/// [`super::assembly::assemble_rope_with_context`] at session creation.
/// Pre-built by the daemon (not on the agent loop's hot path) and
/// immutable for the session's lifetime in v0.
///
/// Future evolution: this could grow to `Arc<RwLock<ProjectContext>>`
/// to support mu-fb0's live-loop mid-session refresh, with the
/// constraint that mid-session refreshes append (cache-discipline.md)
/// rather than rewrite the cacheable prefix.
#[derive(Debug, Clone, Default)]
pub struct ProjectContext {
    pub items: Vec<RecalledItem>,
}

/// Source of recallable items at session start.
///
/// v0: ships [`SubprocessRecallProvider`] (mu-3j32) and
/// [`ProjectFileRecallProvider`] (mu-zj4e). v1+: an
/// `EventLogRecallProvider` over mu's event-pointer index implements
/// the same trait — no call-site change downstream.
///
/// The `capability` parameter is the seam for mu-ywr's filtered-vs-marked
/// discovery model; v0 ignores it but the signature is already
/// capability-aware so v1's implementation slots in without changing
/// the call site.
///
/// The return type is `Vec` for v0 (providers materialize eagerly).
/// Can lift to `Box<dyn Iterator<Item = RecalledItem> + Send>` later
/// if a provider wants lazy generation; consumers using `for` or
/// `.into_iter()` migrate trivially.
pub trait RecallProvider: Send + Sync {
    fn recall(&self, cwd: &Path, capability: &Capability) -> Vec<RecalledItem>;
}

pub mod project_files;
pub mod subprocess;

pub use project_files::ProjectFileRecallProvider;
pub use subprocess::SubprocessRecallProvider;
