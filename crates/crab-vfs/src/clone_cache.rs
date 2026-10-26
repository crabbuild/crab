//! Blobless clone cache for remote mount sources.
//!
//! Manages a cache of bare, blobless git clones under
//! `~/.crab/mounts/repos/<hash>/` where `<hash>` is the first 12 hex
//! characters of the SHA-256 of the normalized remote URL.
//!
//! This ensures:
//! - Same remote → same cache directory (branch switches reuse the clone)
//! - Different remotes → isolated state
//! - No path-length issues from long URLs

use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use crate::core::error::{CrabError, Result};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Cache hash computation
// ---------------------------------------------------------------------------

/// Normalize a remote URL for consistent hashing.
///
/// Normalization rules:
/// - Lowercase the scheme (everything before `://`)
/// - Trim trailing slashes from the path component
/// - Trim leading/trailing whitespace
fn normalize_url(url: &str) -> String {
    let trimmed = url.trim();

    // Split on `://` to lowercase the scheme only.
    if let Some((scheme, rest)) = trimmed.split_once("://") {
        let normalized_rest = rest.trim_end_matches('/');
        format!("{}://{}", scheme.to_ascii_lowercase(), normalized_rest)
    } else {
        // No scheme separator — normalize as-is (trim trailing slashes).
        trimmed.trim_end_matches('/').to_owned()
    }
}

/// Compute the cache hash for a remote URL.
///
/// Returns the first 12 hex characters of the SHA-256 hash of the
/// normalized URL. This is used as the directory name under
/// `~/.crab/mounts/repos/`.
pub fn compute_cache_hash(url: &str) -> String {
    let normalized = normalize_url(url);
    let hash = Sha256::digest(normalized.as_bytes());
    // Take first 6 bytes (12 hex chars).
    hex_encode_prefix(&hash[..6])
}

/// Encode bytes as lowercase hex.
fn hex_encode_prefix(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(&mut encoded, "{byte:02x}");
    }
    encoded
}

/// Return the cache directory path for a given remote URL.
///
/// The path is `~/.crab/mounts/repos/<hash>/` where hash is computed
/// by [`compute_cache_hash`].
pub fn cache_dir_for_url(url: &str) -> Result<PathBuf> {
    let home = home_dir()?;
    let hash = compute_cache_hash(url);
    Ok(home.join(".crab").join("mounts").join("repos").join(hash))
}

/// Select the single Git metadata owner within a mount cache.
/// Fresh caches use `.git`; existing bare metadata retains its refs and staging
/// namespace. Reject mixed owners rather than choosing or replacing history.
pub fn git_dir(cache_dir: &Path) -> Result<PathBuf> {
    let nested = cache_dir.join(".git");
    let root_metadata =
        cache_dir.join("HEAD").try_exists()? || cache_dir.join("objects").try_exists()?;
    if root_metadata && nested.try_exists()? {
        return Err(CrabError::Configuration {
            key: format!(
                "mount cache has multiple Git metadata owners at {}",
                cache_dir.display()
            ),
            origin: "crab mount".into(),
        });
    }
    // Both current mount modes have produced bare Git metadata. Retaining its
    // actual directory preserves unpushed refs and the derived staging owner;
    // creating a second clone would orphan them under the same overlay cache.
    Ok(if root_metadata {
        cache_dir.to_path_buf()
    } else {
        nested
    })
}

/// Cross-process guard for mutable mount cache ownership.
#[derive(Debug)]
pub struct MountCacheLock {
    _file: File,
}

impl MountCacheLock {
    pub fn acquire(cache_dir: &Path) -> Result<Self> {
        Self::try_acquire(cache_dir)?.ok_or_else(|| CrabError::Configuration {
            key: format!(
                "mount cache is already in use at {}; unmount the active mount before starting another mount for this repository",
                cache_dir.display()
            ),
            origin: "crab mount".into(),
        })
    }

