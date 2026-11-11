//! Remote-interaction verbs: fetch-all, pull (with mode), push (with options).
//!
//! Implements `git_fetch_all`, `git_pull` (mode-aware), and
//! `git_push` (force / force-with-lease / set-upstream). Each handler
//! shells out to `git` and returns structured JSON.

use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{AgentError, Result};

// ---- Params structs --------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct FetchAllParams {
    pub repo_id: String,
}

#[derive(Debug, Deserialize)]
pub struct PullParams {
    pub repo_id: String,
    /// Pull mode: `"merge"` (default), `"rebase"`, or `"ff-only"`.
    pub mode: String,
}

#[derive(Debug, Deserialize)]
pub struct PushParams {
    pub repo_id: String,
    /// Branch to push. When set, pushes `origin <branch>` explicitly.
    /// When absent, pushes the current HEAD.
    #[serde(default)]
    pub branch: Option<String>,
    /// `--force`
    #[serde(default)]
    pub force: bool,
    /// `--force-with-lease`
    #[serde(default)]
    pub force_with_lease: bool,
    /// `--set-upstream origin HEAD`
    #[serde(default)]
    pub set_upstream: bool,
}

// ---- Handler functions -----------------------------------------------

/// Fetch all remotes with pruning. Equivalent to `git fetch --all --prune`.
pub async fn git_fetch_all(work_dir: &Path, _params: &FetchAllParams) -> Result<Value> {
    run_git(work_dir, &["fetch", "--all", "--prune"]).await
}

/// Pull with a caller-specified mode.
///
/// - `"merge"` → `git pull`
/// - `"rebase"` → `git pull --rebase`
/// - `"ff-only"` → `git pull --ff-only`
pub async fn git_pull(work_dir: &Path, params: &PullParams) -> Result<Value> {
    let mut args: Vec<&str> = vec!["pull"];
    match params.mode.as_str() {
        "rebase" => args.push("--rebase"),
        "ff-only" => args.push("--ff-only"),
        "merge" | _ => { /* plain pull */ }
    }
    run_git(work_dir, &args).await
}

/// Push with optional force / force-with-lease / set-upstream flags.
///
/// Priority when multiple flags are set: `force` wins over
/// `force_with_lease` (they're mutually exclusive in git anyway).
/// `set_upstream` is additive.
pub async fn git_push(work_dir: &Path, params: &PushParams) -> Result<Value> {
    let mut args: Vec<&str> = vec!["push"];

    if params.force {
        args.push("--force");
    } else if params.force_with_lease {
        args.push("--force-with-lease");
    }

    if params.set_upstream {
        args.push("--set-upstream");
        args.push("origin");
        if let Some(ref branch) = params.branch {
            args.push(branch.as_str());
        } else {
            args.push("HEAD");
        }
    } else if let Some(ref branch) = params.branch {
        args.push("origin");
        args.push(branch.as_str());
    }

    run_git(work_dir, &args).await
}

// ---- Helpers ---------------------------------------------------------

/// Run a git command and return `{ status: 0, stdout, stderr }` on
/// success, or `AgentError::Cli` on non-zero exit.
async fn run_git(work_dir: &Path, args: &[&str]) -> Result<Value> {
    let mut command = crate::shellout::git_command()?;
    let out = command
        .args(args)
        .current_dir(work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;

    let status = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    if !out.status.success() {
        return Err(AgentError::Cli { status, stderr });
    }

    Ok(json!({
        "status": status,
        "stdout": stdout,
        "stderr": stderr,
    }))
}

// ---- Remote management -----------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RemoteAddParams {
    pub repo_id: String,
    pub name: String,
    pub url: String,
}

#[derive(Debug, Deserialize)]
pub struct RemoteRemoveParams {
    pub repo_id: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct RemoteSetUrlParams {
    pub repo_id: String,
    pub name: String,
    pub url: String,
}

/// Add a new remote. Equivalent to `git remote add <name> <url>`.
pub async fn git_remote_add(work_dir: &Path, params: &RemoteAddParams) -> Result<Value> {
    let url = crate::storage_auth::command_url(&params.url)?;
    run_git(work_dir, &["remote", "add", &params.name, &url]).await
}

/// Remove a remote. Equivalent to `git remote remove <name>`.
pub async fn git_remote_remove(work_dir: &Path, params: &RemoteRemoveParams) -> Result<Value> {
    run_git(work_dir, &["remote", "remove", &params.name]).await
}

/// Change the URL of an existing remote. Equivalent to `git remote set-url <name> <url>`.
pub async fn git_remote_set_url(work_dir: &Path, params: &RemoteSetUrlParams) -> Result<Value> {
    let url = crate::storage_auth::command_url(&params.url)?;
    run_git(work_dir, &["remote", "set-url", &params.name, &url]).await
}

// ---- Tests -----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use tempfile::TempDir;

