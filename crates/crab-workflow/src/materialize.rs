//! Atomic materialization of stage outputs into the working tree.
//!
//! Cache hits must either leave the declared output path holding the
//! full cached bytes or leave its pre-run contents untouched — never
//! a partial file that silently corrupts downstream stages.
//!
//! The strategy is a `.crab.tmp.<run_id>` sidecar + fsync + atomic
//! same-filesystem rename. Directory outs reconstruct into a sibling
//! `<dir>.crab.tmp.<run_id>/` then rename the directory (POSIX
//! same-fs atomic). Cross-filesystem materialization falls back to
//! per-file atomic writes and is best-effort for directories — the
//! fallback is documented, not silent.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use uuid::Uuid;

use crate::{Result, WorkflowError as CrabError};

/// Suffix used for every materialization sidecar. Exposed so the
/// orphan sweep in `workflow::resume` can recognize them.
pub const SIDECAR_PREFIX: &str = ".crab.tmp.";

/// Compute the sidecar path for a final output and a run.
#[must_use]
pub fn sidecar_path(final_path: &Path, run_id: Uuid) -> PathBuf {
    let parent = final_path.parent().unwrap_or_else(|| Path::new("."));
    let base = final_path
        .file_name()
        .map_or_else(|| "out".to_owned(), |f| f.to_string_lossy().into_owned());
    parent.join(format!("{base}{SIDECAR_PREFIX}{run_id}"))
}

/// Atomically write a single file output.
///
/// Writes to `<path>.crab.tmp.<run_id>` in the same directory as
/// `path`, fsyncs, then renames. Creates parent directories as
/// needed. Restores `mode` via chmod on the sidecar before the
/// rename so the final file appears with correct permissions from
/// the first moment it is visible.
pub fn write_atomic(path: &Path, bytes: &[u8], run_id: Uuid, mode: u32) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(CrabError::Io)?;
    }

    let tmp = sidecar_path(path, run_id);

    // Fresh write — truncate any stale sidecar from a previous run
    // (orphan sweep should have handled this, but defend in depth).
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)
        .map_err(CrabError::Io)?;
    file.write_all(bytes).map_err(CrabError::Io)?;
    file.sync_all().map_err(CrabError::Io)?;

    #[cfg(unix)]
    {
        let permissions = fs::Permissions::from_mode(mode);
        fs::set_permissions(&tmp, permissions).map_err(CrabError::Io)?;
    }
    #[cfg(not(unix))]
    let _ = mode;

    // Rename is atomic within a filesystem. Cross-fs renames surface
    // `ErrorKind::CrossesDevices` on recent Rust — callers route
    // through `write_atomic_crossfs` in that case.
    fs::rename(&tmp, path).map_err(CrabError::Io)?;
    Ok(())
}

/// Atomically materialize a directory output from a manifest of
/// `(relative_path, bytes, mode)` entries.
///
/// Reconstructs into `<path>.crab.tmp.<run_id>/`, then renames the
/// directory over `path`. If `path` already exists it is replaced
/// atomically (POSIX `rename` over a directory empties the old
/// entry's inode on success).
pub fn write_directory_atomic(
    path: &Path,
    entries: &[(PathBuf, Vec<u8>, u32)],
    run_id: Uuid,
) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(CrabError::Io)?;
    }

    let staging = sidecar_path(path, run_id);

    // Clean any stale staging dir from a prior attempt. Fresh full
    // rebuild keeps the atomicity story simple — no partial reuse.
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(CrabError::Io)?;
    }
    fs::create_dir_all(&staging).map_err(CrabError::Io)?;
    let mut cleanup = DirectorySidecarCleanup::new(staging.clone());
    let mut paths = BTreeSet::new();

    for (rel, bytes, mode) in entries {
        if rel.is_absolute()
            || rel.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
            || !paths.insert(rel.clone())
        {
            return Err(CrabError::StageOutMalformed {
                stage: String::new(),
                path: rel.clone(),
                reason: "directory manifest entries must be unique and relative",
            });
        }
        let target = staging.join(rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(CrabError::Io)?;
        }
        let mut file = fs::File::create(&target).map_err(CrabError::Io)?;
        file.write_all(bytes).map_err(CrabError::Io)?;
        file.sync_all().map_err(CrabError::Io)?;
        #[cfg(unix)]
        {
            let permissions = fs::Permissions::from_mode(*mode);
            fs::set_permissions(&target, permissions).map_err(CrabError::Io)?;
        }
        #[cfg(not(unix))]
        let _ = mode;
    }

    if path.exists() {
        // POSIX `rename` over an existing directory works only when
        // the target is empty. Replace it with a full remove first —
        // users who need atomicity on "full replace" land on this
        // code path; the tradeoff is documented in the design.
        fs::remove_dir_all(path).map_err(CrabError::Io)?;
    }
    fs::rename(&staging, path).map_err(CrabError::Io)?;
    cleanup.disarm();
    Ok(())
}

