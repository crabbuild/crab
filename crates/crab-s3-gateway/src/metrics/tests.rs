use super::*;
use http_body_util::BodyExt as _;
use object_store::{ObjectStoreExt as _, path::Path};
use tokio_util::sync::CancellationToken;

fn setup() -> (Admission, Metrics) {
    let metrics = Metrics::new().unwrap();
    let admission = Admission::new(8, CancellationToken::new(), metrics.clone());
    (admission, metrics)
}

fn metric_value(body: &str, name: &str) -> f64 {
    body.lines()
        .find_map(|line| line.strip_prefix(&format!("{name} ")))
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn completed_request_exports_bounded_prometheus_series() {
    let (admission, metrics) = setup();
    let mut observation = metrics
        .start_request(&Method::GET)
        .response(StatusCode::PARTIAL_CONTENT);
    observation.finish();

    let body = metrics.render(&admission);
    assert!(body.contains("crab_s3_gateway_http_requests_total{method=\"get\",outcome=\"2xx\"} 1"));
    assert!(body.contains("crab_s3_gateway_http_in_flight_requests{method=\"get\"} 0"));
    assert!(body.contains("crab_s3_gateway_http_request_duration_seconds_count{method=\"get\"} 1"));
    assert!(body.contains("crab_s3_gateway_admission_capacity{class=\"transfer\"} 2"));
    assert!(!body.contains("repository=\""));
}

#[test]
fn request_cancelled_before_response_is_counted_and_released() {
    let (admission, metrics) = setup();
    drop(metrics.start_request(&Method::PUT));

    let body = metrics.render(&admission);
    assert!(
        body.contains(
            "crab_s3_gateway_http_requests_total{method=\"put\",outcome=\"cancelled\"} 1"
        )
    );
    assert!(body.contains("crab_s3_gateway_http_in_flight_requests{method=\"put\"} 0"));
}

#[tokio::test]
async fn response_observation_lives_until_the_body_completes() {
    let (admission, metrics) = setup();
    let observation = metrics.start_request(&Method::GET).response(StatusCode::OK);
    let mut body = ObservedBody::new(
        s3s::Body::from(Bytes::from_static(b"response")),
        observation,
    );

    assert!(
        metrics
            .render(&admission)
            .contains("crab_s3_gateway_http_in_flight_requests{method=\"get\"} 1")
    );
    assert_eq!(
        body.frame().await.unwrap().unwrap().into_data().unwrap(),
        b"response"[..]
    );
    let rendered = metrics.render(&admission);
    assert!(rendered.contains("crab_s3_gateway_http_in_flight_requests{method=\"get\"} 0"));
    assert!(rendered.contains("crab_s3_gateway_http_response_body_aborts_total{method=\"get\"} 0"));
}

#[test]
fn dropped_response_body_records_client_abort() {
    let (admission, metrics) = setup();
    let observation = metrics.start_request(&Method::GET).response(StatusCode::OK);
    drop(ObservedBody::new(
        s3s::Body::from(Bytes::from_static(b"response")),
        observation,
    ));

    let rendered = metrics.render(&admission);
    assert!(rendered.contains("crab_s3_gateway_http_response_body_aborts_total{method=\"get\"} 1"));
    assert!(rendered.contains("crab_s3_gateway_http_in_flight_requests{method=\"get\"} 0"));
}

#[tokio::test]
async fn failed_response_body_records_stream_error() {
    let (admission, metrics) = setup();
    let observation = metrics.start_request(&Method::GET).response(StatusCode::OK);
    let stream = futures_util::stream::iter([Err::<Frame<Bytes>, _>(std::io::Error::other(
        "stream failed",
    ))]);
    let source = http_body_util::StreamBody::new(stream);
    let mut body = ObservedBody::new(s3s::Body::http_body_unsync(source), observation);

    assert!(body.frame().await.unwrap().is_err());
    let rendered = metrics.render(&admission);
    assert!(rendered.contains("crab_s3_gateway_http_response_body_errors_total{method=\"get\"} 1"));
    assert!(
        rendered.contains("crab_s3_gateway_http_response_body_timeouts_total{method=\"get\"} 0")
    );
    assert!(rendered.contains("crab_s3_gateway_http_in_flight_requests{method=\"get\"} 0"));
}

#[tokio::test]
async fn timed_out_response_body_records_a_timeout_subclass() {
    let (admission, metrics) = setup();
    let observation = metrics.start_request(&Method::GET).response(StatusCode::OK);
    let stream =
        futures_util::stream::iter([Err::<Frame<Bytes>, _>(crate::content::ResponseIdleTimeout)]);
    let source = http_body_util::StreamBody::new(stream);
    let mut body = ObservedBody::new(s3s::Body::http_body_unsync(source), observation);

    assert!(body.frame().await.unwrap().is_err());
    let rendered = metrics.render(&admission);
    assert!(rendered.contains("crab_s3_gateway_http_response_body_errors_total{method=\"get\"} 1"));
    assert!(
        rendered.contains("crab_s3_gateway_http_response_body_timeouts_total{method=\"get\"} 1")
    );
}

#[test]
fn renderer_emits_the_configured_cumulative_histogram() {
    let (admission, metrics) = setup();
    let mut observation = metrics.start_request(&Method::GET).response(StatusCode::OK);
    observation.finish();

    let body = metrics.render(&admission);
    for bound in ["0.005", "0.01", "0.025", "60", "+Inf"] {
        assert!(body.contains(&format!(
            "crab_s3_gateway_http_request_duration_seconds_bucket{{method=\"get\",le=\"{bound}\"}}"
        )));
    }
}

#[test]
fn renderer_exports_current_scratch_filesystem_capacity() {
    let scratch = tempfile::tempdir().unwrap();
    let metrics = Metrics::new_with_scratch_path(scratch.path().to_owned()).unwrap();
    let admission = Admission::new(8, CancellationToken::new(), metrics.clone());

    let body = metrics.render(&admission);
    let size = metric_value(&body, "crab_s3_gateway_scratch_filesystem_size_bytes");
    let free = metric_value(&body, "crab_s3_gateway_scratch_filesystem_free_bytes");
    let available = metric_value(&body, "crab_s3_gateway_scratch_filesystem_available_bytes");

    assert!(size > 0.0);
    assert!(free <= size);
    assert!(available <= free);
    assert!(metric_value(&body, "crab_s3_gateway_scratch_headroom_bytes") > 0.0);
    assert_eq!(
        metric_value(&body, "crab_s3_gateway_scratch_pending_bytes"),
        0.0
    );
    assert_eq!(
        metric_value(&body, "crab_s3_gateway_scratch_filesystem_probe_success"),
        1.0
    );
    assert_eq!(
        metric_value(
            &body,
            "crab_s3_gateway_scratch_filesystem_probe_failures_total"
        ),
        0.0
    );
}

#[test]
fn renderer_exports_configured_cache_limit() {
    let (admission, metrics) = setup();
    metrics.set_cache_limit(128 * 1024 * 1024);

    assert_eq!(
        metric_value(
            &metrics.render(&admission),
            "crab_s3_gateway_cache_limit_bytes",
        ),
        (128 * 1024 * 1024) as f64,
    );
}

#[test]
fn cache_observer_exports_fixed_source_outcomes_and_verified_bytes() {
    let (admission, metrics) = setup();
    let observer = metrics.cache_observer();
    observer.read(crab_cache_store::CacheReadObservation {
        source: crab_cache_store::CacheSource::Local,
        outcome: crab_cache_store::CacheReadOutcome::Miss,
        bytes: 0,
    });
    observer.read(crab_cache_store::CacheReadObservation {
        source: crab_cache_store::CacheSource::Local,
        outcome: crab_cache_store::CacheReadOutcome::Hit,
        bytes: 4096,
    });
    observer.local_write_failure();

    let body = metrics.render(&admission);
    assert!(body.contains(
        "crab_s3_gateway_cache_read_attempts_total{source=\"local\",outcome=\"miss\"} 1"
    ));
    assert!(
        body.contains(
            "crab_s3_gateway_cache_read_attempts_total{source=\"local\",outcome=\"hit\"} 1"
        )
    );
    assert!(body.contains("crab_s3_gateway_cache_bytes_read_total{source=\"local\"} 4096"));
    assert!(body.contains("crab_s3_gateway_cache_local_write_failures_total 1"));
    assert_eq!(
        body.lines()
            .filter(|line| line.starts_with("crab_s3_gateway_cache_read_attempts_total{"))
            .count(),
        crab_cache_store::CacheSource::ALL.len() * crab_cache_store::CacheReadOutcome::ALL.len()
    );
    assert!(!body.contains("repository="));
    assert!(!body.contains("path="));
}

#[tokio::test]
async fn cache_catalog_probe_exports_current_retention_without_scanning_payloads() {
    let (admission, metrics) = setup();
    let tempdir = tempfile::tempdir().unwrap();
    let cache = crab_cache::LocalCache::with_limits(
        tempdir.path().join("cache"),
        1024 * 1024,
        Some(1024 * 1024),
    );
    cache.prepare().unwrap();
    let body = Bytes::from_static(b"catalog-accounted-cache-body");
    let hash = blake3::hash(&body);
    cache
        .put_bytes(&crab_cache::CacheKey::RefTransaction(hash), body.clone())
        .await
        .unwrap();

    metrics.set_cache_limit(1024 * 1024);
    metrics.refresh_cache(&cache).await;
    let rendered = metrics.render(&admission);

    assert_eq!(
        metric_value(&rendered, "crab_s3_gateway_cache_retained_bytes"),
        body.len() as f64,
    );
    assert_eq!(
        metric_value(&rendered, "crab_s3_gateway_cache_entries"),
        1.0,
    );
    assert_eq!(
        metric_value(&rendered, "crab_s3_gateway_cache_reserved_bytes"),
        0.0,
    );
    assert_eq!(
        metric_value(&rendered, "crab_s3_gateway_cache_catalog_probe_success"),
        1.0,
    );
    assert!(
        metric_value(
            &rendered,
            "crab_s3_gateway_cache_catalog_last_success_timestamp_seconds"
        ) > 0.0
    );
}

#[tokio::test]
async fn failed_cache_catalog_probe_is_visible_without_stale_health() {
    let (admission, metrics) = setup();
    let tempdir = tempfile::tempdir().unwrap();
    let root = tempdir.path().join("not-a-directory");
    std::fs::write(&root, b"file").unwrap();
    let cache = crab_cache::LocalCache::new(root);

    metrics.refresh_cache(&cache).await;
    let rendered = metrics.render(&admission);

    assert_eq!(
        metric_value(&rendered, "crab_s3_gateway_cache_catalog_probe_success"),
        0.0,
    );
    assert_eq!(
        metric_value(
            &rendered,
            "crab_s3_gateway_cache_catalog_probe_failures_total"
        ),
        1.0,
    );
}

#[test]
fn scratch_reservation_is_visible_and_released_on_drop() {
    let scratch = tempfile::tempdir().unwrap();
    let metrics = Metrics::new_with_scratch_path(scratch.path().to_owned()).unwrap();
    let admission = Admission::new(8, CancellationToken::new(), metrics.clone());
    let reservation = metrics.reserve_scratch(4096).unwrap();

    assert_eq!(
        metric_value(
            &metrics.render(&admission),
            "crab_s3_gateway_scratch_pending_bytes"
        ),
        4096.0
    );
    drop(reservation);
    assert_eq!(
        metric_value(
            &metrics.render(&admission),
            "crab_s3_gateway_scratch_pending_bytes"
        ),
        0.0
    );
}

#[test]
fn oversized_scratch_reservation_is_retryable_capacity_pressure() {
    let scratch = tempfile::tempdir().unwrap();
    let metrics = Metrics::new_with_scratch_path(scratch.path().to_owned()).unwrap();
    let admission = Admission::new(8, CancellationToken::new(), metrics.clone());

    assert!(matches!(
        metrics.reserve_scratch(u64::MAX),
        Err(ScratchCapacityError::Exhausted)
    ));
    let body = metrics.render(&admission);
    assert_eq!(
        metric_value(
            &body,
            "crab_s3_gateway_scratch_capacity_rejections_total{reason=\"exhausted\"}"
        ),
        1.0
    );
    assert_eq!(
        metric_value(&body, "crab_s3_gateway_scratch_pending_bytes"),
        0.0
    );
}

#[test]
fn unavailable_scratch_probe_rejects_reservation_without_leaking_capacity() {
    let scratch = tempfile::tempdir().unwrap();
    let metrics = Metrics::new_with_scratch_path(scratch.path().join("missing")).unwrap();
    let admission = Admission::new(8, CancellationToken::new(), metrics.clone());

    assert!(matches!(
        metrics.reserve_scratch(1),
        Err(ScratchCapacityError::Unavailable(_))
    ));
    let body = metrics.render(&admission);
    assert_eq!(
        metric_value(
            &body,
            "crab_s3_gateway_scratch_capacity_rejections_total{reason=\"probe_error\"}"
        ),
        1.0
    );
    assert_eq!(
        metric_value(&body, "crab_s3_gateway_scratch_pending_bytes"),
        0.0
    );
}

#[test]
fn failed_scratch_filesystem_probe_clears_capacity_and_counts_failure() {
    let scratch = tempfile::tempdir().unwrap();
    let missing = scratch.path().join("missing");
    let metrics = Metrics::new_with_scratch_path(missing).unwrap();
    let admission = Admission::new(8, CancellationToken::new(), metrics.clone());

    let body = metrics.render(&admission);

    for name in [
        "crab_s3_gateway_scratch_filesystem_size_bytes",
        "crab_s3_gateway_scratch_filesystem_free_bytes",
        "crab_s3_gateway_scratch_filesystem_available_bytes",
        "crab_s3_gateway_scratch_filesystem_probe_success",
    ] {
        assert_eq!(metric_value(&body, name), 0.0);
    }
    assert_eq!(
        metric_value(
            &body,
            "crab_s3_gateway_scratch_filesystem_probe_failures_total"
        ),
        1.0
    );
}

#[test]
fn multipart_maintenance_exports_aggregate_health_without_identity_labels() {
    let (admission, metrics) = setup();
    let mut stats = SweepStats::default();
    stats.expired = 1;
    stats.terminal_cleanups = 2;
    stats.missing_cleanups = 3;
    stats.published_recoveries = 4;
    stats.reconciliation_failures = 2;
    stats.unresolved_completions = 1;
    let healthy = metrics.record_maintenance_result(&stats);
    metrics.record_maintenance_cycle(MaintenanceCycleOutcome::Degraded, Instant::now(), 100);
    metrics.record_maintenance_cycle(MaintenanceCycleOutcome::Success, Instant::now(), 123);

    let body = metrics.render(&admission);
    assert!(
        body.contains("crab_s3_gateway_multipart_maintenance_cycles_total{outcome=\"degraded\"} 1")
    );
    assert!(body.contains(
        "crab_s3_gateway_multipart_maintenance_failures_total{reason=\"reconciliation_error\"} 2"
    ));
    assert!(body.contains(
        "crab_s3_gateway_multipart_maintenance_failures_total{reason=\"publication_unresolved\"} 1"
    ));
    assert!(body.contains(
        "crab_s3_gateway_multipart_maintenance_actions_total{action=\"published_recovery\"} 4"
    ));
    assert!(body.contains("crab_s3_gateway_multipart_maintenance_cycle_duration_seconds_count 2"));
    assert!(
        body.contains("crab_s3_gateway_multipart_maintenance_last_success_timestamp_seconds 123")
    );
    assert!(!body.contains("upload_id"));
    assert!(!healthy);
}

#[test]
fn scratch_usage_remains_accounted_until_its_owner_drops() {
    let (admission, metrics) = setup();
    let mut usage = metrics.start_scratch(ScratchPurpose::ContentSpool);
    usage.reserve(4096);
    usage.record_written(4096);

    let active = metrics.render(&admission);
    assert!(active.contains("crab_s3_gateway_scratch_files{purpose=\"content_spool\"} 1"));
    assert!(active.contains("crab_s3_gateway_scratch_bytes{purpose=\"content_spool\"} 4096"));
    assert!(
        active.contains(
            "crab_s3_gateway_scratch_bytes_written_total{purpose=\"content_spool\"} 4096"
        )
    );

    drop(usage);
    let released = metrics.render(&admission);
    assert!(released.contains("crab_s3_gateway_scratch_files{purpose=\"content_spool\"} 0"));
    assert!(released.contains("crab_s3_gateway_scratch_bytes{purpose=\"content_spool\"} 0"));
}

#[test]
fn scratch_failures_use_only_bounded_purpose_and_operation_labels() {
    let (admission, metrics) = setup();
    let usage = metrics.start_scratch(ScratchPurpose::XetReconstruction);
    usage.record_failure(ScratchFailure::Read);
    drop(usage);

    let body = metrics.render(&admission);
    assert!(body.contains(
        "crab_s3_gateway_scratch_io_failures_total{purpose=\"xet_reconstruction\",operation=\"read\"} 1"
    ));
    assert!(!body.contains("path="));
}

#[tokio::test]
async fn backend_metrics_cover_stream_lifetime_ranges_and_write_bytes() {
    let (admission, metrics) = setup();
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_storage_observer(metrics.storage_observer());
    let path = Path::from("private/repository/large-object");
    store
        .inner()
        .put(&path, Bytes::from_static(b"0123456789").into())
        .await
        .unwrap();

    let result = store.inner().get_range(&path, 2..8).await.unwrap();
    assert_eq!(result, b"234567"[..]);
    store
        .inner()
        .get(&Path::from("private/repository/missing"))
        .await
        .unwrap_err();
    let listing = store.inner().list(None);
    assert!(
        metrics
            .render(&admission)
            .contains("crab_s3_gateway_backend_in_flight_requests{operation=\"list\"} 1")
    );
    drop(listing);

    let body = metrics.render(&admission);
    assert!(body.contains(
        "crab_s3_gateway_backend_requests_total{operation=\"put\",outcome=\"success\"} 1"
    ));
    assert!(body.contains("crab_s3_gateway_backend_bytes_written_total{operation=\"put\"} 10"));
    assert!(body.contains(
        "crab_s3_gateway_backend_requests_total{operation=\"range\",outcome=\"success\"} 1"
    ));
    assert!(body.contains(
        "crab_s3_gateway_backend_requests_total{operation=\"get\",outcome=\"not_found\"} 1"
    ));
    assert!(body.contains("crab_s3_gateway_backend_bytes_read_total{operation=\"range\"} 6"));
    assert!(body.contains(
        "crab_s3_gateway_backend_requests_total{operation=\"list\",outcome=\"cancelled\"} 1"
    ));
    assert!(
        body.contains(
            "crab_s3_gateway_backend_request_duration_seconds_count{operation=\"range\"} 1"
        )
    );
    assert_eq!(
        body.lines()
            .filter(|line| line.starts_with("crab_s3_gateway_backend_requests_total{"))
            .count(),
        crab_storage::StorageOperation::ALL.len() * crab_storage::StorageOutcome::ALL.len()
    );
    assert!(!body.contains("private/repository"));
}
