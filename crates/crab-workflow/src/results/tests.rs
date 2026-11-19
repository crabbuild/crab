use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::{ExperimentId, WorkflowStore};

mod chart_behavior;
mod matrix_behavior;
mod template_availability;
mod template_parity;

fn run(value: serde_json::Value) -> SealedResult {
    ResultSnapshot {
        schema_version: RESULTS_SCHEMA_VERSION,
        origin: ResultOrigin::Captured,
        experiment_id: ExperimentId::new_v7().to_string(),
        source_metadata_hash: digest(b"metadata"),
        base_commit: "0123456789012345678901234567890123456789".into(),
        name: None,
        status: "success".into(),
        started_at: "2026-11-17T00:00:00Z".into(),
        ended_at: Some("2026-11-17T00:00:01Z".into()),
        parameters: Vec::new(),
        metrics: flatten_scalars("metrics.json", &value).unwrap(),
        stages: vec![StageResult {
            name: "evaluate".into(),
            hash: None,
            state: "Committed".into(),
            duration_ms: Some(1),
            attempts: 1,
            source: "executed".into(),
            inputs: vec![InputWitness {
                path: Some("dataset.csv".into()),
                hash: digest(b"dataset"),
            }],
            outputs: Vec::new(),
        }],
        attachments: Vec::new(),
        plots: Vec::new(),
        reproduction: None,
        capture_issues: Vec::new(),
    }
    .seal()
    .unwrap()
}

fn spec(baseline: &SealedResult) -> ComparisonSpec {
    ComparisonSpec {
        baseline: baseline.snapshot.experiment_id.clone(),
        ..Default::default()
    }
}