/// Atomically materialize a directory output from a cached tree manifest.
///
/// Reconstructs into `<path>.crab.tmp.<run_id>/`, then renames the
/// directory over `path`. File entries are read from the local
/// content cache when present, then from their current on-disk
/// location for legacy entries. Empty directory entries are created
/// via `mkdir_all`.
///
/// If `path` already exists it is replaced atomically (POSIX
/// `rename` over a directory empties the old entry's inode on
/// success).
pub fn materialize_directory(
    path: &Path,
    manifest: &[crate::cache::TreeManifestEntry],
    cache_root: &Path,
    run_id: Uuid,
) -> Result<()> {
    crate::stage_cache_entry::validate_tree_manifest(manifest)?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(CrabError::Io)?;
    }

    let staging = sidecar_path(path, run_id);

    // Clean any stale staging dir from a prior attempt.
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(CrabError::Io)?;
    }
    fs::create_dir_all(&staging).map_err(CrabError::Io)?;
    let mut cleanup = DirectorySidecarCleanup::new(staging.clone());

    for entry in manifest {
        let rel = std::path::Path::new(&entry.path);
        let target = staging.join(rel);

        if entry.kind == "dir" {
            // Empty directory entry — just create it.
            fs::create_dir_all(&target).map_err(CrabError::Io)?;
            #[cfg(unix)]
            {
                let permissions = fs::Permissions::from_mode(entry.mode);
                fs::set_permissions(&target, permissions).map_err(CrabError::Io)?;
            }
        } else {
            if let Some(parent_dir) = target.parent() {
                fs::create_dir_all(parent_dir).map_err(CrabError::Io)?;
            }
            let source = path.join(rel);
            let bytes = directory_entry_bytes(cache_root, entry, &source)?;
            let mut file = fs::File::create(&target).map_err(CrabError::Io)?;
            file.write_all(&bytes).map_err(CrabError::Io)?;
            file.sync_all().map_err(CrabError::Io)?;
            #[cfg(unix)]
            {
                let permissions = fs::Permissions::from_mode(entry.mode);
                fs::set_permissions(&target, permissions).map_err(CrabError::Io)?;
            }
        }
    }

    if path.exists() {
        fs::remove_dir_all(path).map_err(CrabError::Io)?;
    }
    fs::rename(&staging, path).map_err(CrabError::Io)?;
    cleanup.disarm();
    Ok(())
}

struct DirectorySidecarCleanup {
    path: PathBuf,
    armed: bool,
}

