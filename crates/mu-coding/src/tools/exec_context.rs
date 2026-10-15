//! mu-09z5y: a redacted snapshot of the execution context bash and watch
//! children run in — the [`EventPayload::ExecContext`] a session records
//! when it is built in a daemon.
//!
//! It describes what a child RECEIVES, not what the daemon has: a
//! strict-mode child gets only the whitelisted environment
//! ([`bash::strict_child_env`], the same function the command builder uses),
//! a yolo child gets the daemon's whole environment. Of that, a value is
//! recorded only for a name on the recording list that is not secret-named,
//! and only if the value contains no URL ([`value_is_recordable`]).
//! Every other variable is recorded by name only. Values are recorded as
//! they are or not at all: nothing is partly redacted, so there is no
//! scrubber to slip an encoding past.

use std::collections::BTreeMap;
use std::ffi::OsString;

use mu_core::event_log::EventPayload;

use super::bash::{self, BashMode};

/// The recording list used when `[tools] record_env` is unset: variables
/// that change what a command does or resolves to, and carry no secrets.
/// `PATH` decides which binary a bare name runs; locale and `TZ` change
/// sorting, matching and dates.
pub const DEFAULT_RECORD_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "SHELL",
    "TERM",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_COLLATE",
    "TZ",
    "TMPDIR",
    "PWD",
    "VIRTUAL_ENV",
    "CARGO_TARGET_DIR",
];

/// Build the exec-context event for `mode` from the daemon's environment
/// `vars` (`std::env::vars_os`, which never panics). `record_env` is the
/// operator's recording list (`None` = the default list).
///
/// Bytes that are not valid Unicode never fail the snapshot: a name that
/// isn't Unicode can't match the list, and a value that isn't Unicode is
/// never recorded, so either way the variable is withheld (by its lossy
/// name).
pub fn snapshot(
    mode: &BashMode,
    record_env: Option<&[String]>,
    vars: impl IntoIterator<Item = (OsString, OsString)>,
) -> EventPayload {
    let (bash_mode, allowlist, child_env) = match mode {
        BashMode::Strict { allowlist, prompt } => (
            if *prompt { "strict_prompt" } else { "strict" },
            allowlist.iter().map(|entry| entry.join(" ")).collect(),
            bash::strict_child_env(vars),
        ),
        BashMode::Yolo => ("yolo", Vec::new(), vars.into_iter().collect()),
    };
    let recordable = |name: &str| match record_env {
        Some(list) => list.iter().any(|n| n == name),
        None => DEFAULT_RECORD_ENV.contains(&name),
    };
    let mut env = BTreeMap::new();
    let mut env_withheld = Vec::new();
    for (name, value) in child_env {
        match name.to_str() {
            Some(n) if recordable(n) && !bash::is_secret_env_var(n) => {
                match value.to_str().filter(|v| value_is_recordable(v)) {
                    Some(v) => {
                        env.insert(n.to_owned(), v.to_owned());
                    }
                    None => env_withheld.push(n.to_owned()),
                }
            }
            _ => env_withheld.push(name.to_string_lossy().into_owned()),
        }
    }
    env_withheld.sort();
    env_withheld.dedup();
    EventPayload::ExecContext {
        bash_mode: bash_mode.to_owned(),
        allowlist,
        env,
        env_withheld,
    }
}

