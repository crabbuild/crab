//! Verified, atomic local Git LFS object storage.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tempfile::{Builder, NamedTempFile, TempPath};

use crate::core::error::{CrabError, Result};
use crab_git::lfs_pointer::{LfsPointer, hex_encode};

const HASH_BUFFER_SIZE: usize = 1024 * 1024;

/// Incrementally stages one LFS object in the cache filesystem.
pub(crate) struct ObjectWriter {
    temp: NamedTempFile,
    hasher: Sha256,
    size: u64,
}

impl ObjectWriter {
    /// Creates a temporary object beside the final LFS cache.
    pub(crate) fn new(lfs_dir: &Path) -> Result<Self> {
        let temp_dir = lfs_dir.join("tmp");
        std::fs::create_dir_all(&temp_dir).map_err(CrabError::Io)?;
        let temp = Builder::new()
            .prefix("crab-lfs-")
            .tempfile_in(temp_dir)
            .map_err(CrabError::Io)?;
        Ok(Self {
            temp,
            hasher: Sha256::new(),
            size: 0,
        })
    }

    /// Finalizes the temporary object and its SHA-256 metadata.
    pub(crate) fn finish(mut self) -> Result<StagedObject> {
        self.temp.as_file_mut().flush().map_err(CrabError::Io)?;
        self.temp.as_file().sync_all().map_err(CrabError::Io)?;
        Ok(StagedObject {
            temp: self.temp,
            oid: self.hasher.finalize().into(),
            size: self.size,
        })
    }
}

/// Creates a closed temporary path beneath the local LFS cache.
pub(crate) fn new_temp_path(lfs_dir: &Path) -> Result<TempPath> {
    let temp_dir = lfs_dir.join("tmp");
    std::fs::create_dir_all(&temp_dir).map_err(CrabError::Io)?;
    Builder::new()
        .prefix("crab-lfs-download-")
        .tempfile_in(temp_dir)
        .map(NamedTempFile::into_temp_path)
        .map_err(CrabError::Io)
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
pub(crate) struct StagedObject {
    temp: NamedTempFile,
    oid: [u8; 32],
    size: u64,
}

impl StagedObject {
    #[must_use]
    pub(crate) fn oid(&self) -> &[u8; 32] {
        &self.oid
    }

    #[must_use]
    pub(crate) fn size(&self) -> u64 {
        self.size
    }

    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        self.temp.path()
    }

    /// Re-hashes a completed temporary file after an extension transform.
    pub(crate) fn from_temp(temp: NamedTempFile) -> Result<Self> {
        let (oid, size) = hash_file(temp.path())?;
        Ok(Self { temp, oid, size })
    }

    /// Atomically installs the object under its content-addressed cache path.
    pub(crate) fn install(self, lfs_dir: &Path) -> Result<PathBuf> {
        let target = object_path(lfs_dir, &self.oid);
        if target.is_file() && verify_file(&target, &self.oid, self.size).is_ok() {
            return Ok(target);
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(CrabError::Io)?;
        }

        match self.temp.persist(&target) {
            Ok(_) => {}
            Err(error) => {
                if target.is_file() && verify_file(&target, &self.oid, self.size).is_ok() {
                    return Ok(target);
                }
                return Err(CrabError::Io(error.error));
            }
        }
        verify_file(&target, &self.oid, self.size)?;
        Ok(target)
    }
}

/// Returns the standard local object path beneath `.git/lfs`.
#[must_use]
pub(crate) fn object_path(lfs_dir: &Path, oid: &[u8; 32]) -> PathBuf {
    let hex = hex_encode(oid);
    lfs_dir
        .join("objects")
        .join(&hex[..2])
        .join(&hex[2..4])
        .join(hex)
}

/// Reads and verifies a cached object against both pointer fields.
pub(crate) fn read_pointer(lfs_dir: &Path, pointer: &LfsPointer) -> Result<Option<Vec<u8>>> {
    read(lfs_dir, &pointer.oid, pointer.size)
}

