//! Config problems that no retry can fix, found before the daemon starts
//! (bead mu-mu-check-config-8qm1k).
//!
//! One check, [`problems`], used twice: `mu serve` refuses to start on any
//! problem in the config itself ([`ConfigRefused`], exit [`EXIT_CODE`]), and `mu check-config`
//! reports them with where each relevant value came from ([`report`]), so a
//! config can be verified without starting a daemon. An unreachable NATS
//! server is not a config problem; the clients reconnect. Both print the
//! `mu: CONFIG ERROR` banner shape that `loud_config_drop` (mu-core config)
//! uses and mu-solo forwards to its UI.
//!
//! Secrets are never printed: `[mesh].issuer_key` shows as set or unset.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use mu_core::config::{Config, MeshConfig};

/// `EX_CONFIG` from sysexits.h, the code `MU_CONFIG_STRICT` already exits
/// with for a config it had to drop.
pub const EXIT_CODE: i32 = 78;

/// One thing wrong with the effective config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// What is wrong, one line.
    pub headline: String,
    /// Why it matters or how it came about, one line per entry.
    pub detail: Vec<String>,
    /// How to fix it, one line per alternative.
    pub fixes: Vec<String>,
}

/// The mesh features that sign or verify with `[mesh].issuer_key`, each
/// with the flag that turns it off.
fn key_users(mesh: &MeshConfig) -> Vec<&'static str> {
    let mut users = Vec::new();
    if mesh.serves() {
        users.push("serve");
    }
    if mesh.consumes_code_index() {
        users.push("consume_code_index");
    }
    if mesh.dialogues() {
        users.push("dialogue");
    }
    users
}

/// Is `hex` a key the daemon can use? Parsed exactly as the mesh does.
fn issuer_key_parses(hex: &str) -> bool {
    biscuit_auth::PrivateKey::from_bytes_hex(hex, biscuit_auth::builder::Algorithm::Ed25519).is_ok()
}

/// Every problem in `config`. `keygen_target` is the file the fix should
/// tell the operator to write a key into.
pub fn problems(config: &Config, keygen_target: &Path) -> Vec<Problem> {
    let mut out = Vec::new();
    let mesh = &config.mesh;
    let users = key_users(mesh);
    if !users.is_empty() {
        let state = if mesh.issuer_key.is_empty() {
            Some("is missing")
        } else if !issuer_key_parses(&mesh.issuer_key) {
            Some("is not a hex Ed25519 private key")
        } else {
            None
        };
        if let Some(state) = state {
            let flags = users
                .iter()
                .map(|u| format!("[mesh] {u}"))
                .collect::<Vec<_>>()
                .join(", ");
            let mut detail = vec![format!(
                "used by {flags} to sign and verify capabilities; without it nothing on the mesh \
                 can be trusted"
            )];
            if users.contains(&"serve") {
                detail.push("[mesh] serve defaults to true when [mesh] enabled = true".to_string());
            }
            let off = users
                .iter()
                .map(|u| format!("{u} = false"))
                .collect::<Vec<_>>()
                .join(", ");
            out.push(Problem {
                headline: format!("[mesh].issuer_key {state}"),
                detail,
                fixes: vec![
                    format!("set {off} under [mesh], or"),
                    format!(
                        "run: mu mesh keygen --write {} (writes the key, never prints it)",
                        keygen_target.display()
                    ),
                ],
            });
        }
    }
    // The other mesh settings the daemon refuses at startup, checked by the
    // same functions startup uses so the two cannot disagree.
    let refusals = [
        crate::serve::events_out::from_mesh(mesh).err(),
        crate::serve::registry::from_mesh(mesh).err(),
    ];
    for e in refusals.into_iter().flatten() {
        out.push(Problem {
            headline: e.to_string(),
            detail: Vec::new(),
            fixes: vec!["correct the value under [mesh], or turn that feature off".to_string()],
        });
    }
    out
}

