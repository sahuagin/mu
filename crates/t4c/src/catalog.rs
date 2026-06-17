//! The curated catalog + environment intersection (`discover`).
//!
//! The catalog is the durable asset: metadata for common tools, curated once and
//! applied everywhere t4c lands (a new host, a fresh pot, a bare delegate). On a
//! given machine the live registry is `catalog ∩ what's-installed`, so discovery
//! only ever surfaces tools you can actually run — the availability half of
//! "discovery tracks permission." `discover` scans the environment, reports
//! present/absent, and persists the intersection as a self-configured registry.

use crate::capability::Capability;
use crate::chain::Chain;
use crate::source::RegistrySource;
use anyhow::Result;
use std::path::Path;

/// The built-in curated catalog, baked into the binary at compile time.
///
/// This is the **durable asset as config** (goal mu-2332): adding a tool or
/// changing a calling convention is an edit to this TOML, not a code change.
/// It mirrors the `models.default.toml` precedent in mu-core exactly — the
/// engine (probing, ranking, help-ai execution) stays in Rust; the catalog
/// data lives in config and is layered over by install-shipped and
/// operator-local override TOMLs.
pub const DEFAULT_CATALOG_TOML: &str = include_str!("config/curated.default.toml");

/// The curated catalog — metadata for common tools regardless of what's
/// installed here, parsed from the embedded [`DEFAULT_CATALOG_TOML`].
/// `discover` intersects this with the environment.
///
/// Parse failure is a programmer error (the embedded file shipped with the
/// binary), so this panics — the same posture as `models.default.toml`'s
/// `expect("built-in models.default.toml must parse")`.
pub fn curated() -> Vec<Capability> {
    crate::source::TomlConfigSource::parse_str(DEFAULT_CATALOG_TOML)
        .expect("built-in curated.default.toml must parse")
}

/// Curated preference chains — interchangeable-impl slots resolved against the
/// host at `discover` time (mu-d2iy.2), parsed from the embedded
/// [`DEFAULT_CATALOG_TOML`]. These supersede the flat per-tool entries
/// (rg/fd/grep) that the chain slots cover.
pub fn default_chains() -> Vec<Chain> {
    crate::chain::parse_chains(DEFAULT_CATALOG_TOML)
        .expect("built-in curated.default.toml chains must parse")
}

/// Is this capability's underlying command present on `$PATH`?
pub fn is_installed(cap: &Capability) -> bool {
    match cap.invoke.first() {
        Some(cmd) => which(cmd),
        None => false,
    }
}

/// Resolve a command name against the real `$PATH` (absolute paths checked
/// directly).
pub fn which(cmd: &str) -> bool {
    let p = Path::new(cmd);
    if p.is_absolute() {
        return p.is_file();
    }
    match std::env::var("PATH") {
        Ok(path) => which_in(cmd, &path),
        Err(_) => false,
    }
}

/// Pure `$PATH` search (testable): does `cmd` resolve to a file in `path_var`?
pub fn which_in(cmd: &str, path_var: &str) -> bool {
    path_var
        .split(':')
        .any(|dir| !dir.is_empty() && Path::new(dir).join(cmd).is_file())
}

/// Source: the curated catalog filtered to what's installed (catalog ∩ env).
pub struct EnvCatalogSource;

impl RegistrySource for EnvCatalogSource {
    fn name(&self) -> &str {
        "catalog∩env"
    }
    fn capabilities(&self) -> Result<Vec<Capability>> {
        Ok(curated().into_iter().filter(is_installed).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curated_is_nonempty() {
        assert!(!curated().is_empty());
    }

    #[test]
    fn embedded_default_toml_parses_caps_and_chains() {
        // The single-source-of-truth invariant (mu-2332): the baked-in TOML is
        // the catalog, parsed at runtime — not a hardcoded Vec. Both grammars
        // coexist in one file.
        let caps = curated();
        let chains = default_chains();
        assert_eq!(caps.len(), 11, "expected 11 [[capability]] entries");
        assert_eq!(chains.len(), 4, "expected 4 [[chain]] entries");
        // chains carry their per-impl mandatory flags through the TOML round-trip
        let find = chains
            .iter()
            .find(|c| c.slot == "bash.find-files")
            .expect("find-files chain missing");
        assert_eq!(find.impls[0].cmd, "fd");
        assert_eq!(find.impls[0].mandatory_flags, vec!["--one-file-system"]);
    }

    #[test]
    fn curated_contains_gh_pr_jj_workspace_guidance() {
        let caps = curated();
        let gh = caps
            .iter()
            .find(|c| c.path.to_string() == "bash.gh.pr")
            .expect("gh PR capability missing");
        assert!(gh.summary.contains("-R owner/repo"));
        assert!(gh.summary.contains("jj sibling workspaces"));
        assert!(gh.summary.contains("Avoid `gh pr merge -d`"));
        assert_eq!(gh.invoke[..4], ["gh", "pr", "create", "-R"]);
    }

    #[test]
    fn which_in_finds_a_file_on_path() {
        let dir = std::env::temp_dir();
        let name = "t4c_which_probe_marker";
        let p = dir.join(name);
        std::fs::write(&p, b"x").unwrap();
        let path_var = dir.to_string_lossy();
        assert!(which_in(name, &path_var));
        assert!(!which_in("t4c_definitely_absent_xyzzy_cmd", &path_var));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn env_catalog_is_subset_of_curated() {
        let installed = EnvCatalogSource.capabilities().unwrap();
        assert!(installed.len() <= curated().len());
    }

    #[test]
    fn registry_toml_round_trips() {
        use crate::source::TomlConfigSource;
        let caps = curated();
        let text = TomlConfigSource::to_toml(&caps).unwrap();
        let back = TomlConfigSource::parse_str(&text).unwrap();
        let before: Vec<String> = caps.iter().map(|c| c.path.to_string()).collect();
        let after: Vec<String> = back.iter().map(|c| c.path.to_string()).collect();
        assert_eq!(before, after);
    }
}
