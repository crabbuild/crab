//! Branch management verbs.
//!
//! Implements `git_branch_create`, `git_branch_delete`,
//! `git_branch_rename`, `git_checkout`, and `git_set_upstream`.
//! Each handler shells out to `git` and returns structured JSON.

use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{AgentError, Result};

// ---- Params structs --------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct BranchCreateParams {
    pub repo_id: String,
    pub name: String,
    /// Start point for the new branch. Defaults to HEAD when absent.
    #[serde(default)]
    pub start_point: Option<String>,
    /// Optional upstream ref to track. When provided, passes
    /// `--track <upstream>`; when absent, passes `--no-track`.
    #[serde(default)]
    pub track: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BranchDeleteParams {
    pub repo_id: String,
    pub name: String,
    /// `true` → `-D` (force), `false` → `-d` (safe).
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Deserialize)]
pub struct BranchRenameParams {
    pub repo_id: String,
    pub old_name: String,
    pub new_name: String,
}

#[derive(Debug, Deserialize)]
pub struct CheckoutParams {
    pub repo_id: String,
    pub ref_name: String,
    /// `true` → `--detach`.
    #[serde(default)]
    pub detach: bool,
    /// `true` → insert `--` before `ref_name` so git treats it as a
    /// path (file discard) rather than a branch name.
    #[serde(default)]
    pub pathspec: bool,
}

#[derive(Debug, Deserialize)]
pub struct SetUpstreamParams {
    pub repo_id: String,
    pub branch: String,
    pub upstream: String,
}

// ---- Handler functions -----------------------------------------------

/// Create a branch. Returns `{ name, sha }` where sha is the resolved
/// commit the new branch points at.
pub async fn git_branch_create(work_dir: &Path, params: &BranchCreateParams) -> Result<Value> {
    let mut args: Vec<String> = vec!["branch".into()];

    if let Some(upstream) = &params.track {
        args.push("--track".into());
        args.push(upstream.clone());
    } else {
        args.push("--no-track".into());
    }

    args.push(params.name.clone());

    if let Some(sp) = &params.start_point {
        args.push(sp.clone());
    }

    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run_git(work_dir, &args_ref).await?;

    // Resolve the SHA the new branch points at.
    let sha = rev_parse(work_dir, &params.name).await?;

    Ok(json!({ "name": params.name, "sha": sha }))
}

/// Delete a branch. Returns `{ deleted: name }`.
pub async fn git_branch_delete(work_dir: &Path, params: &BranchDeleteParams) -> Result<Value> {
    let flag = if params.force { "-D" } else { "-d" };
    run_git(work_dir, &["branch", flag, &params.name]).await?;
    Ok(json!({ "deleted": params.name }))
}

/// Rename a branch. Returns `{ old_name, new_name }`.
pub async fn git_branch_rename(work_dir: &Path, params: &BranchRenameParams) -> Result<Value> {
    run_git(
        work_dir,
        &["branch", "-m", &params.old_name, &params.new_name],
    )
    .await?;
    Ok(json!({ "old_name": params.old_name, "new_name": params.new_name }))
}

/// Checkout a ref. Returns `{ ref_name, detached }`.
/// When `pathspec` is true, inserts `--` before the target so git
/// treats it as a file path (used for discarding working-tree changes).
pub async fn git_checkout(work_dir: &Path, params: &CheckoutParams) -> Result<Value> {
    let mut args: Vec<&str> = vec!["checkout"];
    if params.detach {
        args.push("--detach");
    }
    if params.pathspec {
        args.push("--");
    }
    args.push(&params.ref_name);
    run_git(work_dir, &args).await?;
    Ok(json!({ "ref_name": params.ref_name, "detached": params.detach }))
}

