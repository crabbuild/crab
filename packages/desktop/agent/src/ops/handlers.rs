//! RPC verb handlers for resumable merge / rebase / cherry-pick operations.
//!
//! Each handler creates or loads an [`OpRecord`], executes the
//! appropriate git command, transitions state, persists the record,
//! and returns a structured JSON response.

use std::path::Path;

use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::driver::{
    MergeStrategy, Op, OpRecord, OpState, TodoAction, TodoItem, load_all, ops_dir_for_repo,
};
use crate::{AgentError, Result};

// ---------------------------------------------------------------------------
// Params
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct OpStartParams {
    pub repo_id: String,
    pub op: Op,
}

#[derive(Debug, Deserialize)]
pub struct OpContinueParams {
    pub repo_id: String,
    pub op_id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct OpAbortParams {
    pub repo_id: String,
    pub op_id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct OpStatusParams {
    pub repo_id: String,
    pub op_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct OpEditTodoParams {
    pub repo_id: String,
    pub op_id: Uuid,
    pub todo: Vec<TodoItem>,
}

#[derive(Debug, Deserialize)]
pub struct WorktreeCherryPickParams {
    pub repo_id: String,
    pub sha: String,
    pub target_worktree_path: String,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Begin a merge, rebase, or cherry-pick. Creates an `OpRecord`,
/// executes the git command, and transitions state based on the
/// outcome.
pub async fn op_start(work_dir: &Path, p: OpStartParams) -> Result<Value> {
    let ops_dir = ops_dir_for_repo(work_dir);
    let id = Uuid::new_v4();

    let mut record = OpRecord {
        id,
        op: p.op.clone(),
        state: OpState::InProgress,
        created_at: Utc::now(),
        paused_reason: None,
    };
    record.save(&ops_dir)?;

    let result = execute_op_command(work_dir, &record.op).await;
    finish_op_execution(&mut record, &ops_dir, result)
}

/// Continue a paused operation after conflicts have been resolved.
pub async fn op_continue(work_dir: &Path, p: OpContinueParams) -> Result<Value> {
    let ops_dir = ops_dir_for_repo(work_dir);
    let path = ops_dir.join(format!("{}.json", p.op_id));
    let mut record = OpRecord::load(&path)?;

    if record.state != OpState::Paused {
        return Err(AgentError::Other(format!(
            "cannot continue op {}: state is {:?}, expected Paused",
            p.op_id, record.state
        )));
    }

    record.transition(OpState::InProgress, None)?;
    record.save(&ops_dir)?;

    let result = execute_continue_command(work_dir, &record.op).await;
    finish_op_execution(&mut record, &ops_dir, result)
}

/// Abort an in-progress or paused operation.
pub async fn op_abort(work_dir: &Path, p: OpAbortParams) -> Result<Value> {
    let ops_dir = ops_dir_for_repo(work_dir);
    let path = ops_dir.join(format!("{}.json", p.op_id));
    let mut record = OpRecord::load(&path)?;

    let abort_args = abort_args_for_op(&record.op);
    run_git(work_dir, &abort_args).await?;

    record.transition(OpState::Aborted, None)?;
    record.save(&ops_dir)?;

    Ok(json!({
        "op_id": record.id.to_string(),
        "state": "aborted",
    }))
}

/// Return the current status of an operation (or the active one).
pub async fn op_status(work_dir: &Path, p: OpStatusParams) -> Result<Value> {
    let ops_dir = ops_dir_for_repo(work_dir);

    let record = if let Some(op_id) = p.op_id {
        let path = ops_dir.join(format!("{op_id}.json"));
        if path.exists() {
            Some(OpRecord::load(&path)?)
        } else {
            None
        }
    } else {
        // Find the active (InProgress or Paused) operation.
        let all = load_all(&ops_dir)?;
        all.into_iter()
            .find(|r| r.state == OpState::InProgress || r.state == OpState::Paused)
    };

    let conflicts = detect_conflicts(work_dir).await?;

    match record {
        Some(r) => Ok(json!({
            "op_id": r.id.to_string(),
            "state": format!("{:?}", r.state),
            "op": r.op,
            "conflicts": conflicts,
            "paused_reason": r.paused_reason,
        })),
        None => Ok(json!({
            "op_id": null,
            "state": null,
            "op": null,
            "conflicts": conflicts,
            "paused_reason": null,
        })),
    }
}

/// Rewrite the interactive rebase todo list for a paused rebase.
pub async fn op_edit_todo(work_dir: &Path, p: OpEditTodoParams) -> Result<Value> {
    let ops_dir = ops_dir_for_repo(work_dir);
    let path = ops_dir.join(format!("{}.json", p.op_id));
    let mut record = OpRecord::load(&path)?;

    if record.state != OpState::Paused {
        return Err(AgentError::Other(format!(
            "cannot edit todo for op {}: state is {:?}, expected Paused",
            p.op_id, record.state
        )));
    }

    // Verify this is a rebase operation.
    match &record.op {
        Op::Rebase { .. } => {}
        _ => {
            return Err(AgentError::Other(format!(
                "op {} is not a rebase, cannot edit todo",
                p.op_id
            )));
        }
    }

    // Write the todo file in the format git expects.
    let todo_path = work_dir.join(".git/rebase-merge/git-rebase-todo");
    let content = format_todo_list(&p.todo);
    std::fs::write(&todo_path, &content).map_err(|e| {
        AgentError::Other(format!(
            "failed to write rebase-todo at {}: {e}",
            todo_path.display()
        ))
    })?;

    // Update the record's op with the new todo.
    if let Op::Rebase { ref mut todo, .. } = record.op {
        *todo = Some(p.todo);
    }
    record.save(&ops_dir)?;

    Ok(json!({
        "op_id": record.id.to_string(),
        "updated": true,
    }))
}

// ---------------------------------------------------------------------------
// Worktree cherry-pick
// ---------------------------------------------------------------------------

/// Cherry-pick a commit from one worktree's branch into another.
///
/// Runs `git cherry-pick <sha>` in the target worktree's directory.
/// Reuses the Pillar A.4 driver's conflict detection and state
/// management pattern but scoped to a specific worktree path.
pub async fn worktree_cherry_pick(work_dir: &Path, p: WorktreeCherryPickParams) -> Result<Value> {
    use crate::worktree::WorktreeManager;

    let mgr = WorktreeManager::new(work_dir.to_path_buf());
    let target_path = std::path::PathBuf::from(&p.target_worktree_path);

    let (code, stdout, stderr) = mgr.worktree_cherry_pick(&p.sha, &target_path).await?;

    if code == 0 {
        return Ok(json!({
            "state": "done",
            "sha": p.sha,
            "target_worktree_path": p.target_worktree_path,
        }));
    }

    // Check for conflicts — same heuristic as finish_op_execution.
    let combined = format!("{stdout}\n{stderr}").to_lowercase();
    let is_conflict = combined.contains("conflict")
        || combined.contains("fix conflicts")
        || combined.contains("merge conflict");

    if is_conflict {
        let resolved_target = if target_path.is_absolute() {
            target_path
        } else {
            work_dir.join(&target_path)
        };
        let conflicts = detect_conflicts(&resolved_target).await.unwrap_or_default();

        return Ok(json!({
            "state": "paused",
            "sha": p.sha,
            "target_worktree_path": p.target_worktree_path,
            "conflicts": conflicts,
        }));
    }

    // Non-conflict failure.
    let error_detail = if stderr.is_empty() { stdout } else { stderr };
    Err(AgentError::Cli {
        status: code,
        stderr: error_detail,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Run a git command and return (exit_code, stdout, stderr).
async fn run_git(work_dir: &Path, args: &[&str]) -> Result<(i32, String, String)> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;

    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Ok((code, stdout, stderr))
}

/// Execute the initial git command for an operation.
async fn execute_op_command(work_dir: &Path, op: &Op) -> Result<(i32, String, String)> {
    if let Op::Merge {
        from,
        into,
        strategy,
        message,
    } = op
    {
        return execute_merge_command(work_dir, from, into, strategy, message.as_deref()).await;
    }

    let args = start_args_for_op(op);
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run_git(work_dir, &refs).await
}

async fn execute_merge_command(
    work_dir: &Path,
    from: &str,
    into: &str,
    strategy: &MergeStrategy,
    message: Option<&str>,
) -> Result<(i32, String, String)> {
    let target_ref = crate::git_ref_merge::local_branch_ref(into);
    if crate::git_ref_merge::current_branch_ref(work_dir)
        .await?
        .as_deref()
        == Some(target_ref.as_str())
    {
        let args = merge_args(from, strategy, message);
        let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        return run_git(work_dir, &refs).await;
    }

    if *strategy != MergeStrategy::MergeCommit {
        return Err(AgentError::Other(format!(
            "Target branch '{into}' is not checked out; switch to it before starting a {:?} merge.",
            strategy
        )));
    }

    let commit_message = message
        .map(str::to_owned)
        .unwrap_or_else(|| format!("Merge branch '{from}' into {into}"));

    match crate::git_ref_merge::merge_commit_without_checkout(work_dir, from, into, &commit_message)
        .await?
    {
        crate::git_ref_merge::RefMergeOutcome::Applied { merge_commit, .. } => Ok((
            0,
            format!("Created merge commit {merge_commit}\n"),
            String::new(),
        )),
        crate::git_ref_merge::RefMergeOutcome::AlreadyUpToDate { commit, .. } => Ok((
            0,
            format!("Already up to date at {commit}\n"),
            String::new(),
        )),
        crate::git_ref_merge::RefMergeOutcome::TargetCheckedOut { target_branch, .. } => {
            Err(AgentError::Other(format!(
                "Target branch '{target_branch}' is checked out in another worktree; switch to that worktree to merge."
            )))
        }
    }
}

/// Execute the continue command for an operation.
async fn execute_continue_command(work_dir: &Path, op: &Op) -> Result<(i32, String, String)> {
    let args = continue_args_for_op(op);
    run_git(work_dir, &args).await
}

/// Build the git arguments for starting an operation.
fn start_args_for_op(op: &Op) -> Vec<String> {
    match op {
        Op::Merge {
            from,
            strategy,
            message,
            ..
        } => merge_args(from, strategy, message.as_deref()),
        Op::Rebase { onto, branch, .. } => {
            vec!["rebase".to_owned(), onto.clone(), branch.clone()]
        }
        Op::CherryPick { shas, .. } => {
            let mut args = vec!["cherry-pick".to_owned()];
            args.extend(shas.iter().cloned());
            args
        }
    }
}

fn merge_args(from: &str, strategy: &MergeStrategy, message: Option<&str>) -> Vec<String> {
    let mut args = vec!["merge".to_owned()];
    match strategy {
        MergeStrategy::Squash => args.push("--squash".to_owned()),
        MergeStrategy::FastForwardOnly => args.push("--ff-only".to_owned()),
        MergeStrategy::MergeCommit => {}
    }
    if let Some(msg) = message {
        args.push("-m".to_owned());
        args.push(msg.to_owned());
    }
    args.push(from.to_owned());
    args
}

/// Build the git arguments for continuing an operation.
fn continue_args_for_op(op: &Op) -> Vec<&str> {
    match op {
        Op::Merge { .. } => vec!["commit", "--no-edit"],
        Op::Rebase { .. } => vec!["rebase", "--continue"],
        Op::CherryPick { .. } => vec!["cherry-pick", "--continue"],
    }
}

/// Build the git arguments for aborting an operation.
fn abort_args_for_op(op: &Op) -> Vec<&str> {
    match op {
        Op::Merge { .. } => vec!["merge", "--abort"],
        Op::Rebase { .. } => vec!["rebase", "--abort"],
        Op::CherryPick { .. } => vec!["cherry-pick", "--abort"],
    }
}

/// After executing a git command, transition the record based on the
/// outcome and return the appropriate JSON response.
fn finish_op_execution(
    record: &mut OpRecord,
    ops_dir: &Path,
    result: Result<(i32, String, String)>,
) -> Result<Value> {
    let (code, stdout, stderr) = match result {
        Ok(output) => output,
        Err(e) => {
            let detail = e.to_string();
            record.transition(OpState::Aborted, Some(detail))?;
            record.save(ops_dir)?;
            return Err(e);
        }
    };

    if code == 0 {
        // For squash merges, git exits 0 but doesn't auto-commit.
        // We still mark as Done since the merge itself succeeded.
        record.transition(OpState::Done, None)?;
        record.save(ops_dir)?;
        return Ok(json!({
            "op_id": record.id.to_string(),
            "state": "done",
        }));
    }

    // Check if the failure is due to conflicts. Git writes conflict
    // messages to stdout (not stderr), so we check both streams.
    let combined = format!("{stdout}\n{stderr}").to_lowercase();
    let is_conflict = combined.contains("conflict")
        || combined.contains("fix conflicts")
        || combined.contains("merge conflict");

    if is_conflict {
        let reason = stdout
            .lines()
            .find(|l| l.to_lowercase().contains("conflict"))
            .unwrap_or("conflicts detected")
            .to_owned();
        record.transition(OpState::Paused, Some(reason))?;
        record.save(ops_dir)?;

        // Detect which files are in conflict.
        let conflicts = detect_conflicts_sync(record.id, ops_dir)?;

        return Ok(json!({
            "op_id": record.id.to_string(),
            "state": "paused",
            "conflicts": conflicts,
        }));
    }

    // Non-conflict failure — abort the operation.
    let error_detail = if stderr.is_empty() { stdout } else { stderr };
    record.transition(OpState::Aborted, Some(error_detail.clone()))?;
    record.save(ops_dir)?;
    Err(AgentError::Cli {
        status: code,
        stderr: error_detail,
    })
}

/// Detect conflicted files by running `git status --porcelain` and
/// filtering for conflict markers (UU, AA, DD, DU, UD, AU, UA).
pub async fn detect_conflicts(work_dir: &Path) -> Result<Vec<String>> {
    let (code, stdout, _stderr) = run_git(work_dir, &["status", "--porcelain"]).await?;
    if code != 0 {
        return Ok(Vec::new());
    }
    Ok(parse_conflict_paths(&stdout))
}

/// Synchronous conflict detection for use inside `finish_op_execution`.
/// Reads the work_dir from the ops_dir path (parent of ops/).
fn detect_conflicts_sync(_op_id: Uuid, ops_dir: &Path) -> Result<Vec<String>> {
    // ops_dir is <work_dir>/.crab/desktop/ops — walk up 3 levels.
    let work_dir = ops_dir
        .parent() // desktop
        .and_then(|p| p.parent()) // .crab
        .and_then(|p| p.parent()) // work_dir
        .ok_or_else(|| AgentError::Other("cannot resolve work_dir from ops_dir".to_owned()))?;

    let output = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(work_dir)
        .output()
        .map_err(AgentError::Io)?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_conflict_paths(&stdout))
}

/// Parse `git status --porcelain` output for conflict entries.
fn parse_conflict_paths(porcelain: &str) -> Vec<String> {
    let conflict_codes = ["UU", "AA", "DD", "DU", "UD", "AU", "UA"];
    porcelain
        .lines()
        .filter_map(|line| {
            if line.len() < 4 {
                return None;
            }
            let code = &line[..2];
            if conflict_codes.contains(&code) {
                Some(line[3..].to_owned())
            } else {
                None
            }
        })
        .collect()
}

/// Format a list of `TodoItem`s into the text format git expects in
/// `.git/rebase-merge/git-rebase-todo`.
fn format_todo_list(items: &[TodoItem]) -> String {
    let mut out = String::new();
    for item in items {
        let action = match &item.action {
            TodoAction::Pick => "pick",
            TodoAction::Reword => "reword",
            TodoAction::Squash => "squash",
            TodoAction::Fixup => "fixup",
            TodoAction::Drop => "drop",
            TodoAction::Exec(cmd) => {
                out.push_str(&format!("exec {cmd}\n"));
                continue;
            }
        };
        out.push_str(&format!("{} {} {}\n", action, item.sha, item.subject));
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    /// Create a git repo with an initial commit on `main`.
    fn init_repo() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir)
            .output()
            .unwrap();

        // These fixtures assert exact LF bytes after Git checkout/cherry-pick,
        // independently of the developer or runner's core.autocrlf setting.
        std::fs::write(dir.join(".gitattributes"), "*.txt text eol=lf\n").unwrap();
        std::fs::write(dir.join("file.txt"), "initial\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(dir)
            .output()
            .unwrap();

        tmp
    }

    /// Create a branch with a non-conflicting change.
    fn create_clean_branch(dir: &Path, branch: &str, file: &str, content: &str) {
        Command::new("git")
            .args(["checkout", "-b", branch])
            .current_dir(dir)
            .output()
            .unwrap();
        std::fs::write(dir.join(file), content).unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", &format!("commit on {branch}")])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["checkout", "main"])
            .current_dir(dir)
            .output()
            .unwrap();
    }

    /// Create a branch with a conflicting change to file.txt.
    fn create_conflicting_branch(dir: &Path, branch: &str, content: &str) {
        Command::new("git")
            .args(["checkout", "-b", branch])
            .current_dir(dir)
            .output()
            .unwrap();
        std::fs::write(dir.join("file.txt"), content).unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", &format!("commit on {branch}")])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["checkout", "main"])
            .current_dir(dir)
            .output()
            .unwrap();
    }

    #[tokio::test]
    async fn op_start_merge_succeeds_on_clean_merge() {
        let tmp = init_repo();
        let dir = tmp.path();

        create_clean_branch(dir, "feature", "new_file.txt", "feature content\n");

        let params = OpStartParams {
            repo_id: "test".to_owned(),
            op: Op::Merge {
                from: "feature".to_owned(),
                into: "main".to_owned(),
                strategy: super::super::driver::MergeStrategy::MergeCommit,
                message: Some("merge feature".to_owned()),
            },
        };

        let result = op_start(dir, params).await.unwrap();
        assert_eq!(result["state"], "done");
        assert!(result["op_id"].as_str().is_some());
    }

    #[tokio::test]
    async fn op_start_merge_updates_inactive_target_without_checkout() {
        let tmp = init_repo();
        let dir = tmp.path();

        Command::new("git")
            .args(["branch", "integration", "main"])
            .current_dir(dir)
            .output()
            .unwrap();
        create_clean_branch(dir, "feature", "new_file.txt", "feature content\n");
        Command::new("git")
            .args(["checkout", "-b", "other", "main"])
            .current_dir(dir)
            .output()
            .unwrap();

        let params = OpStartParams {
            repo_id: "test".to_owned(),
            op: Op::Merge {
                from: "feature".to_owned(),
                into: "integration".to_owned(),
                strategy: super::super::driver::MergeStrategy::MergeCommit,
                message: Some("merge feature".to_owned()),
            },
        };

        let result = op_start(dir, params).await.unwrap();
        assert_eq!(result["state"], "done");

        let current = Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&current.stdout).trim(), "other");

        let ancestry = Command::new("git")
            .args(["merge-base", "--is-ancestor", "feature", "integration"])
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(ancestry.success());

        let show = Command::new("git")
            .args(["show", "integration:new_file.txt"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(show.status.success());
        assert!(
            !dir.join("new_file.txt").exists(),
            "inactive target merge must not materialize files in the current checkout"
        );
    }

    #[tokio::test]
    async fn op_start_merge_pauses_on_conflict() {
        let tmp = init_repo();
        let dir = tmp.path();

        // Create conflicting changes on two branches.
        create_conflicting_branch(dir, "conflict-branch", "conflict content\n");

        // Also modify file.txt on main so there's a real conflict.
        std::fs::write(dir.join("file.txt"), "main content\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "main diverge"])
            .current_dir(dir)
            .output()
            .unwrap();

        let params = OpStartParams {
            repo_id: "test".to_owned(),
            op: Op::Merge {
                from: "conflict-branch".to_owned(),
                into: "main".to_owned(),
                strategy: super::super::driver::MergeStrategy::MergeCommit,
                message: None,
            },
        };

        let result = op_start(dir, params).await.unwrap();
        assert_eq!(result["state"], "paused");
        assert!(result["conflicts"].as_array().is_some());
        let conflicts = result["conflicts"].as_array().unwrap();
        assert!(!conflicts.is_empty());
    }

    #[tokio::test]
    async fn op_abort_cleans_up() {
        let tmp = init_repo();
        let dir = tmp.path();

        create_conflicting_branch(dir, "conflict-branch", "conflict content\n");
        std::fs::write(dir.join("file.txt"), "main content\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "main diverge"])
            .current_dir(dir)
            .output()
            .unwrap();

        // Start a conflicting merge.
        let start_params = OpStartParams {
            repo_id: "test".to_owned(),
            op: Op::Merge {
                from: "conflict-branch".to_owned(),
                into: "main".to_owned(),
                strategy: super::super::driver::MergeStrategy::MergeCommit,
                message: None,
            },
        };
        let start_result = op_start(dir, start_params).await.unwrap();
        let op_id: Uuid = start_result["op_id"].as_str().unwrap().parse().unwrap();

        // Abort.
        let abort_params = OpAbortParams {
            repo_id: "test".to_owned(),
            op_id,
        };
        let abort_result = op_abort(dir, abort_params).await.unwrap();
        assert_eq!(abort_result["state"], "aborted");

        // Verify repo is clean (no conflicts).
        let conflicts = detect_conflicts(dir).await.unwrap();
        assert!(conflicts.is_empty());
    }

    #[tokio::test]
    async fn op_continue_after_resolution() {
        let tmp = init_repo();
        let dir = tmp.path();

        create_conflicting_branch(dir, "conflict-branch", "conflict content\n");
        std::fs::write(dir.join("file.txt"), "main content\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "main diverge"])
            .current_dir(dir)
            .output()
            .unwrap();

        // Start a conflicting merge.
        let start_params = OpStartParams {
            repo_id: "test".to_owned(),
            op: Op::Merge {
                from: "conflict-branch".to_owned(),
                into: "main".to_owned(),
                strategy: super::super::driver::MergeStrategy::MergeCommit,
                message: None,
            },
        };
        let start_result = op_start(dir, start_params).await.unwrap();
        let op_id: Uuid = start_result["op_id"].as_str().unwrap().parse().unwrap();
        assert_eq!(start_result["state"], "paused");

        // Resolve the conflict manually.
        std::fs::write(dir.join("file.txt"), "resolved content\n").unwrap();
        Command::new("git")
            .args(["add", "file.txt"])
            .current_dir(dir)
            .output()
            .unwrap();

        // Continue.
        let continue_params = OpContinueParams {
            repo_id: "test".to_owned(),
            op_id,
        };
        let continue_result = op_continue(dir, continue_params).await.unwrap();
        assert_eq!(continue_result["state"], "done");
    }

    #[tokio::test]
    async fn op_status_returns_current_op() {
        let tmp = init_repo();
        let dir = tmp.path();

        create_conflicting_branch(dir, "conflict-branch", "conflict content\n");
        std::fs::write(dir.join("file.txt"), "main content\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "main diverge"])
            .current_dir(dir)
            .output()
            .unwrap();

        // Start a conflicting merge (will pause).
        let start_params = OpStartParams {
            repo_id: "test".to_owned(),
            op: Op::Merge {
                from: "conflict-branch".to_owned(),
                into: "main".to_owned(),
                strategy: super::super::driver::MergeStrategy::MergeCommit,
                message: None,
            },
        };
        let start_result = op_start(dir, start_params).await.unwrap();
        let op_id: Uuid = start_result["op_id"].as_str().unwrap().parse().unwrap();

        // Query status without specifying op_id — should find the paused op.
        let status_params = OpStatusParams {
            repo_id: "test".to_owned(),
            op_id: None,
        };
        let status_result = op_status(dir, status_params).await.unwrap();
        assert_eq!(status_result["op_id"].as_str().unwrap(), op_id.to_string());
        assert_eq!(status_result["state"], "Paused");
        assert!(status_result["conflicts"].as_array().is_some());
    }

    #[test]
    fn format_todo_list_produces_expected_output() {
        let items = vec![
            TodoItem {
                sha: "abc1234".to_owned(),
                action: TodoAction::Pick,
                subject: "add feature".to_owned(),
            },
            TodoItem {
                sha: "def5678".to_owned(),
                action: TodoAction::Squash,
                subject: "fixup".to_owned(),
            },
            TodoItem {
                sha: "".to_owned(),
                action: TodoAction::Exec("make test".to_owned()),
                subject: "".to_owned(),
            },
        ];

        let output = format_todo_list(&items);
        assert_eq!(
            output,
            "pick abc1234 add feature\nsquash def5678 fixup\nexec make test\n"
        );
    }

    #[test]
    fn parse_conflict_paths_filters_correctly() {
        let porcelain = "UU file.txt\nM  other.txt\nAA both_added.txt\n?? untracked.txt\n";
        let conflicts = parse_conflict_paths(porcelain);
        assert_eq!(conflicts, vec!["file.txt", "both_added.txt"]);
    }

    // -----------------------------------------------------------------------
    // Crash-resume / mid-step drop tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn crash_resume_merge_paused_record_survives_reload() {
        let tmp = init_repo();
        let dir = tmp.path();

        // Set up a conflicting branch.
        create_conflicting_branch(dir, "conflict-branch", "conflict content\n");
        std::fs::write(dir.join("file.txt"), "main content\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "main diverge"])
            .current_dir(dir)
            .output()
            .unwrap();

        // Start a conflicting merge — will pause.
        let start_params = OpStartParams {
            repo_id: "test".to_owned(),
            op: Op::Merge {
                from: "conflict-branch".to_owned(),
                into: "main".to_owned(),
                strategy: super::super::driver::MergeStrategy::MergeCommit,
                message: None,
            },
        };
        let start_result = op_start(dir, start_params).await.unwrap();
        let op_id: Uuid = start_result["op_id"].as_str().unwrap().parse().unwrap();
        assert_eq!(start_result["state"], "paused");

        // Simulate crash: drop all in-memory state and reload from disk.
        let ops_dir = ops_dir_for_repo(dir);
        let records = load_all(&ops_dir).unwrap();
        assert_eq!(records.len(), 1);

        let loaded = &records[0];
        assert_eq!(loaded.id, op_id);
        assert_eq!(loaded.state, OpState::Paused);
        assert!(loaded.paused_reason.is_some());
        match &loaded.op {
            Op::Merge { from, .. } => assert_eq!(from, "conflict-branch"),
            _ => panic!("expected a Merge op after reload"),
        }

        // Resolve the conflict and continue from the reloaded state.
        std::fs::write(dir.join("file.txt"), "resolved content\n").unwrap();
        Command::new("git")
            .args(["add", "file.txt"])
            .current_dir(dir)
            .output()
            .unwrap();

        let continue_params = OpContinueParams {
            repo_id: "test".to_owned(),
            op_id: loaded.id,
        };
        let continue_result = op_continue(dir, continue_params).await.unwrap();
        assert_eq!(continue_result["state"], "done");
    }

    #[tokio::test]
    async fn crash_resume_rebase_in_progress_can_abort() {
        let tmp = init_repo();
        let dir = tmp.path();

        // Create a conflicting branch for rebase.
        create_conflicting_branch(dir, "rebase-branch", "rebase conflict\n");
        std::fs::write(dir.join("file.txt"), "main diverged\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "main diverge for rebase"])
            .current_dir(dir)
            .output()
            .unwrap();

        // Start a rebase that will conflict (pauses).
        let start_params = OpStartParams {
            repo_id: "test".to_owned(),
            op: Op::Rebase {
                onto: "main".to_owned(),
                branch: "rebase-branch".to_owned(),
                todo: None,
            },
        };
        let start_result = op_start(dir, start_params).await.unwrap();
        let op_id: Uuid = start_result["op_id"].as_str().unwrap().parse().unwrap();
        assert_eq!(start_result["state"], "paused");

        // Simulate crash: reload from disk.
        let ops_dir = ops_dir_for_repo(dir);
        let records = load_all(&ops_dir).unwrap();
        let loaded = records.iter().find(|r| r.id == op_id).unwrap();
        assert_eq!(loaded.state, OpState::Paused);

        // Abort using the reloaded op_id.
        let abort_params = OpAbortParams {
            repo_id: "test".to_owned(),
            op_id: loaded.id,
        };
        let abort_result = op_abort(dir, abort_params).await.unwrap();
        assert_eq!(abort_result["state"], "aborted");

        // Verify the repo is clean.
        let conflicts = detect_conflicts(dir).await.unwrap();
        assert!(conflicts.is_empty());

        // Verify the persisted record is Aborted.
        let records_after = load_all(&ops_dir).unwrap();
        let aborted = records_after.iter().find(|r| r.id == op_id).unwrap();
        assert_eq!(aborted.state, OpState::Aborted);
    }

    #[tokio::test]
    async fn mid_step_drop_does_not_corrupt_record() {
        let tmp = init_repo();
        let dir = tmp.path();

        // Set up a conflicting merge.
        create_conflicting_branch(dir, "conflict-branch", "conflict content\n");
        std::fs::write(dir.join("file.txt"), "main content\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "main diverge"])
            .current_dir(dir)
            .output()
            .unwrap();

        // Start a merge that pauses on conflict.
        let start_params = OpStartParams {
            repo_id: "test".to_owned(),
            op: Op::Merge {
                from: "conflict-branch".to_owned(),
                into: "main".to_owned(),
                strategy: super::super::driver::MergeStrategy::MergeCommit,
                message: None,
            },
        };
        let start_result = op_start(dir, start_params).await.unwrap();
        let op_id: Uuid = start_result["op_id"].as_str().unwrap().parse().unwrap();
        assert_eq!(start_result["state"], "paused");

        // Immediately load the record file and verify it's valid JSON
        // with the expected state — simulates reading after a "partial
        // write" scenario where the agent drops before any continue.
        let ops_dir = ops_dir_for_repo(dir);
        let record_path = ops_dir.join(format!("{op_id}.json"));
        assert!(record_path.exists());

        let raw = std::fs::read_to_string(&record_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["state"], "Paused");
        assert_eq!(parsed["id"], op_id.to_string());
        assert!(parsed["paused_reason"].is_string());

        // Deserialize into the typed struct to confirm full validity.
        let record: super::super::driver::OpRecord = serde_json::from_str(&raw).unwrap();
        assert_eq!(record.state, OpState::Paused);
        assert_eq!(record.id, op_id);
    }

    #[tokio::test]
    async fn multiple_ops_only_one_active() {
        let tmp = init_repo();
        let dir = tmp.path();

        // First: a clean merge that completes (Done).
        create_clean_branch(dir, "feature-a", "a.txt", "feature a\n");
        let start_params_a = OpStartParams {
            repo_id: "test".to_owned(),
            op: Op::Merge {
                from: "feature-a".to_owned(),
                into: "main".to_owned(),
                strategy: super::super::driver::MergeStrategy::MergeCommit,
                message: Some("merge feature-a".to_owned()),
            },
        };
        let result_a = op_start(dir, start_params_a).await.unwrap();
        assert_eq!(result_a["state"], "done");

        // Second: a conflicting merge that pauses.
        create_conflicting_branch(dir, "conflict-branch", "conflict content\n");
        std::fs::write(dir.join("file.txt"), "main diverged again\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(dir)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "main diverge again"])
            .current_dir(dir)
            .output()
            .unwrap();

        let start_params_b = OpStartParams {
            repo_id: "test".to_owned(),
            op: Op::Merge {
                from: "conflict-branch".to_owned(),
                into: "main".to_owned(),
                strategy: super::super::driver::MergeStrategy::MergeCommit,
                message: None,
            },
        };
        let result_b = op_start(dir, start_params_b).await.unwrap();
        let op_id_b: Uuid = result_b["op_id"].as_str().unwrap().parse().unwrap();
        assert_eq!(result_b["state"], "paused");

        // Load all records — verify exactly one Done, one Paused.
        let ops_dir = ops_dir_for_repo(dir);
        let records = load_all(&ops_dir).unwrap();
        assert_eq!(records.len(), 2);

        let done_count = records.iter().filter(|r| r.state == OpState::Done).count();
        let paused_count = records
            .iter()
            .filter(|r| r.state == OpState::Paused)
            .count();
        assert_eq!(done_count, 1);
        assert_eq!(paused_count, 1);

        // op_status without an op_id returns the Paused one.
        let status_params = OpStatusParams {
            repo_id: "test".to_owned(),
            op_id: None,
        };
        let status_result = op_status(dir, status_params).await.unwrap();
        assert_eq!(
            status_result["op_id"].as_str().unwrap(),
            op_id_b.to_string()
        );
        assert_eq!(status_result["state"], "Paused");
    }

    #[tokio::test]
    async fn worktree_cherry_pick_applies_commit_to_target_worktree() {
        let tmp = init_repo();
        let dir = tmp.path();

        // Create two worktrees: wt-source and wt-target.
        let wt_source = dir.join("wt-source");
        let wt_target = dir.join("wt-target");

        Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                "source-branch",
                wt_source.to_str().unwrap(),
            ])
            .current_dir(dir)
            .output()
            .unwrap();

        Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                "target-branch",
                wt_target.to_str().unwrap(),
            ])
            .current_dir(dir)
            .output()
            .unwrap();

        // Make a commit in the source worktree.
        std::fs::write(wt_source.join("cherry.txt"), "cherry content\n").unwrap();
        Command::new("git")
            .args(["add", "cherry.txt"])
            .current_dir(&wt_source)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "add cherry.txt"])
            .current_dir(&wt_source)
            .output()
            .unwrap();

        // Get the SHA of the commit we just made.
        let sha_output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&wt_source)
            .output()
            .unwrap();
        let sha = String::from_utf8_lossy(&sha_output.stdout)
            .trim()
            .to_owned();

        // Cherry-pick that commit into the target worktree.
        let params = WorktreeCherryPickParams {
            repo_id: "test".to_owned(),
            sha: sha.clone(),
            target_worktree_path: wt_target.to_string_lossy().into_owned(),
        };

        let result = worktree_cherry_pick(dir, params).await.unwrap();
        assert_eq!(result["state"], "done");
        assert_eq!(result["sha"], sha);

        // Verify the file exists in the target worktree.
        let cherry_file = wt_target.join("cherry.txt");
        assert!(cherry_file.exists());
        let content = std::fs::read_to_string(&cherry_file).unwrap();
        assert_eq!(content, "cherry content\n");
    }
}
