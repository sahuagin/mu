//! Skill discovery for mu-solo.
//!
//! Two formats are supported:
//!
//! **mu-native (`skill.toml` + `body.md`)** — structured metadata
//! separate from body content.  The TOML carries routing, display,
//! and context fields; body.md holds the full instructions loaded on
//! demand.  Designed for progressive loading via the rope.
//!
//! **Legacy compat (`SKILL.md`)** — YAML frontmatter + markdown body
//! in one file, matching claude-code's skill format.  Loaded whole.
//!
//! When both exist in the same directory, `skill.toml` wins.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

// ── mu-native skill.toml schema ────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
struct SkillToml {
    skill: SkillMeta,
    #[serde(default)]
    routing: Option<RoutingMeta>,
    #[serde(default)]
    context: Option<ContextMeta>,
}

#[derive(Debug, Clone, Deserialize)]
struct SkillMeta {
    name: String,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    display: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    categories: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RoutingMeta {
    #[serde(default, rename = "when")]
    when_to_use: Option<String>,
    #[serde(default)]
    manual_only: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct ContextMeta {
    #[serde(default)]
    files: Vec<String>,
}

// ── legacy SKILL.md frontmatter ────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
struct LegacyFrontmatter {
    name: String,
    description: String,
    #[serde(default)]
    when_to_use: Option<String>,
    #[serde(default, rename = "disable-model-invocation")]
    disable_model_invocation: bool,
}

// ── unified skill type ─────────────────────────────────────────────

/// A discovered skill ready for slash-command dispatch.
#[derive(Debug, Clone)]
pub struct DiscoveredSkill {
    pub name: String,
    /// The slash command (without leading `/`). Usually same as name.
    pub command: String,
    /// Human-facing display name for /help.
    pub display: String,
    /// Human-facing description for /help.
    pub description: String,
    /// Model-facing trigger text for the routing index.  When absent,
    /// `description` is used as the fallback.
    pub when_to_use: Option<String>,
    /// If true, model can't see or suggest this skill — user-only.
    pub manual_only: bool,
    /// Categories for filtering/grouping (e.g., "coding", "vcs",
    /// "operations").  Used by `/mode` to load/unload skill groups.
    pub categories: Vec<String>,
    /// Full instructions loaded on demand.
    pub body: String,
    /// Reference file contents: (filename, content).
    pub references: Vec<(String, String)>,
    /// Which format this was loaded from.
    pub format: SkillFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillFormat {
    Native,
    Legacy,
}

impl DiscoveredSkill {
    /// Full injection text: body + all reference files.
    pub fn injection_text(&self) -> String {
        if self.references.is_empty() {
            return self.body.clone();
        }
        let mut out = self.body.clone();
        for (name, content) in &self.references {
            out.push_str("\n\n---\n\n");
            out.push_str(&format!("## Reference: {name}\n\n"));
            out.push_str(content);
        }
        out
    }
}

// ── loaders ────────────────────────────────────────────────────────

fn load_native(dir: &Path) -> Option<DiscoveredSkill> {
    let toml_path = dir.join("skill.toml");
    let content = fs::read_to_string(&toml_path).ok()?;
    let parsed: SkillToml = toml::from_str(&content).ok()?;

    let routing = parsed.routing.unwrap_or(RoutingMeta {
        when_to_use: None,
        manual_only: false,
    });
    let ctx = parsed.context.unwrap_or(ContextMeta {
        files: vec!["body.md".into()],
    });

    let name = &parsed.skill.name;
    let command = parsed.skill.command.unwrap_or_else(|| name.clone());
    let display = parsed.skill.display.unwrap_or_else(|| name.clone());
    let description = parsed.skill.description.unwrap_or_else(|| display.clone());

    // Load body + reference files per the [context].files list.
    let mut body = String::new();
    let mut references = Vec::new();

    for pattern in &ctx.files {
        // Simple glob: if it contains '*', expand; otherwise literal.
        if pattern.contains('*') {
            let parent = dir.join(Path::new(pattern).parent().unwrap_or(Path::new("")));
            if parent.is_dir() {
                let mut matched: Vec<PathBuf> = fs::read_dir(&parent)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.extension()
                            .and_then(|e| e.to_str())
                            .is_some_and(|e| e == "md" || e == "toml" || e == "txt")
                    })
                    .collect();
                matched.sort();
                for p in matched {
                    if let Ok(c) = fs::read_to_string(&p) {
                        let fname = p
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("unknown")
                            .to_owned();
                        references.push((fname, c));
                    }
                }
            }
        } else {
            let path = dir.join(pattern);
            if let Ok(c) = fs::read_to_string(&path) {
                // First file in the list is the body; rest are refs.
                if body.is_empty() {
                    body = c;
                } else {
                    let fname = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown")
                        .to_owned();
                    references.push((fname, c));
                }
            }
        }
    }

    Some(DiscoveredSkill {
        name: name.clone(),
        command,
        display,
        description,
        when_to_use: routing.when_to_use,
        manual_only: routing.manual_only,
        categories: parsed.skill.categories,
        body,
        references,
        format: SkillFormat::Native,
    })
}

fn load_legacy(dir: &Path) -> Option<DiscoveredSkill> {
    let content = fs::read_to_string(dir.join("SKILL.md")).ok()?;
    let (fm, body) = parse_legacy_md(&content)?;

    let mut references = Vec::new();
    let refs_dir = dir.join("references");
    if refs_dir.is_dir() {
        let mut ref_files: Vec<PathBuf> = fs::read_dir(&refs_dir)
            .ok()?
            .filter_map(|e| {
                let p = e.ok()?.path();
                if p.extension().and_then(|e| e.to_str()) == Some("md") {
                    Some(p)
                } else {
                    None
                }
            })
            .collect();
        ref_files.sort();
        for p in ref_files {
            if let Ok(c) = fs::read_to_string(&p) {
                let fname = p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown.md")
                    .to_owned();
                references.push((fname, c));
            }
        }
    }

    Some(DiscoveredSkill {
        name: fm.name.clone(),
        command: fm.name.clone(),
        display: fm.name.clone(),
        description: fm.description,
        when_to_use: fm.when_to_use,
        manual_only: fm.disable_model_invocation,
        categories: Vec::new(),
        body,
        references,
        format: SkillFormat::Legacy,
    })
}

fn parse_legacy_md(content: &str) -> Option<(LegacyFrontmatter, String)> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let after_open = &trimmed[3..];
    let close_pos = after_open.find("\n---")?;
    let yaml_str = &after_open[..close_pos];
    let fm: LegacyFrontmatter = serde_yaml::from_str(yaml_str).ok()?;
    let body_start = 3 + close_pos + 4;
    let body = if body_start < trimmed.len() {
        trimmed[body_start..].trim_start_matches('\n').to_owned()
    } else {
        String::new()
    };
    Some((fm, body))
}

