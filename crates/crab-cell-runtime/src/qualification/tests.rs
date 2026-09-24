use super::profile::{
    MAX_QUALIFICATION_CELLS, MAX_QUALIFICATION_CONCURRENCY, MAX_QUALIFICATION_DURATION_SECS,
    MAX_QUALIFICATION_OPERATIONS, MAX_RECEIPT_BYTES,
};
use super::workload::qualification_run_outcome_digest;
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
fn mixed_workload_is_seed_deterministic_and_covers_every_primitive() {
    let profile = QualificationProfile::pr_contract();
    let first = QualificationWorkload::generate(&profile, 7).unwrap();
    let second = QualificationWorkload::generate(&profile, 7).unwrap();
    let changed = QualificationWorkload::generate(&profile, 8).unwrap();
    assert_eq!(first, second);
    assert_ne!(first.outcome_digest(), changed.outcome_digest());
    assert_eq!(first.primitives().len(), QUALIFICATION_PRIMITIVES.len());
    assert!(
        first
            .primitives()
            .iter()
            .all(|counts| counts.attempted() > 0 && counts.verified() > 0)
    );
    for seed in 0..32 {
        let workload = QualificationWorkload::generate(&profile, seed).unwrap();
        assert!(
            workload
                .primitives()
                .iter()
                .all(|counts| counts.attempted() > 0)
        );
    }
    first.verify_for_profile(&profile).unwrap();
    let encoded = first.encode().unwrap();
    assert_eq!(QualificationWorkload::decode(&encoded).unwrap(), first);

    let mut forged = first.clone();
    forged.outcome_digest[0] ^= 1;
    assert!(forged.encode().is_err());
}

#[test]
fn workload_bounds_and_counter_overflow_fail_closed() {
    let profile = QualificationProfile::pr_contract();
    let mut workload = QualificationWorkload::generate(&profile, 7).unwrap();
    workload.operations = MAX_QUALIFICATION_OPERATIONS + 1;
    assert!(workload.encode().is_err());

    workload.operations = 8;
    workload.primitives[0].attempted = u64::MAX;
    workload.primitives[0].acknowledged = u64::MAX;
    workload.primitives[0].rejected = u64::MAX;
    assert!(workload.encode().is_err());
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

struct ContractExecutor {
    calls: u64,
    case_coverage: bool,
}

impl QualificationOperationExecutor for ContractExecutor {
    type Future<'a> = std::future::Ready<Result<QualificationExecution>>;

    fn execute<'a>(&'a mut self, operation: QualificationOperation) -> Self::Future<'a> {
        self.calls = self.calls.saturating_add(1);
        let execution = if operation.rejection_hint() {
            QualificationExecution::rejected()
        } else if operation.ambiguous_hint() {
            QualificationExecution::ambiguous(u64::from(operation.retry_hint()))
        } else {
            QualificationExecution::acknowledged(true)
                .with_retries(u64::from(operation.retry_hint()))
        };
        let execution = if self.case_coverage {
            execution.with_case(operation.case())
        } else {
            execution
        };
        std::future::ready(Ok(execution))
    }
}

#[derive(Clone)]
struct ConcurrentExecutor {
    active: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    maximum: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    fail_at: Option<u64>,
}

impl QualificationOperationExecutor for ConcurrentExecutor {
    type Future<'a> =
        std::pin::Pin<Box<dyn Future<Output = Result<QualificationExecution>> + Send + 'a>>;

    fn execute<'a>(&'a mut self, operation: QualificationOperation) -> Self::Future<'a> {
        let active = std::sync::Arc::clone(&self.active);
        let maximum = std::sync::Arc::clone(&self.maximum);
        let fail_at = self.fail_at;
        Box::pin(async move {
            let current = active.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
            maximum.fetch_max(current, std::sync::atomic::Ordering::AcqRel);
            tokio::task::yield_now().await;
            active.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            if Some(operation.index()) == fail_at {
                return Err(Error::Control("qualification executor failed"));
            }
            let execution = if operation.rejection_hint() {
                QualificationExecution::rejected()
            } else if operation.ambiguous_hint() {
                QualificationExecution::ambiguous(u64::from(operation.retry_hint()))
            } else {
                QualificationExecution::acknowledged(true)
                    .with_retries(u64::from(operation.retry_hint()))
            };
            Ok(execution)
        })
    }
}

