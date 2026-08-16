//! Prometheus metrics and structured tracing for the cache service.
//!
//! Sets up a `PrometheusBuilder` from `metrics-exporter-prometheus` that
//! installs a global recorder and returns a `PrometheusHandle`. The handle
//! is stored in `CacheMetrics` so the `/v1/metrics` endpoint can call
//! `handle.render()` to produce Prometheus exposition format text.
//!
//! Recording methods use the `metrics` crate macros (`counter!`, `gauge!`,
//! `histogram!`) which emit to whatever recorder is installed globally.
//! Each method corresponds to an observable cache-service event.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

use crate::cache_store::{CacheEvictionStats, ObjectType};

pub(crate) fn record_cache_integrity_repairs(phase: &'static str, event: &'static str, count: u64) {
    if count == 0 {
        return;
    }
    counter!("cache_integrity_repair_total", "phase" => phase, "event" => event).increment(count);
}

#[derive(Default)]
struct TrafficCounters {
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    coalesced_misses: AtomicU64,
    origin_fetches: AtomicU64,
    origin_head_requests: AtomicU64,
    origin_fetch_bytes: AtomicU64,
    bytes_served_from_cache: AtomicU64,
    bytes_served_from_origin: AtomicU64,
    push_warming_writes: AtomicU64,
    push_warming_bytes: AtomicU64,
    dedup_queries: AtomicU64,
    dedup_known_chunks: AtomicU64,
    dedup_unknown_chunks: AtomicU64,
    cache_bytes_stored: AtomicU64,
    inflight_misses: AtomicU64,
    active_connections: AtomicU64,
    mutable_read_rejections: AtomicU64,
    mutable_write_rejections: AtomicU64,
    mutable_proxy_gets: AtomicU64,
    mutable_proxy_heads: AtomicU64,
    mutable_proxy_bytes: AtomicU64,
    mutable_proxy_stream_errors: AtomicU64,
    by_object_type: TrafficByObjectTypeCounters,
}

#[derive(Default)]
struct TrafficByObjectTypeCounters {
    xorb: ObjectTrafficCounters,
    shard: ObjectTrafficCounters,
    pack: ObjectTrafficCounters,
    pack_index: ObjectTrafficCounters,
    metadata: ObjectTrafficCounters,
}

impl TrafficByObjectTypeCounters {
    fn for_type(&self, object_type: ObjectType) -> &ObjectTrafficCounters {
        match object_type {
            ObjectType::Xorb => &self.xorb,
            ObjectType::Shard => &self.shard,
            ObjectType::Pack => &self.pack,
            ObjectType::PackIndex => &self.pack_index,
            ObjectType::Metadata => &self.metadata,
        }
    }

    fn snapshot(&self) -> TrafficByObjectTypeStats {
        TrafficByObjectTypeStats {
            xorb: self.xorb.snapshot(),
            shard: self.shard.snapshot(),
            pack: self.pack.snapshot(),
            pack_index: self.pack_index.snapshot(),
            metadata: self.metadata.snapshot(),
        }
    }
}

#[derive(Default)]
struct ObjectTrafficCounters {
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    coalesced_misses: AtomicU64,
    origin_fetches: AtomicU64,
    origin_head_requests: AtomicU64,
    origin_fetch_bytes: AtomicU64,
    bytes_served_from_cache: AtomicU64,
    bytes_served_from_origin: AtomicU64,
    push_warming_writes: AtomicU64,
    push_warming_bytes: AtomicU64,
}

impl ObjectTrafficCounters {
    fn snapshot(&self) -> ObjectTrafficStats {
        let cache_hits = self.cache_hits.load(Ordering::Relaxed);
        let bytes_served_from_cache = self.bytes_served_from_cache.load(Ordering::Relaxed);
        let bytes_served_from_origin = self.bytes_served_from_origin.load(Ordering::Relaxed);

        ObjectTrafficStats {
            cache_hits,
            cache_misses: self.cache_misses.load(Ordering::Relaxed),
            origin_avoided_reads: cache_hits,
            coalesced_misses: self.coalesced_misses.load(Ordering::Relaxed),
            origin_fetches: self.origin_fetches.load(Ordering::Relaxed),
            origin_head_requests: self.origin_head_requests.load(Ordering::Relaxed),
            origin_fetch_bytes: self.origin_fetch_bytes.load(Ordering::Relaxed),
            bytes_served_from_cache,
            bytes_served_from_origin,
            bytes_served_total: bytes_served_from_cache.saturating_add(bytes_served_from_origin),
            push_warming_writes: self.push_warming_writes.load(Ordering::Relaxed),
            push_warming_bytes: self.push_warming_bytes.load(Ordering::Relaxed),
        }
    }
}