/// The file `mu mesh keygen --write` should target in a fix: the highest
/// precedence config file that was loaded, else the operator's default.
pub fn keygen_target(sources: &[String]) -> PathBuf {
    sources
        .iter()
        .rev()
        .map(PathBuf::from)
        .find(|p| p.is_absolute())
        .or_else(|| Config::default_paths().pop())
        .unwrap_or_else(|| PathBuf::from("config.toml"))
}

/// The banner for `problems`: one `mu: CONFIG ERROR` block each, ended by a
/// blank line, so a spawner that scans stderr for the banner gets each one
/// whole.
pub fn banner(problems: &[Problem], sources: &[String], consequence: &str) -> String {
    let mut s = String::new();
    for p in problems {
        s.push_str(&format!("mu: CONFIG ERROR — {}\n", p.headline));
        for d in &p.detail {
            s.push_str(&format!("    {d}\n"));
        }
        s.push_str(&format!("    config loaded from: {}\n", sources.join(", ")));
        for (i, f) in p.fixes.iter().enumerate() {
            let lead = if i == 0 { "-> fix:" } else { "       " };
            s.push_str(&format!("    {lead} {f}\n"));
        }
        s.push_str(&format!("    {consequence}\n\n"));
    }
    s
}

/// `mu serve` refused to start: the config has problems no retry can fix.
/// Its `Display` is the banner; the binary prints it and exits
/// [`EXIT_CODE`].
#[derive(Debug)]
pub struct ConfigRefused {
    pub problems: Vec<Problem>,
    pub sources: Vec<String>,
}

impl fmt::Display for ConfigRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&banner(
            &self.problems,
            &self.sources,
            "mu serve refused to start; `mu check-config` shows where each value came from.",
        ))
    }
}

impl std::error::Error for ConfigRefused {}

/// Where one `[section].field` is set across `paths` (lowest precedence
/// first): the winning file, or None for the built-in default.
fn origin(files: &[(PathBuf, toml::Value)], section: &str, field: &str) -> Option<PathBuf> {
    files
        .iter()
        .rev()
        .find(|(_, v)| v.get(section).and_then(|s| s.get(field)).is_some())
        .map(|(p, _)| p.clone())
}

/// The `mu check-config` report over the layered files in `paths`, and
/// whether the config is clean. Loads exactly as `mu serve` does.
pub fn report(paths: &[PathBuf]) -> (String, bool) {
    // Quiet: the loader's own banner would print before this report, and
    // under MU_CONFIG_STRICT it would exit before the report could finish.
    let (config, sources, drops) = Config::load_with_sources_quiet(paths);
    let mut out = String::from("mu check-config\n  config files, lowest precedence first:\n");
    let mut parsed: Vec<(PathBuf, toml::Value)> = Vec::new();
    for p in paths {
        let shown = p.display().to_string();
        let state = if !p.exists() {
            "absent"
        } else if sources.contains(&shown) {
            if let Some(v) = std::fs::read_to_string(p)
                .ok()
                .and_then(|t| t.parse::<toml::Value>().ok())
            {
                parsed.push((p.clone(), v));
            }
            "loaded"
        } else {
            "IGNORED (see below)"
        };
        out.push_str(&format!("    {shown}  {state}\n"));
    }

    let mesh = &config.mesh;
    out.push_str("  [mesh]\n");
    let key_state = if mesh.issuer_key.is_empty() {
        "(unset)"
    } else {
        "(set)"
    };
    let rows: [(&str, String); 5] = [
        ("enabled", mesh.enabled.to_string()),
        ("serve", mesh.serve.to_string()),
        ("consume_code_index", mesh.consume_code_index.to_string()),
        ("dialogue", mesh.dialogue.to_string()),
        ("issuer_key", key_state.to_string()),
    ];
    for (field, value) in rows {
        let from = match origin(&parsed, "mesh", field) {
            Some(p) => format!("from {}", p.display()),
            None => "default".to_string(),
        };
        out.push_str(&format!("    {field:<19} = {value:<8} {from}\n"));
    }

    let mut blockers = problems(&config, &keygen_target(&sources));
    // `mu serve` reads [[tools.runner]] strictly and validates every entry
    // before it starts; the same two calls here.
    let runner_refusal = Config::load_runner_tools(paths)
        .map_err(anyhow::Error::msg)
        .and_then(|runners| {
            crate::serve::build_tools(&[], &crate::serve::BashSettings::default(), None, &runners)
                .map(|_| ())
        })
        .err();
    if let Some(e) = runner_refusal {
        blockers.push(Problem {
            headline: e.to_string(),
            detail: Vec::new(),
            fixes: vec!["correct the [tools] section or its [[tools.runner]] entries".to_string()],
        });
    }
    // An ignored layer is not a refusal: serve runs on what loaded, unless
    // MU_CONFIG_STRICT=1. It is still a problem worth failing the check on.
    let ignored: Vec<Problem> = drops
        .iter()
        .map(|d| Problem {
            headline: d.headline.clone(),
            detail: vec![d.detail.clone()],
            fixes: vec![format!(
                "fix it; until then {} is not applied",
                d.what_dropped
            )],
        })
        .collect();
    if blockers.is_empty() && ignored.is_empty() {
        out.push_str("  no problems\n");
        return (out, true);
    }
    out.push('\n');
    out.push_str(&banner(
        &blockers,
        &sources,
        "mu serve would refuse to start.",
    ));
    out.push_str(&banner(
        &ignored,
        &sources,
        "mu serve would start without that file's settings (MU_CONFIG_STRICT=1 refuses instead).",
    ));
    (out, false)
}

