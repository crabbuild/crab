use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use crab_cell_host::CellNodeBuilder;
use crab_cell_runtime::{
    ActivityRunOutcome, ActivitySupervisor, ApplicationId, BlobArtifactStore, BlobCondition,
    BlobMutation, BlobQuery, BlobQueryResult, CatalogRole, CellAuthority, CellCatalog, CellClient,
    CellReplica, CellStorageLayout, CellTarget, CronMutation, CronQueryResult, EffectClaimRequest,
    EffectLeaseOutcome, IncarnationId, KvAtomicOutcome, KvAtomicRequest, KvMutation,
    NodeLeaseGuard, Owner, QueueClaimRequest, QueueLeaseOutcome, QueueSendOutcome,
    QueueSendRequest, QueueState, RecoveryManifestStore, ReplicaLimits, SessionId, SqlBatch,
    SqlStatement, SqlValue, SqlWorkerPool, TenantId, WorkflowOutcome, WorkflowStatus,
    install_blob_schema, install_cron_schema, install_kv_schema, install_queue_schema,
    install_workflow_schema, partition_for_shard,
};
use crab_storage::Store;
use object_store::{memory::InMemory, path::Path};
use tokio_util::sync::CancellationToken;

#[path = "support/reference_application.rs"]
mod fixture;
#[path = "support/qualification.rs"]
mod qualification;
#[path = "support/qualification_fence.rs"]
mod qualification_fence;

use qualification::{assert_zero_reservations, fixed_id, identity, rustfs_public_store};
use qualification_fence::fence_public_session;

