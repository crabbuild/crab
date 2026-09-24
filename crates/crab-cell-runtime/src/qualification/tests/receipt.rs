//! Receipts, runners, and protected verification.

use super::*;

#[test]
fn measured_run_artifact_binds_the_canonical_workload_and_thresholds() {
    let profile = QualificationProfile::pr_contract();
    let workload = QualificationWorkload::generate_with_size(&profile, 19, 1, 64, 1).unwrap();
    let artifact = QualificationRunArtifact {
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
            QualificationMetric::new("operations".into(), 64, "operations".into()).unwrap(),
            QualificationMetric::new("duration_secs".into(), 1, "seconds".into()).unwrap(),
            QualificationMetric::new("throughput_ops_per_sec".into(), 64, "ops/s".into()).unwrap(),
            QualificationMetric::new("p50_latency_ms".into(), 1, "ms".into()).unwrap(),
            QualificationMetric::new("p95_latency_ms".into(), 1, "ms".into()).unwrap(),
            QualificationMetric::new("p99_latency_ms".into(), 1, "ms".into()).unwrap(),
            QualificationMetric::new("max_latency_ms".into(), 1, "ms".into()).unwrap(),
        ],
    };
    let encoded = artifact.encode().unwrap();
    assert_eq!(
        QualificationRunArtifact::decode(&encoded).unwrap(),
        artifact
    );
    artifact.verify_for_profile(&profile).unwrap();

    let mut missing_latency = artifact.clone();
    missing_latency
        .metrics
        .retain(|metric| metric.name() != "p95_latency_ms");
    assert!(missing_latency.encode().is_err());

    let mut unordered_latency = artifact.clone();
    unordered_latency
        .metrics
        .iter_mut()
        .find(|metric| metric.name() == "p50_latency_ms")
        .unwrap()
        .value = 2;
    assert!(unordered_latency.encode().is_err());

    let mut all_acknowledged = artifact.clone();
    for counts in &mut all_acknowledged.primitive_counts {
        counts.acknowledged = counts.attempted;
        counts.rejected = 0;
        counts.ambiguous = 0;
        counts.retried = 0;
        counts.verified = counts.attempted;
    }
    all_acknowledged.outcome_digest = *qualification_run_outcome_digest(
        &all_acknowledged.workload,
        &all_acknowledged.primitive_counts,
        &all_acknowledged.case_coverage,
    )
    .unwrap()
    .as_bytes();
    all_acknowledged.verify_for_profile(&profile).unwrap();

    let mut throughput_profile =
        QualificationProfile::new("throughput-run".into(), 1, 8, 1, 1_000).unwrap();
    throughput_profile.minimum_throughput_ops_per_sec = 8;
    let throughput_workload =
        QualificationWorkload::generate_with_size(&throughput_profile, 19, 1, 8, 1).unwrap();
    let mut slow = QualificationRunArtifact {
        schema_version: QUALIFICATION_RUN_ARTIFACT_SCHEMA_VERSION,
        workload: throughput_workload.clone(),
        profile: throughput_profile.name.clone(),
        profile_digest: *throughput_profile.digest().unwrap().as_bytes(),
        seed: throughput_workload.seed(),
        cells: throughput_workload.cells(),
        operations: throughput_workload.operations(),
        elapsed_ms: 2_000,
        primitive_counts: throughput_workload.primitives.clone(),
        case_coverage: vec![0; QUALIFICATION_CASE_COVERAGE_BYTES],
        outcome_digest: *qualification_run_outcome_digest(
            &throughput_workload,
            &throughput_workload.primitives,
            &[0; QUALIFICATION_CASE_COVERAGE_BYTES],
        )
        .unwrap()
        .as_bytes(),
        metrics: vec![
            QualificationMetric::new("cells".into(), 1, "cells".into()).unwrap(),
            QualificationMetric::new("operations".into(), 8, "operations".into()).unwrap(),
            QualificationMetric::new("duration_secs".into(), 2, "seconds".into()).unwrap(),
            QualificationMetric::new("throughput_ops_per_sec".into(), 4, "ops/s".into()).unwrap(),
            QualificationMetric::new("p50_latency_ms".into(), 1, "ms".into()).unwrap(),
            QualificationMetric::new("p95_latency_ms".into(), 1, "ms".into()).unwrap(),
            QualificationMetric::new("p99_latency_ms".into(), 1, "ms".into()).unwrap(),
            QualificationMetric::new("max_latency_ms".into(), 1, "ms".into()).unwrap(),
        ],
    };
    assert!(slow.verify_for_profile(&throughput_profile).is_err());
    slow.elapsed_ms = 1_000;
    slow.metrics[2].value = 1;
    slow.metrics[3].value = 8;
    slow.verify_for_profile(&throughput_profile).unwrap();

    let mut forged = artifact.clone();
    forged.seed = forged.seed.saturating_add(1);
    assert!(forged.verify_for_profile(&profile).is_err());

    let mut forged_digest = artifact.clone();
    forged_digest.outcome_digest[0] ^= 1;
    assert!(forged_digest.encode().is_err());

    let mut unverified = artifact.clone();
    unverified.primitive_counts[0].verified = 0;
    assert!(unverified.encode().is_err());

    let mut redistributed = artifact.clone();
    let donor = redistributed
        .primitive_counts
        .iter()
        .position(|counts| counts.acknowledged >= 2)
        .unwrap();
    let recipient = redistributed
        .primitive_counts
        .iter()
        .position(|counts| counts.primitive != redistributed.primitive_counts[donor].primitive)
        .unwrap();
    redistributed.primitive_counts[donor].attempted -= 1;
    redistributed.primitive_counts[donor].acknowledged -= 1;
    redistributed.primitive_counts[donor].verified -= 1;
    redistributed.primitive_counts[recipient].attempted += 1;
    redistributed.primitive_counts[recipient].acknowledged += 1;
    redistributed.primitive_counts[recipient].verified += 1;
    assert!(redistributed.encode().is_err());
}

