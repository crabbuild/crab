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
    let (store, _) = process_store();
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
                    SqlStatement {
                        sql: "SELECT payload FROM qualification_rows WHERE id = ?1".into(),
                        parameters: vec![SqlValue::Integer(
                            i64::try_from(SQL_EXPIRY_OPERATION_ID).expect("expiry row ID bounds"),
                        )],
                    },
                    SqlStatement {
                        sql: "SELECT payload FROM qualification_rows WHERE id = ?1".into(),
                        parameters: vec![SqlValue::Integer(
                            i64::try_from(SQL_EXPIRY_OPERATION_ID + 1)
                                .expect("delayed expiry row ID bounds"),
                        )],
                    },
                ],
            },
        )
        .await
        .expect("observer SQL rows");
    let kv_capability = typed
        .kv::<fixture::ReferenceKv>(fixture::KV_NAMESPACE)
        .expect("observer KV");
    let kv = kv_capability
        .get(KV_SCOPE.to_vec(), KV_KEY.to_vec(), None)
        .await
        .expect("observer KV value");
    for key_id in [KV_EXPIRY_OPERATION_ID, KV_EXPIRY_OPERATION_ID + 1] {
        let expired = kv_capability
            .get(KV_EXPIRY_SCOPE.to_vec(), fixed_id(key_id).to_vec(), None)
            .await
            .expect("observer KV expiry read");
        assert!(expired.output.is_none(), "expired KV key appeared");
        if let Some(sequence) = acknowledgement
            .as_ref()
            .and_then(|ack| ack.expiry_kv_sequence)
        {
            assert!(expired.receipt.commit_sequence >= sequence);
        }
    }
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
    let expired_pause = cron
        .get(fixed_id(CRON_EXPIRY_OPERATION_ID), None)
        .await
        .expect("observer Cron expiry schedule");
    if let Some(evidence) = acknowledgement
        .as_ref()
        .and_then(|ack| ack.expiry_cron.as_ref())
    {
        let CronQueryResult::Get(Some(schedule)) = expired_pause.output else {
            panic!("acknowledged Cron expiry schedule disappeared");
        };
        assert_eq!(schedule.schedule_id, fixed_id(CRON_EXPIRY_OPERATION_ID));
        assert_eq!(schedule.payload, CRON_EXPIRY_NONCE.to_be_bytes());
        assert_eq!(schedule.generation, evidence.generation);
        assert_eq!(schedule.next_due_ms, evidence.next_due_ms);
        assert_eq!(schedule.interval_ms, 60_000);
        assert_eq!(schedule.occurrence, 0);
        assert!(schedule.enabled, "expired pause disabled the schedule");
        assert!(expired_pause.receipt.commit_sequence >= evidence.sequence);
    } else {
        assert!(matches!(expired_pause.output, CronQueryResult::Get(None)));
    }
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
    let expiry_id = format!("public-workflow-expiry-{WORKFLOW_EXPIRY_OPERATION_ID}").into_bytes();
    let rejected_id =
        format!("public-workflow-expiry-rejected-{WORKFLOW_EXPIRY_OPERATION_ID}").into_bytes();
    let expiry_run = workflow
        .state(expiry_id.clone(), None)
        .await
        .expect("observer Workflow expiry run");
    if let Some(evidence) = acknowledgement
        .as_ref()
        .and_then(|ack| ack.expiry_workflow.as_ref())
    {
        let run = expiry_run.output.expect("acknowledged Workflow expiry run");
        assert_eq!(run.workflow_id, expiry_id);
        assert_eq!(run.run_id, evidence.run_id);
        assert_eq!(run.definition_digest, fixture::WORKFLOW_DIGEST);
        assert_eq!(run.status, WorkflowStatus::Completed);
        assert_eq!(run.event_sequence, 1);
        assert_eq!(
            run.result,
            Some(WORKFLOW_EXPIRY_NONCE.to_be_bytes().to_vec())
        );
        assert!(expiry_run.receipt.commit_sequence >= evidence.sequence);
    } else {
        assert!(expiry_run.output.is_none());
    }
    assert!(
        workflow
            .state(rejected_id, None)
            .await
            .expect("observer rejected Workflow expiry run")
            .output
            .is_none()
    );

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
        if let Some(sequence) = acknowledged.expiry_sql_sequence {
            assert_eq!(
                rows.output[4].rows,
                vec![vec![SqlValue::Blob(
                    SQL_EXPIRY_NONCE.to_be_bytes().to_vec()
                )]]
            );
            assert!(rows.receipt.commit_sequence >= sequence);
        } else {
            assert!(rows.output[4].rows.is_empty());
        }
        assert!(rows.output[5].rows.is_empty());
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
        // Only the lease and settlement fault boundaries retain an Effect ID.
        // The restored third process must still see the successor's terminal result.
        let expected_source = acknowledged
            .settlements
            .as_ref()
            .map(|settled| {
                (
                    settled.effect_id,
                    settled.effect_attempt,
                    settled.effect_expires_at_ms,
                )
            })
            .or_else(|| {
                acknowledged.leases.as_ref().map(|leased| {
                    (
                        leased.effect_id,
                        leased.effect_attempt + 1,
                        leased.effect_expires_at_ms,
                    )
                })
            });
        if let Some((effect_id, attempt, expires_at_ms)) = expected_source {
            let mut encoded = BoundedEncoder::new(1 << 20).expect("Effect result bound");
            vec![SqlResultSet {
                columns: Vec::new(),
                rows: Vec::new(),
                rows_affected: 1,
            }]
            .encode(&mut encoded)
            .expect("expected SQL Effect result");
            let result = encoded.finish();
            let observed = effects
                .status(effect_id, None)
                .await
                .expect("independent restored Effect source status");
            assert!(observed.receipt.commit_sequence >= acknowledged.effect_sequence);
            assert_eq!(
                observed.output,
                Some(EffectStatus {
                    state: EffectState::Delivered,
                    attempt,
                    token_present: false,
                    lease_until_ms: None,
                    expires_at_ms,
                    result: Some(result.clone()),
                })
            );
            if let Some(settled) = &acknowledged.settlements {
                assert_eq!(settled.effect_result, result);
                assert!(observed.receipt.commit_sequence >= settled.effect_ack_sequence);
            }
        }
    } else {
        assert!(rows.output[0].rows.is_empty());
        assert!(rows.output[1].rows.is_empty());
        assert_eq!(rows.output[2].rows, vec![vec![SqlValue::Integer(0)]]);
        assert!(rows.output[3].rows.is_empty());
        assert!(rows.output[4].rows.is_empty());
        assert!(rows.output[5].rows.is_empty());
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
        if acknowledgement
            .as_ref()
            .and_then(|acknowledged| acknowledged.expiry_sql_sequence)
            .is_some()
        {
            values.extend_from_slice(&SQL_EXPIRY_NONCE.to_be_bytes());
        }
        if acknowledgement
            .as_ref()
            .and_then(|acknowledged| acknowledged.expiry_kv_sequence)
            .is_some()
        {
            values.extend_from_slice(&KV_EXPIRY_NONCE.to_be_bytes());
        }
        if acknowledgement
            .as_ref()
            .and_then(|acknowledged| acknowledged.expiry_cron.as_ref())
            .is_some()
        {
            values.extend_from_slice(&CRON_EXPIRY_NONCE.to_be_bytes());
        }
        if acknowledgement
            .as_ref()
            .and_then(|acknowledged| acknowledged.expiry_workflow.as_ref())
            .is_some()
        {
            values.extend_from_slice(&WORKFLOW_EXPIRY_NONCE.to_be_bytes());
        }
        values
    } else {
        b"absent".to_vec()
    };
    publish_marker(&sync, "independent-observation", &observation);
}
