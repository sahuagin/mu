//! Discover and load declarative skills from SKILL.md files.
//!
//! A skill on disk is a directory containing a `SKILL.md` with YAML
//! frontmatter (`name`, `description`) and a markdown body.  Optional
//! sibling files under `references/` are loaded as additional spans.
//!
//! Discovery walks a priority-ordered list of directories; first
//! directory wins on name collisions (project skills shadow user
//! skills shadow compat-read skills).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::context::{RetentionClass, Span, SpanKind};
use super::Skill;

/// Parsed YAML frontmatter from a SKILL.md file.
#[derive(Debug, Clone, Deserialize)]
pub struct SkillFrontmatter {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub when_to_use: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    /// If true, the skill is only invoked via explicit `/<name>` and
    /// is not advertised in the model's system prompt index.
    #[serde(default, rename = "disable-model-invocation")]
    pub disable_model_invocation: bool,
    /// Optional path globs for auto-activation (future).
    #[serde(default)]
    pub paths: Vec<String>,
    /// Declared compatible runtimes (e.g., `["mu", "claude-code"]`).
    #[serde(default)]
    pub runtime: Vec<String>,
}

/// A loaded skill: parsed frontmatter + constructed `Skill` ready
/// for registration with `SkillManager`.
#[derive(Debug, Clone)]
pub struct LoadedSkill {
    pub frontmatter: SkillFrontmatter,
    pub skill: Skill,
    pub source_dir: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("SKILL.md not found in {0}")]
    NoSkillFile(PathBuf),
    #[error("no YAML frontmatter (missing --- delimiters) in {0}")]
    NoFrontmatter(PathBuf),
    #[error("YAML parse error in {path}: {source}")]
    Yaml {
        path: PathBuf,
        source: serde_yaml::Error,
    },
    #[error("I/O error reading {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Parse a SKILL.md file into frontmatter + body.
///
/// The file format is:
/// ```text
/// ---
/// name: my-skill
/// description: what it does
/// ---
///
/// # Markdown body
/// ...
/// ```
fn parse_skill_md(content: &str, path: &Path) -> Result<(SkillFrontmatter, String), LoadError> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return Err(LoadError::NoFrontmatter(path.to_owned()));
    }

    // Find the closing --- after the opening one.
    let after_open = &trimmed[3..];
    let close_pos = after_open.find("\n---").ok_or_else(|| {
        LoadError::NoFrontmatter(path.to_owned())
    })?;

    let yaml_str = &after_open[..close_pos];
    let frontmatter: SkillFrontmatter =
        serde_yaml::from_str(yaml_str).map_err(|e| LoadError::Yaml {
            path: path.to_owned(),
            source: e,
        })?;

    // Body is everything after the closing --- and its newline.
    let body_start = 3 + close_pos + 4; // "---" + yaml + "\n---"
    let body = if body_start < trimmed.len() {
        trimmed[body_start..].trim_start_matches('\n').to_owned()
    } else {
        String::new()
    };

    Ok((frontmatter, body))
}

