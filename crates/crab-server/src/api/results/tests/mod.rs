use super::*;
use crate::test_support::http::Harness;
use crab_workflow::ExperimentId;
use futures_util::FutureExt;
use reqwest::Method;
use serde_json::Value;

fn address(request: &ResultsRead) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("request", &serde_json::to_string(request).unwrap())
        .finish();
    format!("/api/repos/team/private/results?{query}")
}

#[tokio::test]
async fn frozen_reports_share_repository_authorization_without_requiring_workflow_yaml() {
    report_authorization(Harness::new(false).await).await;
}

#[tokio::test]
#[ignore = "requires an owned S3-compatible Results qualification bucket"]
async fn real_storage_reports_follow_current_http_repository_grants() {
    let bucket = std::env::var("CRAB_SDK_TEST_S3_BUCKET").expect("owned fixture bucket");
    let store = object_store::aws::AmazonS3Builder::from_env()
        .with_bucket_name(bucket)
        .with_allow_http(true)
        .build()
        .unwrap();
    let h = Harness::new_with_store(false, false, crab_storage::Store::new(Arc::new(store))).await;
    h.initialize_directory().await;
    report_authorization(h).await;
}

async fn report_authorization(h: Harness) {
    let outcome = std::panic::AssertUnwindSafe(check_authorization(&h))
        .catch_unwind()
        .await;
    h.close().await;
    if let Err(error) = outcome {
        std::panic::resume_unwind(error);
    }
}

async fn check_authorization(h: &Harness) {
    let repository = h
        .server
        .repositories
        .get(&("team".into(), "private".into()))
        .unwrap();
    let remote = RemoteResults::new(
        WorkflowStore::from_storage(repository.store.clone()),
        repository.layout.repo_prefix(),
        CancellationToken::new(),
    );
    let directory = tempfile::tempdir().unwrap();
    let local = LocalResults::new(directory.path()).unwrap();
    let mut runs = Vec::new();
    for score in [0.1, 0.2] {
        let snapshot = ResultSnapshot {
            schema_version: RESULTS_SCHEMA_VERSION,
            origin: ResultOrigin::Captured,
            experiment_id: ExperimentId::new_v7().to_string(),
            source_metadata_hash: blake3::hash(b"metadata").to_hex().to_string(),
            base_commit: "a".repeat(40),
            name: None,
            status: "success".into(),
            started_at: "2026-11-17T00:00:00Z".into(),
            ended_at: Some("2026-11-17T00:00:01Z".into()),
            parameters: Vec::new(),
            metrics: flatten_scalars("metrics.json", &json!({"score":score})).unwrap(),
            stages: Vec::new(),
            attachments: Vec::new(),
            plots: Vec::new(),
            reproduction: None,
            capture_issues: Vec::new(),
        }
        .seal()
        .unwrap();
        local.install(&snapshot).unwrap();
        remote.push(&local, &snapshot).await.unwrap();
        runs.push(snapshot);
    }
    let request = ResultsRead::List(RunQuery::default());
    let response = h
        .http
        .get(format!("{}{}", h.origin, address(&request)))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let alice = h.login().await;
    let page = h.json(&address(&request), &alice).await;
    assert_eq!(page["data"]["total"], 2);
    let draft = ReportDraft {
        title: "Validation report".into(),
        runs: runs
            .iter()
            .map(|run| RunSelection {
                id: run.snapshot.experiment_id.clone(),
                digest: run.digest.clone(),
            })
            .collect(),
        spec: ComparisonSpec {
            baseline: runs[0].snapshot.experiment_id.clone(),
            ..Default::default()
        },
    };
    let response = h
        .write(
            Method::POST,
            "/api/repos/team/private/results/reports",
            &alice,
            serde_json::to_value(&draft).unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let id = response.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let view = h
        .json(&address(&ResultsRead::Report { id: id.clone() }), &bob)
        .await;
    assert_eq!(
        view["data"]["report"]["comparison"]["rows"][1]["run"]["snapshot"]["metrics"][0]["value"],
        0.2
    );
    let denied = h
        .write(
            Method::POST,
            "/api/repos/team/private/results/reports",
            &bob,
            serde_json::to_value(draft).unwrap(),
        )
        .await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    // A different immutable ID cannot be served from a report response cache.
    let absent = h
        .http
        .get(format!(
            "{}{}",
            h.origin,
            address(&ResultsRead::Report { id: "f".repeat(64) })
        ))
        .header("cookie", &bob)
        .send()
        .await
        .unwrap();
    assert_eq!(absent.status(), StatusCode::NOT_FOUND);
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    let denied = h
        .http
        .get(format!(
            "{}{}",
            h.origin,
            address(&ResultsRead::Report { id })
        ))
        .header("cookie", outsider)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
}
