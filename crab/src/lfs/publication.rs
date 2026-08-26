//! LFS dependency publication shared by every Git push entry point.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::core::error::{CrabError, Result};
use crate::lfs::lock::LockManager;
use crab_git::lfs_pointer::{LfsPointer, hex_encode};
use crab_lfs::{LfsError, LfsObjectStore};

/// Publishes and verifies each LFS dependency introduced after `remote_tips`.
///
/// This gate runs inside the common push pipeline before refs can become
/// visible, so native and remote-helper pushes share one durability boundary.
/// Dependencies reachable from `remote_tips` have already passed the gate.
pub(crate) async fn publish_reachable(
    store: crab_storage::Store,
    prefix: String,
    git_dir: PathBuf,
    tips: Vec<String>,
    remote_tips: Vec<String>,
) -> Result<()> {
    if tips.is_empty() {
        return Ok(());
    }

    let scan_dir = git_dir.clone();
    let scan_tips = tips.clone();
    let entries = tokio::task::spawn_blocking(move || {
        crate::cmd::lfs::push::collect_pointers_from_range_in(&scan_dir, &scan_tips, &remote_tips)
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
    use std::io::Write;
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

    fn git_value(repo: &Path, args: &[&str]) -> String {
        let output = git(repo).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
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

        publish_reachable(
            store.clone(),
            "repo".to_owned(),
            git_dir,
            vec![head],
            Vec::new(),
        )
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
            Vec::new(),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            CrabError::LfsObjectMissing { ref oid } if oid == &hex_encode(&pointer.oid)
        ));
    }

    #[tokio::test]
    async fn publication_does_not_hydrate_objects_reachable_from_remote_tips() {
        let (source, _pointer, _content, remote_tip) = fixture();
        let pointer_blob = git_value(source.path(), &["rev-parse", "HEAD:asset.bin"]);
        let root = tempfile::tempdir().unwrap();
        let remote = root.path().join("remote.git");
        let client = root.path().join("client");
        assert!(
            Command::new("git")
                .args(["clone", "--bare", "--quiet"])
                .arg(source.path())
                .arg(&remote)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            git(&remote)
                .args(["config", "uploadpack.allowFilter", "true"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["clone", "--quiet", "--filter=blob:none", "--no-checkout",])
                .arg(format!("file://{}", remote.display()))
                .arg(&client)
                .status()
                .unwrap()
                .success()
        );
        for (key, value) in [("user.email", "test@example.com"), ("user.name", "Test")] {
            assert!(
                git(&client)
                    .args(["config", key, value])
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let missing_before = git(&client)
            .env("GIT_NO_LAZY_FETCH", "1")
            .args(["cat-file", "-e", &pointer_blob])
            .status()
            .unwrap();
        assert!(!missing_before.success());

        let mut blob = git(&client)
            .args(["hash-object", "-w", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        blob.stdin
            .take()
            .unwrap()
            .write_all(b"new local content\n")
            .unwrap();
        let blob = blob.wait_with_output().unwrap();
        assert!(blob.status.success());
        let blob = String::from_utf8(blob.stdout).unwrap().trim().to_owned();
        let base_tree = git_value(&client, &["ls-tree", "HEAD"]);
        let tree_input = format!("{base_tree}\n100644 blob {blob}\tnew.txt\n");
        let mut tree = git(&client)
            .args(["mktree", "--missing"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        tree.stdin
            .take()
            .unwrap()
            .write_all(tree_input.as_bytes())
            .unwrap();
        let tree = tree.wait_with_output().unwrap();
        assert!(tree.status.success());
        let tree = String::from_utf8(tree.stdout).unwrap().trim().to_owned();
        let local_tip = git_value(
            &client,
            &["commit-tree", &tree, "-p", &remote_tip, "-m", "local"],
        );

        let store = crab_storage::Store::new(Arc::new(InMemory::new()));
        publish_reachable(
            store,
            "repo".to_owned(),
            client.join(".git"),
            vec![local_tip],
            vec![remote_tip],
        )
        .await
        .unwrap();

        let missing_after = git(&client)
            .env("GIT_NO_LAZY_FETCH", "1")
            .args(["cat-file", "-e", &pointer_blob])
            .status()
            .unwrap();
        assert!(!missing_after.success());
    }
}