/// Set the upstream tracking branch. Returns `{ branch, upstream }`.
pub async fn git_set_upstream(work_dir: &Path, params: &SetUpstreamParams) -> Result<Value> {
    let upstream_arg = format!("--set-upstream-to={}", params.upstream);
    run_git(work_dir, &["branch", &upstream_arg, &params.branch]).await?;
    Ok(json!({ "branch": params.branch, "upstream": params.upstream }))
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

/// Resolve a ref to its SHA via `git rev-parse`.
async fn rev_parse(work_dir: &Path, refspec: &str) -> Result<String> {
    run_git(work_dir, &["rev-parse", refspec]).await
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

    fn sh_stdout(dir: &Path, args: &[&str]) -> String {
        let out = sh(dir, args);
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Create a git repo with one commit so branches can be created.
    fn git_init_with_commit() -> (TempDir, std::path::PathBuf) {
        let td = tempfile::tempdir().expect("tempdir");
        let root = td.path().to_path_buf();
        sh(&root, &["init", "--initial-branch=main"]);
        sh(&root, &["config", "user.email", "test@example.com"]);
        sh(&root, &["config", "user.name", "Test User"]);
        // Create an initial commit so HEAD exists.
        std::fs::write(root.join("README.md"), "# test\n").expect("write");
        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-m", "initial"]);
        (td, root)
    }

    #[tokio::test]
    async fn branch_create_from_head() {
        let (_td, root) = git_init_with_commit();

        let params = BranchCreateParams {
            repo_id: "test".into(),
            name: "feature-x".into(),
            start_point: None,
            track: None,
        };

        let result = git_branch_create(&root, &params).await.unwrap();
        assert_eq!(result["name"].as_str(), Some("feature-x"));
        assert!(result["sha"].as_str().is_some_and(|s| s.len() >= 7));

        // Verify the branch exists.
        let branches = sh_stdout(&root, &["branch", "--list", "feature-x"]);
        assert!(branches.contains("feature-x"));
    }

    #[tokio::test]
    async fn branch_create_from_start_point() {
        let (_td, root) = git_init_with_commit();

        // Create a second commit.
        std::fs::write(root.join("second.txt"), "two\n").expect("write");
        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-m", "second"]);

        // Get the first commit SHA.
        let first_sha = sh_stdout(&root, &["rev-parse", "HEAD~1"]);

        let params = BranchCreateParams {
            repo_id: "test".into(),
            name: "from-first".into(),
            start_point: Some(first_sha.clone()),
            track: None,
        };

        let result = git_branch_create(&root, &params).await.unwrap();
        assert_eq!(result["sha"].as_str(), Some(first_sha.as_str()));
    }

    #[tokio::test]
    async fn branch_delete_merged() {
        let (_td, root) = git_init_with_commit();

        // Create and merge a branch so it's safe to delete.
        sh(&root, &["branch", "to-delete"]);

        let params = BranchDeleteParams {
            repo_id: "test".into(),
            name: "to-delete".into(),
            force: false,
        };

        let result = git_branch_delete(&root, &params).await.unwrap();
        assert_eq!(result["deleted"].as_str(), Some("to-delete"));

        // Verify it's gone.
        let branches = sh_stdout(&root, &["branch", "--list", "to-delete"]);
        assert!(!branches.contains("to-delete"));
    }

    #[tokio::test]
    async fn branch_delete_unmerged_without_force_fails() {
        let (_td, root) = git_init_with_commit();

        // Create a branch with a unique commit (unmerged).
        sh(&root, &["checkout", "-b", "unmerged-branch"]);
        std::fs::write(root.join("unmerged.txt"), "data\n").expect("write");
        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-m", "unmerged work"]);
        sh(&root, &["checkout", "main"]);

        let params = BranchDeleteParams {
            repo_id: "test".into(),
            name: "unmerged-branch".into(),
            force: false,
        };

        let err = git_branch_delete(&root, &params).await.unwrap_err();
        assert!(matches!(err, AgentError::Cli { .. }));
    }

    #[tokio::test]
    async fn branch_delete_unmerged_with_force_succeeds() {
        let (_td, root) = git_init_with_commit();

        // Create a branch with a unique commit (unmerged).
        sh(&root, &["checkout", "-b", "unmerged-branch"]);
        std::fs::write(root.join("unmerged.txt"), "data\n").expect("write");
        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-m", "unmerged work"]);
        sh(&root, &["checkout", "main"]);

        let params = BranchDeleteParams {
            repo_id: "test".into(),
            name: "unmerged-branch".into(),
            force: true,
        };

        let result = git_branch_delete(&root, &params).await.unwrap();
        assert_eq!(result["deleted"].as_str(), Some("unmerged-branch"));
    }

    #[tokio::test]
    async fn branch_rename() {
        let (_td, root) = git_init_with_commit();

        sh(&root, &["branch", "old-name"]);

        let params = BranchRenameParams {
            repo_id: "test".into(),
            old_name: "old-name".into(),
            new_name: "new-name".into(),
        };

        let result = git_branch_rename(&root, &params).await.unwrap();
        assert_eq!(result["old_name"].as_str(), Some("old-name"));
        assert_eq!(result["new_name"].as_str(), Some("new-name"));

        // Verify old name is gone and new name exists.
        let branches = sh_stdout(&root, &["branch"]);
        assert!(!branches.contains("old-name"));
        assert!(branches.contains("new-name"));
    }

    #[tokio::test]
    async fn checkout_branch() {
        let (_td, root) = git_init_with_commit();

        sh(&root, &["branch", "other"]);

        let params = CheckoutParams {
            repo_id: "test".into(),
            ref_name: "other".into(),
            detach: false,
            pathspec: false,
        };

        let result = git_checkout(&root, &params).await.unwrap();
        assert_eq!(result["ref_name"].as_str(), Some("other"));
        assert_eq!(result["detached"].as_bool(), Some(false));

        // Verify HEAD points to the branch.
        let head = sh_stdout(&root, &["symbolic-ref", "--short", "HEAD"]);
        assert_eq!(head, "other");
    }

    #[tokio::test]
    async fn checkout_detach() {
        let (_td, root) = git_init_with_commit();

        let head_sha = sh_stdout(&root, &["rev-parse", "HEAD"]);

        let params = CheckoutParams {
            repo_id: "test".into(),
            ref_name: "main".into(),
            detach: true,
            pathspec: false,
        };

        let result = git_checkout(&root, &params).await.unwrap();
        assert_eq!(result["detached"].as_bool(), Some(true));

        // Verify HEAD is detached (symbolic-ref fails on detached HEAD).
        let sym = sh(&root, &["symbolic-ref", "HEAD"]);
        assert!(!sym.status.success());

        // HEAD should resolve to the same SHA.
        let current = sh_stdout(&root, &["rev-parse", "HEAD"]);
        assert_eq!(current, head_sha);
    }

    #[tokio::test]
    async fn set_upstream() {
        let (_td, root) = git_init_with_commit();

        // Create a fake remote tracking branch by adding a remote and
        // fetching (simulated via config).
        sh(
            &root,
            &["remote", "add", "origin", "https://example.com/repo.git"],
        );
        // Create a remote-tracking ref manually.
        sh(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);

        let params = SetUpstreamParams {
            repo_id: "test".into(),
            branch: "main".into(),
            upstream: "origin/main".into(),
        };

        let result = git_set_upstream(&root, &params).await.unwrap();
        assert_eq!(result["branch"].as_str(), Some("main"));
        assert_eq!(result["upstream"].as_str(), Some("origin/main"));

        // Verify via git config.
        let remote = sh_stdout(&root, &["config", "branch.main.remote"]);
        assert_eq!(remote, "origin");
        let merge = sh_stdout(&root, &["config", "branch.main.merge"]);
        assert_eq!(merge, "refs/heads/main");
    }
}
