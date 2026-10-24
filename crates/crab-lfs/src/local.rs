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
