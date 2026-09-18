use super::*;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use std::{collections::HashSet, fs, path::PathBuf, str::FromStr, sync::Mutex};

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
fn mutation_parser_and_template_require_dynamic_request_ids() {
    let mutation = "issue=4@/api/repos/team/repo/issues|request.json"
        .parse::<MutationSpec>()
        .unwrap();
    assert_eq!(mutation.target.name, "issue");
    assert_eq!(mutation.target.concurrency, 4);
    assert_eq!(mutation.body_file, PathBuf::from("request.json"));

    let mut file = tempfile::NamedTempFile::new().unwrap();
    writeln!(
        file,
        r#"{{"request_id":"{{{{request_id}}}}","title":"load"}}"#
    )
    .unwrap();
    assert!(load_mutation_template(file.path()).is_ok());
    fs::write(file.path(), r#"{"request_id":"fixed"}"#).unwrap();
    assert!(load_mutation_template(file.path()).is_err());
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

#[test]
fn target_validation_supports_a_hundred_repository_fleet() {
    let targets = (0..100)
        .map(|index| {
            format!("repo-{index}=1@/api/repos/github/repo-{index}/refs")
                .parse::<TargetSpec>()
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert!(validate_targets(&targets).is_ok());

    let oversized = (0..257)
        .map(|index| {
            format!("repo-{index}=1@/api/repos/github/repo-{index}/refs")
                .parse::<TargetSpec>()
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert!(validate_targets(&oversized).is_err());
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
        method: LoadMethod::Get,
        body_template: None,
    })
    .collect::<Vec<_>>();
    let stats = run_load(
        Client::new(),
        &targets,
        Duration::ZERO,
        Duration::from_millis(50),
        1024,
        None,
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
        method: LoadMethod::Get,
        body_template: None,
    })
    .collect::<Vec<_>>();
    let stats = run_load(
        Client::new(),
        &targets,
        Duration::ZERO,
        Duration::from_millis(50),
        2,
        None,
    )
    .await
    .unwrap();

    assert!(stats[0].server_errors > 0);
    assert!(!stats[0].qualified());
    assert!(stats[1].body_limit_errors > 0);
    assert!(!stats[1].qualified());
    server.abort();
}

#[tokio::test]
async fn mutation_load_runner_sends_unique_request_ids() {
    async fn mutate(
        State(request_ids): State<Arc<Mutex<HashSet<String>>>>,
        Json(body): Json<serde_json::Value>,
    ) -> StatusCode {
        let Some(request_id) = body.get("request_id").and_then(|value| value.as_str()) else {
            return StatusCode::BAD_REQUEST;
        };
        if request_ids.lock().unwrap().insert(request_id.to_owned()) {
            StatusCode::CREATED
        } else {
            StatusCode::CONFLICT
        }
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let request_ids = Arc::new(Mutex::new(HashSet::new()));
    let server_ids = Arc::clone(&request_ids);
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/mutate", post(mutate))
                .with_state(server_ids),
        )
        .await
    });
    let origin = Url::parse(&format!("http://{address}/")).unwrap();
    let spec = TargetSpec::from_str("mutate=2@/mutate").unwrap();
    let target = ResolvedTarget {
        url: origin.join(&spec.path).unwrap(),
        spec,
        method: LoadMethod::Post,
        body_template: Some(Arc::from(
            r#"{"request_id":"{{request_id}}","value":"load"}"#,
        )),
    };
    let stats = run_load(
        Client::new(),
        &[target],
        Duration::ZERO,
        Duration::from_millis(50),
        1024,
        None,
    )
    .await
    .unwrap();

    assert!(stats[0].successful_responses > 1);
    assert_eq!(
        request_ids.lock().unwrap().len() as u64,
        stats[0].successful_responses
    );
    assert!(stats[0].qualified());
    server.abort();
}

#[test]
fn direct_node_authority_is_explicit_and_bounded() {
    assert_eq!(
        load_authority(Some("git.example.com"))
            .unwrap()
            .unwrap()
            .as_bytes(),
        b"git.example.com"
    );
    assert!(load_authority(None).unwrap().is_none());
    for authority in ["", "git.example.com:8788", "user@git.example.com", "/path"] {
        assert!(load_authority(Some(authority)).is_err(), "{authority}");
    }
}

#[tokio::test]
async fn aggregate_rate_is_shared_across_workers_and_targets() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/first", get(|| async { "first" }))
                .route("/second", get(|| async { "second" })),
        )
        .await
    });
    let origin = Url::parse(&format!("http://{address}/")).unwrap();
    let targets = [
        TargetSpec::from_str("first=2@/first").unwrap(),
        TargetSpec::from_str("second=2@/second").unwrap(),
    ]
    .into_iter()
    .map(|spec| ResolvedTarget {
        url: origin.join(&spec.path).unwrap(),
        spec,
        method: LoadMethod::Get,
        body_template: None,
    })
    .collect::<Vec<_>>();

    let started = Instant::now();
    let stats = run_load(
        Client::new(),
        &targets,
        Duration::ZERO,
        Duration::from_millis(250),
        1024,
        Some(20),
    )
    .await
    .unwrap();
    let responses = stats.iter().map(|stats| stats.responses).sum::<u64>();

    assert!((4..=6).contains(&responses));
    assert!(stats.iter().all(|stats| stats.responses > 0));
    assert!(started.elapsed() >= Duration::from_millis(240));
    assert_eq!(minimum_successful_responses(1_000, 60), 57_000);
    assert_eq!(target_rate_qualified(56_999, Some(1_000), 60), Some(false));
    assert_eq!(target_rate_qualified(57_000, Some(1_000), 60), Some(true));
    server.abort();
}
