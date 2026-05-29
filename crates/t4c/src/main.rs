//! `t4c` CLI entry point — a thin dispatch over the library's `cli` module
//! (the surface logic lives in the lib so it stays unit-testable).

use clap::Parser;

fn main() -> anyhow::Result<()> {
    let cli = t4c::cli::Cli::parse();
    let code = t4c::cli::run(cli)?;
    std::process::exit(code);
}
