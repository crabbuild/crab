//! Local directory ownership coordinated with recursive cache cleanup.

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind};
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;
use tokio_util::sync::CancellationToken;

use crate::{CacheError, Result};

const CLEAN_LOCK_SUFFIX: &str = ".crab-cache-clean.lock";
const USE_LOCK_SUFFIX: &str = ".crab-cache-use.lock";

/// Exclusive ownership of a mutable cache directory, excluding cleanup.
pub struct CacheUseGuard {
    path: PathBuf,
    _lock: HeldLock,
}

struct HeldLock(File);

impl Drop for HeldLock {
    fn drop(&mut self) {
        // A concurrent fork can retain this open-file description until exec.
        // Release the lock explicitly so unrelated children cannot extend the
        // owner's lifetime; callers must first join their own cache workers.
        if let Err(error) = FileExt::unlock(&self.0) {
            tracing::warn!(%error, "failed to release cache coordination lock");
        }
    }
}

impl CacheUseGuard {
    /// Acquire nonblocking ownership before creating or mutating a cache.
    ///
    /// Returns a `WouldBlock` I/O error if an overlapping directory is owned
    /// or being cleaned. The parent directory may be created.
    pub fn acquire(path: &Path, cancel: &CancellationToken) -> Result<Self> {
        check_cancelled(cancel)?;
        let absolute = std::env::current_dir()?.join(path);
        let physical = if absolute.exists() {
            absolute.canonicalize()?
        } else {
            let parent = absolute.parent().ok_or_else(invalid_path)?;
            std::fs::create_dir_all(parent)?;
            parent
                .canonicalize()?
                .join(absolute.file_name().ok_or_else(invalid_path)?)
        };
        let lock = open_lock(&lock_path(&physical, USE_LOCK_SUFFIX)?, true)?;
        let lock = lock_exclusive(lock, &physical)?;

        // Announce ownership before checking cleaners. A cleaner announces
        // first, then probes owners; either ordering rejects one contender
        // before it can mutate directory contents.
        check_cleaners(physical.ancestors())?;
        check_users(physical.ancestors().skip(1))?;
        if physical.is_dir() {
            probe_descendants(&physical, cancel)?;
        }
        Ok(Self {
            path: physical,
            _lock: lock,
        })
    }

    /// Return the physical cache path protected by this guard.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Exclusive cleanup admission for a tree containing no active cache owners.
pub struct CacheCleanGuard {
    root: PathBuf,
    _lock: HeldLock,
}

/// Actual cache data counts, excluding persistent coordination markers.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CacheCleanStats {
    pub files_removed: u64,
    pub bytes_reclaimed: u64,
}

impl CacheCleanGuard {
    /// Announce cleanup and reject active owners before any data is removed.
    ///
    /// The root must exist. Overlapping cleanup or directory ownership returns
    /// a `WouldBlock` I/O error. Traversal is cancellable and does not follow symlinks.
    pub fn acquire(root: &Path, cancel: &CancellationToken) -> Result<Self> {
        check_cancelled(cancel)?;
        let root = root.canonicalize()?;
        let lock = open_lock(&lock_path(&root, CLEAN_LOCK_SUFFIX)?, true)?;
        let lock = lock_exclusive(lock, &root)?;
        check_cleaners(root.ancestors().skip(1))?;
        check_users(root.ancestors())?;
        probe_descendants(&root, cancel)?;
        Ok(Self { root, _lock: lock })
    }

    /// Remove cache data while retaining lock markers and their parent directories.
    ///
    /// Cancellation or I/O failure stops further deletion; removed cache data
    /// is not restored. The guard must remain alive until this call returns.
    pub fn clean(&self, cancel: &CancellationToken) -> Result<CacheCleanStats> {
        let mut stats = CacheCleanStats::default();
        walk_contents(&self.root, true, cancel, &mut stats)?;
        Ok(stats)
    }
}

/// Estimate removable cache data without acquiring locks or changing files.
pub fn cleanup_preview(root: &Path, cancel: &CancellationToken) -> Result<CacheCleanStats> {
    let mut stats = CacheCleanStats::default();
    walk_contents(root, false, cancel, &mut stats)?;
    Ok(stats)
}

