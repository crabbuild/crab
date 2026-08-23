//! Auto-inference of xorb optimization profiles from repository statistics.
//!
//! [`infer`] examines the file-index to compute a [`RepoStats`] summary,
//! then selects the best built-in profile based on the p50 file size:
//!
//! - p50 > 100 MiB → `ml`
//! - p50 ≥ 1 MiB → `dataset`
//! - else → `code`
//!
//! The scan reads only the file-index metadata — no xorb bodies are
//! downloaded. Memory usage is bounded: file sizes are collected in a
//! streaming fashion and only the sorted sizes are held for percentile
//! computation.

use tracing::{debug, info};

use crate::optimize::xorbs::profile::Profile;

/// Threshold above which the `ml` profile is selected (100 MiB).
const ML_THRESHOLD: u64 = 100 * 1024 * 1024;

/// Threshold above which the `dataset` profile is selected (1 MiB).
const DATASET_THRESHOLD: u64 = 1024 * 1024;

/// Repository statistics computed from the file-index.
///
/// `RepoStats::scan` walks the file-index with bounded RAM. For
/// inference purposes only the file sizes matter — no xorb reads.
#[derive(Debug, Clone)]
pub struct RepoStats {
    /// Total number of files in the index.
    pub file_count: u64,
    /// Total bytes across all files.
    pub total_bytes: u64,
    /// Median (p50) file size in bytes.
    pub p50_file_size: u64,
    /// 90th percentile file size in bytes.
    pub p90_file_size: u64,
    /// Largest file size in bytes.
    pub max_file_size: u64,
}

impl RepoStats {
    /// Build stats from a pre-collected list of file sizes.
    ///
    /// The sizes slice is sorted internally. For a 10 M-file index this
    /// requires ~80 MB of RAM (10M × 8 bytes) which is within the
    /// bounded-RAM budget.
    pub fn from_sizes(sizes: &mut [u64]) -> Self {
        if sizes.is_empty() {
            return Self {
                file_count: 0,
                total_bytes: 0,
                p50_file_size: 0,
                p90_file_size: 0,
                max_file_size: 0,
            };
        }

        sizes.sort_unstable();
        let n = sizes.len();
        let total_bytes: u64 = sizes.iter().sum();
        let p50_idx = n / 2;
        let p90_idx = (n * 9) / 10;

        Self {
            file_count: n as u64,
            total_bytes,
            p50_file_size: sizes[p50_idx],
            p90_file_size: sizes[p90_idx.min(n - 1)],
            max_file_size: sizes[n - 1],
        }
    }

    /// Scan a repository's file-index to compute stats.
    ///
    /// This is a placeholder that accepts pre-collected sizes. The real
    /// implementation would walk the file-index via the metadata
    /// subsystem's streaming iterator.
    ///
    /// Memory budget: O(file_count × 8 bytes) for the sizes vector.
    /// For 10 M files this is ~80 MB.
    pub fn scan(file_sizes: Vec<u64>) -> Self {
        let mut sizes = file_sizes;
        Self::from_sizes(&mut sizes)
    }
}

