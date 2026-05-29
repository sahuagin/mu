//! The CLI surface — the path-tree made typeable.
//!
//! Tuned for a model consumer (t4c = tools4claude): `find` is the semantic FRONT
//! DOOR (you arrive with an intent, not a path); a bare dotted path or prefix
//! invokes / walks once you hold the handle; a path miss is forgiving (it
//! fuzzy-falls-back to `find` with did-you-mean); every read surface speaks
//! `--json`; and we flush "what I ran" before a child writes, so a piped reader
//! never reads results blind.

use crate::capability::{Capability, HelpSpec};
use crate::path::CapPath;
use crate::rank::{LexicalRanker, Ranker};
use crate::registry::{Registry, Tree};
use crate::source::{StaticSource, TomlConfigSource};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

/// tools4claude — find, learn, and invoke tools by intent.
#[derive(Parser, Debug)]
// `disable_help_subcommand`: we want `t4c help <path>` to be OUR command (show a
// capability's help), not clap's built-in `help` subcommand. The `--help` flag
// still works. (Without this, clap panics: "command name `help` is duplicated".)
#[command(name = "t4c", version, about, disable_help_subcommand = true)]
pub struct Cli {
    /// Emit JSON instead of human-pretty output (the model-facing surface).
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Semantic front door: rank capabilities by intent; return paths, a help
    /// pointer (schema), and a suggested call. The model finalizes the args.
    Find {
        /// Free-text intent, e.g. `where is the App struct defined`.
        #[arg(trailing_var_arg = true)]
        intent: Vec<String>,
    },
    /// Walk a subtree, terse (one line per node). No prefix = the whole tree.
    Walk { prefix: Option<String> },
    /// List the whole tree (alias for `walk` with no prefix).
    List,
    /// Show a capability's help (terse by default; `--full` for all, `--schema`
    /// for the raw `--help-ai --json` document).
    Help {
        path: String,
        #[arg(long)]
        full: bool,
        #[arg(long)]
        schema: bool,
    },
    /// Invoke a capability by path: `t4c run bash.jj.status -- [args]`.
    Run {
        path: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Fallback: a bare dotted path (invoke / walk) or a natural intent (find).
    #[command(external_subcommand)]
    Bare(Vec<String>),
}

/// What a bare invocation resolves to. Pure routing, split out so it is
/// unit-testable without spawning anything.
#[derive(Debug, PartialEq, Eq)]
pub enum BareAction {
    Run { path: CapPath, args: Vec<String> },
    Walk { prefix: CapPath },
    Help { path: CapPath, schema: bool },
    Find { intent: String },
}

/// Resolve a bare token list against the tree: exact path → run; valid prefix →
/// walk; otherwise → find (the forgiving fall-through). `--help-ai`/`--schema`
/// after an exact path redirect to help (meta-ops are flags, not path segments).
pub fn route_bare(tree: &Tree, tokens: &[String]) -> BareAction {
    let Some((first, rest)) = tokens.split_first() else {
        return BareAction::Find { intent: String::new() };
    };
    let wants_help = rest.iter().any(|t| t == "--help-ai" || t == "--help");
    let wants_schema = rest.iter().any(|t| t == "--schema");

    if let Ok(path) = CapPath::parse(first) {
        if tree.get(&path).is_some() {
            if wants_help || wants_schema {
                return BareAction::Help {
                    path,
                    schema: wants_schema,
                };
            }
            let args = rest
                .iter()
                .filter(|t| *t != "--help-ai" && *t != "--schema")
                .cloned()
                .collect();
            return BareAction::Run { path, args };
        }
        if !tree.walk(&path).is_empty() {
            return BareAction::Walk { prefix: path };
        }
    }
    // A path-shaped miss (has dots) reads better as space-separated words, so the
    // ranker matches individual segments — the forgiving fuzzy-fallback.
    BareAction::Find {
        intent: tokens.join(" ").replace('.', " "),
    }
}

/// Build the default registry: a built-in catalog, optionally overridden by a
/// TOML config at `$T4C_CONFIG` or `~/.config/t4c/registry.toml`. (Filtering the
/// catalog by what's actually installed is mu-kex4.4's `discover`.)
pub fn build_registry() -> Registry {
    let mut reg = Registry::new();
    reg.add_source(Box::new(StaticSource::new("catalog", default_catalog())));
    if let Some(cfg) = config_path() {
        if cfg.exists() {
            reg.add_source(Box::new(TomlConfigSource::new(cfg)));
        }
    }
    reg
}

fn config_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("T4C_CONFIG") {
        return Some(PathBuf::from(p));
    }
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config/t4c/registry.toml"))
}

