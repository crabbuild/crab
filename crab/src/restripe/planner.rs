//! Dry-run estimator for restripe operations.
//!
//! [`estimate`] computes the expected outcome of a restripe without
//! downloading any xorb bodies. It uses HEAD / `GetObjectAttributes`
//! to determine source xorb sizes and storage classes, then projects
//! destination xorb counts, bytes to rewrite, wall-clock time, and
//! provider API costs.

use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use serde::Serialize;

use crate::restripe::profile::Profile;

// ---------------------------------------------------------------------------
// Calibration constants
// ---------------------------------------------------------------------------

/// Default compression ratio lookup table indexed by zstd level.
/// These are conservative estimates from benchmarked throughput on
/// mixed binary data. Configurable via `CalibrationConfig`.
const DEFAULT_COMPRESSION_RATIOS: &[(i32, f64)] = &[
    (1, 0.85),
    (3, 0.75),
    (5, 0.65),
    (7, 0.55),
    (9, 0.50),
    (19, 0.40),
];

/// Default throughput in bytes/sec for estimation. Based on benchmarked
/// single-host restripe throughput (download + decompress + recompress +
/// upload). Configurable via `CalibrationConfig`.
const DEFAULT_THROUGHPUT_BYTES_PER_SEC: u64 = 100 * 1024 * 1024; // 100 MiB/s

/// Default PUT cost per 1000 operations (S3 Standard).
const DEFAULT_PUT_PER_K_OPS_USD: &str = "0.005";

/// Default storage cost per GB-month (S3 Standard).
const DEFAULT_GB_MONTH_USD: &str = "0.023";

// ---------------------------------------------------------------------------
// Calibration config
// ---------------------------------------------------------------------------

/// Configurable calibration constants for the planner.
///
/// Defaults come from the benchmarked throughput table. Users can
/// override these in config to match their specific hardware and
/// network characteristics.
#[derive(Debug, Clone)]
pub struct CalibrationConfig {
    /// Throughput in bytes/sec for wall-clock estimation.
    pub throughput_bytes_per_sec: u64,
    /// PUT cost per 1000 operations in USD.
    pub put_per_k_ops_usd: Decimal,
    /// Storage cost per GB-month in USD.
    pub gb_month_usd: Decimal,
}

impl Default for CalibrationConfig {
    fn default() -> Self {
        Self {
            throughput_bytes_per_sec: DEFAULT_THROUGHPUT_BYTES_PER_SEC,
            put_per_k_ops_usd: DEFAULT_PUT_PER_K_OPS_USD
                .parse()
                .unwrap_or_else(|_| Decimal::ZERO),
            gb_month_usd: DEFAULT_GB_MONTH_USD
                .parse()
                .unwrap_or_else(|_| Decimal::ZERO),
        }
    }
}

// ---------------------------------------------------------------------------
// Source xorb metadata
// ---------------------------------------------------------------------------

/// Metadata for a single source xorb, obtained via HEAD.
#[derive(Debug, Clone)]
pub struct SourceXorbMeta {
    /// Xorb hash identifier.
    pub hash: String,
    /// Size in bytes.
    pub size_bytes: u64,
    /// Storage class (for tier-aware planning).
    pub storage_class: String,
    /// Whether this xorb is in an archive class.
    pub is_archive: bool,
}

// ---------------------------------------------------------------------------
// Estimate result
// ---------------------------------------------------------------------------

/// Result of a dry-run estimation.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RestripeEstimate {
    /// Profile name used for the estimate.
    pub profile: String,
    /// Number of source xorbs that would be processed.
    pub source_count: u64,
    /// Total bytes across all source xorbs.
    pub source_bytes: u64,
    /// Estimated number of destination xorbs.
    pub estimated_dest_count: u64,
    /// Estimated bytes after recompression.
    pub estimated_dest_bytes: u64,
    /// Estimated wall-clock duration in seconds.
    pub estimated_wall_secs: u64,
    /// Estimated provider API cost in USD (6 decimal places).
    pub estimated_cost_usd: String,
    /// Number of source xorbs in archive classes (skipped if
    /// `--include-cold=false`).
    pub archive_source_count: u64,
    /// Bytes in archive-class source xorbs.
    pub archive_source_bytes: u64,
}