/// Whether a listed, non-secret-named variable's VALUE may be recorded:
/// never when it contains a URL (`://`). The variable is then recorded by
/// name only.
///
/// No rule can tell a URL that carries a credential from one that doesn't:
/// review found a way around every narrower version (partial redaction of
/// userinfo and query keys; then withholding named dangerous characters,
/// which missed a `#` fragment; then a safe-character allowlist, which
/// can't see a secret embedded in a path, as in a webhook URL). So URLs are
/// not recorded at all. The default recording list holds no URLs.
///
/// This is the ONLY value check: a secret in some other shape (a bare
/// token, a JSON blob), in a variable whose name isn't secret-looking, is
/// recorded, so `[tools] record_env` should list only variables known not
/// to carry secrets.
pub fn value_is_recordable(value: &str) -> bool {
    !value.contains("://")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    fn parts(p: EventPayload) -> (String, Vec<String>, BTreeMap<String, String>, Vec<String>) {
        match p {
            EventPayload::ExecContext {
                bash_mode,
                allowlist,
                env,
                env_withheld,
            } => (bash_mode, allowlist, env, env_withheld),
            other => panic!("expected ExecContext, got {other:?}"),
        }
    }

    const DAEMON_ENV: &[(&str, &str)] = &[
        ("PATH", "/usr/bin:/bin"),
        ("HOME", "/home/u"),
        ("VIRTUAL_ENV", "/home/u/.venv"),
        ("OPENROUTER_API_KEY", "sk-or-very-secret"),
        ("HTTPS_PROXY", "http://bob:hunter2@proxy:3128"),
        ("EDITOR", "vi"),
    ];

    #[test]
    fn yolo_records_listed_values_and_names_the_rest() {
        let (mode, allowlist, env, withheld) =
            parts(snapshot(&BashMode::Yolo, None, vars(DAEMON_ENV)));
        assert_eq!(mode, "yolo");
        assert!(allowlist.is_empty());
        assert_eq!(env.get("PATH").map(String::as_str), Some("/usr/bin:/bin"));
        assert_eq!(
            env.get("VIRTUAL_ENV").map(String::as_str),
            Some("/home/u/.venv")
        );
        assert_eq!(
            withheld,
            vec!["EDITOR", "HTTPS_PROXY", "OPENROUTER_API_KEY"]
        );
    }

    #[test]
    fn a_secret_value_never_appears_whatever_the_list_says() {
        let list = vec!["OPENROUTER_API_KEY".to_owned(), "PATH".to_owned()];
        let p = snapshot(&BashMode::Yolo, Some(&list), vars(DAEMON_ENV));
        let row = serde_json::to_string(&p).expect("serialize");
        assert!(!row.contains("sk-or-very-secret"), "{row}");
        let (_, _, env, withheld) = parts(p);
        assert!(!env.contains_key("OPENROUTER_API_KEY"));
        assert!(withheld.contains(&"OPENROUTER_API_KEY".to_owned()));
    }

    #[test]
    fn strict_records_only_what_the_child_receives() {
        let mode = BashMode::strict_with_extras(&[], false);
        let (name, allowlist, env, withheld) = parts(snapshot(&mode, None, vars(DAEMON_ENV)));
        assert_eq!(name, "strict");
        assert!(allowlist.iter().any(|e| e == "git status"), "{allowlist:?}");
        // the strict child gets the whitelist only: no VIRTUAL_ENV, no key,
        // no proxy, so none of them appear at all, not even by name
        assert_eq!(
            env.keys().cloned().collect::<Vec<_>>(),
            vec!["HOME", "PATH"]
        );
        assert!(withheld.is_empty(), "{withheld:?}");
        let (name, ..) = parts(snapshot(
            &BashMode::strict_with_extras(&[], true),
            None,
            vars(DAEMON_ENV),
        ));
        assert_eq!(name, "strict_prompt");
    }

    #[test]
    fn an_operator_list_replaces_the_default() {
        let list = vec!["EDITOR".to_owned(), "HTTPS_PROXY".to_owned()];
        let (_, _, env, withheld) = parts(snapshot(&BashMode::Yolo, Some(&list), vars(DAEMON_ENV)));
        assert_eq!(env.get("EDITOR").map(String::as_str), Some("vi"));
        // a listed proxy URL carrying credentials is withheld whole
        assert!(!env.contains_key("HTTPS_PROXY"));
        assert!(withheld.contains(&"HTTPS_PROXY".to_owned()));
        assert!(withheld.contains(&"PATH".to_owned()));
    }

    /// A variable that is not valid Unicode, in name or value, never panics
    /// the snapshot (`std::env::vars` would).
    #[test]
    fn non_unicode_variables_do_not_panic() {
        use std::os::unix::ffi::OsStringExt;
        let mut env = vars(DAEMON_ENV);
        env.push((OsString::from_vec(b"BAD\xffNAME".to_vec()), "x".into()));
        env.push(("TZ".into(), OsString::from_vec(b"Europe/\xff".to_vec())));
        let (_, _, recorded, withheld) = parts(snapshot(&BashMode::Yolo, None, env.clone()));
        // a non-Unicode value is withheld, not recorded lossily
        assert!(!recorded.contains_key("TZ"));
        assert!(withheld.contains(&"TZ".to_owned()), "{withheld:?}");
        assert!(
            withheld.iter().any(|n| n.starts_with("BAD")),
            "{withheld:?}"
        );
        // strict drops the non-Unicode name rather than panicking
        let (_, _, recorded, _) = parts(snapshot(
            &BashMode::strict_with_extras(&[], false),
            None,
            env,
        ));
        assert!(recorded.keys().all(|k| !k.starts_with("BAD")));
    }

    /// Every URL is withheld, including each shape an earlier rule missed;
    /// values without a URL are recorded.
    #[test]
    fn values_that_could_hide_a_credential_are_withheld() {
        for withheld in [
            "http://bob:hunter2@proxy:3128",
            "https://host1,https://user:secret@host2",
            "https://h/?%74oken=secret",
            "https://app/cb#access_token=ya29-secret",
            "https://hooks.slack.com/services/T123/B456/secret123",
            "postgres://u:p@db.local/app",
            // even a plain URL: no URL is recorded
            "http://proxy:3128",
        ] {
            assert!(!value_is_recordable(withheld), "{withheld}");
        }
        for recorded in [
            "/usr/bin:/bin",
            "en_US.UTF-8",
            "user@example.com",
            "50%",
            "a?b#c",
        ] {
            assert!(value_is_recordable(recorded), "{recorded}");
        }
    }

    #[test]
    fn a_withheld_value_never_reaches_the_row() {
        let list = vec!["HTTPS_PROXY".to_owned(), "CB".to_owned()];
        let mut env = vars(DAEMON_ENV);
        env.push((
            "CB".into(),
            "https://hooks.slack.com/services/T1/B2/secret".into(),
        ));
        let row =
            serde_json::to_string(&snapshot(&BashMode::Yolo, Some(&list), env)).expect("serialize");
        assert!(!row.contains("hunter2"), "{row}");
        assert!(!row.contains("secret"), "{row}");
    }
}
