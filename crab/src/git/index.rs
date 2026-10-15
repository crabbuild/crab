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

/// Reconcile complete external batches against a stable Git index during maintenance.
pub(crate) fn reconcile_delegated_publications(
    staging: &StagingArea,
    repo_root: &Path,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<usize> {
    crate::core::error::check_cancelled(cancel)?;
    if staging.delegated_publications()?.is_empty() {
        return Ok(0);
    }
    let (_index_lock, index) = lock_publication_index(repo_root)?;
    let repo = gix::open(repo_root).map_err(|error| CrabError::Io(std::io::Error::other(error)))?;
    let mut accepted = std::collections::HashMap::new();
    // Delegation freezes membership. Re-read under Git's lock and retain it
    // through all promotions; a filter response alone cannot attest acceptance.
    for lease in staging.delegated_publications()? {
        crate::core::error::check_cancelled(cancel)?;
        let matches = delegated_pointer_matches(&repo, &index, &lease)?;
        accepted
            .entry(lease.batch_id)
            .and_modify(|all: &mut bool| *all &= matches)
            .or_insert(matches);
    }
    let mut published = 0;
    for (batch_id, matches) in accepted {
        crate::core::error::check_cancelled(cancel)?;
        if matches {
            staging.mark_batch_published(&batch_id)?;
            published += 1;
        }
    }
    // Missing/different entries may be --defer work or older unpushed history.
    // Releasing them needs additional evidence; current index absence is insufficient.
    Ok(published)
}

fn delegated_pointer_matches(
    repo: &gix::Repository,
    index: &gix_index::File,
    lease: &crab_staging::DelegatedPublication,
) -> Result<bool> {
    use bstr::ByteSlice;
    use crab_types::pointer::{MAX_POINTER_SIZE, Pointer, is_supported_version_line};
    use gix_index::entry::{Mode, Stage};

    let Some(entry) =
        index.entry_by_path_and_stage(lease.path_bytes.as_bstr(), Stage::Unconflicted)
    else {
        return Ok(false);
    };
    if !matches!(entry.mode, Mode::FILE | Mode::FILE_EXECUTABLE) {
        return Ok(false);
    }
    // A user can replace a staged pointer with a raw blob. Check its header
    // before materializing it so maintenance cannot allocate a 10-GiB file.
    let header = repo
        .find_header(entry.id)
        .map_err(|error| CrabError::Io(std::io::Error::other(error)))?;
    if header.kind() != gix_object::Kind::Blob {
        return Err(CrabError::CorruptObject {
            path: entry.id.to_string(),
            reason: "Git index file does not reference a blob".to_owned(),
        });
    }
    if header.size() > MAX_POINTER_SIZE as u64 {
        return Ok(false);
    }
    let blob = repo
        .find_blob(entry.id)
        .map_err(|error| CrabError::Io(std::io::Error::other(error)))?;
    let actual = gix_object::compute_hash(repo.object_hash(), gix_object::Kind::Blob, &blob.data)
        .map_err(|error| CrabError::Io(std::io::Error::other(error)))?;
    if actual != entry.id {
        return Err(CrabError::CorruptObject {
            path: entry.id.to_string(),
            reason: "Git index blob hash mismatch".to_owned(),
        });
    }
    let first = blob
        .data
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    if !std::str::from_utf8(first).is_ok_and(is_supported_version_line) {
        return Ok(false);
    }
    let pointer =
        Pointer::parse(&blob.data).map_err(|error| CrabError::Io(std::io::Error::other(error)))?;
    Ok(pointer.file_hash == <[u8; 32]>::from(lease.file_hash) && pointer.size == lease.size)
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
    #[tokio::test(flavor = "multi_thread")]
    async fn delegated_recovery_waits_for_git_acceptance_and_preserves_other_versions() {
        use crab_types::pointer::Pointer;
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(tmp.path())
                .status()
                .unwrap()
                .success()
        );
        let staging = StagingArea::open(tmp.path().join(".crab/staging"))
            .await
            .unwrap();
        let mut versions = Vec::new();
        for (path, data) in [
            ("accepted", b"first".as_slice()),
            ("different", b"second"),
            ("deferred", b"third"),
        ] {
            let staged = crab_staging::stream::stage_reader_streaming(
                &mut std::io::Cursor::new(data),
                Path::new(path),
                &staging,
                crab_staging::stream::StreamStageProgress::default(),
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            staging
                .delegate_publication(&[(&staged.batch_id, Path::new(path))])
                .unwrap();
            let mut pointer = Pointer {
                file_hash: staged.file_hash,
                size: staged.size,
                shard_hint: None,
            };
            if path == "different" {
                pointer.file_hash[0] ^= 1;
            }
            if path != "deferred" {
                std::fs::write(tmp.path().join(path), pointer.serialize()).unwrap();
            }
            versions.push(staged.recipe);
        }
        assert!(
            Command::new("git")
                .args(["add", "--", "accepted", "different"])
                .current_dir(tmp.path())
                .status()
                .unwrap()
                .success()
        );
        let lock = tmp.path().join(".git/index.lock");
        std::fs::write(&lock, b"another Git writer").unwrap();
        let blocked = reconcile_delegated_publications(
            &staging,
            tmp.path(),
            &tokio_util::sync::CancellationToken::new(),
        );
        assert!(blocked.is_err() && staging.delegated_publications().unwrap().len() == 3);
        std::fs::remove_file(lock).unwrap();
        let promoted = reconcile_delegated_publications(
            &staging,
            tmp.path(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .unwrap();
        assert!(
            promoted == 1
                && staging.delegated_publications().unwrap().len() == 2
                && staging
                    .published_recipe_for_file(&versions[0].file_hash())
                    .unwrap()
                    == Some(versions[0].clone())
                && staging
                    .published_recipe_for_file(&versions[1].file_hash())
                    .unwrap()
                    .is_none()
                && staging
                    .published_recipe_for_file(&versions[2].file_hash())
                    .unwrap()
                    .is_none()
        );
        staging.close().await.unwrap();
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn delegated_recovery_requires_small_blob_integrity_and_skips_oversized_body() {
        use std::io::Write;
        for oversized in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            assert!(
                Command::new("git")
                    .args(["init", "-q"])
                    .current_dir(tmp.path())
                    .status()
                    .unwrap()
                    .success()
            );
            let staging = StagingArea::open(tmp.path().join(".crab/staging"))
                .await
                .unwrap();
            let staged = crab_staging::stream::stage_reader_streaming(
                &mut std::io::Cursor::new(b"prepared bytes"),
                Path::new("oversized"),
                &staging,
                crab_staging::stream::StreamStageProgress::default(),
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            staging
                .delegate_publication(&[(&staged.batch_id, Path::new("oversized"))])
                .unwrap();
            // Header inspection is enough to reject this as a pointer. Loading
            // the deliberately incomplete 10-GiB body would fail or allocate it.
            let oid = "d".repeat(40);
            let object_dir = tmp.path().join(".git/objects").join(&oid[..2]);
            std::fs::create_dir_all(&object_dir).unwrap();
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            if oversized {
                encoder
                    .write_all(b"blob 10737418240\0header probe padding")
                    .unwrap();
            } else {
                let bytes = crab_types::pointer::Pointer {
                    file_hash: staged.file_hash,
                    size: staged.size,
                    shard_hint: None,
                }
                .serialize();
                encoder
                    .write_all(format!("blob {}\0", bytes.len()).as_bytes())
                    .unwrap();
                encoder.write_all(&bytes).unwrap();
            }
            std::fs::write(object_dir.join(&oid[2..]), encoder.finish().unwrap()).unwrap();
            assert!(
                Command::new("git")
                    .args([
                        "update-index",
                        "--add",
                        "--cacheinfo",
                        &format!("100644,{oid},oversized")
                    ])
                    .current_dir(tmp.path())
                    .status()
                    .unwrap()
                    .success()
            );
            let promoted = reconcile_delegated_publications(
                &staging,
                tmp.path(),
                &tokio_util::sync::CancellationToken::new(),
            );
            assert!(
                (if oversized {
                    matches!(promoted, Ok(0))
                } else {
                    matches!(promoted, Err(CrabError::CorruptObject { .. }))
                }) && staging.delegated_publications().unwrap().len() == 1
                    && !tmp.path().join(".git/index.lock").exists()
            );
            staging.close().await.unwrap();
        }
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn delegated_recovery_requires_every_batch_path_and_keeps_shared_native_owner() {
        use crab_types::pointer::Pointer;
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(tmp.path())
                .status()
                .unwrap()
                .success()
        );
        let staging = StagingArea::open(tmp.path().join(".crab/staging"))
            .await
            .unwrap();
        let mut versions = Vec::new();
        for path in ["first", "second"] {
            versions.push(
                crab_staging::stream::stage_reader_streaming(
                    &mut std::io::Cursor::new(b"shared payload"),
                    Path::new(path),
                    &staging,
                    crab_staging::stream::StreamStageProgress::default(),
                    &tokio_util::sync::CancellationToken::new(),
                )
                .await
                .unwrap(),
            );
        }
        let first = &versions[0];
        let second = &versions[1];
        staging
            .record_verified_recipe_lease(&first.batch_id, Path::new("second"), &second.recipe)
            .unwrap();
        staging
            .delegate_publication(&[
                (&first.batch_id, Path::new("first")),
                (&first.batch_id, Path::new("second")),
            ])
            .unwrap();
        let pointer = Pointer {
            file_hash: first.file_hash,
            size: first.size,
            shard_hint: None,
        }
        .serialize();
        std::fs::write(tmp.path().join("first"), &pointer).unwrap();
        assert!(
            Command::new("git")
                .args(["add", "--", "first"])
                .current_dir(tmp.path())
                .status()
                .unwrap()
                .success()
        );
        let partial = reconcile_delegated_publications(
            &staging,
            tmp.path(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .unwrap();
        assert!(partial == 0 && staging.delegated_publications().unwrap().len() == 2);
        std::fs::write(tmp.path().join("second"), pointer).unwrap();
        assert!(
            Command::new("git")
                .args(["add", "--", "second"])
                .current_dir(tmp.path())
                .status()
                .unwrap()
                .success()
        );
        let complete = reconcile_delegated_publications(
            &staging,
            tmp.path(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .unwrap();
        let health = staging.lifecycle_health().unwrap();
        assert!(
            complete == 1
                && health.path_heads == 2
                && health.path_leases == 3
                && health.open_batches_without_publication == 1
                && health.payloads == 1
        );
        staging.rollback_batch(&second.batch_id).unwrap();
        assert_eq!(
            staging
                .published_recipe_for_file(&first.recipe.file_hash())
                .unwrap(),
            Some(first.recipe.clone())
        );
        staging.close().await.unwrap();
    }
}
