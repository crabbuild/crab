use crate::*;
use beyonddb::TableRecord;

pub(super) async fn assert_background_recovery(
    host: &crab_cell_host::CellNode,
    application: Arc<crab_cell_app::CompiledApplication>,
    bootstrap: &Bootstrap<'_>,
    directory: &std::path::Path,
    table: &TableRecord,
    participants: &[(CellTarget, CoordinatorParticipant)],
) {
    let account_id = "123456789012";
    let provisioner = Arc::new(
        CellInitialPartitionProvisioner::new(
            host.runtime(),
            application,
            bootstrap.layout.clone(),
            bootstrap.session,
            "http://recovery.internal".into(),
            directory.join("recovery"),
        )
        .unwrap(),
    );
    let client = CellClient::local_runtime(
        bootstrap.registry.clone(),
        host.runtime(),
        bootstrap.layout.clone(),
    );
    let token = "background-recovery";
    let coordinator = coordinator_target(account_id, token.as_bytes()).unwrap();
    // Put an unavailable participant before healthy work in the same shard.
    // A cursor that retries only the oldest transaction would strand both.
    let bad_id = (1_u128..100_000)
        .map(u128::to_be_bytes)
        .find(|id| coordinator_target(account_id, id).unwrap() == coordinator)
        .unwrap();
    provisioner
        .admit_coordinator(account_id, token.as_bytes())
        .await
        .unwrap();
    let isolated_partition = [231; 16];
    let isolated_target = data_target(account_id, &table.id, &isolated_partition).unwrap();
    let TransactionOperation::Put(original) = &participants[0].1.operations[0].operation else {
        unreachable!()
    };
    let mut isolated = participants[0].1.clone();
    isolated.target = CoordinatorParticipantTarget::Data {
        table_id: table.id.clone(),
        partition_id: isolated_partition,
        epoch: 1,
    };
    isolated.operations[0].index = 0;
    transaction_command!(
        client,
        BeginCrossCellTransaction,
        &coordinator,
        identity(231),
        Json(BeginCrossCellTransactionInput {
            account_id: account_id.into(),
            transaction_id: bad_id,
            token: None,
            participants: vec![isolated],
        }),
    )
    .await
    .unwrap();
    let good_id = [232; 16];
    let mut request: Vec<_> = participants.iter().map(|(_, part)| part.clone()).collect();
    for participant in &mut request {
        let TransactionOperation::Put(input) = &mut participant.operations[0].operation else {
            unreachable!()
        };
        input
            .item
            .insert("value".into(), AttributeValue::N("3".into()));
    }
    transaction_command!(
        client,
        BeginCrossCellTransaction,
        &coordinator,
        identity(232),
        Json(BeginCrossCellTransactionInput {
            account_id: account_id.into(),
            transaction_id: good_id,
            token: Some(TransactionToken {
                account_id: account_id.into(),
                token: token.into(),
                fingerprint: "background-two-items".into(),
            }),
            participants: request.clone(),
        }),
    )
    .await
    .unwrap();
    transaction_command!(
        client,
        PreparePartitionTransaction,
        &participants[0].0,
        identity(232),
        Json(PreparePartitionTransactionInput {
            table_id: table.id.clone(),
            epoch: 1,
            transaction_id: good_id,
            coordinator_cell: *coordinator.cell_id().as_bytes(),
            coordinator_key: token.as_bytes().to_vec(),
            operations: request[0]
                .operations
                .iter()
                .map(|indexed| indexed.operation.clone())
                .collect(),
        }),
    )
    .await
    .unwrap();
    let pending = client
        .query::<ReadPendingCrossCellTransactions>(
            &coordinator,
            None,
            Json(ReadPendingCrossCellTransactionsInput {
                after: None,
                limit: 1,
            }),
        )
        .await
        .unwrap();
    assert_eq!(pending.output.0[0].transaction_id, bad_id);
    let boundary = client
        .query::<beyonddb::ReadPendingTransactionBoundary>(&coordinator, None, Json(()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    assert_eq!(boundary.transaction_id, good_id);
    bootstrap
        .cell(
            &isolated_target,
            "beyonddb-data",
            231,
            &directory.join("isolated-participant.sqlite"),
            initialize_partition,
        )
        .await;
    client
        .command::<InstallPartition>(
            &isolated_target,
            identity(231),
            Json(PartitionInstall::Serving(PartitionSpec {
                table: table.clone(),
                partition_id: isolated_partition,
                lower: None,
                upper: None,
                epoch: 1,
            })),
        )
        .await
        .unwrap();
    let peer_session = SessionId::from_bytes([233; 16]);
    let peer_runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 8).unwrap(),
        16 * 1024 * 1024,
        peer_session,
    )
    .unwrap();
    let signer = PeerSigner::new(
        peer_session,
        bootstrap.registry.release_digest(),
        SigningKey::from_bytes(&[233; 32]),
    );
    let unavailable = Arc::new(AtomicBool::new(true));
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let transport = LoopbackPeerRoundTrip {
        verifier: Arc::new(PeerVerifier::new(
            peer_session,
            bootstrap.registry.release_digest(),
            signer.verifying_key(),
        )),
        dispatcher: Arc::new(PeerDispatcher::new(
            bootstrap.registry.clone(),
            Arc::new(UnavailableParticipant {
                inner: LocalRuntimePeerResolver {
                    runtime: host.runtime(),
                    layout: bootstrap.layout.clone(),
                },
                target: isolated_target.clone(),
                unavailable: unavailable.clone(),
                attempts: attempts.clone(),
            }),
            Arc::new(TestPeerAuthorizer),
        )),
    };
    let recovery_client = CellClient::runtime_with_peer(
        bootstrap.registry.clone(),
        peer_runtime.clone(),
        bootstrap.layout.clone(),
        Arc::new(signer),
        PeerPrincipal {
            issuer: "recovery-test".into(),
            subject: "recovery".into(),
            actions: vec!["beyonddb.cell.invoke".into()],
        },
        Arc::new(transport),
    );
    let tasks = host
        .install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    provisioner
        .install_transaction_recovery_loop(&tasks, CellStorage::new(recovery_client, "us-east-1"))
        .unwrap();
    assert_eq!(
        wait_for_resolution(&client, account_id, token.as_bytes(), good_id).await,
        CoordinatorDecision::Commit
    );
    for ((target, _), participant) in participants.iter().zip(&request) {
        let TransactionOperation::Put(input) = &participant.operations[0].operation else {
            unreachable!()
        };
        assert_eq!(
            client
                .query::<PartitionGet>(
                    target,
                    None,
                    Json(PartitionGetInput {
                        table_id: table.id.clone(),
                        epoch: 1,
                        key: extenddb_core::types::extract_key(&input.item, &table.key_schema),
                    })
                )
                .await
                .unwrap()
                .output
                .0,
            PartitionGetOutcome::Found(Some(input.item.clone()))
        );
    }
    assert_eq!(
        client
            .query::<ReadCrossCellTransaction>(
                &coordinator,
                None,
                Json(ReadCrossCellTransactionInput {
                    account_id: account_id.into(),
                    transaction_id: bad_id,
                    routing_key: bad_id.to_vec(),
                })
            )
            .await
            .unwrap()
            .output
            .0
            .unwrap()
            .decision,
        CoordinatorDecision::Begin
    );
    // Availability returns without another client write or a server restart.
    // The next pass must revisit the skipped record and finish it.
    assert!(attempts.load(Ordering::SeqCst) > 0);
    unavailable.store(false, Ordering::SeqCst);
    assert_eq!(
        wait_for_resolution(&client, account_id, &bad_id, bad_id).await,
        CoordinatorDecision::Commit
    );
    // A shard admitted after loop installation must join recovery, including
    // a published abort whose prepared images must never become visible.
    let abort_id = [234; 16];
    let abort_coordinator = coordinator_target(account_id, &abort_id).unwrap();
    assert_ne!(abort_coordinator, coordinator);
    unavailable.store(true, Ordering::SeqCst);
    provisioner
        .admit_coordinator(account_id, &abort_id)
        .await
        .unwrap();
    let mut aborted_put = original.clone();
    aborted_put
        .item
        .insert("value".into(), AttributeValue::N("4".into()));
    transaction_command!(
        client,
        BeginCrossCellTransaction,
        &abort_coordinator,
        identity(234),
        Json(BeginCrossCellTransactionInput {
            account_id: account_id.into(),
            transaction_id: abort_id,
            token: None,
            participants: vec![CoordinatorParticipant {
                target: CoordinatorParticipantTarget::Data {
                    table_id: table.id.clone(),
                    partition_id: isolated_partition,
                    epoch: 1,
                },
                operations: vec![IndexedTransactionOperation {
                    index: 0,
                    operation: TransactionOperation::Put(aborted_put.clone()),
                }],
            }],
        }),
    )
    .await
    .unwrap();
    transaction_command!(
        client,
        PreparePartitionTransaction,
        &isolated_target,
        identity(234),
        Json(PreparePartitionTransactionInput {
            table_id: table.id.clone(),
            epoch: 1,
            transaction_id: abort_id,
            coordinator_cell: *abort_coordinator.cell_id().as_bytes(),
            coordinator_key: abort_id.to_vec(),
            operations: vec![TransactionOperation::Put(aborted_put)],
        }),
    )
    .await
    .unwrap();
    let aborted = CoordinatorDecision::Abort {
        index: None,
        reason: None,
    };
    client
        .command::<DecideCrossCellTransaction>(
            &abort_coordinator,
            identity(235),
            Json(DecideCrossCellTransactionInput {
                account_id: account_id.into(),
                transaction_id: abort_id,
                routing_key: abort_id.to_vec(),
                decision: aborted.clone(),
            }),
        )
        .await
        .unwrap();
    unavailable.store(false, Ordering::SeqCst);
    assert_eq!(
        wait_for_resolution(&client, account_id, &abort_id, abort_id).await,
        aborted
    );
    assert_eq!(
        client
            .query::<PartitionGet>(
                &isolated_target,
                None,
                Json(PartitionGetInput {
                    table_id: table.id.clone(),
                    epoch: 1,
                    key: extenddb_core::types::extract_key(&original.item, &table.key_schema),
                })
            )
            .await
            .unwrap()
            .output
            .0,
        PartitionGetOutcome::Found(Some(original.item.clone()))
    );
    assert!(
        client
            .query::<beyonddb::ReadPendingTransactionBoundary>(&coordinator, None, Json(()))
            .await
            .unwrap()
            .output
            .0
            .is_none()
    );
    // Cancellation owns the in-flight recovery future; shutdown must not wait
    // for another timer tick or leave a task accessing closed Cell handles.
    tokio::time::timeout(Duration::from_secs(5), host.shutdown())
        .await
        .unwrap()
        .unwrap();
    peer_runtime.shutdown().await.unwrap();
}