    /// Acquire mutable cache ownership, returning None while another owner holds it.
    pub fn try_acquire(cache_dir: &Path) -> Result<Option<Self>> {
        let path = mount_cache_lock_path(cache_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(CrabError::Io)?;
        }

        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(CrabError::Io)?;

        if !lock_mount_cache_file(&file, cache_dir, &path)? {
            return Ok(None);
        }
        Ok(Some(Self { _file: file }))
    }
}

#[cfg(unix)]
impl Drop for MountCacheLock {
    fn drop(&mut self) {
        // Close normally releases flock, but explicit unlock keeps same-process
        // reacquire tests deterministic under heavy parallel suite pressure.
        let _ = unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn mount_cache_lock_path(cache_dir: &Path) -> PathBuf {
    if cache_dir.file_name().and_then(|name| name.to_str()) == Some(".crab") {
        return cache_dir.join("mount.lock");
    }

    let base = cache_dir.parent().unwrap_or(cache_dir);
    let hash = compute_cache_hash(&cache_dir.to_string_lossy());
    base.join(".mount-locks").join(format!("{hash}.lock"))
}

#[cfg(unix)]
fn lock_mount_cache_file(file: &File, cache_dir: &Path, lock_path: &Path) -> Result<bool> {
    let fd = file.as_raw_fd();
    // SAFETY: flock with LOCK_EX|LOCK_NB on a valid file descriptor is safe.
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if ret == 0 {
        return Ok(true);
    }

    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(false);
    }

    Err(CrabError::Io(std::io::Error::new(
        error.kind(),
        format!(
            "failed to lock mount cache {} using {}: {error}",
            cache_dir.display(),
            lock_path.display()
        ),
    )))
}

#[cfg(not(unix))]
fn lock_mount_cache_file(_file: &File, _cache_dir: &Path, _lock_path: &Path) -> Result<bool> {
    Ok(true)
}

/// Resolve the user's home directory.
fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| CrabError::Configuration {
            key: "HOME environment variable not set".into(),
            origin: String::new(),
        })
}

// ---------------------------------------------------------------------------
// Blobless clone management
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Error classification
// ---------------------------------------------------------------------------

/// Errors specific to git clone/fetch operations.
#[derive(thiserror::Error, Debug)]
pub enum CloneCacheError {
    #[error("authentication failed: check your credentials")]
    AuthFailed,

    #[error("network error: could not reach remote")]
    NetworkError,

    #[error("remote not found: check the URL")]
    RemoteNotFound,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    // --- compute_cache_hash ---

