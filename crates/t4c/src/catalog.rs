//! The curated catalog + environment intersection (`discover`).
//!
//! The catalog is the durable asset: metadata for common tools, curated once and
//! applied everywhere t4c lands (a new host, a fresh pot, a bare delegate). On a
//! given machine the live registry is `catalog ∩ what's-installed`, so discovery
//! only ever surfaces tools you can actually run — the availability half of
//! "discovery tracks permission." `discover` scans the environment, reports
//! present/absent, and persists the intersection as a self-configured registry.

use crate::capability::{Capability, HelpSpec};
use crate::chain::Chain;
use crate::path::CapPath;
use crate::source::RegistrySource;
use anyhow::Result;
use std::path::Path;

fn cap(
    path: &str,
    summary: &str,
    kw: &[&str],
    invoke: &[&str],
    help: &[&str],
    ai: bool,
) -> Capability {
    Capability {
        path: CapPath::parse(path).expect("curated catalog path is valid"),
        summary: summary.to_string(),
        keywords: kw.iter().map(|s| s.to_string()).collect(),
        invoke: invoke.iter().map(|s| s.to_string()).collect(),
        help: if help.is_empty() {
            None
        } else {
            Some(HelpSpec {
                argv: help.iter().map(|s| s.to_string()).collect(),
                ai,
            })
        },
        requires: vec![],
    }
}

/// The curated catalog — metadata for common tools regardless of what's
/// installed here. `discover` intersects this with the environment.
pub fn curated() -> Vec<Capability> {
    vec![
        cap(
            "mcp.code-index.recall",
            "semantic + lexical code search over an indexed repo (best first pass for 'where is X')",
            &["code", "search", "semantic", "symbol", "recall", "function", "struct", "where"],
            &["code-index", "recall"],
            &["code-index", "--help-ai"],
            true,
        ),
        cap(
            "bash.agent.memory",
            "search persistent agent memory (decisions, feedback, project state, references)",
            &["memory", "remember", "know", "decision", "feedback", "why", "prior", "history", "context"],
            &["agent", "memory", "search"],
            &["agent", "memory", "--help-ai"],
            true,
        ),
        cap(
            "bash.jj.status",
            "jujutsu — working-copy and parent status",
            &["vcs", "version", "diff", "working", "copy", "commit", "jujutsu"],
            &["jj", "status"],
            &["jj", "status", "--help"],
            false,
        ),
        cap(
            "bash.git.status",
            "git — working-tree status",
            &["vcs", "version", "diff", "staged", "commit"],
            &["git", "status"],
            &["git", "status", "--help"],
            false,
        ),
        cap(
            "bash.jq",
            "jq — query and transform JSON",
            &["json", "query", "filter", "transform", "parse"],
            &["jq"],
            &["jq", "--help"],
            false,
        ),
        cap(
            "bash.t4c",
            "tools4claude — discover, learn, and invoke tools by intent (this tool, self-registered)",
            &["discover", "tool", "find", "capability", "help", "registry", "meta"],
            &["t4c"],
            &["t4c", "--help-ai"],
            true,
        ),
    ]
}

/// Curated preference chains — interchangeable-impl slots resolved against the
/// host at `discover` time (mu-d2iy.2). These supersede the flat per-tool entries
/// (rg/fd/grep) that used to live in `curated()`.
pub fn default_chains() -> Vec<Chain> {
    fn ch(slot: &str, summary: &str, kw: &[&str], impls: &[&str]) -> Chain {
        Chain {
            slot: slot.to_string(),
            summary: summary.to_string(),
            keywords: kw.iter().map(|s| s.to_string()).collect(),
            impls: impls.iter().map(|s| s.to_string()).collect(),
        }
    }
    vec![
        ch(
            "bash.search",
            "search file contents for a pattern or regex",
            &["search", "grep", "regex", "pattern", "string", "text"],
            &["rg", "grep"],
        ),
        ch(
            "bash.find-files",
            "find files and directories by name or glob",
            &[
                "find",
                "file",
                "filename",
                "path",
                "glob",
                "locate",
                "directory",
            ],
            &["fd", "find"],
        ),
        ch(
            "bash.ls",
            "list directory contents",
            &["list", "ls", "directory", "files", "tree"],
            &["eza", "exa", "ls"],
        ),
        ch(
            "bash.compress",
            "compress or archive data",
            &["compress", "archive", "zip", "gzip", "tar"],
            &["zstd", "pixz", "xz", "gzip"],
        ),
    ]
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