struct UnavailableParticipant {
    inner: LocalRuntimePeerResolver,
    target: CellTarget,
    unavailable: Arc<AtomicBool>,
    attempts: Arc<std::sync::atomic::AtomicUsize>,
}

impl PeerCellResolver for UnavailableParticipant {
    fn resolve(
        &self,
        target: CellTarget,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = crab_cell_runtime::Result<CellHandle>>
                + Send
                + 'static,
        >,
    > {
        if target == self.target && self.unavailable.load(Ordering::SeqCst) {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            return Box::pin(async { Err(crab_cell_runtime::Error::CellNotActive) });
        }
        self.inner.resolve(target)
    }
}

async fn wait_for_resolution(
    client: &CellClient,
    account_id: &str,
    routing_key: &[u8],
    transaction_id: [u8; 16],
) -> CoordinatorDecision {
    let target = coordinator_target(account_id, routing_key).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = client
                .query::<ReadCrossCellTransaction>(
                    &target,
                    None,
                    Json(ReadCrossCellTransactionInput {
                        account_id: account_id.into(),
                        transaction_id,
                        routing_key: routing_key.to_vec(),
                    }),
                )
                .await
                .unwrap()
                .output
                .0
                .unwrap();
            if status.resolved_count == status.participant_count {
                return status.decision;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap()
}
