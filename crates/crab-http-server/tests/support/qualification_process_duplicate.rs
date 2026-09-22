use super::*;

pub(super) async fn verify(
    typed: &crab_cell_app::ApplicationHandle<fixture::ReferenceApplication>,
    tenant: TenantId,
    application: ApplicationId,
    acknowledged: &Acknowledgement,
) {
    let issued_at_ms = acknowledged.replay_issued_at_ms;
    assert!(now_ms() < issued_at_ms + 300_000, "replay identity expired");

    let sql_target = CellTarget::new(
        tenant,
        application,
        fixture::SQL_NAMESPACE,
        &partition_for_shard(0),
    )
    .expect("replay SQL target");
    let sql = typed
        .sql::<fixture::ReferenceSql>(sql_target)
        .expect("replay SQL");
    let repeated = sql
        .batch(
            replay_identity(900_100, issued_at_ms),
            SqlBatch {
                statements: vec![SqlStatement {
                    sql: "INSERT INTO qualification_rows (id, payload) VALUES (1, ?1)".into(),
                    parameters: vec![SqlValue::Blob(SQL_PAYLOAD.to_vec())],
                }],
            },
        )
        .await
        .expect("recovered SQL identity replay");
    assert_eq!(repeated.receipt.commit_sequence, acknowledged.sql_sequence);
    assert_eq!(repeated.output[0].rows_affected, 1);
    let counted = sql
        .query(
            Some(repeated.receipt),
            SqlBatch {
                statements: vec![SqlStatement {
                    sql: "SELECT COUNT(*) FROM qualification_rows WHERE id = 1".into(),
                    parameters: Vec::new(),
                }],
            },
        )
        .await
        .expect("recovered SQL count");
    assert_eq!(counted.output[0].rows, vec![vec![SqlValue::Integer(1)]]);

    let kv = typed
        .kv::<fixture::ReferenceKv>(fixture::KV_NAMESPACE)
        .expect("replay KV");
    let repeated = kv
        .atomic(
            replay_identity(900_101, issued_at_ms),
            KvAtomicRequest {
                scope: KV_SCOPE.to_vec(),
                checks: Vec::new(),
                mutations: vec![KvMutation::Put {
                    key: KV_KEY.to_vec(),
                    value: KV_PAYLOAD.to_vec(),
                    expires_at_ms: None,
                }],
            },
        )
        .await
        .expect("recovered KV identity replay");
    assert_eq!(repeated.receipt.commit_sequence, acknowledged.kv_sequence);
    let KvAtomicOutcome::Applied(results) = repeated.output else {
        panic!("recovered KV replay was not applied");
    };
    assert_eq!(
        results[0]
            .version
            .as_ref()
            .map(|version| version.as_slice()),
        Some(acknowledged.kv_version.as_slice())
    );

    let blob = typed.blob::<fixture::ReferenceBlob>().expect("replay Blob");
    let repeated = blob
        .mutate(
            replay_identity(900_104, issued_at_ms),
            BlobMutation::Complete {
                key: BLOB_KEY.to_vec(),
                upload_id: [103; 16],
                part_count: 1,
            },
        )
        .await
        .expect("recovered Blob completion replay");
    assert_eq!(repeated.receipt.commit_sequence, acknowledged.blob_sequence);
    assert_eq!(
        repeated.output,
        BlobMutationOutcome::Committed {
            etag: acknowledged.blob_etag,
            size: acknowledged.blob_size,
        }
    );

    let queue = typed
        .queue::<fixture::ReferenceQueue>()
        .expect("replay Queue");
    let repeated = queue
        .send(
            replay_identity(900_105, issued_at_ms),
            QueueSendRequest {
                producer_id: fixed_id(900_105),
                payload: QUEUE_PAYLOAD.to_vec(),
                available_at_ms: acknowledged.queue_available_at_ms,
            },
        )
        .await
        .expect("recovered Queue send replay");
    assert_eq!(
        repeated.receipt.commit_sequence,
        acknowledged.queue_sequence
    );
    assert_eq!(
        repeated.output,
        QueueSendOutcome::Sent {
            message_id: acknowledged.queue_message_id,
        }
    );

    let cron = typed.cron::<fixture::ReferenceCron>().expect("replay Cron");
    let repeated = cron
        .mutate(
            replay_identity(900_108, issued_at_ms),
            CronMutation::Upsert {
                schedule_id: fixed_id(900_108),
                target_index: 0,
                target_partition: partition_for_shard(0).to_vec(),
                payload: CRON_PAYLOAD.to_vec(),
                interval_ms: 300_000,
                next_due_ms: acknowledged.cron_next_due_ms,
            },
        )
        .await
        .expect("recovered Cron upsert replay");
    assert_eq!(repeated.receipt.commit_sequence, acknowledged.cron_sequence);
    assert_eq!(
        repeated.output,
        CronMutationOutcome::Applied {
            generation: acknowledged.cron_generation,
        }
    );

    let workflow = typed
        .workflow::<fixture::ReferenceWorkflow>()
        .expect("replay Workflow");
    let repeated = workflow
        .start(
            replay_identity(900_109, issued_at_ms),
            WORKFLOW_ID.to_vec(),
            WORKFLOW_RESULT.to_vec(),
        )
        .await
        .expect("recovered Workflow start replay");
    assert_eq!(
        repeated.receipt.commit_sequence,
        acknowledged.workflow_sequence
    );
    assert_eq!(
        repeated.output,
        WorkflowOutcome::Applied {
            run_id: acknowledged.workflow_run_id,
            status: WorkflowStatus::Completed,
            event_sequence: acknowledged.workflow_event_sequence,
        }
    );
}
