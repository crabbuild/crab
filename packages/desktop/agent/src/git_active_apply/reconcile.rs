use std::fs::{self, File};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::git_ref_merge::{exact_branch_oid, require_direct_branch, with_verified_active_ref};
use crate::handlers::run_git_worktree_cmd as git;
use crate::{AgentError, Result};

use super::{
    CHECKOUT_BEFORE, Intent, JOURNAL, Stage, WRITERS_CLOSED, common_dir, pending,
    record_closed_writers, target_changed, verify_lock, writers_confirmed,
};

pub(crate) async fn recover(
    root: &Path,
    target: &str,
    source_root: &Path,
    source_ref: &str,
) -> Result<Option<String>> {
    let common = common_dir(root).await?;
    let dir = common.join(JOURNAL);
    let mut claimed = false;
    let result = reconcile(
        root,
        target,
        source_root,
        source_ref,
        &common,
        &dir,
        &mut claimed,
    )
    .await;
    if claimed && result.is_err() && dir.try_exists()? && writers_confirmed() {
        record_closed_writers(&dir).map_err(|error| pending(&dir, Some(error)))?;
    }
    result.map_err(|error| pending(&dir, Some(error)))
}

async fn reconcile(
    root: &Path,
    target: &str,
    source_root: &Path,
    source_ref: &str,
    common: &Path,
    dir: &Path,
    claimed: &mut bool,
) -> Result<Option<String>> {
    if !fs::symlink_metadata(dir)?.is_dir() {
        return Err(uncertain("Apply recovery record is not a directory"));
    }
    let intent: Intent = serde_json::from_slice(&fs::read(dir.join("intent.json"))?)?;
    let git_dir = PathBuf::from(git(root, &["rev-parse", "--absolute-git-dir"]).await?);
    let index = git_dir.join("index");
    let actual_index = git(
        root,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    )
    .await?;
    require_direct_branch(source_root, source_ref).await?;
    let source = exact_branch_oid(source_root, source_ref).await?;
    if intent.worktree != root
        || intent.source_worktree != source_root
        || intent.target != target
        || intent.source != source
        || intent.index != index
        || Path::new(&actual_index) != index
        || !is_oid(&intent.old, source.len())
    {
        return Err(uncertain(
            "Apply recovery record does not match the saved task and checkout",
        ));
    }

    let receipt = dir.join(WRITERS_CLOSED);
    if !fs::symlink_metadata(&receipt)?.is_file() || fs::read(&receipt)? != b"closed\n" {
        return Err(uncertain(
            "The interrupted Apply's writer closure is unconfirmed",
        ));
    }
    // Consume the proof before another recovery command can write. A crash
    // during that command requires a new receipt from its supervisor.
    fs::remove_file(&receipt)?;
    File::open(dir)?.sync_all()?;
    *claimed = true;

    let mut stage = Stage {
        root,
        common,
        dir: dir.to_owned(),
        index,
        lock_path: git_dir.join("index.lock"),
        lock: None,
    };
    let current = exact_branch_oid(root, target).await?;
    let original = read_optional(&dir.join("original-index"))?;
    let candidate = read_optional(&dir.join("candidate"))?
        .map(String::from_utf8)
        .transpose()?;
    if current == intent.old {
        if candidate.is_some() {
            // The ref could have been published and subsequently reset. A
            // prepared candidate alone cannot authorize replaying that merge.
            return Err(uncertain(
                "The interrupted Apply may have published before the target ref moved back",
            ));
        }
        let mut before = [Vec::new(), Vec::new(), Vec::new()];
        for (name, bytes) in CHECKOUT_BEFORE.iter().zip(&mut before) {
            *bytes = read_optional(&dir.join(name))?.unwrap_or_default();
        }
        let verified = with_verified_active_ref(root, target, &current, async || {
            stage.lock = Some(open_owned_lock(&stage, &intent)?);
            if read_optional(&stage.index)? != original {
                return Err(uncertain(
                    "The target index changed after the interrupted Apply",
                ));
            }
            let shadow = stage.shadow_ok(&["rev-parse", "--verify", "HEAD"]).await?;
            if shadow == intent.old {
                if stage.checkout_state().await? != before {
                    return Err(uncertain(
                        "The target checkout changed after the interrupted Apply",
                    ));
                }
            } else {
                let proposal = fs::read_to_string(dir.join("proposal"))?;
                if shadow != proposal || !stage.restore_checkout(&intent.old, &before).await? {
                    return Err(uncertain(
                        "The target checkout could not be restored to its pre-Apply state",
                    ));
                }
            }
            stage.release_lock()
        })
        .await?;
        if !verified {
            return Err(target_changed(target));
        }
        stage.cleanup()?;
        return Ok(None);
    }

    let Some(candidate) =
        candidate.filter(|candidate| candidate == &current && is_oid(candidate, source.len()))
    else {
        return Err(uncertain(
            "The target ref changed after the interrupted Apply",
        ));
    };
    let parents = git(root, &["rev-list", "--parents", "-n1", &candidate]).await?;
    if !parents.split_ascii_whitespace().eq([
        candidate.as_str(),
        intent.old.as_str(),
        source.as_str(),
    ]) {
        return Err(uncertain(
            "The saved merge commit does not contain the interrupted Apply",
        ));
    }
    let verified = with_verified_active_ref(root, target, &candidate, async || {
        let projected = fs::read(dir.join("index"))?;
        match fs::symlink_metadata(&stage.lock_path) {
            Ok(_) => {
                let _lock = open_owned_lock(&stage, &intent)?;
                if read_optional(&stage.index)? != original
                    || fs::read(&stage.lock_path)? != projected
                {
                    return Err(uncertain(
                        "The target index or its prepared replacement changed",
                    ));
                }
                verify_lock(&stage.lock_path, intent.lock_device, intent.lock_inode)?;
                fs::rename(&stage.lock_path, &stage.index)?;
                File::open(&git_dir)?.sync_all()?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // Rename preserves the recorded inode. An already-installed
                // index can be acknowledged without touching newer file edits.
                verify_lock(&stage.index, intent.lock_device, intent.lock_inode)?;
                if fs::read(&stage.index)? != projected {
                    return Err(uncertain("The installed target index changed"));
                }
            }
            Err(error) => return Err(error.into()),
        }
        Ok(())
    })
    .await?;
    if !verified {
        return Err(target_changed(target));
    }
    stage.cleanup()?;
    Ok(Some(candidate))
}

fn open_owned_lock(stage: &Stage<'_>, intent: &Intent) -> Result<File> {
    verify_lock(&stage.lock_path, intent.lock_device, intent.lock_inode)?;
    let lock = File::open(&stage.lock_path)?;
    let owner = lock.metadata()?;
    if (owner.dev(), owner.ino()) != (intent.lock_device, intent.lock_inode) {
        return Err(uncertain("Apply index lock ownership changed"));
    }
    Ok(lock)
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn is_oid(value: &str, length: usize) -> bool {
    matches!(length, 40 | 64)
        && value.len() == length
        && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn uncertain(message: &str) -> AgentError {
    AgentError::Other(message.to_owned())
}