    /// Shell out to git synchronously (test helper).
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

    /// Create a fresh git repo with an initial commit.
    fn init_repo() -> (TempDir, std::path::PathBuf) {
        let td = tempfile::tempdir().expect("tempdir");
        let root = td.path().to_path_buf();
        let out = sh(&root, &["init", "--initial-branch=main"]);
        assert!(out.status.success(), "git init failed");
        sh(&root, &["config", "user.email", "test@example.com"]);
        sh(&root, &["config", "user.name", "Test User"]);
        std::fs::write(root.join("file.txt"), "hello").unwrap();
        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-m", "init"]);
        (td, root)
    }

    /// Create a bare clone of `source` and add it as "origin" remote.
    fn add_bare_remote(source: &Path) -> (TempDir, std::path::PathBuf) {
        let bare_td = tempfile::tempdir().expect("tempdir for bare");
        let bare_path = bare_td.path().to_path_buf();
        let out = std::process::Command::new("git")
            .args([
                "clone",
                "--bare",
                source.to_str().unwrap(),
                bare_path.to_str().unwrap(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("bare clone");
        assert!(out.status.success(), "bare clone failed");
        // Add as remote to source
        sh(
            source,
            &["remote", "add", "origin", bare_path.to_str().unwrap()],
        );
        sh(source, &["fetch", "origin"]);
        sh(source, &["branch", "--set-upstream-to=origin/main", "main"]);
        (bare_td, bare_path)
    }

    #[tokio::test]
    async fn fetch_all_succeeds_with_remote() {
        let (_td, root) = init_repo();
        let (_bare_td, _bare_path) = add_bare_remote(&root);

        let params = FetchAllParams {
            repo_id: "test".into(),
        };
        let result = git_fetch_all(&root, &params).await;
        assert!(result.is_ok(), "fetch_all failed: {result:?}");
        let val = result.unwrap();
        assert_eq!(val["status"].as_i64(), Some(0));
    }

    #[tokio::test]
    async fn pull_ff_only_fails_on_diverged() {
        let (_td, root) = init_repo();
        let (_bare_td, bare_path) = add_bare_remote(&root);

        // Push current state to origin so tracking is in sync.
        sh(&root, &["push", "origin", "main"]);

        // Create a divergence: commit on the remote side via a
        // temporary clone, push, then commit locally.
        let other_td = tempfile::tempdir().expect("tempdir");
        let other = other_td.path().to_path_buf();
        let out = std::process::Command::new("git")
            .args([
                "clone",
                bare_path.to_str().unwrap(),
                other.to_str().unwrap(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert!(out.status.success(), "clone for diverge failed");
        sh(&other, &["config", "user.email", "other@example.com"]);
        sh(&other, &["config", "user.name", "Other"]);
        std::fs::write(other.join("remote.txt"), "from remote").unwrap();
        sh(&other, &["add", "."]);
        sh(&other, &["commit", "-m", "remote commit"]);
        sh(&other, &["push", "origin", "main"]);

        // Local commit that diverges.
        std::fs::write(root.join("local.txt"), "from local").unwrap();
        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-m", "local commit"]);

        // Fetch so we know about the remote commit.
        sh(&root, &["fetch", "origin"]);

        // Pull --ff-only should fail because histories diverged.
        let params = PullParams {
            repo_id: "test".into(),
            mode: "ff-only".into(),
        };
        let result = git_pull(&root, &params).await;
        assert!(
            result.is_err(),
            "expected ff-only pull to fail on diverged branches"
        );
        if let Err(AgentError::Cli { status, .. }) = &result {
            assert_ne!(*status, 0);
        }
    }

    #[tokio::test]
    async fn push_set_upstream_creates_tracking() {
        let (_td, root) = init_repo();
        let (_bare_td, _bare_path) = add_bare_remote(&root);

        // Create a new branch without upstream tracking.
        sh(&root, &["checkout", "-b", "feature-x"]);
        std::fs::write(root.join("feature.txt"), "new feature").unwrap();
        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-m", "feature commit"]);

        // Push with set_upstream.
        let params = PushParams {
            repo_id: "test".into(),
            branch: None,
            force: false,
            force_with_lease: false,
            set_upstream: true,
        };
        let result = git_push(&root, &params).await;
        assert!(result.is_ok(), "push --set-upstream failed: {result:?}");

        // Verify tracking is set up.
        let out = sh(&root, &["config", "--get", "branch.feature-x.remote"]);
        let remote = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        assert_eq!(remote, "origin", "expected tracking remote to be 'origin'");

        let out = sh(&root, &["config", "--get", "branch.feature-x.merge"]);
        let merge = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        assert_eq!(merge, "refs/heads/feature-x", "expected tracking merge ref");
    }
}