#[tokio::test]
async fn protected_run_artifact_binds_receipt_resource_measurements() {
    let mut profile =
        QualificationProfile::new("protected-resource-contract".into(), 1, 8, 1, 5_000).unwrap();
    profile.maximum_peak_rss_bytes = 1_000;
    profile.maximum_local_disk_bytes = 2_000;
    profile.maximum_file_descriptors = 3_000;
    profile.maximum_bucket_calls = 4_000;
    let workload = QualificationWorkload::generate_with_size(&profile, 23, 1, 8, 1).unwrap();
    let mut executor = ContractExecutor {
        calls: 0,
        case_coverage: false,
    };
    let summary = workload.run(&mut executor).await.unwrap();
    assert!(
        summary
            .artifact(&workload)
            .unwrap()
            .verify_for_profile(&profile)
            .is_err()
    );

    let resources = [
        QualificationMetric::new("peak_rss_bytes".into(), 19, "bytes".into()).unwrap(),
        QualificationMetric::new("peak_local_disk_bytes".into(), 29, "bytes".into()).unwrap(),
        QualificationMetric::new("peak_file_descriptors".into(), 39, "count".into()).unwrap(),
        QualificationMetric::new("bucket_calls".into(), 49, "count".into()).unwrap(),
    ];
    let mut run = summary
        .artifact_with_resource_metrics(&workload, &resources)
        .unwrap();
    run.elapsed_ms = 1_000;
    for (name, value) in [("duration_secs", 1), ("throughput_ops_per_sec", 8)] {
        run.metrics
            .iter_mut()
            .find(|metric| metric.name() == name)
            .unwrap()
            .value = value;
    }
    run.verify_for_profile(&profile).unwrap();
    let encoded = run.encode().unwrap();

    let mut receipt_metrics = summary.metrics().unwrap();
    receipt_metrics.extend(resources.iter().cloned());
    let key = SigningKey::from_bytes(&[93; 32]);
    let receipt = QualificationRunner::new(key)
        .emit_with_profile_and_evidence(
            &profile,
            "resource-source".into(),
            Digest::from_bytes([94; 32]),
            "provider".into(),
            "primitives".into(),
            "none".into(),
            receipt_metrics,
            &encoded,
            true,
            (
                "rustc".into(),
                "release".into(),
                "three-process".into(),
                workload.seed(),
                49,
                19,
                false,
            ),
            1,
            1_001,
            b"none",
            vec![Digest::from_bytes(*blake3::hash(&encoded).as_bytes())],
            vec![QualificationOwnership::new(
                1,
                1,
                Digest::from_bytes([95; 32]),
            )],
        )
        .unwrap();
    receipt.verify_profile_thresholds(&profile).unwrap();
    receipt
        .verify_primitive_run_artifact(&profile, &[&encoded])
        .unwrap();

    let mut forged = receipt;
    forged.bucket_calls += 1;
    assert!(
        forged
            .verify_primitive_run_artifact(&profile, &[&encoded])
            .is_err()
    );
}

