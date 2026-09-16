use super::*;
use axum::{Router, routing::get};

#[test]
fn target_parser_binds_name_concurrency_and_origin_path() {
    let target = "readme=16@/api/repos/team/repo/contents/README.md?rev=main"
        .parse::<TargetSpec>()
        .unwrap();
    assert_eq!(target.name, "readme");
    assert_eq!(target.concurrency, 16);
    assert_eq!(
        target.path,
        "/api/repos/team/repo/contents/README.md?rev=main"
    );
    for invalid in [
        "readme",
        "readme=0@/api/repos/team/repo",
        "readme=1@https://other.example/repo",
        "readme=1@//other.example/repo",
        "readme=1@/repo#fragment",
    ] {
        assert!(invalid.parse::<TargetSpec>().is_err(), "accepted {invalid}");
    }
}

#[test]
fn histogram_merges_bounded_percentiles() {
    let mut left = Histogram::default();
    for millis in 1..=99 {
        left.record(Duration::from_millis(millis));
    }
    let mut right = Histogram::default();
    right.record(Duration::from_secs(61));
    left.merge(right);
    let summary = left.summary();
    assert_eq!(summary.p50_ms, Some(50));
    assert_eq!(summary.p95_ms, Some(95));
    assert_eq!(summary.p99_ms, Some(99));
    assert_eq!(summary.max_ms, Some(61_000));
    assert_eq!(summary.over_60s, 1);
}

#[test]
fn duplicate_targets_and_unsafe_origins_fail_validation() {
    let target = "refs=1@/api/repos/team/repo/refs"
        .parse::<TargetSpec>()
        .unwrap();
    assert!(validate_targets(&[target.clone(), target]).is_err());
    assert!(validate_origin(&Url::parse("https://secret@example.com/").unwrap()).is_err());
    assert!(validate_origin(&Url::parse("https://example.com/prefix").unwrap()).is_err());
    assert!(validate_origin(&Url::parse("https://example.com/").unwrap()).is_ok());
}

#[tokio::test]
async fn load_runner_counts_success_and_admission_without_false_failure() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/ok", get(|| async { "ok" })).route(
                "/busy",
                get(|| async { axum::http::StatusCode::TOO_MANY_REQUESTS }),
            ),
        )
        .await
    });
    let origin = Url::parse(&format!("http://{address}/")).unwrap();
    let targets = [
        TargetSpec::from_str("ok=1@/ok").unwrap(),
        TargetSpec::from_str("busy=1@/busy").unwrap(),
    ]
    .into_iter()
    .map(|spec| ResolvedTarget {
        url: origin.join(&spec.path).unwrap(),
        spec,
    })
    .collect::<Vec<_>>();
    let stats = run_load(
        Client::new(),
        &targets,
        Duration::ZERO,
        Duration::from_millis(50),
        1024,
    )
    .await
    .unwrap();

    assert!(stats[0].successful_responses > 0);
    assert_eq!(stats[0].admission_rejections, 0);
    assert!(stats[1].admission_rejections > 0);
    assert_eq!(stats[1].successful_responses, 0);
    assert!(stats.iter().all(WorkerStats::qualified));
    server.abort();
}

#[tokio::test]
async fn load_runner_rejects_server_errors_and_oversized_bodies() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route(
                    "/failure",
                    get(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
                )
                .route("/large", get(|| async { "too large" })),
        )
        .await
    });
    let origin = Url::parse(&format!("http://{address}/")).unwrap();
    let targets = [
        TargetSpec::from_str("failure=1@/failure").unwrap(),
        TargetSpec::from_str("large=1@/large").unwrap(),
    ]
    .into_iter()
    .map(|spec| ResolvedTarget {
        url: origin.join(&spec.path).unwrap(),
        spec,
    })
    .collect::<Vec<_>>();
    let stats = run_load(
        Client::new(),
        &targets,
        Duration::ZERO,
        Duration::from_millis(50),
        2,
    )
    .await
    .unwrap();

    assert!(stats[0].server_errors > 0);
    assert!(!stats[0].qualified());
    assert!(stats[1].body_limit_errors > 0);
    assert!(!stats[1].qualified());
    server.abort();
}
