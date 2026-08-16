//! `crab lfs clone` — Git LFS-compatible clone wrapper.

use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};

use crate::core::error::{CrabError, Result};

#[derive(Debug, Clone, Default)]
pub struct LfsCloneOptions {
    pub args: Vec<String>,
    pub include: Option<String>,
    pub exclude: Option<String>,
    pub skip_repo: bool,
}

pub fn run_lfs_clone(options: LfsCloneOptions) -> Result<ExitCode> {
    if clone_positionals(&options.args).is_empty() {
        return Err(CrabError::Configuration {
            key: "crab lfs clone".to_owned(),
            origin: "expected repository argument".to_owned(),
        });
    }

    eprintln!(
        "WARNING: `crab lfs clone` is deprecated; prefer `git clone` or `crab clone` when possible."
    );

    let cwd = std::env::current_dir()?;
    run_git_clone_without_lfs(&options.args, &cwd)?;

    let cloned_dir = cloned_dir_from_args(&options.args, &cwd)?;
    let _guard = CurrentDirGuard::push(&cloned_dir)?;

    if has_lfs_pointers_in_head() {
        if clone_fetch_only(&options.args) {
            super::fetch::run_lfs_fetch(super::fetch::LfsFetchOptions {
                include: options.include.clone(),
                exclude: options.exclude.clone(),
                ..super::fetch::LfsFetchOptions::default()
            })?;
        } else {
            super::fetch::run_lfs_pull(super::fetch::LfsPullOptions {
                include: options.include.clone(),
                exclude: options.exclude.clone(),
                ..super::fetch::LfsPullOptions::default()
            })?;
        }
    }

    if !options.skip_repo {
        super::install::run_lfs_install_in(
            Path::new("."),
            super::install::LfsInstallOptions {
                local: true,
                ..super::install::LfsInstallOptions::default()
            },
        )?;
    }

    Ok(ExitCode::SUCCESS)
}

fn run_git_clone_without_lfs(args: &[String], cwd: &Path) -> Result<()> {
    let mut cmd = std::process::Command::new("git");
    cmd.args([
        "-c",
        "filter.lfs.smudge=",
        "-c",
        "filter.lfs.clean=",
        "-c",
        "filter.lfs.process=",
        "-c",
        "filter.lfs.required=false",
        "clone",
    ]);
    cmd.args(args);
    cmd.current_dir(cwd);
    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());

    let status = cmd.status().map_err(|source| CrabError::Configuration {
        key: "git clone".to_owned(),
        origin: format!("failed to run git clone: {source}"),
    })?;

    if !status.success() {
        return Err(CrabError::Protocol(format!(
            "git clone exited with status {}",
            status.code().unwrap_or(-1)
        )));
    }

    Ok(())
}

fn cloned_dir_from_args(args: &[String], cwd: &Path) -> Result<PathBuf> {
    let positionals = clone_positionals(args);
    if positionals.len() >= 2 {
        return Ok(cwd.join(positionals[positionals.len() - 1]));
    }

    let Some(url) = positionals.first() else {
        return Err(CrabError::Configuration {
            key: "crab lfs clone".to_owned(),
            origin: "expected repository argument".to_owned(),
        });
    };
    let name = repo_name_from_url(url).ok_or_else(|| CrabError::Configuration {
        key: "crab lfs clone".to_owned(),
        origin: format!("could not derive clone directory from {url:?}"),
    })?;
    Ok(cwd.join(name))
}

fn clone_positionals(args: &[String]) -> Vec<&str> {
    let mut positionals = Vec::new();
    let mut iter = args.iter().map(String::as_str).peekable();
    let mut literal_args = false;

    while let Some(arg) = iter.next() {
        if literal_args {
            positionals.push(arg);
            continue;
        }

        if arg == "--" {
            literal_args = true;
            continue;
        }

        if let Some(long) = arg.strip_prefix("--") {
            if clone_long_option_takes_value(long) && !long.contains('=') {
                let _ = iter.next();
            }
            continue;
        }

        if let Some(shorts) = arg.strip_prefix('-')
            && !shorts.is_empty()
        {
            if clone_short_option_takes_value(shorts) && shorts.len() == 1 {
                let _ = iter.next();
            }
            continue;
        }

        positionals.push(arg);
    }

    positionals
}

