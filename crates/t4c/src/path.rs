//! Capability addressing: a shallow, dotted path `source.tool.subcommand`.
//!
//! Shallow is a design choice, not a limitation. A model consumer arrives
//! guessing structure, and deep hierarchies multiply the guessing — so past a
//! few segments the tail is free-form arguments, not more tree. The first
//! segment is the *source class* (`bash`, `mcp`, `skill`, …), which lines up
//! with mu's capability-manifest categories.

use std::fmt;

/// Maximum addressable depth. Beyond this, content is arguments, not path.
pub const MAX_DEPTH: usize = 3;

/// A dotted capability path, e.g. `bash.jj.status` or `mcp.code-index.recall`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CapPath {
    segments: Vec<String>,
}

impl CapPath {
    /// Parse a dotted path. Rejects empty segments and depth greater than
    /// [`MAX_DEPTH`].
    pub fn parse(s: &str) -> Result<Self, PathError> {
        let segments: Vec<String> = s.split('.').map(|p| p.trim().to_string()).collect();
        if segments.iter().any(String::is_empty) {
            return Err(PathError::EmptySegment(s.to_string()));
        }
        if segments.len() > MAX_DEPTH {
            return Err(PathError::TooDeep {
                path: s.to_string(),
                depth: segments.len(),
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
    /// The path exceeded [`MAX_DEPTH`].
    TooDeep { path: String, depth: usize },
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathError::EmptySegment(p) => write!(f, "empty segment in path {p:?}"),
            PathError::TooDeep { path, depth } => write!(
                f,
                "path {path:?} has depth {depth}, exceeds max {MAX_DEPTH} \
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
    fn rejects_too_deep() {
        assert!(matches!(
            CapPath::parse("a.b.c.d"),
            Err(PathError::TooDeep { depth: 4, .. })
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
