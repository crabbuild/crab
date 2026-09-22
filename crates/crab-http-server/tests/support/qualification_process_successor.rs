use super::*;

pub(super) async fn run() {
    let (root, sync) = process_input();
    let case = process_case();
    let acknowledgement = if case == AFTER_ACK {
        let bytes = std::fs::read(sync.join("ack")).expect("owner acknowledgement");
        Some(serde_json::from_slice::<Acknowledgement>(&bytes).expect("decode acknowledgement"))
    } else {
        None
    };
    let (store, _) = rustfs_public_store();
    let application = Arc::new(fixture::compiled());
    let tenant = TenantId::from_bytes([71; 16]);
    let application_id = ApplicationId::from_bytes([72; 16]);
    let layout = CellStorageLayout::new(store.clone(), root, *application_id.as_bytes());
    let source_session = SessionId::from_bytes([24; 16]);
    let successor_session = SessionId::from_bytes([70; 16]);
    let fenced = fence_public_session(&layout, source_session, successor_session).await;
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
    let directory = tempfile::tempdir().expect("empty successor directory");
    assert_eq!(
        std::fs::read_dir(directory.path())
            .expect("successor directory listing")
            .count(),
        0
    );
    let authority = CellAuthority::new(layout.clone());
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
            .expect("successor target");
        let observed = authority
            .load(target.cell_id())
            .await
            .expect("source authority")
            .expect("source owner");
        let source_root = observed.value().root.clone().expect("acknowledged root");
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
                directory.path().join(format!("{module}-successor.sqlite")),
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
        assert_eq!(current.value().root.as_ref(), Some(&source_root));
        assert_eq!(
            current.value().owner.as_ref().map(|owner| owner.session),
            Some(successor_session)
        );
        restored_cells.push(restored);
    }
    let typed = successor
        .application_handle::<fixture::ReferenceApplication>(
            CellClient::local_many(application.registry(), restored_cells)
                .expect("successor typed client"),
            tenant,
            application_id,
        )
        .with_blob_artifact_store(BlobArtifactStore::new(store));
    let target = CellTarget::new(
        tenant,
        application_id,
        fixture::SQL_NAMESPACE,
        &partition_for_shard(0),
    )
    .expect("successor SQL target");
    let sql = typed
        .sql::<fixture::ReferenceSql>(target)
        .expect("successor SQL");
    let observed = sql
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
    if let Some(acknowledged) = &acknowledgement {
        assert_eq!(
            observed.output[0].rows,
            vec![vec![SqlValue::Blob(SQL_PAYLOAD.to_vec())]]
        );
        assert!(observed.receipt.commit_sequence >= acknowledged.sql_sequence);
    } else {
        assert!(
            observed.output[0].rows.is_empty(),
            "unacknowledged SQL row appeared"
        );
    }
    let kv = typed
        .kv::<fixture::ReferenceKv>(fixture::KV_NAMESPACE)
        .expect("successor KV");
    let observed = kv
        .get(KV_SCOPE.to_vec(), KV_KEY.to_vec(), None)
        .await
        .expect("successor KV read");
    if let Some(acknowledged) = &acknowledgement {
        let entry = observed.output.expect("acknowledged KV value");
        assert_eq!(entry.value, KV_PAYLOAD);
        assert_eq!(entry.version.as_slice(), acknowledged.kv_version);
        assert!(observed.receipt.commit_sequence >= acknowledged.kv_sequence);
    } else {
        assert!(
            observed.output.is_none(),
            "unacknowledged KV value appeared"
        );
    }
    let blob = typed
        .blob::<fixture::ReferenceBlob>()
        .expect("successor Blob");
    let observed = blob
        .query(
            BlobQuery::Read {
                key: BLOB_KEY.to_vec(),
                offset: 0,
                limit: 128,
            },
            None,
        )
        .await
        .expect("successor Blob read");
    if let Some(acknowledged) = &acknowledgement {
        let BlobQueryResult::Read(Some(read)) = observed.output else {
            panic!("acknowledged Blob is absent");
        };
        assert_eq!(read.bytes, BLOB_PAYLOAD);
        assert_eq!(read.metadata.etag, acknowledged.blob_etag);
        assert_eq!(read.metadata.size, acknowledged.blob_size);
        assert!(observed.receipt.commit_sequence >= acknowledged.blob_sequence);
    } else {
        assert!(
            matches!(observed.output, BlobQueryResult::Read(None)),
            "unacknowledged Blob appeared"
        );
    }
    let queue = typed
        .queue::<fixture::ReferenceQueue>()
        .expect("successor Queue");
    let info = queue.info(0, None).await.expect("successor Queue info");
    if let Some(acknowledged) = &acknowledgement {
        assert_eq!(info.output.ready, 1);
        assert_eq!(info.output.leased, 0);
        assert!(info.receipt.commit_sequence >= acknowledged.queue_sequence);
        let claimed = queue
            .claim(
                identity(900_106, now_ms()),
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
        assert_eq!(message.message_id, acknowledged.queue_message_id);
        assert_eq!(message.payload, QUEUE_PAYLOAD);
        assert_eq!(message.attempt, 1);
        let acked = queue
            .ack(
                identity(900_107, now_ms()),
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
        let settled = queue
            .info(0, Some(acked.receipt))
            .await
            .expect("successor Queue settlement");
        assert_eq!(settled.output.acked, 1);
        assert_eq!(settled.output.ready, 0);
        assert_eq!(settled.output.leased, 0);
    } else {
        assert_eq!(info.output.ready, 0);
        assert_eq!(info.output.leased, 0);
        assert_eq!(info.output.acked, 0);
    }
    let cron = typed
        .cron::<fixture::ReferenceCron>()
        .expect("successor Cron");
    let observed = cron
        .get(fixed_id(900_108), None)
        .await
        .expect("successor Cron read");
    if let Some(acknowledged) = &acknowledgement {
        let CronQueryResult::Get(Some(schedule)) = observed.output else {
            panic!("acknowledged Cron schedule is absent");
        };
        assert_eq!(schedule.schedule_id, fixed_id(900_108));
        assert_eq!(schedule.payload, CRON_PAYLOAD);
        assert_eq!(schedule.generation, acknowledged.cron_generation);
        assert_eq!(schedule.next_due_ms, acknowledged.cron_next_due_ms);
        assert_eq!(schedule.interval_ms, 60_000);
        assert_eq!(schedule.occurrence, 0);
        assert!(schedule.enabled);
        assert!(observed.receipt.commit_sequence >= acknowledged.cron_sequence);
    } else {
        assert!(
            matches!(observed.output, CronQueryResult::Get(None)),
            "unacknowledged Cron schedule appeared"
        );
    }
    let workflow = typed
        .workflow::<fixture::ReferenceWorkflow>()
        .expect("successor Workflow");
    let observed = workflow
        .state(WORKFLOW_ID.to_vec(), None)
        .await
        .expect("successor Workflow read");
    if let Some(acknowledged) = &acknowledgement {
        let run = observed.output.expect("acknowledged Workflow run");
        assert_eq!(run.workflow_id, WORKFLOW_ID);
        assert_eq!(run.run_id, acknowledged.workflow_run_id);
        assert_eq!(run.status, WorkflowStatus::Completed);
        assert_eq!(run.result.as_deref(), Some(WORKFLOW_RESULT));
        assert_eq!(run.event_sequence, acknowledged.workflow_event_sequence);
        assert!(observed.receipt.commit_sequence >= acknowledged.workflow_sequence);
    } else {
        assert!(
            observed.output.is_none(),
            "unacknowledged Workflow appeared"
        );
    }
    let activity_state = workflow
        .state(ACTIVITY_WORKFLOW_ID.to_vec(), None)
        .await
        .expect("successor Activity workflow read");
    if let Some(acknowledged) = &acknowledgement {
        let run = activity_state
            .output
            .expect("acknowledged Activity workflow");
        assert_eq!(run.run_id, acknowledged.activity_run_id);
        assert_eq!(run.status, WorkflowStatus::Running);
        assert!(activity_state.receipt.commit_sequence >= acknowledged.activity_sequence);
    } else {
        assert!(
            activity_state.output.is_none(),
            "unacknowledged Activity workflow appeared"
        );
    }
    let activity = ActivitySupervisor::new(
        typed
            .activities::<fixture::ReferenceWorkflow>()
            .expect("successor typed Activity"),
        5_000,
    )
    .expect("successor Activity supervisor");
    let activity_outcome = activity
        .run_once(0, None)
        .await
        .expect("successor Activity run");
    if let Some(acknowledged) = &acknowledgement {
        assert!(matches!(
            activity_outcome,
            ActivityRunOutcome::Completed { .. }
        ));
        let settled = workflow
            .state(ACTIVITY_WORKFLOW_ID.to_vec(), None)
            .await
            .expect("successor Activity settlement");
        let run = settled.output.expect("completed Activity workflow");
        assert_eq!(run.run_id, acknowledged.activity_run_id);
        assert_eq!(run.status, WorkflowStatus::Completed);
        assert!(run.result.as_deref().is_some_and(
            |result| result.starts_with(b"activity\0") && result.ends_with(b"activity-result")
        ));
        assert!(matches!(
            activity
                .run_once(0, None)
                .await
                .expect("successor Activity settled check"),
            ActivityRunOutcome::Idle { .. }
        ));
    } else {
        assert!(matches!(activity_outcome, ActivityRunOutcome::Idle { .. }));
    }
    let effect_state = workflow
        .state(EFFECT_WORKFLOW_ID.to_vec(), None)
        .await
        .expect("successor Effect workflow read");
    if let Some(acknowledged) = &acknowledgement {
        let run = effect_state.output.expect("acknowledged Effect workflow");
        assert_eq!(run.run_id, acknowledged.effect_run_id);
        assert_eq!(run.status, WorkflowStatus::Completed);
        assert_eq!(run.result.as_deref(), Some(b"effect-scheduled".as_slice()));
        assert!(effect_state.receipt.commit_sequence >= acknowledged.effect_sequence);
    } else {
        assert!(
            effect_state.output.is_none(),
            "unacknowledged Effect workflow appeared"
        );
    }
    let effect_target = CellTarget::new(
        tenant,
        application_id,
        fixture::WORKFLOW_NAMESPACE,
        &partition_for_shard(0),
    )
    .expect("successor Effect target");
    let effects = typed
        .effects::<fixture::ReferenceWorkflow>(effect_target)
        .expect("successor Effects");
    let claimed = effects
        .claim(
            identity(900_112, now_ms()),
            EffectClaimRequest {
                limit: 1,
                lease_ms: 5_000,
            },
        )
        .await
        .expect("successor Effect claim");
    if acknowledgement.is_some() {
        assert_eq!(claimed.output.len(), 1);
        let effect = claimed.output[0].clone();
        assert!(
            effects
                .validate(vec![effect.clone()], claimed.receipt)
                .await
                .expect("successor Effect validation")
                .output
        );
        let acked = effects
            .ack(
                identity(900_113, now_ms()),
                effect.clone(),
                EFFECT_RESULT.to_vec(),
            )
            .await
            .expect("successor Effect ack");
        assert_eq!(acked.output, EffectLeaseOutcome::Delivered);
        assert!(
            !effects
                .validate(vec![effect], acked.receipt)
                .await
                .expect("successor Effect settlement")
                .output
        );
        assert!(
            effects
                .claim(
                    identity(900_114, now_ms()),
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
    } else {
        assert!(claimed.output.is_empty(), "unacknowledged Effect appeared");
    }
    successor.shutdown().await.expect("successor drain");
    assert_zero_reservations(&successor);
    let observation = if acknowledgement.is_some() {
        let mut values = Vec::from(SQL_PAYLOAD);
        values.extend_from_slice(KV_PAYLOAD);
        values.extend_from_slice(BLOB_PAYLOAD);
        values.extend_from_slice(QUEUE_PAYLOAD);
        values.extend_from_slice(CRON_PAYLOAD);
        values.extend_from_slice(WORKFLOW_RESULT);
        values.extend_from_slice(b"activity-result");
        values.extend_from_slice(EFFECT_RESULT);
        values
    } else {
        b"absent".to_vec()
    };
    publish_marker(&sync, "observation", &observation);
}
