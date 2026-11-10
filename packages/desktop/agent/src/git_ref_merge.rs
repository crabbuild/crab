use std::path::Path;

use crate::{AgentError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RefMergeOutcome {
    Applied {
        target_branch: String,
        target_ref: String,
        merge_commit: String,
    },
    AlreadyUpToDate {
        target_branch: String,
        target_ref: String,
        commit: String,
    },
    TargetCheckedOut {
        target_branch: String,
        target_ref: String,
    },
}

pub(crate) fn local_branch_ref(branch: &str) -> String {
    if branch.starts_with("refs/heads/") {
        branch.to_owned()
    } else {
        format!("refs/heads/{branch}")
    }
}

pub(crate) async fn current_branch_ref(work_dir: &Path) -> Result<Option<String>> {
    let output = git_output(work_dir, &["symbolic-ref", "-q", "HEAD"]).await?;
    if output.status == 1 {
        return Ok(None);
    }
    if output.status != 0 {
        return Err(AgentError::Cli {
            status: output.status,
            stderr: output.combined_output(),
        });
    }
    Ok(Some(output.stdout.trim().to_owned()))
}

pub(crate) async fn merge_commit_without_checkout(
    work_dir: &Path,
    from: &str,
    into: &str,
    message: &str,
) -> Result<RefMergeOutcome> {
    let target_ref = local_branch_ref(into);
    let target_branch = target_ref
        .strip_prefix("refs/heads/")
        .unwrap_or(&target_ref)
        .to_owned();

    if branch_is_checked_out(work_dir, &target_ref).await? {
        return Ok(RefMergeOutcome::TargetCheckedOut {
            target_branch,
            target_ref,
        });
    }

    let target_sha = git_stdout(
        work_dir,
        &["rev-parse", &format!("{target_ref}^{{commit}}")],
    )
    .await?
    .trim()
    .to_owned();
    let source_sha = git_stdout(work_dir, &["rev-parse", &format!("{from}^{{commit}}")])
        .await?
        .trim()
        .to_owned();

    if git_status(
        work_dir,
        &["merge-base", "--is-ancestor", &source_sha, &target_sha],
    )
    .await?
        == 0
    {
        return Ok(RefMergeOutcome::AlreadyUpToDate {
            target_branch,
            target_ref,
            commit: target_sha,
        });
    }

    let merge_tree = git_output(
        work_dir,
        &["merge-tree", "--write-tree", &target_sha, &source_sha],
    )
    .await?;
    if merge_tree.status != 0 {
        let conflicts = merge_tree_conflict_paths(work_dir, &target_sha, &source_sha).await?;
        let detail = merge_tree.combined_output();
        let conflict_detail = if conflicts.is_empty() {
            detail
        } else {
            conflicts.join(", ")
        };
        return Err(AgentError::Other(format!(
            "Merge would conflict when applying '{from}' into inactive target branch '{target_branch}': {conflict_detail}. Switch to the target branch worktree to resolve it."
        )));
    }
    let tree = merge_tree.stdout.trim();
    if tree.is_empty() {
        return Err(AgentError::Other(
            "git merge-tree did not return a tree id".to_owned(),
        ));
    }

    let merge_commit = git_stdout(
        work_dir,
        &[
            "commit-tree",
            tree,
            "-p",
            &target_sha,
            "-p",
            &source_sha,
            "-m",
            message,
        ],
    )
    .await?
    .trim()
    .to_owned();

    git_stdout(
        work_dir,
        &[
            "update-ref",
            "-m",
            message,
            &target_ref,
            &merge_commit,
            &target_sha,
        ],
    )
    .await?;

    Ok(RefMergeOutcome::Applied {
        target_branch,
        target_ref,
        merge_commit,
    })
}

async fn branch_is_checked_out(work_dir: &Path, branch_ref: &str) -> Result<bool> {
    let list = git_stdout(work_dir, &["worktree", "list", "--porcelain"]).await?;
    Ok(list
        .lines()
        .any(|line| line.strip_prefix("branch ") == Some(branch_ref)))
}

async fn merge_tree_conflict_paths(
    work_dir: &Path,
    target_sha: &str,
    source_sha: &str,
) -> Result<Vec<String>> {
    let output = git_output(
        work_dir,
        &[
            "merge-tree",
            "--write-tree",
            "--name-only",
            target_sha,
            source_sha,
        ],
    )
    .await?;

    let text = output.combined_output();
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| {
            !line.is_empty()
                && !(line.len() == 40 && line.chars().all(|c| c.is_ascii_hexdigit()))
                && !line.starts_with("Auto-merging ")
                && !line.starts_with("CONFLICT")
                && !line.contains('\t')
        })
        .map(str::to_owned)
        .collect())
}

async fn git_status(work_dir: &Path, args: &[&str]) -> Result<i32> {
    Ok(git_output(work_dir, args).await?.status)
}

async fn git_stdout(work_dir: &Path, args: &[&str]) -> Result<String> {
    let output = git_output(work_dir, args).await?;
    if output.status != 0 {
        return Err(AgentError::Cli {
            status: output.status,
            stderr: output.combined_output(),
        });
    }
    Ok(output.stdout)
}

struct GitOutput {
    status: i32,
    stdout: String,
    stderr: String,
}

impl GitOutput {
    fn combined_output(&self) -> String {
        let combined = [self.stdout.trim(), self.stderr.trim()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if combined.is_empty() {
            format!("git exited with status {}", self.status)
        } else {
            combined
        }
    }
}

async fn git_output(work_dir: &Path, args: &[&str]) -> Result<GitOutput> {
    let output = crate::shellout::git_command()
        .args(args)
        .current_dir(work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;

    Ok(GitOutput {
        status: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn current_branch_preserves_repository_errors() {
        let directory = tempfile::tempdir().unwrap();
        let error = current_branch_ref(directory.path()).await.unwrap_err();
        assert!(matches!(error, AgentError::Cli { status: 128, .. }));
    }
}
