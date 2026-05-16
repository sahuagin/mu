//! Analytics — telemetry sink + projection from event-log JSONLs.
//!
//! Spec: mu-042 / bead mu-8ypx.
//!
//! Layout:
//! - [`sink`]: SQLite schema, open/init, idempotent UPSERT of rows
//! - [`compact`]: read event-log JSONLs, project `TaskTelemetry` events into
//!   rows by calling [`mu_core::forensics::classify_task`] on each
//! - [`query`]: preset queries (`summary`, `rate`) over the sink

pub mod compact;
pub mod query;
pub mod sink;

use std::path::PathBuf;

/// Default analytics sink location: `~/.local/share/mu/telemetry.sqlite`.
/// Sibling to the `events/` directory. Returns None when the home dir is
/// not resolvable (rare on Unix; sometimes seen in restricted-container
/// CI).
pub fn default_db_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".local/share/mu/telemetry.sqlite"))
}
