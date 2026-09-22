use std::sync::atomic::Ordering;

use crab_cell_runtime::{
    ActivityClaim, ActivityCompletion, ActivityCompletionOutcome, CellTarget, Resolution,
    WorkflowActivityClaimCommand, WorkflowActivityClaimRequest, WorkflowActivityCompleteCommand,
    WorkflowActivityValidateQuery, WorkflowActivityValidateRequest, WorkflowOutcome,
    WorkflowStatus, partition_for_shard,
};

#[path = "support/reference_application.rs"]
mod fixture;
#[path = "support/qualification.rs"]
mod qualification;
#[path = "support/qualification_cancellation.rs"]
mod qualification_cancellation;
#[path = "support/qualification_fixture.rs"]
mod qualification_fixture;
#[path = "support/qualification_local_fixture.rs"]
mod qualification_local_fixture;
#[path = "support/qualification_peer.rs"]
#[expect(dead_code, reason = "this test uses the paused peer fault helper")]
mod qualification_peer;

use qualification::{assert_zero_reservations, fixed_id, identity, rustfs_public_store};
use qualification_cancellation::{cancel_prepared, committed_output, now_ms};
use qualification_fixture::{PublicHostFixture, public_host_fixture_with_store};
use qualification_local_fixture::public_host_fixture;
use qualification_peer::peer_client_with_paused_mutation;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_activity_claim_cancellation_before_dispatch_resolves_absent() {
    run_claim_cancellation(public_host_fixture().await, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_activity_claim_cancellation_before_dispatch_resolves_absent() {
    let (store, root) = rustfs_public_store();
    run_claim_cancellation(public_host_fixture_with_store(store, root).await, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_activity_claim_cancellation_after_dispatch_resolves_committed() {
    run_claim_cancellation(public_host_fixture().await, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_activity_claim_cancellation_after_dispatch_resolves_committed() {
    let (store, root) = rustfs_public_store();
    run_claim_cancellation(public_host_fixture_with_store(store, root).await, true).await;
}

async fn run_claim_cancellation(
    (node, observer, tenant, application, _directory, registry, handles, _store): PublicHostFixture,
    after_dispatch: bool,
) {
    let (peer_client, entered, dispatched) =
        peer_client_with_paused_mutation(registry, handles, after_dispatch);
    let peer =
        node.application_handle::<fixture::ReferenceApplication>(peer_client, tenant, application);
    let target = CellTarget::new(
        tenant,
        application,
        fixture::WORKFLOW_NAMESPACE,
        &partition_for_shard(0),
    )
    .expect("Activity target");
    let workflow = observer
        .workflow::<fixture::ReferenceWorkflow>()
        .expect("observer Workflow");
    let issued_at_ms = now_ms();
    let workflow_id = if after_dispatch {
        b"cancelled-activity-claim-after".to_vec()
    } else {
        b"cancelled-activity-claim-before".to_vec()
    };
    let started = workflow
        .start(
            identity(270 + u64::from(after_dispatch), issued_at_ms),
            workflow_id.clone(),
            b"activity".to_vec(),
        )
        .await
        .expect("source Activity publication");
    let WorkflowOutcome::Applied {
        run_id,
        status: WorkflowStatus::Running,
        event_sequence: 1,
    } = started.output
    else {
        panic!("source Activity workflow did not start");
    };
    let prepared = peer
        .prepare_command::<WorkflowActivityClaimCommand<fixture::ReferenceWorkflow>>(
            &target,
            identity(272 + u64::from(after_dispatch), issued_at_ms),
            WorkflowActivityClaimRequest {
                limit: 1,
                lease_ms: 60_000,
            },
        )
        .await
        .expect("prepare exact Activity claim");
    let (retained_attempt, resolution) =
        cancel_prepared(prepared, &observer, entered, &dispatched, after_dispatch).await;
    let (claimed, commit_sequence) = match (after_dispatch, resolution) {
        (true, Resolution::Committed(outcome)) => {
            let (claimed, commit_sequence) = committed_output::<Vec<ActivityClaim>>(outcome);
            (claimed, commit_sequence)
        }
        (false, Resolution::Absent) => {
            let absent = workflow
                .state(workflow_id.clone(), None)
                .await
                .expect("independent Activity absence observation");
            let run = absent.output.expect("source Workflow remains present");
            assert_eq!(run.status, WorkflowStatus::Running);
            assert_eq!(run.event_sequence, 1);
            let retried = retained_attempt
                .execute()
                .await
                .expect("retry cancelled absent Activity claim");
            (retried.output, retried.receipt.commit_sequence)
        }
        (_, outcome) => panic!("unexpected cancelled Activity claim resolution: {outcome:?}"),
    };
    assert_eq!(dispatched.load(Ordering::Acquire), 1);
    assert_eq!(claimed.len(), 1);
    let claim = &claimed[0];
    assert_eq!(claim.run_id, run_id);
    assert_eq!(claim.activity_type, "reference-activity");
    assert_eq!(claim.input, b"activity-result");
    assert_eq!(claim.attempt, 1);
    let live = observer
        .query::<WorkflowActivityValidateQuery<fixture::ReferenceWorkflow>>(
            &target,
            None,
            WorkflowActivityValidateRequest {
                claimed: claimed.clone(),
            },
        )
        .await
        .expect("independent Activity claim validation");
    assert!(live.output);
    assert!(live.receipt.commit_sequence >= commit_sequence);
    let result = b"activity-result".to_vec();
    let completed = observer
        .command::<WorkflowActivityCompleteCommand<fixture::ReferenceWorkflow>>(
            &target,
            identity(274 + u64::from(after_dispatch), now_ms()),
            completion(
                claim,
                fixed_id(274 + u64::from(after_dispatch)),
                result.clone(),
            ),
        )
        .await
        .expect("settle claimed Activity");
    assert!(matches!(
        completed.output,
        ActivityCompletionOutcome::Applied(WorkflowOutcome::Applied {
            run_id: completed_run,
            status: WorkflowStatus::Completed,
            event_sequence: 2,
        }) if completed_run == run_id
    ));
    let observed = workflow
        .state(workflow_id, Some(completed.receipt))
        .await
        .expect("independent Activity completion observation");
    let run = observed.output.expect("completed Activity workflow");
    assert_eq!(run.run_id, run_id);
    assert_eq!(run.status, WorkflowStatus::Completed);
    assert_eq!(run.event_sequence, 2);
    assert_eq!(run.result, Some(completion_result(claim, &result)));
    let settled = observer
        .query::<WorkflowActivityValidateQuery<fixture::ReferenceWorkflow>>(
            &target,
            None,
            WorkflowActivityValidateRequest { claimed },
        )
        .await
        .expect("independent Activity settlement observation");
    assert!(!settled.output);

    drop(peer);
    drop(observer);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_activity_completion_cancellation_before_dispatch_resolves_absent() {
    run_completion_cancellation(public_host_fixture().await, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_activity_completion_cancellation_before_dispatch_resolves_absent() {
    let (store, root) = rustfs_public_store();
    run_completion_cancellation(public_host_fixture_with_store(store, root).await, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_activity_completion_cancellation_after_dispatch_resolves_committed() {
    run_completion_cancellation(public_host_fixture().await, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_activity_completion_cancellation_after_dispatch_resolves_committed() {
    let (store, root) = rustfs_public_store();
    run_completion_cancellation(public_host_fixture_with_store(store, root).await, true).await;
}

async fn run_completion_cancellation(
    (node, observer, tenant, application, _directory, registry, handles, _store): PublicHostFixture,
    after_dispatch: bool,
) {
    let (peer_client, entered, dispatched) =
        peer_client_with_paused_mutation(registry, handles, after_dispatch);
    let peer =
        node.application_handle::<fixture::ReferenceApplication>(peer_client, tenant, application);
    let target = CellTarget::new(
        tenant,
        application,
        fixture::WORKFLOW_NAMESPACE,
        &partition_for_shard(0),
    )
    .expect("Activity target");
    let workflow = observer
        .workflow::<fixture::ReferenceWorkflow>()
        .expect("observer Workflow");
    let issued_at_ms = now_ms();
    let workflow_id = if after_dispatch {
        b"cancelled-activity-completion-after".to_vec()
    } else {
        b"cancelled-activity-completion-before".to_vec()
    };
    let started = workflow
        .start(
            identity(280 + u64::from(after_dispatch), issued_at_ms),
            workflow_id.clone(),
            b"activity".to_vec(),
        )
        .await
        .expect("source Activity publication");
    let WorkflowOutcome::Applied {
        run_id,
        status: WorkflowStatus::Running,
        event_sequence: 1,
    } = started.output
    else {
        panic!("source Activity workflow did not start");
    };
    let claimed = observer
        .command::<WorkflowActivityClaimCommand<fixture::ReferenceWorkflow>>(
            &target,
            identity(282 + u64::from(after_dispatch), now_ms()),
            WorkflowActivityClaimRequest {
                limit: 1,
                lease_ms: 60_000,
            },
        )
        .await
        .expect("source Activity claim");
    assert_eq!(claimed.output.len(), 1);
    let claim = &claimed.output[0];
    assert_eq!(claim.run_id, run_id);
    assert_eq!(claim.attempt, 1);
    let result = b"activity-result".to_vec();
    let completion = completion(
        claim,
        fixed_id(284 + u64::from(after_dispatch)),
        result.clone(),
    );
    let prepared = peer
        .prepare_command::<WorkflowActivityCompleteCommand<fixture::ReferenceWorkflow>>(
            &target,
            identity(284 + u64::from(after_dispatch), now_ms()),
            completion.clone(),
        )
        .await
        .expect("prepare exact Activity completion");
    let (retained_attempt, resolution) =
        cancel_prepared(prepared, &observer, entered, &dispatched, after_dispatch).await;
    let commit_sequence = match (after_dispatch, resolution) {
        (true, Resolution::Committed(outcome)) => {
            let (output, commit_sequence) = committed_output::<ActivityCompletionOutcome>(outcome);
            assert!(matches!(
                output,
                ActivityCompletionOutcome::Applied(WorkflowOutcome::Applied {
                    run_id: completed_run,
                    status: WorkflowStatus::Completed,
                    event_sequence: 2,
                }) if completed_run == run_id
            ));
            commit_sequence
        }
        (false, Resolution::Absent) => {
            let absent = workflow
                .state(workflow_id.clone(), None)
                .await
                .expect("independent Activity completion absence observation");
            let run = absent.output.expect("source Workflow remains present");
            assert_eq!(run.status, WorkflowStatus::Running);
            assert_eq!(run.event_sequence, 1);
            let live = observer
                .query::<WorkflowActivityValidateQuery<fixture::ReferenceWorkflow>>(
                    &target,
                    None,
                    WorkflowActivityValidateRequest {
                        claimed: claimed.output.clone(),
                    },
                )
                .await
                .expect("independent live Activity lease observation");
            assert!(live.output);
            let retried = retained_attempt
                .execute()
                .await
                .expect("retry cancelled absent Activity completion");
            assert!(matches!(
                retried.output,
                ActivityCompletionOutcome::Applied(WorkflowOutcome::Applied {
                    run_id: completed_run,
                    status: WorkflowStatus::Completed,
                    event_sequence: 2,
                }) if completed_run == run_id
            ));
            retried.receipt.commit_sequence
        }
        (_, outcome) => {
            panic!("unexpected cancelled Activity completion resolution: {outcome:?}")
        }
    };
    assert_eq!(dispatched.load(Ordering::Acquire), 1);
    let observed = workflow
        .state(workflow_id.clone(), None)
        .await
        .expect("independent Activity completion observation");
    let run = observed.output.expect("completed Activity workflow");
    assert_eq!(run.run_id, run_id);
    assert_eq!(run.status, WorkflowStatus::Completed);
    assert_eq!(run.event_sequence, 2);
    assert_eq!(run.result, Some(completion_result(claim, &result)));
    assert!(observed.receipt.commit_sequence >= commit_sequence);
    let duplicate = observer
        .command::<WorkflowActivityCompleteCommand<fixture::ReferenceWorkflow>>(
            &target,
            identity(286 + u64::from(after_dispatch), now_ms()),
            completion,
        )
        .await
        .expect("exact Activity completion duplicate");
    assert_eq!(
        duplicate.output,
        ActivityCompletionOutcome::Duplicate {
            result: result.clone(),
        }
    );
    let settled = observer
        .query::<WorkflowActivityValidateQuery<fixture::ReferenceWorkflow>>(
            &target,
            None,
            WorkflowActivityValidateRequest {
                claimed: claimed.output,
            },
        )
        .await
        .expect("independent Activity settlement observation");
    assert!(!settled.output);
    let final_run = workflow
        .state(workflow_id, Some(duplicate.receipt))
        .await
        .expect("independent Activity duplicate observation");
    assert_eq!(
        final_run.output.expect("retained Workflow").event_sequence,
        2
    );

    drop(peer);
    drop(observer);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

fn completion(
    claim: &ActivityClaim,
    completion_token: [u8; 16],
    result: Vec<u8>,
) -> ActivityCompletion {
    ActivityCompletion {
        run_id: claim.run_id,
        activity_id: claim.activity_id,
        attempt: claim.attempt,
        lease_token: claim.token,
        completion_token,
        result,
        failed: false,
        retryable: false,
    }
}

fn completion_result(claim: &ActivityClaim, result: &[u8]) -> Vec<u8> {
    let mut expected = b"activity\0\0".to_vec();
    expected.extend_from_slice(&claim.activity_id);
    expected.extend_from_slice(&(result.len() as u32).to_be_bytes());
    expected.extend_from_slice(result);
    expected
}
