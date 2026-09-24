//! Qualification gating for a starting node.

use super::*;

#[derive(Clone)]
struct QualificationStub;

impl QualificationOperationExecutor for QualificationStub {
    type Future<'a> = std::future::Ready<crab_cell_runtime::Result<QualificationExecution>>;

    fn execute<'a>(&'a mut self, operation: QualificationOperation) -> Self::Future<'a> {
        std::future::ready(Ok(
            QualificationExecution::acknowledged(true).with_case(operation.case())
        ))
    }
}

#[tokio::test]
async fn qualification_rejects_a_node_before_readiness() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([32; 16]))
        .build()
        .unwrap();
    let workload = QualificationWorkload::generate_with_size(
        &QualificationProfile::pr_contract(),
        41,
        1,
        56,
        1,
    )
    .unwrap();
    let error = node
        .run_qualification(&workload, &mut QualificationStub)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::CellDraining));
}

#[tokio::test]
async fn concurrent_qualification_is_readiness_gated_and_bounded() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([35; 16]))
        .build()
        .unwrap();
    node.install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    node.install_node_lease(NodeLeaseGuard::new(0, 60_000).unwrap())
        .unwrap();
    let workload = QualificationWorkload::generate_with_size(
        &QualificationProfile::pr_contract(),
        41,
        1,
        56,
        1,
    )
    .unwrap();
    let summary = node
        .run_qualification_concurrent(&workload, QualificationStub, 2)
        .await
        .unwrap();
    assert_eq!(summary.operations(), 56);
    node.shutdown().await.unwrap();
}

struct ObservedQualificationStub;

impl QualificationOperationExecutor for ObservedQualificationStub {
    type Future<'a> = std::future::Ready<crab_cell_runtime::Result<QualificationExecution>>;

    fn execute<'a>(&'a mut self, _operation: QualificationOperation) -> Self::Future<'a> {
        std::future::ready(Ok(QualificationExecution::acknowledged(true)))
    }
}

#[tokio::test]
async fn observed_qualification_preserves_unclaimed_case_coverage() {
    let node = CellNodeBuilder::new(application())
        .with_runtime(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(ReplicaHost::default())
        .with_session(SessionId::from_bytes([36; 16]))
        .build()
        .unwrap();
    node.install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    node.install_node_lease(NodeLeaseGuard::new(0, 60_000).unwrap())
        .unwrap();
    let workload = QualificationWorkload::generate_with_size(
        &QualificationProfile::pr_contract(),
        41,
        1,
        56,
        1,
    )
    .unwrap();
    let summary = node
        .run_qualification_observed(&workload, &mut ObservedQualificationStub)
        .await
        .unwrap();
    assert!(summary.case_coverage().iter().all(|byte| *byte == 0));
    node.shutdown().await.unwrap();
}