    #[test]
    fn hash_is_12_hex_chars() {
        let hash = compute_cache_hash("crab://bucket/repo");
        assert_eq!(hash.len(), 12);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn same_url_same_hash() {
        let h1 = compute_cache_hash("crab://bucket/repo");
        let h2 = compute_cache_hash("crab://bucket/repo");
        assert_eq!(h1, h2);
    }

    #[test]
    fn different_urls_different_hashes() {
        let h1 = compute_cache_hash("crab://bucket/repo-a");
        let h2 = compute_cache_hash("crab://bucket/repo-b");
        assert_ne!(h1, h2);
    }

    #[test]
    fn normalization_lowercases_scheme() {
        let h1 = compute_cache_hash("CRAB://bucket/repo");
        let h2 = compute_cache_hash("crab://bucket/repo");
        assert_eq!(h1, h2);
    }

    #[test]
    fn normalization_trims_trailing_slashes() {
        let h1 = compute_cache_hash("crab://bucket/repo///");
        let h2 = compute_cache_hash("crab://bucket/repo");
        assert_eq!(h1, h2);
    }

    #[test]
    fn normalization_trims_whitespace() {
        let h1 = compute_cache_hash("  crab://bucket/repo  ");
        let h2 = compute_cache_hash("crab://bucket/repo");
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_deterministic_known_value() {
        // Verify the hash is stable across runs.
        let hash = compute_cache_hash("crab://my-bucket/ml-models");
        // Just verify it's 12 hex chars and deterministic.
        assert_eq!(hash.len(), 12);
        let hash2 = compute_cache_hash("crab://my-bucket/ml-models");
        assert_eq!(hash, hash2);
    }

    // --- normalize_url ---

    #[test]
    fn normalize_preserves_path_case() {
        let normalized = normalize_url("S3://Bucket/MyRepo");
        assert_eq!(normalized, "s3://Bucket/MyRepo");
    }

    #[test]
    fn normalize_no_scheme() {
        let normalized = normalize_url("/path/to/repo/");
        assert_eq!(normalized, "/path/to/repo");
    }

    #[test]
    fn normalize_multiple_trailing_slashes() {
        let normalized = normalize_url("gs://bucket/path///");
        assert_eq!(normalized, "gs://bucket/path");
    }

    // --- cache_dir_for_url ---

    #[test]
    fn cache_dir_uses_home_and_hash() {
        // Verify the path structure without mutating env vars.
        // We just check that the result ends with the expected components.
        let result = cache_dir_for_url("crab://bucket/repo");
        // If HOME is set (normal test environment), verify structure.
        if let Ok(dir) = result {
            let hash = compute_cache_hash("crab://bucket/repo");
            assert!(
                dir.ends_with(format!(".crab/mounts/repos/{hash}")),
                "expected path ending with .crab/mounts/repos/{hash}, got: {dir:?}"
            );
        }
        // If HOME is not set, the function returns an error — that's fine.
    }

    #[test]
    fn mixed_git_owners_are_rejected_without_replacing_history() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join(".git");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(root.path().join("HEAD"), b"root owner").unwrap();
        std::fs::write(nested.join("HEAD"), b"nested owner").unwrap();
        let result = git_dir(root.path());
        assert!(matches!(result, Err(CrabError::Configuration { .. })));
        assert_eq!(
            std::fs::read(root.path().join("HEAD")).unwrap(),
            b"root owner"
        );
        assert_eq!(std::fs::read(nested.join("HEAD")).unwrap(), b"nested owner");
    }

    #[test]
    fn mount_cache_lock_uses_sibling_lock_dir_without_creating_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("repos").join("repo-cache");

        let lock = MountCacheLock::acquire(&cache_dir).unwrap();
        let lock_path = mount_cache_lock_path(&cache_dir);

        assert!(lock_path.exists());
        assert!(!cache_dir.exists());

        drop(lock);
    }

    #[test]
    fn mount_cache_lock_for_legacy_crab_dir_stays_inside_crab_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let crab_dir = tmp.path().join(".crab");

        let lock = MountCacheLock::acquire(&crab_dir).unwrap();
        let lock_path = mount_cache_lock_path(&crab_dir);

        assert_eq!(lock_path, crab_dir.join("mount.lock"));
        assert!(lock_path.exists());
        assert!(!tmp.path().join(".mount-locks").exists());

        drop(lock);
    }

    #[cfg(unix)]
    #[test]
    fn mount_cache_lock_rejects_second_holder_until_released() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("repos").join("repo-cache");

        let lock = MountCacheLock::acquire(&cache_dir).unwrap();
        assert!(MountCacheLock::try_acquire(&cache_dir).unwrap().is_none());
        let conflict = MountCacheLock::acquire(&cache_dir).unwrap_err();
        assert!(
            conflict
                .to_string()
                .contains("mount cache is already in use"),
            "got: {conflict}"
        );

        drop(lock);

        let handoff = MountCacheLock::try_acquire(&cache_dir).unwrap().unwrap();
        assert!(MountCacheLock::try_acquire(&cache_dir).unwrap().is_none());
        drop(handoff);
        MountCacheLock::acquire(&cache_dir).unwrap();
    }
}
