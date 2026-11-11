//! Identity and writer leases for disposable remote task workspaces.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::handlers::run_git_worktree_cmd as git;
use crate::{AgentError, Result};

const OWNER_FILE: &str = "crab-desktop-task-owner.json";
const APPLY_FENCE: &str = "crab-desktop-task-apply.pending";

#[derive(Deserialize, Serialize, PartialEq, Eq)]
struct Identity {
    root: PathBuf,
    common: PathBuf,
    git_dir: PathBuf,
}

#[derive(Deserialize, Serialize)]
struct Record {
    task_id: uuid::Uuid,
    branch: String,
    identity: Identity,
}

pub(crate) struct TaskWorkspace {
    record: Record,
    _lease: File,
}

pub(crate) struct Cleanup {
    pub branch_deleted: bool,
    pub error: Option<String>,
}

impl TaskWorkspace {
    pub(crate) async fn record(
        repo: &Path,
        root: &Path,
        task_id: uuid::Uuid,
        branch: &str,
    ) -> Result<()> {
        let identity = identity(repo, root).await?;
        verify_branch(root, branch).await?;
        let record = Record {
            task_id,
            branch: branch.into(),
            identity,
        };
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(record.identity.git_dir.join(OWNER_FILE))?;
        file.write_all(&serde_json::to_vec(&record)?)?;
        file.sync_all()?;
        Ok(())
    }

    pub(crate) async fn open(
        repo: &Path,
        root: &Path,
        task_id: Option<&str>,
        branch: Option<&str>,
    ) -> Result<Self> {
        let workspace = Self::open_for_apply(repo, root, task_id, branch).await?;
        require_no_pending_apply(&workspace.record.identity.git_dir)?;
        Ok(workspace)
    }