/// Load a single skill from a directory containing SKILL.md.
pub fn load_skill_dir(dir: &Path) -> Result<LoadedSkill, LoadError> {
    let skill_path = dir.join("SKILL.md");
    let content = fs::read_to_string(&skill_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            LoadError::NoSkillFile(dir.to_owned())
        } else {
            LoadError::Io {
                path: skill_path.clone(),
                source: e,
            }
        }
    })?;

    let (frontmatter, body) = parse_skill_md(&content, &skill_path)?;
    let id = &frontmatter.name;

    let mut spans = Vec::new();

    // Primary span: the SKILL.md body.
    if !body.is_empty() {
        spans.push(Span::new(
            format!("skill:{id}:SKILL.md"),
            SpanKind::SkillActivation,
            body,
            RetentionClass::Pinned,
        ));
    }

    // Reference files: references/*.md (sorted for determinism).
    let refs_dir = dir.join("references");
    if refs_dir.is_dir() {
        let mut ref_files: Vec<PathBuf> = fs::read_dir(&refs_dir)
            .map_err(|e| LoadError::Io {
                path: refs_dir.clone(),
                source: e,
            })?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("md") {
                    Some(path)
                } else {
                    None
                }
            })
            .collect();
        ref_files.sort();

        for ref_path in ref_files {
            let ref_content =
                fs::read_to_string(&ref_path).map_err(|e| LoadError::Io {
                    path: ref_path.clone(),
                    source: e,
                })?;
            let file_name = ref_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown.md");
            spans.push(Span::new(
                format!("skill:{id}:references/{file_name}"),
                SpanKind::SkillActivation,
                ref_content,
                RetentionClass::Pinned,
            ));
        }
    }

    Ok(LoadedSkill {
        skill: Skill::new(id.clone(), spans),
        frontmatter,
        source_dir: dir.to_owned(),
    })
}

/// Default discovery directories in priority order.
///
/// Higher priority directories shadow lower ones on name collision.
/// `project_root` is optional — when `None`, only user/compat dirs
/// are searched.
pub fn default_search_dirs(project_root: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::with_capacity(3);
    if let Some(root) = project_root {
        dirs.push(root.join(".mu/skills"));
    }
    if let Some(config) = dirs::config_dir() {
        dirs.push(config.join("mu/skills"));
    }
    // Compat-read: claude-code skills directory.
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".claude-personal/skills"));
    }
    dirs
}

