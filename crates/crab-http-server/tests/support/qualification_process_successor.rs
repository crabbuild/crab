use super::*;

pub(super) async fn run() {
    let (root, sync) = process_input();
    let case = process_case();
    let acknowledgement = if case != BEFORE_WRITE {
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
    let peer_effect =
        qualification_peer::peer_effect_client(application.registry(), restored_cells.clone());
    let direct = CellClient::local_many(application.registry(), restored_cells)
        .expect("successor direct client");
    let typed = successor
        .application_handle::<fixture::ReferenceApplication>(direct.clone(), tenant, application_id)
        .with_blob_artifact_store(BlobArtifactStore::new(store));
    if let Some(leases) = acknowledgement.as_ref().and_then(|ack| ack.leases.as_ref()) {
        let deadline = leases
            .queue_until_ms
            .max(leases.activity_until_ms)
            .max(leases.effect_until_ms);
        let remaining_ms = deadline.saturating_sub(now_ms()).max(0).saturating_add(100);
        tokio::time::sleep(Duration::from_millis(
            u64::try_from(remaining_ms).expect("nonnegative lease expiry wait"),
        ))
        .await;
        assert!(now_ms() > deadline);
    }
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
        if let Some(leases) = &acknowledged.leases {
            assert_eq!(info.output.ready + info.output.leased, 1);
            assert!(info.receipt.commit_sequence >= leases.queue_sequence);
            let old_ack = queue
                .ack(
                    identity(900_118, now_ms()),
                    0,
                    acknowledged.queue_message_id,
                    leases.queue_token,
                )
                .await;
            assert!(matches!(
                old_ack,
                Err(InvocationError::Rejected(outcome))
                    if outcome.output == QueueLeaseOutcome::LeaseLost
            ));
        } else {
            assert_eq!(info.output.ready, 1);
            assert_eq!(info.output.leased, 0);
        }
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
        if let Some(leases) = &acknowledged.leases {
            assert_eq!(message.attempt, leases.queue_attempt + 1);
            assert_ne!(message.token, leases.queue_token);
        } else {
            assert_eq!(message.attempt, 1);
        }
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
    let reclaimed_activity =
        if let Some(leases) = acknowledgement.as_ref().and_then(|ack| ack.leases.as_ref()) {
            assert!(activity_state.receipt.commit_sequence >= leases.activity_sequence);
            let target = CellTarget::new(
                tenant,
                application_id,
                fixture::WORKFLOW_NAMESPACE,
                &partition_for_shard(0),
            )
            .expect("successor Activity target");
            let stale = direct
                .command::<WorkflowActivityCompleteCommand<fixture::ReferenceWorkflow>>(
                    &target,
                    identity(900_121, now_ms()),
                    ActivityCompletion {
                        run_id: acknowledgement
                            .as_ref()
                            .expect("Activity acknowledgement")
                            .activity_run_id,
                        activity_id: leases.activity_id,
                        attempt: leases.activity_attempt,
                        lease_token: leases.activity_token,
                        completion_token: fixed_id(900_119),
                        result: b"activity-result".to_vec(),
                        failed: false,
                        retryable: false,
                    },
                )
                .await;
            assert!(matches!(
                stale,
                Err(InvocationError::Rejected(outcome))
                    if outcome.output == ActivityCompletionOutcome::LeaseLost
            ));
            let claimed = direct
                .command::<WorkflowActivityClaimCommand<fixture::ReferenceWorkflow>>(
                    &target,
                    identity(900_122, now_ms()),
                    WorkflowActivityClaimRequest {
                        limit: 1,
                        lease_ms: 10_000,
                    },
                )
                .await
                .expect("successor Activity reclaim");
            let [claim] = claimed.output.as_slice() else {
                panic!("successor did not reclaim one Activity");
            };
            assert_eq!(claim.activity_id, leases.activity_id);
            assert_eq!(claim.attempt, leases.activity_attempt + 1);
            assert_ne!(claim.token, leases.activity_token);
            let completed = direct
                .command::<WorkflowActivityCompleteCommand<fixture::ReferenceWorkflow>>(
                    &target,
                    identity(900_123, now_ms()),
                    ActivityCompletion {
                        run_id: claim.run_id,
                        activity_id: claim.activity_id,
                        attempt: claim.attempt,
                        lease_token: claim.token,
                        completion_token: fixed_id(900_120),
                        result: claim.input.clone(),
                        failed: false,
                        retryable: false,
                    },
                )
                .await
                .expect("successor Activity completion");
            assert!(matches!(
                completed.output,
                ActivityCompletionOutcome::Applied(WorkflowOutcome::Applied {
                    status: WorkflowStatus::Completed,
                    event_sequence: 2,
                    ..
                })
            ));
            Some(claim.clone())
        } else {
            let outcome = activity
                .run_once(0, None)
                .await
                .expect("successor Activity run");
            if acknowledgement.is_some() {
                assert!(matches!(outcome, ActivityRunOutcome::Completed { .. }));
            } else {
                assert!(matches!(outcome, ActivityRunOutcome::Idle { .. }));
            }
            None
        };
    if let Some(acknowledged) = &acknowledgement {
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
        if let Some(claim) = reclaimed_activity {
            let mut expected = b"activity\0".to_vec();
            expected.push(0);
            expected.extend_from_slice(&claim.activity_id);
            expected.extend_from_slice(&(claim.input.len() as u32).to_be_bytes());
            expected.extend_from_slice(&claim.input);
            assert_eq!(run.event_sequence, 2);
            assert_eq!(run.result, Some(expected));
        }
        assert!(matches!(
            activity
                .run_once(0, None)
                .await
                .expect("successor Activity settled check"),
            ActivityRunOutcome::Idle { .. }
        ));
    }
    let effect_state = workflow
        .state(EFFECT_WORKFLOW_ID.to_vec(), None)
        .await
        .expect("successor Effect workflow read");
    if let Some(acknowledged) = &acknowledgement {
        let run = effect_state.output.expect("acknowledged Effect workflow");
        assert_eq!(run.run_id, acknowledged.effect_run_id);
        assert_eq!(run.status, WorkflowStatus::Completed);
        assert_eq!(
            run.result.as_deref(),
            Some(b"effect-valid-scheduled".as_slice())
        );
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
        .effects::<fixture::ReferenceWorkflow>(effect_target.clone())
        .expect("successor Effects");
    let claimed = if let Some(leases) = acknowledgement.as_ref().and_then(|ack| ack.leases.as_ref())
    {
        let stale = direct
            .command::<EffectLeaseCommand<fixture::ReferenceWorkflow>>(
                &effect_target,
                identity(900_124, now_ms()),
                EffectLeaseRequest::Ack(EffectAckRequest {
                    lease: EffectLease {
                        effect_id: leases.effect_id,
                        attempt: leases.effect_attempt,
                        token: leases.effect_token,
                        expires_at_ms: leases.effect_expires_at_ms,
                    },
                    result: EFFECT_RESULT.to_vec(),
                }),
            )
            .await;
        assert!(matches!(
            stale,
            Err(InvocationError::Rejected(outcome))
                if outcome.output == EffectLeaseOutcome::LeaseLost
        ));
        let mut reclaimed = None;
        for attempt in 0..4 {
            let next = effects
                .claim(
                    identity(900_125 + attempt, now_ms()),
                    EffectClaimRequest {
                        limit: 1,
                        lease_ms: 30_000,
                    },
                )
                .await
                .expect("successor Effect reclaim");
            if !next.output.is_empty() {
                reclaimed = Some(next);
                break;
            }
            // Expired source leases become ready after their bounded retry delay.
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        reclaimed.expect("successor Effect became claimable")
    } else {
        effects
            .claim(
                identity(900_112, now_ms()),
                EffectClaimRequest {
                    limit: 1,
                    lease_ms: 30_000,
                },
            )
            .await
            .expect("successor Effect claim")
    };
    if acknowledgement.is_some() {
        assert_eq!(claimed.output.len(), 1);
        let effect = claimed.output[0].clone();
        if let Some(leases) = acknowledgement.as_ref().and_then(|ack| ack.leases.as_ref()) {
            assert!(claimed.receipt.commit_sequence >= leases.effect_sequence);
            assert_eq!(effect.effect_id, leases.effect_id);
            assert_eq!(effect.attempt, leases.effect_attempt + 1);
            assert_ne!(effect.token, leases.effect_token);
        } else {
            assert_eq!(effect.attempt, 1);
        }
        assert!(
            effects
                .validate(vec![effect.clone()], claimed.receipt)
                .await
                .expect("successor Effect validation")
                .output
        );
        let destination = peer_effect
            .deliver(&effect, now_ms())
            .await
            .expect("successor destination Effect delivery");
        assert!(matches!(destination, StoredOutcome::Success { .. }));
        let replay = peer_effect
            .deliver(&effect, now_ms())
            .await
            .expect("duplicate destination Effect delivery");
        assert_eq!(replay, destination);
        assert_eq!(
            peer_effect
                .resolve(&effect, now_ms())
                .await
                .expect("successor destination inbox resolution"),
            Resolution::Committed(destination.clone())
        );
        let destination_state = sql
            .query(
                None,
                SqlBatch {
                    statements: vec![
                        SqlStatement {
                            sql: "SELECT payload FROM qualification_rows WHERE id = 90".into(),
                            parameters: Vec::new(),
                        },
                        SqlStatement {
                            sql: "SELECT COUNT(*) FROM qualification_rows WHERE id = 90".into(),
                            parameters: Vec::new(),
                        },
                    ],
                },
            )
            .await
            .expect("successor destination Effect observation");
        assert_eq!(
            destination_state.output[0].rows,
            vec![vec![SqlValue::Blob(EFFECT_RESULT.to_vec())]]
        );
        assert_eq!(
            destination_state.output[1].rows,
            vec![vec![SqlValue::Integer(1)]]
        );
        assert!(destination_state.receipt.commit_sequence >= destination.commit_sequence());
        let acked = effects
            .ack(
                identity(900_113, now_ms()),
                effect.clone(),
                destination.result().to_vec(),
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
        let absent = sql
            .query(
                None,
                SqlBatch {
                    statements: vec![SqlStatement {
                        sql: "SELECT COUNT(*) FROM qualification_rows WHERE id = 90".into(),
                        parameters: Vec::new(),
                    }],
                },
            )
            .await
            .expect("successor destination Effect absence");
        assert_eq!(absent.output[0].rows, vec![vec![SqlValue::Integer(0)]]);
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
