//! Capability addressing: a shallow, dotted path `source.tool.subcommand`.
//!
//! Shallow is a design choice, not a limitation. A model consumer arrives
//! guessing structure, and deep hierarchies multiply the guessing — so past a
//! few segments the tail is free-form arguments, not more tree. The first
//! segment is the *source class* (`bash`, `mcp`, `skill`, …), which lines up
//! with mu's capability-manifest categories.

use std::fmt;

/// Default maximum addressable depth (number of dotted segments). Beyond this,
/// the tail is arguments, not path. Override at runtime with `T4C_MAX_PATH_DEPTH`.
/// Sized for the deepest real tools — `git remote add`, `agent memory add`,
/// `gh pr create`, `jj git push` are all `class.tool.sub.sub` (depth 4) — with
/// one level of headroom.
pub const DEFAULT_MAX_DEPTH: usize = 5;

/// The active maximum path depth: `T4C_MAX_PATH_DEPTH` when set to a valid usize
/// (>= 1), else [`DEFAULT_MAX_DEPTH`].
pub fn max_depth() -> usize {
    std::env::var("T4C_MAX_PATH_DEPTH")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(DEFAULT_MAX_DEPTH)
}

/// A dotted capability path, e.g. `bash.jj.status` or `mcp.code-index.recall`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CapPath {
    segments: Vec<String>,
}

impl CapPath {
    /// Parse a dotted path. Rejects empty segments and depth greater than the
    /// active [`max_depth`] ([`DEFAULT_MAX_DEPTH`], overridable via
    /// `T4C_MAX_PATH_DEPTH`).
    pub fn parse(s: &str) -> Result<Self, PathError> {
        let segments: Vec<String> = s.split('.').map(|p| p.trim().to_string()).collect();
        if segments.iter().any(String::is_empty) {
            return Err(PathError::EmptySegment(s.to_string()));
        }
        let max = max_depth();
        if segments.len() > max {
            return Err(PathError::TooDeep {
                path: s.to_string(),
                depth: segments.len(),
                max,
            });
        }
        Ok(Self { segments })
    }

    /// The path segments, outermost first.
    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    /// Depth (number of segments).
    pub fn depth(&self) -> usize {
        self.segments.len()
    }

    /// The first segment — the source class (`bash`, `mcp`, `skill`, …).
    pub fn source(&self) -> &str {
        &self.segments[0]
    }

    /// Default invocation argv: the path minus its source segment
    /// (`bash.jj.status` -> `["jj", "status"]`). A source can override this.
    pub fn invoke_argv(&self) -> Vec<String> {
        self.segments.iter().skip(1).cloned().collect()
    }

    /// True if `self` is `prefix` or lies under it — the predicate behind a
    /// subtree walk (`bash` matches `bash.jj`, `bash.jj.status`, …).
    pub fn starts_with(&self, prefix: &CapPath) -> bool {
        prefix.segments.len() <= self.segments.len()
            && prefix
                .segments
                .iter()
                .zip(&self.segments)
                .all(|(a, b)| a == b)
    }
}

impl fmt::Display for CapPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.segments.join("."))
    }
}

/// Errors from [`CapPath::parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    /// A segment was empty (e.g. `bash..status` or a leading/trailing dot).
    EmptySegment(String),
    /// The path exceeded the active [`max_depth`].
    TooDeep {
        path: String,
        depth: usize,
        max: usize,
    },
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathError::EmptySegment(p) => write!(f, "empty segment in path {p:?}"),
            PathError::TooDeep { path, depth, max } => write!(
                f,
                "path {path:?} has depth {depth}, exceeds max {max} \
                 (the tail should be arguments, not path)"
            ),
        }
    }
}

impl std::error::Error for PathError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dotted_path() {
        let p = CapPath::parse("mcp.code-index.recall").unwrap();
        assert_eq!(p.depth(), 3);
        assert_eq!(p.source(), "mcp");
        assert_eq!(p.to_string(), "mcp.code-index.recall");
        assert_eq!(
            p.invoke_argv(),
            vec!["code-index".to_string(), "recall".to_string()]
        );
    }

    #[test]
    fn rejects_beyond_max_depth() {
        // Under the default cap of 5, depth 4 (e.g. `bash.gh.pr.create`) and
        // depth 5 parse; depth 6 is too deep.
        assert!(CapPath::parse("a.b.c.d").is_ok());
        assert!(CapPath::parse("a.b.c.d.e").is_ok());
        assert!(matches!(
            CapPath::parse("a.b.c.d.e.f"),
            Err(PathError::TooDeep { depth: 6, .. })
        ));
    }

    #[test]
    fn rejects_empty_segment() {
        assert!(matches!(
            CapPath::parse("bash..status"),
            Err(PathError::EmptySegment(_))
        ));
        assert!(matches!(
            CapPath::parse(".bash"),
            Err(PathError::EmptySegment(_))
        ));
    }

    #[test]
    fn prefix_walk_predicate() {
        let bash = CapPath::parse("bash").unwrap();
        let status = CapPath::parse("bash.jj.status").unwrap();
        let mcp = CapPath::parse("mcp.code-index").unwrap();
        assert!(status.starts_with(&bash));
        assert!(status.starts_with(&status));
        assert!(!status.starts_with(&mcp));
        assert!(!bash.starts_with(&status)); // a shorter path can't be under a longer one
    }
}
