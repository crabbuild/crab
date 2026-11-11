//! Conflict resolution verbs.
//!
//! Implements `conflict_list`, `conflict_read_sides`, and
//! `git_apply_resolution`. Used by the renderer's ConflictsView
//! and MergeEditor to display and resolve merge conflicts.

use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{AgentError, Result};

// ---- Params structs --------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ConflictListParams {
    pub repo_id: String,
}

#[derive(Debug, Deserialize)]
pub struct ConflictReadSidesParams {
    pub repo_id: String,
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct ApplyResolutionParams {
    pub repo_id: String,
    pub path: String,
    pub content: String,
}

// ---- Handler functions -----------------------------------------------

/// List all conflicting files in the working tree.
///
/// Runs `git status --porcelain` and filters to conflict codes:
/// UU, AA, DD, DU, UD, AU, UA.
pub async fn conflict_list(work_dir: &Path) -> Result<Value> {
    let out = crate::shellout::git_command()?
        .args(["status", "--porcelain=v1", "-z"])
        .current_dir(work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;

    if !out.status.success() {
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }

    let mut files: Vec<Value> = Vec::new();

    // Parse NUL-separated porcelain output. Each entry starts with
    // a two-character XY code, a space, then the path terminated by NUL.
    let mut i = 0usize;
    while i < out.stdout.len() {
        if out.stdout.len() - i < 3 {
            break;
        }
        let xy = &out.stdout[i..i + 2];
        let path_start = i + 3;
        let path_end = out.stdout[path_start..]
            .iter()
            .position(|&b| b == 0)
            .map_or(out.stdout.len(), |p| path_start + p);
        let path = String::from_utf8_lossy(&out.stdout[path_start..path_end]).into_owned();
        let code = std::str::from_utf8(xy).unwrap_or("??").to_owned();

        if is_conflict_code(&code) {
            files.push(json!({ "path": path, "status": code }));
        }

        i = path_end + 1;

        // Skip the rename companion path if present.
        let is_rename = xy[0] == b'R' || xy[0] == b'C' || xy[1] == b'R' || xy[1] == b'C';
        if is_rename {
            let old_end = out.stdout[i..]
                .iter()
                .position(|&b| b == 0)
                .map_or(out.stdout.len(), |p| i + p);
            i = old_end + 1;
        }
    }

    Ok(json!({ "files": files }))
}

/// Read the three sides of a conflicting file from the index stages.
///
/// - Stage 1 (`:1:<path>`) = base (common ancestor)
/// - Stage 2 (`:2:<path>`) = current (ours / HEAD)
/// - Stage 3 (`:3:<path>`) = incoming (theirs / merge head)
///
/// Any side may be absent (e.g., file added on one side only) — returns
/// null for missing sides.
pub async fn conflict_read_sides(
    work_dir: &Path,
    params: &ConflictReadSidesParams,
) -> Result<Value> {
    let base = read_stage(work_dir, 1, &params.path).await?;
    let current = read_stage(work_dir, 2, &params.path).await?;
    let incoming = read_stage(work_dir, 3, &params.path).await?;

    Ok(json!({
        "base": base,
        "current": current,
        "incoming": incoming,
    }))
}

/// Write resolved content to disk and mark the file as resolved via
/// `git add`.
pub async fn git_apply_resolution(
    work_dir: &Path,
    params: &ApplyResolutionParams,
) -> Result<Value> {
    let file_path = work_dir.join(&params.path);

    // Ensure parent directories exist (file may be in a subdirectory).
    if let Some(parent) = file_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(AgentError::Io)?;
    }

    tokio::fs::write(&file_path, &params.content)
        .await
        .map_err(AgentError::Io)?;

    // Stage the resolved file.
    let out = crate::shellout::git_command()?
        .args(["add", "--", &params.path])
        .current_dir(work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;

    if !out.status.success() {
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }

    Ok(json!({ "resolved": params.path }))
}

// ---- Helpers ---------------------------------------------------------

/// Whether a two-letter porcelain code represents a merge conflict.
fn is_conflict_code(code: &str) -> bool {
    matches!(code, "UU" | "AA" | "DD" | "DU" | "UD" | "AU" | "UA")
}

/// Read a file from a specific index stage via `git show :<stage>:<path>`.
/// Returns `None` if the stage doesn't exist (command fails).
async fn read_stage(work_dir: &Path, stage: u8, path: &str) -> Result<Option<String>> {
    let spec = format!(":{stage}:{path}");
    let out = crate::shellout::git_command()?
        .args(["show", &spec])
        .current_dir(work_dir)
        .output()
        .await?;

    if !out.status.success() {
        return Ok(None);
    }

    Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
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

    /// Create a git repo with one commit on main, then create a
    /// conflicting merge scenario:
    /// - main: file.txt = "main content"
    /// - branch: file.txt = "branch content"
    /// - base: file.txt = "base content"
    fn create_conflict_repo() -> (TempDir, std::path::PathBuf) {
        let td = tempfile::tempdir().expect("tempdir");
        let root = td.path().to_path_buf();

        sh(&root, &["init", "--initial-branch=main"]);
        sh(&root, &["config", "user.email", "test@example.com"]);
        sh(&root, &["config", "user.name", "Test User"]);

        // Base commit with shared content.
        std::fs::write(root.join("file.txt"), "base content\n").expect("write");
        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-m", "base"]);

        // Create a branch with different content.
        sh(&root, &["checkout", "-b", "feature"]);
        std::fs::write(root.join("file.txt"), "branch content\n").expect("write");
        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-m", "feature change"]);

        // Back to main with conflicting content.
        sh(&root, &["checkout", "main"]);
        std::fs::write(root.join("file.txt"), "main content\n").expect("write");
        sh(&root, &["add", "."]);
        sh(&root, &["commit", "-m", "main change"]);

        // Attempt merge — this will fail with conflicts.
        sh(&root, &["merge", "feature", "--no-commit"]);

        (td, root)
    }

    #[tokio::test]
    async fn conflict_list_returns_conflicting_files() {
        let (_td, root) = create_conflict_repo();

        let result = conflict_list(&root).await.unwrap();
        let files = result["files"].as_array().unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["path"].as_str(), Some("file.txt"));
        assert_eq!(files[0]["status"].as_str(), Some("UU"));
    }

    #[tokio::test]
    async fn conflict_read_sides_returns_three_versions() {
        let (_td, root) = create_conflict_repo();

        let params = ConflictReadSidesParams {
            repo_id: "test".into(),
            path: "file.txt".into(),
        };

        let result = conflict_read_sides(&root, &params).await.unwrap();

        let base = result["base"].as_str().unwrap();
        let current = result["current"].as_str().unwrap();
        let incoming = result["incoming"].as_str().unwrap();

        assert_eq!(base, "base content\n");
        assert_eq!(current, "main content\n");
        assert_eq!(incoming, "branch content\n");
    }

    #[tokio::test]
    async fn apply_resolution_marks_file_resolved() {
        let (_td, root) = create_conflict_repo();

        // Verify conflict exists before resolution.
        let before = conflict_list(&root).await.unwrap();
        assert_eq!(before["files"].as_array().unwrap().len(), 1);

        let params = ApplyResolutionParams {
            repo_id: "test".into(),
            path: "file.txt".into(),
            content: "resolved content\n".into(),
        };

        let result = git_apply_resolution(&root, &params).await.unwrap();
        assert_eq!(result["resolved"].as_str(), Some("file.txt"));

        // Verify the file is no longer conflicted.
        let after = conflict_list(&root).await.unwrap();
        assert_eq!(after["files"].as_array().unwrap().len(), 0);

        // Verify file content on disk.
        let on_disk = std::fs::read_to_string(root.join("file.txt")).expect("read");
        assert_eq!(on_disk, "resolved content\n");
    }
}
