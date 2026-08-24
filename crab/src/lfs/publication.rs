//! LFS dependency publication shared by every Git push entry point.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::core::error::{CrabError, Result};
use crate::lfs::lock::LockManager;
use crab_git::lfs_pointer::{LfsPointer, hex_encode};
use crab_lfs::{LfsError, LfsObjectStore};

/// Publishes and verifies every LFS dependency reachable from `tips`.
///
/// This gate runs inside the common push pipeline before refs can become
/// visible, so native and remote-helper pushes share one durability boundary.
pub(crate) async fn publish_reachable(
    store: crab_storage::Store,
    prefix: String,
    git_dir: PathBuf,
    tips: Vec<String>,
) -> Result<()> {
    if tips.is_empty() {
        return Ok(());
    }

    let scan_dir = git_dir.clone();
    let scan_tips = tips.clone();
    let entries = tokio::task::spawn_blocking(move || {
        crate::cmd::lfs::push::collect_pointers_from_range_in(&scan_dir, &scan_tips, &[])
    })
    .await
    .map_err(|error| CrabError::Internal(format!("LFS dependency scan failed: {error}")))??;

    if entries.is_empty() {
        return Ok(());
    }

    check_locks(&store, &prefix, &entries).await?;

    let mut pointers = HashMap::<[u8; 32], LfsPointer>::new();
    for (_path, pointer) in entries {
        match pointers.get(&pointer.oid) {
            Some(existing) if existing.size != pointer.size => {
                return Err(CrabError::LfsObjectCorrupt {
                    oid: hex_encode(&pointer.oid),
                });
            }
            Some(_) => {}
            None => {
                pointers.insert(pointer.oid, pointer);
            }
        }
    }

    let remote = LfsObjectStore::new(store, &prefix);
    let local_lfs_dir = git_dir.join("lfs");
    for pointer in pointers.into_values() {
        match remote.verify(&pointer.oid).await {
            Ok(bytes) => {
                crate::lfs::cache::verify_pointer(&pointer, &bytes)?;
                continue;
            }
            Err(LfsError::ObjectMissing { .. } | LfsError::ObjectCorrupt { .. }) => {}
            Err(error) => return Err(error.into()),
        }

        let local =
            crate::lfs::cache::read_pointer(&local_lfs_dir, &pointer)?.ok_or_else(|| {
                CrabError::LfsObjectMissing {
                    oid: hex_encode(&pointer.oid),
                }
            })?;
        crate::lfs::cache::verify_pointer(&pointer, &local)?;
        let local_path = crate::lfs::cache::object_path(&local_lfs_dir, &pointer.oid);
        remote.put_stream(&pointer.oid, &local_path).await?;
        let published = remote.verify(&pointer.oid).await?;
        crate::lfs::cache::verify_pointer(&pointer, &published)?;
    }

    Ok(())
}

async fn check_locks(
    store: &crab_storage::Store,
    prefix: &str,
    entries: &[(String, LfsPointer)],
) -> Result<()> {
    let owner = crate::cmd::lfs::store_setup::git_user_identity().unwrap_or_default();
    let paths: Vec<String> = entries
        .iter()
        .map(|(path, _)| path)
        .filter(|path| !path.is_empty())
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let manager = LockManager::lfs(crate::storage::Store::from_storage(store.clone()), prefix);
    let conflicts = manager.check_conflicts(&paths, &owner).await?;
    let Some(conflict) = conflicts.first() else {
        return Ok(());
    };
    Err(CrabError::LfsLockConflict {
        path: conflict.path.clone(),
        owner: conflict.owner.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use object_store::memory::InMemory;
    use sha2::{Digest, Sha256};
    use std::path::Path;
    use std::process::Command;
    use std::sync::Arc;

    fn git(repo: &Path) -> Command {
        let mut command = Command::new("git");
        command
            .current_dir(repo)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR");
        command
    }

    fn fixture() -> (tempfile::TempDir, LfsPointer, Vec<u8>, String) {
        let repo = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
        ] {
            assert!(git(repo.path()).args(args).status().unwrap().success());
        }
        let content = b"publication dependency".to_vec();
        let pointer = LfsPointer {
            oid: Sha256::digest(&content).into(),
            size: content.len() as u64,
            extensions: Vec::new(),
        };
        std::fs::write(repo.path().join("asset.bin"), pointer.serialize()).unwrap();
        assert!(
            git(repo.path())
                .args(["add", "asset.bin"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            git(repo.path())
                .args(["commit", "-q", "-m", "fixture"])
                .status()
                .unwrap()
                .success()
        );
        let head = git(repo.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        (
            repo,
            pointer,
            content,
            String::from_utf8(head.stdout).unwrap().trim().to_owned(),
        )
    }

    #[tokio::test]
    async fn publication_uploads_and_verifies_reachable_lfs_objects() {
        let (repo, pointer, content, head) = fixture();
        let git_dir = repo.path().join(".git");
        crate::lfs::cache::install_bytes(
            &git_dir.join("lfs"),
            &pointer.oid,
            pointer.size,
            &content,
        )
        .unwrap();
        let store = crab_storage::Store::new(Arc::new(InMemory::new()));

        publish_reachable(store.clone(), "repo".to_owned(), git_dir, vec![head])
            .await
            .unwrap();

        let remote = LfsObjectStore::new(store, "repo");
        assert_eq!(
            remote.verify(&pointer.oid).await.unwrap(),
            Bytes::from(content)
        );
    }

    #[tokio::test]
    async fn publication_rejects_missing_local_dependency() {
        let (repo, pointer, _content, head) = fixture();
        let store = crab_storage::Store::new(Arc::new(InMemory::new()));

        let error = publish_reachable(
            store,
            "repo".to_owned(),
            repo.path().join(".git"),
            vec![head],
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            CrabError::LfsObjectMissing { ref oid } if oid == &hex_encode(&pointer.oid)
        ));
    }
}
