use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

use axum::{
    body::Body,
    http::{Method, StatusCode},
};
use bytes::Bytes;
use http_body::{Body as _, Frame, SizeHint};
use metrics::{Counter, Gauge, Histogram, Key, KeyName, Label, Level, Metadata, Recorder, Unit};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

const METHOD_COUNT: usize = 6;
const OUTCOME_COUNT: usize = 7;
const ADMISSION_COUNT: usize = 4;
const TRANSFER_REJECTION_COUNT: usize = 2;
const DURABILITY_SOURCE_COUNT: usize = 2;
const APPEND_RESULT_COUNT: usize = 2;
const NODE_LOG_LANE_STATE_COUNT: usize = 3;
const SELF_FENCE_REASON_COUNT: usize = 4;
const RECOVERY_STATE_COUNT: usize = 2;
const RECOVERY_FAILURE_REASON_COUNT: usize = 4;
const NODE_LOG_ROTATION_RESULT_COUNT: usize = 4;
const LTX_PHASE_COUNT: usize = 18;
const LTX_READ_ORIGIN_COUNT: usize = 4;
const PROJECTION_PROBE_RESULT_COUNT: usize = 3;
const PROJECTION_PHASE_COUNT: usize = 1;
const PROJECTION_BUILD_RESULT_COUNT: usize = 3;
const PROJECTION_BATCH_KIND_COUNT: usize = 4;
const PROJECTION_ORIGIN_READ_KIND_COUNT: usize = 3;
const DURATION_BUCKETS_SECONDS: [f64; 16] = [
    0.005, 0.010, 0.025, 0.050, 0.100, 0.250, 0.500, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
    600.0,
];
const METHOD_LABELS: [&str; METHOD_COUNT] = ["get", "head", "put", "post", "delete", "other"];
const OUTCOME_LABELS: [&str; OUTCOME_COUNT] =
    ["1xx", "2xx", "3xx", "4xx", "5xx", "other", "cancelled"];
pub(crate) const ADMISSION_LABELS: [&str; ADMISSION_COUNT] =
    ["read", "git_transfer", "application", "maintenance"];
const TRANSFER_REJECTION_LABELS: [&str; TRANSFER_REJECTION_COUNT] = ["capacity", "coordination"];
const DURABILITY_SOURCE_LABELS: [&str; DURABILITY_SOURCE_COUNT] = ["fleet", "object"];
const APPEND_RESULT_LABELS: [&str; APPEND_RESULT_COUNT] = ["acked", "nacked"];
const RESIDENT_ROUTE_LABELS: [&str; 3] = ["hit", "miss", "refused"];
const LTX_PHASE_LABELS: [&str; LTX_PHASE_COUNT] = [
    "capture",
    "preparation",
    "schema_check",
    "wal_existence",
    "position_resolution",
    "wal_read",
    "page_collection",
    "verification",
    "encode",
    "local_write",
    "fsync",
    "parent_sync",
    "checkpoint",
    "root_open",
    "directory",
    "frame_fetch",
    "restore_write",
    "compaction",
];
const LTX_PHASE_RESULT_LABELS: [&str; 2] = ["succeeded", "failed"];
const LTX_READ_ORIGIN_LABELS: [&str; LTX_READ_ORIGIN_COUNT] =
    ["cold", "sparse", "hydrating", "resident"];
const LTX_WAL_READ_LABELS: [&str; 3] = ["sparse", "full", "fallback"];
const NODE_LOG_LANE_STATE_LABELS: [&str; NODE_LOG_LANE_STATE_COUNT] =
    ["open", "degraded", "sealed"];
const SELF_FENCE_REASON_LABELS: [&str; SELF_FENCE_REASON_COUNT] =
    ["expiry", "refresh", "shutdown", "other"];
const RECOVERY_STATE_LABELS: [&str; RECOVERY_STATE_COUNT] = ["running", "waiting"];
const RECOVERY_FAILURE_REASON_LABELS: [&str; RECOVERY_FAILURE_REASON_COUNT] =
    ["storage", "capacity", "fenced", "other"];
const NODE_LOG_ROTATION_RESULT_LABELS: [&str; NODE_LOG_ROTATION_RESULT_COUNT] =
    ["started", "pending", "failed", "completed"];
const PROJECTION_PROBE_RESULT_LABELS: [&str; PROJECTION_PROBE_RESULT_COUNT] =
    ["ready", "changed", "error"];
const PROJECTION_PHASE_LABELS: [&str; PROJECTION_PHASE_COUNT] = ["reconcile"];
const PROJECTION_BUILD_RESULT_LABELS: [&str; PROJECTION_BUILD_RESULT_COUNT] =
    ["ok", "error", "superseded"];
const PROJECTION_BATCH_KIND_LABELS: [&str; PROJECTION_BATCH_KIND_COUNT] =
    ["refs", "commits", "trees", "attribution"];
const PROJECTION_ORIGIN_READ_KIND_LABELS: [&str; PROJECTION_ORIGIN_READ_KIND_COUNT] =
    ["snapshot", "graph", "tree"];
const METADATA: Metadata<'static> = Metadata::new(
    "crab_http_server",
    Level::INFO,
    Some("crab_http_server::metrics"),
);

#[derive(Clone)]
pub(crate) struct Metrics {
    inner: Arc<MetricsInner>,
}

struct MetricsInner {
    handle: PrometheusHandle,
    methods: [MethodMetrics; METHOD_COUNT],
    admission: [AdmissionMetrics; ADMISSION_COUNT],
    repositories: Gauge,
    catalog_healthy: Gauge,
    scheduler_healthy: Gauge,
    scheduler_progress: Gauge,
    scheduler_lag_seconds: Gauge,
    draining: Gauge,
    receive_workers: Gauge,
    cell_active: Gauge,
    cell_active_capacity: Gauge,
    cell_resident_bytes: Gauge,
    cell_resident_capacity_bytes: Gauge,
    cell_file_descriptors: Gauge,
    cell_file_descriptor_capacity: Gauge,
    cell_retained_bytes: Gauge,
    cell_retained_capacity_bytes: Gauge,
    cell_worker_jobs: Gauge,
    cell_worker_job_capacity: Gauge,
    cell_primitive_jobs: Gauge,
    cell_primitive_job_capacity: Gauge,
    cell_hydration_jobs: Gauge,
    cell_hydration_job_capacity: Gauge,
    cell_io_slots: Gauge,
    cell_io_slot_capacity: Gauge,
    cell_blocking_jobs: Gauge,
    cell_blocking_job_capacity: Gauge,
    cell_recovery_jobs: Gauge,
    cell_recovery_job_capacity: Gauge,
    cell_dirty_jobs: Gauge,
    cell_dirty_job_capacity: Gauge,
    cell_scratch_units: Gauge,
    cell_scratch_unit_capacity: Gauge,
    cell_local_disk_reserved_bytes: Gauge,
    cell_local_disk_capacity_bytes: Gauge,
    cell_node_log_uncovered_bytes: Gauge,
    cell_follower_retained_bytes: Gauge,
    durability_proofs: [Counter; DURABILITY_SOURCE_COUNT],
    durability_wait: [Histogram; DURABILITY_SOURCE_COUNT],
    node_log_append_bytes: [Counter; APPEND_RESULT_COUNT],
    resident_routes: [Counter; 3],
    ltx_phase_runs: [[Counter; 2]; LTX_PHASE_COUNT],
    ltx_phase_duration: [Histogram; LTX_PHASE_COUNT],
    ltx_logical_reads: [Counter; LTX_READ_ORIGIN_COUNT],
    ltx_origin_requests: [[Counter; 2]; LTX_READ_ORIGIN_COUNT],
    ltx_origin_bytes: [Counter; LTX_READ_ORIGIN_COUNT],
    ltx_wal_reads: [Counter; 3],
    ltx_wal_image_bytes: Counter,
    ltx_wal_file_bytes: Counter,
    ltx_wal_read_bytes: Counter,
    ltx_wal_snapshot_reads: Counter,
    ltx_capture_wal_bytes: Counter,
    ltx_capture_database_bytes: Counter,
    ltx_capture_ltx_bytes: Counter,
    ltx_capture_segments: Counter,
    ltx_checkpoint_runs: Counter,
    ltx_checkpoint_busy: Counter,
    ltx_checkpoint_busy_errors: Counter,
    ltx_checkpoint_frames: Counter,
    ltx_checkpoint_backfilled: Counter,
    ltx_checkpoint_restarts: Counter,
    node_log_lanes: [Gauge; NODE_LOG_LANE_STATE_COUNT],
    session_lease_seconds: Gauge,
    self_fenced: AtomicBool,
    self_fences: [Counter; SELF_FENCE_REASON_COUNT],
    node_log_recoveries: [Gauge; RECOVERY_STATE_COUNT],
    node_log_recovery_seconds: Histogram,
    node_log_recovery_failures: [Counter; RECOVERY_FAILURE_REASON_COUNT],
    node_log_rotations: [Counter; NODE_LOG_ROTATION_RESULT_COUNT],
    catalog_refresh_failures: Counter,
    transfer_admission_rejections: [Counter; TRANSFER_REJECTION_COUNT],
    projection_probes: [Counter; PROJECTION_PROBE_RESULT_COUNT],
    projection_probe_seconds: [Histogram; PROJECTION_PROBE_RESULT_COUNT],
    projection_build_seconds: [[Histogram; PROJECTION_BUILD_RESULT_COUNT]; PROJECTION_PHASE_COUNT],
    projection_batch_rows: [Counter; PROJECTION_BATCH_KIND_COUNT],
    projection_batch_bytes: [Counter; PROJECTION_BATCH_KIND_COUNT],
    projection_superseded: Counter,
    projection_ready: Gauge,
    projection_lag_generations: Gauge,
    projection_lag_seconds: Gauge,
    projection_sqlite_bytes: Gauge,
    projection_origin_reads: [Counter; PROJECTION_ORIGIN_READ_KIND_COUNT],
}