#[tokio::test]
async fn protected_run_binder_requires_one_canonical_run_and_workload() {
    let mut profile =
        QualificationProfile::new("protected-binder-contract".into(), 1, 8, 1, 5_000).unwrap();
    profile.maximum_peak_rss_bytes = 1_000;
    profile.maximum_local_disk_bytes = 2_000;
    profile.maximum_file_descriptors = 3_000;
    profile.maximum_bucket_calls = 4_000;
    profile.provider = "rustfs".into();
    let workload = QualificationWorkload::generate_with_size(&profile, 29, 1, 8, 1).unwrap();
    let mut executor = ContractExecutor {
        calls: 0,
        case_coverage: false,
    };
    let summary = workload.run(&mut executor).await.unwrap();
    let resources = [
        QualificationMetric::new("peak_rss_bytes".into(), 19, "bytes".into()).unwrap(),
        QualificationMetric::new("peak_local_disk_bytes".into(), 29, "bytes".into()).unwrap(),
        QualificationMetric::new("peak_file_descriptors".into(), 39, "count".into()).unwrap(),
        QualificationMetric::new("bucket_calls".into(), 49, "count".into()).unwrap(),
    ];
    let mut run = summary
        .artifact_with_resource_metrics(&workload, &resources)
        .unwrap();
    run.elapsed_ms = 1_000;
    for (name, value) in [("duration_secs", 1), ("throughput_ops_per_sec", 8)] {
        run.metrics
            .iter_mut()
            .find(|metric| metric.name() == name)
            .unwrap()
            .value = value;
    }
    run.verify_for_profile(&profile).unwrap();
    let run_bytes = run.encode().unwrap();
    let workload_bytes = workload.encode().unwrap();
    let provider_evidence =
        QualificationProviderEvidence::new(&profile, workload.seed(), true, true, true)
            .unwrap()
            .encode()
            .unwrap();
    let key = SigningKey::from_bytes(&[96; 32]);
    let runner = QualificationRunner::new(key.clone());
    let evidence = QualificationExecutionEvidence {
        provider: "rustfs".into(),
        workload: "primitives".into(),
        fault: "none".into(),
        toolchain: "rustc".into(),
        execution_profile: "release".into(),
        topology: "three-process".into(),
        started_at_ms: 1,
        finished_at_ms: 1_001,
        fault_schedule: b"none".to_vec(),
        ownership: vec![QualificationOwnership::new(
            1,
            1,
            Digest::from_bytes([98; 32]),
        )],
        dirty: false,
    };
    assert!(
        runner
            .emit_protected_run(
                &profile,
                "binder-source".into(),
                Digest::from_bytes([97; 32]),
                evidence.clone(),
                &run,
                &[&run_bytes, &workload_bytes],
            )
            .is_err()
    );
    let partial_provider_evidence =
        QualificationProviderEvidence::new(&profile, workload.seed(), true, true, false)
            .unwrap()
            .encode()
            .unwrap();
    assert!(
        runner
            .emit_protected_run(
                &profile,
                "binder-source".into(),
                Digest::from_bytes([97; 32]),
                evidence.clone(),
                &run,
                &[&run_bytes, &workload_bytes, &partial_provider_evidence],
            )
            .is_err()
    );
    let malformed_provider_evidence = br#"{"schema_version":1,"provider":"rustfs","profile":"protected-binder-contract","conditional":true}"#;
    assert!(
        runner
            .emit_protected_run(
                &profile,
                "binder-source".into(),
                Digest::from_bytes([97; 32]),
                evidence.clone(),
                &run,
                &[
                    &run_bytes,
                    &workload_bytes,
                    &provider_evidence,
                    malformed_provider_evidence
                ],
            )
            .is_err()
    );
    assert!(
        runner
            .emit_protected_run(
                &profile,
                "binder-source".into(),
                Digest::from_bytes([97; 32]),
                evidence.clone(),
                &run,
                &[
                    &run_bytes,
                    &workload_bytes,
                    &provider_evidence,
                    &provider_evidence,
                ],
            )
            .is_err()
    );
    let receipt = runner
        .emit_protected_run(
            &profile,
            "binder-source".into(),
            Digest::from_bytes([97; 32]),
            evidence.clone(),
            &run,
            &[&run_bytes, &workload_bytes, &provider_evidence],
        )
        .unwrap();
    receipt
        .verify_for_profile_with_signer(
            "binder-source",
            Digest::from_bytes([97; 32]),
            &profile,
            &[&run_bytes, &workload_bytes, &provider_evidence],
            key.verifying_key().to_bytes(),
        )
        .unwrap();
    receipt
        .verify_primitive_workload(&profile, &[&run_bytes, &workload_bytes, &provider_evidence])
        .unwrap();
    receipt
        .verify_primitive_run_artifact(&profile, &[&run_bytes, &workload_bytes, &provider_evidence])
        .unwrap();
    let mut short_evidence = evidence.clone();
    short_evidence.finished_at_ms = 2;
    assert!(
        runner
            .emit_protected_run(
                &profile,
                "binder-source".into(),
                Digest::from_bytes([97; 32]),
                short_evidence,
                &run,
                &[&run_bytes, &workload_bytes, &provider_evidence],
            )
            .is_err()
    );
    let mut dirty_evidence = evidence.clone();
    dirty_evidence.dirty = true;
    assert!(
        runner
            .emit_protected_run(
                &profile,
                "binder-source".into(),
                Digest::from_bytes([97; 32]),
                dirty_evidence,
                &run,
                &[&run_bytes, &workload_bytes, &provider_evidence],
            )
            .is_err()
    );

    let mismatched_workload =
        QualificationWorkload::generate_with_size(&profile, 29, 2, 8, 1).unwrap();
    let mismatched_workload_bytes = mismatched_workload.encode().unwrap();
    assert!(
        runner
            .emit_protected_run(
                &profile,
                "binder-source".into(),
                Digest::from_bytes([97; 32]),
                evidence,
                &run,
                &[&run_bytes, &mismatched_workload_bytes, &provider_evidence],
            )
            .is_err()
    );
}

