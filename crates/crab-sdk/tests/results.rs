#![cfg(feature = "results")]

use crab_metadata::capsule_protocol::{RepositoryRoot, RootRecord, create_root};
use crab_sdk::operation::{Cancellation, Options, ReadLimits};
use crab_sdk::results::{ComparisonSpec, Report, ResultsRead, ResultsResponse, RunQuery};
use crab_sdk::storage::{AzureOptions, DirectStoreOptions, GcsOptions};
use crab_sdk::{Client, ErrorKind, OpenOptions, RepositoryLocator};
use crab_storage::{Store, StoreLayout};
use crab_workflow::{ExperimentId, WorkflowStore, results::*};
use futures_util::TryStreamExt;
use object_store::ObjectStoreExt;
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn bare_results_use_captured_storage_and_preserve_operation_limits() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    let storage = directory.path().join("storage");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&storage).unwrap();
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let layout = StoreLayout::new(store.clone(), "repository".into());
    let root =
        RootRecord::encode(RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap())
            .unwrap();
    create_root(&layout, root).await.unwrap();
    let local = LocalResults::new(&source).unwrap();
    let (run, report) = retained_observation(&local);
    let remote = RemoteResults::new(
        WorkflowStore::from_storage(store.clone()),
        "repository",
        CancellationToken::new(),
    );
    remote.push(&local, &run).await.unwrap();
    let id = remote.push_report(&local, &report).await.unwrap();
    // A filesystem-backed read fixture contains only published objects. No Git
    // checkout, editable YAML, source metadata or local Results state survives.
    for object in store
        .inner()
        .list(None)
        .try_collect::<Vec<_>>()
        .await
        .unwrap()
    {
        let key = object.location.as_ref();
        if key != layout.capsule_root_path().as_ref()
            && !key.starts_with("repository/workflow/results/")
            && !key.starts_with("repository/refs/crab/results")
        {
            continue;
        }
        let path = storage.join(key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            store
                .inner()
                .get(&object.location)
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap(),
        )
        .unwrap();
    }
    std::fs::remove_dir_all(&source).unwrap();
    let client = Client::builder()
        .direct_store(DirectStoreOptions::filesystem(&storage).unwrap())
        .build()
        .unwrap();
    let repository = client
        .open(OpenOptions::remote(
            RepositoryLocator::new("repository").unwrap(),
        ))
        .await
        .unwrap();
    let remote = repository.remote().unwrap();
    assert!(
        remote
            .capabilities()
            .contains(&crab_sdk::remote::Capability::ReadResults)
    );
    let results = remote.results();
    let selection = RunSelection {
        id: run.snapshot.experiment_id.clone(),
        digest: run.digest.clone(),
    };
    let plot_budget = Options::default()
        .with_limits(ReadLimits {
            max_storage_requests: 1,
            ..Default::default()
        })
        .unwrap();
    assert!(matches!(
        results
            .read(ResultsRead::Run(selection.clone()))
            .with_options(plot_budget.clone())
            .await
            .unwrap(),
        ResultsResponse::Run(_)
    ));
    assert_eq!(
        results
            .read(ResultsRead::Compare {
                runs: vec![selection],
                spec: ComparisonSpec {
                    baseline: run.snapshot.experiment_id.clone(),
                    ..Default::default()
                },
            })
            .with_options(plot_budget)
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::LimitExceeded,
    );
    let response = results
        .read(ResultsRead::Report { id: id.clone() })
        .await
        .unwrap();
    let ResultsResponse::Report(view) = response else {
        panic!("expected immutable report");
    };
    assert_eq!(view.report, report);
    let tiny = Options::default()
        .with_limits(ReadLimits {
            max_response_bytes: 1,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        results
            .read(ResultsRead::Report { id: id.clone() })
            .with_options(tiny)
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::LimitExceeded
    );
    let tiny = Options::default()
        .with_limits(ReadLimits {
            max_storage_requests: 1,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        results
            .read(ResultsRead::List(RunQuery::default()))
            .with_options(tiny)
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::LimitExceeded
    );
    let cancel = Cancellation::default();
    cancel.cancel();
    assert_eq!(
        results
            .read(ResultsRead::Report { id })
            .with_options(Options::default().with_cancellation(cancel))
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::Cancelled
    );
}

const CURVE: &[u8] = b"step,loss\n0,1\n1,2\n";

fn retained_observation(local: &LocalResults) -> (SealedResult, Report) {
    let run = ResultSnapshot {
        schema_version: RESULTS_SCHEMA_VERSION,
        origin: ResultOrigin::Captured,
        experiment_id: ExperimentId::new_v7().to_string(),
        source_metadata_hash: "a".repeat(64),
        base_commit: "b".repeat(40),
        name: Some("retained".into()),
        status: "success".into(),
        started_at: "2026-11-18T00:00:00Z".into(),
        ended_at: None,
        parameters: Vec::new(),
        metrics: flatten_scalars("metrics.json", &json!({"score":0.25,"unknown":null})).unwrap(),
        stages: Vec::new(),
        attachments: vec![Attachment {
            path: "curve.csv".into(),
            kind: AttachmentKind::Plot,
            stage: None,
            hash: Some(local.put_blob(CURVE).unwrap()),
            size: CURVE.len() as u64,
            state: Availability::Present,
            reason: None,
            push: true,
            remote: None,
        }],
        plots: vec![
            serde_json::from_value(json!({"path":"curve.csv","x":"step","y":["loss"]})).unwrap(),
        ],
        reproduction: None,
        capture_issues: Vec::new(),
    }
    .seal()
    .unwrap();
    local.install(&run).unwrap();
    let report = Report::new(
        "Retained comparison".into(),
        compare(
            std::slice::from_ref(&run),
            &ComparisonSpec {
                baseline: run.snapshot.experiment_id.clone(),
                ..Default::default()
            },
        )
        .unwrap(),
        charts(std::slice::from_ref(&run), |hash| local.blob(hash)).unwrap(),
    )
    .unwrap();
    (run, report)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires CRAB_SDK_TEST_S3_BUCKET and credentials for a dedicated test bucket"]
async fn live_s3_results_survive_source_removal_and_sdk_reads_are_read_only() {
    let bucket = std::env::var("CRAB_SDK_TEST_S3_BUCKET").unwrap();
    let store =
        crab_storage::build_static_env_store(&bucket, crab_storage::StorageProviderKind::S3)
            .unwrap();
    provider_results_round_trip(store, DirectStoreOptions::s3_from_env(&bucket).unwrap()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a dedicated GCS bucket and explicit bearer token"]
async fn live_gcs_results_survive_source_removal_and_sdk_reads_are_read_only() {
    let bucket = std::env::var("CRAB_SDK_TEST_GCS_BUCKET").unwrap();
    let token = std::env::var("CRAB_SDK_TEST_GCS_ACCESS_TOKEN").unwrap();
    let store = crab_storage::build_explicit_store(
        &bucket,
        crab_storage::ObjectStoreCredentials::Gcp {
            access_token: token.clone(),
        },
        None,
        false,
    )
    .unwrap();
    let options = DirectStoreOptions::gcs(GcsOptions::new(&bucket, &token).unwrap());
    provider_results_round_trip(store, options).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a dedicated Azure container and explicit bearer token"]
async fn live_azure_results_survive_source_removal_and_sdk_reads_are_read_only() {
    let account = std::env::var("CRAB_SDK_TEST_AZURE_ACCOUNT").unwrap();
    let container = std::env::var("CRAB_SDK_TEST_AZURE_CONTAINER").unwrap();
    let token = std::env::var("CRAB_SDK_TEST_AZURE_BEARER_TOKEN").unwrap();
    let store = crab_storage::build_explicit_store(
        &container,
        crab_storage::ObjectStoreCredentials::Azure {
            account: account.clone(),
            token: crab_storage::AzureAuthorization::Bearer(token.clone()),
        },
        None,
        false,
    )
    .unwrap();
    let options =
        DirectStoreOptions::azure(AzureOptions::bearer(&account, &container, &token).unwrap());
    provider_results_round_trip(store, options).await;
}

async fn provider_results_round_trip(store: Store, options: DirectStoreOptions) {
    let prefix = format!("qualification/results-{}", ExperimentId::new_v7());
    let layout = StoreLayout::new(store.clone(), prefix.clone());
    create_root(
        &layout,
        RootRecord::encode(RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap())
            .unwrap(),
    )
    .await
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let local = LocalResults::new(&source).unwrap();
    let (run, report) = retained_observation(&local);
    let remote = RemoteResults::new(
        WorkflowStore::from_storage(store.clone()),
        &prefix,
        CancellationToken::new(),
    );
    remote.push(&local, &run).await.unwrap();
    let id = remote.push_report(&local, &report).await.unwrap();
    // Retry the canonical publication protocol, then remove every source cache.
    // A cold pull must verify provider objects rather than reuse local evidence.
    remote.push(&local, &run).await.unwrap();
    assert_eq!(remote.push_report(&local, &report).await.unwrap(), id);
    std::fs::remove_dir_all(&source).unwrap();
    let cold = directory.path().join("cold");
    std::fs::create_dir(&cold).unwrap();
    let pulled = LocalResults::new(&cold).unwrap();
    assert_eq!(
        remote
            .pull(&pulled, &run.snapshot.experiment_id)
            .await
            .unwrap(),
        run
    );
    let hash = run.snapshot.attachments[0].hash.as_ref().unwrap();
    assert_eq!(pulled.blob(hash).unwrap(), CURVE);
    std::fs::remove_dir_all(&cold).unwrap();
    remote
        .remove_run(&run.snapshot.experiment_id)
        .await
        .unwrap();
    assert!(remote.list().await.unwrap().is_empty());
    std::fs::create_dir(&cold).unwrap();
    let pulled = LocalResults::new(&cold).unwrap();
    assert_eq!(remote.pull_report(&pulled, &id).await.unwrap(), report);
    assert_eq!(pulled.blob(hash).unwrap(), CURVE);

    let path = object_store::path::Path::from(prefix.clone());
    let inventory = || async {
        let mut objects = store
            .inner()
            .list(Some(&path))
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        objects.sort_by(|a, b| a.location.cmp(&b.location));
        objects
            .into_iter()
            .map(|object| (object.location, object.size, object.e_tag))
            .collect::<Vec<_>>()
    };
    let before = inventory().await;
    let client = Client::builder().direct_store(options).build().unwrap();
    let repository = client
        .open(OpenOptions::remote(
            RepositoryLocator::new(&prefix).unwrap(),
        ))
        .await
        .unwrap();
    let remote = repository.remote().unwrap();
    assert!(
        remote
            .capabilities()
            .contains(&crab_sdk::remote::Capability::ReadResults)
    );
    let results = remote.results();
    let ResultsResponse::Reports(reports) = results.read(ResultsRead::Reports).await.unwrap()
    else {
        panic!("expected report discovery");
    };
    assert_eq!(reports, vec![id.clone()]);
    let response = results
        .read(ResultsRead::Report { id: id.clone() })
        .await
        .unwrap();
    let ResultsResponse::Report(view) = response else {
        panic!("expected immutable report");
    };
    assert_eq!(view.report, report);
    assert_eq!(results.blob(hash.clone()).await.unwrap(), CURVE);
    client.close().await.unwrap();
    let after = inventory().await;
    // Limit cleanup to the unique repository this test created. Never clear
    // the provider bucket/container shared by other qualification runs.
    for (location, _, _) in &after {
        store.delete(location).await.unwrap();
    }
    assert_eq!(before, after);
}