struct MethodMetrics {
    requests: [Counter; OUTCOME_COUNT],
    in_flight: Gauge,
    duration: Histogram,
    body_errors: Counter,
    body_aborts: Counter,
}

struct AdmissionMetrics {
    available: Gauge,
    capacity: Gauge,
}

#[derive(Default)]
pub(crate) struct RuntimeSnapshot {
    pub(crate) repositories: usize,
    pub(crate) catalog_healthy: bool,
    pub(crate) scheduler_healthy: bool,
    pub(crate) scheduler_progress: u64,
    pub(crate) scheduler_lag_seconds: f64,
    pub(crate) draining: bool,
    pub(crate) receive_workers: usize,
    pub(crate) cell_active: usize,
    pub(crate) cell_active_capacity: usize,
    pub(crate) cell_resident_bytes: usize,
    pub(crate) cell_resident_capacity_bytes: usize,
    pub(crate) cell_file_descriptors: usize,
    pub(crate) cell_file_descriptor_capacity: usize,
    pub(crate) cell_retained_bytes: usize,
    pub(crate) cell_retained_capacity_bytes: usize,
    pub(crate) cell_worker_jobs: usize,
    pub(crate) cell_worker_job_capacity: usize,
    pub(crate) cell_primitive_jobs: usize,
    pub(crate) cell_primitive_job_capacity: usize,
    pub(crate) cell_hydration_jobs: usize,
    pub(crate) cell_hydration_job_capacity: usize,
    pub(crate) cell_io_slots: usize,
    pub(crate) cell_io_slot_capacity: usize,
    pub(crate) cell_blocking_jobs: usize,
    pub(crate) cell_blocking_job_capacity: usize,
    pub(crate) cell_recovery_jobs: usize,
    pub(crate) cell_recovery_job_capacity: usize,
    pub(crate) cell_dirty_jobs: usize,
    pub(crate) cell_dirty_job_capacity: usize,
    pub(crate) cell_scratch_units: usize,
    pub(crate) cell_scratch_unit_capacity: usize,
    pub(crate) cell_local_disk_reserved_bytes: u64,
    pub(crate) cell_local_disk_capacity_bytes: u64,
    pub(crate) cell_node_log_uncovered_bytes: u64,
    pub(crate) cell_follower_retained_bytes: u64,
    pub(crate) admission_available: [usize; ADMISSION_COUNT],
    pub(crate) admission_capacity: [usize; ADMISSION_COUNT],
}

impl RuntimeSnapshot {
    pub(crate) fn with_cell_runtime(
        mut self,
        runtime: crab_cell_runtime::CellRuntimeStats,
    ) -> Self {
        self.cell_active = runtime.active_cells();
        self.cell_active_capacity = runtime.active_cell_capacity();
        self.cell_resident_bytes = runtime.resident_bytes();
        self.cell_resident_capacity_bytes = runtime.resident_capacity_bytes();
        self.cell_file_descriptors = runtime.file_descriptors();
        self.cell_file_descriptor_capacity = runtime.file_descriptor_capacity();
        self.cell_retained_bytes = runtime.retained_bytes();
        self.cell_retained_capacity_bytes = runtime.retained_capacity_bytes();
        self.cell_worker_jobs = runtime.worker_jobs();
        self.cell_worker_job_capacity = runtime.worker_job_capacity();
        self.cell_primitive_jobs = runtime.primitive_jobs();
        self.cell_primitive_job_capacity = runtime.primitive_job_capacity();
        self.cell_hydration_jobs = runtime.hydration_jobs();
        self.cell_hydration_job_capacity = runtime.hydration_job_capacity();
        self.cell_io_slots = runtime.io_slots();
        self.cell_io_slot_capacity = runtime.io_slot_capacity();
        self.cell_blocking_jobs = runtime.blocking_jobs();
        self.cell_blocking_job_capacity = runtime.blocking_job_capacity();
        self.cell_recovery_jobs = runtime.recovery_jobs();
        self.cell_recovery_job_capacity = runtime.recovery_job_capacity();
        self.cell_dirty_jobs = runtime.dirty_jobs();
        self.cell_dirty_job_capacity = runtime.dirty_job_capacity();
        self.cell_scratch_units = runtime.scratch_units();
        self.cell_scratch_unit_capacity = runtime.scratch_unit_capacity();
        self.cell_local_disk_reserved_bytes = runtime.local_disk_reserved_bytes();
        self.cell_local_disk_capacity_bytes = runtime.local_disk_capacity_bytes();
        self.cell_node_log_uncovered_bytes = runtime.unpublished_node_log_bytes();
        self
    }
}