fn default_catalog() -> Vec<Capability> {
    fn c(path: &str, summary: &str, kw: &[&str], invoke: &[&str], help: &[&str], ai: bool) -> Capability {
        Capability {
            path: CapPath::parse(path).expect("static catalog path is valid"),
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
    vec![
        c(
            "mcp.code-index.recall",
            "semantic + lexical code search over an indexed repo (best first pass for 'where is X')",
            &["code", "search", "semantic", "symbol", "recall", "function", "struct", "where"],
            &["code-index", "recall"],
            &["code-index", "--help-ai"],
            true,
        ),
        c(
            "bash.agent.memory",
            "search persistent agent memory (decisions, feedback, project state, references)",
            &["memory", "remember", "know", "decision", "feedback", "why", "prior", "history", "context"],
            &["agent", "memory", "search"],
            &["agent", "memory", "--help-ai"],
            true,
        ),
        c(
            "bash.rg",
            "ripgrep — fast literal/regex search across files (exact strings/patterns)",
            &["grep", "regex", "literal", "text", "pattern", "string", "fast"],
            &["rg"],
            &["rg", "--help"],
            false,
        ),
        c(
            "bash.fd",
            "fd — find files/dirs by name or glob (paths, not contents)",
            &["find", "file", "filename", "path", "glob", "locate", "directory"],
            &["fd"],
            &["fd", "--help"],
            false,
        ),
        c(
            "bash.grep",
            "POSIX grep — line search (fallback; prefer rg or code-index)",
            &["grep", "line", "match", "text"],
            &["grep"],
            &["grep", "--help"],
            false,
        ),
    ]
}

/// Dispatch a parsed [`Cli`] to its handler. Returns the process exit code.
pub fn run(cli: Cli) -> Result<i32> {
    let tree = build_registry().build()?;
    match cli.cmd {
        None => {
            print_banner(&tree);
            Ok(0)
        }
        Some(Cmd::Find { intent }) => do_find(&tree, &intent.join(" "), cli.json),
        Some(Cmd::Walk { prefix }) => do_walk(&tree, prefix.as_deref(), cli.json),
        Some(Cmd::List) => do_walk(&tree, None, cli.json),
        Some(Cmd::Help { path, full, schema }) => do_help(&tree, &path, full, schema, cli.json),
        Some(Cmd::Run { path, args }) => do_run(&tree, &path, &args),
        Some(Cmd::Bare(tokens)) => match route_bare(&tree, &tokens) {
            BareAction::Run { path, args } => do_run(&tree, &path.to_string(), &args),
            BareAction::Walk { prefix } => do_walk(&tree, Some(&prefix.to_string()), cli.json),
            BareAction::Help { path, schema } => do_help(&tree, &path.to_string(), false, schema, cli.json),
            BareAction::Find { intent } => do_find(&tree, &intent, cli.json),
        },
    }
}

fn print_banner(tree: &Tree) {
    println!(
        "t4c {} — {} capabilities. Find by intent, then invoke by path.",
        crate::version(),
        tree.len()
    );
    println!("  t4c find <intent>           rank capabilities for what you want to do");
    println!("  t4c <prefix>                walk a subtree (e.g. `t4c bash`)");
    println!("  t4c help <path> [--schema]  learn a capability");
    println!("  t4c run <path> -- [args]    invoke it");
}

#[derive(Serialize)]
struct FindOutput {
    intent: String,
    hits: Vec<Hit>,
}

#[derive(Serialize)]
struct Hit {
    path: String,
    summary: String,
    score: f64,
    /// A starting template — the model finalizes args from the schema (finding #3).
    suggested_call: String,
    help: Option<HelpPtr>,
}

#[derive(Serialize)]
struct HelpPtr {
    argv: Vec<String>,
    ai: bool,
}

fn do_find(tree: &Tree, intent: &str, json: bool) -> Result<i32> {
    let caps: Vec<&Capability> = tree.all().collect();
    let ranked = LexicalRanker.rank(intent, &caps);
    let hits: Vec<Hit> = ranked
        .iter()
        .take(8)
        .map(|r| Hit {
            path: r.cap.path.to_string(),
            summary: r.cap.summary.clone(),
            score: r.score,
            suggested_call: format!("t4c run {}", r.cap.path),
            help: r.cap.help.as_ref().map(|h| HelpPtr {
                argv: h.argv.clone(),
                ai: h.ai,
            }),
        })
        .collect();

    if json {
        let out = FindOutput {
            intent: intent.to_string(),
            hits,
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }

    if hits.is_empty() {
        println!("(no capabilities — try `t4c list`)");
        return Ok(0);
    }
    println!("intent: {intent:?}\n");
    for h in &hits {
        let mark = if h.score > 0.0 { "->" } else { "  " };
        let ai = if h.help.as_ref().is_some_and(|x| x.ai) {
            "  · ai-help"
        } else {
            ""
        };
        println!("  {mark} {:<26} {}{}", h.path, h.summary, ai);
    }
    if let Some(top) = hits.first() {
        println!("\nsuggested: {}", top.suggested_call);
        println!(
            "next: t4c help {} --schema   (then build the args from the schema)",
            top.path
        );
    }
    Ok(0)
}

#[derive(Serialize)]
struct Node {
    path: String,
    summary: String,
}

fn do_walk(tree: &Tree, prefix: Option<&str>, json: bool) -> Result<i32> {
    let caps: Vec<&Capability> = match prefix {
        Some(p) => {
            let path = CapPath::parse(p).with_context(|| format!("bad prefix {p:?}"))?;
            tree.walk(&path)
        }
        None => tree.all().collect(),
    };

    if json {
        let nodes: Vec<Node> = caps
            .iter()
            .map(|c| Node {
                path: c.path.to_string(),
                summary: c.summary.clone(),
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&nodes)?);
        return Ok(0);
    }

    if caps.is_empty() {
        println!("(nothing under that prefix — `t4c list` for all)");
        return Ok(0);
    }
    for c in &caps {
        println!("  {:<28} {}", c.path.to_string(), c.summary);
    }
    Ok(0)
}

fn do_help(tree: &Tree, path_str: &str, full: bool, schema: bool, json: bool) -> Result<i32> {
    let path = match CapPath::parse(path_str) {
        Ok(p) => p,
        Err(_) => return fuzzy_miss(tree, path_str, json),
    };
    let Some(cap) = tree.get(&path) else {
        return fuzzy_miss(tree, path_str, json);
    };
    let Some(help) = &cap.help else {
        println!(
            "no help registered for {} (invoke: {})",
            path,
            cap.invoke.join(" ")
        );
        return Ok(0);
    };

    let mut argv = help.argv.clone();
    if argv.is_empty() {
        anyhow::bail!("help for {path} has empty argv");
    }
    // schema/json mode: ask the tool for its machine help when it speaks --help-ai.
    if (schema || json) && help.ai {
        argv.push("--json".to_string());
    }
    let out = Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .with_context(|| format!("running help: {}", argv.join(" ")))?;
    let text = if out.stdout.is_empty() {
        String::from_utf8_lossy(&out.stderr).into_owned()
    } else {
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    if schema || json {
        print!("{text}");
        if !text.ends_with('\n') {
            println!();
        }
        return Ok(0);
    }

    // terse by default — don't dump the wall; --full for the rest.
    let lines: Vec<&str> = text.lines().collect();
    const LIMIT: usize = 16;
    if full || lines.len() <= LIMIT {
        print!("{text}");
        if !text.ends_with('\n') {
            println!();
        }
    } else {
        for l in &lines[..LIMIT] {
            println!("{l}");
        }
        println!(
            "\n… +{} more lines — run: t4c help {} --full",
            lines.len() - LIMIT,
            path
        );
    }
    Ok(0)
}

/// A path miss is not an error wall: rank the query and offer did-you-mean.
fn fuzzy_miss(tree: &Tree, query: &str, json: bool) -> Result<i32> {
    let caps: Vec<&Capability> = tree.all().collect();
    let ranked = LexicalRanker.rank(query, &caps);
    let suggestions: Vec<(String, String)> = ranked
        .iter()
        .filter(|r| r.score > 0.0)
        .take(3)
        .map(|r| (r.cap.path.to_string(), r.cap.summary.clone()))
        .collect();

    if json {
        #[derive(Serialize)]
        struct Miss {
            error: String,
            query: String,
            did_you_mean: Vec<String>,
        }
        let m = Miss {
            error: "no such path".to_string(),
            query: query.to_string(),
            did_you_mean: suggestions.iter().map(|(p, _)| p.clone()).collect(),
        };
        println!("{}", serde_json::to_string_pretty(&m)?);
        return Ok(2);
    }

    println!("no capability at {query:?}.");
    if suggestions.is_empty() {
        println!("try `t4c find {query}` or `t4c list`.");
    } else {
        println!("did you mean:");
        for (p, s) in &suggestions {
            println!("  {p}  ({s})");
        }
    }
    Ok(2)
}

fn do_run(tree: &Tree, path_str: &str, extra: &[String]) -> Result<i32> {
    let path = match CapPath::parse(path_str) {
        Ok(p) => p,
        Err(_) => return fuzzy_miss(tree, path_str, false),
    };
    let cap = match tree.get(&path) {
        Some(c) => c,
        None => {
            // exact miss: a prefix walks; otherwise fuzzy did-you-mean.
            if !tree.walk(&path).is_empty() {
                return do_walk(tree, Some(path_str), false);
            }
            return fuzzy_miss(tree, path_str, false);
        }
    };
    if cap.invoke.is_empty() {
        anyhow::bail!("{path} has no invocation argv");
    }
    let mut argv = cap.invoke.clone();
    argv.extend(extra.iter().cloned());

    // Flush "what I ran" BEFORE the child writes, so a piped reader isn't blind.
    println!("-> {path} : running {}", argv.join(" "));
    std::io::stdout().flush().ok();

    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .with_context(|| format!("running {}", argv.join(" ")))?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> Tree {
        let mut reg = Registry::new();
        reg.add_source(Box::new(StaticSource::new("c", default_catalog())));
        reg.build().unwrap()
    }

    fn toks(s: &str) -> Vec<String> {
        s.split_whitespace().map(|t| t.to_string()).collect()
    }

    #[test]
    fn bare_exact_path_runs() {
        let t = tree();
        assert_eq!(
            route_bare(&t, &toks("bash.rg foo")),
            BareAction::Run {
                path: CapPath::parse("bash.rg").unwrap(),
                args: vec!["foo".to_string()],
            }
        );
    }

    #[test]
    fn bare_prefix_walks() {
        let t = tree();
        assert_eq!(
            route_bare(&t, &toks("bash")),
            BareAction::Walk {
                prefix: CapPath::parse("bash").unwrap(),
            }
        );
    }

    #[test]
    fn bare_help_and_schema_flags_route_to_help() {
        let t = tree();
        assert_eq!(
            route_bare(&t, &toks("mcp.code-index.recall --help-ai")),
            BareAction::Help {
                path: CapPath::parse("mcp.code-index.recall").unwrap(),
                schema: false,
            }
        );
        assert_eq!(
            route_bare(&t, &toks("bash.rg --schema")),
            BareAction::Help {
                path: CapPath::parse("bash.rg").unwrap(),
                schema: true,
            }
        );
    }

    #[test]
    fn unknown_path_falls_back_to_find() {
        let t = tree();
        match route_bare(&t, &toks("xyzzy nothing here")) {
            BareAction::Find { intent } => assert!(intent.contains("xyzzy")),
            other => panic!("expected find, got {other:?}"),
        }
    }

    #[test]
    fn dotted_miss_becomes_spaced_find_intent() {
        let t = tree();
        match route_bare(&t, &toks("bash.nope")) {
            BareAction::Find { intent } => assert_eq!(intent, "bash nope"),
            other => panic!("expected find, got {other:?}"),
        }
    }

    #[test]
    fn catalog_builds_into_tree() {
        let t = tree();
        assert!(t.get(&CapPath::parse("mcp.code-index.recall").unwrap()).is_some());
        assert_eq!(t.walk(&CapPath::parse("bash").unwrap()).len(), 4);
    }
}
