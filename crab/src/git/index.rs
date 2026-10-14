use std::path::Path;
use std::process::Command;

use crate::core::error::{CrabError, Result};
use crab_staging::{PublicationIntentId, StagingArea, StagingAreaReadOnly};

/// Stage repository metadata paths and propagate Git failures.
pub(crate) fn stage_paths(repo_root: &Path, paths: &[&str]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }

    let output = Command::new("git")
        .arg("add")
        .arg("--")
        .args(paths)
        .current_dir(repo_root)
        .output()?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if !stderr.trim().is_empty() {
        stderr.trim()
    } else if !stdout.trim().is_empty() {
        stdout.trim()
    } else {
        "no diagnostic output"
    };
    Err(CrabError::Internal(format!(
        "git add repository metadata failed: {detail}"
    )))
}

/// Fingerprint the complete semantic state of one Git index path.
pub(crate) fn git_index_path_state(index: &gix_index::File, path: &bstr::BStr) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab git index path state v1\0");
    if let Some(range) = index.entry_range(path) {
        for entry in &index.entries()[range] {
            hasher.update(&entry.flags.bits().to_le_bytes());
            hasher.update(&entry.mode.bits().to_le_bytes());
            hasher.update(entry.id.as_bytes());
        }
    }
    hasher.finalize().to_hex().to_string()
}

/// Reconcile shared staging publications while holding the Git index lock.
pub(crate) async fn reconcile_publication_intents(
    staging: &StagingAreaReadOnly,
    repo_root: &Path,
) -> Result<()> {
    if staging.unresolved_publication_intents()?.is_empty() {
        return Ok(());
    }
    let (_index_lock, index) = lock_publication_index(repo_root)?;
    // Another shared recovery may have completed before lock acquisition.
    // Read intents again under the lock rather than acting on a stale list.
    let actions = publication_recovery_actions(staging.unresolved_publication_intents()?, &index)?;
    for action in actions {
        match action {
            PublicationRecoveryAction::Publish(intent_id) => {
                staging.publish_publication_intent(&intent_id)?;
            }
            PublicationRecoveryAction::Rollback(intent_id) => {
                staging.rollback_publication_intent(&intent_id).await?;
            }
        }
    }
    Ok(())
}

/// Reconcile exclusive staging publications while holding the Git index lock.
pub(crate) fn reconcile_publication_intents_writer(
    staging: &StagingArea,
    repo_root: &Path,
) -> Result<()> {
    if staging.unresolved_publication_intents()?.is_empty() {
        return Ok(());
    }
    let (_index_lock, index) = lock_publication_index(repo_root)?;
    // Another shared recovery may have completed before lock acquisition.
    // Read intents again under the lock rather than acting on a stale list.
    let actions = publication_recovery_actions(staging.unresolved_publication_intents()?, &index)?;
    for action in actions {
        match action {
            PublicationRecoveryAction::Publish(intent_id) => {
                staging.publish_publication_intent(&intent_id)?;
            }
            PublicationRecoveryAction::Rollback(intent_id) => {
                staging.rollback_publication_intent(&intent_id)?;
            }
        }
    }
    Ok(())
}

enum PublicationRecoveryAction {
    Publish(PublicationIntentId),
    Rollback(PublicationIntentId),
}

fn lock_publication_index(repo_root: &Path) -> Result<(gix_lock::Marker, gix_index::File)> {
    use gix_index::{File, decode};

    let index_path =
        crate::git::worktree::WorktreeContext::resolve_from_path(repo_root)?.index_path();
    // Holding Git's lock through staging mutation prevents retirement while
    // another writer is still installing the pointer. Fail immediately to
    // avoid Git-index/staging lock inversion with an active filter child.
    let index_lock = gix_lock::Marker::acquire_to_hold_resource(
        &index_path,
        gix_lock::acquire::Fail::Immediately,
        None,
    )
    .map_err(|error| CrabError::Io(std::io::Error::other(error)))?;
    let index = File::at_or_default(
        &index_path,
        gix_hash::Kind::Sha1,
        false,
        decode::Options::default(),
    )
    .map_err(|error| {
        CrabError::Internal(format!(
            "failed to read Git index for add publication recovery: {error}"
        ))
    })?;

    Ok((index_lock, index))
}

fn publication_recovery_actions(
    intents: Vec<crab_staging::UnresolvedPublicationIntent>,
    index: &gix_index::File,
) -> Result<Vec<PublicationRecoveryAction>> {
    use bstr::ByteSlice;
    use gix_index::entry;

    let mut actions = Vec::with_capacity(intents.len());
    for intent in intents {
        let new_matches = intent
            .entries
            .iter()
            .filter(|expected| {
                index
                    .entry_by_path_and_stage(
                        expected.path_bytes.as_bstr(),
                        entry::Stage::Unconflicted,
                    )
                    .is_some_and(|actual| actual.id.to_string() == expected.expected_pointer_oid)
            })
            .count();
        let old_matches = intent
            .entries
            .iter()
            .filter(|expected| {
                git_index_path_state(index, expected.path_bytes.as_bstr())
                    == expected.previous_index_state
            })
            .count();
        if new_matches == intent.entries.len() {
            actions.push(PublicationRecoveryAction::Publish(intent.intent_id));
        } else if old_matches == intent.entries.len() {
            actions.push(PublicationRecoveryAction::Rollback(intent.intent_id));
        } else {
            return Err(CrabError::StagingCorrupt(format!(
                "add publication {} has an ambiguous Git index state ({new_matches}/{} new paths, {old_matches}/{} prior paths); restore one complete index state, then retry",
                intent.intent_id.as_str(),
                intent.entries.len(),
                intent.entries.len(),
            )));
        }
    }
    Ok(actions)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn metadata_staging_propagates_git_index_failure() {
        let dir = tempfile::tempdir().unwrap();
        let init = Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        if !init.success() {
            eprintln!("SKIP: git init failed");
            return;
        }

        std::fs::write(dir.path().join("crab.toml"), "[remote]\n").unwrap();
        std::fs::write(dir.path().join(".git/index.lock"), "locked").unwrap();

        let error = stage_paths(dir.path(), &["crab.toml"])
            .expect_err("Git index lock must reject metadata staging");

        assert!(
            error
                .to_string()
                .contains("git add repository metadata failed")
        );
    }
}
