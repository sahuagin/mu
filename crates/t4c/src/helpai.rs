//! Emitting conforming `--help-ai --json` — the PRODUCER side of the standard
//! (the consumer side is [`crate::source::HelpAiProbeSource`]).
//!
//! A clap-based tool gets a conforming document for free via [`from_clap`]; t4c
//! uses it on its own command, so t4c is a tool in its own registry (turtles).
//! See `docs/help-ai-standard.md` for the contract and `templates/help-ai.sh`
//! for the shell-tool equivalent.

use crate::capability::{HelpAiDoc, HelpAiSub};
use anyhow::Result;

/// Build a `--help-ai` document from a clap [`clap::Command`] by introspection:
/// the command's name + about, and one entry per (non-hidden) subcommand.
/// Keywords are left empty for clap to stay generic; a tool can enrich them.
pub fn from_clap(cmd: &clap::Command) -> HelpAiDoc {
    HelpAiDoc {
        name: cmd.get_name().to_string(),
        summary: cmd.get_about().map(|s| s.to_string()).unwrap_or_default(),
        keywords: Vec::new(),
        subcommands: cmd
            .get_subcommands()
            .filter(|s| !s.is_hide_set())
            .map(|s| HelpAiSub {
                name: s.get_name().to_string(),
                summary: s.get_about().map(|a| a.to_string()).unwrap_or_default(),
            })
            .collect(),
    }
}

/// Serialize a doc to the conforming `--help-ai --json` form.
pub fn to_json(doc: &HelpAiDoc) -> Result<String> {
    Ok(serde_json::to_string_pretty(doc)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Command;

    #[test]
    fn from_clap_captures_name_and_subcommands() {
        let cmd = Command::new("demo")
            .about("a demo tool")
            .subcommand(Command::new("go").about("do the thing"))
            .subcommand(Command::new("stop"));
        let doc = from_clap(&cmd);
        assert_eq!(doc.name, "demo");
        assert_eq!(doc.summary, "a demo tool");
        assert_eq!(doc.subcommands.len(), 2);
        assert_eq!(doc.subcommands[0].name, "go");
        assert_eq!(doc.subcommands[0].summary, "do the thing");
    }

    #[test]
    fn emitter_output_parses_in_the_consumer() {
        // The contract: what the emitter produces, the probe must be able to read.
        let cmd = Command::new("demo")
            .about("x")
            .subcommand(Command::new("y").about("z"));
        let json = to_json(&from_clap(&cmd)).unwrap();
        let back: HelpAiDoc = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "demo");
        assert_eq!(back.subcommands.len(), 1);
        assert_eq!(back.subcommands[0].name, "y");
    }
}
