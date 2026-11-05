//! Git worktree management verbs.
//!
//! Implements `git_worktree_list`, `git_worktree_add`,
//! `git_worktree_remove`, `git_worktree_move`,
//! `git_worktree_prune`, `git_worktree_lock`,
//! `git_worktree_unlock`, and `git_worktree_repair`.
//!
//! Each handler calls the core `crab worktree` JSON contract and
//! returns the legacy desktop JSON shape. Worktrees let developers
//! work on multiple branches simultaneously without stashing or
//! switching — each worktree is a separate checkout of the same
//! repository.

use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{AgentError, Result};

// ---- Data types ------------------------------------------------------

/// A single worktree entry as returned by `git worktree list --porcelain`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WorktreeEntry {
    /// Absolute path to the worktree directory.
    pub path: String,
    /// HEAD commit SHA (40 hex chars). Empty for bare worktrees.
    pub head: String,
    /// Branch name (e.g. "refs/heads/main") or empty if detached.
    pub branch: String,
    /// Whether this is the main worktree (the original `.git` dir).
    pub is_main: bool,
    /// Whether the worktree is in detached HEAD state.
    pub is_detached: bool,
    /// Whether the worktree is locked (prevents pruning/removal).
    pub is_locked: bool,
    /// Lock reason, if any.
    pub lock_reason: Option<String>,
    /// Whether the worktree directory is missing from disk (prunable).
    pub is_prunable: bool,
}

// ---- Params structs --------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct WorktreeListParams {
    pub repo_id: String,
}

#[derive(Debug, Deserialize)]
pub struct WorktreeAddParams {
    pub repo_id: String,
    /// Filesystem path for the new worktree. Can be relative to the
    /// repo or absolute.
    pub path: String,
    /// Branch to check out in the new worktree. When `create_branch`
    /// is true, this is the name of the new branch to create.
    #[serde(default)]
    pub branch: Option<String>,
    /// When true, create a new branch with the name in `branch`.
    #[serde(default)]
    pub create_branch: bool,
    /// Start point for the new branch (only used when
    /// `create_branch` is true). Defaults to HEAD.
    #[serde(default)]
    pub start_point: Option<String>,
    /// Create a detached worktree at the given commit/tag.
    #[serde(default)]
    pub detach: bool,
    /// Hydration policy for the new worktree: "all", "on_access", or
    /// "pointers_only". Currently accepted but not yet applied.
    #[serde(default)]
    pub hydration: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WorktreeRemoveParams {
    pub repo_id: String,
    /// Path of the worktree to remove.
    pub path: String,
    /// Force removal even if the worktree has uncommitted changes.
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Deserialize)]
pub struct WorktreeMoveParams {
    pub repo_id: String,
    /// Current path of the worktree.
    pub old_path: String,
    /// New path to move the worktree to.
    pub new_path: String,
}