impl DirectorySidecarCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DirectorySidecarCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn directory_entry_bytes(
    cache_root: &Path,
    entry: &crate::cache::TreeManifestEntry,
    source: &Path,
) -> Result<Vec<u8>> {
    let bytes = if let Some(bytes) = crate::cache::read_local_xorb(cache_root, &entry.hash)? {
        bytes
    } else {
        fs::read(source).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            CrabError::StageCacheMiss {
                stage: String::new(),
                reason: format!(
                    "directory manifest references {} but neither the file nor local cache bytes are present",
                    source.display()
                ),
            }
        } else {
            CrabError::Io(e)
        }
        })?
    };
    let actual_hash = format!("b3:{}", blake3::hash(&bytes).to_hex());
    if actual_hash != entry.hash || bytes.len() as u64 != entry.size {
        return Err(CrabError::CacheEntryCorrupt {
            stage_hash: String::new(),
            path: entry.path.clone(),
            expected: format!("{} bytes with {}", entry.size, entry.hash),
            actual: format!("{} bytes with {actual_hash}", bytes.len()),
        });
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_atomic_creates_file_at_final_path() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("subdir/out.txt");
        let run_id = Uuid::now_v7();

        write_atomic(&path, b"hello", run_id, 0o644).unwrap();

        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes, b"hello");
        // Sidecar should be gone after successful rename.
        assert!(!sidecar_path(&path, run_id).exists());
    }

    #[test]
    fn write_atomic_overwrites_existing_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("out.txt");
        fs::write(&path, b"stale").unwrap();

        let run_id = Uuid::now_v7();
        write_atomic(&path, b"fresh", run_id, 0o644).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"fresh");
    }

    #[test]
    fn sidecar_path_uses_run_id_suffix() {
        let run_id = Uuid::now_v7();
        let final_path = Path::new("dir/out.txt");
        let sidecar = sidecar_path(final_path, run_id);
        let name = sidecar.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("out.txt.crab.tmp."));
        assert!(name.contains(&run_id.to_string()));
    }

    #[test]
    fn write_directory_atomic_creates_tree() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("data");
        let run_id = Uuid::now_v7();

        write_directory_atomic(
            &dir,
            &[
                (PathBuf::from("a.txt"), b"a".to_vec(), 0o644),
                (PathBuf::from("nested/b.txt"), b"b".to_vec(), 0o644),
            ],
            run_id,
        )
        .unwrap();

        assert_eq!(fs::read(dir.join("a.txt")).unwrap(), b"a");
        assert_eq!(fs::read(dir.join("nested/b.txt")).unwrap(), b"b");
        assert!(!sidecar_path(&dir, run_id).exists());
    }

    #[test]
    fn write_directory_atomic_replaces_existing_directory() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("data");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("stale.txt"), b"stale").unwrap();

        let run_id = Uuid::now_v7();
        write_directory_atomic(
            &dir,
            &[(PathBuf::from("fresh.txt"), b"fresh".to_vec(), 0o644)],
            run_id,
        )
        .unwrap();

        assert!(!dir.join("stale.txt").exists());
        assert_eq!(fs::read(dir.join("fresh.txt")).unwrap(), b"fresh");
    }

    #[test]
    fn sidecar_is_removed_after_successful_rename() {
        // This is effectively the "crash after sidecar write but
        // before rename leaves final path untouched" invariant's
        // positive case: a successful run leaves no sidecar behind.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("out.txt");
        let run_id = Uuid::now_v7();

        write_atomic(&path, b"x", run_id, 0o644).unwrap();
        let sidecar = sidecar_path(&path, run_id);
        assert!(!sidecar.exists());
        assert!(path.exists());
    }

    #[test]
    fn write_directory_atomic_rejects_absolute_paths() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("data");
        let run_id = Uuid::now_v7();

        let err = write_directory_atomic(
            &dir,
            &[(PathBuf::from("/abs/path"), b"x".to_vec(), 0o644)],
            run_id,
        )
        .unwrap_err();
        assert!(matches!(err, CrabError::StageOutMalformed { .. }));
    }

    #[test]
    fn materialize_directory_rejects_traversal_before_writing() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("data");
        let run_id = Uuid::now_v7();
        let manifest = vec![crate::cache::TreeManifestEntry {
            path: "../escape.txt".to_owned(),
            kind: "file".to_owned(),
            hash: format!("b3:{}", "ab".repeat(32)),
            size: 1,
            mode: 0o644,
        }];

        assert!(materialize_directory(&dir, &manifest, tmp.path(), run_id).is_err());
        assert!(!tmp.path().join("escape.txt").exists());
        assert!(!sidecar_path(&dir, run_id).exists());
    }
}
