//! Runner and protected verification over signed receipts.

use super::*;

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
