use super::*;
use http_body_util::BodyExt as _;

fn snapshot() -> RuntimeSnapshot {
    RuntimeSnapshot {
        repositories: 2,
        catalog_healthy: true,
        draining: false,
        receive_workers: 1,
        admission_available: [16, 3, 8, 1, 12],
        admission_capacity: [16, 4, 8, 2, 12],
    }
}

#[test]
fn git_metrics_keep_checksum_bytes_and_active_producer_lifetimes_separate() {
    use crab_remote_git::{MetricKind, MetricObservation, RemoteGitMetrics};

    let metrics = Metrics::new().unwrap();
    for (kind, value, duration) in [
        (
            MetricKind::CacheVerification,
            4096,
            Some(Duration::from_millis(2)),
        ),
        (MetricKind::BlameProducer, 1, None),
    ] {
        metrics.record(MetricObservation {
            kind,
            value,
            duration,
            outcome: None,
            cache: None,
        });
    }
    let started = metrics.render(snapshot());
    assert!(started.contains("crab_git_active_blame_producers 1"));
    metrics.record(MetricObservation {
        kind: MetricKind::BlameProducer,
        value: 1,
        duration: Some(Duration::from_millis(3)),
        outcome: None,
        cache: None,
    });
    let finished = metrics.render(snapshot());
    assert!(finished.contains("crab_git_active_blame_producers 0"));
    assert!(finished.contains("crab_git_blame_producers_total 1"));
    assert!(finished.contains("crab_git_cache_verification_bytes_total 4096"));
}

#[tokio::test]
async fn completed_request_exports_bounded_full_body_metrics() {
    let metrics = Metrics::new().unwrap();
    let observation = metrics.start_request(&Method::GET).response(StatusCode::OK);
    let body = Body::new(ObservedBody::new(Body::from("response"), observation));
    assert!(
        metrics
            .render(snapshot())
            .contains("crab_server_active_sized_response_bytes{method=\"get\"} 8")
    );
    assert_eq!(
        body.collect().await.unwrap().to_bytes(),
        Bytes::from_static(b"response")
    );
    let rendered = metrics.render(snapshot());
    assert!(rendered.contains("crab_server_requests_total{method=\"get\",outcome=\"2xx\"} 1"));
    assert!(rendered.contains("crab_server_in_flight_requests{method=\"get\"} 0"));
    assert!(rendered.contains("crab_server_active_sized_response_bytes{method=\"get\"} 0"));
    assert!(rendered.contains("crab_server_request_duration_seconds_count{method=\"get\"} 1"));
}

#[test]
fn dropped_response_body_records_abort_and_releases_in_flight_gauge() {
    let metrics = Metrics::new().unwrap();
    let observation = metrics.start_request(&Method::GET).response(StatusCode::OK);
    drop(ObservedBody::new(Body::from("response"), observation));

    let rendered = metrics.render(snapshot());
    assert!(rendered.contains("crab_server_response_body_aborts_total{method=\"get\"} 1"));
    assert!(rendered.contains("crab_server_active_sized_response_bytes{method=\"get\"} 0"));
    assert!(rendered.contains("crab_server_in_flight_requests{method=\"get\"} 0"));
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
    assert!(rendered.contains("crab_server_response_body_errors_total{method=\"get\"} 1"));
    assert!(rendered.contains("crab_server_in_flight_requests{method=\"get\"} 0"));
}

#[test]
fn request_cancelled_before_response_is_counted_and_released() {
    let metrics = Metrics::new().unwrap();
    drop(metrics.start_request(&Method::PUT));

    let rendered = metrics.render(snapshot());
    assert!(
        rendered.contains("crab_server_requests_total{method=\"put\",outcome=\"cancelled\"} 1")
    );
    assert!(rendered.contains("crab_server_in_flight_requests{method=\"put\"} 0"));
}