/// Reads and verifies a cached object against its SHA-256 and declared size.
pub(crate) fn read(lfs_dir: &Path, oid: &[u8; 32], size: u64) -> Result<Option<Vec<u8>>> {
    let path = object_path(lfs_dir, oid);
    if !path.is_file() {
        return Ok(None);
    }
    let content = std::fs::read(&path).map_err(CrabError::Io)?;
    verify_bytes(oid, size, &content)?;
    Ok(Some(content))
}

/// Checks a cached object without retaining its bytes in memory.
pub(crate) fn is_valid(lfs_dir: &Path, oid: &[u8; 32], size: u64) -> Result<bool> {
    let path = object_path(lfs_dir, oid);
    if !path.is_file() {
        return Ok(false);
    }
    match verify_file(&path, oid, size) {
        Ok(()) => Ok(true),
        Err(CrabError::LfsObjectCorrupt { .. }) => Ok(false),
        Err(error) => Err(error),
    }
}

/// Streams a file while verifying the exact emitted bytes against its pointer.
///
/// Emitted bytes are tentative until this returns successfully. Callers must
/// reject partial output on error; prior path validation cannot prove that a
/// mutable cache still contains those bytes when it is later opened or read.
pub(crate) fn stream_verified(
    path: &Path,
    oid: &[u8; 32],
    expected_size: u64,
    mut emit: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let mut file = File::open(path).map_err(CrabError::Io)?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    let corrupt = || CrabError::LfsObjectCorrupt {
        oid: hex_encode(oid),
    };
    loop {
        let read = file.read(&mut buffer).map_err(CrabError::Io)?;
        if read == 0 {
            break;
        }
        size = size.checked_add(read as u64).ok_or_else(corrupt)?;
        if size > expected_size {
            return Err(corrupt());
        }
        hasher.update(&buffer[..read]);
        emit(&buffer[..read])?;
    }
    let actual: [u8; 32] = hasher.finalize().into();
    if actual != *oid || size != expected_size {
        return Err(corrupt());
    }
    Ok(())
}

/// Atomically installs already-materialized bytes after integrity validation.
pub(crate) fn install_bytes(
    lfs_dir: &Path,
    oid: &[u8; 32],
    size: u64,
    content: &[u8],
) -> Result<PathBuf> {
    verify_bytes(oid, size, content)?;
    let mut writer = ObjectWriter::new(lfs_dir)?;
    writer.write_all(content).map_err(CrabError::Io)?;
    writer.finish()?.install(lfs_dir)
}

/// Atomically installs a file that was verified while it was streamed.
pub(crate) fn install_verified_temp_path(
    lfs_dir: &Path,
    oid: &[u8; 32],
    size: u64,
    temp: TempPath,
) -> Result<PathBuf> {
    let metadata = std::fs::metadata(&temp).map_err(CrabError::Io)?;
    if metadata.len() != size {
        return Err(CrabError::LfsObjectCorrupt {
            oid: hex_encode(oid),
        });
    }

    let target = object_path(lfs_dir, oid);
    if target.is_file() && verify_file(&target, oid, size).is_ok() {
        return Ok(target);
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(CrabError::Io)?;
    }

    match temp.persist(&target) {
        Ok(()) => Ok(target),
        Err(error) => {
            if target.is_file() && verify_file(&target, oid, size).is_ok() {
                return Ok(target);
            }
            Err(CrabError::Io(error.error))
        }
    }
}

/// Verifies materialized bytes against an expected SHA-256 and size.
pub(crate) fn verify_bytes(oid: &[u8; 32], size: u64, content: &[u8]) -> Result<()> {
    let actual: [u8; 32] = Sha256::digest(content).into();
    if actual != *oid || content.len() as u64 != size {
        return Err(CrabError::LfsObjectCorrupt {
            oid: hex_encode(oid),
        });
    }
    Ok(())
}

