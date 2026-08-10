//! `--check-config`: resolve the whole configuration, say where every value
//! came from, and exit — without starting the server or binding the port.
//!
//! Written after a deploy where the config was correct for hours and the only
//! way to find out was to restart the service and read the log (mu-qqv0). The
//! failure that cost the most was invisible rather than complicated: a stale
//! `nats_url` inside `[dialogue.mesh]` silently overriding a correct `[mesh]`,
//! with nothing anywhere announcing the shadowing. So provenance, not just
//! values, is the point of this command.

use std::path::{Path, PathBuf};

use crate::{mesh, presence};

/// One place a value is set.
struct Origin {
    value: String,
    /// Human-readable "`[section].field` in <path>".
    where_: String,
}

fn read_toml(path: &Path) -> Option<toml::Value> {
    std::fs::read_to_string(path).ok()?.parse().ok()
}

/// Every place `field` is set, in the SAME precedence order the loader uses.
/// First entry wins; the rest are shadowed and worth reporting as such.
fn origins(cfg: &Path, fleet: &Path, field: &str) -> Vec<Origin> {
    let mut found = Vec::new();
    let mut probe = |path: &Path, table: &[&str]| {
        let Some(root) = read_toml(path) else { return };
        let mut node = &root;
        for k in table {
            match node.get(k) {
                Some(n) => node = n,
                None => return,
            }
        }
        if let Some(v) = node.get(field).and_then(toml::Value::as_str) {
            if !v.is_empty() {
                found.push(Origin {
                    value: v.to_string(),
                    where_: format!("[{}].{field} in {}", table.join("."), path.display()),
                });
            }
        }
    };
    // Mirrors mesh::load: the dialogue section's own value first, then [mesh]
    // in that same file, then [mesh] in the fallback file.
    probe(cfg, &["dialogue", "mesh"]);
    probe(cfg, &["mesh"]);
    if fleet != cfg {
        probe(fleet, &["mesh"]);
    }
    found
}

fn redact(field: &str, value: &str) -> String {
    if field.contains("key") && value.len() > 12 {
        format!(
            "{}…{} ({} chars)",
            &value[..6],
            &value[value.len() - 4..],
            value.len()
        )
    } else {
        value.to_string()
    }
}

fn show_field(cfg: &Path, fleet: &Path, field: &str, effective: &str) -> bool {
    let found = origins(cfg, fleet, field);
    let mut ok = true;
    match found.first() {
        None => {
            println!("      {field:<11} (unset)");
            ok = false;
        }
        Some(win) => {
            println!("      {field:<11} = {}", redact(field, &win.value));
            println!("      {:<11}   from {}", "", win.where_);
            // The thing that cost a day: a value set in two places, the more
            // specific one silently winning while the other looks authoritative.
            for shadowed in found.iter().skip(1) {
                let same = shadowed.value == win.value;
                println!(
                    "      {:<11}   {} also set in {}{}",
                    "",
                    if same { "note:" } else { "SHADOWED:" },
                    shadowed.where_,
                    if same {
                        " (same value)".to_string()
                    } else {
                        format!(" = {} — IGNORED", redact(field, &shadowed.value))
                    }
                );
            }
            // Self-check: if the loader disagrees with this reconstruction, the
            // checker is lying, which is worse than not having one.
            if !effective.is_empty() && effective != win.value {
                println!(
                    "      {:<11}   !! loader resolved {} — this report is WRONG, file a bug",
                    "",
                    redact(field, effective)
                );
                ok = false;
            }
        }
    }
    ok
}

fn source_of(args: &[String], flag: &str, envs: &[&str]) -> String {
    if args
        .iter()
        .any(|a| a == flag || a.starts_with(&format!("{flag}=")))
    {
        return format!("{flag} (command line)");
    }
    for e in envs {
        if std::env::var(e).map(|v| !v.is_empty()).unwrap_or(false) {
            return format!("${e}");
        }
    }
    "default".to_string()
}

