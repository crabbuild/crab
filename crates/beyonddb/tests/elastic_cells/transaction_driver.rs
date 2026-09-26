use crate::*;
use beyonddb::{TableRecord, TransactionFailure};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver_resumes_prepares_and_resolves_commit_condition_and_lock_failures() {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "transaction-driver".into(),
            cargo_lock_digest: Digest::from_bytes([1; 32]),
        })
        .unwrap(),
    );
    let account_id = "123456789012";
    let account = account_target(account_id).unwrap();
    let session = SessionId::from_bytes([180; 16]);
    let directory = tempfile::TempDir::new().unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("transaction-driver"),
        *account.application().as_bytes(),
    );
    let host = CellNodeBuilder::new(application.clone())
        .with_runtime(SqlWorkerPool::new(1, 16).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(Host::default().with_local_disk_budget(DiskBudget::new(1 << 30)))
        .with_session(session)
        .build_unleased_for_maintenance()
        .unwrap();
    let registry = application.registry();
    let bootstrap = Bootstrap {
        runtime: host.runtime(),
        registry: &registry,
        layout: &layout,
        session,
    };
    let mut table_bytes = [180; 32];
    table_bytes[..16].copy_from_slice(account.tenant().as_bytes());
    let table = TableRecord {
        id: blake3::Hash::from_bytes(table_bytes).to_hex().to_string(),
        created_at_ms: 1000,
        table_name: "Driver".into(),
        key_schema: vec![KeySchemaElement {
            attribute_name: "id".into(),
            key_type: KeyType::Hash,
        }],
        attribute_definitions: vec![AttributeDefinition {
            attribute_name: "id".into(),
            attribute_type: ScalarAttributeType::S,
        }],
        billing_mode: BillingMode::PayPerRequest,
        provisioned_throughput: None,
        deletion_protection_enabled: false,
        pay_per_request_since_ms: Some(1000),
    };
    let client = CellClient::local_runtime(registry.clone(), host.runtime(), layout.clone());
    let storage = CellStorage::new(client.clone(), "us-east-1");
    let boundary = [0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut participants = Vec::new();
    for (position, left) in [true, false].into_iter().enumerate() {
        let partition_id = [u8::try_from(position + 1).unwrap(); 16];
        let target = data_target(account_id, &table.id, &partition_id).unwrap();
        bootstrap
            .cell(
                &target,
                "beyonddb-data",
                partition_id[0],
                &directory.path().join(format!("data-{position}.sqlite")),
                initialize_partition,
            )
            .await;
        client
            .command::<InstallPartition>(
                &target,
                identity(180),
                Json(PartitionInstall::Serving(PartitionSpec {
                    table: table.clone(),
                    partition_id,
                    lower: (!left).then_some(boundary),
                    upper: left.then_some(boundary),
                    epoch: 1,
                })),
            )
            .await
            .unwrap();
        let mut item = key_in_range(&table.id, &table.key_schema, left, 1000);
        item.insert("value".into(), AttributeValue::N("1".into()));
        participants.push((
            target,
            CoordinatorParticipant {
                target: CoordinatorParticipantTarget::Data {
                    table_id: table.id.clone(),
                    partition_id,
                    epoch: 1,
                },
                operations: vec![IndexedTransactionOperation {
                    index: u8::try_from(position).unwrap(),
                    operation: TransactionOperation::Put(PutItemInput {
                        table_name: table.table_name.clone(),
                        table_id: table.id.clone(),
                        item,
                        condition: None,
                    }),
                }],
            },
        ));
    }
    participants.sort_by_key(|(target, _)| *target.cell_id().as_bytes());
    // Operation indexes deliberately oppose participant order.
    for (position, (_, participant)) in participants.iter_mut().enumerate() {
        participant.operations[0].index = u8::try_from(1 - position).unwrap();
    }
    for scenario in [181_u8, 182, 183, 184, 185, 186, 187] {
        let transaction_id = [scenario; 16];
        let coordinator = coordinator_target(account_id, &transaction_id).unwrap();
        bootstrap
            .cell(
                &coordinator,
                "beyonddb-coordinator",
                scenario,
                &directory
                    .path()
                    .join(format!("coordinator-{scenario}.sqlite")),
                initialize_coordinator,
            )
            .await;
        let mut request: Vec<_> = participants
            .iter()
            .map(|(_, participant)| participant.clone())
            .collect();
        if !matches!(scenario, 181 | 185 | 186) {
            for participant in &mut request {
                let TransactionOperation::Put(input) = &mut participant.operations[0].operation
                else {
                    unreachable!()
                };
                input
                    .item
                    .insert("value".into(), AttributeValue::N("2".into()));
            }
        }
        if scenario == 182 {
            let TransactionOperation::Put(input) = &mut request[1].operations[0].operation else {
                unreachable!()
            };
            input.condition = Some(serde_json::from_value(serde_json::json!({
                "expression": {"Function": {"name": "attribute_not_exists", "args": [{"Path": [{"Attribute": "id"}]}]}},
                "maps": {"names": {}, "values": {}}
            })).unwrap());
        }
        if scenario == 184 {
            let CoordinatorParticipantTarget::Data { epoch, .. } = &mut request[1].target else {
                unreachable!()
            };
            *epoch = 2;
        }
        transaction_command!(
            client,
            BeginCrossCellTransaction,
            &coordinator,
            identity(scenario),
            Json(BeginCrossCellTransactionInput {
                account_id: account_id.into(),
                transaction_id,
                token: None,
                participants: request.clone(),
            }),
        )
        .await
        .unwrap();
        if scenario == 181 || scenario == 183 {
            // Resume a prepare whose receipt was never recorded, or collide
            // with a different transaction on the second participant.
            let position = usize::from(scenario == 183);
            let held_id = if scenario == 183 {
                [190; 16]
            } else {
                transaction_id
            };
            transaction_command!(
                client,
                PreparePartitionTransaction,
                &participants[position].0,
                identity(scenario),
                Json(PreparePartitionTransactionInput {
                    table_id: table.id.clone(),
                    epoch: 1,
                    transaction_id: held_id,
                    coordinator_cell: *coordinator.cell_id().as_bytes(),
                    coordinator_key: transaction_id.to_vec(),
                    operations: request[position]
                        .operations
                        .iter()
                        .map(|operation| operation.operation.clone())
                        .collect(),
                }),
            )
            .await
            .unwrap();
        }
        let decision = if scenario == 181 {
            let (first, second) = tokio::join!(
                storage.resume_cross_cell_transaction(account_id, &transaction_id, transaction_id),
                storage.resume_cross_cell_transaction(account_id, &transaction_id, transaction_id),
            );
            assert_eq!(first.as_ref().unwrap(), &CoordinatorDecision::Commit);
            assert_eq!(second.as_ref().unwrap(), &CoordinatorDecision::Commit);
            first.unwrap()
        } else if scenario >= 185 {
            let peer_session = SessionId::from_bytes([200; 16]);
            let peer_runtime = CellRuntime::new(
                SqlWorkerPool::new(1, 8).unwrap(),
                16 * 1024 * 1024,
                peer_session,
            )
            .unwrap();
            let signer = PeerSigner::new(
                peer_session,
                registry.release_digest(),
                SigningKey::from_bytes(&[200; 32]),
            );
            let lost = Arc::new(std::sync::atomic::AtomicU8::new(0));
            let transport = DropPhaseReplies {
                verifier: Arc::new(PeerVerifier::new(
                    peer_session,
                    registry.release_digest(),
                    signer.verifying_key(),
                )),
                dispatcher: Arc::new(PeerDispatcher::new(
                    registry.clone(),
                    Arc::new(LocalRuntimePeerResolver {
                        runtime: host.runtime(),
                        layout: layout.clone(),
                    }),
                    Arc::new(TestPeerAuthorizer),
                )),
                lost: lost.clone(),
                enabled: if scenario == 185 { 3 } else { 0 },
            };
            let transport: Arc<dyn PeerRoundTrip> = if scenario == 185 {
                Arc::new(transport)
            } else {
                Arc::new(RefusePhase {
                    inner: transport,
                    command: if scenario == 186 { 12 } else { 14 },
                    winner: (scenario == 186).then(|| (client.clone(), transaction_id)),
                    refused: lost.clone(),
                })
            };
            let remote = CellStorage::new(
                CellClient::runtime_with_peer(
                    registry.clone(),
                    peer_runtime.clone(),
                    layout.clone(),
                    Arc::new(signer),
                    PeerPrincipal {
                        issuer: "driver-test".into(),
                        subject: "driver".into(),
                        actions: vec!["beyonddb.cell.invoke".into()],
                    },
                    transport,
                ),
                "us-east-1",
            );
            let decision = remote
                .resume_cross_cell_transaction(account_id, &transaction_id, transaction_id)
                .await
                .unwrap();
            assert_eq!(
                lost.load(Ordering::SeqCst),
                if scenario == 185 { 3 } else { 1 }
            );
            peer_runtime.shutdown().await.unwrap();
            decision
        } else {
            storage
                .resume_cross_cell_transaction(account_id, &transaction_id, transaction_id)
                .await
                .unwrap()
        };
        match scenario {
            181 | 185 | 186 => assert_eq!(decision, CoordinatorDecision::Commit),
            187 => assert_eq!(
                decision,
                CoordinatorDecision::Abort {
                    index: Some(1),
                    reason: Some(TransactionFailure::Throttled)
                }
            ),
            182 => assert!(
                matches!(&decision, CoordinatorDecision::Abort { index: Some(0), reason: Some(TransactionFailure::ConditionFailed(Some(item))) } if item["value"] == AttributeValue::N("1".into()))
            ),
            _ => assert_eq!(
                decision,
                CoordinatorDecision::Abort {
                    index: Some(0),
                    reason: Some(TransactionFailure::Conflict)
                }
            ),
        }
        assert_eq!(
            storage
                .resume_cross_cell_transaction(account_id, &transaction_id, transaction_id)
                .await
                .unwrap(),
            decision
        );
        let status = client
            .query::<ReadCrossCellTransaction>(
                &coordinator,
                None,
                Json(ReadCrossCellTransactionInput {
                    account_id: account_id.into(),
                    transaction_id,
                    routing_key: transaction_id.to_vec(),
                }),
            )
            .await
            .unwrap()
            .output
            .0
            .unwrap();
        assert_eq!(status.resolved_count, 2);
        if scenario == 183 {
            client
                .command::<ResolvePartitionTransaction>(
                    &participants[1].0,
                    identity(190),
                    Json(ResolveTransactionInput {
                        transaction_id: [190; 16],
                        coordinator_cell: *coordinator.cell_id().as_bytes(),
                        commit: false,
                    }),
                )
                .await
                .unwrap();
        }
        for (target, participant) in &participants {
            let TransactionOperation::Put(input) = &participant.operations[0].operation else {
                unreachable!()
            };
            let key = extenddb_core::types::extract_key(&input.item, &table.key_schema);
            assert_eq!(
                client
                    .query::<PartitionGet>(
                        target,
                        None,
                        Json(PartitionGetInput {
                            table_id: table.id.clone(),
                            epoch: 1,
                            key,
                        })
                    )
                    .await
                    .unwrap()
                    .output
                    .0,
                PartitionGetOutcome::Found(Some(input.item.clone()))
            );
        }
    }
    super::transaction_recovery::assert_background_recovery(
        &host,
        application,
        &bootstrap,
        directory.path(),
        &table,
        &participants,
    )
    .await;
}

pub(super) struct DropPhaseReplies {
    pub(super) verifier: Arc<PeerVerifier>,
    pub(super) dispatcher: Arc<PeerDispatcher>,
    pub(super) lost: Arc<std::sync::atomic::AtomicU8>,
    pub(super) enabled: u8,
}

impl PeerRoundTrip for DropPhaseReplies {
    fn send(
        &self,
        target: CellTarget,
        request: Vec<u8>,
        _remaining_ms: u32,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = crab_cell_runtime::Result<Vec<u8>>> + Send + 'static>,
    > {
        let verifier = self.verifier.clone();
        let dispatcher = self.dispatcher.clone();
        let lost = self.lost.clone();
        let enabled = self.enabled;
        Box::pin(async move {
            use crab_cell_runtime::peer::wire::{mutation_request, peer_request};
            let now_ms = i64::try_from(
                std::time::SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis(),
            )
            .unwrap();
            let verified = verifier.verify(&request, now_ms)?;
            assert_eq!(verified.target(), &target);
            let phase = match verified.operation() {
                Some(peer_request::Operation::Mutate(mutation)) => match &mutation.operation {
                    Some(mutation_request::Operation::CellCommand(command)) => {
                        match command.command_id {
                            12 | 21 => 1,
                            3 => 2,
                            1 => 4,
                            13 | 22 => 8,
                            5 => 16,
                            14 => 32,
                            23 => 64,
                            _ => 0,
                        }
                    }
                    _ => 0,
                },
                _ => 0,
            };
            let phase = phase & enabled;
            let response = dispatcher.dispatch_bytes(&verified, now_ms).await?;
            if phase != 0 && lost.fetch_or(phase, Ordering::SeqCst) & phase == 0 {
                return Err(crab_cell_runtime::Error::PeerTransportUnknown {
                    context: "injected lost phase reply",
                    // Unknown prepare outcomes remain unknown even when a
                    // capacity error caused the reply to be lost.
                    source: if phase == 1 {
                        Box::new(crab_cell_runtime::Error::Capacity(
                            "injected after publication",
                        ))
                    } else {
                        Box::new(std::io::Error::from(std::io::ErrorKind::ConnectionReset))
                    },
                });
            }
            Ok(response)
        })
    }
}

struct RefusePhase {
    inner: DropPhaseReplies,
    command: u32,
    winner: Option<(CellClient, [u8; 16])>,
    refused: Arc<std::sync::atomic::AtomicU8>,
}

impl PeerRoundTrip for RefusePhase {
    fn send(
        &self,
        target: CellTarget,
        request: Vec<u8>,
        _remaining_ms: u32,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = crab_cell_runtime::Result<Vec<u8>>> + Send + 'static>,
    > {
        let verifier = self.inner.verifier.clone();
        let dispatcher = self.inner.dispatcher.clone();
        let winner = self.winner.clone();
        let command = self.command;
        let refused = self.refused.clone();
        Box::pin(async move {
            use crab_cell_runtime::peer::wire::{mutation_request, peer_request};
            let now_ms = i64::try_from(
                std::time::SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis(),
            )
            .unwrap();
            let verified = verifier.verify(&request, now_ms)?;
            assert_eq!(verified.target(), &target);
            let selected = matches!(verified.operation(),
                Some(peer_request::Operation::Mutate(mutation)) if matches!(&mutation.operation,
                    Some(mutation_request::Operation::CellCommand(input)) if input.command_id == command));
            if selected && refused.swap(1, Ordering::SeqCst) == 0 {
                if let Some((client, transaction_id)) = winner {
                    let storage = CellStorage::new(client, "us-east-1");
                    assert_eq!(
                        storage
                            .resume_cross_cell_transaction(
                                "123456789012",
                                &transaction_id,
                                transaction_id
                            )
                            .await
                            .unwrap(),
                        CoordinatorDecision::Commit
                    );
                }
                // This attempt was never submitted. A competing driver may
                // nevertheless have committed before the refusal reaches it.
                return Err(crab_cell_runtime::Error::Capacity(
                    "injected before submission",
                ));
            }
            dispatcher.dispatch_bytes(&verified, now_ms).await
        })
    }
}
