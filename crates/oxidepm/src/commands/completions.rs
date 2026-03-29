//! Shell completion and man page generation

use anyhow::Result;
use clap::CommandFactory;
use clap_complete::{generate, Shell};
use std::io;

use crate::cli::Cli;

pub fn execute_completions(shell: &str) -> Result<()> {
    let shell: Shell = shell.parse().map_err(|_| {
        anyhow::anyhow!(
            "Unknown shell: '{}'. Supported: bash, zsh, fish, elvish, powershell",
            shell
        )
    })?;

    let mut cmd = Cli::command();
    generate(shell, &mut cmd, "oxidepm", &mut io::stdout());

    Ok(())
}

pub fn execute_manpage() -> Result<()> {
    let cmd = Cli::command();
    let man = clap_mangen::Man::new(cmd);
    man.render(&mut io::stdout())?;
    Ok(())
}
