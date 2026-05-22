//! mu-solo — standalone single-pane chat TUI for `mu serve`.
//!
//! Design intent:
//! - Single pane, single provider, single session focus.
//! - Claude-code-inspired command surface (see
//!   `specs/architecture/claude-code-feature-mapping.md` in the workspace
//!   root). Adopting CC's command vocabulary collapses UX choice into
//!   implementation choice; refine later with evidence.
//! - Library + main split from line 1. New file at ~600 LOC.
//! - One render contract (Inline-style `insert_before` into mux
//!   scrollback). No mode-switching state machine.
//! - Add features incrementally; refactor as we grow.
//!
//! Companion to (not replacement for) `mu-tui`. mu-tui keeps the
//! multi-pane / multi-F-key surface; mu-solo is the daily-driver chat.

pub mod app;
pub mod client;
pub mod config;
pub mod picker;
pub mod render;
