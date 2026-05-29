//! `t4c` CLI entry point.
//!
//! This scaffold establishes the binary and its argument parser so later beads
//! only *add* to a working surface. The real subcommands — `find` (semantic
//! front door), the terse path-tree walk, `--help-ai`/`--schema`/`--json`
//! meta-flags, and `discover` — land in mu-kex4.3 / mu-kex4.4.

use clap::Parser;

/// tools4claude — find, learn, and invoke tools by intent.
#[derive(Parser, Debug)]
#[command(name = "t4c", version, about)]
struct Cli {
    // Subcommand surface arrives in mu-kex4.3.
}

fn main() -> anyhow::Result<()> {
    let _cli = Cli::parse();
    println!(
        "t4c {} — discovery surface for agents. Subcommand surface lands in mu-kex4.3; \
         run `t4c --help`.",
        t4c::version()
    );
    Ok(())
}