/// Generate an issuer key and write it to `[mesh].issuer_key` in `path`,
/// keeping the file's layout and permissions. The key is never returned or
/// printed. An existing key is replaced only with `force`, because every
/// capability minted with it stops verifying. A symlink is followed: the
/// file it points to is the one updated, and its path is returned.
pub fn write_issuer_key(path: &Path, force: bool) -> Result<PathBuf> {
    // Renaming over a symlink would replace the link, not the config it
    // points to, so resolve it first and work on the real file.
    let resolved = match std::fs::canonicalize(path) {
        Ok(real) => real,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if std::fs::symlink_metadata(path).is_ok() {
                bail!(
                    "{} is a symlink to a file that does not exist; point it at the config \
                     or pass the real path",
                    path.display()
                );
            }
            path.to_path_buf()
        }
        Err(e) => return Err(e).with_context(|| format!("resolving {}", path.display())),
    };
    let path = resolved.as_path();
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".into());
    // One keygen per file at a time, held across read, check and rename, so
    // two runs cannot both see no key and the second replace the first.
    let _lock = KeygenLock::acquire(&dir.join(format!(".{name}.keygen.lock")))?;
    write_issuer_key_locked(path, force, dir, &name)?;
    Ok(resolved)
}

/// A lock file created exclusively and removed on drop.
struct KeygenLock(PathBuf);

impl KeygenLock {
    fn acquire(path: &Path) -> Result<Self> {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(_) => Ok(Self(path.to_path_buf())),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => bail!(
                "another mu mesh keygen is writing this file (lock {}); if none is running, \
                 remove the lock and run again",
                path.display()
            ),
            Err(e) => Err(e).with_context(|| format!("creating {}", path.display())),
        }
    }
}

impl Drop for KeygenLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// 1-based line and column of byte `offset` in `text`.
fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, col)
}

