//! Mount metadata registry (`~/.crab/mounts/mounts.json`).
//!
//! Tracks active mounts so that Crab commands can discover and manage running
//! mount processes.
//!
//! ## Locking protocol
//!
//! Advisory file locking (`flock`) on a separate `mounts.json.lock` file
//! protects against races between concurrent writers. All public functions
//! (`add_entry`, `remove_entry`, `list_entries`) acquire this lock before
//! reading or writing the data file. `write_entries` uses atomic rename
//! (write to `.tmp`, then rename) so torn writes are never observable even
//! if a reader bypasses the lock. External consumers must use the public API
//! or acquire the same lock file to ensure consistency.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::core::error::{CrabError, Result};

// ---------------------------------------------------------------------------
// Mount entry
// ---------------------------------------------------------------------------

/// A single entry in the mounts registry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MountEntry {
    /// Local path where the virtual filesystem is mounted.
    pub mountpoint: String,
    /// Source repository (remote URL or local path).
    pub source: String,
    /// Git ref being tracked (e.g. `refs/heads/main`).
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// PID of the background mount process.
    pub pid: u32,
    /// ISO 8601 timestamp when the mount was started.
    pub start_time: String,
    /// Whether the mount is read-only.
    pub read_only: bool,
    /// Human-friendly name for this mount.
    pub name: String,
    /// Backend that owns the running mount process, such as `nfs` or `fuse`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Path to backend helper logs, when the mount runs through a helper.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
    /// Local control endpoint for backend-agnostic mount operations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_endpoint: Option<String>,
}

// ---------------------------------------------------------------------------
// Registry path
// ---------------------------------------------------------------------------

/// Return the path to `~/.crab/mounts/mounts.json`.
pub fn registry_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| CrabError::Configuration {
        key: "HOME environment variable not set".into(),
        origin: "mounts_registry".into(),
    })?;
    Ok(PathBuf::from(home)
        .join(".crab")
        .join("mounts")
        .join("mounts.json"))
}

/// Return the path to `mounts.json` given a custom base directory.
///
/// Useful for testing without touching the real home directory.
pub fn registry_path_in(base_dir: &Path) -> PathBuf {
    base_dir.join("mounts.json")
}

// ---------------------------------------------------------------------------
// Read / write with file locking
// ---------------------------------------------------------------------------

/// Read all mount entries from the registry file.
///
/// Returns an empty vec if the file does not exist.
pub fn read_entries(path: &Path) -> Result<Vec<MountEntry>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let content = fs::read_to_string(path).map_err(|e| {
        CrabError::Internal(format!(
            "failed to read mounts registry at {}: {e}",
            path.display()
        ))
    })?;

    if content.trim().is_empty() {
        return Ok(Vec::new());
    }

    let entries: Vec<MountEntry> = serde_json::from_str(&content).map_err(|e| {
        CrabError::Internal(format!(
            "failed to parse mounts registry at {}: {e}",
            path.display()
        ))
    })?;

    Ok(entries)
}

/// Write mount entries to the registry file atomically.
///
/// Writes to a temporary file first, then renames to avoid partial writes.
fn write_entries(path: &Path, entries: &[MountEntry]) -> Result<()> {
    // Ensure parent directory exists.
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            CrabError::Internal(format!(
                "failed to create mounts registry directory {}: {e}",
                parent.display()
            ))
        })?;
        #[cfg(unix)]
        secure_registry_dir(parent)?;
    }

    let json = serde_json::to_string_pretty(entries)
        .map_err(|e| CrabError::Internal(format!("failed to serialize mounts registry: {e}")))?;

    // Write to a temp file in the same directory, then rename for atomicity.
    let tmp_path = path.with_extension("json.tmp");
    fs::write(&tmp_path, json.as_bytes()).map_err(|e| {
        CrabError::Internal(format!(
            "failed to write temp mounts registry at {}: {e}",
            tmp_path.display()
        ))
    })?;
    #[cfg(unix)]
    secure_registry_file(&tmp_path)?;

    fs::rename(&tmp_path, path).map_err(|e| {
        CrabError::Internal(format!(
            "failed to rename temp mounts registry to {}: {e}",
            path.display()
        ))
    })?;
    #[cfg(unix)]
    secure_registry_file(path)?;

    Ok(())
}

