//! Publish a checked-out task merge under Git's index and ref locks.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Output;

use serde::{Deserialize, Serialize};

use crate::git_ref_merge::{
    current_direct_branch_ref, exact_branch_oid, is_ancestor, publish_active_ref,
};
use crate::handlers::run_git_worktree_cmd as git;
use crate::native_process::mutation::{requires_recovery, retain_recovery, writers_confirmed};
use crate::{AgentError, Result, shellout};

const JOURNAL: &str = "crab-desktop-active-apply";
const PLANNING: &str = "planning";
const STAGED_BEFORE: &str = "staged-before.patch";
const CHECKOUT_BEFORE: [&str; 3] = ["status-before", "unstaged-before.patch", STAGED_BEFORE];
const WRITERS_CLOSED: &str = "writers-closed";

mod hooks;
mod reconcile;
pub(crate) use hooks::run_if_hook;
pub(crate) use reconcile::recover;

#[derive(Deserialize, Serialize)]
struct Intent {
    worktree: PathBuf,
    source_worktree: PathBuf,
    source: String,
    target: String,
    old: String,
    index: PathBuf,
    hooks: PathBuf,
    lock_device: u64,
    lock_inode: u64,
}

struct Stage<'a> {
    root: &'a Path,
    common: &'a Path,
    dir: PathBuf,
    index: PathBuf,
    lock_path: PathBuf,
    lock: Option<File>,
}

pub(crate) async fn require_available(root: &Path) -> Result<()> {
    let journal = common_dir(root).await?.join(JOURNAL);
    if journal.try_exists()? {
        return Err(pending(&journal, None));
    }
    Ok(())
}

async fn common_dir(root: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(
        git(
            root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await?,
    ))
}

pub(crate) async fn merge(
    root: &Path,
    target: &str,
    source: &str,
    source_worktree: &Path,
    message: &str,
) -> Result<String> {
    let old = exact_branch_oid(root, target).await?;
    let git_dir = PathBuf::from(git(root, &["rev-parse", "--absolute-git-dir"]).await?);
    let index = git_dir.join("index");
    let actual_index = git(
        root,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    )
    .await?;
    if Path::new(&actual_index) != index {
        return Err(AgentError::Other(
            "Apply requires the checkout's Git index".into(),
        ));
    }
    let common = common_dir(root).await?;
    let dir = common.join(JOURNAL);
    fs::create_dir(&dir).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            pending(&dir, Some(error.into()))
        } else {
            error.into()
        }
    })?;
    let mut stage = Stage {
        root,
        common: &common,
        dir,
        index,
        lock_path: git_dir.join("index.lock"),
        lock: None,
    };

    let setup = async {
        stage
            .prepare(&git_dir, target, source, source_worktree, &old)
            .await?;
        let before = stage.checkout_state().await?;
        for (name, bytes) in CHECKOUT_BEFORE.iter().zip(&before) {
            if !bytes.is_empty() {
                write_synced(&stage.dir.join(name), bytes)?;
            }
        }
        File::open(&stage.dir)?.sync_all()?;
        Ok(before)
    }
    .await;
    let before = match setup {
        Ok(before) => before,
        Err(error) => {
            return match stage.cleanup() {
                Ok(()) => Err(error),
                Err(cleanup) => Err(recovery(&stage.dir, cleanup)),
            };
        }
    };

    match stage.publish(target, source, &old, message, before).await {
        Ok(commit) => {
            stage
                .cleanup()
                .map_err(|error| recovery(&stage.dir, error))?;
            Ok(commit)
        }
        Err(error) => {
            if stage.dir.try_exists()? && writers_confirmed() {
                record_closed_writers(&stage.dir).map_err(|error| recovery(&stage.dir, error))?;
            }
            Err(recovery(&stage.dir, error))
        }
    }
}

