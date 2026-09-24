//! Threshold, coverage, and environment profiles.

use super::*;

#[test]
fn threshold_profiles_are_canonical_and_distinct() {
    let contract = QualificationProfile::pr_contract();
    let local = QualificationProfile::local_provider();
    let scale = QualificationProfile::scale();
    let fault = QualificationProfile::fault();
    let provider = QualificationProfile::provider();
    let compatibility = QualificationProfile::compatibility();
    assert_eq!(
        contract.schema_version,
        QUALIFICATION_PROFILE_SCHEMA_VERSION
    );
    assert!(contract.minimum_operations() < local.minimum_operations());
    assert!(local.minimum_cells() < scale.minimum_cells());
    assert!(!contract.requires_lifecycle_case_coverage());
    assert!(local.requires_lifecycle_case_coverage());
    assert!(scale.requires_lifecycle_case_coverage());
    assert!(fault.requires_lifecycle_case_coverage());
    assert!(provider.requires_lifecycle_case_coverage());
    assert!(compatibility.requires_lifecycle_case_coverage());
    assert_eq!(fault.name(), "fault-v1");
    assert_eq!(provider.name(), "provider-v1");
    assert_eq!(compatibility.name(), "compatibility-v1");
    for (profile, name, provider, topology) in [
        (
            QualificationProfile::provider_s3(),
            "provider-s3-v1",
            "s3",
            "three-process",
        ),
        (
            QualificationProfile::provider_gcs(),
            "provider-gcs-v1",
            "gcs",
            "three-process",
        ),
        (
            QualificationProfile::provider_azure(),
            "provider-azure-v1",
            "azure",
            "three-process",
        ),
        (
            QualificationProfile::fault_s3(),
            "fault-s3-v1",
            "s3",
            "kubernetes",
        ),
        (
            QualificationProfile::fault_gcs(),
            "fault-gcs-v1",
            "gcs",
            "kubernetes",
        ),
        (
            QualificationProfile::fault_azure(),
            "fault-azure-v1",
            "azure",
            "kubernetes",
        ),
    ] {
        assert_eq!(profile.name(), name);
        assert_eq!(profile.required_provider(), provider);
        assert_eq!(profile.required_topology(), topology);
        assert!(profile.requires_protected_evidence());
    }
    assert_ne!(contract.digest().unwrap(), local.digest().unwrap());
    assert_eq!(
        QualificationProfile::decode(&contract.encode().unwrap()).unwrap(),
        contract
    );
}

#[test]
fn threshold_profile_constructor_rejects_unbounded_workloads() {
    assert!(
        QualificationProfile::new(
            "too-many-cells".into(),
            MAX_QUALIFICATION_CELLS + 1,
            1,
            1,
            1,
        )
        .is_err()
    );
    assert!(
        QualificationProfile::new(
            "too-many-operations".into(),
            1,
            MAX_QUALIFICATION_OPERATIONS + 1,
            1,
            1,
        )
        .is_err()
    );
    assert!(
        QualificationProfile::new(
            "too-long".into(),
            1,
            1,
            MAX_QUALIFICATION_DURATION_SECS + 1,
            1,
        )
        .is_err()
    );
}

#[test]
fn tracked_profile_newline_does_not_change_its_canonical_digest() {
    let profile = QualificationProfile::scale();
    let mut encoded = profile.encode().unwrap();
    encoded.extend_from_slice(b"\n");
    assert_eq!(QualificationProfile::decode(&encoded).unwrap(), profile);
    encoded.extend_from_slice(b" ");
    assert_eq!(QualificationProfile::decode(&encoded).unwrap(), profile);
}

#[test]
fn qualification_schedule_reaches_each_outcome_hint() {
    let profile = QualificationProfile::new("hint-run".into(), 1, 8, 1, 1_000).unwrap();
    let workload = QualificationWorkload::generate_with_size(&profile, 41, 1, 2_048, 1).unwrap();
    let operations = workload.iter_operations().collect::<Vec<_>>();
    assert!(operations.iter().any(|operation| operation.retry_hint()));
    assert!(
        operations
            .iter()
            .any(|operation| operation.rejection_hint())
    );
    assert!(
        operations
            .iter()
            .any(|operation| operation.ambiguous_hint())
    );
    assert!(
        operations
            .iter()
            .filter(|operation| operation.ambiguous_hint())
            .all(|operation| !operation.rejection_hint())
    );
}

