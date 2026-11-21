use crab_remote_git::{MetricKind, MetricObservation, RemoteGitMetrics};

use super::{Counter, Gauge, Histogram, Key, KeyName, METADATA, Metrics, Recorder, Unit, key};

pub(super) struct GitReadMetrics {
    checksum_bytes: Counter,
    checksum_seconds: Histogram,
    blame_producers: Counter,
    active_blame_producers: Gauge,
    blame_seconds: Histogram,
    cache_bytes: [Gauge; 5],
    cpu_jobs: Gauge,
    resident_bytes: Gauge,
    resident_capacity: Gauge,
    blame_waiters: Gauge,
    origin_operations: Gauge,
}

impl GitReadMetrics {
    pub(super) fn new(recorder: &impl Recorder) -> Self {
        super::describe_counter(
            recorder,
            "crab_git_cache_verification_bytes_total",
            "Git checksum bytes examined on retained-object hits, including corrupt objects.",
        );
        super::describe_counter(
            recorder,
            "crab_git_blame_producers_total",
            "Uncached attribution computations started; shared waiters are excluded.",
        );
        super::describe_gauge(
            recorder,
            "crab_git_active_blame_producers",
            "Uncached attribution futures currently alive; backend cleanup is tracked separately.",
        );
        super::describe_gauge(
            recorder,
            "crab_git_cache_bytes",
            "Estimated bytes retained by cache maps; excludes caller-held values and active buffers.",
        );
        super::describe_gauge(
            recorder,
            "crab_git_active_cpu_jobs",
            "Admitted tracked CPU jobs, including jobs whose awaiting caller has departed.",
        );
        super::describe_gauge(
            recorder,
            "crab_git_resident_read_bytes",
            "Admitted payload/result bytes and owner overhead retained through their last owner.",
        );
        super::describe_gauge(
            recorder,
            "crab_git_resident_read_capacity_bytes",
            "Maximum admitted payload/result capacity, separate from process RSS and cache occupancy.",
        );
        super::describe_gauge(
            recorder,
            "crab_git_active_blame_waiters",
            "Callers attached to complete attribution flights, including shared followers.",
        );
        super::describe_gauge(
            recorder,
            "crab_git_active_origin_operations",
            "Admitted object-store operations currently alive, including shared producers.",
        );
        for (name, description) in [
            (
                "crab_git_cache_verification_seconds",
                "Elapsed full-checksum verification time on cache hits.",
            ),
            (
                "crab_git_blame_producer_seconds",
                "Elapsed lifetime of uncached attribution futures.",
            ),
        ] {
            recorder.describe_histogram(
                KeyName::from_const_str(name),
                Some(Unit::Seconds),
                description.into(),
            );
        }
        Self {
            checksum_bytes: recorder.register_counter(
                &Key::from_static_name("crab_git_cache_verification_bytes_total"),
                &METADATA,
            ),
            checksum_seconds: recorder.register_histogram(
                &Key::from_static_name("crab_git_cache_verification_seconds"),
                &METADATA,
            ),
            blame_producers: recorder.register_counter(
                &Key::from_static_name("crab_git_blame_producers_total"),
                &METADATA,
            ),
            active_blame_producers: recorder.register_gauge(
                &Key::from_static_name("crab_git_active_blame_producers"),
                &METADATA,
            ),
            blame_seconds: recorder.register_histogram(
                &Key::from_static_name("crab_git_blame_producer_seconds"),
                &METADATA,
            ),
            cache_bytes: ["object", "pack_index", "parsed", "blame", "negative"].map(|class| {
                recorder
                    .register_gauge(&key("crab_git_cache_bytes", &[("class", class)]), &METADATA)
            }),
            cpu_jobs: recorder.register_gauge(
                &Key::from_static_name("crab_git_active_cpu_jobs"),
                &METADATA,
            ),
            resident_bytes: recorder.register_gauge(
                &Key::from_static_name("crab_git_resident_read_bytes"),
                &METADATA,
            ),
            resident_capacity: recorder.register_gauge(
                &Key::from_static_name("crab_git_resident_read_capacity_bytes"),
                &METADATA,
            ),
            blame_waiters: recorder.register_gauge(
                &Key::from_static_name("crab_git_active_blame_waiters"),
                &METADATA,
            ),
            origin_operations: recorder.register_gauge(
                &Key::from_static_name("crab_git_active_origin_operations"),
                &METADATA,
            ),
        }
    }
}

impl RemoteGitMetrics for Metrics {
    fn record(&self, observation: MetricObservation) {
        let metrics = &self.inner.git;
        match observation.kind {
            MetricKind::CpuJob | MetricKind::ResidentBytes => {
                let gauge = if observation.kind == MetricKind::CpuJob {
                    &metrics.cpu_jobs
                } else {
                    &metrics.resident_bytes
                };
                if observation.duration.is_none() {
                    gauge.increment(observation.value as f64);
                } else {
                    gauge.decrement(observation.value as f64);
                }
            }
            MetricKind::CacheVerification => {
                metrics.checksum_bytes.increment(observation.value);
                if let Some(duration) = observation.duration {
                    metrics.checksum_seconds.record(duration.as_secs_f64());
                }
            }
            MetricKind::BlameProducer => match observation.duration {
                Some(duration) => {
                    metrics.active_blame_producers.decrement(1);
                    metrics.blame_seconds.record(duration.as_secs_f64());
                }
                None => {
                    metrics.blame_producers.increment(1);
                    metrics.active_blame_producers.increment(1);
                }
            },
            _ => {}
        }
    }
}

impl Metrics {
    pub(crate) fn update_git_cache_occupancy(&self, snapshot: crab_remote_git::RuntimeSnapshot) {
        self.inner
            .git
            .resident_capacity
            .set(snapshot.resident_capacity_bytes as f64);
        self.inner
            .git
            .blame_waiters
            .set(snapshot.active_blame_waiters as f64);
        self.inner
            .git
            .origin_operations
            .set(snapshot.active_origin_operations as f64);
        for (gauge, bytes) in self.inner.git.cache_bytes.iter().zip([
            snapshot.object_bytes,
            snapshot.pack_index_bytes,
            snapshot.parsed_bytes,
            snapshot.blame_bytes,
            snapshot.negative_bytes,
        ]) {
            gauge.set(bytes as f64);
        }
    }
}
