//! `crab cache clean` — reclaim disk space used by the local cache.
//!
//! The local cache lives at `~/.cache/crab/` (or `$XDG_CACHE_HOME/crab/`)
//! and stores downloaded shards, xorbs, persistent chunk indices, workflow
//! stage entries, and bloom filters.
//! This command removes all cached data.

use std::path::Path;

use crate::core::error::Result;
use crate::core::output::OutputMode;

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
    let cache_root = crate::cache::default_cache_root();

    if !cache_root.exists() {
        if !mode.is_machine() {
            println!("Cache directory does not exist: {}", cache_root.display());
        }
        return Ok(CacheCleanSummary {
            dry_run,
            ..Default::default()
        });
    }

    let (files, bytes) = walk_dir_size(&cache_root)?;

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

    // Remove all contents but keep the root directory itself.
    remove_dir_contents(&cache_root)?;

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

/// Verify content-addressed local cache objects and evict corrupt entries.
pub async fn run_cache_verify(mode: OutputMode) -> Result<CacheVerifySummary> {
    let cache = crate::cache::LocalCache::new(crate::cache::default_cache_root());
    let report = cache.verify().await?;
    let summary = CacheVerifySummary {
        objects_checked: report.total,
        objects_valid: report.valid,
        objects_corrupt: report.corrupt,
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
fn walk_dir_size(dir: &Path) -> Result<(u64, u64)> {
    let mut files = 0u64;
    let mut bytes = 0u64;
    walk_dir_size_inner(dir, &mut files, &mut bytes)?;
    Ok((files, bytes))
}

fn walk_dir_size_inner(dir: &Path, files: &mut u64, bytes: &mut u64) -> Result<()> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        crate::core::error::CrabError::Internal(format!(
            "failed to read directory {}: {e}",
            dir.display()
        ))
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| {
            crate::core::error::CrabError::Internal(format!("dir entry error: {e}"))
        })?;
        let path = entry.path();
        if path.is_dir() {
            walk_dir_size_inner(&path, files, bytes)?;
        } else if path.is_file() {
            *files += 1;
            *bytes += std::fs::metadata(&path).map_or(0, |m| m.len());
        }
    }

    Ok(())
}

/// Remove all files and subdirectories inside `dir`, but keep `dir` itself.
fn remove_dir_contents(dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path)?;
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
    fn format_bytes_units() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1_048_576), "1.0 MB");
    }
}