/// Load a skill from a directory.  Tries `skill.toml` first, falls
/// back to `SKILL.md`.
fn load_skill_dir(dir: &Path) -> Option<DiscoveredSkill> {
    load_native(dir).or_else(|| load_legacy(dir))
}

// ── discovery ──────────────────────────────────────────────────────

/// Default search directories in priority order.
///
/// mu-native (bead `mu-mu-native-config-sources-98j7`): `.mu/skills`
/// (project) then `~/.config/mu/skills` (operator). The former
/// `~/.claude-personal/skills` compat-read was dropped — mu no longer
/// borrows the operator's claude-code skills.
pub fn default_search_dirs(project_root: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::with_capacity(2);
    if let Some(root) = project_root {
        dirs.push(root.join(".mu/skills"));
    }
    if let Some(config) = dirs::config_dir() {
        dirs.push(config.join("mu/skills"));
    }
    dirs
}

/// Discover all skills from the search directories.
///
/// First-dir-wins on name collisions.  Returns a map keyed by
/// command name for O(1) dispatch lookup.
pub fn discover(search_dirs: &[PathBuf]) -> HashMap<String, DiscoveredSkill> {
    let mut seen: HashMap<String, DiscoveredSkill> = HashMap::new();

    for search_dir in search_dirs {
        if !search_dir.is_dir() {
            continue;
        }
        let entries = match fs::read_dir(search_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if let Some(skill) = load_skill_dir(&path) {
                seen.entry(skill.command.clone()).or_insert(skill);
            }
        }
    }
    seen
}

/// Build a compact routing index for system prompt injection.
///
/// One line per model-visible skill.  Skills with `manual_only`
/// are excluded.  Returns `None` if no skills are eligible.
pub fn routing_index(skills: &HashMap<String, DiscoveredSkill>) -> Option<String> {
    let mut entries: Vec<(&str, &str)> = skills
        .values()
        .filter(|s| !s.manual_only)
        .map(|s| {
            let trigger = s.when_to_use.as_deref().unwrap_or(&s.description);
            (s.command.as_str(), trigger)
        })
        .collect();

    if entries.is_empty() {
        return None;
    }

    entries.sort_by_key(|(cmd, _)| *cmd);

    let mut out = String::from("Available skills (invoke via /<name> or suggest when relevant):\n");
    for (cmd, trigger) in &entries {
        out.push_str(&format!("- /{cmd} — {trigger}\n"));
    }
    Some(out)
}
