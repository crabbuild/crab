use std::{future::Future, pin::Pin, time::Duration};

use crab_cell_runtime::{
    Digest, QUALIFICATION_PROTECTED_EVIDENCE_MAX_AGE_MS,
    QUALIFICATION_PROTECTED_EVIDENCE_MAX_CLOCK_SKEW_MS, QualificationExecution,
    QualificationOperation, QualificationOperationExecutor, QualificationOwnership,
    QualificationProfile, QualificationProviderEvidence, QualificationReceipt, QualificationRunner,
    QualificationWorkload, Result,
};
use ed25519_dalek::SigningKey;

const SOURCE: &str = "qualification-source";
const IMAGE: Digest = Digest::from_bytes([7; 32]);
const MATRIX_ROWS: [&str; 10] = [
    "protocol",
    "storage",
    "publication",
    "warm-path",
    "churn",
    "fleet",
    "failover",
    "primitives",
    "accounting",
    "compatibility",
];

fn receipt(workload: &str, artifact: &[u8]) -> QualificationReceipt {
    QualificationRunner::new(SigningKey::from_bytes(&[11; 32]))
        .emit_with_profile(
            &QualificationProfile::pr_contract(),
            SOURCE.into(),
            IMAGE,
            "local".into(),
            workload.into(),
            "none".into(),
            Vec::new(),
            artifact,
            true,
            (
                "rustc".into(),
                "test".into(),
                "local".into(),
                0,
                0,
                0,
                false,
            ),
        )
        .expect("fixture receipt")
}

struct MeasuredExecutor;

impl QualificationOperationExecutor for MeasuredExecutor {
    type Future<'a> = Pin<Box<dyn Future<Output = Result<QualificationExecution>> + Send + 'a>>;

    fn execute<'a>(&'a mut self, _operation: QualificationOperation) -> Self::Future<'a> {
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(150)).await;
            Ok(QualificationExecution::acknowledged(true))
        })
    }
}

#[test]
fn workload_identity_is_reproducible_and_seed_bound() {
    let profile = QualificationProfile::pr_contract();
    let first = QualificationWorkload::generate(&profile, 17).expect("workload generation");
    let same = QualificationWorkload::generate(&profile, 17).expect("same workload generation");
    let changed =
        QualificationWorkload::generate(&profile, 18).expect("changed workload generation");

    assert_eq!(first, same);
    assert_eq!(first.outcome_digest(), same.outcome_digest());
    assert_ne!(first.outcome_digest(), changed.outcome_digest());
    first
        .verify_for_profile(&profile)
        .expect("canonical workload identity");
    assert_eq!(
        QualificationWorkload::decode(&first.encode().expect("workload encoding"))
            .expect("workload decoding"),
        first
    );
}

#[test]
fn public_matrix_verifier_rejects_missing_duplicate_and_forged_evidence() {
    let artifacts = MATRIX_ROWS
        .iter()
        .map(|row| format!("artifact-{row}").into_bytes())
        .collect::<Vec<_>>();
    let receipts = MATRIX_ROWS
        .iter()
        .enumerate()
        .map(|(index, row)| receipt(row, &artifacts[index]))
        .collect::<Vec<_>>();
    let artifact_views = artifacts
        .iter()
        .map(|artifact| vec![artifact.as_slice()])
        .collect::<Vec<_>>();
    let evidence = MATRIX_ROWS
        .iter()
        .enumerate()
        .map(|(index, row)| (*row, &receipts[index], artifact_views[index].as_slice()))
        .collect::<Vec<_>>();

    QualificationReceipt::verify_matrix(SOURCE, IMAGE, &evidence).expect("complete matrix");

    let mut missing = evidence.clone();
    missing.pop();
    assert!(QualificationReceipt::verify_matrix(SOURCE, IMAGE, &missing).is_err());

    let mut duplicate = evidence.clone();
    duplicate[1].0 = MATRIX_ROWS[0];
    assert!(QualificationReceipt::verify_matrix(SOURCE, IMAGE, &duplicate).is_err());

    let mut forged_artifact = artifacts[0].clone();
    forged_artifact.push(b'!');
    let forged_view = [forged_artifact.as_slice()];
    let mut forged = evidence;
    forged[0].2 = &forged_view;
    assert!(QualificationReceipt::verify_matrix(SOURCE, IMAGE, &forged).is_err());
}