fn lock_path(path: &Path, suffix: &str) -> Result<PathBuf> {
    let mut name = path.file_name().ok_or_else(invalid_path)?.to_os_string();
    name.push(suffix);
    Ok(path.with_file_name(name))
}

fn invalid_path() -> CacheError {
    Error::new(ErrorKind::InvalidInput, "cache path must name a directory").into()
}

fn open_lock(path: &Path, create: bool) -> Result<File> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(
                Error::new(ErrorKind::InvalidData, "cache lock must be a regular file").into(),
            );
        }
        Err(error) if error.kind() != ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    Ok(OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .truncate(false)
        .open(path)?)
}

fn lock_exclusive(file: File, path: &Path) -> Result<HeldLock> {
    if !FileExt::try_lock_exclusive(&file)? {
        return Err(busy(path));
    }
    Ok(HeldLock(file))
}

fn busy(path: &Path) -> CacheError {
    Error::new(
        ErrorKind::WouldBlock,
        format!(
            "cache {} is in use; retry after the other operation finishes",
            path.display()
        ),
    )
    .into()
}

fn check_cleaners<'a>(ancestors: impl Iterator<Item = &'a Path>) -> Result<()> {
    for parent in ancestors {
        if parent.file_name().is_none() {
            continue;
        }
        match open_lock(&lock_path(parent, CLEAN_LOCK_SUFFIX)?, false) {
            Ok(file) => {
                if !FileExt::try_lock_shared(&file)? {
                    return Err(busy(parent));
                }
                drop(HeldLock(file));
            }
            Err(CacheError::Io(error)) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn probe_owner(path: &Path) -> Result<()> {
    match open_lock(path, false) {
        Ok(file) => lock_exclusive(file, path).map(drop),
        Err(CacheError::Io(error)) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn check_users<'a>(ancestors: impl Iterator<Item = &'a Path>) -> Result<()> {
    for parent in ancestors.filter(|path| path.file_name().is_some()) {
        probe_owner(&lock_path(parent, USE_LOCK_SUFFIX)?)?;
    }
    Ok(())
}

fn is_marker(name: &OsStr) -> bool {
    let bytes = name.as_encoded_bytes();
    bytes.ends_with(CLEAN_LOCK_SUFFIX.as_bytes()) || bytes.ends_with(USE_LOCK_SUFFIX.as_bytes())
}

fn probe_descendants(path: &Path, cancel: &CancellationToken) -> Result<()> {
    for entry in std::fs::read_dir(path)? {
        check_cancelled(cancel)?;
        let entry = entry?;
        let name = entry.file_name();
        if is_marker(&name) {
            probe_owner(&entry.path())?;
        } else if entry.file_type()?.is_dir() {
            probe_descendants(&entry.path(), cancel)?;
        }
    }
    Ok(())
}

fn walk_contents(
    path: &Path,
    delete: bool,
    cancel: &CancellationToken,
    stats: &mut CacheCleanStats,
) -> Result<bool> {
    check_cancelled(cancel)?;
    let mut retained = false;
    for entry in std::fs::read_dir(path)? {
        check_cancelled(cancel)?;
        let entry = entry?;
        if is_marker(&entry.file_name()) {
            // Never unlink coordination inodes, even when idle: a contender
            // may already have opened one but not yet attempted its lock.
            retained = true;
        } else if entry.file_type()?.is_dir() {
            let child_retained = walk_contents(&entry.path(), delete, cancel, stats)?;
            if delete && !child_retained {
                check_cancelled(cancel)?;
                std::fs::remove_dir(entry.path())?;
            }
            retained |= child_retained;
        } else {
            let size = entry.metadata()?.len();
            if delete {
                std::fs::remove_file(entry.path())?;
            }
            stats.files_removed = stats.files_removed.saturating_add(1);
            stats.bytes_reclaimed = stats.bytes_reclaimed.saturating_add(size);
        }
    }
    Ok(retained)
}

fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(CacheError::Cancelled);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
