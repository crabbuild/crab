//! Local LFS storage selection and verified preparation snapshots.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// Resolve the existing LFS storage keys and environment override.
#[must_use]
pub fn storage_override(
    storage: Option<&str>,
    lfsdir: Option<&str>,
    environment: Option<&str>,
) -> Option<PathBuf> {
    environment.or(storage).or(lfsdir).map(PathBuf::from)
}

/// Resolve a storage override against the common Git directory.
#[must_use]
pub fn storage_dir(common_git_dir: &Path, configured: Option<&Path>) -> PathBuf {
    match configured {
        Some(path) if path.is_absolute() => path.to_owned(),
        Some(path) => common_git_dir.join(path),
        None => common_git_dir.join("lfs"),
    }
}

/// Return one local payload path using Git LFS's SHA-256 fanout.
#[must_use]
pub fn object_path(storage: &Path, oid: &[u8; 32]) -> PathBuf {
    let hex = crab_git::lfs_pointer::hex_encode(oid);
    storage
        .join("objects")
        .join(&hex[..2])
        .join(&hex[2..4])
        .join(hex)
}

/// Resolve the local object directory using Git settings and the existing environment override.
pub fn storage_from_git(git_dir: &Path) -> std::io::Result<PathBuf> {
    let mut values = [None, None];
    for (key, value) in ["lfs.storage", "lfs.lfsdir"].into_iter().zip(&mut values) {
        let output = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(git_dir)
            .args(["config", "--includes", "--null", "--get", key])
            .env_remove("GIT_DIR")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()?;
        if output.status.code() == Some(1) {
            continue;
        }
        if !output.status.success() {
            return Err(std::io::Error::other(format!(
                "Git LFS storage configuration failed: {}",
                output.status
            )));
        }
        let text = String::from_utf8(output.stdout)
            .map_err(|source| std::io::Error::new(std::io::ErrorKind::InvalidData, source))?;
        let text = text.strip_suffix('\0').ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid Git config framing",
            )
        })?;
        *value = Some(text.to_owned());
    }
    let environment = std::env::var("GIT_LFS_DIR").ok();
    let configured = storage_override(
        values[0].as_deref(),
        values[1].as_deref(),
        environment.as_deref(),
    );
    let common = crab_git::discover::resolve_common_dir(git_dir);
    Ok(storage_dir(&common, configured.as_deref()))
}

/// Read a bounded range only after verifying the complete local payload in the same pass.
/// Returns absence only when opening the object fails with `NotFound`.
/// The session retains verification admission if the awaiting future is dropped.
pub async fn read_range(
    storage: &Path,
    oid: &[u8; 32],
    size: u64,
    range: std::ops::Range<u64>,
    cancel: &CancellationToken,
    session: &crate::LfsReadSession,
) -> crate::Result<Option<bytes::Bytes>> {
    if range.start > range.end || range.end > size {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "local LFS read range exceeds its declared size",
        )
        .into());
    }
    let permit = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted, "local LFS verification cancelled",
        ).into()),
        result = crate::object_store::origin::verification_permit() => result?,
    };
    let storage = storage.to_owned();
    let oid = *oid;
    let cancel = cancel.clone();
    session
        .spawn_blocking(move || {
            use std::io::Read;
            let _permit = permit;
            let source = object_path(&storage, &oid);
            let mut input = match std::fs::File::open(source) {
                Ok(input) => input,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            let corrupt = || crate::LfsError::ObjectCorrupt {
                oid: crab_git::lfs_pointer::hex_encode(&oid),
            };
            let capacity =
                usize::try_from(range.end - range.start).map_err(std::io::Error::other)?;
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(capacity)
                .map_err(|source| std::io::Error::new(std::io::ErrorKind::OutOfMemory, source))?;
            let mut hasher = Sha256::new();
            let mut offset = 0u64;
            let mut buffer = [0; 64 * 1024];
            loop {
                if cancel.is_cancelled() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "local LFS verification cancelled",
                    )
                    .into());
                }
                let count = input.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                let end = offset
                    .checked_add(count as u64)
                    .filter(|end| *end <= size)
                    .ok_or_else(corrupt)?;
                hasher.update(&buffer[..count]);
                let start = range.start.max(offset);
                let selected_end = range.end.min(end);
                if start < selected_end {
                    bytes.extend_from_slice(
                        &buffer[(start - offset) as usize..(selected_end - offset) as usize],
                    );
                }
                offset = end;
            }
            if offset != size || hasher.finalize().as_slice() != oid || bytes.len() != capacity {
                return Err(corrupt());
            }
            Ok(Some(bytes::Bytes::from(bytes)))
        })?
        .await
        .map_err(std::io::Error::other)?
}