#[cfg(unix)]
fn secure_registry_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(CrabError::Io)
}

#[cfg(unix)]
fn secure_registry_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(CrabError::Io)
}

/// Acquire an advisory lock on the registry file for exclusive access.
///
/// Returns the lock file handle which must be held for the duration of
/// the read-modify-write operation. The lock is released when the handle
/// is dropped.
#[cfg(unix)]
fn acquire_lock(path: &Path) -> Result<File> {
    let lock_path = path.with_extension("json.lock");

    // Ensure parent directory exists.
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(CrabError::Io)?;
        secure_registry_dir(parent)?;
    }

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| {
            CrabError::Internal(format!(
                "failed to open lock file {}: {e}",
                lock_path.display()
            ))
        })?;
    secure_registry_file(&lock_path)?;

    // SAFETY: flock with LOCK_EX is safe on a valid file descriptor.
    let ret = unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) };
    if ret != 0 {
        return Err(CrabError::Io(std::io::Error::last_os_error()));
    }

    Ok(file)
}

#[cfg(not(unix))]
fn acquire_lock(path: &Path) -> Result<File> {
    // On non-Unix platforms, skip locking (best-effort).
    let lock_path = path.with_extension("json.lock");
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(CrabError::Io)?;
    }
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| {
            CrabError::Internal(format!(
                "failed to open lock file {}: {e}",
                lock_path.display()
            ))
        })
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Add a mount entry to the registry.
///
/// Acquires a file lock, reads existing entries, appends the new entry,
/// and writes back atomically. If an entry with the same mountpoint
/// already exists, it is replaced.
pub fn add_entry(path: &Path, entry: MountEntry) -> Result<()> {
    let _lock = acquire_lock(path)?;

    let mut entries = read_entries(path)?;

    // Remove any existing entry for the same mountpoint.
    entries.retain(|e| e.mountpoint != entry.mountpoint);

    debug!(
        mountpoint = %entry.mountpoint,
        pid = entry.pid,
        "adding mount entry to registry"
    );

    entries.push(entry);
    write_entries(path, &entries)?;

    Ok(())
}

/// Remove a mount entry by mountpoint.
///
/// Returns `true` if an entry was removed, `false` if not found.
pub fn remove_entry(path: &Path, mountpoint: &str) -> Result<bool> {
    let _lock = acquire_lock(path)?;

    let mut entries = read_entries(path)?;
    let original_len = entries.len();
    entries.retain(|e| e.mountpoint != mountpoint);

    if entries.len() == original_len {
        return Ok(false);
    }

    debug!(mountpoint = %mountpoint, "removed mount entry from registry");
    write_entries(path, &entries)?;

    Ok(true)
}

/// Remove a mount entry by PID.
///
/// Returns `true` if an entry was removed, `false` if not found.
pub fn remove_entry_by_pid(path: &Path, pid: u32) -> Result<bool> {
    let _lock = acquire_lock(path)?;

    let mut entries = read_entries(path)?;
    let original_len = entries.len();
    entries.retain(|e| e.pid != pid);

    if entries.len() == original_len {
        return Ok(false);
    }

    debug!(pid = pid, "removed mount entry by PID from registry");
    write_entries(path, &entries)?;

    Ok(true)
}