fn write_issuer_key_locked(path: &Path, force: bool, dir: &Path, name: &str) -> Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    // The parser's own message quotes the offending line, which may hold a
    // secret, so only the position is reported and the error is dropped.
    let mut doc: toml_edit::DocumentMut = match text.parse() {
        Ok(doc) => doc,
        Err(e) => {
            let at = e
                .span()
                .map(|span| {
                    let (line, col) = line_col(&text, span.start);
                    format!(" at line {line}, column {col}")
                })
                .unwrap_or_default();
            bail!(
                "{} is not valid TOML{at}; fix it and run again",
                path.display()
            );
        }
    };
    let mesh = doc
        .entry("mesh")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_like_mut()
        .with_context(|| format!("[mesh] in {} is not a table", path.display()))?;
    let existing = mesh
        .get("issuer_key")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty());
    if existing && !force {
        bail!(
            "[mesh].issuer_key is already set in {}; --force replaces it, and every capability \
             minted with the old key stops verifying",
            path.display()
        );
    }
    let key = biscuit_auth::KeyPair::new().private().to_bytes_hex();
    mesh.insert("issuer_key", toml_edit::value(key));

    // Write beside the file and rename over it, so a crash leaves the old
    // file or the new one, never half of either.
    let tmp = dir.join(format!(".{name}.keygen-{}", std::process::id()));
    // The temp file is created owner-only before the key is written into
    // it, so the key is never in a file others can read; only then does it
    // take the target's mode (a new target stays owner-only).
    let target_mode = file_mode(path);
    let written = write_owner_only(&tmp, doc.to_string().as_bytes())
        .and_then(|()| match target_mode {
            Some(mode) => set_mode(&tmp, mode),
            None => Ok(()),
        })
        .and_then(|()| {
            std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
        });
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Create `path` (which must not exist) readable by its owner only, and
/// write `bytes` into it.
fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", path.display()))
}

/// The permission bits of `path`, or None when it does not exist (or on a
/// platform without them).
fn file_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .ok()
            .map(|m| m.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("setting permissions on {}", path.display()))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// Whether `path` is readable by anyone but its owner.
