use std::path::Path;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};

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

pub(crate) async fn require_direct_branch(work_dir: &Path, branch_ref: &str) -> Result<()> {
    let output = git_output(
        work_dir,
        &["symbolic-ref", "-q", "--no-recurse", branch_ref],
    )
    .await?;
    match output.status {
        1 => Ok(()),
        0 => Err(AgentError::Managed {
            kind: "task_apply_target".into(),
            message: format!("Apply target '{branch_ref}' is a symbolic branch ref"),
            remediation: "Restore the original direct branch before applying changes".into(),
            source: None,
        }),
        status => Err(AgentError::Cli {
            status,
            stderr: output.combined_output(),
        }),
    }
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

    git_stdout(work_dir, &["check-ref-format", &target_ref]).await?;
    require_direct_branch(work_dir, &target_ref).await?;

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

    let ancestry = git_output(
        work_dir,
        &["merge-base", "--is-ancestor", &source_sha, &target_sha],
    )
    .await?;
    match ancestry.status {
        0 => {
            if !publish_inactive_ref(work_dir, &target_ref, &target_sha, None, message).await? {
                return Ok(RefMergeOutcome::TargetCheckedOut {
                    target_branch,
                    target_ref,
                });
            }
            return Ok(RefMergeOutcome::AlreadyUpToDate {
                target_branch,
                target_ref,
                commit: target_sha,
            });
        }
        1 => {}
        status => {
            return Err(AgentError::Cli {
                status,
                stderr: ancestry.combined_output(),
            });
        }
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

    if !publish_inactive_ref(
        work_dir,
        &target_ref,
        &target_sha,
        Some(&merge_commit),
        message,
    )
    .await?
    {
        return Ok(RefMergeOutcome::TargetCheckedOut {
            target_branch,
            target_ref,
        });
    }

    Ok(RefMergeOutcome::Applied {
        target_branch,
        target_ref,
        merge_commit,
    })
}

async fn publish_inactive_ref(
    work_dir: &Path,
    target_ref: &str,
    old: &str,
    next: Option<&str>,
    message: &str,
) -> Result<bool> {
    let edit = match next {
        Some(next) => format!("update {target_ref} {next} {old}"),
        None => format!("verify {target_ref} {old}"),
    };
    let mut command = crate::shellout::git_command();
    command
        .args(["update-ref", "--stdin", "-m", message])
        .current_dir(work_dir)
        .stdin(Stdio::piped());
    let (status, result, stderr) = crate::shellout::piped(&mut command, async |input, output| {
        let mut input = input
            .ok_or_else(|| std::io::Error::other("Git reference transaction stdin unavailable"))?;
        let mut output = BufReader::new(output);
        transaction_step(&mut input, &mut output, "start\n", "start: ok").await?;
        let prepare = format!("option no-deref\n{edit}\nprepare\n");
        transaction_step(&mut input, &mut output, &prepare, "prepare: ok").await?;
        // Git compares the old OID of a symbolic ref through its referent.
        // Inspect the ref after prepare has locked its exact name.
        let validation = async {
            require_direct_branch(work_dir, target_ref).await?;
            Ok(!branch_is_checked_out(work_dir, target_ref).await?)
        }
        .await;
        if matches!(validation, Ok(true)) {
            transaction_step(&mut input, &mut output, "commit\n", "commit: ok").await?;
        } else {
            transaction_step(&mut input, &mut output, "abort\n", "abort: ok").await?;
        }
        validation
    })
    .await?;
    if !status.success() {
        return Err(AgentError::Cli {
            status: status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        });
    }
    result
}

async fn transaction_step(
    input: &mut ChildStdin,
    output: &mut BufReader<ChildStdout>,
    command: &str,
    expected: &str,
) -> Result<()> {
    input.write_all(command.as_bytes()).await?;
    let mut reply = String::new();
    output.read_line(&mut reply).await?;
    if reply.trim_end() != expected {
        return Err(AgentError::Other(format!(
            "Git reference transaction did not acknowledge {expected}"
        )));
    }
    Ok(())
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
    let mut command = crate::shellout::git_command();
    let output = crate::shellout::output(command.args(args).current_dir(work_dir)).await?;

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

    #[tokio::test]
    async fn prepared_transaction_rejects_symbolic_destination_with_matching_oid() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        git_stdout(cwd, &["init", "--template=", "--initial-branch=referent"])
            .await
            .unwrap();
        git_stdout(cwd, &["config", "user.name", "Fixture"])
            .await
            .unwrap();
        git_stdout(cwd, &["config", "user.email", "fixture@example.invalid"])
            .await
            .unwrap();
        git_stdout(
            cwd,
            &["commit", "--allow-empty", "--no-gpg-sign", "-m", "baseline"],
        )
        .await
        .unwrap();
        let old = git_stdout(cwd, &["rev-parse", "HEAD"]).await.unwrap();
        let tree = git_stdout(cwd, &["rev-parse", "HEAD^{tree}"])
            .await
            .unwrap();
        let next = git_stdout(
            cwd,
            &[
                "commit-tree",
                tree.trim(),
                "-p",
                old.trim(),
                "-m",
                "candidate",
            ],
        )
        .await
        .unwrap();
        let saved = "refs/heads/saved";
        let referent = "refs/heads/referent";
        git_stdout(cwd, &["symbolic-ref", saved, referent])
            .await
            .unwrap();
        let error = publish_inactive_ref(cwd, saved, old.trim(), Some(next.trim()), "fixture")
            .await
            .unwrap_err();
        assert!(matches!(error, AgentError::Managed { kind, .. } if kind == "task_apply_target"));
        assert_eq!(
            git_stdout(cwd, &["rev-parse", referent]).await.unwrap(),
            old
        );
        assert_eq!(
            git_stdout(cwd, &["symbolic-ref", "--no-recurse", saved])
                .await
                .unwrap()
                .trim(),
            referent
        );
    }
}
