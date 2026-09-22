use std::time::Duration;

use crab_cell_runtime::{
    ActivityCompletion, ActivityCompletionOutcome, BlobArtifactStore, BlobCondition, BlobMutation,
    BlobMutationOutcome, BlobQuery, BlobQueryResult, CellClient, CellTarget, EffectClaimRequest,
    EffectLeaseOutcome, InvocationError, QueueClaimRequest, QueueLeaseOutcome, QueueSendOutcome,
    QueueSendRequest, QueueState, WorkflowActivityClaimCommand, WorkflowActivityClaimRequest,
    WorkflowActivityCompleteCommand, WorkflowActivityValidateQuery,
    WorkflowActivityValidateRequest, WorkflowOutcome, WorkflowStatus, partition_for_shard,
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
    let remaining_ms = deadline_ms
        .saturating_sub(now_ms())
        .max(0)
        .saturating_add(100);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_activity_expired_lease_reclaims_one_attempt() {
    run_activity_expiry(public_host_fixture().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_activity_expired_lease_reclaims_one_attempt() {
    let (store, root) = rustfs_public_store();
    run_activity_expiry(public_host_fixture_with_store(store, root).await).await;
}

async fn run_activity_expiry(
    (node, writer, tenant, application, _directory, registry, handles, _store): PublicHostFixture,
) {
    let claimant = CellClient::local_many(registry.clone(), handles.clone())
        .expect("independent Activity claimant");
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
    .expect("Activity source target");
    let workflow = writer
        .workflow::<fixture::ReferenceWorkflow>()
        .expect("writer Workflow");
    let workflow_id = b"expired-activity-lease".to_vec();
    let started = workflow
        .start(
            identity(340, now_ms()),
            workflow_id.clone(),
            b"activity".to_vec(),
        )
        .await
        .expect("acknowledged Activity publication");
    let WorkflowOutcome::Applied {
        run_id,
        status: WorkflowStatus::Running,
        event_sequence: 1,
        ..
    } = started.output
    else {
        panic!("Activity workflow did not start running");
    };
    let first = claimant
        .command::<WorkflowActivityClaimCommand<fixture::ReferenceWorkflow>>(
            &target,
            identity(341, now_ms()),
            WorkflowActivityClaimRequest {
                limit: 1,
                lease_ms: 10_000,
            },
        )
        .await
        .expect("first Activity claim");
    assert_eq!(first.output.len(), 1);
    let first_receipt = first.receipt;
    let first = first.output.into_iter().next().expect("claimed Activity");
    assert_eq!(first.run_id, run_id);
    assert_eq!(first.input, b"activity-result");
    assert_eq!(first.attempt, 1);
    let live = claimant
        .query::<WorkflowActivityValidateQuery<fixture::ReferenceWorkflow>>(
            &target,
            Some(first_receipt),
            WorkflowActivityValidateRequest {
                claimed: vec![first.clone()],
            },
        )
        .await
        .expect("live Activity lease observation");
    assert!(live.output);
    wait_until_expired(first.lease_until_ms).await;
    let old_completion = ActivityCompletion {
        run_id: first.run_id,
        activity_id: first.activity_id,
        attempt: first.attempt,
        lease_token: first.token,
        completion_token: fixed_id(350),
        result: first.input.clone(),
        failed: false,
        retryable: false,
    };
    let stale = claimant
        .command::<WorkflowActivityCompleteCommand<fixture::ReferenceWorkflow>>(
            &target,
            identity(342, now_ms()),
            old_completion,
        )
        .await;
    assert!(matches!(
        stale,
        Err(InvocationError::Rejected(outcome))
            if outcome.output == ActivityCompletionOutcome::LeaseLost
    ));
    let expired = claimant
        .query::<WorkflowActivityValidateQuery<fixture::ReferenceWorkflow>>(
            &target,
            Some(first_receipt),
            WorkflowActivityValidateRequest {
                claimed: vec![first.clone()],
            },
        )
        .await
        .expect("expired Activity lease observation");
    assert!(!expired.output);
    let second = claimant
        .command::<WorkflowActivityClaimCommand<fixture::ReferenceWorkflow>>(
            &target,
            identity(343, now_ms()),
            WorkflowActivityClaimRequest {
                limit: 1,
                lease_ms: 60_000,
            },
        )
        .await
        .expect("Activity reclaim after lease expiry");
    assert_eq!(second.output.len(), 1);
    let second = second
        .output
        .into_iter()
        .next()
        .expect("reclaimed Activity");
    assert_eq!(second.run_id, run_id);
    assert_eq!(second.activity_id, first.activity_id);
    assert_eq!(second.input, first.input);
    assert_eq!(second.attempt, 2);
    assert_ne!(second.token, first.token);
    let completion = ActivityCompletion {
        run_id: second.run_id,
        activity_id: second.activity_id,
        attempt: second.attempt,
        lease_token: second.token,
        completion_token: fixed_id(351),
        result: second.input.clone(),
        failed: false,
        retryable: false,
    };
    let completed = claimant
        .command::<WorkflowActivityCompleteCommand<fixture::ReferenceWorkflow>>(
            &target,
            identity(344, now_ms()),
            completion.clone(),
        )
        .await
        .expect("reclaimed Activity completion");
    assert!(matches!(
        completed.output,
        ActivityCompletionOutcome::Applied(WorkflowOutcome::Applied {
            status: WorkflowStatus::Completed,
            event_sequence: 2,
            ..
        })
    ));
    let duplicate = claimant
        .command::<WorkflowActivityCompleteCommand<fixture::ReferenceWorkflow>>(
            &target,
            identity(345, now_ms()),
            completion,
        )
        .await
        .expect("duplicate Activity completion");
    assert_eq!(
        duplicate.output,
        ActivityCompletionOutcome::Duplicate {
            result: second.input.clone()
        }
    );
    let observed = observer
        .workflow::<fixture::ReferenceWorkflow>()
        .expect("observer Workflow")
        .state(workflow_id, Some(duplicate.receipt))
        .await
        .expect("independent Activity result observation");
    let run = observed.output.expect("retained Activity workflow");
    let mut expected = b"activity\0".to_vec();
    expected.push(0);
    expected.extend_from_slice(&second.activity_id);
    expected.extend_from_slice(&(second.input.len() as u32).to_be_bytes());
    expected.extend_from_slice(&second.input);
    assert_eq!(run.run_id, run_id);
    assert_eq!(run.status, WorkflowStatus::Completed);
    assert_eq!(run.event_sequence, 2);
    assert_eq!(run.result, Some(expected));

    drop(writer);
    drop(observer);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_blob_expired_upload_does_not_publish() {
    run_blob_expiry(public_host_fixture().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_blob_expired_upload_does_not_publish() {
    let (store, root) = rustfs_public_store();
    run_blob_expiry(public_host_fixture_with_store(store, root).await).await;
}

async fn run_blob_expiry(
    (node, writer, tenant, application, _directory, registry, handles, store): PublicHostFixture,
) {
    let observer = node
        .application_handle::<fixture::ReferenceApplication>(
            CellClient::local_many(registry, handles).expect("separate typed observer"),
            tenant,
            application,
        )
        .with_blob_artifact_store(BlobArtifactStore::new(store));
    let blob = writer
        .blob::<fixture::ReferenceBlob>()
        .expect("writer Blob");
    let observed_blob = observer
        .blob::<fixture::ReferenceBlob>()
        .expect("observer Blob");
    let issued_at_ms = now_ms() - 40_000;
    let expires_at_ms = issued_at_ms + 60_000;
    let key = b"expired-upload-key".to_vec();
    let old_upload_id = fixed_id(360);
    let begun = blob
        .mutate(
            identity(360, issued_at_ms),
            BlobMutation::Begin {
                key: key.clone(),
                upload_id: old_upload_id,
                condition: BlobCondition::Missing,
                content_type: None,
                metadata: Vec::new(),
                expires_at_ms,
            },
        )
        .await
        .expect("acknowledged expiring Blob upload");
    assert_eq!(begun.output, BlobMutationOutcome::Begun);
    let old_part = blob
        .mutate(
            identity(361, now_ms()),
            BlobMutation::PutPart {
                key: key.clone(),
                upload_id: old_upload_id,
                part_number: 1,
                payload: b"unpublished-expired-part".to_vec(),
            },
        )
        .await
        .expect("acknowledged Blob part before expiry");
    assert!(matches!(
        old_part.output,
        BlobMutationOutcome::PartStored { .. }
    ));
    let invisible = observed_blob
        .query(BlobQuery::Head { key: key.clone() }, Some(old_part.receipt))
        .await
        .expect("independent incomplete Blob observation");
    assert_eq!(invisible.output, BlobQueryResult::Head(None));
    wait_until_expired(expires_at_ms).await;
    let expired = blob
        .mutate(
            identity(362, now_ms()),
            BlobMutation::Complete {
                key: key.clone(),
                upload_id: old_upload_id,
                part_count: 1,
            },
        )
        .await;
    assert!(matches!(
        expired,
        Err(InvocationError::Rejected(outcome))
            if matches!(outcome.output, BlobMutationOutcome::Conflict | BlobMutationOutcome::NotFound)
    ));
    let still_invisible = observed_blob
        .query(BlobQuery::Head { key: key.clone() }, None)
        .await
        .expect("independent expired Blob observation");
    assert_eq!(still_invisible.output, BlobQueryResult::Head(None));
    let new_upload_id = fixed_id(363);
    let begun = blob
        .mutate(
            identity(363, now_ms()),
            BlobMutation::Begin {
                key: key.clone(),
                upload_id: new_upload_id,
                condition: BlobCondition::Missing,
                content_type: None,
                metadata: Vec::new(),
                expires_at_ms: now_ms() + 60_000,
            },
        )
        .await
        .expect("fresh Blob upload after expiry");
    assert_eq!(begun.output, BlobMutationOutcome::Begun);
    let payload = b"published-after-expiry".to_vec();
    let part = blob
        .mutate(
            identity(364, now_ms()),
            BlobMutation::PutPart {
                key: key.clone(),
                upload_id: new_upload_id,
                part_number: 1,
                payload: payload.clone(),
            },
        )
        .await
        .expect("fresh Blob part");
    assert!(matches!(
        part.output,
        BlobMutationOutcome::PartStored { .. }
    ));
    let committed = blob
        .mutate(
            identity(365, now_ms()),
            BlobMutation::Complete {
                key: key.clone(),
                upload_id: new_upload_id,
                part_count: 1,
            },
        )
        .await
        .expect("fresh Blob completion");
    let BlobMutationOutcome::Committed { etag, size } = committed.output else {
        panic!("fresh Blob was not published");
    };
    let observed = observed_blob
        .query(
            BlobQuery::Read {
                key,
                offset: 0,
                limit: 128,
            },
            Some(committed.receipt),
        )
        .await
        .expect("independent published Blob observation");
    let BlobQueryResult::Read(Some(read)) = observed.output else {
        panic!("fresh Blob is absent");
    };
    assert_eq!(read.bytes, payload);
    assert_eq!(read.metadata.etag, etag);
    assert_eq!(read.metadata.size, size);

    drop(writer);
    drop(observer);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}
