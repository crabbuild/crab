use std::sync::atomic::Ordering;

use crab_cell_runtime::cell::executor::Resolution;
use crab_cell_runtime::identity::{CellTarget, partition_for_shard};
use crab_cell_runtime::primitives::blob::{
    BlobCommand, BlobCondition, BlobMutation, BlobMutationOutcome, BlobQuery, BlobQueryResult,
};
use crab_cell_runtime::primitives::effects::{
    EffectAckRequest, EffectClaimRequest, EffectLease, EffectLeaseCommand, EffectLeaseOutcome,
    EffectLeaseRequest,
};
use crab_cell_runtime::primitives::queue::{
    QueueClaimRequest, QueueLeaseOutcome, QueueSendCommand, QueueSendOutcome, QueueSendRequest,
    QueueState,
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
async fn public_queue_cancellation_before_dispatch_resolves_absent() {
    run_queue_cancellation(public_host_fixture().await, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_queue_cancellation_before_dispatch_resolves_absent() {
    let (store, root) = rustfs_public_store();
    run_queue_cancellation(public_host_fixture_with_store(store, root).await, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_queue_cancellation_after_dispatch_resolves_committed() {
    run_queue_cancellation(public_host_fixture().await, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_queue_cancellation_after_dispatch_resolves_committed() {
    let (store, root) = rustfs_public_store();
    run_queue_cancellation(public_host_fixture_with_store(store, root).await, true).await;
}

async fn run_queue_cancellation(
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
        fixture::QUEUE_NAMESPACE,
        &partition_for_shard(0),
    )
    .expect("Queue target");
    let observer_queue = observer
        .queue::<fixture::ReferenceQueue>()
        .expect("observer Queue");
    let marker = if after_dispatch {
        b"cancelled-queue-after".to_vec()
    } else {
        b"cancelled-queue-before".to_vec()
    };
    let issued_at_ms = now_ms();
    let prepared = peer
        .prepare_command::<QueueSendCommand<fixture::ReferenceQueue>>(
            &target,
            identity(246 + u64::from(after_dispatch), issued_at_ms),
            QueueSendRequest {
                producer_id: fixed_id(246 + u64::from(after_dispatch)),
                payload: marker.clone(),
                available_at_ms: issued_at_ms,
            },
        )
        .await
        .expect("prepare exact Queue send");
    let (retained_attempt, resolution) =
        cancel_prepared(prepared, &observer, entered, &dispatched, after_dispatch).await;
    let (commit_sequence, expected_message_id) = match (after_dispatch, resolution) {
        (true, Resolution::Committed(outcome)) => {
            let (output, commit_sequence) = committed_output::<QueueSendOutcome>(outcome);
            let QueueSendOutcome::Sent { message_id } = output else {
                panic!("cancelled Queue ledger result did not send");
            };
            (commit_sequence, message_id)
        }
        (false, Resolution::Absent) => {
            let absent = observer_queue
                .info(0, None)
                .await
                .expect("independent Queue absence observation");
            assert_eq!(absent.output.ready, 0);
            assert_eq!(absent.output.leased, 0);
            let retried = retained_attempt
                .execute()
                .await
                .expect("retry cancelled absent Queue send");
            let QueueSendOutcome::Sent { message_id } = retried.output else {
                panic!("cancelled Queue retry did not send");
            };
            (retried.receipt.commit_sequence, message_id)
        }
        (_, outcome) => panic!("unexpected cancelled Queue resolution: {outcome:?}"),
    };
    assert_eq!(dispatched.load(Ordering::Acquire), 1);
    let claimed = observer_queue
        .claim(
            identity(248, now_ms()),
            0,
            QueueClaimRequest {
                limit: 2,
                lease_ms: 60_000,
            },
        )
        .await
        .expect("independent Queue claim");
    assert_eq!(claimed.output.len(), 1);
    let message = &claimed.output[0];
    assert_eq!(message.payload, marker);
    assert_eq!(message.attempt, 1);
    assert_eq!(message.message_id, expected_message_id);
    assert!(claimed.receipt.commit_sequence > commit_sequence);
    let acked = observer_queue
        .ack(
            identity(249, now_ms()),
            0,
            message.message_id,
            message.token,
        )
        .await
        .expect("Queue settlement");
    assert!(matches!(
        acked.output,
        QueueLeaseOutcome::Applied {
            state: QueueState::Acked,
            ..
        }
    ));
    let info = observer_queue
        .info(0, Some(acked.receipt))
        .await
        .expect("independent Queue settlement observation");
    assert_eq!(info.output.acked, 1);
    assert_eq!(info.output.ready, 0);
    assert_eq!(info.output.leased, 0);

    drop(peer);
    drop(observer);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_blob_cancellation_before_dispatch_resolves_absent() {
    run_blob_cancellation(public_host_fixture().await, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_blob_cancellation_before_dispatch_resolves_absent() {
    let (store, root) = rustfs_public_store();
    run_blob_cancellation(public_host_fixture_with_store(store, root).await, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_blob_cancellation_after_dispatch_resolves_committed() {
    run_blob_cancellation(public_host_fixture().await, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_blob_cancellation_after_dispatch_resolves_committed() {
    let (store, root) = rustfs_public_store();
    run_blob_cancellation(public_host_fixture_with_store(store, root).await, true).await;
}

async fn run_blob_cancellation(
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
        fixture::BLOB_NAMESPACE,
        &partition_for_shard(0),
    )
    .expect("Blob target");
    let observer_blob = observer
        .blob::<fixture::ReferenceBlob>()
        .expect("observer Blob");
    let issued_at_ms = now_ms();
    let key = if after_dispatch {
        b"cancelled-blob-after".to_vec()
    } else {
        b"cancelled-blob-before".to_vec()
    };
    let upload_id = fixed_id(250 + u64::from(after_dispatch));
    let payload = if after_dispatch {
        b"blob-bytes-after-cancellation".to_vec()
    } else {
        b"blob-bytes-before-cancellation".to_vec()
    };
    observer_blob
        .mutate(
            identity(250, issued_at_ms),
            BlobMutation::Begin {
                key: key.clone(),
                upload_id,
                condition: BlobCondition::Missing,
                content_type: None,
                metadata: Vec::new(),
                expires_at_ms: issued_at_ms + 120_000,
            },
        )
        .await
        .expect("Blob upload begin");
    observer_blob
        .mutate(
            identity(251, issued_at_ms),
            BlobMutation::PutPart {
                key: key.clone(),
                upload_id,
                part_number: 1,
                payload: payload.clone(),
            },
        )
        .await
        .expect("Blob upload part");
    let prepared = peer
        .prepare_command::<BlobCommand<fixture::ReferenceBlob>>(
            &target,
            identity(252 + u64::from(after_dispatch), issued_at_ms),
            BlobMutation::Complete {
                key: key.clone(),
                upload_id,
                part_count: 1,
            },
        )
        .await
        .expect("prepare exact Blob completion");
    let (retained_attempt, resolution) =
        cancel_prepared(prepared, &observer, entered, &dispatched, after_dispatch).await;
    let (commit_sequence, expected_etag) = match (after_dispatch, resolution) {
        (true, Resolution::Committed(outcome)) => {
            let (output, commit_sequence) = committed_output::<BlobMutationOutcome>(outcome);
            let BlobMutationOutcome::Committed { etag, size } = output else {
                panic!("cancelled Blob ledger result did not commit");
            };
            assert_eq!(size, payload.len() as u64);
            (commit_sequence, etag)
        }
        (false, Resolution::Absent) => {
            let absent = observer_blob
                .query(
                    BlobQuery::Read {
                        key: key.clone(),
                        offset: 0,
                        limit: 128,
                    },
                    None,
                )
                .await
                .expect("independent Blob absence observation");
            assert!(matches!(absent.output, BlobQueryResult::Read(None)));
            let retried = retained_attempt
                .execute()
                .await
                .expect("retry cancelled absent Blob completion");
            let BlobMutationOutcome::Committed { etag, size } = retried.output else {
                panic!("cancelled Blob retry did not commit");
            };
            assert_eq!(size, payload.len() as u64);
            (retried.receipt.commit_sequence, etag)
        }
        (_, outcome) => panic!("unexpected cancelled Blob resolution: {outcome:?}"),
    };
    assert_eq!(dispatched.load(Ordering::Acquire), 1);
    let observed = observer_blob
        .query(
            BlobQuery::Read {
                key,
                offset: 0,
                limit: 128,
            },
            None,
        )
        .await
        .expect("independent Blob state observation");
    let BlobQueryResult::Read(Some(read)) = observed.output else {
        panic!("cancelled Blob is absent");
    };
    assert_eq!(read.bytes, payload);
    assert_eq!(read.metadata.size, payload.len() as u64);
    assert_eq!(read.metadata.etag, expected_etag);
    assert!(observed.receipt.commit_sequence >= commit_sequence);

    drop(peer);
    drop(observer);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_effect_ack_cancellation_before_dispatch_resolves_absent() {
    run_effect_ack_cancellation(public_host_fixture().await, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_effect_ack_cancellation_before_dispatch_resolves_absent() {
    let (store, root) = rustfs_public_store();
    run_effect_ack_cancellation(public_host_fixture_with_store(store, root).await, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_effect_ack_cancellation_after_dispatch_resolves_committed() {
    run_effect_ack_cancellation(public_host_fixture().await, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_effect_ack_cancellation_after_dispatch_resolves_committed() {
    let (store, root) = rustfs_public_store();
    run_effect_ack_cancellation(public_host_fixture_with_store(store, root).await, true).await;
}

async fn run_effect_ack_cancellation(
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
    .expect("Effect source target");
    let observer_effects = observer
        .effects::<fixture::ReferenceWorkflow>(target.clone())
        .expect("observer Effects");
    let workflow = observer
        .workflow::<fixture::ReferenceWorkflow>()
        .expect("source Workflow");
    let issued_at_ms = now_ms();
    let workflow_id = if after_dispatch {
        b"cancelled-effect-after".to_vec()
    } else {
        b"cancelled-effect-before".to_vec()
    };
    workflow
        .start(
            identity(254 + u64::from(after_dispatch), issued_at_ms),
            workflow_id,
            b"effect".to_vec(),
        )
        .await
        .expect("source Effect publication");
    let claimed = observer_effects
        .claim(
            identity(256 + u64::from(after_dispatch), issued_at_ms),
            EffectClaimRequest {
                limit: 1,
                lease_ms: 60_000,
            },
        )
        .await
        .expect("source Effect claim");
    assert_eq!(claimed.output.len(), 1);
    let claim = claimed.output[0].clone();
    let prepared = peer
        .prepare_command::<EffectLeaseCommand<fixture::ReferenceWorkflow>>(
            &target,
            identity(258 + u64::from(after_dispatch), issued_at_ms),
            EffectLeaseRequest::Ack(EffectAckRequest {
                lease: EffectLease::from(&claim),
                result: b"cancelled-effect-delivery".to_vec(),
            }),
        )
        .await
        .expect("prepare exact Effect acknowledgement");
    let (retained_attempt, resolution) =
        cancel_prepared(prepared, &observer, entered, &dispatched, after_dispatch).await;
    let commit_sequence = match (after_dispatch, resolution) {
        (true, Resolution::Committed(outcome)) => {
            let (output, commit_sequence) = committed_output::<EffectLeaseOutcome>(outcome);
            assert_eq!(output, EffectLeaseOutcome::Delivered);
            commit_sequence
        }
        (false, Resolution::Absent) => {
            let live = observer_effects
                .validate(vec![claim.clone()], claimed.receipt)
                .await
                .expect("independent live Effect lease observation");
            assert!(live.output);
            let retried = retained_attempt
                .execute()
                .await
                .expect("retry cancelled absent Effect acknowledgement");
            assert_eq!(retried.output, EffectLeaseOutcome::Delivered);
            retried.receipt.commit_sequence
        }
        (_, outcome) => panic!("unexpected cancelled Effect resolution: {outcome:?}"),
    };
    assert_eq!(dispatched.load(Ordering::Acquire), 1);
    let settled = observer_effects
        .validate(vec![claim], claimed.receipt)
        .await
        .expect("independent Effect settlement observation");
    assert!(!settled.output);
    assert!(settled.receipt.commit_sequence >= commit_sequence);
    let remaining = observer_effects
        .claim(
            identity(260 + u64::from(after_dispatch), now_ms()),
            EffectClaimRequest {
                limit: 1,
                lease_ms: 5_000,
            },
        )
        .await
        .expect("Effect queue observation");
    assert!(remaining.output.is_empty());

    drop(peer);
    drop(observer);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}
