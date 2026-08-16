//! `crab undo` — detect the last crab operation from git staged changes
//! and reverse it by delegating to the unadopt logic.
//!
//! Inspects `git diff --cached --name-only` for pointer files. If any are
//! found, delegates to `run_unadopt` with those file paths as patterns.
//! If no pointer files are detected, returns `NothingToUndo`.

use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::cmd::unadopt::{UnadoptArgs, run_unadopt};
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::OutputMode;
use crab_types::pointer::{MAX_POINTER_SIZE, is_pointer};

/// Schema name for structured output.
pub const UNDO_SCHEMA: &str = "undo";

/// Run the `crab undo` command.
///
/// Detects the last crab operation (adopt, add) by inspecting git staged
/// changes for pointer files, then delegates to `run_unadopt` for reversal.
pub async fn run_undo(mode: OutputMode, cancel: &CancellationToken) -> Result<()> {
    check_cancelled(cancel)?;

    let cwd = std::env::current_dir()?;
    let repo_root = discover_repo_root(&cwd)?;

    // Get the list of staged file paths.
    let staged_files = get_staged_files(&repo_root)?;

    if staged_files.is_empty() {
        return Err(CrabError::NothingToUndo);
    }

    // Check which staged files are pointer blobs.
    let pointer_patterns = find_staged_pointers(&repo_root, &staged_files);

    if pointer_patterns.is_empty() {
        return Err(CrabError::NothingToUndo);
    }

    debug!(
        count = pointer_patterns.len(),
        "detected pointer files in staged changes, delegating to unadopt"
    );

    // Delegate to unadopt with the detected pointer file paths as patterns.
    let args = UnadoptArgs {
        patterns: pointer_patterns,
        mode,
    };

    run_unadopt(&args, cancel).await
}

/// Run `git diff --cached --name-only` to get the list of staged file paths.
fn get_staged_files(repo_root: &Path) -> Result<Vec<String>> {
    let output = std::process::Command::new("git")
        .args(["diff", "--cached", "--name-only"])
        .current_dir(repo_root)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CrabError::Configuration {
            key: "git".into(),
            origin: format!("git diff --cached failed: {stderr}"),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let files: Vec<String> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();

    Ok(files)
}

/// Check which of the staged files are currently pointer blobs on disk.
fn find_staged_pointers(repo_root: &Path, staged_files: &[String]) -> Vec<String> {
    let mut pointers = Vec::new();

    for rel_path in staged_files {
        let full_path = repo_root.join(rel_path);

        // Skip files that don't exist on disk (deleted files).
        let Ok(meta) = std::fs::metadata(&full_path) else {
            continue;
        };

        // Pointer files are small — skip anything too large.
        if meta.len() > MAX_POINTER_SIZE as u64 {
            continue;
        }

        let Ok(content) = std::fs::read(&full_path) else {
            continue;
        };

        if is_pointer(&content) {
            pointers.push(rel_path.clone());
        }
    }

    pointers
}

/// Discover the git repository root from the current directory.
fn discover_repo_root(start: &Path) -> Result<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(start)
        .output()?;

    if !output.status.success() {
        return Err(CrabError::Configuration {
            key: "git".into(),
            origin: "not inside a git repository".into(),
        });
    }

    let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok(PathBuf::from(root))
}
