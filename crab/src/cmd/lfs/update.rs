//! `crab lfs update` — update git hooks and filter configuration for LFS.
//!
//! Re-applies the pre-push hook and transfer agent configuration to match
//! the current crab binary version. Similar to `install` but designed
//! for upgrades.

use std::path::Path;
use std::process::Command;

use crate::core::error::{CrabError, Result};

/// Run `crab lfs update` with the given flags.
///
/// Updates the pre-push hook and filter configuration to the current
/// crab version. With `--force`, overwrites modified hooks. With
/// `--manual`, prints the commands instead of executing them.
pub fn run_lfs_update(force: bool, manual: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    run_lfs_update_in(&cwd, force, manual)
}

/// Testable inner implementation that accepts an explicit root directory.
pub fn run_lfs_update_in(root: &Path, force: bool, manual: bool) -> Result<()> {
    let bin = crate::cmd::init::crab_binary_path();

    if manual {
        print_manual_instructions(&bin);
        return Ok(());
    }

    // Update git config entries.
    update_config(root, &bin)?;

    // Update the pre-push hook.
    update_pre_push_hook(root, force)?;

    eprintln!("Updated git hooks and LFS configuration.");
    Ok(())
}

/// Print the commands the user should run manually.
fn print_manual_instructions(bin: &str) {
    println!("Run the following commands to update LFS configuration:");
    println!();
    println!("  git config lfs.customtransfer.crab.path \"{bin}\"");
    println!("  git config lfs.customtransfer.crab.args lfs-transfer-agent");
    println!("  git config lfs.standalonetransferagent crab");
    println!("  git config filter.lfs.clean \"{bin} lfs clean -- %f\"");
    println!("  git config filter.lfs.smudge \"{bin} lfs smudge -- %f\"");
    println!("  git config filter.lfs.process \"{bin} lfs filter-process\"");
    println!("  git config filter.lfs.required true");
    println!();
    println!("Replace pre-push in the directory printed by:");
    println!("  git rev-parse --git-path hooks");
    println!();
    for line in super::install::pre_push_hook_content().lines() {
        println!("  {line}");
    }
}

/// Update the transfer agent config entries to the current binary path.
fn update_config(root: &Path, bin: &str) -> Result<()> {
    let configs = [
        ("lfs.customtransfer.crab.path", bin.to_owned()),
        (
            "lfs.customtransfer.crab.args",
            "lfs-transfer-agent".to_owned(),
        ),
        ("lfs.standalonetransferagent", "crab".to_owned()),
        ("filter.lfs.clean", format!("{bin} lfs clean -- %f")),
        ("filter.lfs.smudge", format!("{bin} lfs smudge -- %f")),
        ("filter.lfs.process", format!("{bin} lfs filter-process")),
        ("filter.lfs.required", "true".to_owned()),
    ];

    for (key, value) in configs {
        let output = Command::new("git")
            .args(["config", key, &value])
            .current_dir(root)
            .output()
            .map_err(|e| CrabError::Configuration {
                key: key.to_owned(),
                origin: format!("failed to run git config: {e}"),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(CrabError::Configuration {
                key: key.to_owned(),
                origin: format!("git config failed: {stderr}"),
            });
        }

        tracing::debug!(key, value, "updated git config");
    }

    Ok(())
}

/// Update the pre-push hook.
///
/// Uses the same fail-closed ownership and composition rules as install.
fn update_pre_push_hook(root: &Path, force: bool) -> Result<()> {
    let hooks_dir = super::hooks_dir_from(root)?;
    super::install::install_pre_push_hook_at(&hooks_dir, force)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        dir
    }

    fn get_config(root: &Path, key: &str) -> Option<String> {
        let output = Command::new("git")
            .args(["config", key])
            .current_dir(root)
            .output()
            .unwrap();
        if output.status.success() {
            Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        } else {
            None
        }
    }

    #[test]
    fn update_sets_config_entries() {
        let dir = temp_git_repo();
        run_lfs_update_in(dir.path(), false, false).unwrap();

        let agent = get_config(dir.path(), "lfs.standalonetransferagent");
        assert_eq!(agent.as_deref(), Some("crab"));

        let args = get_config(dir.path(), "lfs.customtransfer.crab.args");
        assert_eq!(args.as_deref(), Some("lfs-transfer-agent"));

        let clean = get_config(dir.path(), "filter.lfs.clean");
        assert!(
            clean
                .as_deref()
                .is_some_and(|v| v.ends_with(" lfs clean -- %f")),
            "clean filter should invoke crab lfs clean, got: {clean:?}"
        );

        let smudge = get_config(dir.path(), "filter.lfs.smudge");
        assert!(
            smudge
                .as_deref()
                .is_some_and(|v| v.ends_with(" lfs smudge -- %f")),
            "smudge filter should invoke crab lfs smudge, got: {smudge:?}"
        );

        let process = get_config(dir.path(), "filter.lfs.process");
        assert!(
            process
                .as_deref()
                .is_some_and(|v| v.ends_with(" lfs filter-process")),
            "process filter should invoke crab lfs filter-process, got: {process:?}"
        );

        let required = get_config(dir.path(), "filter.lfs.required");
        assert_eq!(required.as_deref(), Some("true"));
    }

    #[test]
    fn update_creates_hook_when_missing() {
        let dir = temp_git_repo();
        run_lfs_update_in(dir.path(), false, false).unwrap();

        let hook = dir.path().join(".git/hooks/pre-push");
        assert!(hook.exists());
        let content = fs::read_to_string(&hook).unwrap();
        assert!(content.contains("crab lfs pre-push"));
    }

    #[test]
    fn update_force_overwrites_modified_hook() {
        let dir = temp_git_repo();

        // Create a custom hook.
        let hooks_dir = dir.path().join(".git/hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        fs::write(hooks_dir.join("pre-push"), "#!/bin/sh\necho custom\n").unwrap();

        // Without force, update fails closed and preserves the custom hook.
        assert!(run_lfs_update_in(dir.path(), false, false).is_err());
        let content = fs::read_to_string(hooks_dir.join("pre-push")).unwrap();
        assert!(content.contains("custom"));

        // With force, the hook is overwritten.
        run_lfs_update_in(dir.path(), true, false).unwrap();
        let content = fs::read_to_string(hooks_dir.join("pre-push")).unwrap();
        assert!(content.contains("crab lfs pre-push"));
    }
}