#[test]
fn protected_run_artifacts_require_complete_lifecycle_case_coverage() {
    let mut profile =
        QualificationProfile::new("protected-case-coverage".into(), 1, 8, 1, 1_000).unwrap();
    profile.provider = "rustfs".into();
    profile.topology = "three-process".into();
    let workload = QualificationWorkload::generate_with_size(&profile, 19, 1, 56, 1).unwrap();
    let artifact = QualificationRunArtifact {
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
            QualificationMetric::new("operations".into(), 56, "operations".into()).unwrap(),
            QualificationMetric::new("duration_secs".into(), 1, "seconds".into()).unwrap(),
            QualificationMetric::new("throughput_ops_per_sec".into(), 56, "ops/s".into()).unwrap(),
            QualificationMetric::new("p50_latency_ms".into(), 1, "ms".into()).unwrap(),
            QualificationMetric::new("p95_latency_ms".into(), 1, "ms".into()).unwrap(),
            QualificationMetric::new("p99_latency_ms".into(), 1, "ms".into()).unwrap(),
            QualificationMetric::new("max_latency_ms".into(), 1, "ms".into()).unwrap(),
        ],
    };
    artifact.encode().unwrap();
    assert!(artifact.verify_for_profile(&profile).is_err());

    let mut partial_verification = artifact;
    partial_verification.case_coverage.fill(u8::MAX);
    partial_verification.primitive_counts[0].verified -= 1;
    partial_verification.outcome_digest = *qualification_run_outcome_digest(
        &partial_verification.workload,
        &partial_verification.primitive_counts,
        &partial_verification.case_coverage,
    )
    .unwrap()
    .as_bytes();
    partial_verification.encode().unwrap();
    assert!(partial_verification.verify_for_profile(&profile).is_err());
}

