use std::{
    sync::atomic::Ordering,
    time::{SystemTime, UNIX_EPOCH},
};

use crab_cell_runtime::{
    BlobArtifactStore, BlobCondition, BlobMutation, BlobQuery, BlobQueryResult, CellTarget,
    CronMutation, CronQueryResult, EffectClaimRequest, InvocationError, KvAtomicRequest,
    KvMutation, QueueClaimRequest, QueueLeaseOutcome, QueueSendRequest, QueueState, Resolution,
    SqlBatch, SqlStatement, SqlValue, StoredOutcome, WorkflowStatus, partition_for_shard,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_sql_lost_response_resolves_committed_without_replay() {
    run_public_sql_lost_response(public_host_fixture().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_sql_lost_response_resolves_committed_without_replay() {
    let (store, root) = rustfs_public_store();
    run_public_sql_lost_response(public_host_fixture_with_store(store, root).await).await;
}

async fn run_public_sql_lost_response(
    (node, local, tenant, application, _directory, registry, handles, _store): PublicHostFixture,
) {
    let (peer_client, dropped, _) = peer_client_with_one_lost_mutation(registry, handles, false, 1);
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
    let observer_sql = local
        .sql::<fixture::ReferenceSql>(target)
        .expect("observer SQL");
    let now_ms = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("qualification clock")
            .as_millis(),
    )
    .expect("qualification clock bounds");
    let payload = b"committed-before-response-loss".to_vec();
    let insert = SqlBatch {
        statements: vec![SqlStatement {
            sql: "INSERT INTO qualification_rows (id, payload) VALUES (?1, ?2)".into(),
            parameters: vec![SqlValue::Integer(1), SqlValue::Blob(payload.clone())],
        }],
    };
    let pending = match peer_sql.batch(identity(1, now_ms), insert).await {
        Err(InvocationError::Pending(pending)) => pending,
        outcome => panic!("expected a lost response after dispatch: {outcome:?}"),
    };
    assert_eq!(dropped.load(Ordering::Acquire), 1);
    let commit_sequence = match peer.resolve(&pending).await.expect("request resolution") {
        Resolution::Committed(StoredOutcome::Success {
            commit_sequence, ..
        }) => commit_sequence,
        resolution => panic!("expected committed request: {resolution:?}"),
    };
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
    assert!(observed.receipt.commit_sequence >= commit_sequence);

    drop(peer);
    drop(local);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_kv_lost_response_resolves_committed_without_replay() {
    run_public_kv_lost_response(public_host_fixture().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_kv_lost_response_resolves_committed_without_replay() {
    let (store, root) = rustfs_public_store();
    run_public_kv_lost_response(public_host_fixture_with_store(store, root).await).await;
}

async fn run_public_kv_lost_response(
    (node, local, tenant, application, _directory, registry, handles, _store): PublicHostFixture,
) {
    let (peer_client, dropped, _) = peer_client_with_one_lost_mutation(registry, handles, false, 1);
    let peer =
        node.application_handle::<fixture::ReferenceApplication>(peer_client, tenant, application);
    let peer_kv = peer
        .kv::<fixture::ReferenceKv>(fixture::KV_NAMESPACE)
        .expect("peer KV");
    let observer_kv = local
        .kv::<fixture::ReferenceKv>(fixture::KV_NAMESPACE)
        .expect("observer KV");
    let now_ms = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("qualification clock")
            .as_millis(),
    )
    .expect("qualification clock bounds");
    let scope = b"response-loss".to_vec();
    let key = b"confirmed-commit".to_vec();
    let value = b"committed-before-response-loss".to_vec();
    let pending = match peer_kv
        .atomic(
            identity(2, now_ms),
            KvAtomicRequest {
                scope: scope.clone(),
                checks: Vec::new(),
                mutations: vec![KvMutation::Put {
                    key: key.clone(),
                    value: value.clone(),
                    expires_at_ms: None,
                }],
            },
        )
        .await
    {
        Err(InvocationError::Pending(pending)) => pending,
        outcome => panic!("expected a lost response after dispatch: {outcome:?}"),
    };
    assert_eq!(dropped.load(Ordering::Acquire), 1);
    let commit_sequence = match peer.resolve(&pending).await.expect("request resolution") {
        Resolution::Committed(StoredOutcome::Success {
            commit_sequence, ..
        }) => commit_sequence,
        resolution => panic!("expected committed request: {resolution:?}"),
    };
    let observed = observer_kv
        .get(scope, key, None)
        .await
        .expect("independent KV observation");
    assert_eq!(observed.output.map(|entry| entry.value), Some(value));
    assert!(observed.receipt.commit_sequence >= commit_sequence);

    drop(peer);
    drop(local);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_queue_lost_response_resolves_one_message_without_replay() {
    run_public_queue_lost_response(public_host_fixture().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_queue_lost_response_resolves_one_message_without_replay() {
    let (store, root) = rustfs_public_store();
    run_public_queue_lost_response(public_host_fixture_with_store(store, root).await).await;
}

async fn run_public_queue_lost_response(
    (node, local, tenant, application, _directory, registry, handles, _store): PublicHostFixture,
) {
    let (peer_client, dropped, _) = peer_client_with_one_lost_mutation(registry, handles, false, 1);
    let peer =
        node.application_handle::<fixture::ReferenceApplication>(peer_client, tenant, application);
    let peer_queue = peer.queue::<fixture::ReferenceQueue>().expect("peer Queue");
    let observer_queue = local
        .queue::<fixture::ReferenceQueue>()
        .expect("observer Queue");
    let now_ms = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("qualification clock")
            .as_millis(),
    )
    .expect("qualification clock bounds");
    let payload = b"committed-before-response-loss".to_vec();
    let pending = match peer_queue
        .send(
            identity(3, now_ms),
            QueueSendRequest {
                producer_id: fixed_id(3),
                payload: payload.clone(),
                available_at_ms: now_ms,
            },
        )
        .await
    {
        Err(InvocationError::Pending(pending)) => pending,
        outcome => panic!("expected a lost response after dispatch: {outcome:?}"),
    };
    assert_eq!(dropped.load(Ordering::Acquire), 1);
    let commit_sequence = match peer.resolve(&pending).await.expect("request resolution") {
        Resolution::Committed(StoredOutcome::Success {
            commit_sequence, ..
        }) => commit_sequence,
        resolution => panic!("expected committed request: {resolution:?}"),
    };
    let claimed = observer_queue
        .claim(
            identity(4, now_ms),
            0,
            QueueClaimRequest {
                limit: 2,
                lease_ms: 5_000,
            },
        )
        .await
        .expect("independent Queue claim");
    assert_eq!(claimed.output.len(), 1);
    assert_eq!(claimed.output[0].payload, payload);
    assert_eq!(claimed.output[0].attempt, 1);
    assert!(claimed.receipt.commit_sequence > commit_sequence);
    assert!(
        observer_queue
            .validate_claim(0, claimed.output.clone(), Some(claimed.receipt))
            .await
            .expect("Queue claim validation")
            .output
    );
    let message = &claimed.output[0];
    let acked = observer_queue
        .ack(identity(5, now_ms), 0, message.message_id, message.token)
        .await
        .expect("Queue acknowledgement");
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
        .expect("Queue settlement observation");
    assert_eq!((info.output.ready, info.output.leased), (0, 0));

    drop(peer);
    drop(local);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_cron_lost_response_resolves_schedule_without_replay() {
    run_public_cron_lost_response(public_host_fixture().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_cron_lost_response_resolves_schedule_without_replay() {
    let (store, root) = rustfs_public_store();
    run_public_cron_lost_response(public_host_fixture_with_store(store, root).await).await;
}

async fn run_public_cron_lost_response(
    (node, local, tenant, application, _directory, registry, handles, _store): PublicHostFixture,
) {
    let (peer_client, dropped, _) = peer_client_with_one_lost_mutation(registry, handles, false, 1);
    let peer =
        node.application_handle::<fixture::ReferenceApplication>(peer_client, tenant, application);
    let peer_cron = peer.cron::<fixture::ReferenceCron>().expect("peer Cron");
    let observer_cron = local
        .cron::<fixture::ReferenceCron>()
        .expect("observer Cron");
    let now_ms = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("qualification clock")
            .as_millis(),
    )
    .expect("qualification clock bounds");
    let schedule_id = fixed_id(4);
    let payload = b"committed-before-response-loss".to_vec();
    let next_due_ms = now_ms + 60_000;
    let pending = match peer_cron
        .mutate(
            identity(4, now_ms),
            CronMutation::Upsert {
                schedule_id,
                target_index: 0,
                target_partition: partition_for_shard(0).to_vec(),
                payload: payload.clone(),
                interval_ms: 60_000,
                next_due_ms,
            },
        )
        .await
    {
        Err(InvocationError::Pending(pending)) => pending,
        outcome => panic!("expected a lost response after dispatch: {outcome:?}"),
    };
    assert_eq!(dropped.load(Ordering::Acquire), 1);
    let commit_sequence = match peer.resolve(&pending).await.expect("request resolution") {
        Resolution::Committed(StoredOutcome::Success {
            commit_sequence, ..
        }) => commit_sequence,
        resolution => panic!("expected committed request: {resolution:?}"),
    };
    let observed = observer_cron
        .get(schedule_id, None)
        .await
        .expect("independent Cron observation");
    assert!(
        matches!(observed.output, CronQueryResult::Get(Some(ref schedule))
        if schedule.schedule_id == schedule_id
            && schedule.payload == payload
            && schedule.enabled
            && schedule.generation == 1
            && schedule.occurrence == 0
            && schedule.next_due_ms == next_due_ms)
    );
    assert!(observed.receipt.commit_sequence >= commit_sequence);

    drop(peer);
    drop(local);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_workflow_lost_response_resolves_terminal_run_without_replay() {
    run_public_workflow_lost_response(public_host_fixture().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_workflow_lost_response_resolves_terminal_run_without_replay() {
    let (store, root) = rustfs_public_store();
    run_public_workflow_lost_response(public_host_fixture_with_store(store, root).await).await;
}

async fn run_public_workflow_lost_response(
    (node, local, tenant, application, _directory, registry, handles, _store): PublicHostFixture,
) {
    let (peer_client, dropped, _) = peer_client_with_one_lost_mutation(registry, handles, false, 1);
    let peer =
        node.application_handle::<fixture::ReferenceApplication>(peer_client, tenant, application);
    let peer_workflow = peer
        .workflow::<fixture::ReferenceWorkflow>()
        .expect("peer Workflow");
    let observer_workflow = local
        .workflow::<fixture::ReferenceWorkflow>()
        .expect("observer Workflow");
    let now_ms = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("qualification clock")
            .as_millis(),
    )
    .expect("qualification clock bounds");
    let workflow_id = b"response-loss-workflow".to_vec();
    let event = b"complete".to_vec();
    let pending = match peer_workflow
        .start(identity(5, now_ms), workflow_id.clone(), event.clone())
        .await
    {
        Err(InvocationError::Pending(pending)) => pending,
        outcome => panic!("expected a lost response after dispatch: {outcome:?}"),
    };
    assert_eq!(dropped.load(Ordering::Acquire), 1);
    let commit_sequence = match peer.resolve(&pending).await.expect("request resolution") {
        Resolution::Committed(StoredOutcome::Success {
            commit_sequence, ..
        }) => commit_sequence,
        resolution => panic!("expected committed request: {resolution:?}"),
    };
    let observed = observer_workflow
        .state(workflow_id.clone(), None)
        .await
        .expect("independent Workflow observation");
    assert!(
        observed
            .output
            .as_ref()
            .is_some_and(|run| run.workflow_id == workflow_id
                && run.status == WorkflowStatus::Completed
                && run.event_sequence == 1
                && run.result.as_deref() == Some(event.as_slice()))
    );
    assert!(observed.receipt.commit_sequence >= commit_sequence);

    drop(peer);
    drop(local);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_effect_lost_response_resolves_settled_lease_without_replay() {
    run_public_effect_lost_response(public_host_fixture().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_effect_lost_response_resolves_settled_lease_without_replay() {
    let (store, root) = rustfs_public_store();
    run_public_effect_lost_response(public_host_fixture_with_store(store, root).await).await;
}

async fn run_public_effect_lost_response(
    (node, local, tenant, application, _directory, registry, handles, _store): PublicHostFixture,
) {
    let (peer_client, dropped, _) = peer_client_with_one_lost_mutation(registry, handles, false, 1);
    let peer =
        node.application_handle::<fixture::ReferenceApplication>(peer_client, tenant, application);
    let target = CellTarget::new(
        tenant,
        application,
        fixture::WORKFLOW_NAMESPACE,
        &partition_for_shard(0),
    )
    .expect("Effect source target");
    let peer_effects = peer
        .effects::<fixture::ReferenceWorkflow>(target.clone())
        .expect("peer Effects");
    let observer_effects = local
        .effects::<fixture::ReferenceWorkflow>(target)
        .expect("observer Effects");
    let workflow = local
        .workflow::<fixture::ReferenceWorkflow>()
        .expect("source Workflow");
    let now_ms = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("qualification clock")
            .as_millis(),
    )
    .expect("qualification clock bounds");
    workflow
        .start(
            identity(6, now_ms),
            b"response-loss-effect".to_vec(),
            b"effect".to_vec(),
        )
        .await
        .expect("source Effect publication");
    let claimed = observer_effects
        .claim(
            identity(7, now_ms),
            EffectClaimRequest {
                limit: 1,
                lease_ms: 60_000,
            },
        )
        .await
        .expect("source Effect claim");
    assert_eq!(claimed.output.len(), 1);
    let claim = claimed.output[0].clone();
    assert!(
        observer_effects
            .validate(vec![claim.clone()], claimed.receipt)
            .await
            .expect("Effect lease validation")
            .output
    );
    let pending = match peer_effects
        .ack(
            identity(8, now_ms),
            claim.clone(),
            b"effect-result".to_vec(),
        )
        .await
    {
        Err(InvocationError::Pending(pending)) => pending,
        outcome => panic!("expected a lost response after dispatch: {outcome:?}"),
    };
    assert_eq!(dropped.load(Ordering::Acquire), 1);
    let commit_sequence = match peer.resolve(&pending).await.expect("request resolution") {
        Resolution::Committed(StoredOutcome::Success {
            commit_sequence, ..
        }) => commit_sequence,
        resolution => panic!("expected committed request: {resolution:?}"),
    };
    let settled = observer_effects
        .validate(vec![claim], claimed.receipt)
        .await
        .expect("independent Effect settlement observation");
    assert!(!settled.output);
    assert!(settled.receipt.commit_sequence >= commit_sequence);
    let remaining = observer_effects
        .claim(
            identity(9, now_ms),
            EffectClaimRequest {
                limit: 1,
                lease_ms: 60_000,
            },
        )
        .await
        .expect("Effect queue observation");
    assert!(remaining.output.is_empty());

    drop(peer);
    drop(local);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_blob_lost_response_resolves_published_bytes_without_replay() {
    run_public_blob_lost_response(public_host_fixture().await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_blob_lost_response_resolves_published_bytes_without_replay() {
    let (store, root) = rustfs_public_store();
    run_public_blob_lost_response(public_host_fixture_with_store(store, root).await).await;
}

async fn run_public_blob_lost_response(
    (node, local, tenant, application, _directory, registry, handles, store): PublicHostFixture,
) {
    let (peer_client, dropped, _) = peer_client_with_one_lost_mutation(registry, handles, false, 1);
    let peer = node
        .application_handle::<fixture::ReferenceApplication>(peer_client, tenant, application)
        .with_blob_artifact_store(BlobArtifactStore::new(store));
    let peer_blob = peer.blob::<fixture::ReferenceBlob>().expect("peer Blob");
    let observer_blob = local
        .blob::<fixture::ReferenceBlob>()
        .expect("observer Blob");
    let now_ms = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("qualification clock")
            .as_millis(),
    )
    .expect("qualification clock bounds");
    let key = b"response-loss-blob".to_vec();
    let upload_id = fixed_id(10);
    let payload = b"committed-before-response-loss".to_vec();
    observer_blob
        .mutate(
            identity(10, now_ms),
            BlobMutation::Begin {
                key: key.clone(),
                upload_id,
                condition: BlobCondition::Missing,
                content_type: None,
                metadata: Vec::new(),
                expires_at_ms: now_ms + 60_000,
            },
        )
        .await
        .expect("Blob upload begin");
    observer_blob
        .mutate(
            identity(11, now_ms),
            BlobMutation::PutPart {
                key: key.clone(),
                upload_id,
                part_number: 1,
                payload: payload.clone(),
            },
        )
        .await
        .expect("Blob upload part");
    let pending = match peer_blob
        .mutate(
            identity(12, now_ms),
            BlobMutation::Complete {
                key: key.clone(),
                upload_id,
                part_count: 1,
            },
        )
        .await
    {
        Err(InvocationError::Pending(pending)) => pending,
        outcome => panic!("expected a lost response after dispatch: {outcome:?}"),
    };
    assert_eq!(dropped.load(Ordering::Acquire), 1);
    let commit_sequence = match peer.resolve(&pending).await.expect("request resolution") {
        Resolution::Committed(StoredOutcome::Success {
            commit_sequence, ..
        }) => commit_sequence,
        resolution => panic!("expected committed request: {resolution:?}"),
    };
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
    assert!(
        matches!(observed.output, BlobQueryResult::Read(Some(ref read))
        if read.bytes == payload
            && read.metadata.size == payload.len() as u64)
    );
    assert!(observed.receipt.commit_sequence >= commit_sequence);

    drop(peer);
    drop(local);
    node.shutdown().await.expect("qualification shutdown");
    assert_zero_reservations(&node);
}