impl Stage<'_> {
    async fn prepare(
        &mut self,
        git_dir: &Path,
        target: &str,
        source: &str,
        source_worktree: &Path,
        old: &str,
    ) -> Result<()> {
        self.lock = Some(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&self.lock_path)?,
        );
        let owner = self.lock()?.metadata()?;
        for path in [
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "rebase-merge",
            "rebase-apply",
            "sequencer",
        ] {
            if git_dir.join(path).try_exists()? {
                return Err(AgentError::Other(
                    "Finish the checkout's existing Git operation before Apply".into(),
                ));
            }
        }
        if current_direct_branch_ref(self.root).await?.as_deref() != Some(target)
            || exact_branch_oid(self.root, target).await? != old
        {
            return Err(target_changed(target));
        }

        initialize_private_head(self.root, &self.dir, self.common, old).await?;
        for path in ["index", "config.worktree", "info/sparse-checkout"] {
            let from = git_dir.join(path);
            if from.try_exists()? {
                let to = self.dir.join(path);
                if path.starts_with("info/") {
                    fs::create_dir(self.dir.join("info"))?;
                }
                copy_synced(&from, &to)?;
                if path == "index" {
                    copy_synced(&from, &self.dir.join("original-index"))?;
                    self.lock()?
                        .set_permissions(fs::metadata(&from)?.permissions())?;
                }
            }
        }
        for entry in fs::read_dir(git_dir)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("sharedindex.")
            {
                symlink(entry.path(), self.dir.join(entry.file_name()))?;
            }
        }
        let hooks = git(
            self.root,
            &["rev-parse", "--path-format=absolute", "--git-path", "hooks"],
        )
        .await?;
        let intent = Intent {
            worktree: self.root.to_owned(),
            source_worktree: source_worktree.to_owned(),
            source: source.to_owned(),
            target: target.to_owned(),
            old: old.to_owned(),
            index: self.index.clone(),
            hooks: hooks.into(),
            lock_device: owner.dev(),
            lock_inode: owner.ino(),
        };
        write_synced(&self.dir.join("intent.json"), &serde_json::to_vec(&intent)?)?;
        File::open(&self.dir)?.sync_all()?;
        File::open(git_dir)?.sync_all()?;
        if self.common != git_dir {
            File::open(self.common)?.sync_all()?;
        }
        Ok(())
    }

    async fn publish(
        &mut self,
        target: &str,
        source: &str,
        old: &str,
        message: &str,
        before: [Vec<u8>; 3],
    ) -> Result<String> {
        let next = match self
            .merge_in_planning_checkout(target, old, source, message)
            .await
        {
            Ok(Some(next)) => next,
            Ok(None) => return Ok(old.to_owned()),
            Err(error) => {
                if !requires_recovery() {
                    // A failed protected checkout can leave its private HEAD
                    // unchanged. Compare the full preimage before clearing it.
                    if !self.dir.join("proposal").try_exists()? {
                        self.cleanup()?;
                    } else if self.shadow_ok(&["rev-parse", "--verify", "HEAD"]).await? != old {
                        self.rollback(old, &before).await?;
                    } else if self.checkout_state().await? == before {
                        self.cleanup()?;
                    }
                }
                return Err(error);
            }
        };

        // A split shadow index could refer to shared-index files in the journal.
        // Materialize it before preparing the real index replacement.
        self.shadow_ok(&["update-index", "--no-split-index"])
            .await?;
        let mut index = File::open(self.dir.join("index"))?;
        let mut lock = self.lock()?;
        lock.seek(SeekFrom::Start(0))?;
        io::copy(&mut index, &mut lock)?;
        lock.sync_all()?;
        write_synced(&self.dir.join("candidate"), next.as_bytes())?;
        File::open(&self.dir)?.sync_all()?;
        self.check_lock()?;

        if !publish_active_ref(self.root, target, old, &next, message).await? {
            self.rollback(old, &before).await?;
            return Err(target_changed(target));
        }
        self.check_lock()?;
        fs::rename(&self.lock_path, &self.index)?;
        self.lock.take();
        if let Some(parent) = self.index.parent() {
            File::open(parent)?.sync_all()?;
        }

        let mut hook = shellout::git_command()?;
        hook.current_dir(self.root).args([
            "hook",
            "run",
            "--ignore-missing",
            "post-merge",
            "--",
            "0",
        ]);
        let output = shellout::output(&mut hook).await?;
        if requires_recovery() {
            return Err(AgentError::Other(
                "Apply stopped after publishing the target".into(),
            ));
        }
        if !output.status.success() {
            tracing::warn!(error = %command_error(output), "post-merge hook failed");
        }
        Ok(next)
    }

    async fn merge_in_planning_checkout(
        &self,
        target: &str,
        old: &str,
        source: &str,
        message: &str,
    ) -> Result<Option<String>> {
        let planning = self.dir.join(PLANNING);
        let worktree = planning.join("worktree");
        fs::create_dir_all(planning.join("info"))?;
        fs::create_dir(&worktree)?;
        initialize_private_head(&worktree, &planning, self.common, old).await?;
        write_synced(&planning.join("info/sparse-checkout"), b"!/*\n")?;
        let config = self.dir.join("config.worktree");
        if config.try_exists()? {
            copy_synced(&config, &planning.join("config.worktree"))?;
        }
        let merge_config = self.merge_config(target).await?;
        let hook_path = hooks::install(&planning)?;
        let hook_config = format!("core.hooksPath={}", hook_path.display());
        let named_hooks = hooks::disabled_named_hooks(&worktree, &planning).await?;
        let planning_git = async |args: &[&str]| {
            let mut command = private_git(&worktree, &planning)?;
            hooks::save_git_config(&mut command);
            command.args([
                "-c",
                "core.sparseCheckout=true",
                "-c",
                "core.sparseCheckoutCone=false",
                "-c",
                "index.sparse=false",
                "-c",
                &hook_config,
                "-c",
                "maintenance.auto=false",
                "-c",
                "gc.auto=0",
                "-c",
                "hook.prepare-commit-msg.enabled=true",
                "-c",
                "hook.reference-transaction.enabled=false",
                "-c",
                "hook.post-merge.enabled=false",
                "-c",
                "hook.post-index-change.enabled=false",
            ]);
            for setting in merge_config.iter().chain(&named_hooks) {
                command.args(["-c", setting]);
            }
            command.args(args);
            let output = shellout::output(&mut command).await?;
            output_text(output)
        };

        // Git owns merge options, commit construction and hook order. The
        // first commit hook projects its result with the target index locked.
        planning_git(&["read-tree", "-m", "-u", old]).await?;
        planning_git(&["merge", "--no-ff", "--no-edit", "-m", message, source]).await?;
        let next = planning_git(&["rev-parse", "--verify", "HEAD"]).await?;
        if next == old {
            if is_ancestor(self.root, source, old).await? {
                return Ok(None);
            }
            return Err(AgentError::Other(
                "Configured Git merge did not commit the task merge".into(),
            ));
        }
        let proposal = fs::read_to_string(self.dir.join("proposal"))?;
        let projected = self.shadow_ok(&["rev-parse", "--verify", "HEAD"]).await?;
        let projected_tree = self
            .shadow_ok(&["rev-parse", "--verify", "HEAD^{tree}"])
            .await?;
        let merged_tree = planning_git(&["rev-parse", "--verify", "HEAD^{tree}"]).await?;
        let parents = planning_git(&["rev-list", "--parents", "-n1", "HEAD"]).await?;
        if projected != proposal
            || projected_tree != merged_tree
            || !parents
                .split_ascii_whitespace()
                .eq([next.as_str(), old, source])
        {
            return Err(AgentError::Other(
                "Git's merge commit does not match the projected checkout".into(),
            ));
        }
        Ok(Some(next))
    }

    async fn merge_config(&self, target: &str) -> Result<Vec<String>> {
        let branch = target.strip_prefix("refs/heads/").unwrap_or(target);
        let options = self
            .config_value(&format!("branch.{branch}.mergeOptions"), &[])
            .await?;
        let mut settings = vec![format!("branch.HEAD.mergeOptions={options}")];
        for key in ["merge.verifySignatures", "commit.gpgSign"] {
            let enabled = self.config_value(key, &["--type=bool"]).await? == "true";
            settings.push(format!("{key}={enabled}"));
        }

        // Conditional includes resolve in the real checkout. Carry its merge
        // and signing policy into Git's detached, empty planning checkout.
        let mut command = shellout::git_command()?;
        command.current_dir(self.root).args([
            "config",
            "--null",
            "--get-regexp",
            r"^(gpg\..+|user\.(name|email|signingkey)|commit\.cleanup|pull\.twohead|merge\..+)$",
        ]);
        let output = shellout::output(&mut command).await?;
        if output.status.code() == Some(1) {
            return Ok(settings);
        }
        if !output.status.success() {
            return Err(command_error(output));
        }
        let raw = String::from_utf8(output.stdout)?;
        let ssh = self.config_value("gpg.format", &[]).await? == "ssh";
        for entry in raw.split_terminator('\0') {
            let Some((key, value)) = entry.split_once('\n') else {
                settings.push(entry.to_owned());
                continue;
            };
            let signing_path = key == "user.signingkey"
                && ssh
                && !value.starts_with("key::")
                && !value.starts_with("ssh-");
            let trust_path = matches!(key, "gpg.ssh.allowedsignersfile" | "gpg.ssh.revocationfile");
            let program_path =
                key.starts_with("gpg.") && key.ends_with(".program") && value.contains('/');
            if !value.is_empty() && (signing_path || trust_path || program_path) {
                let resolved = if program_path {
                    value.to_owned()
                } else {
                    self.config_value(key, &["--type=path"]).await?
                };
                settings.push(format!("{key}={}", self.root.join(resolved).display()));
            } else {
                settings.push(format!("{key}={value}"));
            }
        }
        Ok(settings)
    }

    async fn config_value(&self, key: &str, options: &[&str]) -> Result<String> {
        let mut command = shellout::git_command()?;
        command
            .current_dir(self.root)
            .args(["config", "--null"])
            .args(options)
            .args(["--get", key]);
        let output = shellout::output(&mut command).await?;
        match output.status.code() {
            Some(0) => Ok(String::from_utf8(output.stdout)?
                .trim_end_matches('\0')
                .to_owned()),
            Some(1) => Ok(String::new()),
            _ => Err(command_error(output)),
        }
    }

    async fn checkout(&self, revision: &str) -> Result<Output> {
        self.shadow(&["checkout", "--no-overwrite-ignore", "--detach", revision])
            .await
    }

    async fn rollback(&mut self, old: &str, before: &[Vec<u8>; 3]) -> Result<()> {
        if self.restore_checkout(old, before).await? {
            self.cleanup()?;
        }
        Ok(())
    }

    async fn restore_checkout(&self, old: &str, before: &[Vec<u8>; 3]) -> Result<bool> {
        // Git refuses to overwrite an intervening edit or ignored file. Keep
        // the journal and index lock if the original state cannot be proved.
        let output = self.checkout(old).await?;
        if !output.status.success() {
            return Err(command_error(output));
        }
        Ok(self.checkout_state().await? == *before)
    }

    async fn checkout_state(&self) -> Result<[Vec<u8>; 3]> {
        let mut state = [Vec::new(), Vec::new(), Vec::new()];
        let commands: &[&[&str]] = &[
            &[
                "--no-optional-locks",
                "status",
                "--porcelain=v2",
                "-z",
                "--untracked-files=all",
            ],
            &[
                "--no-optional-locks",
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--binary",
                "--full-index",
                "--no-renames",
            ],
            &[
                "--no-optional-locks",
                "diff",
                "--cached",
                "--no-ext-diff",
                "--no-textconv",
                "--binary",
                "--full-index",
                "--no-renames",
            ],
        ];
        for (bytes, args) in state.iter_mut().zip(commands) {
            let output = self.shadow(args).await?;
            if !output.status.success() {
                return Err(command_error(output));
            }
            *bytes = output.stdout;
        }
        Ok(state)
    }

    async fn shadow(&self, args: &[&str]) -> Result<Output> {
        let mut command = projection_git(self.root, &self.dir)?;
        command.args(args);
        shellout::output(&mut command).await
    }

    async fn shadow_ok(&self, args: &[&str]) -> Result<String> {
        output_text(self.shadow(args).await?)
    }

    fn lock(&self) -> Result<&File> {
        self.lock
            .as_ref()
            .ok_or_else(|| AgentError::Other("Apply index lock unavailable".into()))
    }

    fn check_lock(&self) -> Result<()> {
        let owned = self.lock()?.metadata()?;
        verify_lock(&self.lock_path, owned.dev(), owned.ino())
    }

    fn cleanup(&mut self) -> Result<()> {
        self.release_lock()?;
        fs::remove_dir_all(&self.dir)?;
        File::open(self.common)?.sync_all()?;
        Ok(())
    }

    fn release_lock(&mut self) -> Result<()> {
        if self.lock.is_some() {
            self.check_lock()?;
            fs::remove_file(&self.lock_path)?;
            self.lock.take();
            if let Some(parent) = self.index.parent() {
                File::open(parent)?.sync_all()?;
            }
        }
        Ok(())
    }
}

