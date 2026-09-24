//! Every reference primitive driven through one local router.

use crate::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typed_application_executes_every_primitive_through_a_local_router() {
    let application = Arc::new(compiled());
    let tenant = TenantId::from_bytes([31; 16]);
    let application_id = ApplicationId::from_bytes([32; 16]);
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = CellStorageLayout::new(
        store.clone(),
        object_store::path::Path::from("reference-primitive-qualification"),
        *application_id.as_bytes(),
    );
    let directory = tempfile::TempDir::new().unwrap();
    let session = crab_cell_runtime::SessionId::from_bytes([24; 16]);
    let runtime = CellRuntime::new_with_replica_host(
        SqlWorkerPool::new(4, 32).unwrap(),
        64 * 1024 * 1024,
        session,
        reference_host(),
    )
    .unwrap();
    let registry = application.registry();
    let handles = vec![
        bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            session,
            SQL_NAMESPACE,
            CatalogRole::Sql,
            SQL_MODULE,
            40,
            |_| Ok(()),
        )
        .await
        .unwrap(),
        bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            session,
            KV_NAMESPACE,
            CatalogRole::Kv,
            KV_MODULE,
            41,
            install_kv_schema,
        )
        .await
        .unwrap(),
        bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            session,
            BLOB_NAMESPACE,
            CatalogRole::Blob,
            BLOB_MODULE,
            42,
            install_blob_schema,
        )
        .await
        .unwrap(),
        bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            session,
            QUEUE_NAMESPACE,
            CatalogRole::Queue,
            QUEUE_MODULE,
            43,
            install_queue_schema,
        )
        .await
        .unwrap(),
        bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            session,
            DEAD_LETTER_NAMESPACE,
            CatalogRole::Queue,
            DEAD_LETTER_MODULE,
            44,
            install_queue_schema,
        )
        .await
        .unwrap(),
        bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            session,
            CRON_NAMESPACE,
            CatalogRole::Cron,
            CRON_MODULE,
            45,
            install_cron_schema,
        )
        .await
        .unwrap(),
        bootstrap_reference_cell(
            &runtime,
            &registry,
            &layout,
            &directory,
            tenant,
            application_id,
            session,
            WORKFLOW_NAMESPACE,
            CatalogRole::Workflow,
            WORKFLOW_MODULE,
            46,
            install_workflow_schema,
        )
        .await
        .unwrap(),
    ];
    let client = CellClient::local_many(registry.clone(), handles).unwrap();
    let typed = ApplicationHandle::<ReferenceApplication>::new(
        client,
        Arc::clone(&application),
        tenant,
        application_id,
    )
    .with_blob_artifact_store(BlobArtifactStore::new(store.clone()));
    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();

    let sql_target = CellTarget::new(
        tenant,
        application_id,
        SQL_NAMESPACE,
        &partition_for_shard(0),
    )
    .unwrap();
    let queue_target = CellTarget::new(
        tenant,
        application_id,
        QUEUE_NAMESPACE,
        &partition_for_shard(0),
    )
    .unwrap();
    let wrong_command = typed
        .command::<KvAtomicCommand<ReferenceKv>>(
            &sql_target,
            reference_identity(49, now_ms),
            KvAtomicRequest {
                scope: b"wrong-module".to_vec(),
                checks: Vec::new(),
                mutations: Vec::new(),
            },
        )
        .await;
    assert!(matches!(
        wrong_command,
        Err(InvocationError::NotStarted(Error::Registry(
            "namespace module differs from capability"
        )))
    ));
    let wrong_prepared = typed
        .prepare_command::<KvAtomicCommand<ReferenceKv>>(
            &sql_target,
            reference_identity(50, now_ms),
            KvAtomicRequest {
                scope: b"wrong-module".to_vec(),
                checks: Vec::new(),
                mutations: Vec::new(),
            },
        )
        .await;
    assert!(matches!(
        wrong_prepared,
        Err(InvocationError::NotStarted(Error::Registry(
            "namespace module differs from capability"
        )))
    ));
    let wrong_query = typed
        .query::<KvGetQuery<ReferenceKv>>(
            &sql_target,
            None,
            KvGetRequest {
                scope: b"wrong-module".to_vec(),
                key: b"key".to_vec(),
            },
        )
        .await;
    assert!(matches!(
        wrong_query,
        Err(InvocationError::NotStarted(Error::Registry(
            "namespace module differs from capability"
        )))
    ));
    assert!(typed.sql::<ReferenceSql>(queue_target).is_err());
    assert!(typed.kv::<ReferenceKv>(SQL_NAMESPACE).is_err());
    assert!(
        typed
            .effects::<ReferenceWorkflow>(sql_target.clone())
            .is_err()
    );
    let foreign_target = CellTarget::new(
        TenantId::from_bytes([99; 16]),
        application_id,
        SQL_NAMESPACE,
        &partition_for_shard(0),
    )
    .unwrap();
    assert!(typed.sql::<ReferenceSql>(foreign_target).is_err());
    let invalid_partition = CellTarget::new(
        tenant,
        application_id,
        SQL_NAMESPACE,
        &partition_for_shard(1),
    )
    .unwrap();
    assert!(typed.sql::<ReferenceSql>(invalid_partition).is_err());
    let kv_target = CellTarget::new(
        tenant,
        application_id,
        KV_NAMESPACE,
        &partition_for_shard(0),
    )
    .unwrap();
    assert!(typed.effects::<UnregisteredEffects>(kv_target).is_err());
    let sql = typed.sql::<ReferenceSql>(sql_target.clone()).unwrap();
    let sql_result = sql
        .batch(
            reference_identity(50, now_ms),
            SqlBatch {
                statements: vec![SqlStatement {
                    sql: "SELECT ?1".into(),
                    parameters: vec![SqlValue::Integer(11)],
                }],
            },
        )
        .await
        .unwrap();
    assert_eq!(sql_result.output[0].rows.len(), 1);

    let kv = typed.kv::<ReferenceKv>(KV_NAMESPACE).unwrap();
    kv.atomic(
        reference_identity(51, now_ms),
        KvAtomicRequest {
            scope: b"qualification".to_vec(),
            checks: Vec::new(),
            mutations: vec![KvMutation::Put {
                key: b"key".to_vec(),
                value: b"value".to_vec(),
                expires_at_ms: None,
            }],
        },
    )
    .await
    .unwrap();
    let kv_value = kv
        .get(b"qualification".to_vec(), b"key".to_vec(), None)
        .await
        .unwrap();
    assert_eq!(kv_value.output.unwrap().value, b"value");

    let blob = typed.blob::<ReferenceBlob>().unwrap();
    let blob_key = b"qualification/blob".to_vec();
    let upload_id = [52; 16];
    blob.mutate(
        reference_identity(52, now_ms),
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
    .unwrap();
    blob.mutate(
        reference_identity(53, now_ms),
        BlobMutation::PutPart {
            key: blob_key.clone(),
            upload_id,
            part_number: 1,
            payload: b"blob-value".to_vec(),
        },
    )
    .await
    .unwrap();
    let completed = blob
        .mutate(
            reference_identity(54, now_ms),
            BlobMutation::Complete {
                key: blob_key.clone(),
                upload_id,
                part_count: 1,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        completed.output,
        BlobMutationOutcome::Committed { .. }
    ));
    let blob_read = blob
        .query(
            BlobQuery::Read {
                key: blob_key,
                offset: 0,
                limit: 128,
            },
            None,
        )
        .await
        .unwrap();
    assert!(matches!(blob_read.output, BlobQueryResult::Read(Some(_))));

    let queue = typed.queue::<ReferenceQueue>().unwrap();
    let sent = queue
        .send(
            reference_identity(55, now_ms),
            QueueSendRequest {
                producer_id: [55; 16],
                payload: b"queue-value".to_vec(),
                available_at_ms: now_ms,
            },
        )
        .await
        .unwrap();
    let claimed = queue
        .claim(
            reference_identity(56, now_ms),
            0,
            QueueClaimRequest {
                limit: 1,
                lease_ms: 5_000,
            },
        )
        .await
        .unwrap();
    assert_eq!(claimed.output.len(), 1);
    assert!(
        queue
            .validate_claim(0, claimed.output.clone(), Some(claimed.receipt))
            .await
            .unwrap()
            .output
    );
    let message = &claimed.output[0];
    let acked = queue
        .ack(
            reference_identity(57, now_ms),
            0,
            message.message_id,
            message.token,
        )
        .await
        .unwrap();
    assert!(matches!(acked.output, QueueLeaseOutcome::Applied { .. }));
    assert!(matches!(
        sent.output,
        crab_cell_runtime::primitives::queue::QueueSendOutcome::Sent { .. }
    ));

    let cron = typed.cron::<ReferenceCron>().unwrap();
    let schedule_id = [58; 16];
    cron.mutate(
        reference_identity(58, now_ms),
        CronMutation::Upsert {
            schedule_id,
            target_index: 0,
            target_partition: partition_for_shard(0).to_vec(),
            payload: b"cron-value".to_vec(),
            interval_ms: 1_000,
            next_due_ms: now_ms + 1_000,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        cron.get(schedule_id, None).await.unwrap().output,
        CronQueryResult::Get(Some(_))
    ));

    let workflow = typed.workflow::<ReferenceWorkflow>().unwrap();
    let activity_workflow_id = b"activity-run".to_vec();
    let activity_started = workflow
        .start(
            reference_identity(59, now_ms),
            activity_workflow_id.clone(),
            b"activity".to_vec(),
        )
        .await
        .unwrap();
    let activity_run_id = match activity_started.output {
        crab_cell_runtime::primitives::workflow::WorkflowOutcome::Applied { run_id, .. } => run_id,
        outcome => panic!("unexpected activity start outcome: {outcome:?}"),
    };
    let activity = crab_cell_runtime::primitives::workflow::ActivitySupervisor::new(
        typed.activities::<ReferenceWorkflow>().unwrap(),
        5_000,
    )
    .unwrap();
    assert!(matches!(
        activity.run_once(0, None).await.unwrap(),
        ActivityRunOutcome::Completed { .. }
    ));
    let activity_state = workflow
        .state(activity_workflow_id, None)
        .await
        .unwrap()
        .output
        .unwrap();
    assert_eq!(activity_state.run_id, activity_run_id);
    assert_eq!(activity_state.status, WorkflowStatus::Completed);

    let effect_workflow_id = b"effect-run".to_vec();
    workflow
        .start(
            reference_identity(60, now_ms),
            effect_workflow_id,
            b"effect".to_vec(),
        )
        .await
        .unwrap();
    let effects = typed
        .effects::<ReferenceWorkflow>(
            CellTarget::new(
                tenant,
                application_id,
                WORKFLOW_NAMESPACE,
                &partition_for_shard(0),
            )
            .unwrap(),
        )
        .unwrap();
    let claims = effects
        .claim(
            reference_identity(61, now_ms),
            EffectClaimRequest {
                limit: 1,
                lease_ms: 5_000,
            },
        )
        .await
        .unwrap();
    assert_eq!(claims.output.len(), 1);
    assert!(
        effects
            .validate(claims.output.clone(), claims.receipt)
            .await
            .unwrap()
            .output
    );
    let effect = &claims.output[0];
    let acknowledged = effects
        .ack(
            reference_identity(62, now_ms),
            effect.clone(),
            b"effect-result".to_vec(),
        )
        .await
        .unwrap();
    assert_eq!(acknowledged.output, EffectLeaseOutcome::Delivered);

    let profile = QualificationProfile::new("typed-smoke".into(), 1, 1, 1, 5_000).unwrap();
    let workload = QualificationWorkload::generate_with_size(&profile, 91, 1, 16, 1).unwrap();
    let mut executor = TypedQualificationExecutor {
        handle: typed.clone(),
        tenant,
        application: application_id,
        now_ms,
    };
    let summary = workload.run(&mut executor).await.unwrap();
    assert_eq!(summary.operations(), 16);
    assert!(
        summary
            .primitive_counts()
            .iter()
            .all(|counts| counts.attempted() > 0 && counts.verified() > 0)
    );
    assert!(
        summary
            .metrics()
            .unwrap()
            .iter()
            .any(|metric| { metric.name() == "p99_latency_ms" && metric.unit() == "ms" })
    );

    // Drop the first owner and its local SQLite sources. The next owner must
    // recover every declared namespace from the published roots before the
    // same typed application handle is allowed to continue.
    drop(activity);
    drop(effects);
    drop(workflow);
    drop(cron);
    drop(queue);
    drop(blob);
    drop(kv);
    drop(sql);
    drop(typed);
    // The workload executor owns the last cloned local transport. Release it
    // before deleting the source files so the crashed owner cannot continue a
    // background compaction against the torn-down SQLite paths.
    drop(executor);
    drop(runtime);
    drop(directory);

    let restored_directory = tempfile::TempDir::new().unwrap();
    let takeover_session = crab_cell_runtime::SessionId::from_bytes([70; 16]);
    let takeover_runtime = CellRuntime::new_with_replica_host(
        SqlWorkerPool::new(4, 32).unwrap(),
        64 * 1024 * 1024,
        takeover_session,
        reference_host(),
    )
    .unwrap();
    let authority = CellAuthority::new(layout.clone());
    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(layout.clone(), tenant);
    seed_reference_session(&layout, session).await;
    let mut restored_handles = Vec::new();
    for (namespace, module, incarnation_byte) in [
        (SQL_NAMESPACE, SQL_MODULE, 40_u8),
        (KV_NAMESPACE, KV_MODULE, 41_u8),
        (BLOB_NAMESPACE, BLOB_MODULE, 42_u8),
        (QUEUE_NAMESPACE, QUEUE_MODULE, 43_u8),
        (DEAD_LETTER_NAMESPACE, DEAD_LETTER_MODULE, 44_u8),
        (CRON_NAMESPACE, CRON_MODULE, 45_u8),
        (WORKFLOW_NAMESPACE, WORKFLOW_MODULE, 46_u8),
    ] {
        let target =
            CellTarget::new(tenant, application_id, namespace, &partition_for_shard(0)).unwrap();
        let observed = authority.load(target.cell_id()).await.unwrap().unwrap();
        let restored = takeover_runtime
            .takeover_restored(
                catalog.lookup(target.cell_id()).await.unwrap().unwrap(),
                CellReplica::new(
                    layout.clone(),
                    *target.cell_id().as_bytes(),
                    *IncarnationId::from_bytes([incarnation_byte; 16]).as_bytes(),
                    Limits::default(),
                )
                .unwrap(),
                authority.clone(),
                observed,
                fence_reference_session(&layout, session, takeover_session)
                    .await
                    .direct_takeover()
                    .unwrap(),
                crab_cell_runtime::recovery::manifest::RecoveryManifestStore::new(
                    layout.clone(),
                    Limits::default(),
                ),
                restored_directory
                    .path()
                    .join(format!("{module}-takeover.sqlite")),
                Owner {
                    session: takeover_session,
                    endpoint: "https://reference-takeover.internal:8081".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(restored.cell_id(), target.cell_id());
        restored_handles.push(restored);
    }
    let restored_client = CellClient::local_many(registry.clone(), restored_handles).unwrap();
    let restored = ApplicationHandle::<ReferenceApplication>::new(
        restored_client,
        application,
        tenant,
        application_id,
    )
    .with_blob_artifact_store(BlobArtifactStore::new(store));
    restored
        .sql::<ReferenceSql>(sql_target.clone())
        .unwrap()
        .query(
            None,
            SqlBatch {
                statements: vec![SqlStatement {
                    sql: "SELECT 11".into(),
                    parameters: Vec::new(),
                }],
            },
        )
        .await
        .unwrap();
    restored
        .kv::<ReferenceKv>(KV_NAMESPACE)
        .unwrap()
        .atomic(
            reference_identity(100, now_ms),
            KvAtomicRequest {
                scope: b"qualification".to_vec(),
                checks: Vec::new(),
                mutations: vec![KvMutation::Put {
                    key: b"after-recovery".to_vec(),
                    value: b"ok".to_vec(),
                    expires_at_ms: None,
                }],
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        restored
            .blob::<ReferenceBlob>()
            .unwrap()
            .query(
                BlobQuery::Read {
                    key: b"qualification/blob".to_vec(),
                    offset: 0,
                    limit: 128,
                },
                None,
            )
            .await
            .unwrap()
            .output,
        BlobQueryResult::Read(Some(_))
    ));
    let restored_queue = restored.queue::<ReferenceQueue>().unwrap();
    restored_queue
        .send(
            reference_identity(101, now_ms),
            QueueSendRequest {
                producer_id: [101; 16],
                payload: b"after-recovery".to_vec(),
                available_at_ms: now_ms,
            },
        )
        .await
        .unwrap();
    assert!(restored_queue.info(0, None).await.unwrap().output.ready >= 1);
    restored
        .cron::<ReferenceCron>()
        .unwrap()
        .get([58; 16], None)
        .await
        .unwrap();
    let restored_workflow = restored.workflow::<ReferenceWorkflow>().unwrap();
    let activity_workflow_id = b"activity-after-recovery".to_vec();
    restored_workflow
        .start(
            reference_identity(102, now_ms),
            activity_workflow_id.clone(),
            b"activity".to_vec(),
        )
        .await
        .unwrap();
    let restored_activity = crab_cell_runtime::primitives::workflow::ActivitySupervisor::new(
        restored.activities::<ReferenceWorkflow>().unwrap(),
        5_000,
    )
    .unwrap();
    assert!(matches!(
        restored_activity.run_once(0, None).await.unwrap(),
        ActivityRunOutcome::Completed { .. }
    ));
    assert_eq!(
        restored_workflow
            .state(activity_workflow_id, None)
            .await
            .unwrap()
            .output
            .unwrap()
            .status,
        WorkflowStatus::Completed
    );
    restored_workflow
        .start(
            reference_identity(103, now_ms),
            b"effect-after-recovery".to_vec(),
            b"effect".to_vec(),
        )
        .await
        .unwrap();
    let restored_effects = restored
        .effects::<ReferenceWorkflow>(
            CellTarget::new(
                tenant,
                application_id,
                WORKFLOW_NAMESPACE,
                &partition_for_shard(0),
            )
            .unwrap(),
        )
        .unwrap();
    let restored_claims = restored_effects
        .claim(
            reference_identity(104, now_ms),
            EffectClaimRequest {
                limit: 1,
                lease_ms: 5_000,
            },
        )
        .await
        .unwrap();
    assert_eq!(restored_claims.output.len(), 1);
    restored_effects
        .ack(
            reference_identity(105, now_ms),
            restored_claims.output[0].clone(),
            b"recovered-effect".to_vec(),
        )
        .await
        .unwrap();
    takeover_runtime.shutdown().await.unwrap();
}
