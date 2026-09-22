use super::*;

pub(super) async fn run() {
    let (root, sync) = process_input();
    let case = process_case();
    let (store, _) = rustfs_public_store();
    let (node, typed, tenant, application, _directory, registry, handles, _store) =
        public_host_fixture_with_store(store.clone(), root.clone()).await;
    if case == BEFORE_WRITE {
        publish_marker(&sync, "ready", b"owner-bootstrapped");
        let _node = node;
        std::future::pending::<()>().await;
        return;
    }
    let target = CellTarget::new(
        tenant,
        application,
        fixture::SQL_NAMESPACE,
        &partition_for_shard(0),
    )
    .expect("source SQL target");
    let sql = typed
        .sql::<fixture::ReferenceSql>(target.clone())
        .expect("source SQL");
    let committed = sql
        .batch(
            identity(900_100, now_ms()),
            SqlBatch {
                statements: vec![SqlStatement {
                    sql: "INSERT INTO qualification_rows (id, payload) VALUES (1, ?1)".into(),
                    parameters: vec![SqlValue::Blob(SQL_PAYLOAD.to_vec())],
                }],
            },
        )
        .await
        .expect("acknowledged SQL write");
    assert_eq!(committed.output[0].rows_affected, 1);
    let layout = CellStorageLayout::new(store, root, *application.as_bytes());
    let authority = CellAuthority::new(layout);
    let observed = authority
        .load(target.cell_id())
        .await
        .expect("source authority")
        .expect("source owner");
    assert!(observed.value().root.is_some());
    let kv = typed
        .kv::<fixture::ReferenceKv>(fixture::KV_NAMESPACE)
        .expect("source KV");
    let written = kv
        .atomic(
            identity(900_101, now_ms()),
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
        .expect("acknowledged KV write");
    let KvAtomicOutcome::Applied(results) = written.output else {
        panic!("source KV write was not applied");
    };
    let version = results[0].version.expect("published KV version");
    let blob = typed.blob::<fixture::ReferenceBlob>().expect("source Blob");
    let upload_id = [103; 16];
    let issued_at_ms = now_ms();
    blob.mutate(
        identity(900_102, issued_at_ms),
        BlobMutation::Begin {
            key: BLOB_KEY.to_vec(),
            upload_id,
            condition: BlobCondition::Missing,
            content_type: None,
            metadata: Vec::new(),
            expires_at_ms: issued_at_ms + 60_000,
        },
    )
    .await
    .expect("source Blob begin");
    blob.mutate(
        identity(900_103, issued_at_ms),
        BlobMutation::PutPart {
            key: BLOB_KEY.to_vec(),
            upload_id,
            part_number: 1,
            payload: BLOB_PAYLOAD.to_vec(),
        },
    )
    .await
    .expect("source Blob part");
    let published = blob
        .mutate(
            identity(900_104, issued_at_ms),
            BlobMutation::Complete {
                key: BLOB_KEY.to_vec(),
                upload_id,
                part_count: 1,
            },
        )
        .await
        .expect("acknowledged Blob commit");
    let BlobMutationOutcome::Committed { etag, size } = published.output else {
        panic!("source Blob was not committed");
    };
    let queue = typed
        .queue::<fixture::ReferenceQueue>()
        .expect("source Queue");
    let queued = queue
        .send(
            identity(900_105, now_ms()),
            QueueSendRequest {
                producer_id: fixed_id(900_105),
                payload: QUEUE_PAYLOAD.to_vec(),
                available_at_ms: now_ms(),
            },
        )
        .await
        .expect("acknowledged Queue send");
    let QueueSendOutcome::Sent { message_id } = queued.output else {
        panic!("source Queue message was not sent");
    };
    let cron = typed.cron::<fixture::ReferenceCron>().expect("source Cron");
    let schedule_id = fixed_id(900_108);
    let next_due_ms = now_ms() + 20_000;
    let scheduled = cron
        .mutate(
            identity(900_108, now_ms()),
            CronMutation::Upsert {
                schedule_id,
                target_index: 0,
                target_partition: partition_for_shard(0).to_vec(),
                payload: CRON_PAYLOAD.to_vec(),
                interval_ms: 300_000,
                next_due_ms,
            },
        )
        .await
        .expect("acknowledged Cron schedule");
    let CronMutationOutcome::Applied { generation } = scheduled.output else {
        panic!("source Cron schedule was not applied");
    };
    let workflow = typed
        .workflow::<fixture::ReferenceWorkflow>()
        .expect("source Workflow");
    let started = workflow
        .start(
            identity(900_109, now_ms()),
            WORKFLOW_ID.to_vec(),
            WORKFLOW_RESULT.to_vec(),
        )
        .await
        .expect("acknowledged Workflow start");
    let WorkflowOutcome::Applied {
        run_id,
        status: WorkflowStatus::Completed,
        event_sequence,
    } = started.output
    else {
        panic!("source Workflow did not complete");
    };
    let activity_started = workflow
        .start(
            identity(900_110, now_ms()),
            ACTIVITY_WORKFLOW_ID.to_vec(),
            b"activity".to_vec(),
        )
        .await
        .expect("acknowledged Activity workflow start");
    let WorkflowOutcome::Applied {
        run_id: activity_run_id,
        status: WorkflowStatus::Running,
        ..
    } = activity_started.output
    else {
        panic!("source Activity workflow is not running");
    };
    let effect_started = workflow
        .start(
            identity(900_111, now_ms()),
            EFFECT_WORKFLOW_ID.to_vec(),
            b"effect-valid".to_vec(),
        )
        .await
        .expect("acknowledged Effect workflow start");
    let WorkflowOutcome::Applied {
        run_id: effect_run_id,
        status: WorkflowStatus::Completed,
        ..
    } = effect_started.output
    else {
        panic!("source Effect workflow did not complete");
    };
    let leases = if case == AFTER_LEASE {
        let claimed = queue
            .claim(
                identity(900_115, now_ms()),
                0,
                QueueClaimRequest {
                    limit: 1,
                    lease_ms: 5_000,
                },
            )
            .await
            .expect("acknowledged source Queue claim");
        let [queue_claim] = claimed.output.as_slice() else {
            panic!("source Queue claim did not select one message");
        };
        assert_eq!(queue_claim.message_id, message_id);
        assert_eq!(queue_claim.attempt, 1);
        let workflow_target = CellTarget::new(
            tenant,
            application,
            fixture::WORKFLOW_NAMESPACE,
            &partition_for_shard(0),
        )
        .expect("source Workflow target");
        let activity_client =
            CellClient::local_many(registry, handles).expect("source Activity client");
        let activity_claimed = activity_client
            .command::<WorkflowActivityClaimCommand<fixture::ReferenceWorkflow>>(
                &workflow_target,
                identity(900_116, now_ms()),
                WorkflowActivityClaimRequest {
                    limit: 1,
                    lease_ms: 5_000,
                },
            )
            .await
            .expect("acknowledged source Activity claim");
        let [activity_claim] = activity_claimed.output.as_slice() else {
            panic!("source Activity claim did not select one attempt");
        };
        assert_eq!(activity_claim.run_id, activity_run_id);
        assert_eq!(activity_claim.attempt, 1);
        let effects = typed
            .effects::<fixture::ReferenceWorkflow>(workflow_target)
            .expect("source Effects");
        let effect_claimed = effects
            .claim(
                identity(900_117, now_ms()),
                EffectClaimRequest {
                    limit: 1,
                    lease_ms: 5_000,
                },
            )
            .await
            .expect("acknowledged source Effect claim");
        let [effect_claim] = effect_claimed.output.as_slice() else {
            panic!("source Effect claim did not select one delivery");
        };
        assert_eq!(effect_claim.attempt, 1);
        Some(LeaseEvidence {
            queue_attempt: queue_claim.attempt,
            queue_token: queue_claim.token,
            queue_until_ms: queue_claim.lease_until_ms,
            queue_sequence: claimed.receipt.commit_sequence,
            activity_id: activity_claim.activity_id,
            activity_attempt: activity_claim.attempt,
            activity_token: activity_claim.token,
            activity_until_ms: activity_claim.lease_until_ms,
            activity_sequence: activity_claimed.receipt.commit_sequence,
            effect_id: effect_claim.effect_id,
            effect_attempt: effect_claim.attempt,
            effect_token: effect_claim.token,
            effect_until_ms: effect_claim.lease_until_ms,
            effect_expires_at_ms: effect_claim.expires_at_ms,
            effect_sequence: effect_claimed.receipt.commit_sequence,
        })
    } else {
        None
    };
    let acknowledgement = Acknowledgement {
        sql_sequence: committed.receipt.commit_sequence,
        kv_sequence: written.receipt.commit_sequence,
        kv_version: version.to_vec(),
        blob_sequence: published.receipt.commit_sequence,
        blob_etag: etag,
        blob_size: size,
        queue_sequence: queued.receipt.commit_sequence,
        queue_message_id: message_id,
        cron_sequence: scheduled.receipt.commit_sequence,
        cron_generation: generation,
        cron_next_due_ms: next_due_ms,
        workflow_sequence: started.receipt.commit_sequence,
        workflow_run_id: run_id,
        workflow_event_sequence: event_sequence,
        activity_sequence: activity_started.receipt.commit_sequence,
        activity_run_id,
        effect_sequence: effect_started.receipt.commit_sequence,
        effect_run_id,
        leases,
    };
    let bytes = serde_json::to_vec(&acknowledgement).expect("encode acknowledgement");
    publish_marker(&sync, "ack", &bytes);
    let _node = node;
    std::future::pending::<()>().await;
}
