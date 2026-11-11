use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use tokio::process::Command;

use super::{
    Intent, JOURNAL, PLANNING, STAGED_BEFORE, command_error, output_text, private_git,
    projection_git, verify_lock, write_synced,
};
use crate::{AgentError, Result, shellout};

const MERGE_HOOKS: [&str; 3] = ["pre-merge-commit", "prepare-commit-msg", "commit-msg"];
const SAVED_CONFIG: &str = "CRAB_DESKTOP_APPLY_GIT_CONFIG_PARAMETERS";

pub(super) fn install(planning: &Path) -> Result<PathBuf> {
    let hooks = planning.join("hooks");
    fs::create_dir(&hooks)?;
    let sidecar = std::env::current_exe()?;
    for name in MERGE_HOOKS {
        symlink(&sidecar, hooks.join(name))?;
    }
    Ok(hooks)
}

pub(super) async fn disabled_named_hooks(root: &Path, planning: &Path) -> Result<Vec<String>> {
    let mut command = private_git(root, planning);
    command.args([
        "config",
        "--null",
        "--name-only",
        "--get-regexp",
        r"^hook\..*\.event$",
    ]);
    let output = shellout::output(&mut command).await?;
    if output.status.code() == Some(1) {
        return Ok(Vec::new());
    }
    let names = output_text(output)?;
    // Git versions with config-defined hooks can still run them when
    // core.hooksPath points to the private directory. The real hook owns them.
    Ok(names
        .split('\0')
        .filter_map(|name| name.strip_suffix(".event"))
        .map(|name| format!("{name}.enabled=false"))
        .collect())
}

pub(super) fn save_git_config(command: &mut Command) {
    command.env_remove(SAVED_CONFIG);
    if let Some(parameters) = std::env::var_os("GIT_CONFIG_PARAMETERS") {
        command.env(SAVED_CONFIG, parameters);
    }
}

fn restore_git_config(command: &mut Command) {
    command
        .env_remove(SAVED_CONFIG)
        .env_remove("GIT_CONFIG_PARAMETERS");
    if let Some(parameters) = std::env::var_os(SAVED_CONFIG) {
        command.env("GIT_CONFIG_PARAMETERS", parameters);
    }
}

pub(crate) fn run_if_hook() -> Option<io::Result<()>> {
    let mut args = std::env::args_os();
    let program = PathBuf::from(args.next()?);
    let name = program.file_name()?.to_str()?;
    if !MERGE_HOOKS.contains(&name) {
        return None;
    }
    let planning = PathBuf::from(std::env::var_os("GIT_DIR")?);
    let journal = planning.parent()?;
    if planning.file_name()? != PLANNING
        || journal.file_name()? != JOURNAL
        || program != planning.join("hooks").join(name)
    {
        return None;
    }
    Some(run(name, args.collect(), &planning, journal).map_err(io::Error::other))
}

#[tokio::main(flavor = "current_thread")]
async fn run(name: &str, args: Vec<OsString>, planning: &Path, journal: &Path) -> Result<()> {
    let intent: Intent = serde_json::from_slice(&fs::read(journal.join("intent.json"))?)?;
    verify_lock(
        &intent.index.with_extension("lock"),
        intent.lock_device,
        intent.lock_inode,
    )?;
    let proposal = journal.join("proposal");
    if !proposal.try_exists()? {
        if journal.join(STAGED_BEFORE).try_exists()? {
            return Err(AgentError::Other(
                "Commit or unstage the target checkout's staged changes before Apply".into(),
            ));
        }
        let mut tree = private_git(&planning.join("worktree"), planning);
        tree.arg("write-tree");
        let tree = output_text(shellout::output(&mut tree).await?)?;
        let mut commit = projection_git(&intent.worktree, journal);
        restore_git_config(&mut commit);
        commit.args([
            "commit-tree",
            &tree,
            "-p",
            &intent.old,
            "-m",
            "Apply checkout proposal",
        ]);
        let commit = output_text(shellout::output(&mut commit).await?)?;
        write_synced(&proposal, commit.as_bytes())?;
        File::open(journal)?.sync_all()?;

        // Git's checkout protects ignored files. The sparse merge calculates
        // the result without writing into the real checkout.
        let mut checkout = projection_git(&intent.worktree, journal);
        restore_git_config(&mut checkout);
        checkout.args(["checkout", "--no-overwrite-ignore", "--detach", &commit]);
        let result = shellout::output(&mut checkout).await?;
        if !result.status.success() {
            return Err(command_error(result));
        }
    }

    // Git owns the merge metadata and computed tree. The real hook sees the
    // original worktree and can stage edits in its index without changing that tree.
    let mut hook = private_git(&intent.worktree, planning);
    restore_git_config(&mut hook);
    hook.env("GIT_INDEX_FILE", journal.join("index"))
        .args(["-c", &format!("core.hooksPath={}", intent.hooks.display())])
        .args(["hook", "run", "--ignore-missing", name, "--"])
        .args(args);
    let output = shellout::output(&mut hook).await?;
    if !output.status.success() {
        return Err(command_error(output));
    }
    Ok(())
}