/// List all entries in the registry.
pub fn list_entries(path: &Path) -> Result<Vec<MountEntry>> {
    let _lock = acquire_lock(path)?;
    read_entries(path)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Derive a human-friendly name from a source URL or path.
///
/// Takes the last path component of the source. For example:
/// - `crab://bucket/ml-models` → `ml-models`
/// - `/home/user/my-repo` → `my-repo`
/// - `s3://bucket/org/data` → `data`
pub fn derive_name_from_source(source: &str) -> String {
    // Strip trailing slashes.
    let trimmed = source.trim_end_matches('/');

    // Try to extract the last path component after `://`.
    if let Some((_scheme, path)) = trimmed.split_once("://") {
        path.rsplit('/').next().unwrap_or(path).to_owned()
    } else {
        // Local path — use the last component.
        Path::new(trimmed)
            .file_name()
            .map_or_else(|| trimmed.to_owned(), |n| n.to_string_lossy().into_owned())
    }
}

/// Get the current time as an ISO 8601 string (UTC).
pub fn now_iso8601() -> String {
    use std::time::SystemTime;

    let Ok(duration) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) else {
        tracing::warn!("system clock is before UNIX epoch; using 0 as fallback");
        return crab_types::time::from_epoch_millis(0);
    };
    let secs = duration.as_secs();

    format_unix_timestamp(secs)
}

