//! Receipt encoding, measurement identities, and byte bounds.

use super::*;

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
