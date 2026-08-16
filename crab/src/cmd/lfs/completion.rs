//! `crab lfs completion <shell>` — generate shell completion scripts.

use std::io::{ErrorKind, Write};
use std::process::ExitCode;

use clap::{Command, CommandFactory, Parser, Subcommand};
use clap_complete::generate;

use crate::core::error::Result;

/// Generate shell completions for the provided command tree.
pub fn run_lfs_completion(shell: &str, cmd: &mut Command) -> Result<ExitCode> {
    let shell = crate::cmd::completions::parse_shell(shell)?;
    let mut output = Vec::new();
    generate(shell, cmd, "crab", &mut output);
    if let Err(err) = std::io::stdout().write_all(&output) {
        if err.kind() == ErrorKind::BrokenPipe {
            return Ok(ExitCode::SUCCESS);
        }
        return Err(err.into());
    }
    Ok(ExitCode::SUCCESS)
}

/// Build a minimal `crab lfs` command tree for non-binary callers.
pub fn lfs_completion_command() -> Command {
    LfsCompletionRoot::command()
}

#[derive(Parser)]
#[command(name = "crab", version)]
struct LfsCompletionRoot {
    #[command(subcommand)]
    command: LfsCompletionTop,
}

#[derive(Subcommand)]
enum LfsCompletionTop {
    /// Git LFS compatibility commands.
    Lfs {
        #[command(subcommand)]
        command: super::LfsCmd,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generate_for(shell: &str) -> String {
        let mut command = lfs_completion_command();
        let shell = crate::cmd::completions::parse_shell(shell).unwrap();
        let mut output = Vec::new();
        generate(shell, &mut command, "crab", &mut output);
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn bash_completion_mentions_lfs_subcommands() {
        let script = generate_for("bash");

        assert!(script.contains("lfs"));
        assert!(script.contains("completion"));
    }

    #[test]
    fn invalid_shell_is_rejected() {
        let mut command = lfs_completion_command();
        let result = run_lfs_completion("nushell", &mut command);

        assert!(result.is_err());
    }
}