fn verify_file(path: &Path, oid: &[u8; 32], size: u64) -> Result<()> {
    let (actual, actual_size) = hash_file(path)?;
    if actual != *oid || actual_size != size {
        return Err(CrabError::LfsObjectCorrupt {
            oid: hex_encode(oid),
        });
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<([u8; 32], u64)> {
    let mut file = File::open(path).map_err(CrabError::Io)?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = vec![0u8; HASH_BUFFER_SIZE];
    loop {
        let read = file.read(&mut buffer).map_err(CrabError::Io)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size = size
            .checked_add(read as u64)
            .ok_or_else(|| CrabError::Internal("LFS object size overflow".to_owned()))?;
    }
    Ok((hasher.finalize().into(), size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_bytes_remain_tentative_until_digest_verification() {
        let dir = tempfile::tempdir().unwrap();
        let content = vec![0x5a; 3 * 64 * 1024];
        let oid = Sha256::digest(&content).into();
        let path = install_bytes(dir.path(), &oid, content.len() as u64, &content).unwrap();
        let mut emitted = 0;
        let result = stream_verified(&path, &oid, content.len() as u64, |bytes| {
            emitted += bytes.len();
            if emitted == bytes.len() {
                std::fs::write(&path, vec![0; content.len()]).unwrap();
            }
            Ok(())
        });
        assert!(matches!(result, Err(CrabError::LfsObjectCorrupt { .. })));
    }

    #[test]
    fn verified_stream_emits_exact_valid_content_in_bounded_chunks() {
        let dir = tempfile::tempdir().unwrap();
        for content in [Vec::new(), vec![0x5a; 3 * 64 * 1024 + 1]] {
            let oid = Sha256::digest(&content).into();
            let path = install_bytes(dir.path(), &oid, content.len() as u64, &content).unwrap();
            let mut actual = Vec::new();
            stream_verified(&path, &oid, content.len() as u64, |bytes| {
                assert!(bytes.len() <= 64 * 1024);
                actual.extend_from_slice(bytes);
                Ok(())
            })
            .unwrap();
            assert_eq!(actual, content);
        }
    }

    #[test]
    fn verified_stream_does_not_emit_bytes_past_the_declared_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversized");
        std::fs::write(&path, [0; 10]).unwrap();
        let mut emitted = 0;
        let result = stream_verified(&path, &[0; 32], 1, |bytes| {
            emitted += bytes.len();
            Ok(())
        });
        assert!(matches!(result, Err(CrabError::LfsObjectCorrupt { .. })) && emitted == 0);
    }

    #[test]
    fn verified_stream_preserves_sink_errors() {
        let dir = tempfile::tempdir().unwrap();
        let content = b"valid content";
        let oid = Sha256::digest(content).into();
        let path = install_bytes(dir.path(), &oid, content.len() as u64, content).unwrap();
        let result = stream_verified(&path, &oid, content.len() as u64, |_| {
            Err(CrabError::Cancelled)
        });
        assert!(matches!(result, Err(CrabError::Cancelled)));
    }

    #[test]
    fn corrupt_cached_object_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let expected = b"expected";
        let oid: [u8; 32] = Sha256::digest(expected).into();
        let path = object_path(dir.path(), &oid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"corrupt!").unwrap();

        let error = read(dir.path(), &oid, expected.len() as u64).unwrap_err();

        assert!(matches!(error, CrabError::LfsObjectCorrupt { .. }));
    }

    #[test]
    fn atomic_install_replaces_corrupt_cached_object() {
        let dir = tempfile::tempdir().unwrap();
        let content = b"correct bytes";
        let oid: [u8; 32] = Sha256::digest(content).into();
        let path = object_path(dir.path(), &oid);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"corrupt").unwrap();

        install_bytes(dir.path(), &oid, content.len() as u64, content).unwrap();

        assert_eq!(std::fs::read(path).unwrap(), content);
    }
}