fn clone_fetch_only(args: &[String]) -> bool {
    args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--bare" | "--mirror" | "--no-checkout" | "--no-checkout=true" | "-n"
        )
    })
}

fn clone_long_option_takes_value(option: &str) -> bool {
    let name = option.split_once('=').map_or(option, |(name, _)| name);
    matches!(
        name,
        "template"
            | "origin"
            | "branch"
            | "upload-pack"
            | "reference"
            | "reference-if-able"
            | "separate-git-dir"
            | "depth"
            | "config"
            | "shallow-since"
            | "shallow-exclude"
            | "jobs"
            | "server-option"
            | "filter"
            | "bundle-uri"
    )
}

fn clone_short_option_takes_value(shorts: &str) -> bool {
    matches!(
        shorts.as_bytes().first(),
        Some(b'o' | b'b' | b'u' | b'c' | b'j')
    )
}

fn repo_name_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim_end_matches('/');
    let mut parts = trimmed.rsplit(['/', ':']);
    let raw_name = parts.next()?.trim_end_matches(".git");
    if raw_name.is_empty() || raw_name == "." || raw_name == ".." {
        return None;
    }
    Some(raw_name.to_owned())
}

fn has_lfs_pointers_in_head() -> bool {
    std::process::Command::new("git")
        .args([
            "grep",
            "-I",
            "-l",
            "-e",
            "version https://git-lfs.github.com/spec/v1",
            "HEAD",
            "--",
            ".",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

struct CurrentDirGuard {
    previous: PathBuf,
}

impl CurrentDirGuard {
    fn push(path: &Path) -> Result<Self> {
        let previous = std::env::current_dir()?;
        std::env::set_current_dir(path).map_err(|source| CrabError::Configuration {
            key: "crab lfs clone".to_owned(),
            origin: format!(
                "failed to enter clone directory {}: {source}",
                path.display()
            ),
        })?;
        Ok(Self { previous })
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.previous);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_positionals_skip_known_git_clone_options() {
        let args = vec![
            "--branch".to_owned(),
            "main".to_owned(),
            "--depth=1".to_owned(),
            "-c".to_owned(),
            "protocol.file.allow=always".to_owned(),
            "https://example.com/repo.git".to_owned(),
            "dst".to_owned(),
        ];

        assert_eq!(
            clone_positionals(&args),
            vec!["https://example.com/repo.git", "dst"]
        );
    }

    #[test]
    fn cloned_dir_uses_explicit_directory() {
        let args = vec![
            "https://example.com/repo.git".to_owned(),
            "custom".to_owned(),
        ];

        assert_eq!(
            cloned_dir_from_args(&args, Path::new("/tmp")).unwrap(),
            Path::new("/tmp/custom")
        );
    }

    #[test]
    fn cloned_dir_derives_from_repository_url() {
        let args = vec!["git@example.com:org/repo.git".to_owned()];

        assert_eq!(
            cloned_dir_from_args(&args, Path::new("/tmp")).unwrap(),
            Path::new("/tmp/repo")
        );
    }

    #[test]
    fn clone_fetch_only_detects_no_checkout_and_bare_modes() {
        assert!(clone_fetch_only(&["--no-checkout".to_owned()]));
        assert!(clone_fetch_only(&["-n".to_owned()]));
        assert!(clone_fetch_only(&["--bare".to_owned()]));
        assert!(clone_fetch_only(&["--mirror".to_owned()]));
        assert!(!clone_fetch_only(&[
            "https://example.com/repo.git".to_owned()
        ]));
    }
}
