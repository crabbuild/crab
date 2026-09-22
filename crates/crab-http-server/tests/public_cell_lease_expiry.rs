use std::time::Duration;

use crab_cell_runtime::{
    CellClient, CellTarget, EffectClaimRequest, EffectLeaseOutcome, InvocationError,
    QueueClaimRequest, QueueLeaseOutcome, QueueSendOutcome, QueueSendRequest, QueueState,
    WorkflowOutcome, WorkflowStatus, partition_for_shard,
};

#[path = "support/reference_application.rs"]
mod fixture;
#[path = "support/qualification.rs"]
mod qualification;
#[path = "support/qualification_fixture.rs"]
mod qualification_fixture;
#[path = "support/qualification_local_fixture.rs"]
mod qualification_local_fixture;

use qualification::{assert_zero_reservations, fixed_id, identity, rustfs_public_store};
use qualification_fixture::{PublicHostFixture, public_host_fixture_with_store};
use qualification_local_fixture::public_host_fixture;

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("qualification clock")
            .as_millis(),
    )
    .expect("qualification clock bounds")
}

async fn wait_until_expired(deadline_ms: i64) {
    let remaining_ms = deadline_ms.saturating_sub(now_ms()).saturating_add(100);
    let remaining_ms = u64::try_from(remaining_ms).expect("nonnegative expiry wait");
    tokio::time::sleep(Duration::from_millis(remaining_ms)).await;
    assert!(now_ms() > deadline_ms);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_queue_expired_lease_reclaims_one_message() {
    run_queue_expiry(public_host_fixture().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_queue_expired_lease_reclaims_one_message() {
    let (store, root) = rustfs_public_store();
    run_queue_expiry(public_host_fixture_with_store(store, root).await).await;
}

async fn run_queue_expiry(
    (node, writer, tenant, application, _directory, registry, handles, _store): PublicHostFixture,
) {
    let observer = node.application_handle::<fixture::ReferenceApplication>(
        CellClient::local_many(registry, handles).expect("separate typed observer"),
        tenant,
        application,
    );
    let queue = writer
        .queue::<fixture::ReferenceQueue>()
        .expect("writer Queue");
    let observed_queue = observer
        .queue::<fixture::ReferenceQueue>()
        .expect("observer Queue");
    let issued_at_ms = now_ms();
    let payload = b"expired-queue-lease".to_vec();
    let sent = queue
        .send(
            identity(310, issued_at_ms),
            QueueSendRequest {
                producer_id: fixed_id(310),
                payload: payload.clone(),
                available_at_ms: issued_at_ms,
            },
        )
        .await
        .expect("acknowledged Queue send");
    let QueueSendOutcome::Sent { message_id } = sent.output else {
        panic!("Queue send was not applied");
    };
    let first = queue
        .claim(
            identity(311, now_ms()),
            0,
            QueueClaimRequest {
                limit: 1,
                lease_ms: 10_000,
            },
        )
        .await
        .expect("first Queue claim");
    assert_eq!(first.output.len(), 1);
    let first = &first.output[0];
    assert_eq!(first.message_id, message_id);
    assert_eq!(first.payload, payload);
    assert_eq!(first.attempt, 1);
    let observed = observed_queue
        .info(0, None)
        .await
        .expect("independent leased Queue observation");
    assert_eq!(observed.output.leased, 1);
    wait_until_expired(first.lease_until_ms).await;
    let old_ack = queue
        .ack(identity(312, now_ms()), 0, first.message_id, first.token)
        .await;
    assert!(matches!(
        old_ack,
        Err(InvocationError::Rejected(outcome))
            if outcome.output == QueueLeaseOutcome::LeaseLost
    ));
    let second = observed_queue
        .claim(
            identity(313, now_ms()),
            0,
            QueueClaimRequest {
                limit: 1,
                lease_ms: 60_000,
            },
        )
        .await
        .expect("Queue reclaim after lease expiry");
    assert_eq!(second.output.len(), 1);
    let second = &second.output[0];
    assert_eq!(second.message_id, message_id);
    assert_eq!(second.payload, payload);
    assert_eq!(second.attempt, 2);
    assert_ne!(second.token, first.token);
    let acked = observed_queue
        .ack(identity(314, now_ms()), 0, second.message_id, second.token)
        .await
        .expect("reclaimed Queue acknowledgement");
    assert!(matches!(
        acked.output,
        QueueLeaseOutcome::Applied {
            state: QueueState::Acked,
            ..
        }
    ));
    let final_info = queue
        .info(0, Some(acked.receipt))
        .await
        .expect("independent final Queue observation");
    assert_eq!(final_info.output.acked, 1);
    assert_eq!(final_info.output.ready, 0);
    assert_eq!(final_info.output.leased, 0);

    drop(writer);
    drop(observer);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_effect_expired_lease_reclaims_one_delivery() {
    run_effect_expiry(public_host_fixture().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_effect_expired_lease_reclaims_one_delivery() {
    let (store, root) = rustfs_public_store();
    run_effect_expiry(public_host_fixture_with_store(store, root).await).await;
}

async fn run_effect_expiry(
    (node, writer, tenant, application, _directory, registry, handles, _store): PublicHostFixture,
) {
    let observer = node.application_handle::<fixture::ReferenceApplication>(
        CellClient::local_many(registry, handles).expect("separate typed observer"),
        tenant,
        application,
    );
    let target = CellTarget::new(
        tenant,
        application,
        fixture::WORKFLOW_NAMESPACE,
        &partition_for_shard(0),
    )
    .expect("Effect source target");
    let workflow = writer
        .workflow::<fixture::ReferenceWorkflow>()
        .expect("writer Workflow");
    let source = writer
        .effects::<fixture::ReferenceWorkflow>(target.clone())
        .expect("writer Effect source");
    let observed_source = observer
        .effects::<fixture::ReferenceWorkflow>(target)
        .expect("observer Effect source");
    let workflow_id = b"expired-effect-lease".to_vec();
    let started = workflow
        .start(
            identity(320, now_ms()),
            workflow_id.clone(),
            b"effect".to_vec(),
        )
        .await
        .expect("acknowledged source Effect publication");
    assert!(matches!(
        started.output,
        WorkflowOutcome::Applied {
            status: WorkflowStatus::Completed,
            event_sequence: 1,
            ..
        }
    ));
    let first = source
        .claim(
            identity(321, now_ms()),
            EffectClaimRequest {
                limit: 1,
                lease_ms: 10_000,
            },
        )
        .await
        .expect("first Effect claim");
    assert_eq!(first.output.len(), 1);
    let first_receipt = first.receipt;
    let first = &first.output[0];
    assert_eq!(first.attempt, 1);
    let observed = observed_source
        .validate(vec![first.clone()], first_receipt)
        .await
        .expect("independent live Effect lease observation");
    assert!(observed.output);
    wait_until_expired(first.lease_until_ms).await;
    let old_ack = source
        .ack(
            identity(322, now_ms()),
            first.clone(),
            b"stale-effect-result".to_vec(),
        )
        .await;
    assert!(matches!(
        old_ack,
        Err(InvocationError::Rejected(outcome))
            if outcome.output == EffectLeaseOutcome::LeaseLost
    ));
    let stale = observed_source
        .validate(vec![first.clone()], first_receipt)
        .await
        .expect("independent expired Effect lease observation");
    assert!(!stale.output);
    let mut second = None;
    for attempt in 0..4 {
        let claimed = observed_source
            .claim(
                identity(330 + attempt, now_ms()),
                EffectClaimRequest {
                    limit: 1,
                    lease_ms: 60_000,
                },
            )
            .await
            .expect("Effect reclaim after lease expiry");
        if !claimed.output.is_empty() {
            second = Some(claimed);
            break;
        }
        // Expired Effects become ready after the bounded retry delay.
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let second = second.expect("Effect became claimable after retry delay");
    assert_eq!(second.output.len(), 1);
    let second = &second.output[0];
    assert_eq!(second.effect_id, first.effect_id);
    assert_eq!(second.attempt, 2);
    assert_ne!(second.token, first.token);
    let acked = observed_source
        .ack(
            identity(324, now_ms()),
            second.clone(),
            b"reclaimed-effect-result".to_vec(),
        )
        .await
        .expect("reclaimed Effect acknowledgement");
    assert_eq!(acked.output, EffectLeaseOutcome::Delivered);
    let settled = source
        .validate(vec![second.clone()], acked.receipt)
        .await
        .expect("independent settled Effect observation");
    assert!(!settled.output);
    let remaining = source
        .claim(
            identity(325, now_ms()),
            EffectClaimRequest {
                limit: 1,
                lease_ms: 5_000,
            },
        )
        .await
        .expect("Effect queue observation");
    assert!(remaining.output.is_empty());
    let workflow_state = observer
        .workflow::<fixture::ReferenceWorkflow>()
        .expect("observer Workflow")
        .state(workflow_id, None)
        .await
        .expect("independent source Workflow observation");
    let run = workflow_state.output.expect("retained Workflow");
    assert_eq!(run.status, WorkflowStatus::Completed);
    assert_eq!(run.event_sequence, 1);
    assert_eq!(run.result, Some(b"effect-scheduled".to_vec()));

    drop(writer);
    drop(observer);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}
