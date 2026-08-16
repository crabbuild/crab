//! Metrics observer for staging operations.

/// Observer called by staging operations when counters change.
pub trait StagingMetrics: Send + Sync {
    fn add_staging_bytes_read(&self, _value: u64) {}
    fn add_staging_bytes_written(&self, _value: u64) {}
    fn inc_staging_segments_sealed(&self) {}
    fn inc_staging_segments_compacted(&self) {}
    fn inc_staging_fsyncs(&self) {}
    fn inc_staging_compactions_skipped_inflight(&self) {}
}
