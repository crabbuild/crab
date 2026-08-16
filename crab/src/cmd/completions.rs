//! `crab completions <shell>` — generate shell completion scripts.
//!
//! Uses `clap_complete` to produce accurate completions from the clap
//! derive definitions. Supports bash, zsh, fish, and PowerShell.

use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Command;
use clap_complete::{Shell, generate};

use crate::core::error::{CrabError, Result};

/// Generate shell completions and either print to stdout or install to
/// the shell-specific directory.
pub fn run_completions(cmd: &mut Command, shell: &str, install: bool) -> Result<()> {
    let shell_variant = parse_shell(shell)?;

    if install {
        let path = install_path(shell_variant)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::File::create(&path)?;
        generate(shell_variant, cmd, "crab", &mut file);
        file.flush()?;
        eprintln!("Completion script written to: {}", path.display());
    } else {
        generate(shell_variant, cmd, "crab", &mut std::io::stdout());
    }
    Ok(())
}

/// Parse a shell name string into a `clap_complete::Shell` variant.
pub fn parse_shell(s: &str) -> Result<Shell> {
    match s.to_lowercase().as_str() {
        "bash" => Ok(Shell::Bash),
        "zsh" => Ok(Shell::Zsh),
        "fish" => Ok(Shell::Fish),
        "powershell" | "pwsh" => Ok(Shell::PowerShell),
        _ => Err(CrabError::UnsupportedShell {
            shell: s.to_owned(),
        }),
    }
}

/// Resolve the install path for a given shell.
fn install_path(shell: Shell) -> Result<PathBuf> {
    let home = home_dir()?;
    let path = match shell {
        Shell::Bash => home.join(".bash_completion.d/crab"),
        Shell::Zsh => home.join(".zfunc/_crab"),
        Shell::Fish => home.join(".config/fish/completions/crab.fish"),
        Shell::PowerShell => powershell_profile_dir(&home),
        _ => {
            return Err(CrabError::UnsupportedShell {
                shell: format!("{shell:?}"),
            });
        }
    };
    Ok(path)
}

/// Resolve the user's home directory.
fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| CrabError::Configuration {
            key: "HOME".into(),
            origin: "could not determine home directory for completion install".into(),
        })
}

/// Resolve the PowerShell completions directory.
fn powershell_profile_dir(home: &Path) -> PathBuf {
    // On Windows, PowerShell modules live under Documents/PowerShell.
    // On Unix, use ~/.config/powershell.
    if cfg!(windows) {
        home.join("Documents/PowerShell/Completions/crab.ps1")
    } else {
        home.join(".config/powershell/completions/crab.ps1")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_shell_valid_names() {
        assert_eq!(parse_shell("bash").unwrap(), Shell::Bash);
        assert_eq!(parse_shell("zsh").unwrap(), Shell::Zsh);
        assert_eq!(parse_shell("fish").unwrap(), Shell::Fish);
        assert_eq!(parse_shell("powershell").unwrap(), Shell::PowerShell);
        assert_eq!(parse_shell("pwsh").unwrap(), Shell::PowerShell);
    }

    #[test]
    fn parse_shell_case_insensitive() {
        assert_eq!(parse_shell("BASH").unwrap(), Shell::Bash);
        assert_eq!(parse_shell("Zsh").unwrap(), Shell::Zsh);
        assert_eq!(parse_shell("FISH").unwrap(), Shell::Fish);
        assert_eq!(parse_shell("PowerShell").unwrap(), Shell::PowerShell);
        assert_eq!(parse_shell("PWSH").unwrap(), Shell::PowerShell);
    }

    #[test]
    fn parse_shell_invalid_returns_error() {
        let err = parse_shell("nushell").unwrap_err();
        assert!(matches!(err, CrabError::UnsupportedShell { .. }));
        let msg = err.to_string();
        assert!(msg.contains("nushell"));
        assert!(msg.contains("bash"));
        assert!(msg.contains("zsh"));
        assert!(msg.contains("fish"));
        assert!(msg.contains("powershell"));
    }

    #[test]
    fn parse_shell_empty_string_returns_error() {
        let err = parse_shell("").unwrap_err();
        assert!(matches!(err, CrabError::UnsupportedShell { .. }));
    }
}
