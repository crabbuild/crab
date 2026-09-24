//! Matrix manifests and row recomputation.

use super::*;

#[test]
fn matrix_manifest_requires_each_bounded_workload_once() {
    let entries = QUALIFICATION_MATRIX_ROWS
        .iter()
        .map(|workload| {
            QualificationMatrixEntry::new(
                (*workload).to_owned(),
                format!("receipts/{workload}.json"),
                vec![format!("artifacts/{workload}.json")],
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    let manifest = QualificationMatrixManifest::new(entries).unwrap();
    assert_eq!(
        QualificationMatrixManifest::decode(&manifest.encode().unwrap()).unwrap(),
        manifest
    );

    let mut incomplete = manifest.entries().to_vec();
    incomplete.pop();
    assert!(QualificationMatrixManifest::new(incomplete).is_err());

    let mut reordered = manifest.entries().to_vec();
    reordered.swap(0, 1);
    assert!(QualificationMatrixManifest::new(reordered).is_err());

    let mut duplicate = manifest.entries().to_vec();
    duplicate[0] = QualificationMatrixEntry::new(
        duplicate[1].workload().to_owned(),
        duplicate[0].receipt().to_owned(),
        duplicate[0].artifacts().to_vec(),
    )
    .unwrap();
    assert!(QualificationMatrixManifest::new(duplicate).is_err());

    assert!(
        QualificationMatrixEntry::new(
            "protocol".into(),
            "../receipt.json".into(),
            vec!["artifact.bin".into()],
        )
        .is_err()
    );
    assert!(
        QualificationMatrixEntry::new(
            "protocol".into(),
            "/tmp/receipt.json".into(),
            vec!["artifact.bin".into()],
        )
        .is_err()
    );
}

#[test]
fn matrix_verifier_recomputes_every_row_artifact() {
    let image = Digest::from_bytes([7; 32]);
    let runner = QualificationRunner::new(SigningKey::from_bytes(&[8; 32]));
    let mut receipts = Vec::new();
    let mut artifacts = Vec::new();
    for workload in QUALIFICATION_MATRIX_ROWS {
        let artifact = format!("artifact-{workload}").into_bytes();
        receipts.push(
            runner
                .emit(
                    "source".into(),
                    image,
                    "local".into(),
                    (*workload).into(),
                    "none".into(),
                    Vec::new(),
                    &artifact,
                    true,
                    (
                        "rustc".into(),
                        "test".into(),
                        "local".into(),
                        1,
                        0,
                        0,
                        false,
                    ),
                )
                .unwrap(),
        );
        artifacts.push(artifact);
    }
    let artifact_views = artifacts
        .iter()
        .map(|artifact| vec![artifact.as_slice()])
        .collect::<Vec<_>>();
    let evidence = QUALIFICATION_MATRIX_ROWS
        .iter()
        .enumerate()
        .map(|(index, workload)| {
            (
                *workload,
                &receipts[index],
                artifact_views[index].as_slice(),
            )
        })
        .collect::<Vec<_>>();
    QualificationReceipt::verify_matrix("source", image, &evidence).unwrap();

    let mut forged_artifact = artifacts[0].clone();
    forged_artifact.push(b'!');
    let forged_views = [vec![forged_artifact.as_slice()]];
    let forged_evidence = evidence
        .iter()
        .enumerate()
        .map(|(index, (workload, receipt, row_artifacts))| {
            if index == 0 {
                (*workload, *receipt, forged_views[0].as_slice())
            } else {
                (*workload, *receipt, *row_artifacts)
            }
        })
        .collect::<Vec<_>>();
    assert!(QualificationReceipt::verify_matrix("source", image, &forged_evidence).is_err());
}

#[test]
fn primitive_matrix_binds_the_receipt_to_the_workload_seed() {
    let profile = QualificationProfile::pr_contract();
    let image = Digest::from_bytes([17; 32]);
    let workload = QualificationWorkload::generate(&profile, 7).unwrap();
    let workload_artifact = workload.encode().unwrap();
    let runner = QualificationRunner::new(SigningKey::from_bytes(&[18; 32]));
    let threshold_metrics = || {
        vec![
            QualificationMetric::new("cells".into(), 1, "cells".into()).unwrap(),
            QualificationMetric::new("operations".into(), 1, "operations".into()).unwrap(),
            QualificationMetric::new("duration_secs".into(), 1, "seconds".into()).unwrap(),
            QualificationMetric::new("p99_latency_ms".into(), 1, "ms".into()).unwrap(),
        ]
    };
    let mut receipts = Vec::new();
    let mut artifacts = Vec::new();
    for workload_name in QUALIFICATION_MATRIX_ROWS {
        let artifact = if *workload_name == "primitives" {
            workload_artifact.clone()
        } else {
            format!("artifact-{workload_name}").into_bytes()
        };
        let seed = if *workload_name == "primitives" {
            workload.seed()
        } else {
            0
        };
        receipts.push(
            runner
                .emit_with_profile(
                    &profile,
                    "source".into(),
                    image,
                    "local".into(),
                    (*workload_name).into(),
                    "none".into(),
                    threshold_metrics(),
                    &artifact,
                    true,
                    (
                        "rustc".into(),
                        "test".into(),
                        "local".into(),
                        seed,
                        0,
                        0,
                        false,
                    ),
                )
                .unwrap(),
        );
        artifacts.push(artifact);
    }
    let artifact_views = artifacts
        .iter()
        .map(|artifact| vec![artifact.as_slice()])
        .collect::<Vec<_>>();
    let evidence = QUALIFICATION_MATRIX_ROWS
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
    QualificationReceipt::verify_matrix_for_profile("source", image, &profile, &evidence).unwrap();

    let below_threshold = runner
        .emit_with_profile(
            &profile,
            "source".into(),
            image,
            "local".into(),
            "protocol".into(),
            "none".into(),
            vec![
                QualificationMetric::new("cells".into(), 1, "cells".into()).unwrap(),
                QualificationMetric::new("operations".into(), 1, "operations".into()).unwrap(),
                QualificationMetric::new("duration_secs".into(), 1, "seconds".into()).unwrap(),
                QualificationMetric::new("p99_latency_ms".into(), 5_001, "ms".into()).unwrap(),
            ],
            b"artifact-protocol",
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
        .unwrap();
    let mut below_threshold_evidence = evidence.clone();
    let below_threshold_artifacts = [b"artifact-protocol" as &[u8]];
    below_threshold_evidence[0] = ("protocol", &below_threshold, &below_threshold_artifacts);
    assert!(
        QualificationReceipt::verify_matrix_for_profile(
            "source",
            image,
            &profile,
            &below_threshold_evidence,
        )
        .is_err()
    );

    let bad = runner
        .emit_with_profile(
            &profile,
            "source".into(),
            image,
            "local".into(),
            "primitives".into(),
            "none".into(),
            threshold_metrics(),
            &workload_artifact,
            true,
            (
                "rustc".into(),
                "test".into(),
                "local".into(),
                workload.seed() + 1,
                0,
                0,
                false,
            ),
        )
        .unwrap();
    let mut bad_evidence = evidence;
    bad_evidence[7] = ("primitives", &bad, artifact_views[7].as_slice());
    assert!(
        QualificationReceipt::verify_matrix_for_profile("source", image, &profile, &bad_evidence,)
            .is_err()
    );
}

#[test]
fn protected_primitive_matrix_requires_measured_run_artifact() {
    let profile = QualificationProfile::pr_contract();
    let image = Digest::from_bytes([27; 32]);
    let key = SigningKey::from_bytes(&[28; 32]);
    let runner = QualificationRunner::new(key.clone());
    let workload = QualificationWorkload::generate_with_size(&profile, 7, 1, 8, 1).unwrap();
    let workload_artifact = workload.encode().unwrap();
    let run_artifact = QualificationRunArtifact {
        schema_version: QUALIFICATION_RUN_ARTIFACT_SCHEMA_VERSION,
        workload: workload.clone(),
        profile: profile.name.clone(),
        profile_digest: *profile.digest().unwrap().as_bytes(),
        seed: workload.seed(),
        cells: workload.cells(),
        operations: workload.operations(),
        elapsed_ms: 1_000,
        primitive_counts: workload.primitives.clone(),
        case_coverage: vec![0; QUALIFICATION_CASE_COVERAGE_BYTES],
        outcome_digest: *qualification_run_outcome_digest(
            &workload,
            &workload.primitives,
            &[0; QUALIFICATION_CASE_COVERAGE_BYTES],
        )
        .unwrap()
        .as_bytes(),
        metrics: vec![
            QualificationMetric::new("cells".into(), 1, "cells".into()).unwrap(),
            QualificationMetric::new("operations".into(), 8, "operations".into()).unwrap(),
            QualificationMetric::new("duration_secs".into(), 1, "seconds".into()).unwrap(),
            QualificationMetric::new("throughput_ops_per_sec".into(), 8, "ops/s".into()).unwrap(),
            QualificationMetric::new("p50_latency_ms".into(), 1, "ms".into()).unwrap(),
            QualificationMetric::new("p95_latency_ms".into(), 1, "ms".into()).unwrap(),
            QualificationMetric::new("p99_latency_ms".into(), 1, "ms".into()).unwrap(),
            QualificationMetric::new("max_latency_ms".into(), 1, "ms".into()).unwrap(),
        ],
    }
    .encode()
    .unwrap();
    let threshold_metrics = || {
        vec![
            QualificationMetric::new("cells".into(), 1, "cells".into()).unwrap(),
            QualificationMetric::new("operations".into(), 8, "operations".into()).unwrap(),
            QualificationMetric::new("duration_secs".into(), 1, "seconds".into()).unwrap(),
            QualificationMetric::new("throughput_ops_per_sec".into(), 8, "ops/s".into()).unwrap(),
            QualificationMetric::new("p50_latency_ms".into(), 1, "ms".into()).unwrap(),
            QualificationMetric::new("p95_latency_ms".into(), 1, "ms".into()).unwrap(),
            QualificationMetric::new("p99_latency_ms".into(), 1, "ms".into()).unwrap(),
            QualificationMetric::new("max_latency_ms".into(), 1, "ms".into()).unwrap(),
        ]
    };
    let mut receipts = Vec::new();
    let mut artifacts = Vec::new();
    for workload_name in QUALIFICATION_MATRIX_ROWS {
        let (primary, row_artifacts, seed) = if *workload_name == "primitives" {
            (
                workload_artifact.clone(),
                vec![workload_artifact.clone(), run_artifact.clone()],
                workload.seed(),
            )
        } else {
            let artifact = format!("artifact-{workload_name}").into_bytes();
            (artifact.clone(), vec![artifact], 0)
        };
        let raw_digests = row_artifacts
            .iter()
            .map(|artifact| Digest::from_bytes(*blake3::hash(artifact).as_bytes()))
            .collect();
        receipts.push(
            runner
                .emit_with_profile_and_evidence(
                    &profile,
                    "source".into(),
                    image,
                    "local".into(),
                    (*workload_name).into(),
                    "none".into(),
                    threshold_metrics(),
                    &primary,
                    true,
                    (
                        "rustc".into(),
                        "test".into(),
                        "local".into(),
                        seed,
                        0,
                        0,
                        false,
                    ),
                    1,
                    2,
                    b"none",
                    raw_digests,
                    Vec::new(),
                )
                .unwrap(),
        );
        artifacts.push(row_artifacts);
    }
    let artifact_views = artifacts
        .iter()
        .map(|row| row.iter().map(Vec::as_slice).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let evidence = QUALIFICATION_MATRIX_ROWS
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
        "source",
        image,
        &profile,
        &evidence,
        key.verifying_key().to_bytes(),
    )
    .unwrap();

    let mut mismatched_receipt = receipts[7].clone();
    mismatched_receipt
        .metrics
        .iter_mut()
        .find(|metric| metric.name() == "p95_latency_ms")
        .unwrap()
        .value = 2;
    let mismatched_receipt = mismatched_receipt.attest(&key).unwrap();
    let mut mismatched_evidence = evidence.clone();
    mismatched_evidence[7] = (
        "primitives",
        &mismatched_receipt,
        artifact_views[7].as_slice(),
    );
    assert!(
        QualificationReceipt::verify_matrix_for_profile_with_signer(
            "source",
            image,
            &profile,
            &mismatched_evidence,
            key.verifying_key().to_bytes(),
        )
        .is_err()
    );

    let missing_artifacts = artifacts
        .iter()
        .enumerate()
        .map(|(index, row)| {
            if index == 7 {
                vec![row[0].clone()]
            } else {
                row.clone()
            }
        })
        .collect::<Vec<_>>();
    let missing_views = missing_artifacts
        .iter()
        .map(|row| row.iter().map(Vec::as_slice).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let missing_run = QUALIFICATION_MATRIX_ROWS
        .iter()
        .enumerate()
        .map(|(index, workload_name)| {
            (
                *workload_name,
                &receipts[index],
                missing_views[index].as_slice(),
            )
        })
        .collect::<Vec<_>>();
    assert!(
        QualificationReceipt::verify_matrix_for_profile_with_signer(
            "source",
            image,
            &profile,
            &missing_run,
            key.verifying_key().to_bytes(),
        )
        .is_err()
    );
}
