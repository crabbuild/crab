//! Routing decision engine.
//!
//! Implements the LFS-vs-XET routing logic: size threshold, version count,
//! streaming entropy estimation, and user override.

use std::collections::HashMap;
use std::path::PathBuf;

/// The routing decision for a file.
#[derive(Debug, Clone, Copy)]
pub enum RoutingDecision {
    /// Route to LFS storage (SHA-256 + LFS pointer).
    Lfs(LfsReason),
    /// Route to XET/Crab storage (blake3 + CDC + xorb).
    Xet(XetReason),
}

/// Why a file was routed to LFS.
#[derive(Debug, Clone, Copy)]
pub enum LfsReason {
    /// Below the size threshold.
    BelowThreshold { size: u64, threshold: u64 },
    /// Single version — no dedup benefit.
    SingleVersion,
    /// High entropy — CDC produces few shared chunks.
    HighEntropy { entropy: f64, threshold: f64 },
    /// User explicitly chose LFS via `.gitattributes`.
    UserOverride,
}

/// Why a file was routed to XET.
#[derive(Debug, Clone, Copy)]
pub enum XetReason {
    /// Above threshold with multiple versions.
    MultiVersion { size: u64, versions: u64 },
    /// User explicitly chose XET via `.gitattributes`.
    UserOverride,
}

/// Configuration for the routing engine.
#[derive(Debug, Clone)]
pub struct RoutingConfig {
    /// Whether automatic routing is enabled.
    pub enabled: bool,
    /// Size threshold in bytes (default: 10 MB).
    pub lfs_xet_threshold: u64,
    /// Minimum versions for XET eligibility (default: 2).
    pub min_versions_for_xet: u64,
    /// Entropy threshold above which files go to LFS (default: 0.95).
    pub entropy_threshold: f64,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            lfs_xet_threshold: 10 * 1024 * 1024, // 10 MB
            min_versions_for_xet: 2,
            entropy_threshold: 0.95,
        }
    }
}

/// Cached version counts for files.
pub struct VersionCache {
    /// Maps (path_hash, blob_hash) → version count.
    counts: HashMap<(String, String), u64>,
}

impl VersionCache {
    /// Create an empty in-memory version cache (no disk persistence).
    pub fn new() -> Self {
        Self {
            counts: HashMap::new(),
        }
    }

    /// Load or create the version cache from a disk path.
    pub fn load(_path: PathBuf) -> Self {
        let counts = HashMap::new(); // In production: load from file.
        Self { counts }
    }

    /// Get the version count for a file, lazily populating from git.
    pub fn get(&mut self, pathname: &str, blob_hash: &str) -> u64 {
        let key = (pathname.to_string(), blob_hash.to_string());
        if let Some(count) = self.counts.get(&key) {
            return *count;
        }
        // Lazy populate from git.
        let count = query_version_count(pathname);
        self.counts.insert(key, count);
        count
    }
}

impl Default for VersionCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Query git for the number of commits that modified a file.
fn query_version_count(pathname: &str) -> u64 {
    std::process::Command::new("git")
        .args(["log", "--oneline", "--", pathname])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).lines().count() as u64)
            } else {
                None
            }
        })
        .unwrap_or(1) // Default to 1 if git fails (first version).
}

/// Streaming entropy estimator using a 256-bin byte-frequency histogram.
pub struct EntropyEstimator {
    histogram: [u64; 256],
    total_bytes: u64,
}

impl EntropyEstimator {
    pub fn new() -> Self {
        Self {
            histogram: [0; 256],
            total_bytes: 0,
        }
    }

    /// Feed bytes into the estimator (streaming, no seeking).
    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.histogram[b as usize] = self.histogram[b as usize].saturating_add(1);
        }
        self.total_bytes = self.total_bytes.saturating_add(bytes.len() as u64);
    }

    /// Compute normalized Shannon entropy (0.0 to 1.0).
    pub fn entropy(&self) -> f64 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        let total = self.total_bytes as f64;
        let mut h = 0.0f64;
        for &count in &self.histogram {
            if count > 0 {
                let p = count as f64 / total;
                h -= p * p.log2();
            }
        }
        h / 8.0 // Normalize to [0, 1] (max entropy for 256 bins = 8 bits)
    }
}

impl Default for EntropyEstimator {
    fn default() -> Self {
        Self::new()
    }
}

/// Accumulated routing statistics.
#[derive(Debug, Clone, Default)]
pub struct RoutingStats {
    pub lfs_below_threshold: u64,
    pub lfs_single_version: u64,
    pub lfs_high_entropy: u64,
    pub lfs_user_override: u64,
    pub xet_multi_version: u64,
    pub xet_user_override: u64,
}

impl RoutingStats {
    pub fn record(&mut self, decision: &RoutingDecision) {
        match decision {
            RoutingDecision::Lfs(reason) => match reason {
                LfsReason::BelowThreshold { .. } => self.lfs_below_threshold += 1,
                LfsReason::SingleVersion => self.lfs_single_version += 1,
                LfsReason::HighEntropy { .. } => self.lfs_high_entropy += 1,
                LfsReason::UserOverride => self.lfs_user_override += 1,
            },
            RoutingDecision::Xet(reason) => match reason {
                XetReason::MultiVersion { .. } => self.xet_multi_version += 1,
                XetReason::UserOverride => self.xet_user_override += 1,
            },
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "LFS (size):      {}\n\
             LFS (entropy):   {}\n\
             LFS (single-ver):{}\n\
             LFS (override):  {}\n\
             XET (multi-ver): {}\n\
             XET (override):  {}",
            self.lfs_below_threshold,
            self.lfs_high_entropy,
            self.lfs_single_version,
            self.lfs_user_override,
            self.xet_multi_version,
            self.xet_user_override,
        )
    }
}

/// Make a routing decision for a file.
pub fn decide_routing(
    config: &RoutingConfig,
    pathname: &str,
    file_size: u64,
    version_cache: &mut VersionCache,
    blob_hash: &str,
    entropy_estimator: &EntropyEstimator,
    user_override: Option<crate::git::filter_attr_cache::FilterKind>,
) -> RoutingDecision {
    // 1. User override always wins.
    if let Some(filter) = user_override {
        match filter {
            crate::git::filter_attr_cache::FilterKind::Lfs => {
                return RoutingDecision::Lfs(LfsReason::UserOverride);
            }
            crate::git::filter_attr_cache::FilterKind::Crab => {
                return RoutingDecision::Xet(XetReason::UserOverride);
            }
        }
    }

    // 2. Routing disabled → default to XET (current behavior).
    if !config.enabled {
        return RoutingDecision::Xet(XetReason::MultiVersion {
            size: file_size,
            versions: 1,
        });
    }

    // 3. Size below threshold → LFS.
    if file_size < config.lfs_xet_threshold {
        return RoutingDecision::Lfs(LfsReason::BelowThreshold {
            size: file_size,
            threshold: config.lfs_xet_threshold,
        });
    }

    // 4. Version count → single version means no dedup benefit.
    let versions = version_cache.get(pathname, blob_hash);
    if versions < config.min_versions_for_xet {
        return RoutingDecision::Lfs(LfsReason::SingleVersion);
    }

    // 5. Entropy → high entropy means CDC finds few shared chunks.
    let entropy = entropy_estimator.entropy();
    if entropy > config.entropy_threshold {
        return RoutingDecision::Lfs(LfsReason::HighEntropy {
            entropy,
            threshold: config.entropy_threshold,
        });
    }

    // 6. Default → XET.
    RoutingDecision::Xet(XetReason::MultiVersion {
        size: file_size,
        versions,
    })
}
