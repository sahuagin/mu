use std::path::PathBuf;

/// Expand human path shorthand accepted by shell users without turning
/// structured path arguments into shell strings. Only leading `~` and
/// `~/...` are expanded; `$VARS`, globs, command substitution, and
/// `~user` are not.
pub fn expand_leading_tilde(path: &str) -> PathBuf {
    match path {
        "~" => home_dir().unwrap_or_else(|| PathBuf::from(path)),
        _ if path.starts_with("~/") => home_dir()
            .map(|home| home.join(&path[2..]))
            .unwrap_or_else(|| PathBuf::from(path)),
        _ => PathBuf::from(path),
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_current_users_home() {
        let Some(home) = home_dir() else {
            return;
        };
        assert_eq!(expand_leading_tilde("~"), home);
        assert_eq!(expand_leading_tilde("~/src"), home.join("src"));
    }

    #[test]
    fn does_not_expand_other_shell_syntax() {
        assert_eq!(
            expand_leading_tilde("~other/src"),
            PathBuf::from("~other/src")
        );
        assert_eq!(
            expand_leading_tilde("$HOME/src"),
            PathBuf::from("$HOME/src")
        );
        assert_eq!(expand_leading_tilde("*.rs"), PathBuf::from("*.rs"));
    }
}