/// Failure to freeze a local payload for publication.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("LFS payload preparation cancelled")]
    Cancelled,
    #[error("LFS payload preparation I/O failed")]
    Io(#[from] std::io::Error),
    #[error("local LFS payload differs from its pointer")]
    Corrupt(#[source] crate::LfsError),
}

/// Copy and verify exact LFS bytes into a new private preparation file.
///
/// The caller owns destination cleanup on every exit path. Returns the Blake3
/// digest used to bind durable recovery bytes; SHA-256 and size must match the
/// Git pointer before success. Neither path is trusted after this snapshot.
pub async fn snapshot(
    source: &Path,
    destination: &Path,
    oid: &[u8; 32],
    size: u64,
    cancel: &CancellationToken,
) -> Result<[u8; 32], SnapshotError> {
    if cancel.is_cancelled() {
        return Err(SnapshotError::Cancelled);
    }
    let mut input = tokio::fs::File::open(source).await?;
    let mut output = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .await?;
    let mut sha256 = Sha256::new();
    let mut blake3 = blake3::Hasher::new();
    let mut written = 0u64;
    let mut buffer = vec![0; 64 * 1024];
    loop {
        if cancel.is_cancelled() {
            return Err(SnapshotError::Cancelled);
        }
        let count = input.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        written = written
            .checked_add(count as u64)
            .filter(|total| *total <= size)
            .ok_or_else(|| corrupt(oid))?;
        let bytes = &buffer[..count];
        sha256.update(bytes);
        blake3.update(bytes);
        output.write_all(bytes).await?;
    }
    if written != size || sha256.finalize().as_slice() != oid {
        return Err(corrupt(oid));
    }
    output.flush().await?;
    if cancel.is_cancelled() {
        return Err(SnapshotError::Cancelled);
    }
    Ok(*blake3.finalize().as_bytes())
}

fn corrupt(oid: &[u8; 32]) -> SnapshotError {
    SnapshotError::Corrupt(crate::LfsError::ObjectCorrupt {
        oid: crab_git::lfs_pointer::hex_encode(oid),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn range_verifies_bytes_outside_the_requested_window() {
        let root = tempfile::tempdir().unwrap();
        let oid: [u8; 32] = Sha256::digest(b"abcdef").into();
        let path = object_path(root.path(), &oid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let session = crate::LfsReadSession::default();
        for (body, declared, valid) in [
            (b"abcdef".as_slice(), 6, true),
            (b"abcdeX".as_slice(), 6, false),
            (b"abcde".as_slice(), 6, false),
            (b"abcdefg".as_slice(), 6, false),
            (b"abcdef".as_slice(), 7, false),
        ] {
            std::fs::write(&path, body).unwrap();
            let result = read_range(
                root.path(),
                &oid,
                declared,
                1..3,
                &CancellationToken::new(),
                &session,
            )
            .await;
            if valid {
                assert_eq!(result.unwrap().unwrap(), b"bc".as_slice());
            } else {
                assert!(matches!(result, Err(crate::LfsError::ObjectCorrupt { .. })));
            }
        }
        session.close().await;
    }

    #[tokio::test]
    async fn local_absence_differs_from_unreadable_object() {
        let root = tempfile::tempdir().unwrap();
        let session = crate::LfsReadSession::default();
        let cancel = CancellationToken::new();
        let missing = read_range(root.path(), &[0; 32], 3, 0..1, &cancel, &session)
            .await
            .unwrap();
        assert!(missing.is_none());
        let path = object_path(root.path(), &[0; 32]);
        std::fs::create_dir_all(&path).unwrap();
        let invalid = read_range(root.path(), &[0; 32], 3, 0..1, &cancel, &session).await;
        session.close().await;
        assert!(invalid.is_err());
    }

    #[test]
    fn git_storage_selection_includes_linked_worktree_configuration() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let linked = root.path().join("linked");
        let git = |args: &[&str]| {
            let result = std::process::Command::new("git")
                .arg("-C")
                .arg(&source)
                .args(args)
                .env_remove("GIT_DIR")
                .env_remove("GIT_COMMON_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        };
        std::fs::create_dir(&source).unwrap();
        git(&["init", "-b", "main"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "base",
        ]);
        let include = root.path().join("included.config");
        std::fs::write(&include, "[lfs]\n storage = payloads\n").unwrap();
        git(&["config", "include.path", include.to_str().unwrap()]);
        git(&["worktree", "add", "-b", "linked", linked.to_str().unwrap()]);
        let git_dir = source.join(".git/worktrees/linked");
        assert_eq!(
            storage_from_git(&git_dir).unwrap(),
            source.join(".git/payloads")
        );
    }

    #[tokio::test]
    async fn snapshot_binds_owned_bytes_and_rejects_corruption() {
        for (body, size, valid) in [
            (b"abc".as_slice(), 3, true),
            (b"def".as_slice(), 3, false),
            (b"abc".as_slice(), 2, false),
            (b"abc".as_slice(), 4, false),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let source = directory.path().join("source");
            let destination = directory.path().join("snapshot");
            tokio::fs::write(&source, body).await.unwrap();
            let oid: [u8; 32] = Sha256::digest(b"abc").into();
            let result =
                snapshot(&source, &destination, &oid, size, &CancellationToken::new()).await;
            if valid {
                tokio::fs::write(source, b"changed").await.unwrap();
                assert!(
                    result.unwrap() == *blake3::hash(b"abc").as_bytes()
                        && tokio::fs::read(destination).await.unwrap() == b"abc"
                );
            } else {
                assert!(matches!(result, Err(SnapshotError::Corrupt(_))));
            }
        }
    }

    #[tokio::test]
    async fn cancelled_snapshot_does_not_open_its_paths() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = snapshot(
            Path::new("absent/source"),
            Path::new("absent/output"),
            &[0; 32],
            1,
            &cancel,
        )
        .await;
        assert!(matches!(result, Err(SnapshotError::Cancelled)));
    }

    #[test]
    fn storage_selection_preserves_shared_precedence_and_common_directory() {
        let common = Path::new("repository/.git");
        for (storage, alias, environment, expected) in [
            (None, None, None, "lfs"),
            (Some("primary"), Some("alias"), None, "primary"),
            (None, Some("alias"), None, "alias"),
            (
                Some("primary"),
                Some("alias"),
                Some("environment"),
                "environment",
            ),
        ] {
            let configured = storage_override(storage, alias, environment);
            assert_eq!(
                storage_dir(common, configured.as_deref()),
                common.join(expected)
            );
        }
    }
}