#[test]
fn receipt_round_trips_and_binds_measurements() {
    let receipt = QualificationReceipt::new(
        "abc".into(),
        Digest::from_bytes([1; 32]),
        "memory".into(),
        "warm-route".into(),
        "none".into(),
        vec![QualificationMetric::new("p99".into(), 7, "ms".into()).unwrap()],
        Digest::from_bytes([2; 32]),
        true,
    )
    .unwrap()
    .with_execution(
        "rustc".into(),
        "unit".into(),
        "local".into(),
        7,
        0,
        1024,
        false,
    )
    .unwrap()
    .with_profile(&QualificationProfile::pr_contract())
    .unwrap()
    .attest(&SigningKey::from_bytes(&[9; 32]))
    .unwrap();
    assert_eq!(receipt.schema_version(), QUALIFICATION_SCHEMA_VERSION);
    let decoded = QualificationReceipt::decode(&receipt.encode().unwrap()).unwrap();
    assert_eq!(decoded, receipt);
}

#[test]
fn qualification_metrics_reject_duplicate_identities() {
    let duplicate = vec![
        QualificationMetric::new("p99".into(), 7, "ms".into()).unwrap(),
        QualificationMetric::new("p99".into(), 8, "ms".into()).unwrap(),
    ];
    assert!(
        QualificationReceipt::new(
            "abc".into(),
            Digest::from_bytes([1; 32]),
            "memory".into(),
            "warm-route".into(),
            "none".into(),
            duplicate,
            Digest::from_bytes([2; 32]),
            true,
        )
        .is_err()
    );

    let profile = QualificationProfile::pr_contract();
    let workload = QualificationWorkload::generate(&profile, 19).unwrap();
    let artifact = QualificationRunArtifact {
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
            QualificationMetric::new("p99_latency_ms".into(), 1, "ms".into()).unwrap(),
            QualificationMetric::new("p99_latency_ms".into(), 2, "ms".into()).unwrap(),
        ],
    };
    assert!(artifact.encode().is_err());
}

#[test]
fn runner_binds_artifact_and_rejects_forged_or_dirty_receipts() {
    let runner = QualificationRunner::new(SigningKey::from_bytes(&[4; 32]));
    let receipt = runner
        .emit(
            "abc".into(),
            Digest::from_bytes([1; 32]),
            "rustfs".into(),
            "warm".into(),
            "none".into(),
            Vec::new(),
            b"artifact",
            true,
            (
                "rustc".into(),
                "ci".into(),
                "three-node".into(),
                1,
                2,
                3,
                false,
            ),
        )
        .unwrap();
    assert_eq!(
        receipt.artifact_digest(),
        Digest::from_bytes(*blake3::hash(b"artifact").as_bytes())
    );
    let encoded = receipt.encode().unwrap();
    assert_eq!(QualificationReceipt::decode(&encoded).unwrap(), receipt);
    assert_eq!(
        receipt.profile_digest(),
        QualificationProfile::pr_contract().digest().unwrap()
    );
    receipt
        .verify_for("abc", Digest::from_bytes([1; 32]), b"artifact")
        .unwrap();
    let failed = QualificationReceipt::new(
        "abc".into(),
        Digest::from_bytes([1; 32]),
        "rustfs".into(),
        "warm".into(),
        "none".into(),
        Vec::new(),
        Digest::from_bytes(*blake3::hash(b"artifact").as_bytes()),
        false,
    )
    .unwrap()
    .with_execution(
        "rustc".into(),
        "ci".into(),
        "three-node".into(),
        1,
        2,
        3,
        false,
    )
    .unwrap()
    .with_profile(&QualificationProfile::pr_contract())
    .unwrap()
    .attest(&SigningKey::from_bytes(&[4; 32]))
    .unwrap();
    assert!(
        failed
            .verify_for("abc", Digest::from_bytes([1; 32]), b"artifact")
            .is_err()
    );
    assert!(
        receipt
            .verify_for("other", Digest::from_bytes([1; 32]), b"artifact")
            .is_err()
    );
    let mut forged = receipt.clone();
    forged.bucket_calls = forged.bucket_calls.saturating_add(1);
    assert!(QualificationReceipt::decode(&forged.encode().unwrap()).is_err());
    assert!(
        receipt
            .verify_for("abc", Digest::from_bytes([1; 32]), b"other")
            .is_err()
    );
    let dirty = runner
        .emit(
            "abc".into(),
            Digest::from_bytes([1; 32]),
            "rustfs".into(),
            "warm".into(),
            "none".into(),
            Vec::new(),
            b"artifact",
            true,
            (
                "rustc".into(),
                "ci".into(),
                "three-node".into(),
                1,
                2,
                3,
                true,
            ),
        )
        .unwrap();
    assert!(dirty.encode().is_err());
}

