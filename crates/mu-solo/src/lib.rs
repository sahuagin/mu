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
//!
//! Use case — fullscreen TUI in a multiplexer (why the inline render
//! contract earns its keep). Reference operator runs the daily driver
//! fullscreen inside zellij across two monitors. The terminal fact that
//! drives the design: a TUI renders EITHER into the *alternate screen*
//! (rich fixed layout, but the multiplexer cannot capture its scrollback)
//! OR into the *main screen* (output accrues in the mux's real scrollback).
//! The two are mutually exclusive in one buffer; every "hybrid" that tries
//! to straddle them IS the mode-switch state machine we refuse above (cc's
//! fullscreen-vs-default split plus its bolted-on hybrid was the cautionary
//! case). mu-solo's `insert_before` inline contract picks the main screen
//! on purpose, so zellij scrollback / select / copy keep working — mu-solo
//! stays a good multiplexer citizen. Alt-screen is reserved for transient
//! modals (`picker.rs`) that restore the prior screen on exit. (mu-tui,
//! which holds the alt-screen the whole session, loses mux scrollback and
//! today has no in-app copy/export — the quadrant to avoid for a driver.)
//!
//! Corollary for extraction: do NOT make copy / handoff scrape the live
//! screen. The bottom viewport repaints under an in-progress mouse
//! selection and resets it (the classic inline-redraw cost). Route
//! extraction through the *record*, not the *view* — and mu already has the
//! record: events are persisted write-ahead to JSONL
//! (`~/.local/share/mu/events/<daemon>/<session>.jsonl`, the source of
//! truth per the workspace CLAUDE.md). Copy / export / handoff / re-read
//! should read that log (or a text projection of it); the TUI is one view
//! onto it. For a quick "yank this message" that must survive both the
//! multiplexer and SSH, OSC52 is the one copy mechanism that does — mu has
//! none yet (no clipboard dep as of 2026-05-29). Captured as a reference
//! use case, not a mandate.
//!
//! Design idea (deferred) — event-picker `/copy`. Because every turn is a
//! distinct persisted event, `/copy` need not scrape scrollback at all: pop
//! a transient picker (the `picker.rs` modal pattern — alt-screen, restores
//! on exit) listing events as summarized objects, let the user pick one /
//! many / a range, then render the selection to clipboard (OSC52) or a file.
//! Each event type gets a compact summary render for the list and a full
//! render for the output (a projection of the log; markdown for paste).
//! Granularity knob: group at turn level (a tool call + its result are one
//! logical unit) with drill-down; "range" = a contiguous log slice, "many"
//! = a toggle-set. Two payoffs: (1) copy stops depending on retained
//! scrollback, so the inline viewport can stay lean — scrollback is then
//! only for glancing back; (2) it is the only copy path that survives
//! rehydration, since the picker reads the rebuilt event projection (the
//! JSONL log is the source of truth) while terminal scrollback does not
//! rehydrate. Subsumes export-to-file and gives the handoff an event-
//! selection front end. (tcovert's idea, 2026-05-29.)

pub mod app;
pub mod client;
pub mod config;
pub mod input;
pub mod mcp_status;
pub mod menu;
pub mod picker;
pub mod render;
pub mod skills;
pub mod transcript;
pub mod viewport;
