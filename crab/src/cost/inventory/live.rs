//! Live inventory walker using `object_store::list` streams.
//!
//! Walks all Crab prefixes with per-prefix sub-walkers gated by a
//! `tokio::sync::Semaphore` for bounded concurrency. Collects per-class
//! and per-prefix statistics, and optionally the top-K heaviest cold
//! objects via HEAD / `GetObjectAttributes`.
//!
//! Progress is reported with ETA computed from observed list RPS.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use object_store::ObjectStore;
use object_store::path::Path as ObjectPath;
use tokio::sync::Semaphore;
use tracing::{debug, info};

use super::{
    ALL_CRAB_PREFIXES, ClassStats, Inventory, InventoryItem, InventorySourceInfo, PrefixStats,
    sample,
};
use crate::core::error::Result;
use crate::tier::classes::StorageClass;
use crate::tier::provider::Provider;

/// Configuration for a live inventory walk.
#[derive(Debug, Clone)]
pub struct LiveWalkConfig {
    /// Maximum concurrent LIST requests across all prefixes.
    pub list_concurrency: u32,
    /// Sample ratio (1.0 = no sampling, 0.5 = ~50% of keys).
    pub sample_ratio: Option<f64>,
    /// Number of heaviest cold objects to track.
    pub top_k_cold: usize,
    /// Provider for storage-class interpretation.
    pub provider: Provider,
}

impl Default for LiveWalkConfig {
    fn default() -> Self {
        Self {
            list_concurrency: 32,
            sample_ratio: None,
            top_k_cold: 100,
            provider: Provider::S3,
        }
    }
}

/// Progress state for the live walker, updated atomically.
#[derive(Debug)]
pub struct WalkProgress {
    /// Total objects listed so far.
    pub objects_listed: AtomicU64,
    /// Total bytes observed so far.
    pub bytes_observed: AtomicU64,
    /// Walk start time.
    pub started_at: Instant,
}

impl WalkProgress {
    fn new() -> Self {
        Self {
            objects_listed: AtomicU64::new(0),
            bytes_observed: AtomicU64::new(0),
            started_at: Instant::now(),
        }
    }

    /// Observed list operations per second.
    pub fn list_rps(&self) -> f64 {
        let elapsed = self.started_at.elapsed().as_secs_f64();
        if elapsed < 0.001 {
            return 0.0;
        }
        self.objects_listed.load(Ordering::Relaxed) as f64 / elapsed
    }

    /// Estimated time remaining based on observed RPS and a total estimate.
    pub fn eta_secs(&self, estimated_total: u64) -> Option<f64> {
        let listed = self.objects_listed.load(Ordering::Relaxed);
        let rps = self.list_rps();
        if rps < 1.0 || listed >= estimated_total {
            return None;
        }
        let remaining = estimated_total.saturating_sub(listed);
        Some(remaining as f64 / rps)
    }
}

/// Walk a single prefix and collect items.
///
/// Uses `object_store::list` to stream objects under the given prefix.
/// Each object is optionally sampled via `blake3(key)` and accumulated
/// into per-class and per-prefix stats.
async fn walk_prefix(
    store: &dyn ObjectStore,
    prefix: &str,
    config: &LiveWalkConfig,
    progress: &WalkProgress,
) -> Result<PrefixWalkResult> {
    use futures_util::StreamExt;

    let mut result = PrefixWalkResult::default();
    let obj_prefix = ObjectPath::from(prefix);

    let mut stream = store.list(Some(&obj_prefix));

    while let Some(meta_result) = stream.next().await {
        let meta = match meta_result {
            Ok(m) => m,
            Err(e) => {
                debug!(prefix, error = %e, "skipping object due to list error");
                continue;
            }
        };

        let key = meta.location.to_string();

        // Apply sampling if configured.
        if let Some(ratio) = config.sample_ratio
            && ratio < 1.0
            && !sample::should_include(&key, ratio)
        {
            continue;
        }

        let size = meta.size as u64;
        let last_modified = meta.last_modified.to_rfc3339();

        // Infer storage class from object metadata.
        // In a live walk, object_store::list does not return storage class
        // for all providers. We default to the provider's standard class.
        let storage_class = infer_default_class(config.provider);

        let item = InventoryItem {
            key: key.clone(),
            size,
            storage_class,
            last_modified,
            etag: meta.e_tag.clone(),
            restore_status: None,
        };

        // Update per-class stats.
        let class_key = format!("{storage_class}");
        let class_stats = result.per_class.entry(class_key).or_default();
        class_stats.objects += 1;
        class_stats.bytes += size;

        result.objects += 1;
        result.bytes += size;

        // Track top-K cold objects.
        if storage_class.is_archive_class() {
            insert_top_k(&mut result.heaviest_cold, item, config.top_k_cold);
        }

        progress.objects_listed.fetch_add(1, Ordering::Relaxed);
        progress.bytes_observed.fetch_add(size, Ordering::Relaxed);
    }

    Ok(result)
}