#[derive(Debug, Deserialize)]
pub struct WorktreePruneParams {
    pub repo_id: String,
    /// Dry-run: report what would be pruned without actually doing it.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Deserialize)]
pub struct WorktreeLockParams {
    pub repo_id: String,
    /// Path of the worktree to lock.
    pub path: String,
    /// Optional reason for locking.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WorktreeUnlockParams {
    pub repo_id: String,
    /// Path of the worktree to unlock.
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct WorktreeRepairParams {
    pub repo_id: String,
    /// Paths to repair. Empty lets Git repair discoverable worktrees.
    #[serde(default)]
    pub paths: Vec<String>,
}

// ---- Handler functions -----------------------------------------------

/// List all worktrees. Returns `{ worktrees: [...] }`.
pub async fn git_worktree_list(work_dir: &Path) -> Result<Value> {
    let worktrees = crate::worktree::core_cli::list_worktrees(work_dir, false)
        .await?
        .into_iter()
        .map(core_entry_to_legacy)
        .collect::<Vec<_>>();
    Ok(json!({ "worktrees": worktrees }))
}

/// Add a new worktree. Returns the created worktree entry.
pub async fn git_worktree_add(work_dir: &Path, params: &WorktreeAddParams) -> Result<Value> {
    let mut args: Vec<String> = Vec::new();

    if params.detach {
        args.push("--detach".into());
    } else if params.create_branch {
        args.push("-b".into());
        if let Some(ref branch) = params.branch {
            args.push(branch.clone());
        }
    }

    args.push(params.path.clone());

    if params.detach {
        // For detached, the branch field holds the commit/tag to detach at.
        if let Some(ref commitish) = params.branch {
            args.push(commitish.clone());
        }
    } else if !params.create_branch {
        // Existing branch to check out.
        if let Some(ref branch) = params.branch {
            args.push(branch.clone());
        }
    } else if let Some(ref sp) = params.start_point {
        // Start point for the new branch.
        args.push(sp.clone());
    }

    if let Some(hydration) = hydration_arg(params.hydration.as_deref())? {
        args.push(hydration);
    }

    let result = crate::worktree::core_cli::add_worktree(work_dir, &args).await?;

    Ok(json!({
        "created": result.created,
        "path": result.path,
        "worktree": result.worktree.map(core_entry_to_legacy),
    }))
}

/// Remove a worktree. Returns `{ removed: path }`.
///
pub async fn git_worktree_remove(work_dir: &Path, params: &WorktreeRemoveParams) -> Result<Value> {
    let mut args: Vec<String> = Vec::new();
    if params.force {
        args.push("--force".to_owned());
    }
    args.push(params.path.clone());
    let result = crate::worktree::core_cli::remove_worktree(work_dir, &args).await?;
    Ok(json!({ "removed": result.path }))
}

/// Move a worktree to a new path. Returns `{ old_path, new_path }`.
pub async fn git_worktree_move(work_dir: &Path, params: &WorktreeMoveParams) -> Result<Value> {
    let args = vec![params.old_path.clone(), params.new_path.clone()];
    let result = crate::worktree::core_cli::move_worktree(work_dir, &args).await?;
    Ok(json!({ "old_path": result.old_path, "new_path": result.new_path }))
}

/// Prune stale worktree metadata. Returns list of pruned entries.
pub async fn git_worktree_prune(work_dir: &Path, params: &WorktreePruneParams) -> Result<Value> {
    let mut args: Vec<String> = vec!["--verbose".to_owned()];
    if params.dry_run {
        args.push("--dry-run".to_owned());
    }
    let result = crate::worktree::core_cli::prune_worktrees(work_dir, &args).await?;

    Ok(json!({
        "dry_run": result.dry_run,
        "pruned_lines": result.pruned_lines,
    }))
}

/// Lock a worktree to prevent accidental removal. Returns `{ locked: path }`.
pub async fn git_worktree_lock(work_dir: &Path, params: &WorktreeLockParams) -> Result<Value> {
    let mut args: Vec<String> = Vec::new();
    if let Some(ref reason) = params.reason {
        args.push("--reason".into());
        args.push(reason.clone());
    }
    args.push(params.path.clone());

    let result = crate::worktree::core_cli::lock_worktree(work_dir, &args).await?;
    Ok(json!({ "locked": result.path, "reason": params.reason }))
}

/// Unlock a worktree. Returns `{ unlocked: path }`.
pub async fn git_worktree_unlock(work_dir: &Path, params: &WorktreeUnlockParams) -> Result<Value> {
    let args = vec![params.path.clone()];
    let result = crate::worktree::core_cli::unlock_worktree(work_dir, &args).await?;
    Ok(json!({ "unlocked": result.path }))
}

/// Repair worktree administrative links. Returns `{ repaired, paths }`.
pub async fn git_worktree_repair(work_dir: &Path, params: &WorktreeRepairParams) -> Result<Value> {
    let result = crate::worktree::core_cli::repair_worktrees(work_dir, &params.paths).await?;
    Ok(json!({ "repaired": result.repaired, "paths": result.paths }))
}

fn hydration_arg(choice: Option<&str>) -> Result<Option<String>> {
    match choice {
        Some("all") => Ok(Some("--hydrate=full".to_owned())),
        Some("on_access") => Ok(Some("--hydrate=lazy".to_owned())),
        Some("pointers_only") => Ok(Some("--hydrate=pointer-only".to_owned())),
        Some(other) => Err(AgentError::Other(format!(
            "unsupported worktree hydration policy: {other}"
        ))),
        None => Ok(None),
    }
}

fn core_entry_to_legacy(entry: crate::worktree::core_cli::CrabWorktreeListEntry) -> WorktreeEntry {
    let is_main = entry.is_main();
    WorktreeEntry {
        path: entry.path,
        head: entry.head.unwrap_or_default(),
        branch: entry.branch.unwrap_or_default(),
        is_main,
        is_detached: entry.detached,
        is_locked: entry.locked,
        lock_reason: entry.lock_reason,
        is_prunable: entry.prunable,
    }
}

// ---- Parsing ---------------------------------------------------------

/// Parse `git worktree list --porcelain` output into structured entries.
///
/// Porcelain format is a sequence of blocks separated by blank lines.
/// Each block has key-value lines:
///   worktree /path/to/worktree
///   HEAD <sha>
///   branch refs/heads/main
///   (or: detached)
///   (or: bare)
///   locked [reason]
///   prunable
fn parse_porcelain_list(raw: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut path = String::new();
    let mut head = String::new();
    let mut branch = String::new();
    let mut is_main = false;
    let mut is_detached = false;
    let mut is_locked = false;
    let mut lock_reason: Option<String> = None;
    let mut is_prunable = false;
    let mut in_entry = false;
    let mut entry_index = 0;

    for line in raw.lines() {
        if line.is_empty() {
            // End of a block — flush the current entry.
            if in_entry && !path.is_empty() {
                entries.push(WorktreeEntry {
                    path: path.clone(),
                    head: head.clone(),
                    branch: branch.clone(),
                    is_main,
                    is_detached,
                    is_locked,
                    lock_reason: lock_reason.clone(),
                    is_prunable,
                });
            }
            // Reset for next block.
            path.clear();
            head.clear();
            branch.clear();
            is_main = false;
            is_detached = false;
            is_locked = false;
            lock_reason = None;
            is_prunable = false;
            in_entry = false;
            entry_index += 1;
            continue;
        }

        if let Some(rest) = line.strip_prefix("worktree ") {
            path = rest.to_owned();
            in_entry = true;
            // The first worktree entry is always the main worktree.
            if entry_index == 0 {
                is_main = true;
            }
        } else if let Some(rest) = line.strip_prefix("HEAD ") {
            head = rest.to_owned();
        } else if let Some(rest) = line.strip_prefix("branch ") {
            branch = rest.to_owned();
        } else if line == "detached" {
            is_detached = true;
        } else if line == "bare" {
            // Bare repos have no branch or detached state.
        } else if line == "locked" {
            is_locked = true;
        } else if let Some(rest) = line.strip_prefix("locked ") {
            is_locked = true;
            lock_reason = Some(rest.to_owned());
        } else if line == "prunable" {
            is_prunable = true;
        }
    }

    // Flush the last entry if the output doesn't end with a blank line.
    if in_entry && !path.is_empty() {
        entries.push(WorktreeEntry {
            path,
            head,
            branch,
            is_main,
            is_detached,
            is_locked,
            lock_reason,
            is_prunable,
        });
    }

    entries
}

// ---- Helpers ---------------------------------------------------------

/// Run a git command and return stdout on success, or an error on failure.
async fn run_git(work_dir: &Path, args: &[&str]) -> Result<String> {
    let out = crate::shellout::git_command()
        .args(args)
        .current_dir(work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;

    if !out.status.success() {
        let status = out.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        return Err(AgentError::Cli { status, stderr });
    }

    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

// ---- Tests -----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use tempfile::TempDir;

    fn sh(dir: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("git command spawn")
    }

    /// Create a git repo with one commit so worktrees can be created.
    fn git_init_with_commit() -> (TempDir, std::path::PathBuf) {
        let td = tempfile::tempdir().expect("tempdir");
        let root = td.path().to_path_buf();
        sh(&root, &["init", "--initial-branch=main"]);
        sh(&root, &["config", "user.email", "test@example.com"]);
        sh(&root, &["config", "user.name", "Test User"]);
        std::fs::write(root.join("README.md"), "# test\n").expect("write");
        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-m", "initial"]);
        (td, root)
    }

    #[test]
    fn parse_porcelain_single_worktree() {
        let input = concat!(
            "worktree /home/user/project\n",
            "HEAD abc1234567890abcdef1234567890abcdef123456\n",
            "branch refs/heads/main\n",
            "\n",
        );
        let entries = parse_porcelain_list(input);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "/home/user/project");
        assert_eq!(entries[0].head, "abc1234567890abcdef1234567890abcdef123456");
        assert_eq!(entries[0].branch, "refs/heads/main");
        assert!(entries[0].is_main);
        assert!(!entries[0].is_detached);
        assert!(!entries[0].is_locked);
    }

    #[test]
    fn parse_porcelain_multiple_worktrees() {
        let input = concat!(
            "worktree /home/user/project\n",
            "HEAD aaaa\n",
            "branch refs/heads/main\n",
            "\n",
            "worktree /home/user/project-feature\n",
            "HEAD bbbb\n",
            "branch refs/heads/feature\n",
            "\n",
            "worktree /home/user/project-detached\n",
            "HEAD cccc\n",
            "detached\n",
            "\n",
        );
        let entries = parse_porcelain_list(input);
        assert_eq!(entries.len(), 3);
        assert!(entries[0].is_main);
        assert!(!entries[1].is_main);
        assert!(!entries[2].is_main);
        assert!(entries[2].is_detached);
        assert_eq!(entries[1].branch, "refs/heads/feature");
    }

    #[test]
    fn parse_porcelain_locked_with_reason() {
        let input = concat!(
            "worktree /home/user/project\n",
            "HEAD aaaa\n",
            "branch refs/heads/main\n",
            "\n",
            "worktree /tmp/wt-locked\n",
            "HEAD bbbb\n",
            "branch refs/heads/locked-branch\n",
            "locked on external drive\n",
            "\n",
        );
        let entries = parse_porcelain_list(input);
        assert_eq!(entries.len(), 2);
        assert!(entries[1].is_locked);
        assert_eq!(entries[1].lock_reason.as_deref(), Some("on external drive"));
    }

    #[test]
    fn parse_porcelain_prunable() {
        let input = concat!(
            "worktree /home/user/project\n",
            "HEAD aaaa\n",
            "branch refs/heads/main\n",
            "\n",
            "worktree /tmp/gone\n",
            "HEAD bbbb\n",
            "branch refs/heads/gone-branch\n",
            "prunable\n",
            "\n",
        );
        let entries = parse_porcelain_list(input);
        assert_eq!(entries.len(), 2);
        assert!(entries[1].is_prunable);
    }

    #[tokio::test]
    async fn worktree_list_on_fresh_repo() {
        let (_td, root) = git_init_with_commit();
        let result = git_worktree_list(&root).await.unwrap();
        let worktrees = result["worktrees"].as_array().unwrap();
        assert_eq!(worktrees.len(), 1);
        assert!(worktrees[0]["is_main"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn worktree_add_and_remove() {
        let (_td, root) = git_init_with_commit();

        // Create a branch to check out in the worktree.
        sh(&root, &["branch", "feature-wt"]);

        let wt_dir = tempfile::tempdir().expect("wt tempdir");
        let wt_path = wt_dir.path().join("wt-feature");
        let wt_path_str = wt_path.to_string_lossy().into_owned();

        let add_params = WorktreeAddParams {
            repo_id: "test".into(),
            path: wt_path_str.clone(),
            branch: Some("feature-wt".into()),
            create_branch: false,
            start_point: None,
            detach: false,
            hydration: None,
        };

        let result = git_worktree_add(&root, &add_params).await.unwrap();
        assert!(result["created"].as_bool().unwrap());

        // Verify it shows up in the list.
        let list = git_worktree_list(&root).await.unwrap();
        let worktrees = list["worktrees"].as_array().unwrap();
        assert_eq!(worktrees.len(), 2);

        // Remove it.
        let remove_params = WorktreeRemoveParams {
            repo_id: "test".into(),
            path: wt_path_str,
            force: false,
        };
        let removed = git_worktree_remove(&root, &remove_params).await.unwrap();
        assert!(removed["removed"].as_str().is_some());

        // Verify it's gone.
        let list2 = git_worktree_list(&root).await.unwrap();
        let worktrees2 = list2["worktrees"].as_array().unwrap();
        assert_eq!(worktrees2.len(), 1);
    }

    #[tokio::test]
    async fn worktree_add_with_new_branch() {
        let (_td, root) = git_init_with_commit();

        let wt_dir = tempfile::tempdir().expect("wt tempdir");
        let wt_path = wt_dir.path().join("wt-new-branch");
        let wt_path_str = wt_path.to_string_lossy().into_owned();

        let add_params = WorktreeAddParams {
            repo_id: "test".into(),
            path: wt_path_str.clone(),
            branch: Some("new-feature".into()),
            create_branch: true,
            start_point: None,
            detach: false,
            hydration: None,
        };

        let result = git_worktree_add(&root, &add_params).await.unwrap();
        assert!(result["created"].as_bool().unwrap());

        // Clean up.
        let remove_params = WorktreeRemoveParams {
            repo_id: "test".into(),
            path: wt_path_str,
            force: false,
        };
        git_worktree_remove(&root, &remove_params).await.unwrap();
    }

    #[tokio::test]
    async fn worktree_lock_and_unlock() {
        let (_td, root) = git_init_with_commit();
        sh(&root, &["branch", "lock-test"]);

        let wt_dir = tempfile::tempdir().expect("wt tempdir");
        let wt_path = wt_dir.path().join("wt-lock-test");
        let wt_path_str = wt_path.to_string_lossy().into_owned();

        let add_params = WorktreeAddParams {
            repo_id: "test".into(),
            path: wt_path_str.clone(),
            branch: Some("lock-test".into()),
            create_branch: false,
            start_point: None,
            detach: false,
            hydration: None,
        };
        git_worktree_add(&root, &add_params).await.unwrap();

        // Lock it.
        let lock_params = WorktreeLockParams {
            repo_id: "test".into(),
            path: wt_path_str.clone(),
            reason: Some("on external drive".into()),
        };
        let locked = git_worktree_lock(&root, &lock_params).await.unwrap();
        let canonical_wt_path = wt_path
            .canonicalize()
            .expect("canonical worktree path")
            .to_string_lossy()
            .into_owned();
        assert_eq!(locked["locked"].as_str().unwrap(), canonical_wt_path);

        // Verify locked state in list.
        let list = git_worktree_list(&root).await.unwrap();
        let worktrees = list["worktrees"].as_array().unwrap();
        let wt = worktrees
            .iter()
            .find(|w| !w["is_main"].as_bool().unwrap())
            .unwrap();
        assert!(wt["is_locked"].as_bool().unwrap());

        // Unlock it.
        let unlock_params = WorktreeUnlockParams {
            repo_id: "test".into(),
            path: wt_path_str.clone(),
        };
        git_worktree_unlock(&root, &unlock_params).await.unwrap();

        // Force remove (locked worktrees need force).
        let remove_params = WorktreeRemoveParams {
            repo_id: "test".into(),
            path: wt_path_str,
            force: true,
        };
        git_worktree_remove(&root, &remove_params).await.unwrap();
    }
}