/// Low-cardinality traffic counters returned by the admin stats endpoint.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct TrafficStats {
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub origin_avoided_reads: u64,
    pub coalesced_misses: u64,
    pub origin_fetches: u64,
    pub origin_head_requests: u64,
    pub origin_fetch_bytes: u64,
    pub bytes_served_from_cache: u64,
    pub bytes_served_from_origin: u64,
    pub bytes_served_total: u64,
    pub push_warming_writes: u64,
    pub push_warming_bytes: u64,
    pub dedup_queries: u64,
    pub dedup_known_chunks: u64,
    pub dedup_unknown_chunks: u64,
    pub inflight_misses: u64,
    pub active_connections: u64,
    pub mutable_read_rejections: u64,
    pub mutable_write_rejections: u64,
    pub mutable_proxy_reads: u64,
    pub mutable_proxy_gets: u64,
    pub mutable_proxy_heads: u64,
    pub mutable_proxy_bytes: u64,
    pub mutable_proxy_stream_errors: u64,
    pub by_object_type: TrafficByObjectTypeStats,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct TrafficByObjectTypeStats {
    pub xorb: ObjectTrafficStats,
    pub shard: ObjectTrafficStats,
    pub pack: ObjectTrafficStats,
    pub pack_index: ObjectTrafficStats,
    pub metadata: ObjectTrafficStats,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ObjectTrafficStats {
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub origin_avoided_reads: u64,
    pub coalesced_misses: u64,
    pub origin_fetches: u64,
    pub origin_head_requests: u64,
    pub origin_fetch_bytes: u64,
    pub bytes_served_from_cache: u64,
    pub bytes_served_from_origin: u64,
    pub bytes_served_total: u64,
    pub push_warming_writes: u64,
    pub push_warming_bytes: u64,
}

/// Holds the Prometheus recorder handle for rendering metrics on demand.
///
/// All recording methods are cheap (atomic increments / CAS) and safe to
/// call from any async context without blocking.
pub struct CacheMetrics {
    /// When `Some`, the recorder is installed and `render()` produces real
    /// Prometheus exposition output. `None` in test stubs.
    handle: Option<PrometheusHandle>,
    counters: TrafficCounters,
}

impl CacheMetrics {
    /// Install the Prometheus recorder globally and return a `CacheMetrics`
    /// that can render the current metrics snapshot.
    ///
    /// This should be called once during server startup. Calling it more than
    /// once will fail because the global recorder is already set.
    pub fn new() -> Result<Self, metrics_exporter_prometheus::BuildError> {
        let handle = PrometheusBuilder::new().install_recorder()?;
        Ok(Self {
            handle: Some(handle),
            counters: TrafficCounters::default(),
        })
    }

    /// Create a stub `CacheMetrics` for tests that don't need real metrics.
    /// `render()` returns an empty string; recording methods still call the
    /// `metrics` macros but they no-op without an installed recorder.
    pub fn stub() -> Self {
        Self {
            handle: None,
            counters: TrafficCounters::default(),
        }
    }

    /// Render the current metrics snapshot in Prometheus exposition format.
    pub fn render(&self) -> String {
        self.render_with_cache_store(0, 0, &CacheEvictionStats::default())
    }

    /// Render metrics with cache-store capacity and eviction counters included.
    pub fn render_with_cache_store(
        &self,
        cache_max_bytes: u64,
        cache_max_object_bytes: u64,
        eviction_stats: &CacheEvictionStats,
    ) -> String {
        let mut body = String::new();
        append_authoritative_traffic_metrics(
            &mut body,
            &self.snapshot(),
            self.counters.cache_bytes_stored.load(Ordering::Relaxed),
            cache_max_bytes,
            cache_max_object_bytes,
            eviction_stats,
        );

        if let Some(handle) = &self.handle {
            append_exporter_metrics(&mut body, &handle.render());
        }

        body
    }

    /// Return a JSON-friendly snapshot of the in-process traffic counters.
    pub fn snapshot(&self) -> TrafficStats {
        let cache_hits = self.counters.cache_hits.load(Ordering::Relaxed);
        let bytes_served_from_cache = self
            .counters
            .bytes_served_from_cache
            .load(Ordering::Relaxed);
        let bytes_served_from_origin = self
            .counters
            .bytes_served_from_origin
            .load(Ordering::Relaxed);
        let mutable_proxy_gets = self.counters.mutable_proxy_gets.load(Ordering::Relaxed);
        let mutable_proxy_heads = self.counters.mutable_proxy_heads.load(Ordering::Relaxed);

        TrafficStats {
            cache_hits,
            cache_misses: self.counters.cache_misses.load(Ordering::Relaxed),
            origin_avoided_reads: cache_hits,
            coalesced_misses: self.counters.coalesced_misses.load(Ordering::Relaxed),
            origin_fetches: self.counters.origin_fetches.load(Ordering::Relaxed),
            origin_head_requests: self.counters.origin_head_requests.load(Ordering::Relaxed),
            origin_fetch_bytes: self.counters.origin_fetch_bytes.load(Ordering::Relaxed),
            bytes_served_from_cache,
            bytes_served_from_origin,
            bytes_served_total: bytes_served_from_cache.saturating_add(bytes_served_from_origin),
            push_warming_writes: self.counters.push_warming_writes.load(Ordering::Relaxed),
            push_warming_bytes: self.counters.push_warming_bytes.load(Ordering::Relaxed),
            dedup_queries: self.counters.dedup_queries.load(Ordering::Relaxed),
            dedup_known_chunks: self.counters.dedup_known_chunks.load(Ordering::Relaxed),
            dedup_unknown_chunks: self.counters.dedup_unknown_chunks.load(Ordering::Relaxed),
            inflight_misses: self.counters.inflight_misses.load(Ordering::Relaxed),
            active_connections: self.counters.active_connections.load(Ordering::Relaxed),
            mutable_read_rejections: self
                .counters
                .mutable_read_rejections
                .load(Ordering::Relaxed),
            mutable_write_rejections: self
                .counters
                .mutable_write_rejections
                .load(Ordering::Relaxed),
            mutable_proxy_reads: mutable_proxy_gets.saturating_add(mutable_proxy_heads),
            mutable_proxy_gets,
            mutable_proxy_heads,
            mutable_proxy_bytes: self.counters.mutable_proxy_bytes.load(Ordering::Relaxed),
            mutable_proxy_stream_errors: self
                .counters
                .mutable_proxy_stream_errors
                .load(Ordering::Relaxed),
            by_object_type: self.counters.by_object_type.snapshot(),
        }
    }

    // --- Cache hit / miss ---

    /// Record a cache hit for the given object type.
    pub fn record_cache_hit(&self, object_type: ObjectType) {
        self.counters.cache_hits.fetch_add(1, Ordering::Relaxed);
        self.counters
            .by_object_type
            .for_type(object_type)
            .cache_hits
            .fetch_add(1, Ordering::Relaxed);
        counter!("cache_hit_total", "object_type" => object_type.metric_label().to_string())
            .increment(1);
    }

    /// Record a cache miss for the given object type.
    pub fn record_cache_miss(&self, object_type: ObjectType) {
        self.counters.cache_misses.fetch_add(1, Ordering::Relaxed);
        self.counters
            .by_object_type
            .for_type(object_type)
            .cache_misses
            .fetch_add(1, Ordering::Relaxed);
        counter!("cache_miss_total", "object_type" => object_type.metric_label().to_string())
            .increment(1);
    }

    /// Record a miss request that waited for another request's origin fill.
    pub fn record_coalesced_miss(&self, object_type: ObjectType) {
        self.counters
            .coalesced_misses
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .by_object_type
            .for_type(object_type)
            .coalesced_misses
            .fetch_add(1, Ordering::Relaxed);
        counter!("cache_miss_coalesced_total", "object_type" => object_type.metric_label().to_string()).increment(1);
    }

    // --- Bytes served / stored ---

    /// Record bytes served to a client, tagged by hit vs miss.
    pub fn record_bytes_served(&self, object_type: ObjectType, bytes: u64, hit: bool) {
        let object_counters = self.counters.by_object_type.for_type(object_type);
        if hit {
            self.counters
                .bytes_served_from_cache
                .fetch_add(bytes, Ordering::Relaxed);
            object_counters
                .bytes_served_from_cache
                .fetch_add(bytes, Ordering::Relaxed);
        } else {
            self.counters
                .bytes_served_from_origin
                .fetch_add(bytes, Ordering::Relaxed);
            object_counters
                .bytes_served_from_origin
                .fetch_add(bytes, Ordering::Relaxed);
        }
        counter!("cache_bytes_served", "hit" => hit.to_string(), "object_type" => object_type.metric_label().to_string()).increment(bytes);
    }

    /// Set the current total bytes stored in the cache (gauge).
    pub fn set_bytes_stored(&self, bytes: u64) {
        self.counters
            .cache_bytes_stored
            .store(bytes, Ordering::Relaxed);
        gauge!("cache_bytes_stored").set(bytes as f64);
    }

    // --- Dedup queries ---

    /// Record a dedup query with its latency and result counts.
    pub fn record_dedup_query(&self, latency_ms: f64, known: u64, unknown: u64) {
        self.counters.dedup_queries.fetch_add(1, Ordering::Relaxed);
        self.counters
            .dedup_known_chunks
            .fetch_add(known, Ordering::Relaxed);
        self.counters
            .dedup_unknown_chunks
            .fetch_add(unknown, Ordering::Relaxed);
        counter!("dedup_query_total").increment(1);
        histogram!("dedup_query_latency_ms").record(latency_ms);
        counter!("dedup_chunks_known").increment(known);
        counter!("dedup_chunks_unknown").increment(unknown);
    }

    // --- Origin fetches ---

    /// Record an origin fetch with its latency and byte count.
    pub fn record_origin_fetch(&self, object_type: ObjectType, latency_ms: f64, bytes: u64) {
        self.counters.origin_fetches.fetch_add(1, Ordering::Relaxed);
        self.counters
            .origin_fetch_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        let object_counters = self.counters.by_object_type.for_type(object_type);
        object_counters
            .origin_fetches
            .fetch_add(1, Ordering::Relaxed);
        object_counters
            .origin_fetch_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        counter!("origin_fetch_total", "object_type" => object_type.metric_label().to_string())
            .increment(1);
        histogram!("origin_fetch_latency_ms", "object_type" => object_type.metric_label().to_string()).record(latency_ms);
        counter!("origin_fetch_bytes", "object_type" => object_type.metric_label().to_string())
            .increment(bytes);
    }

    /// Record an origin metadata HEAD request.
    pub fn record_origin_head(&self, object_type: ObjectType, latency_ms: f64) {
        self.counters
            .origin_head_requests
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .by_object_type
            .for_type(object_type)
            .origin_head_requests
            .fetch_add(1, Ordering::Relaxed);
        counter!("origin_head_total", "object_type" => object_type.metric_label().to_string())
            .increment(1);
        histogram!("origin_head_latency_ms", "object_type" => object_type.metric_label().to_string()).record(latency_ms);
    }

    // --- Mutable route handling ---

    /// Record a strict-mode mutable read rejected before it can reach origin.
    pub fn record_mutable_read_rejection(&self, method: &'static str) {
        self.counters
            .mutable_read_rejections
            .fetch_add(1, Ordering::Relaxed);
        counter!("mutable_path_rejection_total", "method" => method).increment(1);
    }

    /// Record a mutable write rejected before it can populate the cache.
    pub fn record_mutable_write_rejection(&self) {
        self.counters
            .mutable_write_rejections
            .fetch_add(1, Ordering::Relaxed);
        counter!("mutable_path_rejection_total", "method" => "PUT").increment(1);
    }

    /// Record a transparent-mode mutable GET proxied to origin without cache.
    pub fn record_mutable_proxy_get(&self) {
        self.counters
            .mutable_proxy_gets
            .fetch_add(1, Ordering::Relaxed);
        counter!("mutable_path_proxy_read_total", "method" => "GET").increment(1);
    }

    /// Record a transparent-mode mutable HEAD proxied to origin without cache.
    pub fn record_mutable_proxy_head(&self) {
        self.counters
            .mutable_proxy_heads
            .fetch_add(1, Ordering::Relaxed);
        counter!("mutable_path_proxy_read_total", "method" => "HEAD").increment(1);
    }

    /// Record the response bytes for a successful transparent-mode mutable GET.
    pub fn record_mutable_proxy_bytes(&self, bytes: u64) {
        self.counters
            .mutable_proxy_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        counter!("mutable_path_proxy_bytes", "method" => "GET").increment(bytes);
    }

    /// Record a transparent-mode mutable proxy stream failure after response start.
    pub fn record_mutable_proxy_stream_error(&self, method: &'static str) {
        self.counters
            .mutable_proxy_stream_errors
            .fetch_add(1, Ordering::Relaxed);
        counter!("mutable_path_proxy_stream_error_total", "method" => method).increment(1);
    }

    // --- Push warming ---

    /// Record a push-warming write.
    pub fn record_push_warming(&self, object_type: ObjectType, bytes: u64) {
        self.counters
            .push_warming_writes
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .push_warming_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        let object_counters = self.counters.by_object_type.for_type(object_type);
        object_counters
            .push_warming_writes
            .fetch_add(1, Ordering::Relaxed);
        object_counters
            .push_warming_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        counter!("push_warming_total", "object_type" => object_type.metric_label().to_string())
            .increment(1);
        counter!("push_warming_bytes", "object_type" => object_type.metric_label().to_string())
            .increment(bytes);
    }

    /// Set the current number of object miss fills in flight.
    pub fn set_inflight_misses(&self, count: u64) {
        self.counters
            .inflight_misses
            .store(count, Ordering::Relaxed);
        gauge!("cache_inflight_misses").set(count as f64);
    }

    // --- Active connections ---

    /// Set the current number of active connections (gauge).
    pub fn set_active_connections(&self, count: u64) {
        self.counters
            .active_connections
            .store(count, Ordering::Relaxed);
        gauge!("active_connections").set(count as f64);
    }
}

const AUTHORITATIVE_PROMETHEUS_METRICS: &[&str] = &[
    "active_connections",
    "cache_bytes_served",
    "cache_eviction_total",
    "cache_hit_total",
    "cache_inflight_misses",
    "cache_max_bytes",
    "cache_max_object_bytes",
    "cache_bytes_stored",
    "cache_miss_coalesced_total",
    "cache_miss_total",
    "dedup_chunks_known",
    "dedup_chunks_unknown",
    "dedup_query_total",
    "mutable_path_proxy_bytes",
    "mutable_path_proxy_read_total",
    "mutable_path_proxy_stream_error_total",
    "origin_avoided_reads_total",
    "origin_fetch_bytes",
    "origin_fetch_total",
    "origin_head_total",
    "push_warming_bytes",
    "push_warming_total",
];

fn append_authoritative_traffic_metrics(
    body: &mut String,
    stats: &TrafficStats,
    cache_bytes_stored: u64,
    cache_max_bytes: u64,
    cache_max_object_bytes: u64,
    eviction_stats: &CacheEvictionStats,
) {
    append_metric_header(
        body,
        "cache_hit_total",
        "Cache hits by immutable object type.",
        "counter",
    );
    for_each_object_stats(&stats.by_object_type, |object_type, object_stats| {
        append_prometheus_u64(
            body,
            "cache_hit_total",
            &[("object_type", object_type)],
            object_stats.cache_hits,
        );
    });

    append_metric_header(
        body,
        "origin_avoided_reads_total",
        "Origin reads avoided by immutable object type.",
        "counter",
    );
    for_each_object_stats(&stats.by_object_type, |object_type, object_stats| {
        append_prometheus_u64(
            body,
            "origin_avoided_reads_total",
            &[("object_type", object_type)],
            object_stats.origin_avoided_reads,
        );
    });

    append_metric_header(
        body,
        "cache_miss_total",
        "Cache misses by immutable object type.",
        "counter",
    );
    for_each_object_stats(&stats.by_object_type, |object_type, object_stats| {
        append_prometheus_u64(
            body,
            "cache_miss_total",
            &[("object_type", object_type)],
            object_stats.cache_misses,
        );
    });

    append_metric_header(
        body,
        "cache_miss_coalesced_total",
        "Cold miss requests served by an in-flight fill.",
        "counter",
    );
    for_each_object_stats(&stats.by_object_type, |object_type, object_stats| {
        append_prometheus_u64(
            body,
            "cache_miss_coalesced_total",
            &[("object_type", object_type)],
            object_stats.coalesced_misses,
        );
    });

    append_metric_header(
        body,
        "cache_bytes_served",
        "Bytes served to clients by immutable object type and hit status.",
        "counter",
    );
    for_each_object_stats(&stats.by_object_type, |object_type, object_stats| {
        append_prometheus_u64(
            body,
            "cache_bytes_served",
            &[("object_type", object_type), ("hit", "true")],
            object_stats.bytes_served_from_cache,
        );
        append_prometheus_u64(
            body,
            "cache_bytes_served",
            &[("object_type", object_type), ("hit", "false")],
            object_stats.bytes_served_from_origin,
        );
    });

    append_metric_header(
        body,
        "origin_fetch_total",
        "Origin body fetches by immutable object type.",
        "counter",
    );
    for_each_object_stats(&stats.by_object_type, |object_type, object_stats| {
        append_prometheus_u64(
            body,
            "origin_fetch_total",
            &[("object_type", object_type)],
            object_stats.origin_fetches,
        );
    });

    append_metric_header(
        body,
        "origin_fetch_bytes",
        "Bytes fetched from origin by immutable object type.",
        "counter",
    );
    for_each_object_stats(&stats.by_object_type, |object_type, object_stats| {
        append_prometheus_u64(
            body,
            "origin_fetch_bytes",
            &[("object_type", object_type)],
            object_stats.origin_fetch_bytes,
        );
    });

    append_metric_header(
        body,
        "origin_head_total",
        "Origin metadata HEAD requests by immutable object type.",
        "counter",
    );
    for_each_object_stats(&stats.by_object_type, |object_type, object_stats| {
        append_prometheus_u64(
            body,
            "origin_head_total",
            &[("object_type", object_type)],
            object_stats.origin_head_requests,
        );
    });

    append_metric_header(
        body,
        "push_warming_total",
        "Successful push-warming writes by immutable object type.",
        "counter",
    );
    for_each_object_stats(&stats.by_object_type, |object_type, object_stats| {
        append_prometheus_u64(
            body,
            "push_warming_total",
            &[("object_type", object_type)],
            object_stats.push_warming_writes,
        );
    });

    append_metric_header(
        body,
        "push_warming_bytes",
        "Bytes written through push warming by immutable object type.",
        "counter",
    );
    for_each_object_stats(&stats.by_object_type, |object_type, object_stats| {
        append_prometheus_u64(
            body,
            "push_warming_bytes",
            &[("object_type", object_type)],
            object_stats.push_warming_bytes,
        );
    });

    append_metric_header(
        body,
        "cache_eviction_total",
        "Cache objects removed by eviction path and immutable object type.",
        "counter",
    );
    for_each_eviction_stats(eviction_stats, |object_type, count| {
        append_prometheus_u64(
            body,
            "cache_eviction_total",
            &[("object_type", object_type)],
            count,
        );
    });

    append_metric_header(body, "dedup_query_total", "Dedup query count.", "counter");
    append_prometheus_u64(body, "dedup_query_total", &[], stats.dedup_queries);
    append_metric_header(
        body,
        "dedup_chunks_known",
        "Chunks reported known by dedup queries.",
        "counter",
    );
    append_prometheus_u64(body, "dedup_chunks_known", &[], stats.dedup_known_chunks);
    append_metric_header(
        body,
        "dedup_chunks_unknown",
        "Chunks reported unknown by dedup queries.",
        "counter",
    );
    append_prometheus_u64(
        body,
        "dedup_chunks_unknown",
        &[],
        stats.dedup_unknown_chunks,
    );

    append_metric_header(
        body,
        "mutable_path_proxy_read_total",
        "Transparent-mode mutable reads proxied to origin.",
        "counter",
    );
    append_prometheus_u64(
        body,
        "mutable_path_proxy_read_total",
        &[("method", "GET")],
        stats.mutable_proxy_gets,
    );
    append_prometheus_u64(
        body,
        "mutable_path_proxy_read_total",
        &[("method", "HEAD")],
        stats.mutable_proxy_heads,
    );

    append_metric_header(
        body,
        "mutable_path_proxy_bytes",
        "Transparent-mode mutable GET response bytes proxied from origin.",
        "counter",
    );
    append_prometheus_u64(
        body,
        "mutable_path_proxy_bytes",
        &[("method", "GET")],
        stats.mutable_proxy_bytes,
    );

    append_metric_header(
        body,
        "mutable_path_proxy_stream_error_total",
        "Transparent-mode mutable proxy body streams that failed after response start.",
        "counter",
    );
    append_prometheus_u64(
        body,
        "mutable_path_proxy_stream_error_total",
        &[("method", "GET")],
        stats.mutable_proxy_stream_errors,
    );

    append_metric_header(body, "cache_bytes_stored", "Current cache size.", "gauge");
    append_prometheus_u64(body, "cache_bytes_stored", &[], cache_bytes_stored);
    append_metric_header(
        body,
        "cache_max_bytes",
        "Configured cache byte budget.",
        "gauge",
    );
    append_prometheus_u64(body, "cache_max_bytes", &[], cache_max_bytes);
    append_metric_header(
        body,
        "cache_max_object_bytes",
        "Maximum accepted cache object request body size.",
        "gauge",
    );
    append_prometheus_u64(body, "cache_max_object_bytes", &[], cache_max_object_bytes);
    append_metric_header(
        body,
        "cache_inflight_misses",
        "Current object fills in progress.",
        "gauge",
    );
    append_prometheus_u64(body, "cache_inflight_misses", &[], stats.inflight_misses);
    append_metric_header(
        body,
        "active_connections",
        "Current active HTTP connections.",
        "gauge",
    );
    append_prometheus_u64(body, "active_connections", &[], stats.active_connections);
}

fn for_each_object_stats(
    stats: &TrafficByObjectTypeStats,
    mut f: impl FnMut(&'static str, &ObjectTrafficStats),
) {
    f("xorb", &stats.xorb);
    f("shard", &stats.shard);
    f("pack", &stats.pack);
    f("pack_index", &stats.pack_index);
    f("metadata", &stats.metadata);
}

fn for_each_eviction_stats(stats: &CacheEvictionStats, mut f: impl FnMut(&'static str, u64)) {
    f("xorb", stats.xorb);
    f("shard", stats.shard);
    f("pack", stats.pack);
    f("pack_index", stats.pack_index);
    f("metadata", stats.metadata);
}

fn append_exporter_metrics(body: &mut String, exporter_body: &str) {
    for line in exporter_body.lines() {
        if prometheus_line_metric_name(line).is_some_and(is_authoritative_prometheus_metric) {
            continue;
        }
        let _ = writeln!(body, "{line}");
    }
}

fn prometheus_line_metric_name(line: &str) -> Option<&str> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    if let Some(rest) = line.strip_prefix("# HELP ") {
        return rest.split_whitespace().next();
    }
    if let Some(rest) = line.strip_prefix("# TYPE ") {
        return rest.split_whitespace().next();
    }

    let series = line.split_whitespace().next()?;
    Some(series.split_once('{').map_or(series, |(metric, _)| metric))
}

fn is_authoritative_prometheus_metric(name: &str) -> bool {
    AUTHORITATIVE_PROMETHEUS_METRICS.contains(&name)
}

fn append_metric_header(body: &mut String, name: &str, help: &str, metric_type: &str) {
    let _ = writeln!(body, "# HELP {name} {help}");
    let _ = writeln!(body, "# TYPE {name} {metric_type}");
}

fn append_prometheus_u64(body: &mut String, name: &str, labels: &[(&str, &str)], value: u64) {
    if labels.is_empty() {
        let _ = writeln!(body, "{name} {value}");
        return;
    }

    let _ = write!(body, "{name}{{");
    for (index, (label, value)) in labels.iter().enumerate() {
        if index > 0 {
            let _ = write!(body, ",");
        }
        let _ = write!(body, "{label}=\"");
        append_escaped_label_value(body, value);
        let _ = write!(body, "\"");
    }
    let _ = writeln!(body, "}} {value}");
}

fn append_escaped_label_value(body: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '\\' => {
                let _ = write!(body, "\\\\");
            }
            '"' => {
                let _ = write!(body, "\\\"");
            }
            '\n' => {
                let _ = write!(body, "\\n");
            }
            _ => {
                let _ = write!(body, "{ch}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const MONITORING_DOCS: &str =
        include_str!("../../../crab-web/content/docs/cli/cache-service/monitoring.mdx");
    const RUNBOOK_DOCS: &str =
        include_str!("../../../crab-web/content/docs/cli/cache-service/runbooks.mdx");
    const GRAFANA_DASHBOARD: &str =
        include_str!("../../../crab/deploy/cache-service/grafana-dashboard.json");
    const KUBERNETES_SERVICE: &str =
        include_str!("../../../crab/deploy/cache-service/kubernetes/service.yaml");
    const PROMETHEUS_RULES: &str =
        include_str!("../../../crab/deploy/cache-service/kubernetes/prometheus-rules.yaml");
    const SERVICE_MONITOR: &str =
        include_str!("../../../crab/deploy/cache-service/kubernetes/service-monitor.yaml");

    #[derive(Debug, serde::Deserialize)]
    struct KubernetesServiceManifest {
        #[serde(rename = "apiVersion")]
        api_version: String,
        kind: String,
        metadata: ManifestMetadata,
        spec: KubernetesServiceSpec,
    }

    #[derive(Debug, serde::Deserialize)]
    struct ManifestMetadata {
        name: String,
        labels: BTreeMap<String, String>,
    }

    #[derive(Debug, serde::Deserialize)]
    struct KubernetesServiceSpec {
        ports: Vec<KubernetesServicePort>,
    }

    #[derive(Debug, serde::Deserialize)]
    struct KubernetesServicePort {
        name: String,
        port: u16,
    }

    #[derive(Debug, serde::Deserialize)]
    struct PrometheusRuleManifest {
        #[serde(rename = "apiVersion")]
        api_version: String,
        kind: String,
        metadata: ManifestMetadata,
        spec: PrometheusRuleSpec,
    }

    #[derive(Debug, serde::Deserialize)]
    struct ServiceMonitorManifest {
        #[serde(rename = "apiVersion")]
        api_version: String,
        kind: String,
        metadata: ManifestMetadata,
        spec: ServiceMonitorSpec,
    }

    #[derive(Debug, serde::Deserialize)]
    struct ServiceMonitorSpec {
        selector: LabelSelector,
        endpoints: Vec<ServiceMonitorEndpoint>,
    }

    #[derive(Debug, serde::Deserialize)]
    struct LabelSelector {
        #[serde(rename = "matchLabels")]
        match_labels: BTreeMap<String, String>,
    }

    #[derive(Debug, serde::Deserialize)]
    struct ServiceMonitorEndpoint {
        port: String,
        path: String,
        interval: String,
        #[serde(rename = "scrapeTimeout")]
        scrape_timeout: String,
    }

    #[derive(Debug, serde::Deserialize)]
    struct PrometheusRuleSpec {
        groups: Vec<PrometheusRuleGroup>,
    }

    #[derive(Debug, serde::Deserialize)]
    struct PrometheusRuleGroup {
        name: String,
        rules: Vec<PrometheusAlertRule>,
    }

    #[derive(Debug, serde::Deserialize)]
    struct PrometheusAlertRule {
        alert: String,
        expr: String,
        annotations: BTreeMap<String, String>,
    }

    fn parse_prometheus_rules() -> PrometheusRuleManifest {
        serde_yaml::from_str(PROMETHEUS_RULES)
            .expect("cache-service PrometheusRule manifest must parse as YAML")
    }

    fn parse_service_monitor() -> ServiceMonitorManifest {
        serde_yaml::from_str(SERVICE_MONITOR)
            .expect("cache-service ServiceMonitor manifest must parse as YAML")
    }

    fn parse_kubernetes_service() -> KubernetesServiceManifest {
        serde_yaml::from_str(KUBERNETES_SERVICE)
            .expect("cache-service Kubernetes Service manifest must parse as YAML")
    }

    fn parse_grafana_dashboard() -> serde_json::Value {
        serde_json::from_str(GRAFANA_DASHBOARD)
            .expect("cache-service Grafana dashboard must parse as JSON")
    }

    #[test]
    fn snapshot_tracks_object_type_traffic_independently() {
        let metrics = CacheMetrics::stub();

        metrics.record_cache_miss(ObjectType::Metadata);
        metrics.record_origin_fetch(ObjectType::Metadata, 10.0, 7);
        metrics.record_origin_head(ObjectType::Metadata, 4.0);
        metrics.record_bytes_served(ObjectType::Metadata, 7, false);
        metrics.record_cache_hit(ObjectType::Shard);
        metrics.record_bytes_served(ObjectType::Shard, 11, true);
        metrics.record_push_warming(ObjectType::Shard, 11);
        metrics.record_mutable_read_rejection("GET");
        metrics.record_mutable_write_rejection();
        metrics.record_mutable_proxy_get();
        metrics.record_mutable_proxy_head();
        metrics.record_mutable_proxy_bytes(13);
        metrics.record_mutable_proxy_stream_error("GET");

        let stats = metrics.snapshot();
        assert_eq!(stats.cache_misses, 1);
        assert_eq!(stats.cache_hits, 1);
        assert_eq!(stats.origin_fetches, 1);
        assert_eq!(stats.origin_head_requests, 1);
        assert_eq!(stats.by_object_type.metadata.cache_misses, 1);
        assert_eq!(stats.by_object_type.metadata.origin_fetches, 1);
        assert_eq!(stats.by_object_type.metadata.origin_head_requests, 1);
        assert_eq!(stats.by_object_type.metadata.origin_fetch_bytes, 7);
        assert_eq!(stats.by_object_type.metadata.bytes_served_from_origin, 7);
        assert_eq!(stats.by_object_type.shard.cache_hits, 1);
        assert_eq!(stats.by_object_type.shard.origin_avoided_reads, 1);
        assert_eq!(stats.by_object_type.shard.bytes_served_from_cache, 11);
        assert_eq!(stats.by_object_type.shard.push_warming_writes, 1);
        assert_eq!(stats.by_object_type.xorb.cache_hits, 0);
        assert_eq!(stats.mutable_read_rejections, 1);
        assert_eq!(stats.mutable_write_rejections, 1);
        assert_eq!(stats.mutable_proxy_reads, 2);
        assert_eq!(stats.mutable_proxy_gets, 1);
        assert_eq!(stats.mutable_proxy_heads, 1);
        assert_eq!(stats.mutable_proxy_bytes, 13);
        assert_eq!(stats.mutable_proxy_stream_errors, 1);
    }

    #[test]
    fn rendered_core_metrics_match_admin_snapshot_counters() {
        let metrics = CacheMetrics::stub();

        metrics.record_cache_miss(ObjectType::Metadata);
        metrics.record_coalesced_miss(ObjectType::Metadata);
        metrics.record_origin_fetch(ObjectType::Metadata, 10.0, 7);
        metrics.record_origin_head(ObjectType::Metadata, 4.0);
        metrics.record_bytes_served(ObjectType::Metadata, 7, false);
        metrics.record_cache_hit(ObjectType::Shard);
        metrics.record_bytes_served(ObjectType::Shard, 11, true);
        metrics.record_push_warming(ObjectType::Shard, 13);
        metrics.record_dedup_query(5.0, 3, 2);
        metrics.record_mutable_proxy_get();
        metrics.record_mutable_proxy_head();
        metrics.record_mutable_proxy_bytes(17);
        metrics.record_mutable_proxy_stream_error("GET");
        metrics.set_bytes_stored(29);
        metrics.set_inflight_misses(31);
        metrics.set_active_connections(37);

        let stats = metrics.snapshot();
        let rendered = metrics.render();

        assert_eq!(
            sum_prometheus_metric(&rendered, "cache_hit_total"),
            stats.cache_hits as f64
        );
        assert_eq!(
            sum_prometheus_metric(&rendered, "origin_avoided_reads_total"),
            stats.origin_avoided_reads as f64
        );
        assert_eq!(
            sum_prometheus_metric(&rendered, "cache_miss_total"),
            stats.cache_misses as f64
        );
        assert_eq!(
            sum_prometheus_metric(&rendered, "cache_miss_coalesced_total"),
            stats.coalesced_misses as f64
        );
        assert_eq!(
            sum_prometheus_metric(&rendered, "origin_fetch_total"),
            stats.origin_fetches as f64
        );
        assert_eq!(
            sum_prometheus_metric(&rendered, "origin_fetch_bytes"),
            stats.origin_fetch_bytes as f64
        );
        assert_eq!(
            sum_prometheus_metric(&rendered, "origin_head_total"),
            stats.origin_head_requests as f64
        );
        assert_eq!(
            sum_prometheus_metric(&rendered, "cache_bytes_served"),
            stats.bytes_served_total as f64
        );
        assert_eq!(
            sum_prometheus_metric(&rendered, "push_warming_total"),
            stats.push_warming_writes as f64
        );
        assert_eq!(
            sum_prometheus_metric(&rendered, "push_warming_bytes"),
            stats.push_warming_bytes as f64
        );
        assert_eq!(
            sum_prometheus_metric(&rendered, "dedup_query_total"),
            stats.dedup_queries as f64
        );
        assert_eq!(
            sum_prometheus_metric(&rendered, "dedup_chunks_known"),
            stats.dedup_known_chunks as f64
        );
        assert_eq!(
            sum_prometheus_metric(&rendered, "dedup_chunks_unknown"),
            stats.dedup_unknown_chunks as f64
        );
        assert_eq!(
            sum_prometheus_metric(&rendered, "mutable_path_proxy_read_total"),
            stats.mutable_proxy_reads as f64
        );
        assert_eq!(
            sum_prometheus_metric(&rendered, "mutable_path_proxy_bytes"),
            stats.mutable_proxy_bytes as f64
        );
        assert_eq!(
            sum_prometheus_metric(&rendered, "mutable_path_proxy_stream_error_total"),
            stats.mutable_proxy_stream_errors as f64
        );
        assert_eq!(sum_prometheus_metric(&rendered, "cache_bytes_stored"), 29.0);
        assert_eq!(
            sum_prometheus_metric(&rendered, "cache_inflight_misses"),
            stats.inflight_misses as f64
        );
        assert_eq!(
            sum_prometheus_metric(&rendered, "active_connections"),
            stats.active_connections as f64
        );
    }

    #[test]
    fn rendered_eviction_metrics_match_cache_store_snapshot() {
        let metrics = CacheMetrics::stub();
        let eviction_stats = CacheEvictionStats {
            total: 5,
            xorb: 2,
            shard: 1,
            pack: 0,
            pack_index: 1,
            metadata: 1,
        };

        let rendered = metrics.render_with_cache_store(100, 200, &eviction_stats);

        assert_eq!(
            sum_prometheus_metric(&rendered, "cache_eviction_total"),
            5.0
        );
        assert_eq!(sum_prometheus_metric(&rendered, "cache_max_bytes"), 100.0);
        assert_eq!(
            sum_prometheus_metric(&rendered, "cache_max_object_bytes"),
            200.0
        );
        assert!(rendered.contains(r#"cache_eviction_total{object_type="xorb"} 2"#));
        assert!(rendered.contains(r#"cache_eviction_total{object_type="pack_index"} 1"#));
        assert!(rendered.contains(r#"cache_eviction_total{object_type="metadata"} 1"#));
    }

    #[test]
    fn render_includes_integrity_repair_counter() {
        let Ok(metrics) = CacheMetrics::new() else {
            return;
        };

        record_cache_integrity_repairs("runtime", "missing_files_repaired", 1);

        let body = metrics.render();
        assert!(body.contains("cache_integrity_repair_total"));
        assert!(body.contains(r#"phase="runtime""#));
        assert!(body.contains(r#"event="missing_files_repaired""#));
    }

    #[test]
    fn monitoring_docs_include_cache_service_alert_rules() {
        let manifest = parse_prometheus_rules();
        assert_eq!(manifest.api_version, "monitoring.coreos.com/v1");
        assert_eq!(manifest.kind, "PrometheusRule");
        assert_eq!(manifest.metadata.name, "crab-cache-server");
        assert_eq!(
            manifest.metadata.labels.get("app").map(String::as_str),
            Some("crab-cache-server")
        );

        let group = manifest
            .spec
            .groups
            .iter()
            .find(|group| group.name == "crab-cache.rules")
            .expect("cache-service PrometheusRule must define crab-cache.rules");

        for (alert, runbook_anchor) in [
            (
                "CrabCacheRuntimeIntegrityRepair",
                "#crab-cache-runtime-integrity-repair",
            ),
            (
                "CrabCacheStartupIntegrityRepair",
                "#crab-cache-startup-integrity-repair",
            ),
            (
                "CrabCacheOriginFallbackHigh",
                "#crab-cache-origin-fallback-high",
            ),
            ("CrabCacheHitRateLow", "#crab-cache-hit-rate-low"),
            ("CrabCacheEvictionPressure", "#crab-cache-eviction-pressure"),
            (
                "CrabCacheMutableProxyActive",
                "#crab-cache-mutable-proxy-active",
            ),
        ] {
            let rule = group
                .rules
                .iter()
                .find(|rule| rule.alert == alert)
                .unwrap_or_else(|| panic!("PrometheusRule manifest missing alert rule {alert}"));
            assert!(
                !rule.expr.trim().is_empty(),
                "PrometheusRule manifest missing alert rule {alert}"
            );
            let expected_runbook_url =
                format!("https://crab.build/docs/cli/cache-service/runbooks{runbook_anchor}");
            assert_eq!(
                rule.annotations.get("runbook_url").map(String::as_str),
                Some(expected_runbook_url.as_str()),
                "PrometheusRule manifest missing runbook_url for {alert}"
            );
            assert!(
                MONITORING_DOCS.contains(alert),
                "monitoring docs missing alert rule {alert}"
            );
            assert!(
                RUNBOOK_DOCS.contains(alert),
                "runbook docs missing alert rule {alert}"
            );
            assert!(
                RUNBOOK_DOCS.contains(runbook_anchor.trim_start_matches('#')),
                "runbook docs missing stable anchor for {alert}"
            );
        }

        for metric in [
            "cache_integrity_repair_total",
            "cache_eviction_total",
            "cache_max_bytes",
            "origin_fetch_total",
            "cache_hit_total",
            "cache_miss_total",
            "mutable_path_proxy_read_total",
            "origin_avoided_reads_total",
        ] {
            assert!(
                PROMETHEUS_RULES.contains(metric),
                "PrometheusRule manifest missing metric {metric}"
            );
            assert!(
                MONITORING_DOCS.contains(metric),
                "monitoring docs missing metric {metric}"
            );
        }

        assert!(
            MONITORING_DOCS.contains("crab/deploy/cache-service/kubernetes/prometheus-rules.yaml")
        );
        assert!(MONITORING_DOCS.contains("runbook_url"));
        assert!(MONITORING_DOCS.contains("/docs/cli/cache-service/runbooks"));
    }

    #[test]
    fn service_monitor_scrapes_cache_service_metrics_endpoint() {
        let service = parse_kubernetes_service();
        assert_eq!(service.api_version, "v1");
        assert_eq!(service.kind, "Service");
        assert_eq!(service.metadata.name, "crab-cache-server");

        let monitor = parse_service_monitor();
        assert_eq!(monitor.api_version, "monitoring.coreos.com/v1");
        assert_eq!(monitor.kind, "ServiceMonitor");
        assert_eq!(monitor.metadata.name, service.metadata.name);
        assert_eq!(
            monitor.metadata.labels.get("app").map(String::as_str),
            Some("crab-cache-server")
        );
        assert_eq!(
            monitor.spec.selector.match_labels.get("app"),
            service.metadata.labels.get("app")
        );

        let endpoint = monitor
            .spec
            .endpoints
            .first()
            .expect("cache-service ServiceMonitor must define an endpoint");
        let service_port = service
            .spec
            .ports
            .iter()
            .find(|port| port.name == endpoint.port)
            .expect("ServiceMonitor endpoint port must match a Service port name");

        assert_eq!(service_port.port, 8443);
        assert_eq!(endpoint.path, "/v1/metrics");
        assert_eq!(endpoint.interval, "30s");
        assert_eq!(endpoint.scrape_timeout, "10s");
        assert!(MONITORING_DOCS.contains("/v1/metrics"));
        assert!(
            MONITORING_DOCS.contains("crab/deploy/cache-service/kubernetes/service-monitor.yaml")
        );
    }

    #[test]
    fn grafana_dashboard_queries_exported_cache_service_metrics() {
        let dashboard = parse_grafana_dashboard();
        assert_eq!(dashboard["uid"], "crab-cache-service");
        assert_eq!(dashboard["title"], "Crab Cache Service");

        let variables = dashboard["templating"]["list"]
            .as_array()
            .expect("dashboard templating list must be an array");
        assert!(variables.iter().any(|variable| {
            variable["name"] == "DS_PROMETHEUS" && variable["type"] == "datasource"
        }));

        let panels = dashboard["panels"]
            .as_array()
            .expect("dashboard panels must be an array");
        assert!(
            panels.len() >= 8,
            "dashboard should cover core cache-service signals"
        );

        let exported_metrics = [
            "active_connections",
            "cache_bytes_served",
            "cache_bytes_stored",
            "cache_eviction_total",
            "cache_hit_total",
            "cache_inflight_misses",
            "cache_integrity_repair_total",
            "cache_max_bytes",
            "cache_max_object_bytes",
            "cache_miss_total",
            "dedup_chunks_known",
            "dedup_chunks_unknown",
            "dedup_query_total",
            "mutable_path_proxy_read_total",
            "mutable_path_proxy_stream_error_total",
            "origin_avoided_reads_total",
            "origin_fetch_bytes",
            "origin_fetch_total",
            "push_warming_total",
        ];

        let mut expressions = Vec::new();
        for panel in panels {
            assert!(
                panel["title"]
                    .as_str()
                    .is_some_and(|title| !title.is_empty()),
                "dashboard panel is missing a title"
            );
            let targets = panel["targets"]
                .as_array()
                .expect("dashboard panel targets must be an array");
            assert!(
                !targets.is_empty(),
                "dashboard panel must include at least one query target"
            );
            for target in targets {
                let expr = target["expr"]
                    .as_str()
                    .expect("dashboard target must define a PromQL expression");
                assert!(
                    exported_metrics.iter().any(|metric| expr.contains(metric)),
                    "dashboard query references no exported cache-service metric: {expr}"
                );
                expressions.push(expr);
            }
        }

        let dashboard_queries = expressions.join("\n");
        for metric in exported_metrics {
            assert!(
                dashboard_queries.contains(metric),
                "dashboard missing exported metric {metric}"
            );
            assert!(
                MONITORING_DOCS.contains(metric),
                "monitoring docs missing dashboard metric {metric}"
            );
        }
        assert!(MONITORING_DOCS.contains("crab/deploy/cache-service/grafana-dashboard.json"));
    }

    fn sum_prometheus_metric(body: &str, name: &str) -> f64 {
        body.lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    return None;
                }
                let mut fields = line.split_whitespace();
                let series = fields.next()?;
                let metric = series.split_once('{').map_or(series, |(metric, _)| metric);
                if metric != name {
                    return None;
                }
                fields.next()?.parse::<f64>().ok()
            })
            .sum()
    }
}

/// Log a per-request summary line with structured fields.
///
/// Called from middleware or handler wrappers to satisfy C9.3: every request
/// gets a single summary line with method, path, status, latency, hit/miss,
/// and bytes served.
pub fn log_request_summary(
    method: &str,
    path: &str,
    status: u16,
    latency_ms: f64,
    cache_result: &str,
    bytes_served: u64,
) {
    tracing::info!(
        http.method = method,
        http.path = path,
        http.status = status,
        latency_ms = latency_ms,
        cache = cache_result,
        bytes = bytes_served,
        "request complete"
    );
}