#[test]
fn protected_verification_pins_signer_and_threshold_metrics() {
    let profile = QualificationProfile::pr_contract();
    let key = SigningKey::from_bytes(&[12; 32]);
    let metrics = vec![
        QualificationMetric::new("cells".into(), 1, "cells".into()).unwrap(),
        QualificationMetric::new("operations".into(), 1, "operations".into()).unwrap(),
        QualificationMetric::new("duration_secs".into(), 1, "seconds".into()).unwrap(),
        QualificationMetric::new("p99_latency_ms".into(), 5_000, "ms".into()).unwrap(),
    ];
    let receipt = QualificationRunner::new(key.clone())
        .emit_with_profile_and_evidence(
            &profile,
            "source".into(),
            Digest::from_bytes([13; 32]),
            "provider".into(),
            "primitives".into(),
            "none".into(),
            metrics,
            b"artifact",
            true,
            (
                "rustc".into(),
                "release".into(),
                "local".into(),
                7,
                0,
                0,
                false,
            ),
            1,
            1,
            b"none",
            vec![Digest::from_bytes(*blake3::hash(b"artifact").as_bytes())],
            Vec::new(),
        )
        .unwrap();
    receipt
        .verify_for_profile_with_signer(
            "source",
            Digest::from_bytes([13; 32]),
            &profile,
            &[b"artifact"],
            key.verifying_key().to_bytes(),
        )
        .unwrap();
    assert!(
        receipt
            .verify_for_profile_with_signer(
                "source",
                Digest::from_bytes([13; 32]),
                &profile,
                &[b"artifact"],
                SigningKey::from_bytes(&[14; 32]).verifying_key().to_bytes(),
            )
            .is_err()
    );
    let mut below_threshold = receipt.clone();
    below_threshold.metrics[1].value = 0;
    assert!(below_threshold.verify_profile_thresholds(&profile).is_err());
    below_threshold.metrics[1].value = 1;
    below_threshold.metrics[2].value = 0;
    assert!(below_threshold.verify_profile_thresholds(&profile).is_err());
}

#[test]
fn multi_artifact_receipts_require_all_raw_bytes() {
    let primary = b"primary";
    let secondary = b"secondary";
    let runner = QualificationRunner::new(SigningKey::from_bytes(&[10; 32]));
    let receipt = runner
        .emit_with_evidence(
            "source".into(),
            Digest::from_bytes([11; 32]),
            "local".into(),
            "storage".into(),
            "none".into(),
            Vec::new(),
            primary,
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
            1,
            2,
            b"none",
            vec![
                Digest::from_bytes(*blake3::hash(primary).as_bytes()),
                Digest::from_bytes(*blake3::hash(secondary).as_bytes()),
            ],
            Vec::new(),
        )
        .unwrap();
    receipt
        .verify_for_artifacts(
            "source",
            Digest::from_bytes([11; 32]),
            &[primary, secondary],
        )
        .unwrap();
    assert!(
        receipt
            .verify_for("source", Digest::from_bytes([11; 32]), primary)
            .is_err()
    );
}

#[test]
fn receipt_rejects_unbounded_or_non_ascii_identity() {
    assert!(
        QualificationReceipt::new(
            "\n".into(),
            Digest::from_bytes([1; 32]),
            "provider".into(),
            "workload".into(),
            "fault".into(),
            Vec::new(),
            Digest::from_bytes([2; 32]),
            false,
        )
        .is_err()
    );
    assert!(QualificationReceipt::decode(&vec![b' '; MAX_RECEIPT_BYTES + 1]).is_err());
}