pub fn readable_by_others(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o044 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mesh_on(f: impl FnOnce(&mut MeshConfig)) -> Config {
        let mut c = Config::default();
        c.mesh.enabled = true;
        f(&mut c.mesh);
        c
    }

    fn valid_key() -> String {
        biscuit_auth::KeyPair::new().private().to_bytes_hex()
    }

    const TARGET: &str = "/home/op/.config/mu/config.toml";

    #[test]
    fn serve_by_default_without_a_key_is_a_problem() {
        let c = mesh_on(|_| {});
        let p = problems(&c, Path::new(TARGET));
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].headline, "[mesh].issuer_key is missing");
        assert!(p[0].detail.iter().any(|d| d.contains("defaults to true")));
        assert!(p[0].fixes[0].contains("serve = false"));
        assert!(p[0].fixes[1].contains(&format!("mu mesh keygen --write {TARGET}")));
    }

    #[test]
    fn an_unparseable_key_is_a_problem_and_is_not_echoed() {
        let c = mesh_on(|m| m.issuer_key = "deadbeefsecret".into());
        let p = problems(&c, Path::new(TARGET));
        assert_eq!(
            p[0].headline,
            "[mesh].issuer_key is not a hex Ed25519 private key"
        );
        let text = banner(&p, &["defaults".into()], "x");
        assert!(!text.contains("deadbeefsecret"), "{text}");
    }

    #[test]
    fn every_feature_that_needs_the_key_is_named() {
        let c = mesh_on(|m| {
            m.serve = false;
            m.consume_code_index = true;
            m.dialogue = true;
        });
        let p = problems(&c, Path::new(TARGET));
        assert_eq!(p.len(), 1);
        assert!(p[0].detail[0].contains("used by [mesh] consume_code_index, [mesh] dialogue to"));
        assert!(p[0].fixes[0].contains("consume_code_index = false, dialogue = false"));
        assert!(!p[0].detail.iter().any(|d| d.contains("defaults to true")));
    }

    #[test]
    fn no_problem_with_a_valid_key_or_without_key_users() {
        assert!(problems(&mesh_on(|m| m.issuer_key = valid_key()), Path::new(TARGET)).is_empty());
        assert!(problems(&mesh_on(|m| m.serve = false), Path::new(TARGET)).is_empty());
        // The master switch off: nothing on the mesh runs.
        assert!(problems(&Config::default(), Path::new(TARGET)).is_empty());
    }

    /// The rest of what startup refuses is a problem too, from the same
    /// validators, whether or not the key is in play.
    #[test]
    fn startup_refusals_beyond_the_key_are_problems() {
        let c = mesh_on(|m| {
            m.serve = false;
            m.events = true;
            m.events_queue_len = 0;
            m.registry = true;
            m.registry_ttl_secs = 1;
        });
        let p = problems(&c, Path::new(TARGET));
        assert_eq!(p.len(), 2, "{p:?}");
        assert!(p[0].headline.contains("events_queue_len"), "{p:?}");
        assert!(p[1].headline.contains("registry_ttl_secs"), "{p:?}");
    }

    #[test]
    fn the_banner_is_one_scannable_block_per_problem() {
        let p = problems(&mesh_on(|_| {}), Path::new(TARGET));
        let text = banner(
            &p,
            &["defaults".into(), TARGET.into()],
            "mu serve refused to start.",
        );
        assert!(text.starts_with("mu: CONFIG ERROR — [mesh].issuer_key is missing\n"));
        assert!(text.ends_with("mu serve refused to start.\n\n"));
        assert!(text.contains(&format!("config loaded from: defaults, {TARGET}")));
        // mu-solo's scanner stops a block at a blank line or 16 lines.
        let block: Vec<&str> = text.trim_end().lines().collect();
        assert!(block.len() < 16 && !block.iter().any(|l| l.is_empty()));
    }

    #[test]
    fn keygen_target_is_the_highest_precedence_loaded_file() {
        let s = vec![
            "defaults".to_string(),
            "/etc/mu/config.toml".to_string(),
            TARGET.to_string(),
            "cli:--bare".to_string(),
        ];
        assert_eq!(keygen_target(&s), PathBuf::from(TARGET));
    }

    #[test]
    fn report_shows_provenance_and_fails_on_a_problem() {
        let dir = tempfile::tempdir().unwrap();
        let site = dir.path().join("site.toml");
        let op = dir.path().join("op.toml");
        std::fs::write(&site, "[mesh]\nenabled = true\n").unwrap();
        let (text, ok) = report(&[site.clone(), op.clone()]);
        assert!(!ok, "{text}");
        assert!(
            text.contains(&format!("{}  loaded", site.display())),
            "{text}"
        );
        assert!(
            text.contains(&format!("{}  absent", op.display())),
            "{text}"
        );
        assert!(text.contains(&format!("from {}", site.display())), "{text}");
        assert!(text.contains("issuer_key          = (unset)"), "{text}");
        assert!(
            text.contains("mu: CONFIG ERROR — [mesh].issuer_key is missing"),
            "{text}"
        );

        let key = valid_key();
        std::fs::write(&op, format!("[mesh]\nissuer_key = \"{key}\"\n")).unwrap();
        let (text, ok) = report(&[site, op.clone()]);
        assert!(ok, "{text}");
        assert!(text.contains("no problems"), "{text}");
        assert!(
            text.contains(&format!("(set)    from {}", op.display())),
            "{text}"
        );
        assert!(!text.contains(&key), "the key must never be printed");
    }

    #[test]
    fn report_flags_a_file_that_was_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "[mesh]\nno_such_field = 1\n").unwrap();
        let (text, ok) = report(std::slice::from_ref(&bad));
        assert!(!ok, "{text}");
        assert!(text.contains("IGNORED"), "{text}");
        assert!(text.contains("your ENTIRE config was IGNORED"), "{text}");
        assert!(text.contains("unknown field `no_such_field`"), "{text}");
    }

    #[test]
    fn keygen_writes_a_usable_key_and_keeps_the_rest_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original =
            "# operator notes\n[mesh]\nenabled = true # on\n\n[journal]\nfsync = \"never\"\n";
        std::fs::write(&path, original).unwrap();
        write_issuer_key(&path, false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# operator notes"), "{text}");
        assert!(text.contains("enabled = true # on"), "{text}");
        assert!(text.contains("fsync = \"never\""), "{text}");
        let (c, _) = Config::load_with_sources(std::slice::from_ref(&path));
        assert!(issuer_key_parses(&c.mesh.issuer_key));
        assert!(problems(&c, &path).is_empty());

        // An existing key stays unless forced.
        let first = c.mesh.issuer_key.clone();
        let err = write_issuer_key(&path, false).unwrap_err().to_string();
        assert!(err.contains("--force"), "{err}");
        assert!(!err.contains(&first), "the key must never be printed");
        write_issuer_key(&path, true).unwrap();
        let (c, _) = Config::load_with_sources(&[path]);
        assert_ne!(c.mesh.issuer_key, first);
    }

    #[test]
    fn keygen_on_unparseable_toml_reports_the_position_not_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let secret = "0123456789abcdefsecretsecretsecret";
        std::fs::write(&path, format!("[mesh]\nissuer_key = \"{secret}\n")).unwrap();
        let err = format!("{:#}", write_issuer_key(&path, true).unwrap_err());
        assert!(err.contains("is not valid TOML at line 2"), "{err}");
        assert!(
            !err.contains(secret),
            "the offending line must not be echoed: {err}"
        );
        assert!(!err.contains("secretsecret"), "{err}");
    }

    #[test]
    fn a_second_keygen_on_the_same_file_waits_its_turn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let lock = KeygenLock::acquire(&dir.path().join(".config.toml.keygen.lock")).unwrap();
        let err = write_issuer_key(&path, false).unwrap_err().to_string();
        assert!(err.contains("another mu mesh keygen"), "{err}");
        assert!(
            !path.exists(),
            "nothing written while another holds the lock"
        );
        drop(lock);
        write_issuer_key(&path, false).unwrap();
        assert!(!dir.path().join(".config.toml.keygen.lock").exists());
    }

    /// A malformed line holding a secret is reported by position; neither
    /// the line nor the secret reaches the report.
    #[test]
    fn report_never_echoes_a_malformed_secret_line() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.toml");
        let secret = "0123456789abcdefsecretsecretsecret";
        std::fs::write(&bad, format!("[mesh]\nissuer_key = \"{secret}\n")).unwrap();
        let (text, ok) = report(std::slice::from_ref(&bad));
        assert!(!ok, "{text}");
        assert!(text.contains("line 2, column"), "{text}");
        assert!(!text.contains("secretsecret"), "{text}");
    }

    /// The runner tools serve would refuse fail the check too.
    #[test]
    fn report_fails_on_a_runner_tool_serve_would_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");
        std::fs::write(
            &cfg,
            "[[tools.runner]]\nname = \"bash\"\ndescription = \"d\"\ngrant = \"g\"\n\
             runner = \"/bin/true\"\n",
        )
        .unwrap();
        let (text, ok) = report(std::slice::from_ref(&cfg));
        assert!(!ok, "{text}");
        assert!(text.contains("is a reserved tool name"), "{text}");
        assert!(text.contains("mu serve would refuse to start"), "{text}");
    }

    #[test]
    fn report_says_an_ignored_file_is_not_a_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "[mesh]\nno_such_field = 1\n").unwrap();
        let (text, _) = report(std::slice::from_ref(&bad));
        assert!(
            text.contains("would start without that file's settings"),
            "{text}"
        );
        assert!(!text.contains("would refuse to start"), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn keygen_through_a_symlink_updates_the_file_it_points_to() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.toml");
        let link = dir.path().join("link.toml");
        std::fs::write(&real, "[mesh]\nenabled = true\n").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let written = write_issuer_key(&link, false).unwrap();
        assert_eq!(written, std::fs::canonicalize(&real).unwrap());
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        let (c, _) = Config::load_with_sources(std::slice::from_ref(&real));
        assert!(issuer_key_parses(&c.mesh.issuer_key));

        let dangling = dir.path().join("dangling.toml");
        std::os::unix::fs::symlink(dir.path().join("missing.toml"), &dangling).unwrap();
        let err = write_issuer_key(&dangling, false).unwrap_err().to_string();
        assert!(
            err.contains("symlink to a file that does not exist"),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn keygen_keeps_the_mode_and_a_new_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fresh = dir.path().join("sub").join("config.toml");
        write_issuer_key(&fresh, false).unwrap();
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(!readable_by_others(&fresh));

        let shared = dir.path().join("shared.toml");
        std::fs::write(&shared, "").unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_issuer_key(&shared, false).unwrap();
        let mode = std::fs::metadata(&shared).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
        assert!(readable_by_others(&shared));
    }
}