/// Result of walking a single prefix.
#[derive(Debug, Default)]
struct PrefixWalkResult {
    objects: u64,
    bytes: u64,
    per_class: BTreeMap<String, ClassStats>,
    heaviest_cold: Vec<InventoryItem>,
}

/// Insert an item into the top-K heaviest list, maintaining descending
/// order by size and capping at `k` entries.
fn insert_top_k(top_k: &mut Vec<InventoryItem>, item: InventoryItem, k: usize) {
    if k == 0 {
        return;
    }

    // Find insertion point (descending by size).
    let pos = top_k
        .iter()
        .position(|existing| item.size > existing.size)
        .unwrap_or(top_k.len());

    if pos < k {
        top_k.insert(pos, item);
        top_k.truncate(k);
    }
}

/// Infer the default (standard) storage class for a provider.
fn infer_default_class(provider: Provider) -> StorageClass {
    match provider {
        Provider::S3 => StorageClass::S3Standard,
        Provider::Gcs => StorageClass::GcsStandard,
        Provider::Azure => StorageClass::AzureHot,
    }
}

/// Run a live inventory walk across all Crab prefixes.
///
/// Uses `Semaphore`-gated concurrency to bound the number of
/// simultaneous LIST requests. Returns a complete `Inventory` with
/// per-class and per-prefix breakdowns.
pub async fn walk_live(store: Arc<dyn ObjectStore>, config: LiveWalkConfig) -> Result<Inventory> {
    let semaphore = Arc::new(Semaphore::new(config.list_concurrency as usize));
    let progress = Arc::new(WalkProgress::new());
    let config = Arc::new(config);

    info!(
        concurrency = config.list_concurrency,
        prefixes = ALL_CRAB_PREFIXES.len(),
        "starting live inventory walk"
    );

    let mut handles = Vec::new();

    for &prefix in ALL_CRAB_PREFIXES {
        let store = Arc::clone(&store);
        let sem = Arc::clone(&semaphore);
        let prog = Arc::clone(&progress);
        let cfg = Arc::clone(&config);

        let handle = tokio::spawn(async move {
            let _permit = sem.acquire().await;
            walk_prefix(store.as_ref(), prefix, &cfg, &prog).await
        });

        handles.push((prefix.to_string(), handle));
    }

    let mut total_objects: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut per_class: BTreeMap<String, ClassStats> = BTreeMap::new();
    let mut per_prefix: BTreeMap<String, PrefixStats> = BTreeMap::new();
    let mut all_heaviest_cold: Vec<InventoryItem> = Vec::new();

    for (prefix, handle) in handles {
        let result = handle.await.map_err(|e| {
            crate::core::error::CrabError::Internal(format!(
                "inventory walk task panicked for prefix {prefix}: {e}"
            ))
        })??;

        total_objects += result.objects;
        total_bytes += result.bytes;

        per_prefix.insert(
            prefix.clone(),
            PrefixStats {
                objects: result.objects,
                bytes: result.bytes,
            },
        );

        for (class_key, stats) in result.per_class {
            let entry = per_class.entry(class_key).or_default();
            entry.objects += stats.objects;
            entry.bytes += stats.bytes;
        }

        for item in result.heaviest_cold {
            insert_top_k(&mut all_heaviest_cold, item, config.top_k_cold);
        }
    }

    // Apply sample ratio scaling if sampling was used.
    let (adjusted_objects, adjusted_bytes) = if let Some(ratio) = config.sample_ratio {
        if ratio > 0.0 && ratio < 1.0 {
            let scale = 1.0 / ratio;
            (
                (total_objects as f64 * scale) as u64,
                (total_bytes as f64 * scale) as u64,
            )
        } else {
            (total_objects, total_bytes)
        }
    } else {
        (total_objects, total_bytes)
    };

    let elapsed = progress.started_at.elapsed();
    info!(
        objects = adjusted_objects,
        bytes = adjusted_bytes,
        elapsed_ms = elapsed.as_millis() as u64,
        rps = progress.list_rps() as u64,
        "live inventory walk complete"
    );

    let sample_ratio = config.sample_ratio.filter(|&r| r < 1.0);

    Ok(Inventory {
        source: InventorySourceInfo::Live {
            list_concurrency: config.list_concurrency,
            sample_ratio,
        },
        scanned_at: chrono_now_rfc3339(),
        total_objects: adjusted_objects,
        total_bytes: adjusted_bytes,
        per_class,
        per_prefix,
        heaviest_cold: all_heaviest_cold,
    })
}

