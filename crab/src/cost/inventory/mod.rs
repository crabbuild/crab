//! Inventory subsystem — collects object metadata from cloud storage.
//!
//! Two collection strategies:
//!
//! - **Live** — walks the bucket via `object_store::list` streams with
//!   per-prefix sub-walkers and `Semaphore`-gated concurrency.
//! - **Report** — consumes provider-side inventory reports (S3 Inventory
//!   Parquet/ORC/CSV, GCS Storage Insights, Azure Blob Inventory).
//!
//! The `InventorySource::Auto` selector prefers a fresh report when
//! available, falling back to a live walk.
//!
//! # Submodules
//!
//! - `live` — live walker using `object_store::list`.
//! - `sample` — deterministic prefix sampling via `blake3`.
//! - `report/` — provider-side report parsers.

pub mod live;
pub mod report;
pub mod sample;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::tier::classes::StorageClass;

/// A single object in the inventory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventoryItem {
    /// Full object key (e.g. `.crab/xorbs/ab/abcd1234...`).
    pub key: String,
    /// Object size in bytes.
    pub size: u64,
    /// Provider-native storage class.
    pub storage_class: StorageClass,
    /// Last-modified timestamp as RFC 3339 string.
    pub last_modified: String,
    /// Object ETag (provider-specific).
    pub etag: Option<String>,
    /// Restore status for archive-class objects.
    pub restore_status: Option<String>,
}

/// Per-class aggregate statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClassStats {
    /// Number of objects in this class.
    pub objects: u64,
    /// Total bytes in this class.
    pub bytes: u64,
}

/// Per-prefix aggregate statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PrefixStats {
    /// Number of objects under this prefix.
    pub objects: u64,
    /// Total bytes under this prefix.
    pub bytes: u64,
}

/// Collected inventory of a bucket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Inventory {
    /// How the inventory was collected.
    pub source: InventorySourceInfo,
    /// When the scan completed (RFC 3339).
    pub scanned_at: String,
    /// Total number of objects scanned.
    pub total_objects: u64,
    /// Total bytes across all objects.
    pub total_bytes: u64,
    /// Breakdown by storage class.
    pub per_class: BTreeMap<String, ClassStats>,
    /// Breakdown by top-level prefix.
    pub per_prefix: BTreeMap<String, PrefixStats>,
    /// Top-K heaviest cold objects (archive classes).
    pub heaviest_cold: Vec<InventoryItem>,
}

/// Describes how the inventory was collected.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum InventorySourceInfo {
    /// Collected via live `object_store::list` walk.
    Live {
        list_concurrency: u32,
        sample_ratio: Option<f64>,
    },
    /// Consumed from a provider-side inventory report.
    Report {
        provider: String,
        report_at: String,
        schema: String,
    },
}

/// Inventory source selection strategy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InventorySource {
    /// Prefer report if fresh, else live.
    Auto,
    /// Always use live walk.
    Live,
    /// Always use provider-side report.
    Report,
}

impl std::str::FromStr for InventorySource {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "live" => Self::Live,
            "report" => Self::Report,
            _ => Self::Auto,
        })
    }
}

/// All Crab prefixes that the inventory walker must cover.
///
/// Inventory covers **all** Crab prefixes, not only tier-eligible ones.
pub const ALL_CRAB_PREFIXES: &[&str] = &[
    ".crab/xorbs/",
    ".crab/shards/",
    ".crab/file-index/",
    ".crab/tier/",
    ".crab/audit/",
    ".crab/tombstones/",
    ".crab/optimize/xorbs/",
    ".crab/ref-registry",
];

/// Prefixes that are per-repo (mutable state).
pub const REPO_PREFIXES: &[&str] = &["refs/", "manifests/", "packs/", "locks/"];

/// Choose the inventory source based on config and report freshness.
///
/// When `source` is `Auto`, checks whether a report exists and is
/// within `max_staleness_hours`. If so, returns `Report`; otherwise
/// falls back to `Live`.
pub fn resolve_source(
    source: &InventorySource,
    report_available: bool,
    report_age_hours: Option<u64>,
    max_staleness_hours: u32,
) -> InventorySource {
    match source {
        InventorySource::Live => InventorySource::Live,
        InventorySource::Report => InventorySource::Report,
        InventorySource::Auto => {
            if report_available
                && let Some(age) = report_age_hours
                && age <= u64::from(max_staleness_hours)
            {
                return InventorySource::Report;
            }
            InventorySource::Live
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_source_from_str_auto() {
        assert_eq!(
            "auto".parse::<InventorySource>().unwrap(),
            InventorySource::Auto
        );
        assert_eq!(
            "AUTO".parse::<InventorySource>().unwrap(),
            InventorySource::Auto
        );
        assert_eq!(
            "".parse::<InventorySource>().unwrap(),
            InventorySource::Auto
        );
        assert_eq!(
            "unknown".parse::<InventorySource>().unwrap(),
            InventorySource::Auto
        );
    }

    #[test]
    fn inventory_source_from_str_live() {
        assert_eq!(
            "live".parse::<InventorySource>().unwrap(),
            InventorySource::Live
        );
        assert_eq!(
            "LIVE".parse::<InventorySource>().unwrap(),
            InventorySource::Live
        );
    }

    #[test]
    fn inventory_source_from_str_report() {
        assert_eq!(
            "report".parse::<InventorySource>().unwrap(),
            InventorySource::Report
        );
        assert_eq!(
            "REPORT".parse::<InventorySource>().unwrap(),
            InventorySource::Report
        );
    }

    #[test]
    fn resolve_source_auto_prefers_fresh_report() {
        let result = resolve_source(&InventorySource::Auto, true, Some(24), 48);
        assert_eq!(result, InventorySource::Report);
    }

    #[test]
    fn resolve_source_auto_falls_back_to_live_when_stale() {
        let result = resolve_source(&InventorySource::Auto, true, Some(72), 48);
        assert_eq!(result, InventorySource::Live);
    }

    #[test]
    fn resolve_source_auto_falls_back_to_live_when_no_report() {
        let result = resolve_source(&InventorySource::Auto, false, None, 48);
        assert_eq!(result, InventorySource::Live);
    }

    #[test]
    fn resolve_source_explicit_live_ignores_report() {
        let result = resolve_source(&InventorySource::Live, true, Some(1), 48);
        assert_eq!(result, InventorySource::Live);
    }

    #[test]
    fn resolve_source_explicit_report() {
        let result = resolve_source(&InventorySource::Report, false, None, 48);
        assert_eq!(result, InventorySource::Report);
    }

    #[test]
    fn all_crab_prefixes_covers_required_paths() {
        // C1.6: all Crab prefixes, not only tier-eligible.
        assert!(ALL_CRAB_PREFIXES.contains(&".crab/xorbs/"));
        assert!(ALL_CRAB_PREFIXES.contains(&".crab/shards/"));
        assert!(ALL_CRAB_PREFIXES.contains(&".crab/file-index/"));
    }

    #[test]
    fn resolve_source_auto_at_boundary() {
        // Exactly at max staleness should still be fresh.
        let result = resolve_source(&InventorySource::Auto, true, Some(48), 48);
        assert_eq!(result, InventorySource::Report);
    }
}