/// Compute a dry-run estimate for a restripe operation.
///
/// No writes are performed. Only HEAD and list operations are used
/// to gather source xorb metadata.
pub fn estimate(
    profile_name: &str,
    profile: &Profile,
    sources: &[SourceXorbMeta],
    calibration: &CalibrationConfig,
    include_cold: bool,
) -> RestripeEstimate {
    let compression_ratio = lookup_compression_ratio(profile);

    let mut source_count: u64 = 0;
    let mut source_bytes: u64 = 0;
    let mut archive_count: u64 = 0;
    let mut archive_bytes: u64 = 0;
    let mut dest_bytes_total: u64 = 0;
    let mut dest_count_total: u64 = 0;

    for src in sources {
        if src.is_archive {
            archive_count += 1;
            archive_bytes += src.size_bytes;
            if !include_cold {
                continue;
            }
        }

        source_count += 1;
        source_bytes += src.size_bytes;

        // Estimate bytes after recompression.
        let recompressed = (src.size_bytes as f64 * compression_ratio).max(1.0) as u64;
        dest_bytes_total += recompressed;

        // Estimate destination xorb count.
        let dest_xorbs = if profile.target_xorb_bytes > 0 {
            (recompressed + profile.target_xorb_bytes - 1) / profile.target_xorb_bytes
        } else {
            1
        };
        dest_count_total += dest_xorbs.max(1);
    }

    // Wall-clock estimate.
    let wall_secs = if calibration.throughput_bytes_per_sec > 0 {
        source_bytes / calibration.throughput_bytes_per_sec
    } else {
        0
    };

    // Cost estimate: PUT ops + prorated storage for the new xorbs.
    let put_ops = Decimal::from_u64(dest_count_total).unwrap_or(Decimal::ZERO);
    let put_cost = (put_ops / Decimal::from(1000)) * calibration.put_per_k_ops_usd;

    // Prorated storage cost: assume 1 month of overlap before GC reclaims
    // old xorbs. Cost = dest_bytes in GB × gb_month_usd.
    let dest_gb = Decimal::from_u64(dest_bytes_total).unwrap_or(Decimal::ZERO)
        / Decimal::from(1_073_741_824u64);
    let storage_cost = dest_gb * calibration.gb_month_usd;

    let total_cost = put_cost + storage_cost;

    RestripeEstimate {
        profile: profile_name.to_string(),
        source_count,
        source_bytes,
        estimated_dest_count: dest_count_total,
        estimated_dest_bytes: dest_bytes_total,
        estimated_wall_secs: wall_secs,
        estimated_cost_usd: format!("{:.6}", total_cost),
        archive_source_count: archive_count,
        archive_source_bytes: archive_bytes,
    }
}

/// Look up the compression ratio for a profile's compression config.
fn lookup_compression_ratio(profile: &Profile) -> f64 {
    use crate::core::config::CompressionConfig;

    match profile.compression {
        CompressionConfig::None => 1.0,
        CompressionConfig::Lz4 => 0.80,
        CompressionConfig::Zstd { level } => {
            // Find the closest level in the lookup table.
            let mut best_ratio = 0.75; // default for unknown levels
            let mut best_dist = i32::MAX;
            for &(l, r) in DEFAULT_COMPRESSION_RATIOS {
                let dist = (l - level).abs();
                if dist < best_dist {
                    best_dist = dist;
                    best_ratio = r;
                }
            }
            best_ratio
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::restripe::profile::Profile;

    fn make_sources(count: usize, size: u64, archive: bool) -> Vec<SourceXorbMeta> {
        (0..count)
            .map(|i| SourceXorbMeta {
                hash: format!("xorb-{i:04}"),
                size_bytes: size,
                storage_class: if archive {
                    "GLACIER".to_string()
                } else {
                    "STANDARD".to_string()
                },
                is_archive: archive,
            })
            .collect()
    }

    #[test]
    fn estimate_basic_ml_profile() {
        let profile = Profile::ml();
        let sources = make_sources(10, 256 * 1024 * 1024, false);
        let cal = CalibrationConfig::default();

        let est = estimate("ml", &profile, &sources, &cal, true);

        assert_eq!(est.source_count, 10);
        assert_eq!(est.source_bytes, 10 * 256 * 1024 * 1024);
        assert!(est.estimated_dest_count > 0);
        assert!(est.estimated_dest_bytes > 0);
        assert!(est.estimated_wall_secs > 0);
        assert!(est.estimated_cost_usd.parse::<f64>().unwrap() > 0.0);
        assert_eq!(est.archive_source_count, 0);
    }

    #[test]
    fn estimate_skips_archive_when_include_cold_false() {
        let profile = Profile::ml();
        let mut sources = make_sources(5, 100 * 1024 * 1024, false);
        sources.extend(make_sources(3, 100 * 1024 * 1024, true));
        let cal = CalibrationConfig::default();

        let est = estimate("ml", &profile, &sources, &cal, false);

        // Only warm sources counted.
        assert_eq!(est.source_count, 5);
        assert_eq!(est.archive_source_count, 3);
        assert_eq!(est.archive_source_bytes, 3 * 100 * 1024 * 1024);
    }

    #[test]
    fn estimate_includes_archive_when_include_cold_true() {
        let profile = Profile::ml();
        let mut sources = make_sources(5, 100 * 1024 * 1024, false);
        sources.extend(make_sources(3, 100 * 1024 * 1024, true));
        let cal = CalibrationConfig::default();

        let est = estimate("ml", &profile, &sources, &cal, true);

        assert_eq!(est.source_count, 8);
        assert_eq!(est.archive_source_count, 3);
    }

    #[test]
    fn estimate_empty_sources() {
        let profile = Profile::code();
        let cal = CalibrationConfig::default();

        let est = estimate("code", &profile, &[], &cal, true);

        assert_eq!(est.source_count, 0);
        assert_eq!(est.estimated_dest_count, 0);
        assert_eq!(est.estimated_dest_bytes, 0);
        assert_eq!(est.estimated_wall_secs, 0);
    }

    #[test]
    fn compression_ratio_lookup() {
        let ml = Profile::ml();
        let ratio = lookup_compression_ratio(&ml);
        assert!((ratio - 0.75).abs() < 0.01); // Zstd(3) → 0.75

        let code = Profile::code();
        let ratio = lookup_compression_ratio(&code);
        assert!((ratio - 0.50).abs() < 0.01); // Zstd(9) → 0.50
    }

    #[test]
    fn calibration_config_defaults() {
        let cal = CalibrationConfig::default();
        assert_eq!(cal.throughput_bytes_per_sec, 100 * 1024 * 1024);
        assert!(cal.put_per_k_ops_usd > Decimal::ZERO);
        assert!(cal.gb_month_usd > Decimal::ZERO);
    }
}