impl Metrics {
    pub(crate) fn new() -> Result<Self, metrics_exporter_prometheus::BuildError> {
        let recorder = PrometheusBuilder::new()
            .set_buckets(&DURATION_BUCKETS_SECONDS)?
            .build_recorder();
        describe_metrics(&recorder);
        let methods = METHOD_LABELS.map(|method| MethodMetrics::new(&recorder, method));
        let admission = ADMISSION_LABELS.map(|class| AdmissionMetrics::new(&recorder, class));
        Ok(Self {
            inner: Arc::new(MetricsInner {
                handle: recorder.handle(),
                methods,
                admission,
                repositories: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_repositories"),
                    &METADATA,
                ),
                catalog_healthy: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_catalog_healthy"),
                    &METADATA,
                ),
                scheduler_healthy: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_scheduler_healthy"),
                    &METADATA,
                ),
                scheduler_progress: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_scheduler_progress"),
                    &METADATA,
                ),
                scheduler_lag_seconds: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_scheduler_lag_seconds"),
                    &METADATA,
                ),
                draining: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_draining"),
                    &METADATA,
                ),
                receive_workers: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_receive_workers"),
                    &METADATA,
                ),
                cell_active: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_cell_runtime_active_cells"),
                    &METADATA,
                ),
                cell_active_capacity: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_cell_runtime_active_cell_capacity"),
                    &METADATA,
                ),
                cell_resident_bytes: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_cell_runtime_resident_bytes"),
                    &METADATA,
                ),
                cell_resident_capacity_bytes: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_cell_runtime_resident_capacity_bytes"),
                    &METADATA,
                ),
                cell_file_descriptors: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_cell_runtime_file_descriptors"),
                    &METADATA,
                ),
                cell_file_descriptor_capacity: recorder.register_gauge(
                    &Key::from_static_name(
                        "crab_http_server_cell_runtime_file_descriptor_capacity",
                    ),
                    &METADATA,
                ),
                cell_retained_bytes: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_cell_runtime_retained_bytes"),
                    &METADATA,
                ),
                cell_retained_capacity_bytes: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_cell_runtime_retained_capacity_bytes"),
                    &METADATA,
                ),
                cell_worker_jobs: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_cell_runtime_worker_jobs"),
                    &METADATA,
                ),
                cell_worker_job_capacity: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_cell_runtime_worker_job_capacity"),
                    &METADATA,
                ),
                cell_primitive_jobs: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_cell_runtime_primitive_jobs"),
                    &METADATA,
                ),
                cell_primitive_job_capacity: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_cell_runtime_primitive_job_capacity"),
                    &METADATA,
                ),
                cell_hydration_jobs: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_cell_runtime_hydration_jobs"),
                    &METADATA,
                ),
                cell_hydration_job_capacity: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_cell_runtime_hydration_job_capacity"),
                    &METADATA,
                ),
                cell_io_slots: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_cell_runtime_io_slots"),
                    &METADATA,
                ),
                cell_io_slot_capacity: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_cell_runtime_io_slot_capacity"),
                    &METADATA,
                ),
                cell_blocking_jobs: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_cell_runtime_blocking_jobs"),
                    &METADATA,
                ),
                cell_blocking_job_capacity: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_cell_runtime_blocking_job_capacity"),
                    &METADATA,
                ),
                cell_recovery_jobs: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_cell_runtime_recovery_jobs"),
                    &METADATA,
                ),
                cell_recovery_job_capacity: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_cell_runtime_recovery_job_capacity"),
                    &METADATA,
                ),
                cell_dirty_jobs: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_cell_runtime_dirty_jobs"),
                    &METADATA,
                ),
                cell_dirty_job_capacity: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_cell_runtime_dirty_job_capacity"),
                    &METADATA,
                ),
                cell_scratch_units: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_cell_runtime_scratch_units"),
                    &METADATA,
                ),
                cell_scratch_unit_capacity: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_cell_runtime_scratch_unit_capacity"),
                    &METADATA,
                ),
                cell_local_disk_reserved_bytes: recorder.register_gauge(
                    &Key::from_static_name(
                        "crab_http_server_cell_runtime_local_disk_reserved_bytes",
                    ),
                    &METADATA,
                ),
                cell_local_disk_capacity_bytes: recorder.register_gauge(
                    &Key::from_static_name(
                        "crab_http_server_cell_runtime_local_disk_capacity_bytes",
                    ),
                    &METADATA,
                ),
                cell_node_log_uncovered_bytes: recorder.register_gauge(
                    &Key::from_static_name("crab_cell_node_log_uncovered_bytes"),
                    &METADATA,
                ),
                cell_follower_retained_bytes: recorder.register_gauge(
                    &Key::from_static_name("crab_cell_follower_retained_bytes"),
                    &METADATA,
                ),
                durability_proofs: DURABILITY_SOURCE_LABELS.map(|source| {
                    recorder.register_counter(
                        &key("crab_cell_durability_proofs_total", &[("source", source)]),
                        &METADATA,
                    )
                }),
                durability_wait: DURABILITY_SOURCE_LABELS.map(|source| {
                    recorder.register_histogram(
                        &key("crab_cell_durability_wait_seconds", &[("source", source)]),
                        &METADATA,
                    )
                }),
                node_log_append_bytes: APPEND_RESULT_LABELS.map(|result| {
                    recorder.register_counter(
                        &key(
                            "crab_cell_node_log_append_bytes_total",
                            &[("result", result)],
                        ),
                        &METADATA,
                    )
                }),
                resident_routes: RESIDENT_ROUTE_LABELS.map(|outcome| {
                    recorder.register_counter(
                        &key("crab_cell_resident_route_total", &[("outcome", outcome)]),
                        &METADATA,
                    )
                }),
                ltx_phase_runs: LTX_PHASE_LABELS.map(|phase| {
                    LTX_PHASE_RESULT_LABELS.map(|result| {
                        recorder.register_counter(
                            &key(
                                "crab_cell_ltx_phase_total",
                                &[("phase", phase), ("result", result)],
                            ),
                            &METADATA,
                        )
                    })
                }),
                ltx_phase_duration: LTX_PHASE_LABELS.map(|phase| {
                    recorder.register_histogram(
                        &key("crab_cell_ltx_phase_seconds", &[("phase", phase)]),
                        &METADATA,
                    )
                }),
                ltx_logical_reads: LTX_READ_ORIGIN_LABELS.map(|origin| {
                    recorder.register_counter(
                        &key("crab_cell_ltx_logical_reads_total", &[("origin", origin)]),
                        &METADATA,
                    )
                }),
                ltx_origin_requests: LTX_READ_ORIGIN_LABELS.map(|origin| {
                    LTX_PHASE_RESULT_LABELS.map(|result| {
                        recorder.register_counter(
                            &key(
                                "crab_cell_ltx_origin_requests_total",
                                &[("origin", origin), ("result", result)],
                            ),
                            &METADATA,
                        )
                    })
                }),
                ltx_origin_bytes: LTX_READ_ORIGIN_LABELS.map(|origin| {
                    recorder.register_counter(
                        &key("crab_cell_ltx_origin_bytes_total", &[("origin", origin)]),
                        &METADATA,
                    )
                }),
                ltx_wal_reads: LTX_WAL_READ_LABELS.map(|strategy| {
                    recorder.register_counter(
                        &key("crab_cell_ltx_wal_reads_total", &[("strategy", strategy)]),
                        &METADATA,
                    )
                }),
                ltx_wal_image_bytes: recorder.register_counter(
                    &Key::from_static_name("crab_cell_ltx_wal_image_bytes_total"),
                    &METADATA,
                ),
                ltx_wal_file_bytes: recorder.register_counter(
                    &Key::from_static_name("crab_cell_ltx_wal_file_bytes_total"),
                    &METADATA,
                ),
                ltx_wal_read_bytes: recorder.register_counter(
                    &Key::from_static_name("crab_cell_ltx_wal_read_bytes_total"),
                    &METADATA,
                ),
                ltx_wal_snapshot_reads: recorder.register_counter(
                    &Key::from_static_name("crab_cell_ltx_wal_snapshot_reads_total"),
                    &METADATA,
                ),
                ltx_capture_wal_bytes: recorder.register_counter(
                    &Key::from_static_name("crab_cell_ltx_capture_wal_bytes_total"),
                    &METADATA,
                ),
                ltx_capture_database_bytes: recorder.register_counter(
                    &Key::from_static_name("crab_cell_ltx_capture_database_bytes_total"),
                    &METADATA,
                ),
                ltx_capture_ltx_bytes: recorder.register_counter(
                    &Key::from_static_name("crab_cell_ltx_capture_ltx_bytes_total"),
                    &METADATA,
                ),
                ltx_capture_segments: recorder.register_counter(
                    &Key::from_static_name("crab_cell_ltx_capture_segments_total"),
                    &METADATA,
                ),
                ltx_checkpoint_runs: recorder.register_counter(
                    &Key::from_static_name("crab_cell_ltx_checkpoint_runs_total"),
                    &METADATA,
                ),
                ltx_checkpoint_busy: recorder.register_counter(
                    &Key::from_static_name("crab_cell_ltx_checkpoint_busy_total"),
                    &METADATA,
                ),
                ltx_checkpoint_busy_errors: recorder.register_counter(
                    &Key::from_static_name("crab_cell_ltx_checkpoint_busy_errors_total"),
                    &METADATA,
                ),
                ltx_checkpoint_frames: recorder.register_counter(
                    &Key::from_static_name("crab_cell_ltx_checkpoint_frames_total"),
                    &METADATA,
                ),
                ltx_checkpoint_backfilled: recorder.register_counter(
                    &Key::from_static_name("crab_cell_ltx_checkpoint_backfilled_total"),
                    &METADATA,
                ),
                ltx_checkpoint_restarts: recorder.register_counter(
                    &Key::from_static_name("crab_cell_ltx_checkpoint_restarts_total"),
                    &METADATA,
                ),
                node_log_lanes: NODE_LOG_LANE_STATE_LABELS.map(|state| {
                    recorder.register_gauge(
                        &key("crab_cell_node_log_lanes", &[("state", state)]),
                        &METADATA,
                    )
                }),
                session_lease_seconds: recorder.register_gauge(
                    &Key::from_static_name("crab_cell_session_lease_seconds"),
                    &METADATA,
                ),
                self_fenced: AtomicBool::new(false),
                self_fences: SELF_FENCE_REASON_LABELS.map(|reason| {
                    recorder.register_counter(
                        &key("crab_cell_self_fences_total", &[("reason", reason)]),
                        &METADATA,
                    )
                }),
                node_log_recoveries: RECOVERY_STATE_LABELS.map(|state| {
                    recorder.register_gauge(
                        &key("crab_cell_node_log_recoveries", &[("state", state)]),
                        &METADATA,
                    )
                }),
                node_log_recovery_seconds: recorder.register_histogram(
                    &Key::from_static_name("crab_cell_node_log_recovery_seconds"),
                    &METADATA,
                ),
                node_log_recovery_failures: RECOVERY_FAILURE_REASON_LABELS.map(|reason| {
                    recorder.register_counter(
                        &key(
                            "crab_cell_node_log_recovery_failures_total",
                            &[("reason", reason)],
                        ),
                        &METADATA,
                    )
                }),
                node_log_rotations: NODE_LOG_ROTATION_RESULT_LABELS.map(|result| {
                    recorder.register_counter(
                        &key("crab_cell_node_log_rotations_total", &[("result", result)]),
                        &METADATA,
                    )
                }),
                catalog_refresh_failures: recorder.register_counter(
                    &Key::from_static_name("crab_http_server_catalog_refresh_failures_total"),
                    &METADATA,
                ),
                transfer_admission_rejections: TRANSFER_REJECTION_LABELS.map(|reason| {
                    recorder.register_counter(
                        &key(
                            "crab_http_server_transfer_admission_rejections_total",
                            &[("reason", reason)],
                        ),
                        &METADATA,
                    )
                }),
                projection_probes: PROJECTION_PROBE_RESULT_LABELS.map(|result| {
                    recorder.register_counter(
                        &key("crab_git_projection_probe_total", &[("result", result)]),
                        &METADATA,
                    )
                }),
                projection_probe_seconds: PROJECTION_PROBE_RESULT_LABELS.map(|result| {
                    recorder.register_histogram(
                        &key("crab_git_projection_probe_seconds", &[("result", result)]),
                        &METADATA,
                    )
                }),
                projection_build_seconds: std::array::from_fn(|phase| {
                    std::array::from_fn(|result| {
                        recorder.register_histogram(
                            &key(
                                "crab_git_projection_build_seconds",
                                &[
                                    ("phase", PROJECTION_PHASE_LABELS[phase]),
                                    ("result", PROJECTION_BUILD_RESULT_LABELS[result]),
                                ],
                            ),
                            &METADATA,
                        )
                    })
                }),
                projection_batch_rows: PROJECTION_BATCH_KIND_LABELS.map(|kind| {
                    recorder.register_counter(
                        &key("crab_git_projection_batch_rows_total", &[("kind", kind)]),
                        &METADATA,
                    )
                }),
                projection_batch_bytes: PROJECTION_BATCH_KIND_LABELS.map(|kind| {
                    recorder.register_counter(
                        &key("crab_git_projection_batch_bytes_total", &[("kind", kind)]),
                        &METADATA,
                    )
                }),
                projection_superseded: recorder.register_counter(
                    &Key::from_static_name("crab_git_projection_superseded_total"),
                    &METADATA,
                ),
                projection_ready: recorder.register_gauge(
                    &Key::from_static_name("crab_git_projection_ready"),
                    &METADATA,
                ),
                projection_lag_generations: recorder.register_gauge(
                    &Key::from_static_name("crab_git_projection_lag_generations"),
                    &METADATA,
                ),
                projection_lag_seconds: recorder.register_gauge(
                    &Key::from_static_name("crab_git_projection_lag_seconds"),
                    &METADATA,
                ),
                projection_sqlite_bytes: recorder.register_gauge(
                    &Key::from_static_name("crab_git_projection_sqlite_bytes"),
                    &METADATA,
                ),
                projection_origin_reads: PROJECTION_ORIGIN_READ_KIND_LABELS.map(|kind| {
                    recorder.register_counter(
                        &key("crab_git_projection_origin_reads_total", &[("kind", kind)]),
                        &METADATA,
                    )
                }),
            }),
        })
    }

    pub(crate) fn start_request(&self, method: &Method) -> RequestObservation {
        let method = method_index(method);
        self.inner.methods[method].in_flight.increment(1);
        RequestObservation {
            metrics: self.clone(),
            method,
            started: Instant::now(),
            outcome_recorded: false,
            finished: false,
        }
    }

    pub(crate) fn record_catalog_refresh_failure(&self) {
        self.inner.catalog_refresh_failures.increment(1);
    }

    pub(crate) fn record_transfer_admission_rejection(&self, coordination: bool) {
        self.inner.transfer_admission_rejections[usize::from(coordination)].increment(1);
    }

    pub(crate) fn record_projection_probe(&self, result: ProjectionProbeResult, elapsed: Duration) {
        self.inner.projection_probes[result as usize].increment(1);
        self.inner.projection_probe_seconds[result as usize].record(elapsed.as_secs_f64());
    }

    pub(crate) fn record_projection_build(
        &self,
        phase: ProjectionPhase,
        result: ProjectionBuildResult,
        elapsed: Duration,
    ) {
        self.inner.projection_build_seconds[phase as usize][result as usize]
            .record(elapsed.as_secs_f64());
    }

    pub(crate) fn record_projection_batch(&self, kind: ProjectionBatchKind, rows: u64, bytes: u64) {
        let index = kind as usize;
        self.inner.projection_batch_rows[index].increment(rows);
        self.inner.projection_batch_bytes[index].increment(bytes);
    }

    pub(crate) fn record_projection_superseded(&self) {
        self.inner.projection_superseded.increment(1);
    }

    pub(crate) fn update_projection_state(
        &self,
        ready: bool,
        lag_generations: u64,
        lag_seconds: f64,
        sqlite_bytes: u64,
    ) {
        self.inner.projection_ready.set(f64::from(ready));
        self.inner
            .projection_lag_generations
            .set(lag_generations as f64);
        self.inner.projection_lag_seconds.set(lag_seconds);
        self.inner.projection_sqlite_bytes.set(sqlite_bytes as f64);
    }

    pub(crate) fn record_projection_origin_read(&self, kind: ProjectionOriginReadKind) {
        self.inner.projection_origin_reads[kind as usize].increment(1);
    }

    pub(crate) fn render(&self, snapshot: RuntimeSnapshot) -> String {
        self.inner.repositories.set(snapshot.repositories as f64);
        self.inner
            .catalog_healthy
            .set(f64::from(snapshot.catalog_healthy));
        self.inner
            .scheduler_healthy
            .set(f64::from(snapshot.scheduler_healthy));
        self.inner
            .scheduler_progress
            .set(snapshot.scheduler_progress as f64);
        self.inner
            .scheduler_lag_seconds
            .set(snapshot.scheduler_lag_seconds);
        self.inner.draining.set(f64::from(snapshot.draining));
        self.inner
            .receive_workers
            .set(snapshot.receive_workers as f64);
        self.inner.cell_active.set(snapshot.cell_active as f64);
        self.inner
            .cell_active_capacity
            .set(snapshot.cell_active_capacity as f64);
        self.inner
            .cell_resident_bytes
            .set(snapshot.cell_resident_bytes as f64);
        self.inner
            .cell_resident_capacity_bytes
            .set(snapshot.cell_resident_capacity_bytes as f64);
        self.inner
            .cell_file_descriptors
            .set(snapshot.cell_file_descriptors as f64);
        self.inner
            .cell_file_descriptor_capacity
            .set(snapshot.cell_file_descriptor_capacity as f64);
        self.inner
            .cell_retained_bytes
            .set(snapshot.cell_retained_bytes as f64);
        self.inner
            .cell_retained_capacity_bytes
            .set(snapshot.cell_retained_capacity_bytes as f64);
        self.inner
            .cell_worker_jobs
            .set(snapshot.cell_worker_jobs as f64);
        self.inner
            .cell_worker_job_capacity
            .set(snapshot.cell_worker_job_capacity as f64);
        self.inner
            .cell_primitive_jobs
            .set(snapshot.cell_primitive_jobs as f64);
        self.inner
            .cell_primitive_job_capacity
            .set(snapshot.cell_primitive_job_capacity as f64);
        self.inner
            .cell_hydration_jobs
            .set(snapshot.cell_hydration_jobs as f64);
        self.inner
            .cell_hydration_job_capacity
            .set(snapshot.cell_hydration_job_capacity as f64);
        self.inner.cell_io_slots.set(snapshot.cell_io_slots as f64);
        self.inner
            .cell_io_slot_capacity
            .set(snapshot.cell_io_slot_capacity as f64);
        self.inner
            .cell_blocking_jobs
            .set(snapshot.cell_blocking_jobs as f64);
        self.inner
            .cell_blocking_job_capacity
            .set(snapshot.cell_blocking_job_capacity as f64);
        self.inner
            .cell_recovery_jobs
            .set(snapshot.cell_recovery_jobs as f64);
        self.inner
            .cell_recovery_job_capacity
            .set(snapshot.cell_recovery_job_capacity as f64);
        self.inner
            .cell_dirty_jobs
            .set(snapshot.cell_dirty_jobs as f64);
        self.inner
            .cell_dirty_job_capacity
            .set(snapshot.cell_dirty_job_capacity as f64);
        self.inner
            .cell_scratch_units
            .set(snapshot.cell_scratch_units as f64);
        self.inner
            .cell_scratch_unit_capacity
            .set(snapshot.cell_scratch_unit_capacity as f64);
        self.inner
            .cell_local_disk_reserved_bytes
            .set(snapshot.cell_local_disk_reserved_bytes as f64);
        self.inner
            .cell_local_disk_capacity_bytes
            .set(snapshot.cell_local_disk_capacity_bytes as f64);
        self.inner
            .cell_node_log_uncovered_bytes
            .set(snapshot.cell_node_log_uncovered_bytes as f64);
        self.inner
            .cell_follower_retained_bytes
            .set(snapshot.cell_follower_retained_bytes as f64);
        for (index, admission) in self.inner.admission.iter().enumerate() {
            admission
                .available
                .set(snapshot.admission_available[index] as f64);
            admission
                .capacity
                .set(snapshot.admission_capacity[index] as f64);
        }
        self.inner.handle.run_upkeep();
        self.inner.handle.render()
    }

    fn record_request(&self, method: usize, outcome: usize) {
        self.inner.methods[method].requests[outcome].increment(1);
    }

    fn record_duration(&self, method: usize, started: Instant) {
        self.inner.methods[method]
            .duration
            .record(started.elapsed().as_secs_f64());
    }
}

