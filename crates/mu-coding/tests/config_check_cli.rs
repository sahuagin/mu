//! The config check through the real binary (bead mu-mu-check-config-8qm1k):
//! `mu serve` refuses a config it cannot run with, banner on stderr and exit
//! 78; `mu check-config` says the same without starting; `mu mesh keygen
//! --write` fixes it without printing the key.
//!
//! The operator config is pointed at a temp dir with `HOME` and
//! `XDG_CONFIG_HOME`. A host with a site config (`/etc/mu/config.toml`)
//! layers it in too, so these tests skip there rather than assert on it.

use std::path::Path;
use std::process::{Command, Output, Stdio};

const MU_BIN: &str = env!("CARGO_BIN_EXE_mu");

fn site_config_present() -> bool {
    if Path::new("/etc/mu/config.toml").exists() {
        eprintln!("skipping: /etc/mu/config.toml is layered into every load on this host");
        return true;
    }
    false
}

fn mu(home: &Path, args: &[&str]) -> Output {
    Command::new(MU_BIN)
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env_remove("MU_BEARER_TOKEN")
        .env_remove("MU_CONFIG_STRICT")
        .stdin(Stdio::null())
        .output()
        .expect("run mu")
}

fn write_config(home: &Path, text: &str) -> std::path::PathBuf {
    let dir = home.join(".config").join("mu");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, text).unwrap();
    path
}

#[test]
fn serve_refuses_a_mesh_without_an_issuer_key() {
    if site_config_present() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let path = write_config(home.path(), "[mesh]\nenabled = true\n");
    let out = mu(home.path(), &["serve"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(78), "{stderr}");
    assert!(out.stdout.is_empty(), "nothing on the JSON-RPC channel");
    assert!(
        stderr.contains("mu: CONFIG ERROR — [mesh].issuer_key is missing"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("mu mesh keygen --write {}", path.display())),
        "{stderr}"
    );
    assert!(
        stderr.contains("set serve = false under [mesh]"),
        "{stderr}"
    );
}

#[test]
fn check_config_then_keygen_then_clean() {
    if site_config_present() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let path = write_config(home.path(), "[mesh]\nenabled = true\n");

    let out = mu(home.path(), &["check-config"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(78), "{stdout}");
    assert!(
        stdout.contains(&format!("from {}", path.display())),
        "{stdout}"
    );
    assert!(stdout.contains("mu: CONFIG ERROR"), "{stdout}");

    let out = mu(
        home.path(),
        &["mesh", "keygen", "--write", path.to_str().unwrap()],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let printed =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    // The temp dir may sit behind a symlink, so the written path can be the
    // resolved one.
    assert!(
        String::from_utf8_lossy(&out.stdout).starts_with("wrote [mesh].issuer_key to "),
        "{printed}"
    );
    let written: toml::Value = std::fs::read_to_string(&path).unwrap().parse().unwrap();
    let key = written["mesh"]["issuer_key"].as_str().unwrap().to_string();
    assert!(!key.is_empty());
    assert!(!printed.contains(&key), "keygen must never print the key");

    let out = mu(home.path(), &["check-config"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(stdout.contains("no problems"), "{stdout}");
    assert!(
        !stdout.contains(&key),
        "check-config must never print the key"
    );
}
