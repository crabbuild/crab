use super::*;

pub(super) async fn run() {
    let (root, sync) = process_input();
    let case = process_case();
    let (store, _) = process_store();
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
    let replay_issued_at_ms = now_ms();
    let committed = sql
        .batch(
            replay_identity(900_100, replay_issued_at_ms),
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
            replay_identity(900_101, replay_issued_at_ms),
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
            replay_identity(900_104, replay_issued_at_ms),
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
    let queue_available_at_ms = now_ms();
    let queued = queue
        .send(
            replay_identity(900_105, replay_issued_at_ms),
            QueueSendRequest {
                producer_id: fixed_id(900_105),
                payload: QUEUE_PAYLOAD.to_vec(),
                available_at_ms: queue_available_at_ms,
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
            replay_identity(900_108, replay_issued_at_ms),
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
            replay_identity(900_109, replay_issued_at_ms),
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
    let (expiry_sql_sequence, expiry_kv_sequence, expiry_cron, expiry_workflow) = if case
        == AFTER_SQL_EXPIRY
        || case == AFTER_KV_EXPIRY
        || case == AFTER_CRON_EXPIRY
        || case == AFTER_WORKFLOW_EXPIRY
    {
        let observer_client = CellClient::local_many(Arc::clone(&registry), handles.clone())
            .expect("source expiry observer client");
        let observer = node.application_handle::<fixture::ReferenceApplication>(
            observer_client,
            tenant,
            application,
        );
        let (peer_client, entered, release, dispatched) =
            qualification_peer::peer_client_with_delayed_mutation_receive(
                Arc::clone(&registry),
                handles.clone(),
            );
        let peer = node.application_handle::<fixture::ReferenceApplication>(
            peer_client,
            tenant,
            application,
        );
        let expiry = process_expiry::ExpiryCase::new(
            &typed,
            &peer,
            &observer,
            &entered,
            &release,
            &dispatched,
            tenant,
            application,
        );
        if case == AFTER_SQL_EXPIRY {
            (
                Some(
                    expiry
                        .sql(SQL_EXPIRY_OPERATION_ID, SQL_EXPIRY_NONCE)
                        .await
                        .expect("source scheduled SQL expiry"),
                ),
                None,
                None,
                None,
            )
        } else if case == AFTER_KV_EXPIRY {
            (
                None,
                Some(
                    expiry
                        .kv(KV_EXPIRY_OPERATION_ID, KV_EXPIRY_NONCE)
                        .await
                        .expect("source scheduled KV expiry"),
                ),
                None,
                None,
            )
        } else if case == AFTER_CRON_EXPIRY {
            let (sequence, generation, next_due_ms) = expiry
                .cron(CRON_EXPIRY_OPERATION_ID, CRON_EXPIRY_NONCE)
                .await
                .expect("source scheduled Cron expiry");
            (
                None,
                None,
                Some(CronExpiryEvidence {
                    sequence,
                    generation,
                    next_due_ms,
                }),
                None,
            )
        } else {
            let (sequence, run_id) = expiry
                .workflow(WORKFLOW_EXPIRY_OPERATION_ID, WORKFLOW_EXPIRY_NONCE)
                .await
                .expect("source scheduled Workflow expiry");
            (
                None,
                None,
                None,
                Some(WorkflowExpiryEvidence { sequence, run_id }),
            )
        }
    } else {
        (None, None, None, None)
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
        let activity_client = CellClient::local_many(Arc::clone(&registry), handles.clone())
            .expect("source Activity client");
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
    let settlements = if case == AFTER_SETTLEMENT {
        let claimed = queue
            .claim(
                identity(900_135, now_ms()),
                0,
                QueueClaimRequest {
                    limit: 1,
                    lease_ms: 30_000,
                },
            )
            .await
            .expect("owner Queue settlement claim");
        let [queue_claim] = claimed.output.as_slice() else {
            panic!("owner Queue settlement claim did not select one message");
        };
        assert_eq!(queue_claim.message_id, message_id);
        let queue_acked = queue
            .ack(
                replay_identity(900_136, replay_issued_at_ms),
                0,
                queue_claim.message_id,
                queue_claim.token,
            )
            .await
            .expect("acknowledged owner Queue settlement");
        assert!(matches!(
            queue_acked.output,
            QueueLeaseOutcome::Applied {
                state: QueueState::Acked,
                ..
            }
        ));

        let workflow_target = CellTarget::new(
            tenant,
            application,
            fixture::WORKFLOW_NAMESPACE,
            &partition_for_shard(0),
        )
        .expect("owner Workflow settlement target");
        let activity_claimed = typed
            .command::<WorkflowActivityClaimCommand<fixture::ReferenceWorkflow>>(
                &workflow_target,
                identity(900_137, now_ms()),
                WorkflowActivityClaimRequest {
                    limit: 1,
                    lease_ms: 30_000,
                },
            )
            .await
            .expect("owner Activity settlement claim");
        let [activity_claim] = activity_claimed.output.as_slice() else {
            panic!("owner Activity settlement claim did not select one attempt");
        };
        assert_eq!(activity_claim.run_id, activity_run_id);
        let completed = typed
            .command::<WorkflowActivityCompleteCommand<fixture::ReferenceWorkflow>>(
                &workflow_target,
                replay_identity(900_138, replay_issued_at_ms),
                ActivityCompletion {
                    run_id: activity_claim.run_id,
                    activity_id: activity_claim.activity_id,
                    attempt: activity_claim.attempt,
                    lease_token: activity_claim.token,
                    completion_token: fixed_id(900_138),
                    result: activity_claim.input.clone(),
                    failed: false,
                    retryable: false,
                },
            )
            .await
            .expect("acknowledged owner Activity completion");
        assert!(matches!(
            completed.output,
            ActivityCompletionOutcome::Applied(WorkflowOutcome::Applied {
                status: WorkflowStatus::Completed,
                event_sequence: 2,
                ..
            })
        ));
        let activity_state = workflow
            .state(ACTIVITY_WORKFLOW_ID.to_vec(), Some(completed.receipt))
            .await
            .expect("owner Activity settlement read");
        let activity_run = activity_state.output.expect("completed owner Activity run");
        let activity_result = activity_run
            .result
            .expect("completed owner Activity result");
        assert_eq!(activity_run.event_sequence, 2);

        let effects = typed
            .effects::<fixture::ReferenceWorkflow>(workflow_target)
            .expect("owner settlement Effects");
        let effect_claimed = effects
            .claim(
                identity(900_139, now_ms()),
                EffectClaimRequest {
                    limit: 1,
                    lease_ms: 30_000,
                },
            )
            .await
            .expect("owner Effect settlement claim");
        let [effect] = effect_claimed.output.as_slice() else {
            panic!("owner Effect settlement claim did not select one delivery");
        };
        let peer_effect = qualification_peer::peer_effect_client(registry, handles);
        let destination = peer_effect
            .deliver(effect, now_ms())
            .await
            .expect("acknowledged owner destination delivery");
        assert!(matches!(destination, StoredOutcome::Success { .. }));
        assert_eq!(
            peer_effect
                .resolve(effect, now_ms())
                .await
                .expect("owner destination inbox resolution"),
            Resolution::Committed(destination.clone())
        );
        let effect_acked = effects
            .ack(
                replay_identity(900_140, replay_issued_at_ms),
                effect.clone(),
                destination.result().to_vec(),
            )
            .await
            .expect("acknowledged owner Effect settlement");
        assert_eq!(effect_acked.output, EffectLeaseOutcome::Delivered);
        Some(SettlementEvidence {
            queue_token: queue_claim.token,
            queue_ack_sequence: queue_acked.receipt.commit_sequence,
            activity_id: activity_claim.activity_id,
            activity_attempt: activity_claim.attempt,
            activity_token: activity_claim.token,
            activity_completion_token: fixed_id(900_138),
            activity_input: activity_claim.input.clone(),
            activity_complete_sequence: completed.receipt.commit_sequence,
            activity_event_sequence: activity_run.event_sequence,
            activity_result,
            effect_id: effect.effect_id,
            effect_attempt: effect.attempt,
            effect_token: effect.token,
            effect_expires_at_ms: effect.expires_at_ms,
            effect_result: destination.result().to_vec(),
            effect_destination_sequence: destination.commit_sequence(),
            effect_ack_sequence: effect_acked.receipt.commit_sequence,
        })
    } else {
        None
    };
    let acknowledgement = Acknowledgement {
        replay_issued_at_ms,
        queue_available_at_ms,
        sql_sequence: committed.receipt.commit_sequence,
        expiry_sql_sequence,
        expiry_kv_sequence,
        expiry_cron,
        expiry_workflow,
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
        settlements,
    };
    let bytes = serde_json::to_vec(&acknowledgement).expect("encode acknowledgement");
    publish_marker(&sync, "ack", &bytes);
    let _node = node;
    std::future::pending::<()>().await;
}
