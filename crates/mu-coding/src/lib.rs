//! mu-coding: the actual coding agent.
//!
//! This crate hosts the `mu` binary and the modes it dispatches to:
//!
//! - `serve` — JSON-RPC core daemon over stdio (default) or unix socket.
//! - `ask` — one-shot: spawn `serve` as child, single roundtrip, exit.
//! - `tui` — interactive terminal UI; spawns `serve` as child.
//! - `orchestrate` — coordinator: spawns N `serve` children and routes
//!   work between them based on a plan.
//!
//! ## Planned module layout
//!
//! - `config` — TOML parsing for `~/.config/mu/config.toml`. Provider
//!   selection, MCP server registration, session paths.
//! - `session` — sqlite-backed session store. Conversation, tool call
//!   log, model usage / cost.
//! - `tools/` — built-in tool implementations (`read`, `bash`, `edit`,
//!   `write`). Each in its own file. None of them as `unsafe`.
//! - `slash` — slash command registry and dispatcher.
//! - `extensions/` — MCP server loader and runner.
//! - `modes/{serve,ask,tui,orchestrate}` — one file per mode.
//!
//! Empty for now; modules will land as their milestones do.

#![deny(unsafe_code)]

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_nonempty() {
        assert!(!version().is_empty());
    }
}