async fn run_public_primitive_takeover(store: Store, root: Path) {
    let application = Arc::new(fixture::compiled());
    let tenant = TenantId::from_bytes([71; 16]);
    let application_id = ApplicationId::from_bytes([72; 16]);
    let layout = CellStorageLayout::new(store.clone(), root, *application_id.as_bytes());
    let source_session = SessionId::from_bytes([24; 16]);
    let successor_session = SessionId::from_bytes([70; 16]);
    let source = CellNodeBuilder::new(Arc::clone(&application))
        .with_runtime(
            SqlWorkerPool::new(2, 8).expect("source pool"),
            32 * 1024 * 1024,
        )
        .with_replica_host(fixture::reference_host())
        .with_session(source_session)
        .build()
        .expect("source node");
    source
        .install_task_group(CancellationToken::new(), CancellationToken::new())
        .expect("source tasks");
    source
        .install_node_lease(NodeLeaseGuard::new(0, 60_000).expect("source lease"))
        .expect("source readiness");
    let source_dir = tempfile::tempdir().expect("source directory");
    let source_cells = vec![
        fixture::bootstrap_reference_cell(
            &source.runtime(),
            &application.registry(),
            &layout,
            &source_dir,
            tenant,
            application_id,
            fixture::SQL_NAMESPACE,
            CatalogRole::Sql,
            fixture::SQL_MODULE,
            40,
            |transaction| {
                transaction.execute_batch(
                    "CREATE TABLE qualification_rows (id INTEGER PRIMARY KEY, payload BLOB NOT NULL)",
                )?;
                Ok(())
            },
        )
        .await
        .expect("source SQL Cell"),
        fixture::bootstrap_reference_cell(
            &source.runtime(),
            &application.registry(),
            &layout,
            &source_dir,
            tenant,
            application_id,
            fixture::KV_NAMESPACE,
            CatalogRole::Kv,
            fixture::KV_MODULE,
            41,
            install_kv_schema,
        )
        .await
        .expect("source KV Cell"),
        fixture::bootstrap_reference_cell(
            &source.runtime(),
            &application.registry(),
            &layout,
            &source_dir,
            tenant,
            application_id,
            fixture::BLOB_NAMESPACE,
            CatalogRole::Blob,
            fixture::BLOB_MODULE,
            42,
            install_blob_schema,
        )
        .await
        .expect("source Blob Cell"),
        fixture::bootstrap_reference_cell(
            &source.runtime(),
            &application.registry(),
            &layout,
            &source_dir,
            tenant,
            application_id,
            fixture::QUEUE_NAMESPACE,
            CatalogRole::Queue,
            fixture::QUEUE_MODULE,
            43,
            install_queue_schema,
        )
        .await
        .expect("source Queue Cell"),
        fixture::bootstrap_reference_cell(
            &source.runtime(),
            &application.registry(),
            &layout,
            &source_dir,
            tenant,
            application_id,
            fixture::CRON_NAMESPACE,
            CatalogRole::Cron,
            fixture::CRON_MODULE,
            45,
            install_cron_schema,
        )
        .await
        .expect("source Cron Cell"),
        fixture::bootstrap_reference_cell(
            &source.runtime(),
            &application.registry(),
            &layout,
            &source_dir,
            tenant,
            application_id,
            fixture::WORKFLOW_NAMESPACE,
            CatalogRole::Workflow,
            fixture::WORKFLOW_MODULE,
            46,
            install_workflow_schema,
        )
        .await
        .expect("source Workflow Cell"),
    ];
    let source_handle = source
        .application_handle::<fixture::ReferenceApplication>(
            CellClient::local_many(application.registry(), source_cells)
                .expect("source typed client"),
            tenant,
            application_id,
        )
        .with_blob_artifact_store(BlobArtifactStore::new(store.clone()));
    let source_kv = source_handle
        .kv::<fixture::ReferenceKv>(fixture::KV_NAMESPACE)
        .expect("source typed KV");
    let scope = b"public-takeover".to_vec();
    let key = b"acknowledged".to_vec();
    let value = b"published-value".to_vec();
    let now_ms = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_millis(),
    )
    .expect("time fits i64");
    let sql_target = CellTarget::new(
        tenant,
        application_id,
        fixture::SQL_NAMESPACE,
        &partition_for_shard(0),
    )
    .expect("SQL target");
    let source_sql = source_handle
        .sql::<fixture::ReferenceSql>(sql_target.clone())
        .expect("source typed SQL");
    let inserted = source_sql
        .batch(
            identity(900_000, now_ms),
            SqlBatch {
                statements: vec![SqlStatement {
                    sql: "INSERT INTO qualification_rows (id, payload) VALUES (1, ?1)".into(),
                    parameters: vec![SqlValue::Blob(b"published-sql-value".to_vec())],
                }],
            },
        )
        .await
        .expect("acknowledged source SQL write");
    assert_eq!(inserted.output[0].rows_affected, 1);
    let written = source_kv
        .atomic(
            identity(900_001, now_ms),
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
        .expect("acknowledged source write");
    assert!(matches!(written.output, KvAtomicOutcome::Applied(_)));
    let source_blob = source_handle
        .blob::<fixture::ReferenceBlob>()
        .expect("source typed Blob");
    let blob_key = b"published-blob".to_vec();
    let upload_id = fixed_id(900_004);
    source_blob
        .mutate(
            identity(900_004, now_ms),
            BlobMutation::Begin {
                key: blob_key.clone(),
                upload_id,
                condition: BlobCondition::Missing,
                content_type: None,
                metadata: Vec::new(),
                expires_at_ms: now_ms + 60_000,
            },
        )
        .await
        .expect("source Blob begin");
    source_blob
        .mutate(
            identity(900_005, now_ms),
            BlobMutation::PutPart {
                key: blob_key.clone(),
                upload_id,
                part_number: 1,
                payload: b"published-blob-value".to_vec(),
            },
        )
        .await
        .expect("source Blob part");
    let committed = source_blob
        .mutate(
            identity(900_006, now_ms),
            BlobMutation::Complete {
                key: blob_key.clone(),
                upload_id,
                part_count: 1,
            },
        )
        .await
        .expect("acknowledged Blob commit");
    let crab_cell_runtime::BlobMutationOutcome::Committed { etag, size } = committed.output else {
        panic!("source Blob was not committed");
    };
    let source_queue = source_handle
        .queue::<fixture::ReferenceQueue>()
        .expect("source typed Queue");
    let producer_id = fixed_id(900_007);
    let queued = source_queue
        .send(
            identity(900_007, now_ms),
            QueueSendRequest {
                producer_id,
                payload: b"published-queue-value".to_vec(),
                available_at_ms: now_ms,
            },
        )
        .await
        .expect("acknowledged Queue send");
    let QueueSendOutcome::Sent { message_id } = queued.output else {
        panic!("source Queue message was not sent");
    };
    let source_cron = source_handle
        .cron::<fixture::ReferenceCron>()
        .expect("source typed Cron");
    let schedule_id = fixed_id(900_012);
    let scheduled = source_cron
        .mutate(
            identity(900_012, now_ms),
            CronMutation::Upsert {
                schedule_id,
                target_index: 0,
                target_partition: partition_for_shard(0).to_vec(),
                payload: b"published-cron-value".to_vec(),
                interval_ms: 60_000,
                next_due_ms: now_ms + 60_000,
            },
        )
        .await
        .expect("acknowledged Cron schedule");
    assert!(matches!(
        scheduled.output,
        crab_cell_runtime::CronMutationOutcome::Applied { .. }
    ));
    let source_workflow = source_handle
        .workflow::<fixture::ReferenceWorkflow>()
        .expect("source typed Workflow");
    let workflow_id = b"published-workflow".to_vec();
    let workflow_started = source_workflow
        .start(
            identity(900_013, now_ms),
            workflow_id.clone(),
            b"workflow-result".to_vec(),
        )
        .await
        .expect("acknowledged Workflow start");
    let WorkflowOutcome::Applied {
        run_id: workflow_run_id,
        status: WorkflowStatus::Completed,
        ..
    } = workflow_started.output
    else {
        panic!("source Workflow did not complete");
    };
    let activity_workflow_id = b"published-activity".to_vec();
    let activity_started = source_workflow
        .start(
            identity(900_014, now_ms),
            activity_workflow_id.clone(),
            b"activity".to_vec(),
        )
        .await
        .expect("acknowledged Activity workflow start");
    assert!(matches!(
        activity_started.output,
        WorkflowOutcome::Applied {
            status: WorkflowStatus::Running,
            ..
        }
    ));
    let effect_workflow_id = b"published-effect".to_vec();
    let effect_started = source_workflow
        .start(
            identity(900_015, now_ms),
            effect_workflow_id.clone(),
            b"effect".to_vec(),
        )
        .await
        .expect("acknowledged Effect workflow start");
    assert!(matches!(
        effect_started.output,
        WorkflowOutcome::Applied {
            status: WorkflowStatus::Completed,
            ..
        }
    ));

    let fenced = fence_public_session(&layout, source_session, successor_session).await;
    let authority = CellAuthority::new(layout.clone());
    let successor = CellNodeBuilder::new(Arc::clone(&application))
        .with_runtime(
            SqlWorkerPool::new(2, 8).expect("successor pool"),
            32 * 1024 * 1024,
        )
        .with_replica_host(fixture::reference_host())
        .with_session(successor_session)
        .build()
        .expect("successor node");
    successor
        .install_task_group(CancellationToken::new(), CancellationToken::new())
        .expect("successor tasks");
    successor
        .install_node_lease(NodeLeaseGuard::new(0, 60_000).expect("successor lease"))
        .expect("successor readiness");
    let successor_dir = tempfile::tempdir().expect("successor directory");
    assert_eq!(
        std::fs::read_dir(successor_dir.path())
            .expect("successor directory listing")
            .count(),
        0,
        "successor did not start from empty local storage"
    );
    let mut restored_cells = Vec::new();
    for (namespace, module, incarnation_byte) in [
        (fixture::SQL_NAMESPACE, fixture::SQL_MODULE, 40_u8),
        (fixture::KV_NAMESPACE, fixture::KV_MODULE, 41_u8),
        (fixture::BLOB_NAMESPACE, fixture::BLOB_MODULE, 42_u8),
        (fixture::QUEUE_NAMESPACE, fixture::QUEUE_MODULE, 43_u8),
        (fixture::CRON_NAMESPACE, fixture::CRON_MODULE, 45_u8),
        (fixture::WORKFLOW_NAMESPACE, fixture::WORKFLOW_MODULE, 46_u8),
    ] {
        let target = CellTarget::new(tenant, application_id, namespace, &partition_for_shard(0))
            .expect("restored target");
        let observed = authority
            .load(target.cell_id())
            .await
            .expect("source authority")
            .expect("source owner");
        let published_root = observed.value().root.clone().expect("acknowledged root");
        let source_epoch = observed.value().epoch;
        let proof = CellCatalog::new(layout.clone(), tenant)
            .lookup(target.cell_id())
            .await
            .expect("source catalog")
            .expect("source provisioned");
        let restored = successor
            .runtime()
            .takeover_restored(
                proof,
                CellReplica::new(
                    layout.clone(),
                    *target.cell_id().as_bytes(),
                    *IncarnationId::from_bytes([incarnation_byte; 16]).as_bytes(),
                    ReplicaLimits::default(),
                )
                .expect("successor replica"),
                authority.clone(),
                observed,
                fenced.direct_takeover().expect("direct takeover proof"),
                RecoveryManifestStore::new(layout.clone(), ReplicaLimits::default()),
                successor_dir
                    .path()
                    .join(format!("{module}-successor.sqlite")),
                Owner {
                    session: successor_session,
                    endpoint: "https://public-successor.internal:8081".into(),
                },
            )
            .await
            .expect("exact-root takeover");
        let current = authority
            .load(target.cell_id())
            .await
            .expect("successor authority")
            .expect("successor owner");
        assert!(current.value().epoch > source_epoch);
        assert_eq!(current.value().root.as_ref(), Some(&published_root));
        assert_eq!(
            current.value().owner.as_ref().map(|owner| owner.session),
            Some(successor_session)
        );
        restored_cells.push(restored);
    }
    let successor_handle = successor
        .application_handle::<fixture::ReferenceApplication>(
            CellClient::local_many(application.registry(), restored_cells)
                .expect("successor typed client"),
            tenant,
            application_id,
        )
        .with_blob_artifact_store(BlobArtifactStore::new(store));
    let successor_sql = successor_handle
        .sql::<fixture::ReferenceSql>(sql_target)
        .expect("successor typed SQL");
    let sql_read = successor_sql
        .query(
            None,
            SqlBatch {
                statements: vec![SqlStatement {
                    sql: "SELECT payload FROM qualification_rows WHERE id = 1".into(),
                    parameters: Vec::new(),
                }],
            },
        )
        .await
        .expect("successor SQL read");
    assert_eq!(
        sql_read.output[0].rows,
        vec![vec![SqlValue::Blob(b"published-sql-value".to_vec())]]
    );
    let successor_blob = successor_handle
        .blob::<fixture::ReferenceBlob>()
        .expect("successor typed Blob");
    let blob_read = successor_blob
        .query(
            BlobQuery::Read {
                key: blob_key.clone(),
                offset: 0,
                limit: 128,
            },
            None,
        )
        .await
        .expect("successor Blob read");
    assert!(
        matches!(blob_read.output, BlobQueryResult::Read(Some(ref read))
        if read.bytes == b"published-blob-value"
            && read.metadata.etag == etag
            && read.metadata.size == size)
    );
    let successor_queue = successor_handle
        .queue::<fixture::ReferenceQueue>()
        .expect("successor typed Queue");
    let claimed = successor_queue
        .claim(
            identity(900_008, now_ms),
            0,
            QueueClaimRequest {
                limit: 1,
                lease_ms: 5_000,
            },
        )
        .await
        .expect("successor Queue claim");
    assert_eq!(claimed.output.len(), 1);
    let message = &claimed.output[0];
    assert_eq!(message.message_id, message_id);
    assert_eq!(message.payload, b"published-queue-value");
    let acked = successor_queue
        .ack(
            identity(900_009, now_ms),
            0,
            message.message_id,
            message.token,
        )
        .await
        .expect("successor Queue ack");
    assert!(matches!(
        acked.output,
        QueueLeaseOutcome::Applied {
            state: QueueState::Acked,
            ..
        }
    ));
    let queue_info = successor_queue
        .info(0, Some(acked.receipt))
        .await
        .expect("successor Queue info");
    assert_eq!(queue_info.output.acked, 1);
    assert_eq!(queue_info.output.ready, 0);
    assert_eq!(queue_info.output.leased, 0);
    let successor_cron = successor_handle
        .cron::<fixture::ReferenceCron>()
        .expect("successor typed Cron");
    let cron_read = successor_cron
        .get(schedule_id, None)
        .await
        .expect("successor Cron read");
    assert!(
        matches!(cron_read.output, CronQueryResult::Get(Some(ref schedule))
        if schedule.schedule_id == schedule_id
            && schedule.enabled
            && schedule.payload == b"published-cron-value"
            && schedule.next_due_ms == now_ms + 60_000)
    );
    let successor_workflow = successor_handle
        .workflow::<fixture::ReferenceWorkflow>()
        .expect("successor typed Workflow");
    let workflow_read = successor_workflow
        .state(workflow_id.clone(), None)
        .await
        .expect("successor Workflow read");
    assert!(matches!(workflow_read.output, Some(ref run)
        if run.run_id == workflow_run_id
            && run.status == WorkflowStatus::Completed
            && run.result.as_deref() == Some(b"workflow-result".as_slice())));
    let activity = ActivitySupervisor::new(
        successor_handle
            .activities::<fixture::ReferenceWorkflow>()
            .expect("successor typed Activity"),
        5_000,
    )
    .expect("successor Activity supervisor");
    let activity_outcome = activity
        .run_once(0, None)
        .await
        .expect("successor Activity run");
    assert!(matches!(
        activity_outcome,
        ActivityRunOutcome::Completed { .. }
    ));
    let activity_read = successor_workflow
        .state(activity_workflow_id, None)
        .await
        .expect("successor Activity state");
    assert!(matches!(activity_read.output, Some(ref run)
        if run.status == WorkflowStatus::Completed
            && run.result.as_deref().is_some_and(|result|
                result.starts_with(b"activity\0")
                    && result.ends_with(b"activity-result"))));
    assert!(matches!(
        activity
            .run_once(0, None)
            .await
            .expect("successor Activity settled check"),
        ActivityRunOutcome::Idle { .. }
    ));
    let effect_target = CellTarget::new(
        tenant,
        application_id,
        fixture::WORKFLOW_NAMESPACE,
        &partition_for_shard(0),
    )
    .expect("Effect target");
    let successor_effects = successor_handle
        .effects::<fixture::ReferenceWorkflow>(effect_target)
        .expect("successor typed Effects");
    let effects_claimed = successor_effects
        .claim(
            identity(900_016, now_ms),
            EffectClaimRequest {
                limit: 1,
                lease_ms: 5_000,
            },
        )
        .await
        .expect("successor Effect claim");
    assert_eq!(effects_claimed.output.len(), 1);
    let effect = effects_claimed.output[0].clone();
    assert!(
        successor_effects
            .validate(vec![effect.clone()], effects_claimed.receipt)
            .await
            .expect("successor Effect validation")
            .output
    );
    let effect_acked = successor_effects
        .ack(
            identity(900_017, now_ms),
            effect.clone(),
            b"published-effect-result".to_vec(),
        )
        .await
        .expect("successor Effect ack");
    assert_eq!(effect_acked.output, EffectLeaseOutcome::Delivered);
    assert!(
        !successor_effects
            .validate(vec![effect], effect_acked.receipt)
            .await
            .expect("successor Effect settlement")
            .output
    );
    assert!(
        successor_effects
            .claim(
                identity(900_020, now_ms),
                EffectClaimRequest {
                    limit: 1,
                    lease_ms: 5_000,
                },
            )
            .await
            .expect("successor Effect settled claim")
            .output
            .is_empty()
    );
    let effect_read = successor_workflow
        .state(effect_workflow_id, None)
        .await
        .expect("successor Effect workflow state");
    assert!(matches!(effect_read.output, Some(ref run)
        if run.status == WorkflowStatus::Completed
            && run.result.as_deref() == Some(b"effect-scheduled".as_slice())));
    let successor_kv = successor_handle
        .kv::<fixture::ReferenceKv>(fixture::KV_NAMESPACE)
        .expect("successor typed KV");
    let read = successor_kv
        .get(scope.clone(), key.clone(), None)
        .await
        .expect("successor read");
    assert_eq!(
        read.output.as_ref().map(|entry| entry.value.as_slice()),
        Some(value.as_slice())
    );
    let stale = source_kv
        .atomic(
            identity(900_002, now_ms),
            KvAtomicRequest {
                scope: scope.clone(),
                checks: Vec::new(),
                mutations: vec![KvMutation::Put {
                    key: key.clone(),
                    value: b"stale-write".to_vec(),
                    expires_at_ms: None,
                }],
            },
        )
        .await;
    assert!(stale.is_err(), "fenced owner accepted a new write");
    let read_after_fence = successor_kv
        .get(scope, key, None)
        .await
        .expect("successor recheck");
    assert_eq!(
        read_after_fence
            .output
            .as_ref()
            .map(|entry| entry.value.as_slice()),
        Some(value.as_slice())
    );
    let stale_sql = source_sql
        .batch(
            identity(900_003, now_ms),
            SqlBatch {
                statements: vec![SqlStatement {
                    sql: "INSERT INTO qualification_rows (id, payload) VALUES (2, X'00')".into(),
                    parameters: Vec::new(),
                }],
            },
        )
        .await;
    assert!(stale_sql.is_err(), "fenced owner accepted a SQL write");
    let sql_after_fence = successor_sql
        .query(
            None,
            SqlBatch {
                statements: vec![SqlStatement {
                    sql: "SELECT COUNT(*) FROM qualification_rows".into(),
                    parameters: Vec::new(),
                }],
            },
        )
        .await
        .expect("successor SQL recheck");
    assert_eq!(
        sql_after_fence.output[0].rows,
        vec![vec![SqlValue::Integer(1)]]
    );
    let stale_blob = source_blob
        .mutate(
            identity(900_010, now_ms),
            BlobMutation::Delete {
                key: blob_key.clone(),
                condition: BlobCondition::Any,
            },
        )
        .await;
    assert!(stale_blob.is_err(), "fenced owner accepted a Blob delete");
    let blob_after_fence = successor_blob
        .query(
            BlobQuery::Read {
                key: blob_key,
                offset: 0,
                limit: 128,
            },
            None,
        )
        .await
        .expect("successor Blob recheck");
    assert!(
        matches!(blob_after_fence.output, BlobQueryResult::Read(Some(ref read))
        if read.bytes == b"published-blob-value" && read.metadata.etag == etag)
    );
    let stale_queue = source_queue
        .send(
            identity(900_011, now_ms),
            QueueSendRequest {
                producer_id: fixed_id(900_011),
                payload: b"stale-queue-value".to_vec(),
                available_at_ms: now_ms,
            },
        )
        .await;
    assert!(stale_queue.is_err(), "fenced owner accepted a Queue send");
    let queue_after_fence = successor_queue
        .info(0, None)
        .await
        .expect("successor Queue recheck");
    assert_eq!(queue_after_fence.output.acked, 1);
    assert_eq!(queue_after_fence.output.ready, 0);
    assert_eq!(queue_after_fence.output.leased, 0);
    let stale_cron = source_cron
        .mutate(
            identity(900_018, now_ms),
            CronMutation::Pause { schedule_id },
        )
        .await;
    assert!(stale_cron.is_err(), "fenced owner accepted a Cron mutation");
    let cron_after_fence = successor_cron
        .get(schedule_id, None)
        .await
        .expect("successor Cron recheck");
    assert!(
        matches!(cron_after_fence.output, CronQueryResult::Get(Some(ref schedule)) if schedule.enabled)
    );
    let stale_workflow_id = b"stale-workflow".to_vec();
    let stale_workflow = source_workflow
        .start(
            identity(900_019, now_ms),
            stale_workflow_id.clone(),
            b"stale-result".to_vec(),
        )
        .await;
    assert!(
        stale_workflow.is_err(),
        "fenced owner accepted a Workflow start"
    );
    let workflow_after_fence = successor_workflow
        .state(stale_workflow_id, None)
        .await
        .expect("successor Workflow recheck");
    assert!(workflow_after_fence.output.is_none());
    source.shutdown().await.expect("source drain");
    successor.shutdown().await.expect("successor drain");
    assert_zero_reservations(&source);
    assert_zero_reservations(&successor);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_cell_node_primitive_takeover_preserves_acknowledged_values() {
    run_public_primitive_takeover(
        Store::new(Arc::new(InMemory::new())),
        Path::from("public-kv-takeover"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_cell_node_primitive_takeover_preserves_acknowledged_values() {
    let (store, root) = rustfs_public_store();
    run_public_primitive_takeover(store, root).await;
}