#[test]
fn qualification_schedule_covers_each_lifecycle_case_per_primitive() {
    let profile = QualificationProfile::new("case-run".into(), 1, 1, 1, 1_000).unwrap();
    let workload = QualificationWorkload::generate(&profile, 41).unwrap();
    assert_eq!(
        workload.operations(),
        QUALIFICATION_CASE_COVERAGE_OPERATIONS
    );
    for primitive in QUALIFICATION_PRIMITIVES {
        for case in QUALIFICATION_CASES {
            assert!(
                workload
                    .iter_operations()
                    .any(|operation| operation.primitive() == *primitive
                        && operation.case() == *case),
                "missing {} case for {primitive}",
                case.name()
            );
        }
    }
}

#[test]
fn protected_profiles_require_measured_nonlocal_environment() {
    let profile = QualificationProfile::scale();
    assert!(profile.requires_protected_evidence());
    let renamed_profile =
        QualificationProfile::new("pr-contract-v1".into(), 2, 1, 1, 5_000).unwrap();
    assert!(renamed_profile.requires_protected_evidence());
    let image = Digest::from_bytes([31; 32]);
    let key = SigningKey::from_bytes(&[32; 32]);
    let artifact = b"scale-evidence";
    let artifact_digest = Digest::from_bytes(*blake3::hash(artifact).as_bytes());
    let metrics = vec![
        QualificationMetric::new("cells".into(), 10_000, "cells".into()).unwrap(),
        QualificationMetric::new("operations".into(), 10_000_000, "operations".into()).unwrap(),
        QualificationMetric::new("duration_secs".into(), 3_600, "seconds".into()).unwrap(),
        QualificationMetric::new("p99_latency_ms".into(), 500, "ms".into()).unwrap(),
        QualificationMetric::new("peak_local_disk_bytes".into(), 1, "bytes".into()).unwrap(),
        QualificationMetric::new("peak_file_descriptors".into(), 1, "count".into()).unwrap(),
    ];
    let local = QualificationRunner::new(key.clone())
        .emit_with_profile_and_evidence(
            &profile,
            "source".into(),
            image,
            "rustfs".into(),
            "storage".into(),
            "none".into(),
            metrics.clone(),
            artifact,
            true,
            (
                "rustc".into(),
                "release".into(),
                "local".into(),
                7,
                1,
                1,
                false,
            ),
            1,
            3_600_001,
            b"none",
            vec![artifact_digest],
            Vec::new(),
        )
        .unwrap();
    assert!(
        local
            .verify_for_profile_with_signer(
                "source",
                image,
                &profile,
                &[artifact],
                key.verifying_key().to_bytes(),
            )
            .is_err()
    );

    let protected = QualificationRunner::new(key.clone())
        .emit_with_profile_and_evidence(
            &profile,
            "source".into(),
            image,
            "rustfs".into(),
            "storage".into(),
            "none".into(),
            metrics,
            artifact,
            true,
            (
                "rustc".into(),
                "release".into(),
                "dedicated-hosts".into(),
                7,
                1,
                1,
                false,
            ),
            1,
            3_600_001,
            b"none",
            vec![artifact_digest],
            vec![QualificationOwnership::new(
                2,
                3,
                Digest::from_bytes([33; 32]),
            )],
        )
        .unwrap();
    protected
        .verify_for_profile_with_signer(
            "source",
            image,
            &profile,
            &[artifact],
            key.verifying_key().to_bytes(),
        )
        .unwrap();

    let debug_execution = protected
        .clone()
        .with_execution(
            "rustc".into(),
            "debug".into(),
            "dedicated-hosts".into(),
            7,
            1,
            1,
            false,
        )
        .unwrap()
        .attest(&key)
        .unwrap();
    assert!(
        debug_execution
            .verify_for_profile_with_signer(
                "source",
                image,
                &profile,
                &[artifact],
                key.verifying_key().to_bytes(),
            )
            .is_err(),
        "protected evidence must come from a release execution profile"
    );

    for metric_name in ["peak_local_disk_bytes", "peak_file_descriptors"] {
        let mut missing_measurement = protected.clone();
        missing_measurement
            .metrics
            .iter_mut()
            .find(|metric| metric.name() == metric_name)
            .expect("resource metric")
            .value = 0;
        assert!(
            missing_measurement
                .verify_profile_thresholds(&profile)
                .is_err(),
            "zero {metric_name} must not stand in for a protected measurement"
        );
    }
}

