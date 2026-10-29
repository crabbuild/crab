//! Local LFS storage selection and verified preparation snapshots.

use crab_git::lfs_pointer::hex_encode;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// Incrementally stages one LFS object in the cache filesystem.
pub struct ObjectWriter {
    temp: NamedTempFile,
    hasher: Sha256,
    size: u64,
}

impl ObjectWriter {
    /// Creates a temporary object beside the final LFS cache.
    pub fn new(lfs_dir: &Path) -> crate::Result<Self> {
        let temp_dir = lfs_dir.join("tmp");
        std::fs::create_dir_all(&temp_dir).map_err(crate::LfsError::from)?;
        let temp = tempfile::Builder::new()
            .prefix("crab-lfs-")
            .tempfile_in(temp_dir)
            .map_err(crate::LfsError::from)?;
        Ok(Self {
            temp,
            hasher: Sha256::new(),
            size: 0,
        })
    }

    /// Finalizes the temporary object and its SHA-256 metadata.
    pub fn finish(mut self) -> crate::Result<StagedObject> {
        self.temp
            .as_file_mut()
            .flush()
            .map_err(crate::LfsError::from)?;
        self.temp
            .as_file()
            .sync_all()
            .map_err(crate::LfsError::from)?;
        Ok(StagedObject {
            temp: self.temp,
            oid: self.hasher.finalize().into(),
            size: self.size,
        })
    }
}

impl Write for ObjectWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.temp.write(buf)?;
        self.hasher.update(&buf[..written]);
        self.size = self
            .size
            .checked_add(written as u64)
            .ok_or_else(|| std::io::Error::other("LFS object size overflow"))?;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.temp.flush()
    }
}

/// A fully hashed temporary LFS object awaiting atomic installation.
pub struct StagedObject {
    temp: NamedTempFile,
    oid: [u8; 32],
    size: u64,
}

impl StagedObject {
    /// SHA-256 identity of the staged payload.
    #[must_use]
    pub fn oid(&self) -> &[u8; 32] {
        &self.oid
    }

    /// Exact byte length of the staged payload.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Owned temporary payload path, valid until installation or drop.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.temp.path()
    }

    /// Re-hashes a completed temporary file after an extension transform.
    pub fn from_temp(temp: NamedTempFile) -> crate::Result<Self> {
        let (oid, size) = hash_file(temp.path())?;
        Ok(Self { temp, oid, size })
    }

    /// Atomically installs the object under its content-addressed cache path.
    pub fn install(self, lfs_dir: &Path) -> crate::Result<PathBuf> {
        let target = object_path(lfs_dir, &self.oid);
        if target.is_file() && verify_file(&target, &self.oid, self.size).is_ok() {
            return Ok(target);
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(crate::LfsError::from)?;
        }

        match self.temp.persist(&target) {
            Ok(_) => {}
            Err(error) => {
                if target.is_file() && verify_file(&target, &self.oid, self.size).is_ok() {
                    return Ok(target);
                }
                return Err(error.error.into());
            }
        }
        verify_file(&target, &self.oid, self.size)?;
        Ok(target)
    }
}

/// Verify exact SHA-256 identity and size without retaining payload bytes.
pub fn verify_file(path: &Path, oid: &[u8; 32], size: u64) -> crate::Result<()> {
    let (actual, actual_size) = hash_file(path)?;
    if actual != *oid || actual_size != size {
        return Err(crate::LfsError::ObjectCorrupt {
            oid: hex_encode(oid),
        });
    }
    Ok(())
}

fn hash_file(path: &Path) -> crate::Result<([u8; 32], u64)> {
    let mut file = File::open(path).map_err(crate::LfsError::from)?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(crate::LfsError::from)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size = size
            .checked_add(read as u64)
            .ok_or_else(|| std::io::Error::other("LFS object size overflow"))?;
    }
    Ok((hasher.finalize().into(), size))
}

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
            let result = session
                .read_local_range(root.path(), &oid, declared, 1..3, &CancellationToken::new())
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
    async fn verified_local_ranges_retain_the_opened_incarnation() {
        let root = tempfile::tempdir().unwrap();
        let body = vec![b'a'; 2 * 1024 * 1024];
        let oid: [u8; 32] = Sha256::digest(&body).into();
        let path = object_path(root.path(), &oid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &body).unwrap();
        let session = crate::LfsReadSession::default();
        let cancel = CancellationToken::new();
        session
            .read_local_range(root.path(), &oid, body.len() as u64, 0..1, &cancel)
            .await
            .unwrap();
        std::fs::rename(&path, root.path().join("retired")).unwrap();
        std::fs::write(&path, vec![b'X'; body.len()]).unwrap();
        let next = session
            .read_local_range(
                root.path(),
                &oid,
                body.len() as u64,
                1024 * 1024..1024 * 1024 + 4,
                &cancel,
            )
            .await;
        session.close().await;
        assert_eq!(next.unwrap().unwrap(), b"aaaa".as_slice());
    }

    #[tokio::test]
    async fn local_absence_differs_from_unreadable_object() {
        let root = tempfile::tempdir().unwrap();
        let session = crate::LfsReadSession::default();
        let cancel = CancellationToken::new();
        let missing = session
            .read_local_range(root.path(), &[0; 32], 3, 0..1, &cancel)
            .await
            .unwrap();
        assert!(missing.is_none());
        let path = object_path(root.path(), &[0; 32]);
        std::fs::create_dir_all(&path).unwrap();
        let invalid = session
            .read_local_range(root.path(), &[0; 32], 3, 0..1, &cancel)
            .await;
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