/// Print the resolved configuration. Returns the process exit code: non-zero
/// only when something is BROKEN (a feature enabled but unusable), not merely
/// disabled — so a deploy script can gate on it.
pub fn run(args: &[String]) -> i32 {
    let agent = presence::config_path_for_key(&["__none__"]); // resolves to the fallback
    let home_agent = agent.parent().and_then(Path::parent).map(|p| {
        let mut p = PathBuf::from(p);
        p.push("agent/config.toml");
        p
    });
    let mut broken = false;

    println!("mu-dialogue --check-config");
    println!();
    println!("  config files");
    if let Some(a) = &home_agent {
        println!(
            "    agent  {}{}",
            a.display(),
            if a.exists() { "" } else { "   (absent)" }
        );
    }
    println!(
        "    mu     {}{}",
        agent.display(),
        if agent.exists() { "" } else { "   (absent)" }
    );
    if let Ok(v) = std::env::var("MU_CONFIG") {
        println!("    NOTE   $MU_CONFIG={v} overrides BOTH for every section");
    }
    println!();

    // ── presence ─────────────────────────────────────────────────────────
    let p_path = presence::config_path_for("presence");
    println!("  [dialogue.presence]   resolved from {}", p_path.display());
    match presence::load(&p_path) {
        Some(cfg) => {
            println!("      enabled     = true");
            println!("      etcd        = {:?}", cfg.etcd);
            println!("      prefix      = {}", cfg.prefix);
        }
        None => println!("      disabled (absent, enabled = false, or no endpoints)"),
    }
    println!();

    // ── mesh gateway ─────────────────────────────────────────────────────
    let m_path = presence::config_path_for("mesh");
    let f_path = presence::config_path_for_key(&["mesh"]);
    println!("  [dialogue.mesh]       resolved from {}", m_path.display());
    if f_path != m_path {
        println!("      inherits [mesh] from {}", f_path.display());
    }
    match mesh::load(&m_path, &f_path) {
        Ok(cfg) => {
            println!("      enabled     = true");
            show_field(&m_path, &f_path, "nats_url", &cfg.nats_url);
            show_field(&m_path, &f_path, "issuer_key", &cfg.issuer_key);
            println!();
            println!(
                "      => gateway WOULD START and connect to {}",
                cfg.nats_url
            );
            println!(
                "         verify the broker is reachable FROM THIS HOST first:\n\
                 \x20           nc -z {} || sockstat -46l | grep 4222",
                cfg.nats_url.replace(':', " ")
            );
        }
        Err(why) => {
            println!("      => gateway OFF — {why}");
            // Enabled-but-unusable is a broken config; simply absent is not.
            if !why.contains("no [dialogue.mesh] section") && !why.contains("enabled = false") {
                broken = true;
                show_field(&m_path, &f_path, "nats_url", "");
                show_field(&m_path, &f_path, "issuer_key", "");
            }
        }
    }
    println!();

    // ── runtime settings (not from the TOML files) ───────────────────────
    println!("  runtime");
    println!(
        "    listen        {}",
        source_of(args, "--listen", &["LISTEN", "MU_DIALOGUE_ADDR"])
    );
    println!("    database      ${{DATABASE_PATH}} or ~/.local/share/agent.sqlite");
    println!(
        "    peer ttl      {}",
        source_of(args, "--peer-ttl-ms", &["MU_DIALOGUE_PEER_TTL_MS"])
    );
    println!(
        "    allowed hosts {}",
        source_of(args, "--allow-host", &["MU_DIALOGUE_ALLOWED_HOSTS"])
    );
    println!();
    println!(
        "  {}",
        if broken {
            "VERDICT: something is enabled but unusable — see above (exit 1)"
        } else {
            "VERDICT: coherent (exit 0). Features may still be off by choice."
        }
    );
    i32::from(broken)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, body: &str) -> PathBuf {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
        p
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mu-dlg-check-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The failure this command exists for: a value set in two places, the more
    /// specific one silently winning. `origins` must list BOTH, winner first.
    #[test]
    fn shadowed_values_are_reported_winner_first() {
        let d = tmp("shadow");
        let cfg = write(
            &d,
            "agent.toml",
            "[dialogue.mesh]\nenabled = true\nnats_url = \"stale:4222\"\n\
             [mesh]\nnats_url = \"correct:4222\"\n",
        );
        let found = origins(&cfg, &cfg, "nats_url");
        assert_eq!(found.len(), 2, "both settings must be reported");
        assert_eq!(
            found[0].value, "stale:4222",
            "the winner is the specific one"
        );
        assert!(found[0].where_.contains("[dialogue.mesh].nats_url"));
        assert_eq!(found[1].value, "correct:4222");
        assert!(found[1].where_.contains("[mesh].nats_url"));
    }

    /// Precedence here must match `mesh::load`, or the report is a lie. Asserts
    /// the reconstruction and the loader agree on the effective value.
    #[test]
    fn reported_winner_matches_what_the_loader_resolves() {
        let d = tmp("agree");
        let cfg = write(
            &d,
            "agent.toml",
            "[dialogue.mesh]\nenabled = true\nnats_url = \"specific:4222\"\n",
        );
        let fleet = write(
            &d,
            "mu.toml",
            "[mesh]\nnats_url = \"fleet:4222\"\nissuer_key = \"beef\"\n",
        );
        let loaded = crate::mesh::load(&cfg, &fleet).expect("loads");
        let found = origins(&cfg, &fleet, "nats_url");
        assert_eq!(found[0].value, loaded.nats_url);
        // The fleet value is still reported as shadowed, from the other file.
        assert_eq!(found[1].value, "fleet:4222");
        assert!(found[1].where_.contains("mu.toml"));
        // issuer_key is only in the fleet file, so it wins there uncontested.
        let keys = origins(&cfg, &fleet, "issuer_key");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].value, loaded.issuer_key);
    }

    /// Secrets must not be printed in full by a command people paste into
    /// issues and chat logs.
    #[test]
    fn issuer_key_is_redacted_but_identifiable() {
        let full = "1fcc4a241dca71b372d5430e5dc522811c435897c7bb1de3b602b0e2bf523437";
        let shown = redact("issuer_key", full);
        assert!(!shown.contains(full), "must not print the whole key");
        assert!(shown.starts_with("1fcc4a"), "enough to tell two keys apart");
        assert!(shown.contains("64 chars"));
        // Non-secret fields are shown verbatim.
        assert_eq!(redact("nats_url", "10.1.1.172:4222"), "10.1.1.172:4222");
    }

    #[test]
    fn an_unset_field_reports_no_origins() {
        let d = tmp("unset");
        let cfg = write(&d, "a.toml", "[dialogue.mesh]\nenabled = true\n");
        assert!(origins(&cfg, &cfg, "nats_url").is_empty());
    }
}