impl crab_cell_runtime::CellTelemetry for Metrics {
    fn durability_proof(&self, source: crab_cell_runtime::DurabilitySource, waited: Duration) {
        let index = match source {
            crab_cell_runtime::DurabilitySource::Fleet => 0,
            crab_cell_runtime::DurabilitySource::Object => 1,
        };
        self.inner.durability_proofs[index].increment(1);
        self.inner.durability_wait[index].record(waited.as_secs_f64());
    }

    fn node_log_append(&self, acknowledged: bool, bytes: u64) {
        self.inner.node_log_append_bytes[usize::from(!acknowledged)].increment(bytes);
    }

    fn resident_route(&self, outcome: crab_cell_runtime::ResidentRouteOutcome) {
        let index = match outcome {
            crab_cell_runtime::ResidentRouteOutcome::Hit => 0,
            crab_cell_runtime::ResidentRouteOutcome::Miss => 1,
            crab_cell_runtime::ResidentRouteOutcome::Refused => 2,
        };
        self.inner.resident_routes[index].increment(1);
    }

    fn ltx_phase(&self, phase: crab_cell_runtime::LtxPhase, elapsed: Duration, succeeded: bool) {
        let index = match phase {
            crab_cell_runtime::LtxPhase::Capture => 0,
            crab_cell_runtime::LtxPhase::Preparation => 1,
            crab_cell_runtime::LtxPhase::SchemaCheck => 2,
            crab_cell_runtime::LtxPhase::WalExistence => 3,
            crab_cell_runtime::LtxPhase::PositionResolution => 4,
            crab_cell_runtime::LtxPhase::WalRead => 5,
            crab_cell_runtime::LtxPhase::PageCollection => 6,
            crab_cell_runtime::LtxPhase::Verification => 7,
            crab_cell_runtime::LtxPhase::Encode => 8,
            crab_cell_runtime::LtxPhase::LocalWrite => 9,
            crab_cell_runtime::LtxPhase::Fsync => 10,
            crab_cell_runtime::LtxPhase::ParentSync => 11,
            crab_cell_runtime::LtxPhase::Checkpoint => 12,
            crab_cell_runtime::LtxPhase::RootOpen => 13,
            crab_cell_runtime::LtxPhase::Directory => 14,
            crab_cell_runtime::LtxPhase::FrameFetch => 15,
            crab_cell_runtime::LtxPhase::RestoreWrite => 16,
            crab_cell_runtime::LtxPhase::Compaction => 17,
        };
        self.inner.ltx_phase_runs[index][usize::from(!succeeded)].increment(1);
        self.inner.ltx_phase_duration[index].record(elapsed.as_secs_f64());
    }

    fn ltx_logical_read(&self, origin: crab_cell_runtime::LtxReadOrigin) {
        let index = match origin {
            crab_cell_runtime::LtxReadOrigin::Cold => 0,
            crab_cell_runtime::LtxReadOrigin::Sparse => 1,
            crab_cell_runtime::LtxReadOrigin::Hydrating => 2,
            crab_cell_runtime::LtxReadOrigin::Resident => 3,
        };
        self.inner.ltx_logical_reads[index].increment(1);
    }

    fn ltx_origin_request(
        &self,
        origin: crab_cell_runtime::LtxReadOrigin,
        outcome: crab_cell_runtime::LtxRequestOutcome,
        bytes: u64,
    ) {
        let index = match origin {
            crab_cell_runtime::LtxReadOrigin::Cold => 0,
            crab_cell_runtime::LtxReadOrigin::Sparse => 1,
            crab_cell_runtime::LtxReadOrigin::Hydrating => 2,
            crab_cell_runtime::LtxReadOrigin::Resident => 3,
        };
        let outcome = match outcome {
            crab_cell_runtime::LtxRequestOutcome::Succeeded => 0,
            crab_cell_runtime::LtxRequestOutcome::Failed => 1,
        };
        self.inner.ltx_origin_requests[index][outcome].increment(1);
        self.inner.ltx_origin_bytes[index].increment(bytes);
    }