#[tokio::test]
async fn workload_iterator_and_executor_are_streaming_and_reproducible() {
    let profile = QualificationProfile::new("contract-run".into(), 1, 32, 1, 1_000).unwrap();
    let workload = QualificationWorkload::generate_with_size(&profile, 41, 3, 32, 1).unwrap();
    let operations = workload.iter_operations().collect::<Vec<_>>();
    assert_eq!(operations.len(), 32);
    for (index, operation) in operations.iter().copied().enumerate() {
        assert_eq!(workload.operation_at(index as u64).unwrap(), operation);
    }
    let mut executor = ContractExecutor {
        calls: 0,
        case_coverage: false,
    };
    let summary = workload.run(&mut executor).await.unwrap();
    assert_eq!(executor.calls, workload.operations());
    assert_eq!(summary.operations(), workload.operations());
    assert_eq!(summary.cells(), workload.cells());
    assert!(
        summary
            .primitive_counts()
            .iter()
            .all(|counts| counts.attempted() > 0 && counts.verified() > 0)
    );
    assert!(
        summary
            .metrics()
            .unwrap()
            .iter()
            .any(|metric| { metric.name() == "p99_latency_ms" && metric.unit() == "ms" })
    );
    assert_ne!(summary.outcome_digest(), workload.outcome_digest());
}

#[tokio::test]
async fn case_coverage_runner_rejects_unverified_lifecycle_hints() {
    let profile = QualificationProfile::pr_contract();
    let workload = QualificationWorkload::generate(&profile, 41).unwrap();
    let mut missing = ContractExecutor {
        calls: 0,
        case_coverage: false,
    };
    assert!(workload.run_with_case_coverage(&mut missing).await.is_err());

    let mut covered = ContractExecutor {
        calls: 0,
        case_coverage: true,
    };
    let summary = workload.run_with_case_coverage(&mut covered).await.unwrap();
    assert!(summary.case_coverage().iter().all(|byte| *byte == u8::MAX));
    summary.artifact(&workload).unwrap();
}

#[tokio::test]
async fn concurrent_workload_runner_bounds_inflight_operations() {
    let profile = QualificationProfile::new("concurrent-run".into(), 1, 32, 1, 1_000).unwrap();
    let workload = QualificationWorkload::generate_with_size(&profile, 41, 2, 32, 1).unwrap();
    let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let maximum = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let executor = ConcurrentExecutor {
        active: std::sync::Arc::clone(&active),
        maximum: std::sync::Arc::clone(&maximum),
        fail_at: None,
    };
    assert!(workload.run_concurrent(executor.clone(), 0).await.is_err());
    assert!(
        workload
            .run_concurrent(executor.clone(), MAX_QUALIFICATION_CONCURRENCY + 1)
            .await
            .is_err()
    );
    let summary = workload.run_concurrent(executor, 4).await.unwrap();
    assert_eq!(summary.operations(), workload.operations());
    assert!(maximum.load(std::sync::atomic::Ordering::Acquire) >= 2);
    assert!(maximum.load(std::sync::atomic::Ordering::Acquire) <= 4);
    summary.artifact(&workload).unwrap().encode().unwrap();
}

#[tokio::test]
async fn concurrent_workload_runner_drains_inflight_operations_after_failure() {
    let profile = QualificationProfile::new("concurrent-failure".into(), 1, 32, 1, 1_000).unwrap();
    let workload = QualificationWorkload::generate_with_size(&profile, 41, 2, 32, 1).unwrap();
    let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let maximum = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let executor = ConcurrentExecutor {
        active: std::sync::Arc::clone(&active),
        maximum,
        fail_at: Some(0),
    };
    assert!(workload.run_concurrent(executor, 4).await.is_err());
    assert_eq!(active.load(std::sync::atomic::Ordering::Acquire), 0);
}

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
