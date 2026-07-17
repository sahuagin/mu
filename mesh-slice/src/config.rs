//! Config resolution for the mesh deployables — the fleet convention, not a
//! new one: an env var overrides the `[mesh]` section of
//! `~/.config/agent/config.toml` (the same file `code_index` reads its
//! `[code_index]` section from). Callers layer an explicit CLI arg above this
//! and a default (if any) below it: **arg > env > config file > default**.

use std::path::PathBuf;

/// Resolve `key`: the `env` var if set and non-empty, else `key` from the
/// `[mesh]` section of `~/.config/agent/config.toml`, else `None`.
pub fn setting(env: &str, key: &str) -> Option<String> {
    if let Ok(v) = std::env::var(env) {
        if !v.is_empty() {
            return Some(v);
        }
    }
    config_toml_value("mesh", key)
}

/// Minimal `[section] key = "value"` lookup over the shared agent config.
/// Hand-rolled (same approach as `code_index::embed::read_config_toml_value`)
/// so the proof crate doesn't grow a toml dependency for one lookup.
fn config_toml_value(section: &str, key: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let path: PathBuf = format!("{home}/.config/agent/config.toml").into();
    let content = std::fs::read_to_string(&path).ok()?;

    let header = format!("[{section}]");
    let mut in_section = false;
    for raw in content.lines() {
        let line = raw.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_section = line == header;
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((k, value)) = line.split_once('=') else {
            continue;
        };
        if k.trim() == key {
            return Some(value.trim().trim_matches('"').to_string());
        }
    }
    None
}