#[test]
fn public_receipt_verifier_binds_source_image_and_artifact() {
    let artifact = b"raw-evidence";
    let receipt = receipt("protocol", artifact);
    receipt
        .verify_for(SOURCE, IMAGE, artifact)
        .expect("exact release identity");
    assert!(receipt.verify_for("other-source", IMAGE, artifact).is_err());
    assert!(
        receipt
            .verify_for(SOURCE, Digest::from_bytes([8; 32]), artifact)
            .is_err()
    );
    assert!(
        receipt
            .verify_for(SOURCE, IMAGE, b"forged-evidence")
            .is_err()
    );
    assert!(
        receipt
            .verify_for_trusted_signer(SOURCE, IMAGE, artifact, [12; 32])
            .is_err()
    );
}

#[test]
fn protected_evidence_freshness_rejects_stale_and_future_receipts() {
    let current = receipt("protocol", b"raw-evidence")
        .with_evidence(
            900,
            1_000,
            b"none",
            vec![Digest::from_bytes(
                *blake3::hash(b"raw-evidence").as_bytes(),
            )],
            Vec::new(),
        )
        .expect("evidence timestamps");

    current.verify_fresh_at(1_000).expect("current evidence");
    current
        .verify_fresh_at(1_000 + QUALIFICATION_PROTECTED_EVIDENCE_MAX_AGE_MS)
        .expect("boundary evidence");
    assert!(
        current
            .verify_fresh_at(1_001 + QUALIFICATION_PROTECTED_EVIDENCE_MAX_AGE_MS)
            .is_err()
    );
    let future = receipt("protocol", b"raw-evidence")
        .with_evidence(
            QUALIFICATION_PROTECTED_EVIDENCE_MAX_CLOCK_SKEW_MS + 1_000,
            QUALIFICATION_PROTECTED_EVIDENCE_MAX_CLOCK_SKEW_MS + 2_000,
            b"none",
            vec![Digest::from_bytes(
                *blake3::hash(b"raw-evidence").as_bytes(),
            )],
            Vec::new(),
        )
        .expect("future evidence timestamps");
    assert!(future.verify_fresh_at(1_000).is_err());
    assert!(current.verify_fresh_at(0).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_protected_matrix_binds_run_artifact_profile_and_signer() {
    let mut profile_value = serde_json::to_value(
        QualificationProfile::new("protected-contract".into(), 1, 8, 1, 5_000)
            .expect("protected profile"),
    )
    .expect("protected profile value");
    profile_value["provider"] = serde_json::Value::String("protected-provider".into());
    let profile: QualificationProfile =
        serde_json::from_value(profile_value).expect("named protected profile");
    let workload = QualificationWorkload::generate_with_size(&profile, 91, 1, 8, 1)
        .expect("protected workload");
    let mut executor = MeasuredExecutor;
    let summary = workload
        .run(&mut executor)
        .await
        .expect("measured workload");
    let run = summary.artifact(&workload).expect("run artifact");
    let workload_bytes = workload.encode().expect("workload encoding");
    let run_bytes = run.encode().expect("run encoding");
    let provider_evidence =
        QualificationProviderEvidence::new(&profile, workload.seed(), true, true, true)
            .expect("provider evidence")
            .encode()
            .expect("provider evidence encoding");
    run.verify_for_profile(&profile)
        .expect("measured run thresholds");

    let signing_key = SigningKey::from_bytes(&[12; 32]);
    let trusted_signer = signing_key.verifying_key().to_bytes();
    let image = Digest::from_bytes([13; 32]);
    let mut receipts = Vec::with_capacity(MATRIX_ROWS.len());
    let mut artifacts = Vec::with_capacity(MATRIX_ROWS.len());
    for workload_name in MATRIX_ROWS {
        let row_artifacts = if workload_name == "primitives" {
            vec![
                workload_bytes.clone(),
                run_bytes.clone(),
                provider_evidence.clone(),
            ]
        } else {
            vec![format!("protected-{workload_name}").into_bytes()]
        };
        let metrics = if workload_name == "primitives" {
            summary.metrics().expect("run metrics")
        } else {
            vec![
                crab_cell_runtime::QualificationMetric::new("cells".into(), 1, "cells".into())
                    .expect("cell metric"),
                crab_cell_runtime::QualificationMetric::new(
                    "operations".into(),
                    8,
                    "operations".into(),
                )
                .expect("operation metric"),
                crab_cell_runtime::QualificationMetric::new(
                    "duration_secs".into(),
                    1,
                    "seconds".into(),
                )
                .expect("duration metric"),
                crab_cell_runtime::QualificationMetric::new(
                    "p99_latency_ms".into(),
                    1,
                    "ms".into(),
                )
                .expect("latency metric"),
            ]
        };
        let primary = row_artifacts[0].clone();
        let raw_digests = row_artifacts
            .iter()
            .map(|artifact| Digest::from_bytes(*blake3::hash(artifact).as_bytes()))
            .collect();
        let receipt = QualificationRunner::new(signing_key.clone())
            .emit_with_profile_and_evidence(
                &profile,
                "protected-source".into(),
                image,
                "protected-provider".into(),
                workload_name.into(),
                "none".into(),
                metrics,
                &primary,
                true,
                (
                    "rustc".into(),
                    "release".into(),
                    "protected-topology".into(),
                    if workload_name == "primitives" {
                        workload.seed()
                    } else {
                        0
                    },
                    1,
                    1,
                    false,
                ),
                1,
                1_001,
                b"none",
                raw_digests,
                vec![QualificationOwnership::new(
                    1,
                    1,
                    Digest::from_bytes([14; 32]),
                )],
            )
            .expect("protected receipt");
        receipts.push(receipt);
        artifacts.push(row_artifacts);
    }

    let artifact_views = artifacts
        .iter()
        .map(|row| row.iter().map(Vec::as_slice).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let evidence = MATRIX_ROWS
        .iter()
        .enumerate()
        .map(|(index, workload_name)| {
            (
                *workload_name,
                &receipts[index],
                artifact_views[index].as_slice(),
            )
        })
        .collect::<Vec<_>>();
    QualificationReceipt::verify_matrix_for_profile_with_signer(
        "protected-source",
        image,
        &profile,
        &evidence,
        trusted_signer,
    )
    .expect("complete protected matrix");
    QualificationReceipt::verify_matrix_for_profile_with_signer_fresh_at(
        "protected-source",
        image,
        &profile,
        &evidence,
        trusted_signer,
        2,
    )
    .expect("fresh protected matrix");
    assert!(
        QualificationReceipt::verify_matrix_for_profile_with_signer_fresh_at(
            "protected-source",
            image,
            &profile,
            &evidence,
            trusted_signer,
            1_001 + QUALIFICATION_PROTECTED_EVIDENCE_MAX_AGE_MS + 1,
        )
        .is_err()
    );
    let mismatched_metrics = summary
        .metrics()
        .expect("run metrics")
        .into_iter()
        .map(|metric| {
            let value = if metric.name() == "p99_latency_ms" {
                metric.value().saturating_add(1)
            } else {
                metric.value()
            };
            crab_cell_runtime::QualificationMetric::new(
                metric.name().into(),
                value,
                metric.unit().into(),
            )
            .expect("mismatched metric")
        })
        .collect();
    let primitive_artifacts = &artifacts[7];
    let mismatched_primitive = QualificationRunner::new(signing_key.clone())
        .emit_with_profile_and_evidence(
            &profile,
            "protected-source".into(),
            image,
            "protected-provider".into(),
            "primitives".into(),
            "none".into(),
            mismatched_metrics,
            &primitive_artifacts[0],
            true,
            (
                "rustc".into(),
                "release".into(),
                "protected-topology".into(),
                workload.seed(),
                1,
                1,
                false,
            ),
            1,
            2,
            b"none",
            primitive_artifacts
                .iter()
                .map(|artifact| Digest::from_bytes(*blake3::hash(artifact).as_bytes()))
                .collect(),
            vec![QualificationOwnership::new(
                1,
                1,
                Digest::from_bytes([14; 32]),
            )],
        )
        .expect("mismatched protected receipt");
    let mut mismatched_receipts = receipts.clone();
    mismatched_receipts[7] = mismatched_primitive;
    let mismatched_evidence = MATRIX_ROWS
        .iter()
        .enumerate()
        .map(|(index, workload_name)| {
            (
                *workload_name,
                &mismatched_receipts[index],
                artifact_views[index].as_slice(),
            )
        })
        .collect::<Vec<_>>();
    assert!(
        QualificationReceipt::verify_matrix_for_profile_with_signer(
            "protected-source",
            image,
            &profile,
            &mismatched_evidence,
            trusted_signer,
        )
        .is_err()
    );
    assert!(
        QualificationReceipt::verify_matrix_for_profile_with_signer(
            "protected-source",
            image,
            &profile,
            &evidence,
            [15; 32],
        )
        .is_err()
    );
}
