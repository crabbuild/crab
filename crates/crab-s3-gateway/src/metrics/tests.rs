use super::*;
use http_body_util::BodyExt as _;
use tokio_util::sync::CancellationToken;

fn setup() -> (Admission, Metrics) {
    let metrics = Metrics::new().unwrap();
    let admission = Admission::new(8, CancellationToken::new(), metrics.clone());
    (admission, metrics)
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
    assert!(rendered.contains("crab_s3_gateway_http_in_flight_requests{method=\"get\"} 0"));
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
