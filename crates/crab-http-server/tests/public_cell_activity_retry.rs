use std::{
    sync::atomic::Ordering,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use cellule_runtime::{
    ActivityRunOutcome, ActivitySupervisor, ActivitySupervisorError, Resolution, StoredOutcome,
    WorkflowOutcome, WorkflowStatus,
};

#[path = "support/reference_application.rs"]
mod fixture;
#[path = "support/qualification.rs"]
mod qualification;
#[path = "support/qualification_fixture.rs"]
mod qualification_fixture;
#[path = "support/qualification_local_fixture.rs"]
mod qualification_local_fixture;
#[path = "support/qualification_peer.rs"]
#[expect(
    dead_code,
    reason = "this test uses only the response-loss peer fault helper"
)]
mod qualification_peer;

use qualification::{assert_zero_reservations, identity, rustfs_public_store};
use qualification_fixture::{PublicHostFixture, public_host_fixture_with_store};
use qualification_local_fixture::public_host_fixture;
use qualification_peer::peer_client_with_one_lost_mutation;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_activity_absent_claim_runs_once_after_resolution() {
    run_activity_claim_fault(public_host_fixture().await, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_activity_absent_claim_runs_once_after_resolution() {
    let (store, root) = rustfs_public_store();
    run_activity_claim_fault(public_host_fixture_with_store(store, root).await, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_activity_lost_committed_claim_reclaims_after_lease_expiry() {
    run_activity_claim_fault(public_host_fixture().await, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_activity_lost_committed_claim_reclaims_after_lease_expiry() {
    let (store, root) = rustfs_public_store();
    run_activity_claim_fault(public_host_fixture_with_store(store, root).await, false).await;
}

async fn run_activity_claim_fault(
    (node, observer, tenant, application, _directory, registry, handles, _store): PublicHostFixture,
    drop_before_dispatch: bool,
) {
    let (peer_client, dropped, dispatched) =
        peer_client_with_one_lost_mutation(registry, handles, drop_before_dispatch, 1);
    let peer =
        node.application_handle::<fixture::ReferenceApplication>(peer_client, tenant, application);
    let peer_activity = ActivitySupervisor::new(
        peer.activities::<fixture::ReferenceWorkflow>()
            .expect("peer Activity"),
        5_000,
    )
    .expect("peer Activity supervisor");
    let workflow = observer
        .workflow::<fixture::ReferenceWorkflow>()
        .expect("observer Workflow");
    let now_ms = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("qualification clock")
            .as_millis(),
    )
    .expect("qualification clock bounds");
    let workflow_id = b"activity-claim-after-absence".to_vec();
    let started = workflow
        .start(
            identity(220, now_ms),
            workflow_id.clone(),
            b"activity".to_vec(),
        )
        .await
        .expect("source Activity workflow start");
    let WorkflowOutcome::Applied {
        run_id,
        status: WorkflowStatus::Running,
        ..
    } = started.output
    else {
        panic!("Activity workflow did not start running");
    };
    let pending = match peer_activity.run_once(0, None).await {
        Err(ActivitySupervisorError::Pending(pending)) => pending,
        outcome => panic!("expected a lost Activity claim: {outcome:?}"),
    };
    assert_eq!(dropped.load(Ordering::Acquire), 1);
    let resolution = peer
        .resolve(&pending)
        .await
        .expect("Activity claim resolution");
    if drop_before_dispatch {
        assert_eq!(dispatched.load(Ordering::Acquire), 0);
        assert_eq!(resolution, Resolution::Absent);
    } else {
        assert_eq!(dispatched.load(Ordering::Acquire), 1);
        assert!(matches!(
            resolution,
            Resolution::Committed(StoredOutcome::Success { .. })
        ));
        let still_running = workflow
            .state(workflow_id.clone(), None)
            .await
            .expect("lost-claim workflow observation");
        assert_eq!(
            still_running.output.expect("claimed workflow").status,
            WorkflowStatus::Running
        );
        tokio::time::sleep(Duration::from_secs(7)).await;
    }
    let completed = peer_activity
        .run_once(0, None)
        .await
        .expect("Activity run after claim fault");
    assert!(matches!(completed, ActivityRunOutcome::Completed { .. }));
    assert_eq!(
        dispatched.load(Ordering::Acquire),
        if drop_before_dispatch { 2 } else { 3 }
    );
    let observed = workflow
        .state(workflow_id, None)
        .await
        .expect("independent Activity observation");
    let run = observed.output.expect("completed Activity workflow");
    assert_eq!(run.run_id, run_id);
    assert_eq!(run.status, WorkflowStatus::Completed);
    assert!(run.result.as_deref().is_some_and(
        |result| result.starts_with(b"activity\0") && result.ends_with(b"activity-result")
    ));
    assert!(matches!(
        peer_activity
            .run_once(0, None)
            .await
            .expect("Activity settled check"),
        ActivityRunOutcome::Idle { .. }
    ));

    drop(peer);
    drop(observer);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_activity_absent_completion_retries_exact_attempt() {
    run_activity_completion_fault(public_host_fixture().await, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_activity_absent_completion_retries_exact_attempt() {
    let (store, root) = rustfs_public_store();
    run_activity_completion_fault(public_host_fixture_with_store(store, root).await, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_activity_lost_committed_completion_resolves_exact_attempt() {
    run_activity_completion_fault(public_host_fixture().await, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_activity_lost_committed_completion_resolves_exact_attempt() {
    let (store, root) = rustfs_public_store();
    run_activity_completion_fault(public_host_fixture_with_store(store, root).await, false).await;
}

async fn run_activity_completion_fault(
    (node, observer, tenant, application, _directory, registry, handles, _store): PublicHostFixture,
    drop_before_dispatch: bool,
) {
    let (peer_client, dropped, dispatched) =
        peer_client_with_one_lost_mutation(registry, handles, drop_before_dispatch, 2);
    let peer =
        node.application_handle::<fixture::ReferenceApplication>(peer_client, tenant, application);
    let peer_activity = ActivitySupervisor::new(
        peer.activities::<fixture::ReferenceWorkflow>()
            .expect("peer Activity"),
        5_000,
    )
    .expect("peer Activity supervisor");
    let workflow = observer
        .workflow::<fixture::ReferenceWorkflow>()
        .expect("observer Workflow");
    let now_ms = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("qualification clock")
            .as_millis(),
    )
    .expect("qualification clock bounds");
    let workflow_id = b"activity-completion-response-loss".to_vec();
    let started = workflow
        .start(
            identity(221, now_ms),
            workflow_id.clone(),
            b"activity".to_vec(),
        )
        .await
        .expect("source Activity workflow start");
    let WorkflowOutcome::Applied {
        run_id,
        status: WorkflowStatus::Running,
        ..
    } = started.output
    else {
        panic!("Activity workflow did not start running");
    };
    let outcome = peer_activity
        .run_once(0, None)
        .await
        .expect("Activity completion fault recovery");
    let ActivityRunOutcome::Completed { receipt, .. } = outcome else {
        panic!("Activity did not complete after response fault: {outcome:?}");
    };
    assert_eq!(dropped.load(Ordering::Acquire), 1);
    assert_eq!(dispatched.load(Ordering::Acquire), 2);
    let observed = workflow
        .state(workflow_id, Some(receipt))
        .await
        .expect("independent Activity completion observation");
    let run = observed.output.expect("completed Activity workflow");
    assert_eq!(run.run_id, run_id);
    assert_eq!(run.status, WorkflowStatus::Completed);
    assert_eq!(run.event_sequence, 2);
    assert!(run.result.as_deref().is_some_and(
        |result| result.starts_with(b"activity\0") && result.ends_with(b"activity-result")
    ));
    assert!(matches!(
        peer_activity
            .run_once(0, None)
            .await
            .expect("Activity settled check"),
        ActivityRunOutcome::Idle { .. }
    ));

    drop(peer);
    drop(observer);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}