/// Infer the best built-in profile from repository statistics.
///
/// Selection logic:
/// - p50 > 100 MiB → `ml` (large weight files, few xorbs per file)
/// - p50 ≥ 1 MiB → `dataset` (medium files, directory locality)
/// - else → `code` (small files, maximize dedup)
pub fn infer(stats: &RepoStats) -> Profile {
    let profile = if stats.p50_file_size > ML_THRESHOLD {
        info!(
            p50_mib = stats.p50_file_size / (1024 * 1024),
            "inferred profile: ml (p50 > 100 MiB)"
        );
        Profile::ml()
    } else if stats.p50_file_size >= DATASET_THRESHOLD {
        info!(
            p50_mib = stats.p50_file_size / (1024 * 1024),
            "inferred profile: dataset (p50 >= 1 MiB)"
        );
        Profile::dataset()
    } else {
        info!(
            p50_bytes = stats.p50_file_size,
            "inferred profile: code (p50 < 1 MiB)"
        );
        Profile::code()
    };

    debug!(
        file_count = stats.file_count,
        total_bytes = stats.total_bytes,
        p50 = stats.p50_file_size,
        p90 = stats.p90_file_size,
        max = stats.max_file_size,
        profile = %profile.group_by,
        target_mib = profile.target_xorb_bytes / (1024 * 1024),
        "profile inference complete"
    );

    profile
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::core::config::CompressionConfig;
    use crate::optimize::xorbs::profile::GroupBy;

    /// ML workload: large files (200+ MiB each).
    #[test]
    fn infer_ml_workload() {
        // 10 files, each 200 MiB
        let sizes: Vec<u64> = (0..10).map(|_| 200 * 1024 * 1024).collect();
        let stats = RepoStats::scan(sizes);

        assert_eq!(stats.file_count, 10);
        assert_eq!(stats.p50_file_size, 200 * 1024 * 1024);

        let profile = infer(&stats);
        assert_eq!(profile, Profile::ml());
        assert_eq!(profile.target_xorb_bytes, 256 * 1024 * 1024);
        assert_eq!(profile.group_by, GroupBy::File);
    }

    /// Dataset workload: medium files (5 MiB each).
    #[test]
    fn infer_dataset_workload() {
        // 1000 files, each 5 MiB
        let sizes: Vec<u64> = (0..1000).map(|_| 5 * 1024 * 1024).collect();
        let stats = RepoStats::scan(sizes);

        assert_eq!(stats.file_count, 1000);
        assert_eq!(stats.p50_file_size, 5 * 1024 * 1024);

        let profile = infer(&stats);
        assert_eq!(profile, Profile::dataset());
        assert_eq!(profile.target_xorb_bytes, 64 * 1024 * 1024);
        assert_eq!(profile.group_by, GroupBy::Directory);
    }

    /// Code workload: small files (10 KiB each).
    #[test]
    fn infer_code_workload() {
        // 50000 files, each 10 KiB
        let sizes: Vec<u64> = (0..50_000).map(|_| 10 * 1024).collect();
        let stats = RepoStats::scan(sizes);

        assert_eq!(stats.file_count, 50_000);
        assert_eq!(stats.p50_file_size, 10 * 1024);

        let profile = infer(&stats);
        assert_eq!(profile, Profile::code());
        assert_eq!(profile.target_xorb_bytes, 16 * 1024 * 1024);
        assert_eq!(profile.group_by, GroupBy::Hash);
        assert_eq!(profile.compression, CompressionConfig::Zstd { level: 9 });
    }

    /// Empty repo defaults to code profile.
    #[test]
    fn infer_empty_repo() {
        let stats = RepoStats::scan(vec![]);
        assert_eq!(stats.file_count, 0);
        assert_eq!(stats.p50_file_size, 0);

        let profile = infer(&stats);
        assert_eq!(profile, Profile::code());
    }

    /// Mixed workload: p50 is in the dataset range even though some
    /// files are very large.
    #[test]
    fn infer_mixed_workload() {
        let mut sizes: Vec<u64> = Vec::new();
        // 80 files at 2 MiB (dataset range)
        sizes.extend((0..80).map(|_| 2 * 1024 * 1024));
        // 20 files at 500 MiB (ML range)
        sizes.extend((0..20).map(|_| 500 * 1024 * 1024));

        let stats = RepoStats::scan(sizes);
        // p50 should be 2 MiB (the 50th file in sorted order)
        assert_eq!(stats.p50_file_size, 2 * 1024 * 1024);

        let profile = infer(&stats);
        assert_eq!(profile, Profile::dataset());
    }

    /// Boundary: p50 exactly at 1 MiB → dataset.
    #[test]
    fn infer_boundary_1mib() {
        let sizes: Vec<u64> = (0..100).map(|_| 1024 * 1024).collect();
        let stats = RepoStats::scan(sizes);
        assert_eq!(stats.p50_file_size, 1024 * 1024);

        let profile = infer(&stats);
        assert_eq!(profile, Profile::dataset());
    }

    /// Boundary: p50 exactly at 100 MiB → dataset (not ml, since > not >=).
    #[test]
    fn infer_boundary_100mib() {
        let sizes: Vec<u64> = (0..100).map(|_| 100 * 1024 * 1024).collect();
        let stats = RepoStats::scan(sizes);
        assert_eq!(stats.p50_file_size, 100 * 1024 * 1024);

        let profile = infer(&stats);
        // 100 MiB is NOT > 100 MiB, so dataset
        assert_eq!(profile, Profile::dataset());
    }

    /// RepoStats::scan handles bounded RAM for large file counts.
    #[test]
    fn repo_stats_scan_bounded_ram() {
        // Simulate 10M files with small sizes — just verify it doesn't
        // blow up. We use a smaller count for test speed.
        let sizes: Vec<u64> = (0..100_000).map(|i| (i % 1000) * 1024).collect();
        let stats = RepoStats::scan(sizes);
        assert_eq!(stats.file_count, 100_000);
        assert!(stats.p50_file_size > 0);
        assert!(stats.max_file_size > 0);
    }
}
