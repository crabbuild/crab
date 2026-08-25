//! `crab cache clean` — reclaim disk space used by the local cache.
//!
//! The local cache lives at `~/.cache/crab/` (or `$XDG_CACHE_HOME/crab/`)
//! and stores downloaded shards, xorbs, persistent chunk indices, workflow
//! stage entries, and bloom filters.
//! This command removes all cached data.

use std::path::{Path, PathBuf};

use crate::core::config::Config;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::OutputMode;
use tokio_util::sync::CancellationToken;

/// Summary of a cache clean operation.
#[derive(Debug, Default, serde::Serialize)]
pub struct CacheCleanSummary {
    pub files_removed: u64,
    pub bytes_reclaimed: u64,
    pub dry_run: bool,
}

/// Summary of a cache verify operation.
#[derive(Debug, Default, serde::Serialize)]
pub struct CacheVerifySummary {
    pub objects_checked: u64,
    pub objects_valid: u64,
    pub objects_corrupt: u64,
}

/// Remove all files under the cache root directory.
///
/// When `dry_run` is true, reports what would be removed without deleting.
pub fn run_cache_clean(dry_run: bool, mode: OutputMode) -> Result<CacheCleanSummary> {
    run_cache_clean_with_cancel(dry_run, mode, &CancellationToken::new())
}

/// Remove cache contents while honoring the caller's cancellation token.
pub fn run_cache_clean_with_cancel(
    dry_run: bool,
    mode: OutputMode,
    cancel: &CancellationToken,
) -> Result<CacheCleanSummary> {
    check_cancelled(cancel)?;
    let config = Config::resolve_local()?;
    let targets = clean_targets(
        crate::cache::default_cache_root(),
        config.effective_chunk_cache_dir(),
    )?;
    if targets.is_empty() {
        if !mode.is_machine() {
            println!("Cache directories do not exist.");
        }
        return Ok(CacheCleanSummary {
            dry_run,
            ..Default::default()
        });
    }

    let mut files = 0u64;
    let mut bytes = 0u64;
    for target in &targets {
        check_cancelled(cancel)?;
        let (target_files, target_bytes) = walk_dir_size_with_cancel(target, cancel)?;
        files = files.saturating_add(target_files);
        bytes = bytes.saturating_add(target_bytes);
    }

    if dry_run {
        if !mode.is_machine() {
            println!(
                "Would remove {} file(s), reclaiming {}",
                files,
                format_bytes(bytes),
            );
        }
        return Ok(CacheCleanSummary {
            files_removed: files,
            bytes_reclaimed: bytes,
            dry_run: true,
        });
    }

    for target in &targets {
        check_cancelled(cancel)?;
        remove_dir_contents_with_cancel(target, cancel)?;
    }

    if !mode.is_machine() {
        println!(
            "Removed {} file(s), reclaimed {}",
            files,
            format_bytes(bytes),
        );
    }

    Ok(CacheCleanSummary {
        files_removed: files,
        bytes_reclaimed: bytes,
        dry_run: false,
    })
}

fn clean_targets(object_root: PathBuf, chunk_root: PathBuf) -> Result<Vec<PathBuf>> {
    let mut targets = Vec::new();
    for path in [object_root, chunk_root] {
        if !path.exists() {
            continue;
        }
        let canonical = std::fs::canonicalize(&path)?;
        validate_destructive_cache_root(&canonical)?;
        if targets
            .iter()
            .any(|root: &PathBuf| canonical.starts_with(root))
        {
            continue;
        }
        targets.retain(|root| !root.starts_with(&canonical));
        targets.push(canonical);
    }
    Ok(targets)
}

fn validate_destructive_cache_root(path: &Path) -> Result<()> {
    let cwd = std::fs::canonicalize(std::env::current_dir()?)?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.exists())
        .map(std::fs::canonicalize)
        .transpose()?;
    let unsafe_path = path.parent().is_none()
        || home.as_ref().is_some_and(|home| path == home)
        || cwd.starts_with(path);
    if unsafe_path {
        return Err(CrabError::Configuration {
            key: "cache directory is unsafe for recursive cleanup".to_owned(),
            origin: path.display().to_string(),
        });
    }
    Ok(())
}

/// Verify content-addressed local cache objects and evict corrupt entries.
pub async fn run_cache_verify(mode: OutputMode) -> Result<CacheVerifySummary> {
    run_cache_verify_with_cancel(mode, &CancellationToken::new()).await
}