#[test]
fn canonical_bytes_ignore_struct_and_nested_object_insertion_order() {
    #[derive(serde::Serialize)]
    struct Document {
        z: serde_json::Value,
        a: u8,
    }
    let document = Document {
        z: serde_json::from_str(r#"[{"z":1,"a":{"z":2,"a":3}}]"#).unwrap(),
        a: 0,
    };
    assert_eq!(
        encode(&document).unwrap(),
        br#"{"a":0,"z":[{"a":{"a":3,"z":2},"z":1}]}"#
    );
}

#[test]
fn reproduction_trees_reject_unsafe_or_ambiguous_directory_identities() {
    let spec = ReproductionSpec {
        source_commit: "0".repeat(40),
        source_result: None,
        files: vec![Attachment {
            path: "input.bin".into(),
            kind: AttachmentKind::Input,
            stage: None,
            hash: Some(digest(b"input")),
            size: 5,
            state: Availability::Present,
            reason: None,
            push: true,
            remote: None,
        }],
        directories: vec!["state".into(), "state/empty".into()],
        runtimes: Vec::new(),
        targets: Vec::new(),
        recursive: false,
        single_item: false,
        downstream: false,
        pipeline: false,
        all_pipelines: false,
        glob: false,
        unavailable_reasons: Vec::new(),
    };
    assert!(spec.validate_inputs().is_ok());
    for directories in [
        vec!["../outside".into()],
        vec![".crab/private".into()],
        vec!["/outside".into()],
        vec!["state".into(), "state".into()],
        vec!["z".into(), "a".into()],
        vec!["input.bin".into()],
        vec!["input.bin/child".into()],
        (0..=MAX_REPRODUCTION_FILES)
            .map(|i| format!("dir{i:04}"))
            .collect(),
    ] {
        let mut invalid = spec.clone();
        invalid.directories = directories;
        assert!(
            invalid.validate_inputs().is_err(),
            "{:?}",
            invalid.directories
        );
    }
}

#[test]
fn reproduction_parent_is_a_distinct_canonical_content_address() {
    let parent = run(json!({"score": 1}));
    let mut child = run(json!({"score": 1})).snapshot;
    child.reproduction = Some(ReproductionSpec {
        source_commit: child.base_commit.clone(),
        source_result: Some(RunSelection {
            id: parent.snapshot.experiment_id,
            digest: parent.digest,
        }),
        files: Vec::new(),
        directories: Vec::new(),
        runtimes: Vec::new(),
        targets: Vec::new(),
        recursive: false,
        single_item: false,
        downstream: false,
        pipeline: false,
        all_pipelines: false,
        glob: false,
        unavailable_reasons: Vec::new(),
    });
    let sealed = child.clone().seal().unwrap();
    assert_eq!(sealed.snapshot.reproduction, child.reproduction);
    for source in [
        RunSelection {
            id: child.experiment_id.clone(),
            digest: "a".repeat(64),
        },
        RunSelection {
            id: "01A00000-0000-7000-8000-000000000001".into(),
            digest: "a".repeat(64),
        },
        RunSelection {
            id: ExperimentId::new_v7().to_string(),
            digest: "a".repeat(63),
        },
    ] {
        let mut invalid = child.clone();
        invalid.reproduction.as_mut().unwrap().source_result = Some(source);
        assert!(invalid.seal().is_err());
    }
}

#[test]
fn scalar_identity_preserves_literal_keys_and_nested_keys() {
    let fields = flatten_scalars(
        "metrics.json",
        &json!({"a.b": 1, "a": {"b": 2}, "a/b": 3, "~": null}),
    )
    .unwrap();
    let keys = fields
        .iter()
        .map(Datum::key)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(keys.len(), 4);
}

#[test]
fn comparison_columns_distinguish_automatic_empty_and_explicit() {
    let baseline = run(json!({"score": 1, "loss": 2}));
    let score = baseline
        .snapshot
        .metrics
        .iter()
        .find(|datum| datum.pointer == "/score")
        .unwrap()
        .key();
    for (columns, expected) in [(None, 2), (Some(Vec::new()), 0), (Some(vec![score]), 1)] {
        let specification = ComparisonSpec {
            columns,
            ..spec(&baseline)
        };
        assert_eq!(
            compare(std::slice::from_ref(&baseline), &specification)
                .unwrap()
                .rows[0]
                .cells
                .len(),
            expected
        );
    }
}

#[test]
fn unsafe_integers_are_explicitly_unsupported_instead_of_rounded() {
    for value in [json!(u64::MAX), json!(i64::MIN)] {
        let captured = flatten_scalars("params.json", &value).unwrap();
        assert_eq!(
            (captured[0].state, &captured[0].value),
            (Availability::Unsupported, &serde_json::Value::Null)
        );
    }
}

#[test]
fn snapshots_and_chart_specs_refuse_unsafe_wire_integers() {
    let mut snapshot = run(json!({"score": 1})).snapshot;
    snapshot.metrics[0].value = json!(u64::MAX);
    assert!(snapshot.seal().is_err());
    assert!(validate_inline_spec(&json!({"data":{"values":[{"step":u64::MAX}]}})).is_err());
}

#[test]
fn zero_baseline_keeps_absolute_delta_without_relative_delta() {
    let baseline = run(json!({"score": 0}));
    let candidate = run(json!({"score": 2}));
    let comparison = compare(&[baseline.clone(), candidate], &spec(&baseline)).unwrap();
    let cell = &comparison.rows[1].cells[0];
    assert_eq!(
        (cell.absolute_delta, cell.relative_delta, cell.improvement),
        (Some(2.0), None, None)
    );
}

#[test]
fn missing_metric_is_not_zero() {
    let baseline = run(json!({"score": 5}));
    let candidate = run(json!({}));
    let comparison = compare(&[baseline.clone(), candidate], &spec(&baseline)).unwrap();
    assert_eq!(comparison.rows[1].cells[0].absolute_delta, None);
}

#[test]
fn improvement_requires_direction_and_matching_dataset_split_evaluator() {
    let baseline = run(json!({"score": 5}));
    let candidate = run(json!({"score": 2}));
    let mut config = spec(&baseline);
    config
        .objectives
        .insert(baseline.snapshot.metrics[0].key(), Direction::Minimize);
    let identity = DatasetIdentity {
        hash: digest(b"dataset"),
        split: "validation".into(),
        evaluator: "eval-v1".into(),
    };
    config.datasets = BTreeMap::from([
        (baseline.snapshot.experiment_id.clone(), identity.clone()),
        (candidate.snapshot.experiment_id.clone(), identity),
    ]);
    let comparison = compare(&[baseline, candidate], &config).unwrap();
    assert_eq!(comparison.rows[1].cells[0].improvement, Some(true));
}

#[test]
fn sorting_precedes_paging_and_catalog_changes_reject_old_cursors() {
    let runs = vec![
        run(json!({"score": 2})),
        run(json!({"score": 9})),
        run(json!({"score": 1})),
    ];
    let mut request = RunQuery {
        limit: 1,
        sort: format!("metric:{}", runs[0].snapshot.metrics[0].key()),
        ..Default::default()
    };
    let page = query(&runs, &request).unwrap();
    assert_eq!(page.rows[0].metrics[0].value, json!(9));
    request.cursor = page.next_cursor;
    let mut changed = runs;
    changed.push(run(json!({"score": 10})));
    assert!(query(&changed, &request).is_err());
}

#[test]
fn report_survives_run_removal_and_local_republication() {
    let directory = tempfile::tempdir().unwrap();
    let local = LocalResults::new(directory.path()).unwrap();
    let mut captured = run(json!({"score": 3}));
    let bytes = b"step,loss\n0,3\n1,2\n";
    captured.snapshot.attachments.push(Attachment {
        path: "loss.csv".into(),
        kind: AttachmentKind::Plot,
        stage: None,
        hash: Some(local.put_blob(bytes).unwrap()),
        size: bytes.len() as u64,
        state: Availability::Present,
        reason: None,
        push: true,
        remote: None,
    });
    captured = captured.snapshot.seal().unwrap();
    local.install(&captured).unwrap();
    let report = Report {
        schema_version: RESULTS_SCHEMA_VERSION,
        renderer: RESULTS_RENDERER.into(),
        charts: Vec::new(),
        title: "Comparison".into(),
        comparison: compare(&[captured.clone()], &spec(&captured)).unwrap(),
        created_at: "2026-11-17".into(),
    };
    let id = local.save_report(&report).unwrap();
    local.remove_run(&captured.snapshot.experiment_id).unwrap();
    assert_eq!(local.report(&id).unwrap(), report);
}

#[test]
fn install_refuses_corrupt_or_missing_evidence_before_publishing_root() {
    let directory = tempfile::tempdir().unwrap();
    let local = LocalResults::new(directory.path()).unwrap();
    let mut captured = run(json!({"score": 3})).snapshot;
    captured.attachments.push(Attachment {
        path: "loss.csv".into(),
        kind: AttachmentKind::Plot,
        stage: None,
        hash: Some(digest(b"not installed")),
        size: 13,
        state: Availability::Present,
        reason: None,
        push: true,
        remote: None,
    });
    let captured = captured.seal().unwrap();
    assert!(local.install(&captured).is_err());
    assert!(local.list().unwrap().is_empty());
}

#[tokio::test]
async fn remote_report_round_trip_uses_independent_verified_objects() {
    let source = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let local = LocalResults::new(source.path()).unwrap();
    let cold = LocalResults::new(target.path()).unwrap();
    let remote = RemoteResults::new(
        WorkflowStore::new(Arc::new(object_store::memory::InMemory::new())),
        "org/repo",
        CancellationToken::new(),
    );
    let mut snapshot = run(json!({"score": 3})).snapshot;
    snapshot.plots =
        vec![serde_json::from_value(json!({"path":"curve.csv","x":"step","y":["loss"]})).unwrap()];
    let mut model_hash = String::new();
    for (path, kind, body, push) in [
        (
            "curve.csv",
            AttachmentKind::Plot,
            b"step,loss\n0,3\n1,2\n".as_slice(),
            true,
        ),
        (
            "model.bin",
            AttachmentKind::Output,
            b"private model".as_slice(),
            false,
        ),
    ] {
        let hash = local.put_blob(body).unwrap();
        if kind == AttachmentKind::Output {
            model_hash = hash.clone();
        }
        snapshot.attachments.push(Attachment {
            path: path.into(),
            kind,
            stage: None,
            hash: Some(hash),
            size: body.len() as u64,
            state: Availability::Present,
            reason: None,
            push,
            remote: None,
        });
    }
    let captured = snapshot.seal().unwrap();
    local.install(&captured).unwrap();
    let charts = charts(&[captured.clone()], |hash| local.blob(hash)).unwrap();
    let mut specification = spec(&captured);
    specification.axes.insert(
        charts[0].interaction.as_ref().unwrap().key.clone(),
        AxisRange {
            lower: 0.25,
            upper: 0.75,
        },
    );
    let report = Report::new(
        "Comparison".into(),
        compare(&[captured.clone()], &specification).unwrap(),
        charts,
    )
    .unwrap();
    let id = remote.push_report(&local, &report).await.unwrap();
    remote.pull_report(&cold, &id).await.unwrap();
    local.remove_run(&captured.snapshot.experiment_id).unwrap();
    assert_eq!(cold.report(&id).unwrap(), report);
    assert!(cold.blob(&model_hash).is_err());
    let html = report_html(&report, |hash| cold.blob(hash)).unwrap();
    assert!(html.contains("0.75"));
    let response = ResultsReader::Local(cold)
        .read(ResultsRead::Evidence {
            run: RunSelection {
                id: captured.snapshot.experiment_id,
                digest: captured.digest,
            },
            path: "model.bin".into(),
            report: Some(id),
        })
        .await
        .unwrap();
    let ResultsResponse::Evidence(view) = response else {
        panic!("expected immutable link");
    };
    assert!(view.reason.unwrap().contains("immutable link"));
    assert!(view.text.is_none());
}

#[tokio::test]
async fn cancellation_is_not_an_empty_catalog() {
    let token = CancellationToken::new();
    token.cancel();
    let remote = RemoteResults::new(
        WorkflowStore::new(Arc::new(object_store::memory::InMemory::new())),
        "org/repo",
        token,
    );
    assert!(matches!(
        remote.run(&ExperimentId::new_v7().to_string()).await,
        Err(crate::WorkflowError::Cancelled)
    ));
}

#[tokio::test]
async fn interrupted_report_transfers_publish_no_partial_roots_and_release_gc_admission() {
    use futures_util::TryStreamExt;
    use object_store::{
        ObjectStore, ObjectStoreExt,
        path::Path,
        throttle::{ThrottleConfig, ThrottledStore},
    };
    use std::time::Duration;
    let source = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let local = LocalResults::new(source.path()).unwrap();
    let cold = LocalResults::new(target.path()).unwrap();
    let memory = Arc::new(object_store::memory::InMemory::new());
    let raw: Arc<dyn ObjectStore> = memory.clone();
    let slow = Arc::new(ThrottledStore::new(
        memory.clone(),
        ThrottleConfig {
            wait_put_per_call: Duration::from_millis(200),
            ..Default::default()
        },
    ));
    let mut snapshot = run(json!({"score":1})).snapshot;
    for text in ["first", "second", "third"] {
        snapshot.attachments.push(Attachment {
            path: format!("{text}.txt"),
            kind: AttachmentKind::Log,
            stage: None,
            hash: Some(local.put_blob(text.as_bytes()).unwrap()),
            size: text.len() as u64,
            state: Availability::Present,
            reason: None,
            push: true,
            remote: None,
        });
    }
    let run = snapshot.seal().unwrap();
    let report = Report::new(
        "Interrupt transfers".into(),
        compare(
            &[run.clone()],
            &ComparisonSpec {
                include_logs: true,
                ..spec(&run)
            },
        )
        .unwrap(),
        Vec::new(),
    )
    .unwrap();
    let id = report.id().unwrap();
    let prefix = "org/repo";
    let layout =
        crab_storage::StoreLayout::new(crab_storage::Store::new(raw.clone()), prefix.into());
    // Initialize lease ledgers without throttling; the observed cancellation
    // point must be evidence transfer, not first-time fence creation.
    WorkflowStore::new(raw.clone())
        .acquire_gc_writer(prefix)
        .await
        .unwrap()
        .release()
        .await
        .unwrap();
    let cancellation = CancellationToken::new();
    let remote = RemoteResults::new(WorkflowStore::new(slow), prefix, cancellation.clone());
    let uploading_local = local.clone();
    let uploading_report = report.clone();
    let upload = tokio::spawn(async move {
        remote
            .push_report(&uploading_local, &uploading_report)
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while memory
            .list(Some(&Path::from(format!(
                "{prefix}/workflow/results/objects/"
            ))))
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        crab_coordination::GcFenceLease::acquire_sweep(
            &raw,
            layout.repo_prefix(),
            Duration::from_secs(30)
        )
        .await
        .is_err()
    );
    cancellation.cancel();
    assert!(matches!(
        upload.await.unwrap(),
        Err(crate::WorkflowError::Cancelled)
    ));
    assert!(matches!(
        memory
            .head(&Path::from(format!(
                "{prefix}/refs/crab/results-reports/{id}"
            )))
            .await,
        Err(object_store::Error::NotFound { .. })
    ));
    for domain in [layout.repo_prefix(), layout.global_prefix()] {
        crab_coordination::GcFenceLease::acquire_sweep(&raw, domain, Duration::from_secs(30))
            .await
            .unwrap()
            .release()
            .await
            .unwrap();
    }
    let remote = RemoteResults::new(
        WorkflowStore::new(raw.clone()),
        prefix,
        CancellationToken::new(),
    );
    remote.push_report(&local, &report).await.unwrap();
    let slow = Arc::new(ThrottledStore::new(
        memory,
        ThrottleConfig {
            wait_get_per_call: Duration::from_millis(200),
            ..Default::default()
        },
    ));
    let cancellation = CancellationToken::new();
    let remote = RemoteResults::new(WorkflowStore::new(slow), prefix, cancellation.clone());
    let receiving = cold.clone();
    let requested = id.clone();
    let download = tokio::spawn(async move { remote.pull_report(&receiving, &requested).await });
    tokio::time::timeout(Duration::from_secs(10), async {
        while !run
            .snapshot
            .attachments
            .iter()
            .any(|attachment| cold.blob(attachment.hash.as_ref().unwrap()).is_ok())
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    cancellation.cancel();
    assert!(matches!(
        download.await.unwrap(),
        Err(crate::WorkflowError::Cancelled)
    ));
    assert!(cold.reports().unwrap().is_empty());
    let remote = RemoteResults::new(WorkflowStore::new(raw), prefix, CancellationToken::new());
    assert_eq!(remote.pull_report(&cold, &id).await.unwrap(), report);
}

#[test]
fn present_null_and_missing_metrics_round_trip_as_distinct_observations() {
    let directory = tempfile::tempdir().unwrap();
    let local = LocalResults::new(directory.path()).unwrap();
    let mut snapshot = run(json!({"score":null})).snapshot;
    snapshot.metrics.push(Datum {
        file: "missing.json".into(),
        pointer: "".into(),
        state: Availability::Missing,
        value: serde_json::Value::Null,
        reason: Some("not produced".into()),
    });
    let captured = snapshot.seal().unwrap();
    local.install(&captured).unwrap();
    assert_eq!(
        local.run(&captured.snapshot.experiment_id).unwrap(),
        captured
    );
}

#[test]
fn failed_run_keeps_raw_delta_without_claiming_an_improvement() {
    let baseline = run(json!({"score":5}));
    let mut candidate = run(json!({"score":2})).snapshot;
    candidate.status = "failed".into();
    let candidate = candidate.seal().unwrap();
    let mut config = spec(&baseline);
    config
        .objectives
        .insert(baseline.snapshot.metrics[0].key(), Direction::Minimize);
    let identity = DatasetIdentity {
        hash: digest(b"dataset"),
        split: "validation".into(),
        evaluator: "eval-v1".into(),
    };
    config
        .datasets
        .insert(baseline.snapshot.experiment_id.clone(), identity.clone());
    config
        .datasets
        .insert(candidate.snapshot.experiment_id.clone(), identity);
    let comparison = compare(&[baseline, candidate], &config).unwrap();
    let cell = &comparison.rows[1].cells[0];
    assert_eq!((cell.absolute_delta, cell.improvement), (Some(-3.0), None));
}

#[test]
fn large_and_external_inputs_do_not_prevent_retaining_observed_metrics() {
    let directory = tempfile::tempdir().unwrap();
    let work = directory.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let external = directory.path().join("external.csv");
    std::fs::write(&external, b"external input").unwrap();
    let large = std::fs::File::create(work.join("large.bin")).unwrap();
    large.set_len(MAX_REPRODUCTION_BYTES as u64 + 1).unwrap();
    std::fs::create_dir(work.join("dataset")).unwrap();
    for index in 0..MAX_REPRODUCTION_FILES + 1 {
        std::fs::write(work.join(format!("dataset/{index:04}.csv")), b"x").unwrap();
    }
    std::fs::write(work.join("metrics.json"), b"{\"score\":7}").unwrap();
    let document = serde_json::to_string(&json!({"stages":{"evaluate":{
        "cmd":{"argv":["/usr/bin/true"]}, "env":"empty",
        "deps":[external,"large.bin","dataset"], "metrics":["metrics.json"]
    }}}))
    .unwrap();
    std::fs::write(work.join("crab.yaml"), &document).unwrap();
    let workflow = crate::yaml::parse(&document).unwrap();
    let original = run(json!({})).snapshot;
    let metadata = crate::ExperimentMetadata {
        schema_version: 1,
        exp_id: original.experiment_id.parse().unwrap(),
        base_commit: original.base_commit.clone(),
        queue_commit: None,
        name: None,
        message: None,
        status: "success".into(),
        param_overrides: BTreeMap::new(),
        stages: BTreeMap::new(),
        metrics: BTreeMap::new(),
        cli_args: Vec::new(),
        host_fingerprint: "fixture".into(),
        started_at: original.started_at,
        ended_at: original.ended_at,
    };
    let reproduction = ReproductionSpec {
        source_commit: metadata.base_commit.clone(),
        source_result: None,
        files: Vec::new(),
        directories: Vec::new(),
        runtimes: Vec::new(),
        targets: Vec::new(),
        recursive: false,
        single_item: false,
        downstream: false,
        pipeline: false,
        all_pipelines: false,
        glob: false,
        unavailable_reasons: Vec::new(),
    };
    let local = LocalResults::new(directory.path()).unwrap();
    let captured = ResultCapture::begin(
        local,
        &work,
        workflow,
        &["crab.yaml".into()],
        &[],
        reproduction,
    )
    .unwrap()
    .finish(&metadata, &crate::Lockfile::default(), &[])
    .unwrap();
    assert_eq!(captured.snapshot.metrics[0].value, json!(7));
    let reproduction = captured.snapshot.reproduction.unwrap();
    assert!(!reproduction.unavailable_reasons.is_empty());
    assert!(
        reproduction
            .files
            .iter()
            .any(|file| file.state == Availability::NotCaptured)
    );
}

#[test]
fn curve_sampling_preserves_extrema_and_discloses_original_point_count() {
    let directory = tempfile::tempdir().unwrap();
    let local = LocalResults::new(directory.path()).unwrap();
    let mut data = String::from("step,loss\n");
    for index in 0..10_000 {
        data.push_str(&format!(
            "{index},{}\n",
            if index == 1234 { 1_000_000 } else { index }
        ));
    }
    let mut snapshot = run(json!({"score":1})).snapshot;
    snapshot.plots =
        vec![serde_json::from_value(json!({"path":"curve.csv","x":"step","y":["loss"]})).unwrap()];
    snapshot.attachments.push(Attachment {
        path: "curve.csv".into(),
        kind: AttachmentKind::Plot,
        stage: None,
        hash: Some(local.put_blob(data.as_bytes()).unwrap()),
        size: data.len() as u64,
        state: Availability::Present,
        reason: None,
        push: true,
        remote: None,
    });
    let captured = snapshot.seal().unwrap();
    let chart = charts(&[captured], |hash| local.blob(hash))
        .unwrap()
        .remove(0);
    assert_eq!(chart.points, 10_000);
    assert_eq!(
        chart.interaction.as_ref().unwrap().domain,
        Some(AxisRange {
            lower: 0.0,
            upper: 9999.0
        })
    );
    assert!(chart.displayed_points <= MAX_SERIES_POINTS);
    assert!(
        chart.spec.unwrap()["data"]["values"]
            .as_array()
            .unwrap()
            .iter()
            .any(|point| point["value"] == 1_000_000)
    );
}

#[tokio::test]
async fn image_directory_plots_retain_verified_report_members_and_safe_previews() {
    let directory = tempfile::tempdir().unwrap();
    let local = LocalResults::new(directory.path()).unwrap();
    let bytes = b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1\" height=\"1\"/>";
    let mut snapshot = run(json!({"score":1})).snapshot;
    snapshot.plots = vec![serde_json::from_value(json!({"path":"predictions"})).unwrap()];
    snapshot.attachments.push(Attachment {
        path: "predictions/image.svg".into(),
        kind: AttachmentKind::Plot,
        stage: None,
        hash: Some(local.put_blob(bytes).unwrap()),
        size: bytes.len() as u64,
        state: Availability::Present,
        reason: None,
        push: true,
        remote: None,
    });
    let captured = snapshot.seal().unwrap();
    local.install(&captured).unwrap();
    let charts = charts(&[captured.clone()], |hash| local.blob(hash)).unwrap();
    assert_eq!(charts[0].images.len(), 1);
    let report = Report::new(
        "Predictions".into(),
        compare(&[captured.clone()], &spec(&captured)).unwrap(),
        charts,
    )
    .unwrap();
    let id = local.save_report(&report).unwrap();
    let response = ResultsReader::Local(local)
        .read(ResultsRead::Evidence {
            run: RunSelection {
                id: captured.snapshot.experiment_id,
                digest: captured.digest,
            },
            path: "predictions/image.svg".into(),
            report: Some(id),
        })
        .await
        .unwrap();
    let ResultsResponse::Evidence(preview) = response else {
        panic!("expected image preview");
    };
    assert!(
        preview
            .data_url
            .unwrap()
            .starts_with("data:image/svg+xml;base64,")
    );
}

#[test]
#[ignore = "dedicated 10000-run catalog qualification"]
fn ten_thousand_run_catalog_keeps_bounded_pages_and_records_timings() {
    let started = std::time::Instant::now();
    let values: serde_json::Map<String, serde_json::Value> = (0..MAX_COLUMNS)
        .map(|index| (format!("metric_{index:02}"), json!(index)))
        .collect();
    let runs = (0..10_000)
        .map(|_| run(serde_json::Value::Object(values.clone())))
        .collect::<Vec<_>>();
    let build_ms = started.elapsed().as_secs_f64() * 1000.0;
    let request = RunQuery {
        limit: MAX_PAGE_ROWS,
        ..Default::default()
    };
    let started = std::time::Instant::now();
    let page = query(&runs, &request).unwrap();
    let cold_ms = started.elapsed().as_secs_f64() * 1000.0;
    let started = std::time::Instant::now();
    let repeated = query(&runs, &request).unwrap();
    let warm_ms = started.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(
        (page.total, page.rows.len(), page.display_columns.len()),
        (10_000, 200, 32)
    );
    assert_eq!(page.generation, repeated.generation);
    let bytes = encode(&page).unwrap().len();
    assert!(bytes <= MAX_DOCUMENT_BYTES);
    let evidence = json!({"runs":runs.len(),"rows":page.rows.len(),"columns":page.display_columns.len(),
        "catalog_build_ms":build_ms,"query_cold_ms":cold_ms,"query_warm_ms":warm_ms,"response_bytes":bytes,
        "platform":std::env::consts::OS,"architecture":std::env::consts::ARCH});
    if let Some(output) = std::env::var_os("CRAB_RESULTS_EVIDENCE") {
        std::fs::write(
            std::path::PathBuf::from(output).join("catalog-scale.json"),
            serde_json::to_vec_pretty(&evidence).unwrap(),
        )
        .unwrap();
    }
    println!("{evidence}");
}

#[test]
fn same_name_runs_keep_distinct_curve_series_with_shared_id_prefixes_or_suffixes() {
    for ids in [
        [
            "019a0000-0000-7000-8000-000000000001",
            "019a0000-0000-7000-8000-000000000002",
        ],
        [
            "019a0000-0000-7000-8000-000000000001",
            "019b0000-0000-7000-8000-000000000001",
        ],
    ] {
        let directory = tempfile::tempdir().unwrap();
        let local = LocalResults::new(directory.path()).unwrap();
        let mut runs = Vec::new();
        for (offset, id) in ids.into_iter().enumerate() {
            let index = offset + 1;
            let bytes = format!("step,loss\n0,{}\n1,{}\n", index * 2, index);
            let mut snapshot = run(json!({"score":index})).snapshot;
            snapshot.experiment_id = id.into();
            snapshot.name = Some("same label".into());
            snapshot.plots = vec![
                serde_json::from_value(json!({"path":"curve.csv","x":"step","y":["loss"]}))
                    .unwrap(),
            ];
            snapshot.attachments.push(Attachment {
                path: "curve.csv".into(),
                kind: AttachmentKind::Plot,
                stage: None,
                hash: Some(local.put_blob(bytes.as_bytes()).unwrap()),
                size: bytes.len() as u64,
                state: Availability::Present,
                reason: None,
                push: true,
                remote: None,
            });
            runs.push(snapshot.seal().unwrap());
        }
        let chart = charts(&runs, |hash| local.blob(hash)).unwrap().remove(0);
        let series = chart.spec.unwrap()["data"]["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["series"].as_str().unwrap().to_owned())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(series.len(), 2);
    }
}
