use std::{path::Path, sync::Arc, time::SystemTime};

use crab_cache::CacheCatalog;
use crab_cache_store::{CacheObserver, CacheReadObservation, CacheReadOutcome, CacheSource};
use metrics::{Counter, Gauge, Key, KeyName, Recorder, Unit};

use super::{METADATA, key};

#[derive(Clone)]
pub(super) struct CacheMetrics {
    inner: Arc<CacheMetricsInner>,
}

struct CacheMetricsInner {
    reads: [[Counter; CacheReadOutcome::ALL.len()]; CacheSource::ALL.len()],
    bytes_read: [Counter; CacheSource::ALL.len()],
    local_write_failures: Counter,
    limit: Gauge,
    retained: Gauge,
    entries: Gauge,
    reserved: Gauge,
    temporary: Gauge,
    probe_success: Gauge,
    probe_failures: Counter,
    last_success: Gauge,
    refresh: tokio::sync::Mutex<()>,
}

impl CacheMetrics {
    pub(super) fn new(recorder: &impl Recorder) -> Self {
        describe(recorder);
        Self {
            inner: Arc::new(CacheMetricsInner {
                reads: CacheSource::ALL.map(|source| {
                    CacheReadOutcome::ALL.map(|outcome| {
                        recorder.register_counter(
                            &key(
                                "crab_s3_gateway_cache_read_attempts_total",
                                &[("source", source.label()), ("outcome", outcome.label())],
                            ),
                            &METADATA,
                        )
                    })
                }),
                bytes_read: CacheSource::ALL.map(|source| {
                    recorder.register_counter(
                        &key(
                            "crab_s3_gateway_cache_bytes_read_total",
                            &[("source", source.label())],
                        ),
                        &METADATA,
                    )
                }),
                local_write_failures: recorder.register_counter(
                    &Key::from_static_name("crab_s3_gateway_cache_local_write_failures_total"),
                    &METADATA,
                ),
                limit: gauge(recorder, "crab_s3_gateway_cache_limit_bytes"),
                retained: gauge(recorder, "crab_s3_gateway_cache_retained_bytes"),
                entries: gauge(recorder, "crab_s3_gateway_cache_entries"),
                reserved: gauge(recorder, "crab_s3_gateway_cache_reserved_bytes"),
                temporary: gauge(recorder, "crab_s3_gateway_cache_temporary_bytes"),
                probe_success: gauge(recorder, "crab_s3_gateway_cache_catalog_probe_success"),
                probe_failures: recorder.register_counter(
                    &Key::from_static_name("crab_s3_gateway_cache_catalog_probe_failures_total"),
                    &METADATA,
                ),
                last_success: gauge(
                    recorder,
                    "crab_s3_gateway_cache_catalog_last_success_timestamp_seconds",
                ),
                refresh: tokio::sync::Mutex::new(()),
            }),
        }
    }

    pub(super) fn observer(&self) -> Arc<dyn CacheObserver> {
        Arc::new(self.clone())
    }

    pub(super) fn set_limit(&self, bytes: u64) {
        self.inner.limit.set(bytes as f64);
    }

    pub(super) async fn refresh(&self, root: &Path) {
        let Ok(_refresh) = self.inner.refresh.try_lock() else {
            return;
        };
        let root = root.to_owned();
        match tokio::task::spawn_blocking(move || CacheCatalog::read_only_stats(&root)).await {
            Ok(Ok(stats)) => {
                self.inner.retained.set(stats.total_bytes as f64);
                self.inner.entries.set(stats.entries as f64);
                self.inner.reserved.set(stats.reservations_bytes as f64);
                self.inner.temporary.set(stats.temporary_bytes as f64);
                self.inner.probe_success.set(1);
                if let Ok(now) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
                    self.inner.last_success.set(now.as_secs_f64());
                }
            }
            Ok(Err(error)) => {
                self.record_probe_failure();
                tracing::warn!(%error, "local cache catalog metrics probe failed");
            }
            Err(error) => {
                self.record_probe_failure();
                tracing::warn!(%error, "local cache catalog metrics worker failed");
            }
        }
    }

    fn record_probe_failure(&self) {
        self.inner.probe_success.set(0);
        self.inner.probe_failures.increment(1);
    }
}

impl CacheObserver for CacheMetrics {
    fn read(&self, observation: CacheReadObservation) {
        self.inner.reads[observation.source.index()][observation.outcome.index()].increment(1);
        self.inner.bytes_read[observation.source.index()].increment(observation.bytes);
    }

    fn local_write_failure(&self) {
        self.inner.local_write_failures.increment(1);
    }
}

fn gauge(recorder: &impl Recorder, name: &'static str) -> Gauge {
    recorder.register_gauge(&Key::from_static_name(name), &METADATA)
}

fn describe(recorder: &impl Recorder) {
    recorder.describe_counter(
        KeyName::from_const_str("crab_s3_gateway_cache_read_attempts_total"),
        None,
        "Cache read attempts by bounded source and terminal outcome.".into(),
    );
    recorder.describe_counter(
        KeyName::from_const_str("crab_s3_gateway_cache_bytes_read_total"),
        Some(Unit::Bytes),
        "Verified bytes returned by cache reads.".into(),
    );
    recorder.describe_counter(
        KeyName::from_const_str("crab_s3_gateway_cache_local_write_failures_total"),
        None,
        "Best-effort local cache writes that failed after verified bytes remained usable.".into(),
    );
    for (name, description) in [
        (
            "crab_s3_gateway_cache_limit_bytes",
            "Configured process-local cache retention ceiling.",
        ),
        (
            "crab_s3_gateway_cache_retained_bytes",
            "Bytes retained in the cache catalog.",
        ),
        (
            "crab_s3_gateway_cache_entries",
            "Entries retained in the cache catalog.",
        ),
        (
            "crab_s3_gateway_cache_reserved_bytes",
            "Bytes reserved by in-progress cache writes.",
        ),
        (
            "crab_s3_gateway_cache_temporary_bytes",
            "Retained cache bytes classified as temporary.",
        ),
        (
            "crab_s3_gateway_cache_catalog_probe_success",
            "Whether the latest cache catalog metrics probe succeeded.",
        ),
        (
            "crab_s3_gateway_cache_catalog_last_success_timestamp_seconds",
            "Unix timestamp of the latest successful cache catalog metrics probe.",
        ),
    ] {
        recorder.describe_gauge(KeyName::from_const_str(name), None, description.into());
    }
    recorder.describe_counter(
        KeyName::from_const_str("crab_s3_gateway_cache_catalog_probe_failures_total"),
        None,
        "Cache catalog metrics probes that failed.".into(),
    );
}