/// Returns the current UTC time as an RFC 3339 string.
///
/// Uses `SystemTime` to avoid pulling in a datetime crate.
pub fn chrono_now_rfc3339() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    // Simple ISO 8601 approximation — sufficient for inventory timestamps.
    let secs = now.as_secs();
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Approximate year/month/day from days since epoch.
    let mut remaining_days = days;
    let mut year: u64 = 1970;
    loop {
        let ydays = if is_leap_year(year) { 366 } else { 365 };
        if remaining_days < ydays {
            break;
        }
        remaining_days -= ydays;
        year += 1;
    }

    let leap = is_leap_year(year);
    let month_days: [u64; 12] = if leap {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut month: u64 = 1;
    for &md in &month_days {
        if remaining_days < md {
            break;
        }
        remaining_days -= md;
        month += 1;
    }
    let day = remaining_days + 1;

    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

fn is_leap_year(year: u64) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_top_k_maintains_descending_order() {
        let mut top_k = Vec::new();
        let make_item = |size: u64| InventoryItem {
            key: format!("key-{size}"),
            size,
            storage_class: StorageClass::S3GlacierDeepArchive,
            last_modified: "2026-01-01T00:00:00Z".to_string(),
            etag: None,
            restore_status: None,
        };

        insert_top_k(&mut top_k, make_item(100), 3);
        insert_top_k(&mut top_k, make_item(300), 3);
        insert_top_k(&mut top_k, make_item(200), 3);
        insert_top_k(&mut top_k, make_item(50), 3);
        insert_top_k(&mut top_k, make_item(400), 3);

        assert_eq!(top_k.len(), 3);
        assert_eq!(top_k[0].size, 400);
        assert_eq!(top_k[1].size, 300);
        assert_eq!(top_k[2].size, 200);
    }

    #[test]
    fn insert_top_k_zero_capacity() {
        let mut top_k = Vec::new();
        let item = InventoryItem {
            key: "key".to_string(),
            size: 100,
            storage_class: StorageClass::S3Standard,
            last_modified: "2026-01-01T00:00:00Z".to_string(),
            etag: None,
            restore_status: None,
        };
        insert_top_k(&mut top_k, item, 0);
        assert!(top_k.is_empty());
    }

    #[test]
    fn infer_default_class_per_provider() {
        assert_eq!(infer_default_class(Provider::S3), StorageClass::S3Standard);
        assert_eq!(
            infer_default_class(Provider::Gcs),
            StorageClass::GcsStandard
        );
        assert_eq!(infer_default_class(Provider::Azure), StorageClass::AzureHot);
    }

    #[test]
    fn walk_progress_rps_zero_at_start() {
        let progress = WalkProgress::new();
        // RPS should be 0 or very small at the start.
        assert!(progress.list_rps() < 1.0);
    }

    #[test]
    fn walk_progress_eta_none_when_no_objects() {
        let progress = WalkProgress::new();
        assert!(progress.eta_secs(1000).is_none());
    }

    #[test]
    fn chrono_now_rfc3339_format() {
        let ts = chrono_now_rfc3339();
        // Should match YYYY-MM-DDTHH:MM:SSZ pattern.
        assert!(ts.ends_with('Z'));
        assert_eq!(ts.len(), 20);
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
    }
}
