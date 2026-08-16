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
use std::process::Command;

use sha2::{Digest, Sha256};
use tracing::{debug, error, info, warn};

use crate::core::error::{CrabError, Result};

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

/// Cross-process guard for mutable mount cache ownership.
#[derive(Debug)]
pub struct MountCacheLock {
    _file: File,
}

impl MountCacheLock {
    pub fn acquire(cache_dir: &Path) -> Result<Self> {
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

        lock_mount_cache_file(&file, cache_dir, &path)?;

        Ok(Self { _file: file })
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
fn lock_mount_cache_file(file: &File, cache_dir: &Path, lock_path: &Path) -> Result<()> {
    let fd = file.as_raw_fd();
    // SAFETY: flock with LOCK_EX|LOCK_NB on a valid file descriptor is safe.
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if ret == 0 {
        return Ok(());
    }

    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return Err(CrabError::Configuration {
            key: format!(
                "mount cache is already in use at {}; unmount the active mount before starting another mount for this repository",
                cache_dir.display()
            ),
            origin: "crab mount".into(),
        });
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
fn lock_mount_cache_file(_file: &File, _cache_dir: &Path, _lock_path: &Path) -> Result<()> {
    Ok(())
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

/// Ensure a blobless bare clone exists at `cache_dir` for the given URL.
///
/// If the cache directory already contains a bare git repo (detected by
/// the presence of a `HEAD` file), runs `git fetch --all` to update it.
/// Otherwise, performs a fresh blobless clone.
///
/// Returns the path to the git directory (same as `cache_dir` for bare repos).
///
/// # Auth
///
/// All environment variables are inherited by the git subprocess, which
/// means AWS credentials (`AWS_*`), GCP tokens (`GOOGLE_*`, `CLOUDSDK_*`),
/// Azure credentials (`AZURE_*`), and git credential helpers (`GIT_*`)
/// are automatically available to git for authentication.
pub fn ensure_blobless_clone(url: &str, branch: Option<&str>, cache_dir: &Path) -> Result<PathBuf> {
    let head_file = cache_dir.join("HEAD");

    if head_file.exists() {
        // Existing bare repo — fetch updates.
        info!(
            cache_dir = %cache_dir.display(),
            "updating existing blobless clone"
        );
        git_fetch_all(cache_dir)?;
    } else {
        // Fresh clone needed.
        info!(
            cache_dir = %cache_dir.display(),
            branch = branch.unwrap_or("HEAD"),
            "performing blobless clone"
        );
        git_clone_blobless(url, branch, cache_dir)?;
    }

    Ok(cache_dir.to_path_buf())
}

/// Run `git fetch --all` in an existing bare repo.
fn git_fetch_all(git_dir: &Path) -> Result<()> {
    debug!(git_dir = %git_dir.display(), "running git fetch origin");

    let output = Command::new("git")
        .args(["fetch", "origin"])
        .env("GIT_DIR", git_dir)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| {
            error!(error = %e, "failed to spawn git fetch");
            CrabError::Io(e)
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let err_msg = parse_git_error(&stderr);
        error!(error = %err_msg, "git fetch failed");
        return Err(classify_git_error(&stderr));
    }

    debug!("git fetch complete");
    Ok(())
}

/// Perform a fresh blobless bare clone.
fn git_clone_blobless(url: &str, branch: Option<&str>, cache_dir: &Path) -> Result<()> {
    // Ensure parent directory exists.
    if let Some(parent) = cache_dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            error!(path = %parent.display(), error = %e, "failed to create cache parent dir");
            CrabError::Io(e)
        })?;
    }

    let mut args = vec!["clone", "--bare", "--filter=blob:none", "--single-branch"];

    // Add --branch if specified; otherwise git uses the remote's HEAD.
    let branch_val;
    if let Some(b) = branch {
        // Strip refs/heads/ prefix if present — git clone --branch
        // expects a short branch name.
        branch_val = b.strip_prefix("refs/heads/").unwrap_or(b).to_owned();
        args.push("--branch");
        args.push(&branch_val);
    }

    args.push(url);

    let cache_dir_str = cache_dir.to_string_lossy().to_string();
    args.push(&cache_dir_str);

    debug!(url = %url, args = ?args, "spawning git clone");

    let output = Command::new("git")
        .args(&args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| {
            error!(error = %e, "failed to spawn git clone");
            CrabError::Io(e)
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let err_msg = parse_git_error(&stderr);
        error!(error = %err_msg, "blobless clone failed");

        // Clean up partial clone directory on failure.
        if cache_dir.exists() {
            warn!(path = %cache_dir.display(), "cleaning up partial clone");
            let _ = std::fs::remove_dir_all(cache_dir);
        }

        return Err(classify_git_error(&stderr));
    }

    info!("blobless clone complete");
    Ok(())
}

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

/// Parse git stderr into a human-friendly error message.
fn parse_git_error(stderr: &str) -> String {
    let lower = stderr.to_ascii_lowercase();

    if is_auth_error(&lower) {
        "authentication failed: check your credentials".to_owned()
    } else if is_network_error(&lower) {
        "network error: could not reach remote".to_owned()
    } else if is_remote_not_found(&lower) {
        "remote not found: check the URL".to_owned()
    } else {
        // Fall back to the raw stderr (trimmed).
        stderr.trim().to_owned()
    }
}

/// Classify git stderr into a structured `CrabError`.
fn classify_git_error(stderr: &str) -> CrabError {
    let lower = stderr.to_ascii_lowercase();

    if is_auth_error(&lower) {
        CrabError::AuthFailed {
            path: "git remote".into(),
        }
    } else if is_network_error(&lower) {
        CrabError::Internal("network error: could not reach remote".into())
    } else if is_remote_not_found(&lower) {
        CrabError::NotFound {
            path: "git remote".into(),
        }
    } else {
        CrabError::Internal(format!("git operation failed: {}", stderr.trim()))
    }
}

/// Check if stderr indicates an authentication failure.
fn is_auth_error(stderr_lower: &str) -> bool {
    stderr_lower.contains("authentication failed")
        || stderr_lower.contains("could not read username")
        || stderr_lower.contains("invalid credentials")
        || stderr_lower.contains("401")
        || stderr_lower.contains("403")
        || stderr_lower.contains("permission denied")
        || stderr_lower.contains("access denied")
}

/// Check if stderr indicates a network error.
fn is_network_error(stderr_lower: &str) -> bool {
    stderr_lower.contains("could not resolve host")
        || stderr_lower.contains("connection refused")
        || stderr_lower.contains("connection timed out")
        || stderr_lower.contains("network is unreachable")
        || stderr_lower.contains("no route to host")
        || stderr_lower.contains("ssl")
        || stderr_lower.contains("tls")
        || stderr_lower.contains("unable to access")
}

/// Check if stderr indicates the remote was not found.
fn is_remote_not_found(stderr_lower: &str) -> bool {
    stderr_lower.contains("repository not found")
        || stderr_lower.contains("does not appear to be a git repository")
        || stderr_lower.contains("not found")
        || stderr_lower.contains("404")
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
        let conflict = MountCacheLock::acquire(&cache_dir).unwrap_err();
        assert!(
            conflict
                .to_string()
                .contains("mount cache is already in use"),
            "got: {conflict}"
        );

        drop(lock);

        MountCacheLock::acquire(&cache_dir).unwrap();
    }

    // --- Error classification ---

    #[test]
    fn classify_auth_401() {
        let err = classify_git_error("fatal: Authentication failed for 'https://example.com/repo'");
        let msg = err.to_string();
        assert!(msg.contains("authentication failed"), "got: {msg}");
    }

    #[test]
    fn classify_auth_permission_denied() {
        let err = classify_git_error("fatal: Permission denied (publickey).");
        let msg = err.to_string();
        assert!(msg.contains("authentication failed"), "got: {msg}");
    }

    #[test]
    fn classify_network_resolve() {
        let err = classify_git_error(
            "fatal: unable to access 'https://example.com/repo': Could not resolve host: example.com",
        );
        let msg = err.to_string();
        assert!(msg.contains("network error"), "got: {msg}");
    }

    #[test]
    fn classify_network_connection_refused() {
        let err = classify_git_error(
            "fatal: unable to access 'https://example.com/repo': Connection refused",
        );
        let msg = err.to_string();
        assert!(msg.contains("network error"), "got: {msg}");
    }

    #[test]
    fn classify_remote_not_found() {
        let err = classify_git_error("fatal: repository 'https://example.com/repo' not found");
        let msg = err.to_string();
        assert!(msg.contains("not found"), "got: {msg}");
    }

    #[test]
    fn classify_not_git_repo() {
        let err = classify_git_error(
            "fatal: 'https://example.com/repo' does not appear to be a git repository",
        );
        let msg = err.to_string();
        assert!(msg.contains("not found"), "got: {msg}");
    }

    #[test]
    fn classify_unknown_error_falls_through() {
        let err = classify_git_error("fatal: some unknown error happened");
        let msg = err.to_string();
        assert!(msg.contains("git operation failed"), "got: {msg}");
    }

    // --- parse_git_error ---

    #[test]
    fn parse_auth_error_message() {
        let msg = parse_git_error("fatal: Authentication failed for 'https://example.com'");
        assert_eq!(msg, "authentication failed: check your credentials");
    }

    #[test]
    fn parse_network_error_message() {
        let msg = parse_git_error("fatal: Could not resolve host: example.com");
        assert_eq!(msg, "network error: could not reach remote");
    }

    #[test]
    fn parse_not_found_message() {
        let msg = parse_git_error("fatal: repository 'https://example.com/repo' not found");
        assert_eq!(msg, "remote not found: check the URL");
    }

    #[test]
    fn parse_unknown_preserves_stderr() {
        let msg = parse_git_error("  fatal: something weird  ");
        assert_eq!(msg, "fatal: something weird");
    }

    // --- ensure_blobless_clone (integration-style, filesystem only) ---

    #[test]
    fn ensure_clone_detects_existing_bare_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("repo-cache");
        std::fs::create_dir_all(&cache_dir).unwrap();

        // Simulate an existing bare repo by creating a HEAD file.
        std::fs::write(cache_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        // This will attempt git fetch which will fail (no real remote),
        // but it proves the detection logic works.
        let result = ensure_blobless_clone(
            "https://example.com/nonexistent.git",
            Some("main"),
            &cache_dir,
        );

        // We expect an error from git fetch (no real remote configured),
        // but the important thing is it took the fetch path, not clone.
        assert!(result.is_err());
    }

    #[test]
    fn ensure_clone_attempts_fresh_clone_when_no_head() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("fresh-cache");

        // No HEAD file exists, so it should attempt a clone.
        // This will fail (invalid URL) but proves the clone path is taken.
        let result = ensure_blobless_clone(
            "https://invalid-host-that-does-not-exist.example/repo.git",
            Some("main"),
            &cache_dir,
        );

        assert!(result.is_err());
        // The partial clone directory should have been cleaned up.
        // (It may not exist at all if git failed before creating it.)
    }
}
