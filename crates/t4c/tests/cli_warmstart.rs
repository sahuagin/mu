//! End-to-end CLI tests driving the real `t4c` binary (mu-2332).
//!
//! These lock in the warm-start contract that the unit tests can't reach,
//! because it spans process invocations and on-disk artifacts:
//!   - `discover` writes a snapshot + a SEPARATE self-config file;
//!   - a hand-authored override survives `discover` (the input/output split —
//!     the bug that motivated this test: discover used to clobber overrides);
//!   - a warm `find` sees both the catalog and the override.
//!
//! No extra deps: cargo injects `CARGO_BIN_EXE_t4c` for integration tests, and
//! we isolate every run in a temp dir via the `T4C_*` env overrides so the
//! suite never touches the developer's real `~/.cache` / `~/.config`.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A throwaway sandbox: a unique temp dir plus the four `T4C_*` paths that
/// redirect all of t4c's state into it. `Drop` cleans up.
struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "t4c-it-{tag}-{}-{}",
            std::process::id(),
            // nanosecond suffix so parallel tests don't collide
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Run the t4c binary with this sandbox's env, returning (stdout, status).
    fn run(&self, args: &[&str]) -> (String, bool) {
        let (out, _code, ok) = self.run_full(args, None);
        (out, ok)
    }

    /// Run and return the exact exit code (the machine contract `verify` relies
    /// on). `path_override` replaces `$PATH` for this invocation — used to
    /// simulate a host where some tools are absent, WITHOUT mutating the test
    /// process's own environment (subprocess-scoped, so no cross-test race).
    fn run_code(&self, args: &[&str], path_override: Option<&str>) -> (String, i32) {
        let (out, code, _ok) = self.run_full(args, path_override);
        (out, code)
    }

    fn run_full(&self, args: &[&str], path_override: Option<&str>) -> (String, i32, bool) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_t4c"));
        cmd.args(args)
            .env("T4C_SNAPSHOT", self.path("snapshot.rkyv"))
            .env("T4C_CONFIG", self.path("overrides.toml")) // override INPUT
            .env("T4C_SELF_CONFIG", self.path("registry.toml")) // discover OUTPUT
            .env("T4C_VECTORS", self.path("vectors.json"))
            // No embedder configured for the sandbox: find stays lexical, which
            // is deterministic and network-free — exactly what CI needs.
            .env_remove("T4C_EMBED_MODEL");
        if let Some(p) = path_override {
            cmd.env("PATH", p);
        }
        let out = cmd.output().expect("spawn t4c");
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            out.status.code().unwrap_or(-1),
            out.status.success(),
        )
    }

    /// Make a dir of FAKE executable stubs for the given command names — each a
    /// trivial `#!/bin/sh\nexit 0` script, chmod +x. Unlike `tooldir`, presence
    /// is deterministic and HOST-INDEPENDENT: it does not depend on `jj`/`gh`/etc
    /// being installed on the test runner (the bug reviewer #2 caught — tooldir
    /// silently skips absent tools, so a degraded test could validate nothing on
    /// a bare CI host). t4c's presence probe is `which` (file exists on PATH),
    /// which these satisfy. Returns the dir path.
    fn fake_tooldir(&self, name: &str, cmds: &[&str]) -> String {
        let dir = self.path(name);
        std::fs::create_dir_all(&dir).unwrap();
        for c in cmds {
            let p = dir.join(c);
            std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
            let mut perms = std::fs::metadata(&p).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
            std::fs::set_permissions(&p, perms).unwrap();
        }
        dir.display().to_string()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn exists(p: &Path) -> bool {
    p.exists()
}

#[test]
fn discover_writes_snapshot_and_separate_self_config() {
    let sb = Sandbox::new("disc");
    let (out, ok) = sb.run(&["discover"]);
    assert!(ok, "discover failed: {out}");

    // Snapshot written.
    assert!(
        exists(&sb.path("snapshot.rkyv")),
        "discover should write the rkyv snapshot"
    );
    // Self-config output written to the OUTPUT path...
    assert!(
        exists(&sb.path("registry.toml")),
        "discover should write the self-config registry"
    );
    // ...and NOT to the override-input path (no override authored here).
    assert!(
        !exists(&sb.path("overrides.toml")),
        "discover must not write the override-input file"
    );
}

#[test]
fn override_survives_discover_and_shows_up_in_find() {
    let sb = Sandbox::new("override");

    // Hand-author an override for a tool that's actually installed (`sh`).
    std::fs::write(
        sb.path("overrides.toml"),
        r#"[[capability]]
path = "bash.itshell"
summary = "POSIX shell, hand-authored override that must survive discover"
invoke = ["sh"]
keywords = ["shell", "posix"]
"#,
    )
    .unwrap();

    // The discover that used to clobber the override.
    let (_o, ok) = sb.run(&["discover"]);
    assert!(ok);

    // The override file is intact.
    let after = std::fs::read_to_string(sb.path("overrides.toml")).unwrap();
    assert!(
        after.contains("bash.itshell"),
        "override must survive discover, got:\n{after}"
    );

    // A second discover still doesn't clobber it (the bug was a clobber-on-run).
    let (_o2, ok2) = sb.run(&["discover"]);
    assert!(ok2);
    assert!(std::fs::read_to_string(sb.path("overrides.toml"))
        .unwrap()
        .contains("bash.itshell"));

    // And a warm find surfaces the override (re-layered on the snapshot).
    let (find_out, ok3) = sb.run(&["find", "posix shell hand authored override"]);
    assert!(ok3);
    assert!(
        find_out.contains("bash.itshell"),
        "warm find should surface the override; got:\n{find_out}"
    );
}

#[test]
fn warm_find_returns_catalog_hits() {
    let sb = Sandbox::new("warm");
    // Cold discover builds the snapshot.
    assert!(sb.run(&["discover"]).1);
    // Warm find (snapshot present) returns ranked hits in JSON.
    let (out, ok) = sb.run(&["--json", "find", "version control status"]);
    assert!(ok);
    assert!(
        out.contains("\"hits\""),
        "find --json should emit a hits array; got:\n{out}"
    );
}

#[test]
fn find_self_heals_when_no_snapshot() {
    let sb = Sandbox::new("heal");
    // No discover first => no snapshot => find must cold-build, not error.
    let (out, ok) = sb.run(&["--json", "find", "search files for text"]);
    assert!(
        ok,
        "find without a snapshot must self-heal (cold build), not fail"
    );
    assert!(out.contains("\"hits\""));
}

// --- mu-2332: `verify` exit-code ladder (the machine contract) ---------------
// verify's exit code is the cheap gate a caller checks before parsing anything:
//   0 fresh · 1 stale-equivalent (lossless) · 2 stale-degraded (category lost)
//   · 3 missing. These tests pin that ladder. They're written to be HOST-
// PORTABLE: the equivalent/degraded split is judged on capability PATHS, not
// tool identity, so they pass on the FreeBSD dev box and the Ubuntu CI runner
// alike — we construct the PATH explicitly via tooldir() rather than trusting
// the ambient one.

#[test]
fn verify_missing_snapshot_exits_3() {
    let sb = Sandbox::new("vmiss");
    let (out, code) = sb.run_code(&["verify"], None);
    assert_eq!(code, 3, "no snapshot => missing => exit 3; got:\n{out}");
    let (jout, jcode) = sb.run_code(&["--json", "verify"], None);
    assert_eq!(jcode, 3);
    assert!(jout.contains("\"missing\""));
}

#[test]
fn verify_fresh_after_discover_exits_0() {
    let sb = Sandbox::new("vfresh");
    // Discover and verify under the SAME PATH => fresh.
    let tools = sb.fake_tooldir("p", &["jj", "gh", "rg", "fd", "jq", "git", "eza", "sh"]);
    assert_eq!(sb.run_code(&["discover"], Some(&tools)).1, 0);
    let (out, code) = sb.run_code(&["verify"], Some(&tools));
    assert_eq!(code, 0, "same world => fresh => exit 0; got:\n{out}");
    assert!(out.contains("fresh"));
}

#[test]
fn verify_degraded_when_a_catalog_tool_vanishes_exits_2() {
    let sb = Sandbox::new("vdeg");
    // Discover with FAKE jj + gh stubs guaranteed present (host-independent:
    // does NOT depend on real jj/gh being installed on the CI runner — the
    // portability bug reviewer #2 caught). The catalog's bash.jj.status /
    // bash.gh.pr resolve to these stubs.
    let full = sb.fake_tooldir("full", &["jj", "gh"]);
    assert_eq!(sb.run_code(&["discover"], Some(&full)).1, 0);
    // ...then verify under a PATH where jj + gh are GONE (empty tool dir).
    let partial = sb.fake_tooldir("partial", &[]);
    let (out, code) = sb.run_code(&["verify", "--diff"], Some(&partial));
    assert_eq!(
        code, 2,
        "a catalogued tool's binary vanished => degraded => exit 2; got:\n{out}"
    );
    // --json names the lost categories.
    let (jout, jcode) = sb.run_code(&["--json", "verify"], Some(&partial));
    assert_eq!(jcode, 2);
    assert!(
        jout.contains("stale-degraded"),
        "verdict should be stale-degraded; got:\n{jout}"
    );
    assert!(
        jout.contains("bash.jj.status") && jout.contains("bash.gh.pr"),
        "dropped[] should name jj and gh capabilities; got:\n{jout}"
    );
}

#[test]
fn verify_degraded_when_tool_deleted_from_unchanged_path_dir() {
    // The invariant hole both reviewers flagged: a tool removed from a PATH dir
    // that STAYS on PATH (path_set_hash unchanged) must still invalidate. The
    // tool_fingerprint (stat of the depended-on binaries) is what catches this.
    let sb = Sandbox::new("vdelinplace");
    let dir = sb.fake_tooldir("bin", &["jj", "gh"]);
    assert_eq!(sb.run_code(&["discover"], Some(&dir)).1, 0);
    assert_eq!(
        sb.run_code(&["verify"], Some(&dir)).1,
        0,
        "fresh right after discover"
    );

    // Delete jj's binary but keep the SAME PATH dir (path unchanged).
    std::fs::remove_file(format!("{dir}/jj")).unwrap();
    let (out, code) = sb.run_code(&["verify"], Some(&dir));
    assert_eq!(
        code, 2,
        "tool deleted from unchanged PATH dir must go stale-degraded; got:\n{out}"
    );
    assert!(
        out.contains("tool binary changed") || out.contains("degraded"),
        "verdict should cite the tool-binary change; got:\n{out}"
    );
}

#[test]
fn verify_equivalent_when_only_override_edited_exits_1() {
    let sb = Sandbox::new("vequiv");
    let tools = sb.fake_tooldir("p", &["jj", "gh", "rg", "fd", "jq", "git", "eza", "sh"]);
    // Discover with an override present (override is hashed into catalog_hash).
    std::fs::write(
        sb.path("overrides.toml"),
        "[[capability]]\npath = \"bash.ovr\"\nsummary = \"o\"\ninvoke = [\"sh\"]\nkeywords = [\"o\"]\n",
    )
    .unwrap();
    assert_eq!(sb.run_code(&["discover"], Some(&tools)).1, 0);
    // Editing the override changes catalog_hash => stale, but every PROBED
    // capability still resolves under the same PATH => equivalent (lossless).
    std::fs::write(
        sb.path("overrides.toml"),
        "[[capability]]\npath = \"bash.ovr\"\nsummary = \"o EDITED\"\ninvoke = [\"sh\"]\nkeywords = [\"o\"]\n",
    )
    .unwrap();
    let (out, code) = sb.run_code(&["verify"], Some(&tools));
    assert_eq!(
        code, 1,
        "override edit => stale but lossless => exit 1; got:\n{out}"
    );
    assert!(out.contains("equivalent"));
}

#[test]
fn verify_writes_nothing() {
    // The whole point of verify: it's a non-destructive terrain check. Confirm
    // it never creates the snapshot/config (unlike discover).
    let sb = Sandbox::new("vnowrite");
    let _ = sb.run_code(&["verify"], None);
    assert!(
        !exists(&sb.path("snapshot.rkyv")),
        "verify must not create a snapshot"
    );
    assert!(
        !exists(&sb.path("registry.toml")),
        "verify must not write the self-config"
    );
}

#[test]
fn verify_survives_malformed_override() {
    // Finding (gpt-5.5 panel): verify must terrain-check and return a freshness
    // exit code even if $T4C_CONFIG is malformed — it dispatches BEFORE building
    // the registry tree, so a broken override TOML can't make it error out.
    let sb = Sandbox::new("vbadcfg");
    let tools = sb.fake_tooldir("p", &["jj", "gh", "rg", "fd", "jq", "git", "eza", "sh"]);
    // A snapshot must exist for a freshness verdict (else 'missing' is also fine,
    // but we want to prove the malformed override doesn't crash the path).
    assert_eq!(sb.run_code(&["discover"], Some(&tools)).1, 0);
    // Now corrupt the override file.
    std::fs::write(sb.path("overrides.toml"), "this is not valid toml ][ {{").unwrap();
    let (out, code) = sb.run_code(&["verify"], Some(&tools));
    // It must NOT panic / NOT return a generic error exit; it returns a real
    // verify verdict code (0..=3). (Malformed override changes catalog_hash, so
    // realistically stale-equivalent=1; the key assertion is "a verdict code".)
    assert!(
        (0..=3).contains(&code),
        "verify must return a freshness verdict code despite malformed override; got {code}, out:\n{out}"
    );
    assert!(
        out.contains("verify:"),
        "verify should still print its verdict line; got:\n{out}"
    );
}

#[test]
fn find_survives_malformed_override() {
    // A user's typo in $T4C_CONFIG must not brick `find` (the hot path). The
    // override layer is best-effort: warn to stderr, skip it, still resolve
    // catalog + chains. (Companion to verify_survives_malformed_override —
    // same fail-soft-but-visible posture applied to the registry-building path.)
    let sb = Sandbox::new("findbadcfg");
    let tools = sb.fake_tooldir("p", &["jj", "gh", "rg", "fd", "jq", "git", "eza", "sh"]);
    assert_eq!(sb.run_code(&["discover"], Some(&tools)).1, 0);
    std::fs::write(sb.path("overrides.toml"), "garbage ][ {{ not toml").unwrap();
    let (out, code) = sb.run_code(&["--json", "find", "version control"], Some(&tools));
    assert_eq!(
        code, 0,
        "find must not fail on a malformed override; out:\n{out}"
    );
    assert!(
        out.contains("\"hits\""),
        "find should still return hits from catalog+chains; got:\n{out}"
    );
}

#[test]
fn verify_augmentable_when_tool_installed_into_unchanged_path_dir() {
    // Reviewer #2 (round 3 split): the SYMMETRIC gap to delete-in-place — a
    // catalogued tool ABSENT at discover, later installed into an existing PATH
    // dir. path_set_hash unchanged, and it wasn't in the present-set to be
    // tool-fingerprinted, so the load is Fresh. verify must still surface it
    // (recomputes the live present-set) as stale-augmentable: nothing lost,
    // rediscover only gains. This lives in verify (opt-in check), NOT on the
    // warm find path (which stays zero-probe) — the adjudicated boundary.
    let sb = Sandbox::new("vaug");
    let dir = sb.fake_tooldir("bin", &["jj"]); // jq deliberately absent at discover
    assert_eq!(sb.run_code(&["discover"], Some(&dir)).1, 0);
    assert_eq!(
        sb.run_code(&["verify"], Some(&dir)).1,
        0,
        "fresh: jq not yet installed"
    );

    // Install jq into the SAME dir (path-set unchanged).
    let jq = format!("{dir}/jq");
    std::fs::write(&jq, "#!/bin/sh\nexit 0\n").unwrap();
    let mut perms = std::fs::metadata(&jq).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&jq, perms).unwrap();

    let (out, code) = sb.run_code(&["--json", "verify"], Some(&dir));
    assert_eq!(
        code, 1,
        "newly-installed tool => augmentable => exit 1; got:\n{out}"
    );
    assert!(
        out.contains("stale-augmentable"),
        "verdict should be stale-augmentable; got:\n{out}"
    );
    assert!(
        out.contains("bash.jq"),
        "only_live should name the newly-installed tool; got:\n{out}"
    );
}
