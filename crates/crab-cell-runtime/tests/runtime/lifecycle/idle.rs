//! Idle release, eviction, churn, and capacity release.

use super::*;

mod acquire;
mod bootstrap;
mod churn;
mod release;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_targets_preserve_tenants_after_cancelled_caller() {
    let first = Arc::new(fixture());
    let mut second = fixture_with_limits_and_store(
        first.target.partition(),
        Limits::default(),
        first.layout.store().clone(),
    );
    second.target = CellTarget::new(
        TenantId::from_bytes([9; 16]),
        first.target.application(),
        first.target.namespace(),
        first.target.partition(),
    )
    .unwrap();
    second.replica = CellReplica::new(
        second.layout.clone(),
        *second.target.cell_id().as_bytes(),
        [2; 16],
        Limits::default(),
    )
    .unwrap();
    let session = SessionId::from_bytes([4; 16]);
    let runtime = CellRuntime::new(SqlWorkerPool::new(1, 2).unwrap(), 4 << 20, session).unwrap();
    let started = Arc::new(Notify::new());
    let activation = {
        let runtime = runtime.clone();
        let first = first.clone();
        let started = started.clone();
        tokio::spawn(async move {
            let handle = bootstrap_on(&runtime, &first, session).await;
            // The runtime has returned ownership, but caller-side bookkeeping
            // has not run. Cancellation here must not hide the resident owner.
            started.notify_one();
            std::future::pending::<()>().await;
            handle
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), started.notified())
        .await
        .unwrap();
    activation.abort();
    assert!(matches!(activation.await, Err(error) if error.is_cancelled()));
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while runtime.active_cell_targets().await.unwrap().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let second_handle = bootstrap_on(&runtime, &second, session).await;
    let mut targets = runtime.active_cell_targets().await.unwrap();
    targets.sort_by_key(|target| *target.cell_id().as_bytes());
    let mut expected = vec![first.target.clone(), second.target.clone()];
    expected.sort_by_key(|target| *target.cell_id().as_bytes());
    assert_eq!(targets, expected);
    second_handle.drain().await.unwrap();
    assert_eq!(
        runtime.active_cell_targets().await.unwrap(),
        vec![first.target.clone()]
    );
    runtime.shutdown().await.unwrap();
}