/// Discover and load all skills from the given search directories.
///
/// First-dir-wins: if a skill named `foo` appears in both
/// `<project>/.mu/skills/foo/` and `~/.config/mu/skills/foo/`, the
/// project one wins. Errors on individual skills are logged via
/// `tracing::warn` and skipped.
pub fn discover_skills(search_dirs: &[PathBuf]) -> Vec<LoadedSkill> {
    let mut seen: HashMap<String, LoadedSkill> = HashMap::new();

    for search_dir in search_dirs {
        if !search_dir.is_dir() {
            continue;
        }
        let entries = match fs::read_dir(search_dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(?search_dir, error = %e, "cannot read skill search dir");
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();

            // Skill dirs contain SKILL.md; skip non-directories and
            // symlinks that resolve to directories (follow them).
            let is_dir = path.is_dir();
            if !is_dir {
                continue;
            }

            match load_skill_dir(&path) {
                Ok(loaded) => {
                    // First-dir-wins dedup.
                    if !seen.contains_key(&loaded.frontmatter.name) {
                        seen.insert(loaded.frontmatter.name.clone(), loaded);
                    }
                }
                Err(LoadError::NoSkillFile(_)) => {
                    // Directory without SKILL.md — not a skill, skip silently.
                }
                Err(e) => {
                    tracing::warn!(dir = %path.display(), error = %e, "skipping broken skill");
                }
            }
        }
    }

    let mut skills: Vec<LoadedSkill> = seen.into_values().collect();
    skills.sort_by(|a, b| a.frontmatter.name.cmp(&b.frontmatter.name));
    skills
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_skill(dir: &Path, name: &str, body: &str) {
        let skill_dir = dir.join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: test skill {name}\n---\n\n{body}\n"
            ),
        )
        .unwrap();
    }

    fn write_skill_with_refs(dir: &Path, name: &str, body: &str, refs: &[(&str, &str)]) {
        write_skill(dir, name, body);
        let refs_dir = dir.join(name).join("references");
        fs::create_dir_all(&refs_dir).unwrap();
        for (fname, content) in refs {
            fs::write(refs_dir.join(fname), content).unwrap();
        }
    }

    #[test]
    fn parse_simple_frontmatter() {
        let content = "---\nname: review\ndescription: code review\n---\n\n# Body\nHello.\n";
        let (fm, body) = parse_skill_md(content, Path::new("test.md")).unwrap();
        assert_eq!(fm.name, "review");
        assert_eq!(fm.description, "code review");
        assert!(body.starts_with("# Body"));
        assert!(!fm.disable_model_invocation);
    }

    #[test]
    fn parse_multiline_description() {
        let content = "---\nname: goal\ndescription: >-\n  Set up a goal-driven\n  autonomous session.\n---\n\nBody here.\n";
        let (fm, body) = parse_skill_md(content, Path::new("test.md")).unwrap();
        assert_eq!(fm.name, "goal");
        assert!(fm.description.contains("goal-driven"));
        assert_eq!(body, "Body here.\n");
    }

    #[test]
    fn parse_disable_model_invocation() {
        let content = "---\nname: secret\ndescription: hidden\ndisable-model-invocation: true\n---\n\nBody.\n";
        let (fm, _) = parse_skill_md(content, Path::new("test.md")).unwrap();
        assert!(fm.disable_model_invocation);
    }

    #[test]
    fn missing_frontmatter_errors() {
        let content = "# Just markdown\n\nNo frontmatter here.\n";
        let err = parse_skill_md(content, Path::new("test.md")).unwrap_err();
        assert!(matches!(err, LoadError::NoFrontmatter(_)));
    }

    #[test]
    fn load_skill_dir_body_and_refs() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill_with_refs(
            tmp.path(),
            "jj-runbook",
            "# jj recovery",
            &[
                ("recovery-a.md", "Recovery A content"),
                ("recovery-b.md", "Recovery B content"),
                ("not-md.txt", "ignored"),
            ],
        );

        let loaded = load_skill_dir(&tmp.path().join("jj-runbook")).unwrap();
        assert_eq!(loaded.frontmatter.name, "jj-runbook");
        assert_eq!(loaded.skill.id, "jj-runbook");
        // 1 body + 2 reference .md files (txt ignored).
        assert_eq!(loaded.skill.spans.len(), 3);
        assert_eq!(loaded.skill.spans[0].id(), "skill:jj-runbook:SKILL.md");
        assert_eq!(
            loaded.skill.spans[1].id(),
            "skill:jj-runbook:references/recovery-a.md"
        );
        assert_eq!(
            loaded.skill.spans[2].id(),
            "skill:jj-runbook:references/recovery-b.md"
        );
    }

    #[test]
    fn load_skill_dir_no_refs() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "postmortem", "# Template");

        let loaded = load_skill_dir(&tmp.path().join("postmortem")).unwrap();
        assert_eq!(loaded.skill.spans.len(), 1);
    }

    #[test]
    fn discover_deduplicates_first_wins() {
        let project = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();

        write_skill(project.path(), "review", "# Project review");
        write_skill(user.path(), "review", "# User review (shadowed)");
        write_skill(user.path(), "postmortem", "# User postmortem");

        let skills = discover_skills(&[project.path().to_owned(), user.path().to_owned()]);
        assert_eq!(skills.len(), 2);

        let review = skills.iter().find(|s| s.frontmatter.name == "review").unwrap();
        assert_eq!(review.source_dir, project.path().join("review"));

        let pm = skills.iter().find(|s| s.frontmatter.name == "postmortem").unwrap();
        assert_eq!(pm.source_dir, user.path().join("postmortem"));
    }

    #[test]
    fn discover_skips_missing_dirs() {
        let skills = discover_skills(&[PathBuf::from("/nonexistent/path/skills")]);
        assert!(skills.is_empty());
    }

    #[test]
    fn discover_skips_broken_skills() {
        let dir = tempfile::tempdir().unwrap();
        let broken = dir.path().join("broken");
        fs::create_dir_all(&broken).unwrap();
        fs::write(broken.join("SKILL.md"), "no frontmatter here").unwrap();

        write_skill(dir.path(), "good", "# Good skill");

        let skills = discover_skills(&[dir.path().to_owned()]);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].frontmatter.name, "good");
    }
}
