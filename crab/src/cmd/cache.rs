//! Local cache inspection, cleanup, and verification command policy.
//!
//! Only recognized private payloads are removed. Unknown state, databases,
//! retained profiles, and live workspace trees retain their owner's lifecycle.

use std::path::{Path, PathBuf};

use crate::core::config::Config;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::{OutputMode, emit_json};
use crab_cache::health::{CacheCatalogHealth, CacheHealthReport, inspect_cache};
use tokio_util::sync::CancellationToken;

/// Summary of a cache verify operation.
#[derive(Debug, Default, serde::Serialize)]
pub struct CacheVerifySummary {
    pub objects_checked: u64,
    pub objects_valid: u64,
    pub objects_corrupt: u64,
}

/// Print a non-mutating cache report, returning whether all inspections were available.
pub async fn run_cache_stats(mode: OutputMode, cancel: &CancellationToken) -> Result<bool> {
    check_cancelled(cancel)?;
    let config = Config::resolve_local()?;
    let root = crate::cache::default_cache_root();
    let report = inspect_cache(&root, config.cache.max_bytes, cancel).await?;
    check_cancelled(cancel)?;
    if mode.is_machine() {
        emit_json("cache.stats", "1.0", &report);
    } else {
        print_cache_stats(&report);
    }
    Ok(report.is_available())
}

fn print_cache_stats(report: &CacheHealthReport) {
    println!(
        "Local cache: {} ({:?})",
        report.root.display(),
        report.root_state
    );
    println!("  budget: {} bytes", report.budget_bytes);
    println!(
        "  observed: {} logical bytes, {} allocated bytes ({})",
        report.observed.logical_bytes,
        report.observed.allocated_bytes,
        if report.scan_complete {
            "complete scan"
        } else {
            "partial scan; lower bounds"
        }
    );
    println!(
        "  over budget: {}",
        match report.over_budget {
            Some(true) => "yes",
            Some(false) => "no",
            None => "unknown",
        }
    );
    println!(
        "\n  {:<14} {:>8} {:>8} {:>14} {:>14}  status",
        "family", "files", "dirs", "logical bytes", "allocated bytes"
    );
    for (family, health) in &report.families {
        let state = if !health.complete {
            "partial"
        } else if health.issues > 0 {
            "unavailable"
        } else {
            "inspected"
        };
        println!(
            "  {family:<14} {:>8} {:>8} {:>14} {:>14}  {state}",
            health.usage.files,
            health.usage.directories,
            health.usage.logical_bytes,
            health.usage.allocated_bytes
        );
    }
    match &report.catalog {
        CacheCatalogHealth::Readable { stats } => {
            println!(
                "\n  catalog: {} entries, {} recorded bytes, {} temporary bytes, {} reserved bytes",
                stats.entries, stats.total_bytes, stats.temporary_bytes, stats.reservations_bytes
            );
            println!(
                "  last maintenance (Unix ms): {}",
                stats
                    .last_maintenance_unix_ms
                    .map_or_else(|| "unknown".into(), |value| value.to_string())
            );
        }
        CacheCatalogHealth::Missing => println!("\n  catalog: missing (not initialized)"),
        CacheCatalogHealth::Unavailable => println!("\n  catalog: unavailable"),
    }
    for issue in &report.issues {
        println!(
            "  unavailable: {} [{}]: {}",
            issue.path,
            issue.family.unwrap_or("root"),
            issue.error
        );
    }
    if report.omitted_issues > 0 {
        println!("  {} additional issues omitted", report.omitted_issues);
    }
    println!(
        "\nAllocation includes linked files and directories, not unlinked open files; reservations are separate."
    );
    println!(
        "Live scan, not an atomic snapshot or payload/index integrity verification. No persistent hit rate is recorded."
    );
}

/// Remove eligible payloads through the shared ownership-aware cleanup boundary.
pub async fn run_cache_clean(
    dry_run: bool,
    mode: OutputMode,
    cancel: &CancellationToken,
) -> Result<crab_cache::CacheCleanReport> {
    let root = crate::cache::default_cache_root();
    let report = clean_root(&root, dry_run, cancel).await?;
    if mode.is_machine() {
        println!(
            "{}",
            serde_json::to_string(&report).map_err(std::io::Error::other)?
        );
    } else {
        println!(
            "{} {} cache payload(s), {}; retained {} entries/subtrees, {} busy, {} unsafe",
            if dry_run { "Would remove" } else { "Removed" },
            report.files_removed,
            format_bytes(report.bytes_reclaimed),
            report.retained_entries,
            report.busy_entries,
            report.unsafe_entries,
        );
    }
    Ok(report)
}

async fn clean_root(
    root: &Path,
    dry_run: bool,
    cancel: &CancellationToken,
) -> Result<crab_cache::CacheCleanReport> {
    check_cancelled(cancel)?;
    validate_destructive_cache_root(root)?;
    // The canonical path is a destructive-root safeguard, not I/O authority.
    // Pass the original root so the private boundary can reject root symlinks.
    Ok(crab_cache::clean_cache(root, dry_run, cancel).await?)
}

// Even canonical payload names do not authorize cleanup of a live user root.
// All destructive cache commands share this policy; private I/O still receives
// the original path and independently validates/pins the actual directories.
pub(super) fn validate_destructive_cache_root(root: &Path) -> Result<()> {
    let path = match std::fs::canonicalize(root) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let cwd = std::fs::canonicalize(std::env::current_dir()?)?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.exists())
        .map(std::fs::canonicalize)
        .transpose()?;
    let unsafe_path = path.parent().is_none()
        || home.as_ref().is_some_and(|home| &path == home)
        || cwd.starts_with(&path);
    if unsafe_path {
        return Err(CrabError::Configuration {
            key: "cache directory is unsafe for cleanup".to_owned(),
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
    let root = crate::cache::default_cache_root();
    validate_destructive_cache_root(&root)?;
    let config = Config::resolve_local()?;
    let cache = crate::cache::LocalCache::new(root);
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
    fn format_bytes_units() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1_048_576), "1.0 MB");
    }

    #[test]
    fn clean_rejects_current_working_directory() {
        let cwd = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();

        let error = validate_destructive_cache_root(&cwd).unwrap_err();

        assert!(matches!(error, CrabError::Configuration { .. }));
    }

    #[tokio::test]
    async fn stats_honors_cancellation_before_inspection() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            run_cache_stats(OutputMode::Text, &cancel).await,
            Err(CrabError::Cancelled)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn clean_command_uses_payload_ownership_and_private_root_checks() {
        use crate::cache::{CacheKey, LocalCache};
        use crab_xet::hash::compute_data_hash;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let cache = LocalCache::new(root.clone());
        cache
            .put(&CacheKey::Chunk(compute_data_hash(b"data")), b"data")
            .await
            .unwrap();
        let retained = root.join("user-notes");
        std::fs::write(&retained, b"keep").unwrap();
        let cancel = CancellationToken::new();
        let preview = clean_root(&root, true, &cancel).await.unwrap();
        assert_eq!((preview.files_removed, preview.bytes_reclaimed), (1, 4));
        let report = clean_root(&root, false, &cancel).await.unwrap();
        assert_eq!((report.files_removed, report.bytes_reclaimed), (1, 4));
        assert_eq!(std::fs::read(retained).unwrap(), b"keep");
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        assert!(clean_root(&alias, false, &cancel).await.is_err());
        cancel.cancel();
        assert!(matches!(
            clean_root(&root, false, &cancel).await,
            Err(CrabError::Cancelled)
        ));
    }
}