#[test]
fn fault_profiles_require_injected_schedule_and_monotonic_ownership() {
    let profile = QualificationProfile::fault_s3();
    assert!(profile.requires_fault_injection());
    let image = Digest::from_bytes([34; 32]);
    let artifact = b"fault-evidence";
    let artifact_digest = Digest::from_bytes(*blake3::hash(artifact).as_bytes());
    let metrics = vec![
        QualificationMetric::new("cells".into(), 256, "cells".into()).unwrap(),
        QualificationMetric::new("operations".into(), 1_000_000, "operations".into()).unwrap(),
        QualificationMetric::new("duration_secs".into(), 60, "seconds".into()).unwrap(),
        QualificationMetric::new("p99_latency_ms".into(), 1, "ms".into()).unwrap(),
        QualificationMetric::new("peak_local_disk_bytes".into(), 1, "bytes".into()).unwrap(),
        QualificationMetric::new("peak_file_descriptors".into(), 1, "count".into()).unwrap(),
    ];
    let key = SigningKey::from_bytes(&[35; 32]);
    let runner = QualificationRunner::new(key.clone());
    let incomplete = runner
        .emit_with_profile_and_evidence(
            &profile,
            "fault-source".into(),
            image,
            "s3".into(),
            "failover".into(),
            "none".into(),
            metrics.clone(),
            artifact,
            true,
            (
                "rustc".into(),
                "release".into(),
                "kubernetes".into(),
                7,
                1,
                1,
                false,
            ),
            1,
            60_001,
            b"none",
            vec![artifact_digest],
            vec![QualificationOwnership::new(
                1,
                1,
                Digest::from_bytes([36; 32]),
            )],
        )
        .unwrap();
    assert!(
        incomplete
            .verify_for_profile_with_signer(
                "fault-source",
                image,
                &profile,
                &[artifact],
                key.verifying_key().to_bytes(),
            )
            .is_err()
    );

    let complete = runner
        .emit_with_profile_and_evidence(
            &profile,
            "fault-source".into(),
            image,
            "s3".into(),
            "failover".into(),
            "owner-kill".into(),
            metrics,
            artifact,
            true,
            (
                "rustc".into(),
                "release".into(),
                "kubernetes".into(),
                7,
                1,
                1,
                false,
            ),
            1,
            60_001,
            b"fault=owner-kill;phase=after-publication",
            vec![artifact_digest],
            vec![
                QualificationOwnership::new(1, 1, Digest::from_bytes([36; 32])),
                QualificationOwnership::new(2, 2, Digest::from_bytes([37; 32])),
            ],
        )
        .unwrap();
    complete
        .verify_for_profile_with_signer(
            "fault-source",
            image,
            &profile,
            &[artifact],
            key.verifying_key().to_bytes(),
        )
        .unwrap();

    let no_transition = runner
        .emit_with_profile_and_evidence(
            &profile,
            "fault-source".into(),
            image,
            "s3".into(),
            "failover".into(),
            "owner-kill".into(),
            vec![
                QualificationMetric::new("cells".into(), 256, "cells".into()).unwrap(),
                QualificationMetric::new("operations".into(), 1_000_000, "operations".into())
                    .unwrap(),
                QualificationMetric::new("duration_secs".into(), 60, "seconds".into()).unwrap(),
                QualificationMetric::new("p99_latency_ms".into(), 1, "ms".into()).unwrap(),
                QualificationMetric::new("peak_local_disk_bytes".into(), 1, "bytes".into())
                    .unwrap(),
                QualificationMetric::new("peak_file_descriptors".into(), 1, "count".into())
                    .unwrap(),
            ],
            artifact,
            true,
            (
                "rustc".into(),
                "release".into(),
                "kubernetes".into(),
                7,
                1,
                1,
                false,
            ),
            1,
            60_001,
            b"fault=owner-kill;phase=after-publication",
            vec![artifact_digest],
            vec![
                QualificationOwnership::new(1, 1, Digest::from_bytes([36; 32])),
                QualificationOwnership::new(1, 1, Digest::from_bytes([36; 32])),
            ],
        )
        .unwrap();
    assert!(
        no_transition
            .verify_for_profile_with_signer(
                "fault-source",
                image,
                &profile,
                &[artifact],
                key.verifying_key().to_bytes(),
            )
            .is_err(),
        "fault evidence must contain an actual ownership transition"
    );
}
