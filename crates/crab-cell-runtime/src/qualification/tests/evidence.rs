//! Execution evidence and ownership watermarks.

use super::*;

#[test]
fn execution_evidence_round_trip_is_canonical_and_bounded() {
    let evidence = QualificationExecutionEvidence {
        provider: "s3".into(),
        workload: "primitives".into(),
        fault: "owner-loss".into(),
        toolchain: "rustc-1.90".into(),
        execution_profile: "release".into(),
        topology: "kubernetes".into(),
        started_at_ms: 10,
        finished_at_ms: 20,
        fault_schedule: b"owner-loss-before-commit".to_vec(),
        ownership: vec![QualificationOwnership::new(
            4,
            8,
            Digest::from_bytes([7; 32]),
        )],
        dirty: false,
    };
    let encoded = evidence.encode().expect("evidence encoding");
    assert_eq!(
        QualificationExecutionEvidence::decode(&encoded).expect("evidence decoding"),
        evidence
    );
    let mut noncanonical = encoded;
    noncanonical.push(b'\n');
    assert!(QualificationExecutionEvidence::decode(&noncanonical).is_err());
}

#[test]
fn evidence_binds_fault_artifacts_and_ownership_watermarks() {
    let artifact = b"raw qualification output";
    let runner = QualificationRunner::new(SigningKey::from_bytes(&[6; 32]));
    let receipt = runner
        .emit_with_evidence(
            "abc".into(),
            Digest::from_bytes([1; 32]),
            "rustfs".into(),
            "failover".into(),
            "lost-release-reply".into(),
            Vec::new(),
            artifact,
            true,
            (
                "rustc".into(),
                "release".into(),
                "three-node".into(),
                7,
                8,
                9,
                false,
            ),
            10,
            20,
            b"seed=7;fault=lost-release-reply",
            vec![Digest::from_bytes(*blake3::hash(artifact).as_bytes())],
            vec![QualificationOwnership::new(
                4,
                12,
                Digest::from_bytes([3; 32]),
            )],
        )
        .unwrap();
    let encoded = receipt.encode().unwrap();
    let decoded = QualificationReceipt::decode(&encoded).unwrap();
    assert_eq!(decoded.started_at_ms(), 10);
    assert_eq!(decoded.finished_at_ms(), 20);
    assert_eq!(decoded.ownership()[0].epoch(), 4);
    assert_eq!(decoded.ownership()[0].published_sequence(), 12);
    assert!(
        decoded
            .raw_artifact_digests()
            .any(|digest| { digest == Digest::from_bytes(*blake3::hash(artifact).as_bytes()) })
    );
}

#[test]
fn evidence_requires_bounded_fault_schedule_and_nonzero_ownership_roots() {
    let valid = QualificationExecutionEvidence {
        provider: "rustfs".into(),
        workload: "failover".into(),
        fault: "owner-kill".into(),
        toolchain: "rustc".into(),
        execution_profile: "release".into(),
        topology: "three-process".into(),
        started_at_ms: 1,
        finished_at_ms: 2,
        fault_schedule: b"owner-kill".to_vec(),
        ownership: Vec::new(),
        dirty: false,
    };
    assert!(valid.encode().is_ok());

    let mut empty = valid.clone();
    empty.fault_schedule.clear();
    assert!(empty.encode().is_err());

    let mut zero_root = valid.clone();
    zero_root.ownership = vec![QualificationOwnership::new(
        1,
        1,
        Digest::from_bytes([0; 32]),
    )];
    assert!(zero_root.encode().is_err());

    let mut oversized = valid;
    oversized.fault_schedule = vec![0; MAX_RECEIPT_BYTES + 1];
    assert!(oversized.encode().is_err());

    let artifact = b"raw qualification output";
    let digest = Digest::from_bytes(*blake3::hash(artifact).as_bytes());
    let receipt = QualificationReceipt::new(
        "source".into(),
        Digest::from_bytes([1; 32]),
        "rustfs".into(),
        "failover".into(),
        "owner-kill".into(),
        Vec::new(),
        digest,
        true,
    )
    .expect("receipt identity");
    assert!(
        receipt
            .clone()
            .with_evidence(1, 2, b"", vec![digest], Vec::new())
            .is_err()
    );
    assert!(
        receipt
            .clone()
            .with_evidence(
                1,
                2,
                b"owner-kill",
                vec![digest],
                vec![QualificationOwnership::new(
                    1,
                    1,
                    Digest::from_bytes([0; 32]),
                )]
            )
            .is_err()
    );
    assert!(
        receipt
            .with_evidence(
                1,
                2,
                &vec![0; MAX_RECEIPT_BYTES + 1],
                vec![digest],
                Vec::new()
            )
            .is_err()
    );
}