    fn ltx_capture(&self, timing: &crab_cell_runtime::CaptureTiming, succeeded: bool) {
        <Self as crab_cell_runtime::CellTelemetry>::ltx_phase(
            self,
            crab_cell_runtime::LtxPhase::Capture,
            Duration::from_nanos(timing.total_nanos),
            succeeded,
        );
        let phase = |phase, nanos| {
            if nanos > 0 {
                <Self as crab_cell_runtime::CellTelemetry>::ltx_phase(
                    self,
                    phase,
                    Duration::from_nanos(nanos),
                    succeeded,
                );
            }
        };
        phase(
            crab_cell_runtime::LtxPhase::Preparation,
            timing.preparation_nanos,
        );
        phase(
            crab_cell_runtime::LtxPhase::SchemaCheck,
            timing.schema_check_nanos,
        );
        phase(
            crab_cell_runtime::LtxPhase::WalExistence,
            timing.wal_existence_nanos,
        );
        phase(
            crab_cell_runtime::LtxPhase::PositionResolution,
            timing.position_resolution_nanos,
        );
        phase(crab_cell_runtime::LtxPhase::WalRead, timing.wal_read_nanos);
        phase(
            crab_cell_runtime::LtxPhase::PageCollection,
            timing.page_collection_nanos,
        );
        phase(
            crab_cell_runtime::LtxPhase::Verification,
            timing.verification_nanos,
        );
        phase(crab_cell_runtime::LtxPhase::Encode, timing.encode_nanos);
        phase(
            crab_cell_runtime::LtxPhase::LocalWrite,
            timing.local_write_nanos,
        );
        phase(crab_cell_runtime::LtxPhase::Fsync, timing.fsync_nanos);
        phase(
            crab_cell_runtime::LtxPhase::ParentSync,
            timing.parent_sync_nanos,
        );
        phase(
            crab_cell_runtime::LtxPhase::Checkpoint,
            timing.checkpoint_nanos,
        );
        self.inner.ltx_wal_reads[0].increment(u64::from(timing.wal_sparse_reads));
        self.inner.ltx_wal_reads[1].increment(u64::from(timing.wal_full_reads));
        self.inner.ltx_wal_reads[2].increment(u64::from(timing.wal_fallback_reads));
        self.inner
            .ltx_wal_image_bytes
            .increment(timing.wal_image_bytes);
        self.inner
            .ltx_wal_file_bytes
            .increment(timing.wal_file_bytes);
        self.inner
            .ltx_wal_read_bytes
            .increment(timing.wal_read_bytes);
        self.inner
            .ltx_wal_snapshot_reads
            .increment(u64::from(timing.wal_snapshot_reads));
        self.inner.ltx_capture_wal_bytes.increment(timing.wal_bytes);
        self.inner
            .ltx_capture_database_bytes
            .increment(timing.database_bytes);
        self.inner.ltx_capture_ltx_bytes.increment(timing.ltx_bytes);
        self.inner
            .ltx_capture_segments
            .increment(u64::from(timing.segment_count));
        self.inner
            .ltx_checkpoint_runs
            .increment(u64::from(timing.checkpoint_runs));
        self.inner
            .ltx_checkpoint_busy
            .increment(u64::from(timing.checkpoint_busy));
        self.inner
            .ltx_checkpoint_busy_errors
            .increment(u64::from(timing.checkpoint_busy_errors));
        self.inner
            .ltx_checkpoint_frames
            .increment(timing.checkpoint_frames);
        self.inner
            .ltx_checkpoint_backfilled
            .increment(timing.checkpoint_backfilled);
        self.inner
            .ltx_checkpoint_restarts
            .increment(u64::from(timing.checkpoint_restarts));
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ProjectionProbeResult {
    Ready,
    Changed,
    Error,
}

#[derive(Clone, Copy)]
pub(crate) enum ProjectionPhase {
    Reconcile,
}

#[derive(Clone, Copy)]
pub(crate) enum ProjectionBuildResult {
    Ok,
    Error,
    Superseded,
}

#[derive(Clone, Copy)]
pub(crate) enum ProjectionBatchKind {
    Refs,
    Commits,
    Trees,
    Attribution,
}

#[derive(Clone, Copy)]
pub(crate) enum ProjectionOriginReadKind {
    Snapshot,
    Graph,
    Tree,
}

#[derive(Clone, Copy)]
pub(crate) enum SelfFenceReason {
    Expiry,
    Refresh,
    Shutdown,
    Other,
}

#[derive(Clone, Copy)]
pub(crate) enum RecoveryFailureReason {
    Storage,
    Capacity,
    Fenced,
    Other,
}

impl Metrics {
    pub(crate) fn update_node_log(
        &self,
        log: Option<&crab_cell_runtime::NodeLogStatus>,
        lease_remaining: Duration,
    ) {
        let Some(log) = log else {
            for lane in &self.inner.node_log_lanes {
                lane.set(0.0);
            }
            self.inner
                .session_lease_seconds
                .set(lease_remaining.as_secs_f64());
            return;
        };
        let state = match log.phase() {
            crab_cell_runtime::NodeLogPhase::Open if log.active() => 0,
            crab_cell_runtime::NodeLogPhase::Open | crab_cell_runtime::NodeLogPhase::Recovering => {
                1
            }
            crab_cell_runtime::NodeLogPhase::Sealed | crab_cell_runtime::NodeLogPhase::Retired => 2,
        };
        for (index, lane) in self.inner.node_log_lanes.iter().enumerate() {
            lane.set(f64::from(index == state));
        }
        self.inner
            .session_lease_seconds
            .set(lease_remaining.as_secs_f64());
    }

    pub(crate) fn record_self_fence(&self, reason: SelfFenceReason) {
        if self.inner.self_fenced.swap(true, Ordering::AcqRel) {
            return;
        }
        let index = match reason {
            SelfFenceReason::Expiry => 0,
            SelfFenceReason::Refresh => 1,
            SelfFenceReason::Shutdown => 2,
            SelfFenceReason::Other => 3,
        };
        self.inner.self_fences[index].increment(1);
        for lane in &self.inner.node_log_lanes {
            lane.set(0.0);
        }
        self.inner.session_lease_seconds.set(0.0);
    }

    pub(crate) fn update_recovery_states(&self, running: usize, waiting: usize) {
        self.inner.node_log_recoveries[0].set(running as f64);
        self.inner.node_log_recoveries[1].set(waiting as f64);
    }

    pub(crate) fn record_recovery_finished(
        &self,
        elapsed: Duration,
        failure: Option<RecoveryFailureReason>,
    ) {
        self.inner
            .node_log_recovery_seconds
            .record(elapsed.as_secs_f64());
        if let Some(reason) = failure {
            let index = match reason {
                RecoveryFailureReason::Storage => 0,
                RecoveryFailureReason::Capacity => 1,
                RecoveryFailureReason::Fenced => 2,
                RecoveryFailureReason::Other => 3,
            };
            self.inner.node_log_recovery_failures[index].increment(1);
        }
    }

    pub(crate) fn record_node_log_rotation(&self, result: NodeLogRotationResult) {
        let index = match result {
            NodeLogRotationResult::Started => 0,
            NodeLogRotationResult::Pending => 1,
            NodeLogRotationResult::Failed => 2,
            NodeLogRotationResult::Completed => 3,
        };
        self.inner.node_log_rotations[index].increment(1);
    }
}

#[derive(Clone, Copy)]
pub(crate) enum NodeLogRotationResult {
    Started,
    Pending,
    Failed,
    Completed,
}

impl MethodMetrics {
    fn new(recorder: &impl Recorder, method: &'static str) -> Self {
        Self {
            requests: OUTCOME_LABELS.map(|outcome| {
                recorder.register_counter(
                    &key(
                        "crab_http_server_requests_total",
                        &[("method", method), ("outcome", outcome)],
                    ),
                    &METADATA,
                )
            }),
            in_flight: recorder.register_gauge(
                &key("crab_http_server_in_flight_requests", &[("method", method)]),
                &METADATA,
            ),
            duration: recorder.register_histogram(
                &key(
                    "crab_http_server_request_duration_seconds",
                    &[("method", method)],
                ),
                &METADATA,
            ),
            body_errors: recorder.register_counter(
                &key(
                    "crab_http_server_response_body_errors_total",
                    &[("method", method)],
                ),
                &METADATA,
            ),
            body_aborts: recorder.register_counter(
                &key(
                    "crab_http_server_response_body_aborts_total",
                    &[("method", method)],
                ),
                &METADATA,
            ),
        }
    }
}

impl AdmissionMetrics {
    fn new(recorder: &impl Recorder, class: &'static str) -> Self {
        Self {
            available: recorder.register_gauge(
                &key(
                    "crab_http_server_admission_available_permits",
                    &[("class", class)],
                ),
                &METADATA,
            ),
            capacity: recorder.register_gauge(
                &key("crab_http_server_admission_capacity", &[("class", class)]),
                &METADATA,
            ),
        }
    }
}

pub(crate) struct RequestObservation {
    metrics: Metrics,
    method: usize,
    started: Instant,
    outcome_recorded: bool,
    finished: bool,
}

impl RequestObservation {
    pub(crate) fn response(mut self, status: StatusCode) -> Self {
        self.metrics
            .record_request(self.method, status_outcome(status));
        self.outcome_recorded = true;
        self
    }

    fn body_error(&mut self) {
        self.metrics.inner.methods[self.method]
            .body_errors
            .increment(1);
        self.finish();
    }

    fn body_abort(&mut self) {
        self.metrics.inner.methods[self.method]
            .body_aborts
            .increment(1);
        self.finish();
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.metrics.record_duration(self.method, self.started);
        self.metrics.inner.methods[self.method]
            .in_flight
            .decrement(1);
        self.finished = true;
    }
}

impl Drop for RequestObservation {
    fn drop(&mut self) {
        if !self.outcome_recorded {
            self.metrics.record_request(self.method, 6);
            self.outcome_recorded = true;
        }
        self.finish();
    }
}

pub(crate) struct ObservedBody {
    inner: Body,
    observation: Option<RequestObservation>,
}

impl ObservedBody {
    pub(crate) fn new(inner: Body, mut observation: RequestObservation) -> Self {
        let observation = if inner.is_end_stream() {
            observation.finish();
            None
        } else {
            Some(observation)
        };
        Self { inner, observation }
    }

    fn finish(&mut self) {
        if let Some(mut observation) = self.observation.take() {
            observation.finish();
        }
    }
}

impl http_body::Body for ObservedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_frame(context) {
            Poll::Ready(Some(Ok(frame))) => {
                if this.inner.is_end_stream() {
                    this.finish();
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                if let Some(mut observation) = this.observation.take() {
                    observation.body_error();
                }
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                this.finish();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for ObservedBody {
    fn drop(&mut self) {
        if let Some(mut observation) = self.observation.take() {
            observation.body_abort();
        }
    }
}

fn describe_metrics(recorder: &impl Recorder) {
    describe_counter(
        recorder,
        "crab_http_server_requests_total",
        "Public HTTP requests by bounded method and response class.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_in_flight_requests",
        "Public requests whose handler or response body is still active.",
    );
    recorder.describe_histogram(
        KeyName::from_const_str("crab_http_server_request_duration_seconds"),
        Some(Unit::Seconds),
        "Full public request and response-body lifetime.".into(),
    );
    describe_counter(
        recorder,
        "crab_http_server_response_body_errors_total",
        "Response streams that failed after headers were produced.",
    );
    describe_counter(
        recorder,
        "crab_http_server_response_body_aborts_total",
        "Response streams dropped before their body completed.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_admission_available_permits",
        "Available fast-path permits in this process; Git transfers also require a deployment-wide storage lease.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_admission_capacity",
        "Configured permits in each process-local admission class.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_repositories",
        "Repositories loaded from the durable catalog.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_catalog_healthy",
        "Whether the latest catalog refresh and storage probe succeeded.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_scheduler_healthy",
        "Whether a complete repository scheduler cycle finished within 15 seconds.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_scheduler_progress",
        "Completed repository scheduler cycles in this boot session.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_scheduler_lag_seconds",
        "Seconds since this node completed a repository scheduler cycle.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_draining",
        "Whether graceful shutdown has started.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_receive_workers",
        "Receive workers retained for publication or cleanup.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_active_cells",
        "SQLite Cells currently retained by this process's fixed worker pool.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_active_cell_capacity",
        "Startup admission ceiling for active SQLite Cells in this process.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_resident_bytes",
        "Native resident bytes reserved by active Cells in the shared runtime ledger.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_resident_capacity_bytes",
        "Native resident-byte ceiling in the shared runtime ledger.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_file_descriptors",
        "File descriptors reserved by active Cells in the shared runtime ledger.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_file_descriptor_capacity",
        "File-descriptor ceiling for active Cells in the shared runtime ledger.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_retained_bytes",
        "Node-wide bytes currently reserved outside Cell mailboxes.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_retained_capacity_bytes",
        "Startup admission ceiling for node-wide retained bytes.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_worker_jobs",
        "SQL worker jobs currently admitted by the shared Cell ledger.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_worker_job_capacity",
        "SQL worker-job ceiling in the shared Cell ledger.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_primitive_jobs",
        "Primitive jobs currently admitted by the shared Cell ledger.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_primitive_job_capacity",
        "Primitive-job ceiling in the shared Cell ledger.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_hydration_jobs",
        "Background hydration jobs currently admitted by the shared Cell ledger.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_hydration_job_capacity",
        "Background hydration-job ceiling in the shared Cell ledger.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_io_slots",
        "Bounded LTX object-store I/O operations currently admitted by the shared Cell ledger.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_io_slot_capacity",
        "LTX object-store I/O ceiling in the shared Cell ledger.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_blocking_jobs",
        "Replica-host blocking jobs currently admitted by the shared Cell ledger.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_blocking_job_capacity",
        "Replica-host blocking-job ceiling in the shared Cell ledger.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_recovery_jobs",
        "Replica-host recovery cohorts currently admitted by the shared Cell ledger.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_recovery_job_capacity",
        "Replica-host recovery-cohort ceiling in the shared Cell ledger.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_dirty_jobs",
        "Replica-host dirty-memory cohorts currently admitted by the shared Cell ledger.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_dirty_job_capacity",
        "Replica-host dirty-memory-cohort ceiling in the shared Cell ledger.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_scratch_units",
        "Temporary scratch MiB units currently admitted by the shared Cell ledger.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_scratch_unit_capacity",
        "Temporary scratch MiB ceiling in the shared Cell ledger.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_local_disk_reserved_bytes",
        "Local database, WAL, LTX, sparse-page, and staging bytes currently reserved.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_cell_runtime_local_disk_capacity_bytes",
        "Startup admission ceiling for local Cell working bytes.",
    );
    describe_gauge(
        recorder,
        "crab_cell_node_log_uncovered_bytes",
        "Owner LTX bytes retained in node logs but not yet covered by object roots.",
    );
    describe_gauge(
        recorder,
        "crab_cell_follower_retained_bytes",
        "Verified follower node-log bytes retained on this node.",
    );
    describe_counter(
        recorder,
        "crab_cell_durability_proofs_total",
        "Completed Cell durability proofs by bounded source.",
    );
    recorder.describe_histogram(
        KeyName::from_const_str("crab_cell_durability_wait_seconds"),
        Some(Unit::Seconds),
        "Time from captured commit submission to the selected durability proof.".into(),
    );
    describe_counter(
        recorder,
        "crab_cell_node_log_append_bytes_total",
        "Bytes attempted across bounded follower append lanes by result.",
    );
    describe_counter(
        recorder,
        "crab_cell_resident_route_total",
        "Actor-owned resident route lookups by bounded outcome.",
    );
    describe_counter(
        recorder,
        "crab_cell_ltx_phase_total",
        "Completed LTX work by bounded phase and result.",
    );
    recorder.describe_histogram(
        KeyName::from_const_str("crab_cell_ltx_phase_seconds"),
        Some(Unit::Seconds),
        "Elapsed time for bounded LTX work phases.".into(),
    );
    describe_counter(
        recorder,
        "crab_cell_ltx_logical_reads_total",
        "Logical Cell reads by bounded residency class.",
    );
    describe_counter(
        recorder,
        "crab_cell_ltx_origin_requests_total",
        "Provider read attempts by bounded Cell residency class and result.",
    );
    describe_counter(
        recorder,
        "crab_cell_ltx_origin_bytes_total",
        "Returned read bytes by bounded Cell residency class.",
    );
    describe_counter(
        recorder,
        "crab_cell_ltx_wal_reads_total",
        "Capture WAL image reads by bounded strategy.",
    );
    describe_counter(
        recorder,
        "crab_cell_ltx_wal_image_bytes_total",
        "Peak allocated WAL image bytes summed across Cell captures.",
    );
    describe_counter(
        recorder,
        "crab_cell_ltx_wal_file_bytes_total",
        "Largest observed physical WAL file bytes summed across Cell captures.",
    );
    describe_counter(
        recorder,
        "crab_cell_ltx_wal_read_bytes_total",
        "Physical WAL bytes transferred into capture memory.",
    );
    describe_counter(
        recorder,
        "crab_cell_ltx_wal_snapshot_reads_total",
        "Complete WAL images selected before incremental WAL parsing.",
    );
    for (name, description) in [
        (
            "crab_cell_ltx_capture_wal_bytes_total",
            "Logical WAL bytes consumed by Cell captures.",
        ),
        (
            "crab_cell_ltx_capture_database_bytes_total",
            "Logical database bytes represented by Cell captures.",
        ),
        (
            "crab_cell_ltx_capture_ltx_bytes_total",
            "LTX bytes inspected by completed Cell captures.",
        ),
        (
            "crab_cell_ltx_capture_segments_total",
            "LTX segments produced by completed Cell captures.",
        ),
        (
            "crab_cell_ltx_checkpoint_runs_total",
            "SQLite checkpoint pragmas executed by Cell captures.",
        ),
        (
            "crab_cell_ltx_checkpoint_busy_total",
            "SQLite checkpoint pragmas that reported busy.",
        ),
        (
            "crab_cell_ltx_checkpoint_busy_errors_total",
            "SQLite checkpoint pragmas that failed busy or locked.",
        ),
        (
            "crab_cell_ltx_checkpoint_frames_total",
            "WAL frames reported by Cell checkpoints.",
        ),
        (
            "crab_cell_ltx_checkpoint_backfilled_total",
            "WAL frames backfilled by Cell checkpoints.",
        ),
        (
            "crab_cell_ltx_checkpoint_restarts_total",
            "Cell checkpoints that restarted the WAL lineage.",
        ),
    ] {
        describe_counter(recorder, name, description);
    }
    describe_gauge(
        recorder,
        "crab_cell_node_log_lanes",
        "Current bounded node-log lane state; exactly one state is active for this node session.",
    );
    describe_gauge(
        recorder,
        "crab_cell_session_lease_seconds",
        "Seconds remaining on the authoritative node-session lease.",
    );
    describe_counter(
        recorder,
        "crab_cell_self_fences_total",
        "Terminal node self-fences by bounded reason.",
    );
    describe_gauge(
        recorder,
        "crab_cell_node_log_recoveries",
        "Node-log recovery sessions currently running or waiting.",
    );
    recorder.describe_histogram(
        KeyName::from_const_str("crab_cell_node_log_recovery_seconds"),
        Some(Unit::Seconds),
        "Node-log recovery duration from claim to sealed witness.".into(),
    );
    describe_counter(
        recorder,
        "crab_cell_node_log_recovery_failures_total",
        "Node-log recovery failures by bounded reason.",
    );
    describe_counter(
        recorder,
        "crab_cell_node_log_rotations_total",
        "Node-log epoch rotation attempts by bounded result.",
    );
    describe_counter(
        recorder,
        "crab_http_server_catalog_refresh_failures_total",
        "Catalog refresh attempts that failed or moved backwards.",
    );
    describe_counter(
        recorder,
        "crab_http_server_transfer_admission_rejections_total",
        "Transfers rejected by deployment-wide capacity or coordination failures.",
    );
    describe_counter(
        recorder,
        "crab_git_projection_probe_total",
        "Repository projection source probes by bounded result.",
    );
    recorder.describe_histogram(
        KeyName::from_const_str("crab_git_projection_probe_seconds"),
        Some(Unit::Seconds),
        "Repository projection source probe duration by bounded result.".into(),
    );
    recorder.describe_histogram(
        KeyName::from_const_str("crab_git_projection_build_seconds"),
        Some(Unit::Seconds),
        "Repository projection build duration by phase and result.".into(),
    );
    describe_counter(
        recorder,
        "crab_git_projection_batch_rows_total",
        "Rows submitted to repository projection batches by kind.",
    );
    describe_counter(
        recorder,
        "crab_git_projection_batch_bytes_total",
        "Serialized bytes submitted to repository projection batches by kind.",
    );
    describe_counter(
        recorder,
        "crab_git_projection_superseded_total",
        "Projection epochs superseded after an origin identity changed.",
    );
    describe_gauge(
        recorder,
        "crab_git_projection_ready",
        "Whether the most recently observed repository projection is ready.",
    );
    describe_gauge(
        recorder,
        "crab_git_projection_lag_generations",
        "Manifest generations between origin and the ready projection.",
    );
    recorder.describe_gauge(
        KeyName::from_const_str("crab_git_projection_lag_seconds"),
        Some(Unit::Seconds),
        "Seconds since the ready projection was verified.".into(),
    );
    describe_gauge(
        recorder,
        "crab_git_projection_sqlite_bytes",
        "Approximate repository projection SQLite bytes when available.",
    );
    describe_counter(
        recorder,
        "crab_git_projection_origin_reads_total",
        "Origin reads performed while rebuilding repository projections by kind.",
    );
}

fn describe_counter(recorder: &impl Recorder, name: &'static str, description: &'static str) {
    recorder.describe_counter(KeyName::from_const_str(name), None, description.into());
}

fn describe_gauge(recorder: &impl Recorder, name: &'static str, description: &'static str) {
    recorder.describe_gauge(KeyName::from_const_str(name), None, description.into());
}

fn key(name: &'static str, labels: &[(&'static str, &'static str)]) -> Key {
    Key::from_parts(
        name,
        labels
            .iter()
            .map(|(name, value)| Label::from_static_parts(name, value))
            .collect::<Vec<_>>(),
    )
}

fn method_index(method: &Method) -> usize {
    match *method {
        Method::GET => 0,
        Method::HEAD => 1,
        Method::PUT => 2,
        Method::POST => 3,
        Method::DELETE => 4,
        _ => 5,
    }
}

fn status_outcome(status: StatusCode) -> usize {
    match status.as_u16() / 100 {
        1 => 0,
        2 => 1,
        3 => 2,
        4 => 3,
        5 => 4,
        _ => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt as _;

    fn snapshot() -> RuntimeSnapshot {
        RuntimeSnapshot {
            repositories: 2,
            catalog_healthy: true,
            scheduler_healthy: true,
            scheduler_progress: 3,
            scheduler_lag_seconds: 0.25,
            draining: false,
            receive_workers: 1,
            cell_active: 0,
            cell_active_capacity: 7,
            cell_resident_bytes: 0,
            cell_resident_capacity_bytes: 458_752,
            cell_file_descriptors: 0,
            cell_file_descriptor_capacity: 56,
            cell_retained_bytes: 0,
            cell_retained_capacity_bytes: 4_096,
            cell_worker_jobs: 1,
            cell_worker_job_capacity: 4,
            cell_primitive_jobs: 0,
            cell_primitive_job_capacity: 4,
            cell_hydration_jobs: 0,
            cell_hydration_job_capacity: 2,
            cell_io_slots: 0,
            cell_io_slot_capacity: 32,
            cell_blocking_jobs: 1,
            cell_blocking_job_capacity: 4,
            cell_recovery_jobs: 0,
            cell_recovery_job_capacity: 2,
            cell_dirty_jobs: 0,
            cell_dirty_job_capacity: 4,
            cell_scratch_units: 0,
            cell_scratch_unit_capacity: 1_024,
            cell_local_disk_reserved_bytes: 0,
            cell_local_disk_capacity_bytes: 8_192,
            cell_node_log_uncovered_bytes: 128,
            cell_follower_retained_bytes: 256,
            admission_available: [16, 3, 8, 1],
            admission_capacity: [16, 4, 8, 2],
        }
    }

    #[tokio::test]
    async fn runtime_snapshot_projects_live_cell_ledger() {
        let disk_budget = crab_ltx::DiskBudget::new(8_192);
        let runtime = crab_cell_runtime::CellRuntime::new_with_replica_host(
            crab_cell_runtime::SqlWorkerPool::new(1, 2).unwrap(),
            1_024,
            crab_cell_runtime::SessionId::from_bytes([3; 16]),
            crab_ltx::Host::default().with_local_disk_budget(disk_budget.clone()),
        )
        .unwrap();
        let reservation = runtime.try_reserve_worker_job().unwrap().unwrap();
        let disk = disk_budget.try_reserve(128).unwrap();
        let stats = runtime.stats();
        let metrics = Metrics::new().unwrap();
        let rendered = metrics.render(RuntimeSnapshot::default().with_cell_runtime(stats));

        assert!(rendered.contains(&format!(
            "crab_http_server_cell_runtime_active_cell_capacity {}",
            stats.active_cell_capacity()
        )));
        assert!(rendered.contains(&format!(
            "crab_http_server_cell_runtime_primitive_jobs {}",
            stats.primitive_jobs()
        )));
        assert!(rendered.contains(&format!(
            "crab_http_server_cell_runtime_primitive_job_capacity {}",
            stats.primitive_job_capacity()
        )));
        assert!(rendered.contains(&format!(
            "crab_http_server_cell_runtime_local_disk_capacity_bytes {}",
            stats.local_disk_capacity_bytes()
        )));
        assert!(rendered.contains(&format!(
            "crab_http_server_cell_runtime_local_disk_reserved_bytes {}",
            stats.local_disk_reserved_bytes()
        )));

        drop(disk);
        drop(reservation);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn completed_request_exports_bounded_full_body_metrics() {
        let metrics = Metrics::new().unwrap();
        metrics.record_transfer_admission_rejection(false);
        metrics.record_transfer_admission_rejection(true);
        <Metrics as crab_cell_runtime::CellTelemetry>::durability_proof(
            &metrics,
            crab_cell_runtime::DurabilitySource::Fleet,
            Duration::from_millis(25),
        );
        <Metrics as crab_cell_runtime::CellTelemetry>::durability_proof(
            &metrics,
            crab_cell_runtime::DurabilitySource::Object,
            Duration::from_millis(50),
        );
        <Metrics as crab_cell_runtime::CellTelemetry>::node_log_append(&metrics, true, 512);
        <Metrics as crab_cell_runtime::CellTelemetry>::node_log_append(&metrics, false, 128);
        <Metrics as crab_cell_runtime::CellTelemetry>::resident_route(
            &metrics,
            crab_cell_runtime::ResidentRouteOutcome::Hit,
        );
        <Metrics as crab_cell_runtime::CellTelemetry>::resident_route(
            &metrics,
            crab_cell_runtime::ResidentRouteOutcome::Miss,
        );
        <Metrics as crab_cell_runtime::CellTelemetry>::ltx_phase(
            &metrics,
            crab_cell_runtime::LtxPhase::RootOpen,
            Duration::from_millis(10),
            true,
        );
        <Metrics as crab_cell_runtime::CellTelemetry>::ltx_phase(
            &metrics,
            crab_cell_runtime::LtxPhase::FrameFetch,
            Duration::from_millis(5),
            false,
        );
        <Metrics as crab_cell_runtime::CellTelemetry>::ltx_logical_read(
            &metrics,
            crab_cell_runtime::LtxReadOrigin::Resident,
        );
        <Metrics as crab_cell_runtime::CellTelemetry>::ltx_origin_request(
            &metrics,
            crab_cell_runtime::LtxReadOrigin::Cold,
            crab_cell_runtime::LtxRequestOutcome::Succeeded,
            1_024,
        );
        <Metrics as crab_cell_runtime::CellTelemetry>::ltx_origin_request(
            &metrics,
            crab_cell_runtime::LtxReadOrigin::Hydrating,
            crab_cell_runtime::LtxRequestOutcome::Failed,
            4_096,
        );
        <Metrics as crab_cell_runtime::CellTelemetry>::ltx_capture(
            &metrics,
            &crab_cell_runtime::CaptureTiming {
                schema_check_nanos: 1_000_000,
                wal_sparse_reads: 1,
                wal_image_bytes: 8_192,
                wal_file_bytes: 12_288,
                wal_read_bytes: 4_160,
                wal_snapshot_reads: 1,
                wal_bytes: 4_096,
                database_bytes: 16_384,
                ltx_bytes: 2_048,
                segment_count: 1,
                checkpoint_runs: 1,
                checkpoint_frames: 4,
                checkpoint_backfilled: 3,
                checkpoint_restarts: 1,
                ..Default::default()
            },
            true,
        );
        metrics.record_self_fence(SelfFenceReason::Refresh);
        metrics.record_self_fence(SelfFenceReason::Shutdown);
        metrics.update_recovery_states(1, 0);
        metrics.record_recovery_finished(
            Duration::from_millis(75),
            Some(RecoveryFailureReason::Storage),
        );
        metrics.record_node_log_rotation(NodeLogRotationResult::Started);
        metrics.record_node_log_rotation(NodeLogRotationResult::Pending);
        metrics.record_node_log_rotation(NodeLogRotationResult::Failed);
        metrics.record_node_log_rotation(NodeLogRotationResult::Completed);
        let observation = metrics.start_request(&Method::GET).response(StatusCode::OK);
        let body = Body::new(ObservedBody::new(Body::from("response"), observation));
        assert_eq!(
            body.collect().await.unwrap().to_bytes(),
            Bytes::from_static(b"response")
        );

        let rendered = metrics.render(snapshot());
        assert!(
            rendered.contains("crab_http_server_requests_total{method=\"get\",outcome=\"2xx\"} 1")
        );
        assert!(rendered.contains("crab_http_server_in_flight_requests{method=\"get\"} 0"));
        assert!(
            rendered.contains("crab_http_server_request_duration_seconds_count{method=\"get\"} 1")
        );
        assert!(rendered.contains("crab_http_server_catalog_healthy 1"));
        assert!(rendered.contains("crab_http_server_scheduler_healthy 1"));
        assert!(rendered.contains("crab_http_server_scheduler_progress 3"));
        assert!(rendered.contains("crab_http_server_scheduler_lag_seconds 0.25"));
        assert!(rendered.contains("crab_http_server_repositories 2"));
        assert!(rendered.contains("crab_http_server_cell_runtime_active_cells 0"));
        assert!(rendered.contains("crab_http_server_cell_runtime_active_cell_capacity 7"));
        assert!(rendered.contains("crab_http_server_cell_runtime_resident_bytes 0"));
        assert!(rendered.contains("crab_http_server_cell_runtime_resident_capacity_bytes 458752"));
        assert!(rendered.contains("crab_http_server_cell_runtime_file_descriptors 0"));
        assert!(rendered.contains("crab_http_server_cell_runtime_file_descriptor_capacity 56"));
        assert!(rendered.contains("crab_http_server_cell_runtime_retained_bytes 0"));
        assert!(rendered.contains("crab_http_server_cell_runtime_retained_capacity_bytes 4096"));
        assert!(rendered.contains("crab_http_server_cell_runtime_io_slots 0"));
        assert!(rendered.contains("crab_http_server_cell_runtime_io_slot_capacity 32"));
        assert!(rendered.contains("crab_http_server_cell_runtime_blocking_jobs 1"));
        assert!(rendered.contains("crab_http_server_cell_runtime_blocking_job_capacity 4"));
        assert!(rendered.contains("crab_http_server_cell_runtime_recovery_jobs 0"));
        assert!(rendered.contains("crab_http_server_cell_runtime_recovery_job_capacity 2"));
        assert!(rendered.contains("crab_http_server_cell_runtime_dirty_jobs 0"));
        assert!(rendered.contains("crab_http_server_cell_runtime_dirty_job_capacity 4"));
        assert!(rendered.contains("crab_http_server_cell_runtime_scratch_units 0"));
        assert!(rendered.contains("crab_http_server_cell_runtime_scratch_unit_capacity 1024"));
        assert!(rendered.contains("crab_http_server_cell_runtime_local_disk_reserved_bytes 0"));
        assert!(rendered.contains("crab_http_server_cell_runtime_local_disk_capacity_bytes 8192"));
        assert!(rendered.contains("crab_cell_node_log_uncovered_bytes 128"));
        assert!(rendered.contains("crab_cell_follower_retained_bytes 256"));
        assert!(rendered.contains("crab_cell_durability_proofs_total{source=\"fleet\"} 1"));
        assert!(rendered.contains("crab_cell_durability_proofs_total{source=\"object\"} 1"));
        assert!(rendered.contains("crab_cell_node_log_append_bytes_total{result=\"acked\"} 512"));
        assert!(rendered.contains("crab_cell_node_log_append_bytes_total{result=\"nacked\"} 128"));
        assert!(rendered.contains("crab_cell_resident_route_total{outcome=\"hit\"} 1"));
        assert!(rendered.contains("crab_cell_resident_route_total{outcome=\"miss\"} 1"));
        assert!(
            rendered
                .contains("crab_cell_ltx_phase_total{phase=\"root_open\",result=\"succeeded\"} 1")
        );
        assert!(
            rendered
                .contains("crab_cell_ltx_phase_total{phase=\"frame_fetch\",result=\"failed\"} 1")
        );
        assert!(rendered.contains("crab_cell_ltx_phase_seconds_count{phase=\"root_open\"} 1"));
        assert!(rendered.contains("crab_cell_ltx_logical_reads_total{origin=\"resident\"} 1"));
        assert!(rendered.contains(
            "crab_cell_ltx_origin_requests_total{origin=\"cold\",result=\"succeeded\"} 1"
        ));
        assert!(rendered.contains("crab_cell_ltx_origin_bytes_total{origin=\"cold\"} 1024"));
        assert!(rendered.contains(
            "crab_cell_ltx_origin_requests_total{origin=\"hydrating\",result=\"failed\"} 1"
        ));
        assert!(rendered.contains("crab_cell_ltx_origin_bytes_total{origin=\"hydrating\"} 4096"));
        assert!(
            rendered.contains(
                "crab_cell_ltx_phase_total{phase=\"schema_check\",result=\"succeeded\"} 1"
            )
        );
        assert!(rendered.contains("crab_cell_ltx_wal_reads_total{strategy=\"sparse\"} 1"));
        assert!(rendered.contains("crab_cell_ltx_wal_image_bytes_total 8192"));
        assert!(rendered.contains("crab_cell_ltx_wal_file_bytes_total 12288"));
        assert!(rendered.contains("crab_cell_ltx_wal_read_bytes_total 4160"));
        assert!(rendered.contains("crab_cell_ltx_wal_snapshot_reads_total 1"));
        assert!(rendered.contains("crab_cell_ltx_capture_wal_bytes_total 4096"));
        assert!(rendered.contains("crab_cell_ltx_checkpoint_runs_total 1"));
        assert!(rendered.contains("crab_cell_ltx_checkpoint_frames_total 4"));
        assert!(rendered.contains("crab_cell_ltx_checkpoint_backfilled_total 3"));
        assert!(rendered.contains("crab_cell_ltx_checkpoint_restarts_total 1"));
        assert!(rendered.contains("crab_cell_self_fences_total{reason=\"refresh\"} 1"));
        assert!(rendered.contains("crab_cell_session_lease_seconds 0"));
        assert!(rendered.contains("crab_cell_node_log_recoveries{state=\"running\"} 1"));
        assert!(rendered.contains("crab_cell_node_log_recovery_seconds_count 1"));
        assert!(
            rendered.contains("crab_cell_node_log_recovery_failures_total{reason=\"storage\"} 1")
        );
        assert!(rendered.contains("crab_cell_node_log_rotations_total{result=\"started\"} 1"));
        assert!(rendered.contains("crab_cell_node_log_rotations_total{result=\"pending\"} 1"));
        assert!(rendered.contains("crab_cell_node_log_rotations_total{result=\"failed\"} 1"));
        assert!(rendered.contains("crab_cell_node_log_rotations_total{result=\"completed\"} 1"));
        assert!(
            rendered
                .contains("crab_http_server_admission_available_permits{class=\"git_transfer\"} 3")
        );
        assert!(rendered.contains(
            "crab_http_server_transfer_admission_rejections_total{reason=\"capacity\"} 1"
        ));
        assert!(rendered.contains(
            "crab_http_server_transfer_admission_rejections_total{reason=\"coordination\"} 1"
        ));
        assert!(!rendered.contains("repository=\""));
    }

    #[test]
    fn dropped_response_body_records_abort_and_releases_in_flight_gauge() {
        let metrics = Metrics::new().unwrap();
        let observation = metrics.start_request(&Method::GET).response(StatusCode::OK);
        drop(ObservedBody::new(Body::from("response"), observation));

        let rendered = metrics.render(snapshot());
        assert!(rendered.contains("crab_http_server_response_body_aborts_total{method=\"get\"} 1"));
        assert!(rendered.contains("crab_http_server_in_flight_requests{method=\"get\"} 0"));
    }

    #[tokio::test]
    async fn failed_response_body_records_stream_error() {
        let metrics = Metrics::new().unwrap();
        let observation = metrics.start_request(&Method::GET).response(StatusCode::OK);
        let stream =
            futures_util::stream::iter([Err::<Bytes, _>(std::io::Error::other("stream failed"))]);
        let mut body = Body::new(ObservedBody::new(Body::from_stream(stream), observation));

        assert!(body.frame().await.unwrap().is_err());
        let rendered = metrics.render(snapshot());
        assert!(rendered.contains("crab_http_server_response_body_errors_total{method=\"get\"} 1"));
        assert!(rendered.contains("crab_http_server_in_flight_requests{method=\"get\"} 0"));
    }

    #[test]
    fn request_cancelled_before_response_is_counted_and_released() {
        let metrics = Metrics::new().unwrap();
        drop(metrics.start_request(&Method::PUT));

        let rendered = metrics.render(snapshot());
        assert!(
            rendered.contains(
                "crab_http_server_requests_total{method=\"put\",outcome=\"cancelled\"} 1"
            )
        );
        assert!(rendered.contains("crab_http_server_in_flight_requests{method=\"put\"} 0"));
    }
}
