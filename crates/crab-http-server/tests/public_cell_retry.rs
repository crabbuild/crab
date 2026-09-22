use std::{
    sync::atomic::Ordering,
    time::{SystemTime, UNIX_EPOCH},
};

use crab_cell_runtime::{
    BlobArtifactStore, BlobCondition, BlobMutation, BlobMutationOutcome, BlobQuery,
    BlobQueryResult, CellTarget, CronMutation, CronMutationOutcome, CronQueryResult,
    EffectClaimRequest, EffectLeaseOutcome, InvocationError, KvAtomicOutcome, KvAtomicRequest,
    KvMutation, QueueClaimRequest, QueueLeaseOutcome, QueueSendOutcome, QueueSendRequest,
    QueueState, Resolution, SqlBatch, SqlStatement, SqlValue, WorkflowOutcome, WorkflowStatus,
    partition_for_shard,
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

use qualification::{assert_zero_reservations, fixed_id, identity, rustfs_public_store};
use qualification_fixture::{PublicHostFixture, public_host_fixture_with_store};
use qualification_local_fixture::public_host_fixture;
use qualification_peer::peer_client_with_one_lost_mutation;

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("qualification clock")
            .as_millis(),
    )
    .expect("qualification clock bounds")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_sql_absent_request_retries_same_identity_once() {
    run_sql_absent_request_retry(public_host_fixture().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_sql_absent_request_retries_same_identity_once() {
    let (store, root) = rustfs_public_store();
    run_sql_absent_request_retry(public_host_fixture_with_store(store, root).await).await;
}

async fn run_sql_absent_request_retry(
    (node, observer, tenant, application, _directory, registry, handles, _store): PublicHostFixture,
) {
    let (peer_client, dropped, dispatched) =
        peer_client_with_one_lost_mutation(registry, handles, true, 1);
    let peer =
        node.application_handle::<fixture::ReferenceApplication>(peer_client, tenant, application);
    let target = CellTarget::new(
        tenant,
        application,
        fixture::SQL_NAMESPACE,
        &partition_for_shard(0),
    )
    .expect("SQL target");
    let peer_sql = peer
        .sql::<fixture::ReferenceSql>(target.clone())
        .expect("peer SQL");
    let observer_sql = observer
        .sql::<fixture::ReferenceSql>(target)
        .expect("observer SQL");
    let mutation = identity(200, now_ms());
    let payload = b"retry-after-absent-resolution".to_vec();
    let insert = SqlBatch {
        statements: vec![SqlStatement {
            sql: "INSERT INTO qualification_rows (id, payload) VALUES (?1, ?2)".into(),
            parameters: vec![SqlValue::Integer(1), SqlValue::Blob(payload.clone())],
        }],
    };
    let pending = match peer_sql.batch(mutation, insert.clone()).await {
        Err(InvocationError::Pending(pending)) => pending,
        outcome => panic!("expected a lost request before dispatch: {outcome:?}"),
    };
    assert_eq!(dropped.load(Ordering::Acquire), 1);
    assert_eq!(dispatched.load(Ordering::Acquire), 0);
    assert_eq!(
        peer.resolve(&pending).await.expect("request resolution"),
        Resolution::Absent
    );
    let retried = peer_sql
        .batch(mutation, insert)
        .await
        .expect("retry absent SQL request");
    assert_eq!(retried.output.len(), 1);
    assert_eq!(retried.output[0].rows_affected, 1);
    assert_eq!(dispatched.load(Ordering::Acquire), 1);
    let observed = observer_sql
        .query(
            None,
            SqlBatch {
                statements: vec![SqlStatement {
                    sql: "SELECT payload FROM qualification_rows WHERE id = ?1".into(),
                    parameters: vec![SqlValue::Integer(1)],
                }],
            },
        )
        .await
        .expect("independent SQL observation");
    assert_eq!(observed.output[0].rows, vec![vec![SqlValue::Blob(payload)]]);
    assert!(observed.receipt.commit_sequence >= retried.receipt.commit_sequence);

    drop(peer);
    drop(observer);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_kv_absent_request_retries_same_identity_once() {
    run_kv_absent_request_retry(public_host_fixture().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_kv_absent_request_retries_same_identity_once() {
    let (store, root) = rustfs_public_store();
    run_kv_absent_request_retry(public_host_fixture_with_store(store, root).await).await;
}

async fn run_kv_absent_request_retry(
    (node, observer, tenant, application, _directory, registry, handles, _store): PublicHostFixture,
) {
    let (peer_client, dropped, dispatched) =
        peer_client_with_one_lost_mutation(registry, handles, true, 1);
    let peer =
        node.application_handle::<fixture::ReferenceApplication>(peer_client, tenant, application);
    let peer_kv = peer
        .kv::<fixture::ReferenceKv>(fixture::KV_NAMESPACE)
        .expect("peer KV");
    let observer_kv = observer
        .kv::<fixture::ReferenceKv>(fixture::KV_NAMESPACE)
        .expect("observer KV");
    let mutation = identity(201, now_ms());
    let scope = b"retry-after-absence".to_vec();
    let key = b"one-value".to_vec();
    let value = b"committed-on-retry".to_vec();
    let request = KvAtomicRequest {
        scope: scope.clone(),
        checks: Vec::new(),
        mutations: vec![KvMutation::Put {
            key: key.clone(),
            value: value.clone(),
            expires_at_ms: None,
        }],
    };
    let pending = match peer_kv.atomic(mutation, request.clone()).await {
        Err(InvocationError::Pending(pending)) => pending,
        outcome => panic!("expected a lost request before dispatch: {outcome:?}"),
    };
    assert_eq!(dropped.load(Ordering::Acquire), 1);
    assert_eq!(dispatched.load(Ordering::Acquire), 0);
    assert_eq!(
        peer.resolve(&pending).await.expect("request resolution"),
        Resolution::Absent
    );
    let retried = peer_kv
        .atomic(mutation, request)
        .await
        .expect("retry absent KV request");
    let KvAtomicOutcome::Applied(results) = retried.output else {
        panic!("retried KV request was not applied");
    };
    let version = results[0].version.expect("retried KV version");
    assert_eq!(dispatched.load(Ordering::Acquire), 1);
    let observed = observer_kv
        .get(scope, key, None)
        .await
        .expect("independent KV observation");
    let entry = observed.output.expect("retried KV value");
    assert_eq!(entry.value, value);
    assert_eq!(entry.version, version);
    assert!(observed.receipt.commit_sequence >= retried.receipt.commit_sequence);

    drop(peer);
    drop(observer);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_cron_absent_request_retries_same_identity_once() {
    run_cron_absent_request_retry(public_host_fixture().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_cron_absent_request_retries_same_identity_once() {
    let (store, root) = rustfs_public_store();
    run_cron_absent_request_retry(public_host_fixture_with_store(store, root).await).await;
}

async fn run_cron_absent_request_retry(
    (node, observer, tenant, application, _directory, registry, handles, _store): PublicHostFixture,
) {
    let (peer_client, dropped, dispatched) =
        peer_client_with_one_lost_mutation(registry, handles, true, 1);
    let peer =
        node.application_handle::<fixture::ReferenceApplication>(peer_client, tenant, application);
    let peer_cron = peer.cron::<fixture::ReferenceCron>().expect("peer Cron");
    let observer_cron = observer
        .cron::<fixture::ReferenceCron>()
        .expect("observer Cron");
    let mutation = identity(202, now_ms());
    let schedule_id = fixed_id(202);
    let payload = b"scheduled-on-retry".to_vec();
    let next_due_ms = now_ms() + 60_000;
    let upsert = CronMutation::Upsert {
        schedule_id,
        target_index: 0,
        target_partition: partition_for_shard(0).to_vec(),
        payload: payload.clone(),
        interval_ms: 60_000,
        next_due_ms,
    };
    let pending = match peer_cron.mutate(mutation, upsert.clone()).await {
        Err(InvocationError::Pending(pending)) => pending,
        outcome => panic!("expected a lost request before dispatch: {outcome:?}"),
    };
    assert_eq!(dropped.load(Ordering::Acquire), 1);
    assert_eq!(dispatched.load(Ordering::Acquire), 0);
    assert_eq!(
        peer.resolve(&pending).await.expect("request resolution"),
        Resolution::Absent
    );
    let retried = peer_cron
        .mutate(mutation, upsert)
        .await
        .expect("retry absent Cron request");
    assert_eq!(
        retried.output,
        CronMutationOutcome::Applied { generation: 1 }
    );
    assert_eq!(dispatched.load(Ordering::Acquire), 1);
    let observed = observer_cron
        .get(schedule_id, None)
        .await
        .expect("independent Cron observation");
    let CronQueryResult::Get(Some(schedule)) = observed.output else {
        panic!("retried Cron schedule is absent");
    };
    assert_eq!(schedule.payload, payload);
    assert_eq!(schedule.generation, 1);
    assert_eq!(schedule.next_due_ms, next_due_ms);
    assert_eq!(schedule.occurrence, 0);
    assert!(schedule.enabled);
    assert!(observed.receipt.commit_sequence >= retried.receipt.commit_sequence);

    drop(peer);
    drop(observer);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_workflow_absent_request_retries_same_identity_once() {
    run_workflow_absent_request_retry(public_host_fixture().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_workflow_absent_request_retries_same_identity_once() {
    let (store, root) = rustfs_public_store();
    run_workflow_absent_request_retry(public_host_fixture_with_store(store, root).await).await;
}

async fn run_workflow_absent_request_retry(
    (node, observer, tenant, application, _directory, registry, handles, _store): PublicHostFixture,
) {
    let (peer_client, dropped, dispatched) =
        peer_client_with_one_lost_mutation(registry, handles, true, 1);
    let peer =
        node.application_handle::<fixture::ReferenceApplication>(peer_client, tenant, application);
    let peer_workflow = peer
        .workflow::<fixture::ReferenceWorkflow>()
        .expect("peer Workflow");
    let observer_workflow = observer
        .workflow::<fixture::ReferenceWorkflow>()
        .expect("observer Workflow");
    let mutation = identity(203, now_ms());
    let workflow_id = b"workflow-start-on-retry".to_vec();
    let event = b"completed-on-retry".to_vec();
    let pending = match peer_workflow
        .start(mutation, workflow_id.clone(), event.clone())
        .await
    {
        Err(InvocationError::Pending(pending)) => pending,
        outcome => panic!("expected a lost request before dispatch: {outcome:?}"),
    };
    assert_eq!(dropped.load(Ordering::Acquire), 1);
    assert_eq!(dispatched.load(Ordering::Acquire), 0);
    assert_eq!(
        peer.resolve(&pending).await.expect("request resolution"),
        Resolution::Absent
    );
    let retried = peer_workflow
        .start(mutation, workflow_id.clone(), event.clone())
        .await
        .expect("retry absent Workflow request");
    let WorkflowOutcome::Applied {
        run_id,
        status: WorkflowStatus::Completed,
        event_sequence: 1,
    } = retried.output
    else {
        panic!("retried Workflow start did not complete");
    };
    assert_eq!(dispatched.load(Ordering::Acquire), 1);
    let observed = observer_workflow
        .state(workflow_id.clone(), None)
        .await
        .expect("independent Workflow observation");
    let run = observed.output.expect("retried Workflow run");
    assert_eq!(run.workflow_id, workflow_id);
    assert_eq!(run.run_id, run_id);
    assert_eq!(run.status, WorkflowStatus::Completed);
    assert_eq!(run.event_sequence, 1);
    assert_eq!(run.result.as_deref(), Some(event.as_slice()));
    assert!(observed.receipt.commit_sequence >= retried.receipt.commit_sequence);

    drop(peer);
    drop(observer);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_blob_absent_request_retries_same_identity_once() {
    run_blob_absent_request_retry(public_host_fixture().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_blob_absent_request_retries_same_identity_once() {
    let (store, root) = rustfs_public_store();
    run_blob_absent_request_retry(public_host_fixture_with_store(store, root).await).await;
}

async fn run_blob_absent_request_retry(
    (node, observer, tenant, application, _directory, registry, handles, store): PublicHostFixture,
) {
    let (peer_client, dropped, dispatched) =
        peer_client_with_one_lost_mutation(registry, handles, true, 1);
    let peer = node
        .application_handle::<fixture::ReferenceApplication>(peer_client, tenant, application)
        .with_blob_artifact_store(BlobArtifactStore::new(store));
    let peer_blob = peer.blob::<fixture::ReferenceBlob>().expect("peer Blob");
    let observer_blob = observer
        .blob::<fixture::ReferenceBlob>()
        .expect("observer Blob");
    let issued_at_ms = now_ms();
    let key = b"blob-complete-on-retry".to_vec();
    let upload_id = fixed_id(204);
    let payload = b"published-on-retry".to_vec();
    observer_blob
        .mutate(
            identity(204, issued_at_ms),
            BlobMutation::Begin {
                key: key.clone(),
                upload_id,
                condition: BlobCondition::Missing,
                content_type: None,
                metadata: Vec::new(),
                expires_at_ms: issued_at_ms + 60_000,
            },
        )
        .await
        .expect("Blob upload begin");
    observer_blob
        .mutate(
            identity(205, issued_at_ms),
            BlobMutation::PutPart {
                key: key.clone(),
                upload_id,
                part_number: 1,
                payload: payload.clone(),
            },
        )
        .await
        .expect("Blob upload part");
    let mutation = identity(206, issued_at_ms);
    let complete = BlobMutation::Complete {
        key: key.clone(),
        upload_id,
        part_count: 1,
    };
    let pending = match peer_blob.mutate(mutation, complete.clone()).await {
        Err(InvocationError::Pending(pending)) => pending,
        outcome => panic!("expected a lost request before dispatch: {outcome:?}"),
    };
    assert_eq!(dropped.load(Ordering::Acquire), 1);
    assert_eq!(dispatched.load(Ordering::Acquire), 0);
    assert_eq!(
        peer.resolve(&pending).await.expect("request resolution"),
        Resolution::Absent
    );
    let retried = peer_blob
        .mutate(mutation, complete)
        .await
        .expect("retry absent Blob request");
    let BlobMutationOutcome::Committed { etag, size } = retried.output else {
        panic!("retried Blob was not committed");
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
        .expect("independent Blob observation");
    let BlobQueryResult::Read(Some(read)) = observed.output else {
        panic!("retried Blob is absent");
    };
    assert_eq!(read.bytes, payload);
    assert_eq!(read.metadata.etag, etag);
    assert_eq!(read.metadata.size, size);
    assert!(observed.receipt.commit_sequence >= retried.receipt.commit_sequence);

    drop(peer);
    drop(observer);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_queue_absent_request_retries_same_identity_once() {
    run_queue_absent_request_retry(public_host_fixture().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_queue_absent_request_retries_same_identity_once() {
    let (store, root) = rustfs_public_store();
    run_queue_absent_request_retry(public_host_fixture_with_store(store, root).await).await;
}

async fn run_queue_absent_request_retry(
    (node, observer, tenant, application, _directory, registry, handles, _store): PublicHostFixture,
) {
    let (peer_client, dropped, dispatched) =
        peer_client_with_one_lost_mutation(registry, handles, true, 1);
    let peer =
        node.application_handle::<fixture::ReferenceApplication>(peer_client, tenant, application);
    let peer_queue = peer.queue::<fixture::ReferenceQueue>().expect("peer Queue");
    let observer_queue = observer
        .queue::<fixture::ReferenceQueue>()
        .expect("observer Queue");
    let issued_at_ms = now_ms();
    let mutation = identity(207, issued_at_ms);
    let payload = b"queued-on-retry".to_vec();
    let request = QueueSendRequest {
        producer_id: fixed_id(207),
        payload: payload.clone(),
        available_at_ms: issued_at_ms,
    };
    let pending = match peer_queue.send(mutation, request.clone()).await {
        Err(InvocationError::Pending(pending)) => pending,
        outcome => panic!("expected a lost request before dispatch: {outcome:?}"),
    };
    assert_eq!(dropped.load(Ordering::Acquire), 1);
    assert_eq!(dispatched.load(Ordering::Acquire), 0);
    assert_eq!(
        peer.resolve(&pending).await.expect("request resolution"),
        Resolution::Absent
    );
    let retried = peer_queue
        .send(mutation, request)
        .await
        .expect("retry absent Queue request");
    let QueueSendOutcome::Sent { message_id } = retried.output else {
        panic!("retried Queue message was not sent");
    };
    assert_eq!(dispatched.load(Ordering::Acquire), 1);
    let claimed = observer_queue
        .claim(
            identity(208, now_ms()),
            0,
            QueueClaimRequest {
                limit: 2,
                lease_ms: 5_000,
            },
        )
        .await
        .expect("independent Queue claim");
    assert_eq!(claimed.output.len(), 1);
    let message = &claimed.output[0];
    assert_eq!(message.message_id, message_id);
    assert_eq!(message.payload, payload);
    assert_eq!(message.attempt, 1);
    assert!(claimed.receipt.commit_sequence > retried.receipt.commit_sequence);
    let acked = observer_queue
        .ack(
            identity(209, now_ms()),
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
async fn public_effect_absent_ack_retries_same_identity_once() {
    run_effect_absent_ack_retry(public_host_fixture().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_effect_absent_ack_retries_same_identity_once() {
    let (store, root) = rustfs_public_store();
    run_effect_absent_ack_retry(public_host_fixture_with_store(store, root).await).await;
}

async fn run_effect_absent_ack_retry(
    (node, observer, tenant, application, _directory, registry, handles, _store): PublicHostFixture,
) {
    let (peer_client, dropped, dispatched) =
        peer_client_with_one_lost_mutation(registry, handles, true, 1);
    let peer =
        node.application_handle::<fixture::ReferenceApplication>(peer_client, tenant, application);
    let target = CellTarget::new(
        tenant,
        application,
        fixture::WORKFLOW_NAMESPACE,
        &partition_for_shard(0),
    )
    .expect("Effect target");
    let peer_effects = peer
        .effects::<fixture::ReferenceWorkflow>(target.clone())
        .expect("peer Effects");
    let observer_effects = observer
        .effects::<fixture::ReferenceWorkflow>(target)
        .expect("observer Effects");
    let workflow = observer
        .workflow::<fixture::ReferenceWorkflow>()
        .expect("source Workflow");
    let issued_at_ms = now_ms();
    workflow
        .start(
            identity(210, issued_at_ms),
            b"effect-ack-on-retry".to_vec(),
            b"effect".to_vec(),
        )
        .await
        .expect("source Effect publication");
    let claimed = observer_effects
        .claim(
            identity(211, issued_at_ms),
            EffectClaimRequest {
                limit: 1,
                lease_ms: 60_000,
            },
        )
        .await
        .expect("source Effect claim");
    assert_eq!(claimed.output.len(), 1);
    let claim = claimed.output[0].clone();
    let mutation = identity(212, issued_at_ms);
    let result = b"delivered-on-retry".to_vec();
    let pending = match peer_effects
        .ack(mutation, claim.clone(), result.clone())
        .await
    {
        Err(InvocationError::Pending(pending)) => pending,
        outcome => panic!("expected a lost request before dispatch: {outcome:?}"),
    };
    assert_eq!(dropped.load(Ordering::Acquire), 1);
    assert_eq!(dispatched.load(Ordering::Acquire), 0);
    assert_eq!(
        peer.resolve(&pending).await.expect("request resolution"),
        Resolution::Absent
    );
    let retried = peer_effects
        .ack(mutation, claim.clone(), result)
        .await
        .expect("retry absent Effect acknowledgement");
    assert_eq!(retried.output, EffectLeaseOutcome::Delivered);
    assert_eq!(dispatched.load(Ordering::Acquire), 1);
    let settled = observer_effects
        .validate(vec![claim], claimed.receipt)
        .await
        .expect("independent Effect settlement observation");
    assert!(!settled.output);
    assert!(settled.receipt.commit_sequence >= retried.receipt.commit_sequence);
    let remaining = observer_effects
        .claim(
            identity(213, now_ms()),
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