fn private_git(root: &Path, git_dir: &Path) -> Result<tokio::process::Command> {
    let mut command = shellout::git_command()?;
    command
        .current_dir(root)
        .env("GIT_DIR", git_dir)
        .env("GIT_WORK_TREE", root)
        .env("GIT_INDEX_FILE", git_dir.join("index"));
    Ok(command)
}

async fn initialize_private_head(
    root: &Path,
    directory: &Path,
    common: &Path,
    old: &str,
) -> Result<()> {
    write_synced(
        &directory.join("commondir"),
        format!("{}\n", common.display()).as_bytes(),
    )?;
    write_synced(&directory.join("HEAD"), format!("{old}\n").as_bytes())?;
    // Reftable repositories use the per-worktree stack for HEAD. Git needs
    // that stack initialized before it can read the private detached ref.
    if common.join("reftable").is_dir() {
        fs::create_dir(directory.join("reftable"))?;
        let mut command = projection_git(root, directory)?;
        command.args(["update-ref", "--no-deref", "HEAD", old]);
        output_text(shellout::output(&mut command).await?)?;
    }
    Ok(())
}

fn projection_git(root: &Path, git_dir: &Path) -> Result<tokio::process::Command> {
    let mut command = private_git(root, git_dir)?;
    command.args([
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "hook.post-checkout.enabled=false",
        "-c",
        "hook.reference-transaction.enabled=false",
        "-c",
        "hook.post-index-change.enabled=false",
    ]);
    Ok(command)
}

