//! Configuration for the local staging area.

use crate::error::{Result, StagingError};

/// Default segment target size: 256 MiB.
const DEFAULT_SEGMENT_TARGET_BYTES: u64 = 256 * 1024 * 1024;

/// Default segment hard cap: 512 MiB.
const DEFAULT_SEGMENT_HARD_CAP_BYTES: u64 = 512 * 1024 * 1024;

/// Default number of open segment file descriptors in the reader pool.
const DEFAULT_FD_POOL_SIZE: usize = 64;

/// Default batch size for `get_chunks_batch` calls during push packing.
const DEFAULT_BATCH_READ_SIZE: usize = 256;

/// Default dead-byte ratio threshold for compaction eligibility.
const DEFAULT_COMPACT_DEAD_RATIO: f64 = 0.5;

/// Default retention window for stale push-inflight markers, in hours.
const DEFAULT_RETENTION_HOURS: u64 = 24;

/// Maximum allowed value for `compact_dead_ratio`.
const MAX_COMPACT_DEAD_RATIO: f64 = 0.95;

/// Configuration for the segment-based staging area.
#[derive(Debug, Clone)]
pub struct StagingConfig {
    pub segment_target_bytes: u64,
    pub segment_hard_cap_bytes: u64,
    pub fd_pool_size: usize,
    pub batch_read_size: usize,
    pub auto_compact: bool,
    pub compact_dead_ratio: f64,
    pub durable_register: bool,
    pub retention_hours: u64,
}

impl Default for StagingConfig {
    fn default() -> Self {
        Self {
            segment_target_bytes: DEFAULT_SEGMENT_TARGET_BYTES,
            segment_hard_cap_bytes: DEFAULT_SEGMENT_HARD_CAP_BYTES,
            fd_pool_size: DEFAULT_FD_POOL_SIZE,
            batch_read_size: DEFAULT_BATCH_READ_SIZE,
            auto_compact: false,
            compact_dead_ratio: DEFAULT_COMPACT_DEAD_RATIO,
            durable_register: true,
            retention_hours: DEFAULT_RETENTION_HOURS,
        }
    }
}

impl StagingConfig {
    pub fn validate(&self) -> Result<()> {
        if self.segment_target_bytes == 0 {
            return Err(StagingError::Configuration {
                key: "segment_target_bytes must be > 0".into(),
                origin: "staging".into(),
            });
        }
        if self.segment_hard_cap_bytes < self.segment_target_bytes {
            return Err(StagingError::Configuration {
                key: "segment_hard_cap_bytes must be >= segment_target_bytes".into(),
                origin: "staging".into(),
            });
        }
        if self.fd_pool_size == 0 {
            return Err(StagingError::Configuration {
                key: "fd_pool_size must be > 0".into(),
                origin: "staging".into(),
            });
        }
        if self.batch_read_size == 0 {
            return Err(StagingError::Configuration {
                key: "batch_read_size must be > 0".into(),
                origin: "staging".into(),
            });
        }
        if !(0.0..=MAX_COMPACT_DEAD_RATIO).contains(&self.compact_dead_ratio) {
            return Err(StagingError::Configuration {
                key: format!(
                    "compact_dead_ratio must be in 0.0..={MAX_COMPACT_DEAD_RATIO}, got {}",
                    self.compact_dead_ratio
                ),
                origin: "staging".into(),
            });
        }
        if self.retention_hours == 0 {
            return Err(StagingError::Configuration {
                key: "retention_hours must be > 0".into(),
                origin: "staging".into(),
            });
        }
        Ok(())
    }
}
