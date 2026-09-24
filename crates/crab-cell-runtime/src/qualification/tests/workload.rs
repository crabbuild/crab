//! Workload generation, streaming, and bounded execution.

use super::*;

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