fn verify_lock(path: &Path, device: u64, inode: u64) -> Result<()> {
    let live = fs::symlink_metadata(path)?;
    if !live.is_file() || (device, inode) != (live.dev(), live.ino()) {
        return Err(AgentError::Other(
            "Apply index lock ownership changed".into(),
        ));
    }
    Ok(())
}

fn output_text(output: Output) -> Result<String> {
    if !output.status.success() {
        return Err(command_error(output));
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn copy_synced(from: &Path, to: &Path) -> io::Result<()> {
    let mut input = File::open(from)?;
    let mut output = OpenOptions::new().write(true).create_new(true).open(to)?;
    io::copy(&mut input, &mut output)?;
    output.sync_all()
}

fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn command_error(output: Output) -> AgentError {
    AgentError::Cli {
        status: output.status.code().unwrap_or(-1),
        stderr: format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    }
}

fn record_closed_writers(dir: &Path) -> Result<()> {
    // Only the supervising worker can issue this proof after its commands
    // close. A worker crash cannot authorize recovery from a released lease.
    write_synced(&dir.join(WRITERS_CLOSED), b"closed\n")?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

fn target_changed(target: &str) -> AgentError {
    AgentError::Managed {
        kind: "task_apply_target".into(),
        message: format!("Saved Apply target '{target}' changed before publication"),
        remediation: "Verify the checkout and retry Apply".into(),
        source: None,
    }
}

fn recovery(dir: &Path, source: AgentError) -> AgentError {
    if !dir.exists() {
        return source;
    }
    retain_recovery();
    pending(dir, Some(source))
}

fn pending(dir: &Path, source: Option<AgentError>) -> AgentError {
    AgentError::Managed {
        kind: "task_apply_pending".into(),
        message: "The target checkout needs Apply recovery".into(),
        remediation: format!(
            "Inspect the target ref and index with the recovery record at {}",
            dir.display()
        ),
        source: source.map(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>),
    }
}