/// Format a Unix timestamp as ISO 8601 UTC string.
pub fn format_unix_timestamp(secs: u64) -> String {
    // Days since epoch.
    let days = secs / 86400;
    let time_of_day = secs % 86400;

    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Civil date from days since 1970-01-01 (algorithm from Howard Hinnant).
    let (year, month, day) = civil_from_days(days as i64);

    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

/// Convert days since 1970-01-01 to (year, month, day).
/// Algorithm by Howard Hinnant.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = i64::from(yoe) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year as i32, m, d)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    fn sample_entry(mountpoint: &str, pid: u32) -> MountEntry {
        MountEntry {
            mountpoint: mountpoint.to_owned(),
            source: "crab://bucket/ml-models".to_owned(),
            git_ref: "refs/heads/main".to_owned(),
            pid,
            start_time: "2024-01-15T10:30:00Z".to_owned(),
            read_only: false,
            name: "ml-models".to_owned(),
            backend: None,
            log_path: None,
            control_endpoint: None,
        }
    }

    #[test]
    fn serialize_deserialize_roundtrip() {
        let entries = vec![
            sample_entry("/mnt/models", 12345),
            sample_entry("/mnt/code", 67890),
        ];

        let json = serde_json::to_string_pretty(&entries).unwrap();
        let parsed: Vec<MountEntry> = serde_json::from_str(&json).unwrap();

        assert_eq!(entries, parsed);
    }

    #[test]
    fn serialize_matches_expected_schema() {
        let entry = sample_entry("/mnt/models", 12345);
        let json = serde_json::to_string_pretty(&[&entry]).unwrap();

        // Verify the JSON contains the expected field names.
        assert!(json.contains("\"mountpoint\""));
        assert!(json.contains("\"source\""));
        assert!(json.contains("\"ref\""));
        assert!(json.contains("\"pid\""));
        assert!(json.contains("\"start_time\""));
        assert!(json.contains("\"read_only\""));
        assert!(json.contains("\"name\""));

        // Verify the "ref" field is serialized correctly (not "git_ref").
        assert!(!json.contains("\"git_ref\""));
    }

    #[test]
    fn deserialize_from_expected_json() {
        let json = r#"[
            {
                "mountpoint": "/mnt/models",
                "source": "crab://bucket/ml-models",
                "ref": "refs/heads/main",
                "pid": 12345,
                "start_time": "2024-01-15T10:30:00Z",
                "read_only": false,
                "name": "ml-models"
            }
        ]"#;

        let entries: Vec<MountEntry> = serde_json::from_str(json).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].mountpoint, "/mnt/models");
        assert_eq!(entries[0].git_ref, "refs/heads/main");
        assert_eq!(entries[0].pid, 12345);
        assert!(!entries[0].read_only);
        assert_eq!(entries[0].backend, None);
        assert_eq!(entries[0].log_path, None);
        assert_eq!(entries[0].control_endpoint, None);
    }

    #[test]
    fn add_entry_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mounts.json");

        let entry = sample_entry("/mnt/models", 1234);
        add_entry(&path, entry.clone()).unwrap();

        let entries = read_entries(&path).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], entry);
    }

    #[test]
    #[cfg(unix)]
    fn add_entry_writes_private_registry_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mounts").join("mounts.json");

        add_entry(&path, sample_entry("/mnt/models", 1234)).unwrap();

        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let lock_mode = std::fs::metadata(path.with_extension("json.lock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);
        assert_eq!(lock_mode, 0o600);
    }

    #[test]
    fn add_entry_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mounts.json");

        add_entry(&path, sample_entry("/mnt/a", 100)).unwrap();
        add_entry(&path, sample_entry("/mnt/b", 200)).unwrap();

        let entries = read_entries(&path).unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn add_entry_replaces_same_mountpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mounts.json");

        add_entry(&path, sample_entry("/mnt/a", 100)).unwrap();
        add_entry(&path, sample_entry("/mnt/a", 200)).unwrap();

        let entries = read_entries(&path).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].pid, 200);
    }

    #[test]
    fn remove_entry_by_mountpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mounts.json");

        add_entry(&path, sample_entry("/mnt/a", 100)).unwrap();
        add_entry(&path, sample_entry("/mnt/b", 200)).unwrap();

        let removed = remove_entry(&path, "/mnt/a").unwrap();
        assert!(removed);

        let entries = read_entries(&path).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].mountpoint, "/mnt/b");
    }

    #[test]
    fn remove_entry_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mounts.json");

        add_entry(&path, sample_entry("/mnt/a", 100)).unwrap();

        let removed = remove_entry(&path, "/mnt/nonexistent").unwrap();
        assert!(!removed);
    }

    #[test]
    fn remove_entry_by_pid_works() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mounts.json");

        add_entry(&path, sample_entry("/mnt/a", 100)).unwrap();
        add_entry(&path, sample_entry("/mnt/b", 200)).unwrap();

        let removed = remove_entry_by_pid(&path, 100).unwrap();
        assert!(removed);

        let entries = read_entries(&path).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].pid, 200);
    }

    #[test]
    fn read_entries_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mounts.json");
        fs::write(&path, "").unwrap();

        let entries = read_entries(&path).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn read_entries_nonexistent_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nonexistent.json");

        let entries = read_entries(&path).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn derive_name_from_remote_url() {
        assert_eq!(
            derive_name_from_source("crab://bucket/ml-models"),
            "ml-models"
        );
        assert_eq!(derive_name_from_source("s3://bucket/org/data"), "data");
        assert_eq!(derive_name_from_source("gs://bucket/repo/"), "repo");
    }

    #[test]
    fn derive_name_from_local_path() {
        assert_eq!(derive_name_from_source("/home/user/my-repo"), "my-repo");
        assert_eq!(derive_name_from_source("/path/to/project/"), "project");
        assert_eq!(derive_name_from_source("./relative/repo"), "repo");
    }

    #[test]
    fn now_iso8601_format() {
        let ts = now_iso8601();
        // Should match YYYY-MM-DDTHH:MM:SSZ pattern.
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[13..14], ":");
        assert_eq!(&ts[16..17], ":");
    }

    #[test]
    fn format_known_timestamp() {
        // 2024-01-15T10:30:00Z = 1705314600 seconds since epoch.
        let ts = format_unix_timestamp(1_705_314_600);
        assert_eq!(ts, "2024-01-15T10:30:00Z");
    }

    #[test]
    fn format_epoch() {
        let ts = format_unix_timestamp(0);
        assert_eq!(ts, "1970-01-01T00:00:00Z");
    }

    #[test]
    fn list_entries_with_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mounts.json");

        add_entry(&path, sample_entry("/mnt/a", 100)).unwrap();
        add_entry(&path, sample_entry("/mnt/b", 200)).unwrap();

        let entries = list_entries(&path).unwrap();
        assert_eq!(entries.len(), 2);
    }
}