    pub(crate) async fn open_for_apply(
        repo: &Path,
        root: &Path,
        task_id: Option<&str>,
        branch: Option<&str>,
    ) -> Result<Self> {
        let identity = identity(repo, root).await?;
        let mut lease = owner_file(&identity.git_dir)?;
        lease.try_lock().map_err(lease_error)?;
        let record = read_record(&mut lease)?;
        let expected_id = task_id
            .map(uuid::Uuid::parse_str)
            .transpose()
            .map_err(|source| AgentError::Managed {
                kind: "task_identity".into(),
                message: "Remote workspace cleanup requires a task UUID".into(),
                remediation: "Recover the saved task identity before retrying".into(),
                source: Some(Box::new(source)),
            })?;
        if record.identity != identity
            || expected_id.is_some_and(|id| id != record.task_id)
            || branch.is_some_and(|branch| branch != record.branch)
        {
            return Err(AgentError::Other(
                "Remote workspace does not belong to this task, repository or branch".into(),
            ));
        }
        verify_branch(root, &record.branch).await?;
        Ok(Self {
            record,
            _lease: lease,
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.record.identity.root
    }

    #[cfg(unix)]
    pub(crate) fn apply_pending(&self) -> Result<bool> {
        match require_no_pending_apply(&self.record.identity.git_dir) {
            Ok(()) => Ok(false),
            Err(AgentError::Managed { .. }) => Ok(true),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn begin_apply(&self) -> Result<()> {
        // Publish the fence before launching any writer. The process lease
        // alone cannot protect a replacement sidecar after an abrupt exit.
        let git_dir = &self.record.identity.git_dir;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(git_dir.join(APPLY_FENCE))?
            .sync_all()?;
        #[cfg(unix)]
        File::open(git_dir)?.sync_all()?;
        Ok(())
    }

    pub(crate) fn finish_apply(&self) -> Result<()> {
        let git_dir = &self.record.identity.git_dir;
        match std::fs::remove_file(git_dir.join(APPLY_FENCE)) {
            Ok(()) => {
                #[cfg(unix)]
                File::open(git_dir)?.sync_all()?;
                Ok(())
            }
            // A successful worktree removal also removes its private Git dir.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) async fn remove(
        &self,
        repo: &Path,
        force: bool,
        delete_branch: bool,
    ) -> Result<Cleanup> {
        if !force {
            self.require_clean().await?;
        }
        // Git remains the removal authority. A locked, replaced or unregistered
        // checkout must fail without recursive filesystem cleanup or pruning.
        let manager = crate::worktree::WorktreeManager::new(repo.to_path_buf());
        manager.worktree_remove(self.root(), force).await?;
        if !delete_branch {
            return Ok(Cleanup {
                branch_deleted: false,
                error: None,
            });
        }
        let branch = &self.record.branch;
        match git(repo, &["branch", "-D", "--", branch]).await {
            Ok(_) => Ok(Cleanup {
                branch_deleted: true,
                error: None,
            }),
            Err(error) => {
                // Removal already happened. Preserve that outcome so callers
                // never misreport a completed merge/discard as unperformed.
                tracing::warn!(%error, %branch, "task workspace removed; branch retained");
                Ok(Cleanup {
                    branch_deleted: false,
                    error: Some(error.to_string()),
                })
            }
        }
    }

    async fn require_clean(&self) -> Result<()> {
        // Normal Git removal honors display settings and omits ignored files.
        // A landed merge cannot authorize deleting source data outside its commit.
        let status = git(
            self.root(),
            &[
                "--no-optional-locks",
                "status",
                "--porcelain",
                "--ignored",
                "--untracked-files=all",
                "--ignore-submodules=none",
            ],
        )
        .await?;
        if !status.is_empty() {
            return Err(AgentError::Other(
                "Workspace retained modified, untracked or ignored source files".into(),
            ));
        }
        let mut command = crate::shellout::git_command();
        let output = crate::shellout::output(command.current_dir(self.root()).args([
            "ls-files",
            "-v",
            "-z",
            "--full-name",
        ]))
        .await?;
        if !output.status.success() {
            return Err(AgentError::Cli {
                status: output.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        let entries = String::from_utf8(output.stdout)?;
        for entry in entries.split_terminator('\0') {
            let (tag, name) = entry
                .split_once(' ')
                .ok_or_else(|| AgentError::Other("Git returned an invalid index entry".into()))?;
            if tag != "S" && !tag.as_bytes().first().is_some_and(u8::is_ascii_lowercase) {
                continue;
            }
            // Sparse omissions contain no bytes to preserve. A materialized
            // excluded file may hide edits from both status and ordinary removal.
            let path = self.root().join(name);
            match std::fs::symlink_metadata(path) {
                Ok(_) => {
                    return Err(AgentError::Other(
                        "Workspace retained files excluded by index flags".into(),
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

fn require_no_pending_apply(git_dir: &Path) -> Result<()> {
    match std::fs::symlink_metadata(git_dir.join(APPLY_FENCE)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => Err(AgentError::Managed {
            kind: "task_apply_pending".into(),
            message: "An interrupted Apply needs recovery".into(),
            remediation: "Confirm its Git writers have stopped and inspect the worktree and target branch before retrying".into(),
            source: None,
        }),
    }
}

async fn identity(repo: &Path, root: &Path) -> Result<Identity> {
    if !root.is_absolute() {
        return Err(AgentError::Other(
            "Remote workspace path must be absolute".into(),
        ));
    }
    let canonical = tokio::fs::canonicalize(root).await?;
    let identity = Identity {
        root: directory(root, "--show-toplevel").await?,
        common: directory(root, "--git-common-dir").await?,
        git_dir: directory(root, "--absolute-git-dir").await?,
    };
    let repository = directory(repo, "--git-common-dir").await?;
    if identity.root != canonical
        || identity.common != repository
        || identity.git_dir == identity.common
    {
        return Err(AgentError::Other(
            "Remote task requires its registered linked worktree in the opened repository".into(),
        ));
    }
    Ok(identity)
}

async fn directory(root: &Path, option: &str) -> Result<PathBuf> {
    let output = git(root, &["rev-parse", option]).await?;
    Ok(tokio::fs::canonicalize(root.join(output)).await?)
}

async fn verify_branch(root: &Path, branch: &str) -> Result<()> {
    let current = git(root, &["symbolic-ref", "HEAD"]).await?;
    if current != format!("refs/heads/{branch}") {
        return Err(AgentError::Other(
            "Remote workspace branch changed; recover its ownership before cleanup".into(),
        ));
    }
    Ok(())
}

fn owner_file(git_dir: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options
        .open(git_dir.join(OWNER_FILE))
        .map_err(|source| AgentError::Managed {
            kind: "task_workspace_owner".into(),
            message: "Remote workspace ownership is unavailable".into(),
            remediation: "Preserve the checkout and recover its task ownership before cleanup"
                .into(),
            source: Some(Box::new(source)),
        })
}

fn read_record(file: &mut File) -> Result<Record> {
    let mut bytes = Vec::new();
    file.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 16 * 1024 {
        return Err(AgentError::Other(
            "Remote workspace ownership record exceeds its limit".into(),
        ));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn lease_error(source: std::fs::TryLockError) -> AgentError {
    if let std::fs::TryLockError::Error(error) = source {
        return error.into();
    }
    AgentError::Managed {
        kind: "task_workspace_busy".into(),
        message: "Remote workspace still has an active writer or lifecycle operation".into(),
        remediation: "Stop its native processes and wait for workspace operations before retrying"
            .into(),
        source: Some(Box::new(source)),
    }
}

pub(crate) async fn writer_lease(root: &Path, task_id: Option<uuid::Uuid>) -> Result<Option<File>> {
    let pointer = match tokio::fs::read_to_string(root.join(".git")).await {
        Ok(pointer) => pointer,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                || error.kind() == std::io::ErrorKind::IsADirectory =>
        {
            if task_id.is_none() {
                return Ok(None);
            }
            return Err(error.into());
        }
        Err(error) => return Err(error.into()),
    };
    let git_dir = pointer
        .trim()
        .strip_prefix("gitdir:")
        .ok_or_else(|| AgentError::Other("Invalid workspace Git directory pointer".into()))?;
    let git_dir = tokio::fs::canonicalize(root.join(git_dir.trim())).await?;
    if task_id.is_none() && !tokio::fs::try_exists(git_dir.join(OWNER_FILE)).await? {
        return Ok(None);
    }
    let mut lease = owner_file(&git_dir)?;
    lease.try_lock_shared().map_err(lease_error)?;
    let record = read_record(&mut lease)?;
    if record.identity.git_dir != git_dir
        || record.identity.root != tokio::fs::canonicalize(root).await?
        || task_id.is_some_and(|id| id != record.task_id)
    {
        return Err(AgentError::Other(
            "Native process workspace ownership changed".into(),
        ));
    }
    verify_branch(root, &record.branch).await?;
    require_no_pending_apply(&git_dir)?;
    Ok(Some(lease))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn non_force_cleanup_preserves_materialized_excluded_files() {
        for (flag, materialized) in [
            ("--assume-unchanged", true),
            ("--skip-worktree", true),
            ("--skip-worktree", false),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let repo = directory.path().join("repo");
            let root = directory.path().join("source");
            std::fs::create_dir(&repo).unwrap();
            git(&repo, &["init", "--template=", "--initial-branch=main"])
                .await
                .unwrap();
            std::fs::write(repo.join("file.txt"), "before\n").unwrap();
            git(&repo, &["add", "file.txt"]).await.unwrap();
            git(
                &repo,
                &[
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "-c",
                    "commit.gpgSign=false",
                    "commit",
                    "-m",
                    "baseline",
                ],
            )
            .await
            .unwrap();
            git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "agent/source",
                    root.to_str().unwrap(),
                ],
            )
            .await
            .unwrap();
            let id = uuid::Uuid::new_v4();
            TaskWorkspace::record(&repo, &root, id, "agent/source")
                .await
                .unwrap();
            git(&root, &["update-index", flag, "file.txt"])
                .await
                .unwrap();
            if materialized {
                std::fs::write(root.join("file.txt"), "private editor bytes\n").unwrap();
            } else {
                std::fs::remove_file(root.join("file.txt")).unwrap();
            }
            let workspace =
                TaskWorkspace::open(&repo, &root, Some(&id.to_string()), Some("agent/source"))
                    .await
                    .unwrap();
            let result = workspace.remove(&repo, false, false).await;
            assert_eq!(
                (result.is_err(), root.exists()),
                (materialized, materialized),
                "{flag}, materialized={materialized}"
            );
        }
    }
}