/// Verify content-addressed cache objects while honoring cancellation.
pub async fn run_cache_verify_with_cancel(
    mode: OutputMode,
    cancel: &CancellationToken,
) -> Result<CacheVerifySummary> {
    check_cancelled(cancel)?;
    let config = Config::resolve_local()?;
    let cache = crate::cache::LocalCache::new(crate::cache::default_cache_root());
    let local_verify = cache.verify();
    tokio::pin!(local_verify);
    let report = tokio::select! {
        () = cancel.cancelled() => return Err(CrabError::Cancelled),
        result = &mut local_verify => result?,
    };
    check_cancelled(cancel)?;
    let xet = crate::cache::verify_xet_chunk_cache_with_cancel(
        &config.effective_chunk_cache_dir(),
        cancel,
    )
    .await?;
    check_cancelled(cancel)?;
    let summary = CacheVerifySummary {
        objects_checked: report.total.saturating_add(xet.total),
        objects_valid: report.valid.saturating_add(xet.valid),
        objects_corrupt: report.corrupt.saturating_add(xet.corrupt),
    };

    if mode.is_machine() {
        let json = serde_json::to_string(&summary).map_err(|e| {
            crate::core::error::CrabError::Internal(format!(
                "failed to serialize cache verify summary: {e}"
            ))
        })?;
        println!("{json}");
    } else {
        println!(
            "Checked {} cache object(s): {} valid, {} corrupt evicted",
            summary.objects_checked, summary.objects_valid, summary.objects_corrupt,
        );
    }

    Ok(summary)
}

/// Walk a directory tree and return (file_count, total_bytes).
#[cfg(test)]
fn walk_dir_size(dir: &Path) -> Result<(u64, u64)> {
    walk_dir_size_with_cancel(dir, &CancellationToken::new())
}

fn walk_dir_size_with_cancel(dir: &Path, cancel: &CancellationToken) -> Result<(u64, u64)> {
    let mut files = 0u64;
    let mut bytes = 0u64;
    walk_dir_size_inner(dir, &mut files, &mut bytes, cancel)?;
    Ok((files, bytes))
}

fn walk_dir_size_inner(
    dir: &Path,
    files: &mut u64,
    bytes: &mut u64,
    cancel: &CancellationToken,
) -> Result<()> {
    check_cancelled(cancel)?;
    let entries = std::fs::read_dir(dir).map_err(|e| {
        crate::core::error::CrabError::Internal(format!(
            "failed to read directory {}: {e}",
            dir.display()
        ))
    })?;

    for entry in entries {
        check_cancelled(cancel)?;
        let entry = entry.map_err(|e| {
            crate::core::error::CrabError::Internal(format!("dir entry error: {e}"))
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(CrabError::Io)?;
        if file_type.is_dir() {
            walk_dir_size_inner(&path, files, bytes, cancel)?;
        } else {
            *files += 1;
            *bytes = bytes.saturating_add(entry.metadata().map_or(0, |meta| meta.len()));
        }
    }

    Ok(())
}

/// Remove all files and subdirectories inside `dir`, but keep `dir` itself.
#[cfg(test)]
fn remove_dir_contents(dir: &Path) -> Result<()> {
    remove_dir_contents_with_cancel(dir, &CancellationToken::new())
}

fn remove_dir_contents_with_cancel(dir: &Path, cancel: &CancellationToken) -> Result<()> {
    check_cancelled(cancel)?;
    for entry in std::fs::read_dir(dir)? {
        check_cancelled(cancel)?;
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            remove_dir_contents_with_cancel(&path, cancel)?;
            check_cancelled(cancel)?;
            std::fs::remove_dir(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn walk_dir_size_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let (files, bytes) = walk_dir_size(dir.path()).unwrap();
        assert_eq!(files, 0);
        assert_eq!(bytes, 0);
    }

    #[test]
    fn walk_dir_size_counts_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"world!").unwrap();
        let (files, bytes) = walk_dir_size(dir.path()).unwrap();
        assert_eq!(files, 2);
        assert_eq!(bytes, 11);
    }

    #[test]
    fn remove_dir_contents_clears_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.txt"), b"world").unwrap();
        remove_dir_contents(dir.path()).unwrap();
        assert!(dir.path().exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn cache_clean_walk_honors_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let error = walk_dir_size_with_cancel(dir.path(), &cancel).unwrap_err();
        assert!(matches!(error, CrabError::Cancelled));
        assert!(dir.path().join("a.txt").exists());
    }

    #[test]
    fn cache_clean_delete_honors_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let error = remove_dir_contents_with_cancel(dir.path(), &cancel).unwrap_err();
        assert!(matches!(error, CrabError::Cancelled));
        assert!(dir.path().join("a.txt").exists());
    }

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1_048_576), "1.0 MB");
    }

    #[test]
    fn clean_targets_deduplicates_nested_chunk_cache() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("cache");
        let chunks = root.join("chunks");
        std::fs::create_dir_all(&chunks).unwrap();

        let targets = clean_targets(root.clone(), chunks).unwrap();

        assert_eq!(targets, vec![std::fs::canonicalize(root).unwrap()]);
    }

    #[test]
    fn clean_rejects_current_working_directory() {
        let cwd = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();

        let error = validate_destructive_cache_root(&cwd).unwrap_err();

        assert!(matches!(error, CrabError::Configuration { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn remove_dir_contents_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("keep"), b"data").unwrap();
        symlink(outside.path(), root.path().join("link")).unwrap();

        remove_dir_contents(root.path()).unwrap();

        assert!(outside.path().join("keep").exists());
    }
}
