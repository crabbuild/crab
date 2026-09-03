//! `crab cache clean` — reclaim disk space used by the local cache.
//!
//! The local cache lives at `~/.cache/crab/` (or `$XDG_CACHE_HOME/crab/`)
//! and stores downloaded shards, xorbs, persistent chunk indices, workflow
//! stage entries, and bloom filters.
//! Cleanup retains coordination markers and refuses active mirror owners.

use std::path::{Path, PathBuf};

use crate::core::config::Config;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::OutputMode;
use crab_cache::lifecycle::{CacheCleanGuard, cleanup_preview};
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

/// Remove cache data while retaining coordination markers.
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

    if dry_run {
        let mut files = 0u64;
        let mut bytes = 0u64;
        for target in &targets {
            let stats = cleanup_preview(target, cancel)?;
            files = files.saturating_add(stats.files_removed);
            bytes = bytes.saturating_add(stats.bytes_reclaimed);
        }
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

    let summary = clean_admitted_targets(&targets, cancel)?;

    if !mode.is_machine() {
        println!(
            "Removed {} file(s), reclaimed {}",
            summary.files_removed,
            format_bytes(summary.bytes_reclaimed),
        );
    }

    Ok(summary)
}

fn clean_admitted_targets(
    targets: &[PathBuf],
    cancel: &CancellationToken,
) -> Result<CacheCleanSummary> {
    // Admit every configured root before deleting from any of them. A busy
    // mirror in the second root must not cause partial cleanup of the first.
    let guards = targets
        .iter()
        .map(|target| CacheCleanGuard::acquire(target, cancel))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut summary = CacheCleanSummary::default();
    for guard in guards {
        check_cancelled(cancel)?;
        let stats = guard.clean(cancel)?;
        summary.files_removed = summary.files_removed.saturating_add(stats.files_removed);
        summary.bytes_reclaimed = summary
            .bytes_reclaimed
            .saturating_add(stats.bytes_reclaimed);
    }
    Ok(summary)
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
    fn busy_second_root_prevents_deletion_from_first_root() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::write(first.join("keep"), b"data").unwrap();
        let owner = crab_cache::lifecycle::CacheUseGuard::acquire(
            &second.join("mirror.git"),
            &CancellationToken::new(),
        )
        .unwrap();
        let roots = vec![first.clone(), second];
        let error = clean_admitted_targets(&roots, &CancellationToken::new()).unwrap_err();
        assert!(
            matches!(error, CrabError::Io(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
        assert!(first.join("keep").exists());
        drop(owner);
        let summary = clean_admitted_targets(&roots, &CancellationToken::new()).unwrap();
        assert_eq!(summary.files_removed, 1);
        assert_eq!(summary.bytes_reclaimed, 4);
    }

    #[test]
    fn cache_clean_admission_honors_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let error = clean_admitted_targets(&[dir.path().to_owned()], &cancel).unwrap_err();
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
}
