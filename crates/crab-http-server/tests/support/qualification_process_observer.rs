use super::*;

pub(super) async fn run() {
    let (root, sync) = process_input();
    let case = process_case();
    let acknowledgement = if case == BEFORE_WRITE {
        None
    } else {
        let bytes = std::fs::read(sync.join("ack")).expect("owner acknowledgement");
        Some(serde_json::from_slice::<Acknowledgement>(&bytes).expect("decode acknowledgement"))
    };
    let (store, _) = rustfs_public_store();
    let application = Arc::new(fixture::compiled());
    let tenant = TenantId::from_bytes([71; 16]);
    let application_id = ApplicationId::from_bytes([72; 16]);
    let layout = CellStorageLayout::new(store.clone(), root, *application_id.as_bytes());
    let observer_session = SessionId::from_bytes([73; 16]);
    let observer = CellNodeBuilder::new(Arc::clone(&application))
        .with_runtime(
            SqlWorkerPool::new(2, 8).expect("observer pool"),
            32 * 1024 * 1024,
        )
        .with_replica_host(fixture::reference_host())
        .with_session(observer_session)
        .build()
        .expect("observer node");
    process_restore::install_process_lease(&observer);
    let directory = tempfile::tempdir().expect("empty observer directory");
    assert_eq!(
        std::fs::read_dir(directory.path())
            .expect("observer directory listing")
            .count(),
        0
    );
    let restored = process_restore::restore_cells(
        &observer,
        &layout,
        tenant,
        application_id,
        observer_session,
        None,
        directory.path(),
        "observer",
    )
    .await;
    let client =
        CellClient::local_many(application.registry(), restored).expect("observer direct client");
    let typed = observer
        .application_handle::<fixture::ReferenceApplication>(client, tenant, application_id)
        .with_blob_artifact_store(BlobArtifactStore::new(store));
    let sql = typed
        .sql::<fixture::ReferenceSql>(
            CellTarget::new(
                tenant,
                application_id,
                fixture::SQL_NAMESPACE,
                &partition_for_shard(0),
            )
            .expect("observer SQL target"),
        )
        .expect("observer SQL");
    let rows = sql
        .query(
            None,
            SqlBatch {
                statements: vec![
                    SqlStatement {
                        sql: "SELECT payload FROM qualification_rows WHERE id = 1".into(),
                        parameters: Vec::new(),
                    },
                    SqlStatement {
                        sql: "SELECT payload FROM qualification_rows WHERE id = 90".into(),
                        parameters: Vec::new(),
                    },
                    SqlStatement {
                        sql: "SELECT COUNT(*) FROM qualification_rows WHERE id = 90".into(),
                        parameters: Vec::new(),
                    },
                    SqlStatement {
                        sql: "SELECT schedule_id, generation, occurrence, scheduled_at_ms, payload FROM qualification_cron_invocations".into(),
                        parameters: Vec::new(),
                    },
                ],
            },
        )
        .await
        .expect("observer SQL rows");
    let kv = typed
        .kv::<fixture::ReferenceKv>(fixture::KV_NAMESPACE)
        .expect("observer KV")
        .get(KV_SCOPE.to_vec(), KV_KEY.to_vec(), None)
        .await
        .expect("observer KV value");
    let blob = typed
        .blob::<fixture::ReferenceBlob>()
        .expect("observer Blob")
        .query(
            BlobQuery::Read {
                key: BLOB_KEY.to_vec(),
                offset: 0,
                limit: 128,
            },
            None,
        )
        .await
        .expect("observer Blob value");
    let queue = typed
        .queue::<fixture::ReferenceQueue>()
        .expect("observer Queue");
    let queue_info = queue.info(0, None).await.expect("observer Queue counts");
    let cron = typed
        .cron::<fixture::ReferenceCron>()
        .expect("observer Cron");
    let schedule = cron
        .get(fixed_id(900_108), None)
        .await
        .expect("observer Cron schedule");
    let workflow = typed
        .workflow::<fixture::ReferenceWorkflow>()
        .expect("observer Workflow");
    let main_run = workflow
        .state(WORKFLOW_ID.to_vec(), None)
        .await
        .expect("observer main Workflow");
    let activity_run = workflow
        .state(ACTIVITY_WORKFLOW_ID.to_vec(), None)
        .await
        .expect("observer Activity Workflow");
    let effect_run = workflow
        .state(EFFECT_WORKFLOW_ID.to_vec(), None)
        .await
        .expect("observer Effect Workflow");

    if let Some(acknowledged) = &acknowledgement {
        assert_eq!(
            rows.output[0].rows,
            vec![vec![SqlValue::Blob(SQL_PAYLOAD.to_vec())]]
        );
        assert_eq!(
            rows.output[1].rows,
            vec![vec![SqlValue::Blob(EFFECT_RESULT.to_vec())]]
        );
        assert_eq!(rows.output[2].rows, vec![vec![SqlValue::Integer(1)]]);
        assert_eq!(
            rows.output[3].rows,
            vec![vec![
                SqlValue::Blob(fixed_id(900_108).to_vec()),
                SqlValue::Integer(
                    i64::try_from(acknowledged.cron_generation).expect("generation bounds")
                ),
                SqlValue::Integer(1),
                SqlValue::Integer(acknowledged.cron_next_due_ms),
                SqlValue::Blob(CRON_PAYLOAD.to_vec()),
            ]]
        );
        assert!(rows.receipt.commit_sequence >= acknowledged.sql_sequence);
        let entry = kv.output.expect("acknowledged KV value");
        assert_eq!(entry.value, KV_PAYLOAD);
        assert_eq!(entry.version.as_slice(), acknowledged.kv_version);
        assert!(kv.receipt.commit_sequence >= acknowledged.kv_sequence);
        let BlobQueryResult::Read(Some(read)) = blob.output else {
            panic!("acknowledged Blob disappeared");
        };
        assert_eq!(read.bytes, BLOB_PAYLOAD);
        assert_eq!(read.metadata.etag, acknowledged.blob_etag);
        assert_eq!(read.metadata.size, acknowledged.blob_size);
        assert!(blob.receipt.commit_sequence >= acknowledged.blob_sequence);
        assert_eq!(queue_info.output.acked, 1);
        assert_eq!(queue_info.output.ready, 0);
        assert_eq!(queue_info.output.leased, 0);
        assert!(queue_info.receipt.commit_sequence >= acknowledged.queue_sequence);
        let CronQueryResult::Get(Some(schedule)) = schedule.output else {
            panic!("acknowledged Cron schedule disappeared");
        };
        assert_eq!(schedule.generation, acknowledged.cron_generation);
        assert_eq!(schedule.occurrence, 1);
        assert_eq!(
            schedule.next_due_ms,
            acknowledged.cron_next_due_ms + 300_000
        );
        assert_eq!(
            main_run.output.expect("main Workflow").result,
            Some(WORKFLOW_RESULT.to_vec())
        );
        let activity = activity_run.output.expect("Activity Workflow");
        assert_eq!(activity.run_id, acknowledged.activity_run_id);
        assert_eq!(activity.status, WorkflowStatus::Completed);
        assert_eq!(activity.event_sequence, 2);
        assert!(activity.result.as_deref().is_some_and(|result| {
            result.starts_with(b"activity\0") && result.ends_with(b"activity-result")
        }));
        assert_eq!(
            effect_run.output.expect("Effect Workflow").result,
            Some(b"effect-valid-scheduled".to_vec())
        );
        assert!(main_run.receipt.commit_sequence >= acknowledged.workflow_sequence);
        assert!(activity_run.receipt.commit_sequence >= acknowledged.activity_sequence);
        assert!(effect_run.receipt.commit_sequence >= acknowledged.effect_sequence);
        assert!(
            queue
                .claim(
                    identity(900_145, now_ms()),
                    0,
                    QueueClaimRequest {
                        limit: 1,
                        lease_ms: 5_000,
                    },
                )
                .await
                .expect("observer Queue claim")
                .output
                .is_empty()
        );
        let effects = typed
            .effects::<fixture::ReferenceWorkflow>(
                CellTarget::new(
                    tenant,
                    application_id,
                    fixture::WORKFLOW_NAMESPACE,
                    &partition_for_shard(0),
                )
                .expect("observer Effect target"),
            )
            .expect("observer Effects");
        assert!(
            effects
                .claim(
                    identity(900_146, now_ms()),
                    EffectClaimRequest {
                        limit: 1,
                        lease_ms: 5_000,
                    },
                )
                .await
                .expect("observer Effect claim")
                .output
                .is_empty()
        );
    } else {
        assert!(rows.output[0].rows.is_empty());
        assert!(rows.output[1].rows.is_empty());
        assert_eq!(rows.output[2].rows, vec![vec![SqlValue::Integer(0)]]);
        assert!(rows.output[3].rows.is_empty());
        assert!(kv.output.is_none());
        assert!(matches!(blob.output, BlobQueryResult::Read(None)));
        assert_eq!(queue_info.output.acked, 0);
        assert_eq!(queue_info.output.ready, 0);
        assert_eq!(queue_info.output.leased, 0);
        assert!(matches!(schedule.output, CronQueryResult::Get(None)));
        assert!(main_run.output.is_none());
        assert!(activity_run.output.is_none());
        assert!(effect_run.output.is_none());
    }

    let workflow_target = CellTarget::new(
        tenant,
        application_id,
        fixture::WORKFLOW_NAMESPACE,
        &partition_for_shard(0),
    )
    .expect("observer Activity target");
    let pending_activities = typed
        .command::<WorkflowActivityClaimCommand<fixture::ReferenceWorkflow>>(
            &workflow_target,
            identity(900_147, now_ms()),
            WorkflowActivityClaimRequest {
                limit: 1,
                lease_ms: 5_000,
            },
        )
        .await
        .expect("observer Activity claim");
    assert!(pending_activities.output.is_empty());

    observer.shutdown().await.expect("observer drain");
    assert_zero_reservations(&observer);
    let observation = if acknowledgement.is_some() {
        let mut values = Vec::from(SQL_PAYLOAD);
        values.extend_from_slice(KV_PAYLOAD);
        values.extend_from_slice(BLOB_PAYLOAD);
        values.extend_from_slice(QUEUE_PAYLOAD);
        values.extend_from_slice(CRON_PAYLOAD);
        values.extend_from_slice(WORKFLOW_RESULT);
        values.extend_from_slice(b"activity-result");
        values.extend_from_slice(EFFECT_RESULT);
        values.extend_from_slice(CRON_PAYLOAD);
        values
    } else {
        b"absent".to_vec()
    };
    publish_marker(&sync, "independent-observation", &observation);
}
