//! `pydl completions <SHELL>` — emit a shell-completion script for the `pydl`
//! CLI to stdout. Delegates to `clap_complete`, which walks the already-built
//! `clap::Command` so every subcommand/flag is picked up automatically.

use std::io;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use clap_complete::{Shell, generate};

use crate::Cli;

#[derive(Parser, Debug)]
pub struct Args {
    /// Shell to generate completions for.
    pub shell: Shell,
}

// `args` by value and `Result<()>` match the shape of every other
// `cmd::*::run` so `main`'s dispatch table stays uniform. Neither clippy
// hint actually simplifies the call site.
#[allow(clippy::needless_pass_by_value, clippy::unnecessary_wraps)]
pub fn run(args: Args) -> Result<()> {
    let mut cmd = Cli::command();
    let bin_name = cmd.get_name().to_string();
    generate(args.shell, &mut cmd, bin_name, &mut io::stdout());
    Ok(())
}
