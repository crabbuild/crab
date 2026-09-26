use crate::*;
use beyonddb::{PrepareAccountTransaction, PrepareAccountTransactionInput, ReadAccountTransaction};
use crab_cell_runtime::codec::{BoundedDecoder, WireValue};
use std::sync::atomic::AtomicU8;
use tokio::sync::Notify;

fn mutation() -> MutationIdentity {
    let mut value = identity(246);
    value.request_id = RequestId::from_bytes(*uuid::Uuid::now_v7().as_bytes());
    value
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_resolution_progresses_past_unavailable_participants_and_receipts() {
    resolution_progress(2, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_resolution_releases_healthy_cells_while_first_rpc_is_pending() {
    // More participants than the resolution window also exercises replenishing
    // it while the first request stays pending, rather than waiting in batches.
    resolution_progress(6, true).await;
}

async fn resolution_progress(participant_count: usize, hold_first: bool) {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "resolution-progress".into(),
            cargo_lock_digest: Digest::from_bytes([1; 32]),
        })
        .unwrap(),
    );
    let account_id = "123456789012";
    let account = account_target(account_id).unwrap();
    let directory = tempfile::TempDir::new().unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("resolution-progress"),
        *account.application().as_bytes(),
    );
    let session = SessionId::from_bytes([246; 16]);
    let host = CellNodeBuilder::new(application.clone())
        .with_runtime(SqlWorkerPool::new(1, 16).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(Host::default().with_local_disk_budget(DiskBudget::new(1 << 30)))
        .with_session(session)
        .build_unleased_for_maintenance()
        .unwrap();
    let registry = application.registry();
    let provisioner = Arc::new(
        CellInitialPartitionProvisioner::new(
            host.runtime(),
            application,
            layout.clone(),
            session,
            "https://resolution.internal".into(),
            directory.path().into(),
        )
        .unwrap(),
    );
    provisioner.admit_account(account_id).await.unwrap();
    let client = CellClient::local_runtime(registry.clone(), host.runtime(), layout.clone());
    let local = CellStorage::new(client.clone(), "us-east-1");
    let routed =
        CellStorage::new(client.clone(), "us-east-1").with_initial_partitions(provisioner.clone());
    let mut participants = Vec::new();
    for index in 0..participant_count {
        let name = format!("Items{index}");
        let storage = if index == 0 { &local } else { &routed };
        let table = storage
            .create_table(
                account_id,
                serde_json::from_value(serde_json::json!({
                    "TableName": name, "KeySchema": [{"AttributeName": "id", "KeyType": "HASH"}],
                    "AttributeDefinitions": [{"AttributeName": "id", "AttributeType": "S"}],
                    "BillingMode": "PAY_PER_REQUEST"
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        let info = storage.table_key_info(account_id, &name).await.unwrap();
        let route = client
            .query::<ReadTableRoute>(&account, None, Json(table.table_id))
            .await
            .unwrap()
            .output
            .0;
        let (target, participant) = match route {
            Some(route) => {
                let spec = &route.partitions[0];
                (
                    data_target(account_id, &info.table_id, &spec.partition_id).unwrap(),
                    CoordinatorParticipantTarget::Data {
                        table_id: info.table_id.clone(),
                        partition_id: spec.partition_id,
                        epoch: spec.epoch,
                    },
                )
            }
            None => (account.clone(), CoordinatorParticipantTarget::Account),
        };
        participants.push((target, participant, info));
    }
    participants.sort_by_key(|(target, _, _)| *target.cell_id().as_bytes());
    let peer_session = SessionId::from_bytes([247; 16]);
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 8).unwrap(),
        16 * 1024 * 1024,
        peer_session,
    )
    .unwrap();
    let signer = PeerSigner::new(
        peer_session,
        registry.release_digest(),
        SigningKey::from_bytes(&[247; 32]),
    );
    let fault = Arc::new(AtomicU8::new(0));
    let pause = hold_first.then(|| Arc::new(ResolutionPause::default()));
    let transport = ResolutionFault {
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
        first: participants[0].0.clone(),
        fault: fault.clone(),
        pause: pause.clone(),
    };
    let remote = Arc::new(CellStorage::new(
        CellClient::runtime_with_peer(
            registry,
            runtime.clone(),
            layout,
            Arc::new(signer),
            PeerPrincipal {
                issuer: "resolution-test".into(),
                subject: "resolution".into(),
                actions: vec!["beyonddb.cell.invoke".into()],
            },
            Arc::new(transport),
        ),
        "us-east-1",
    ));

    // Each cut follows a durable decision. Refusing the first state read or
    // apply must not strand the second owner; a lost first receipt must not
    // stop later applies either. Run the same cuts for COMMIT and ABORT.
    for (case, (commit, cut)) in [true, false]
        .into_iter()
        .flat_map(|commit| (1..=3).map(move |cut| (commit, cut)))
        .enumerate()
    {
        let token = format!("resolution-{case}");
        let id = *uuid::Uuid::now_v7().as_bytes();
        let key = Item::from([("id".into(), AttributeValue::S(token.clone()))]);
        let mut proposed = key.clone();
        proposed.insert("value".into(), AttributeValue::S("prepared".into()));
        provisioner
            .admit_coordinator(account_id, token.as_bytes())
            .await
            .unwrap();
        let coordinator = coordinator_target(account_id, token.as_bytes()).unwrap();
        let request: Vec<_> = participants
            .iter()
            .enumerate()
            .map(|(index, (_, target, info))| CoordinatorParticipant {
                target: target.clone(),
                operations: vec![IndexedTransactionOperation {
                    index: index as u8,
                    operation: TransactionOperation::Put(PutItemInput {
                        table_name: info.table_name.clone(),
                        table_id: info.table_id.clone(),
                        item: proposed.clone(),
                        condition: None,
                    }),
                }],
            })
            .collect();
        transaction_command!(
            client,
            BeginCrossCellTransaction,
            &coordinator,
            mutation(),
            Json(BeginCrossCellTransactionInput {
                account_id: account_id.into(),
                transaction_id: id,
                token: Some(TransactionToken {
                    account_id: account_id.into(),
                    token: token.clone(),
                    fingerprint: token.clone()
                }),
                participants: request.clone(),
            }),
        )
        .await
        .unwrap();
        for (position, ((target, participant, _), request)) in
            participants.iter().zip(&request).enumerate()
        {
            let operations = request
                .operations
                .iter()
                .map(|operation| operation.operation.clone())
                .collect();
            let prepared = match participant {
                CoordinatorParticipantTarget::Account => transaction_command!(
                    client,
                    PrepareAccountTransaction,
                    target,
                    mutation(),
                    Json(PrepareAccountTransactionInput {
                        transaction_id: id,
                        coordinator_cell: *coordinator.cell_id().as_bytes(),
                        coordinator_key: token.as_bytes().to_vec(),
                        operations,
                    }),
                )
                .await
                .unwrap(),
                CoordinatorParticipantTarget::Data {
                    table_id, epoch, ..
                } => transaction_command!(
                    client,
                    PreparePartitionTransaction,
                    target,
                    mutation(),
                    Json(PreparePartitionTransactionInput {
                        table_id: table_id.clone(),
                        epoch: *epoch,
                        transaction_id: id,
                        coordinator_cell: *coordinator.cell_id().as_bytes(),
                        coordinator_key: token.as_bytes().to_vec(),
                        operations,
                    }),
                )
                .await
                .unwrap(),
            };
            client
                .command::<RecordParticipantPrepare>(
                    &coordinator,
                    mutation(),
                    Json(CoordinatorPhaseInput {
                        account_id: account_id.into(),
                        transaction_id: id,
                        routing_key: token.as_bytes().to_vec(),
                        position: position as u8,
                        participant_cell: *target.cell_id().as_bytes(),
                        sequence: prepared.receipt.commit_sequence,
                    }),
                )
                .await
                .unwrap();
        }
        let decision = if commit {
            CoordinatorDecision::Commit
        } else {
            CoordinatorDecision::Abort {
                index: None,
                reason: None,
            }
        };
        client
            .command::<DecideCrossCellTransaction>(
                &coordinator,
                mutation(),
                Json(DecideCrossCellTransactionInput {
                    account_id: account_id.into(),
                    transaction_id: id,
                    routing_key: token.as_bytes().to_vec(),
                    decision: decision.clone(),
                }),
            )
            .await
            .unwrap();
        fault.store(cut, Ordering::SeqCst);
        let resolver = remote.clone();
        let resolver_token = token.clone();
        let resolving = tokio::spawn(async move {
            resolver
                .resume_cross_cell_transaction(account_id, resolver_token.as_bytes(), id)
                .await
        });
        let resolving = if let Some(pause) = &pause {
            tokio::time::timeout(Duration::from_secs(10), pause.entered.notified())
                .await
                .expect("selected participant RPC must start");
            // Query raw coordinator progress: a public item read would help
            // recovery and conceal a resolver that still waits sequentially.
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let status = client
                        .query::<ReadCrossCellTransaction>(
                            &coordinator,
                            None,
                            Json(ReadCrossCellTransactionInput {
                                account_id: account_id.into(),
                                transaction_id: id,
                                routing_key: token.as_bytes().to_vec(),
                            }),
                        )
                        .await
                        .unwrap()
                        .output
                        .0
                        .unwrap();
                    if usize::from(status.resolved_count) == participant_count - 1 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("healthy participants must resolve before the stalled RPC returns");
            assert!(
                !resolving.is_finished(),
                "incomplete resolution cannot succeed"
            );
            Some(resolving)
        } else {
            assert!(
                matches!(resolving.await.unwrap(), Err(StorageError::Transient(_))),
                "incomplete resolution must remain retryable"
            );
            None
        };
        assert_eq!(fault.load(Ordering::SeqCst), 0, "fault must be exercised");
        let terminal = if commit {
            ParticipantTransactionState::Committed
        } else {
            ParticipantTransactionState::Aborted
        };
        for (position, (target, participant, _)) in participants.iter().enumerate() {
            let input = Json(ReadTransactionInput {
                transaction_id: id,
                coordinator_cell: *coordinator.cell_id().as_bytes(),
            });
            let state = match participant {
                CoordinatorParticipantTarget::Account => {
                    client
                        .query::<ReadAccountTransaction>(target, None, input)
                        .await
                        .unwrap()
                        .output
                        .0
                }
                CoordinatorParticipantTarget::Data { .. } => {
                    client
                        .query::<ReadPartitionTransaction>(target, None, input)
                        .await
                        .unwrap()
                        .output
                        .0
                }
            };
            assert_eq!(
                state,
                if position == 0 && cut != 3 {
                    ParticipantTransactionState::Prepared
                } else {
                    terminal.clone()
                },
                "healthy later participant must finish: commit={commit}, cut={cut}, position={position}"
            );
        }
        let status = client
            .query::<ReadCrossCellTransaction>(
                &coordinator,
                None,
                Json(ReadCrossCellTransactionInput {
                    account_id: account_id.into(),
                    transaction_id: id,
                    routing_key: token.as_bytes().to_vec(),
                }),
            )
            .await
            .unwrap()
            .output
            .0
            .unwrap();
        assert_eq!(
            (status.decision, status.resolved_count),
            (decision.clone(), (participant_count - 1) as u8)
        );
        let healthy_info = &participants[1].2;
        assert_eq!(
            local.get_item(healthy_info, &key).await.unwrap(),
            commit.then(|| proposed.clone())
        );
        // A newer write proves the healthy lock was released. Retrying the old
        // transaction must not apply its staged image over that newer value.
        let mut newer = key.clone();
        newer.insert("value".into(), AttributeValue::S("newer".into()));
        for (position, (_, _, info)) in participants.iter().enumerate() {
            if position == 0 && cut != 3 {
                continue;
            }
            // Include the first apply whose coordinator receipt was lost.
            // Its terminal participant record must prevent double apply too.
            local
                .put_item(
                    info,
                    newer.clone(),
                    false,
                    None,
                    &ExpressionMaps::default(),
                    None,
                )
                .await
                .unwrap();
        }
        if let Some(resolving) = resolving {
            if cut == 3 {
                // Losing the caller after apply but before its receipt must
                // leave the decision recoverable without overwriting newer data.
                resolving.abort();
                assert!(resolving.await.unwrap_err().is_cancelled());
            } else {
                pause.as_ref().unwrap().released.notify_one();
                assert!(matches!(
                    resolving.await.unwrap(),
                    Err(StorageError::Transient(_))
                ));
            }
        }
        assert_eq!(
            remote
                .resume_cross_cell_transaction(account_id, token.as_bytes(), id)
                .await
                .unwrap(),
            decision
        );
        assert_eq!(
            local.get_item(healthy_info, &key).await.unwrap(),
            Some(newer.clone())
        );
        let status = client
            .query::<ReadCrossCellTransaction>(
                &coordinator,
                None,
                Json(ReadCrossCellTransactionInput {
                    account_id: account_id.into(),
                    transaction_id: id,
                    routing_key: token.as_bytes().to_vec(),
                }),
            )
            .await
            .unwrap()
            .output
            .0
            .unwrap();
        assert_eq!(usize::from(status.resolved_count), participant_count);
        for (position, (_, _, info)) in participants.iter().enumerate() {
            assert_eq!(
                local.get_item(info, &key).await.unwrap(),
                if position > 0 || cut == 3 {
                    Some(newer.clone())
                } else {
                    commit.then(|| proposed.clone())
                }
            );
        }
    }
    runtime.shutdown().await.unwrap();
    host.shutdown().await.unwrap();
}

struct ResolutionFault {
    verifier: Arc<PeerVerifier>,
    dispatcher: Arc<PeerDispatcher>,
    first: CellTarget,
    fault: Arc<AtomicU8>,
    pause: Option<Arc<ResolutionPause>>,
}

#[derive(Default)]
struct ResolutionPause {
    entered: Notify,
    released: Notify,
}

impl PeerRoundTrip for ResolutionFault {
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
        let first = self.first.clone();
        let fault = self.fault.clone();
        let pause = self.pause.clone();
        Box::pin(async move {
            use crab_cell_runtime::peer::wire::{mutation_request, peer_request};
            use crab_cell_runtime::registry::Command;
            let now_ms = i64::try_from(
                std::time::SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis(),
            )
            .unwrap();
            let verified = verifier.verify(&request, now_ms)?;
            assert_eq!(verified.target(), &target);
            let command = match verified.operation() {
                Some(peer_request::Operation::Mutate(mutation)) => match &mutation.operation {
                    Some(mutation_request::Operation::CellCommand(command)) => Some(command),
                    _ => None,
                },
                _ => None,
            };
            let cut = fault.load(Ordering::SeqCst);
            let selected = match cut {
                1 => target == first,
                2 => {
                    target == first
                        && matches!(
                            command.map(|command| command.command_id),
                            Some(
                                <ResolvePartitionTransaction as Command>::ID
                                    | <beyonddb::ResolveAccountTransaction as Command>::ID
                            )
                        )
                }
                3 => command.is_some_and(|command| {
                    if command.command_id != RecordParticipantResolution::ID {
                        return false;
                    }
                    let mut decoder = BoundedDecoder::new(&command.input, 4096).unwrap();
                    let phase = Json::<CoordinatorPhaseInput>::decode(&mut decoder)
                        .unwrap()
                        .0;
                    phase.participant_cell == *first.cell_id().as_bytes()
                }),
                _ => false,
            };
            if selected
                && fault
                    .compare_exchange(cut, 0, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                if let Some(pause) = pause {
                    pause.entered.notify_one();
                    pause.released.notified().await;
                }
                return Err(crab_cell_runtime::Error::PeerTransportUnknown {
                    context: "injected resolution outage",
                    source: Box::new(std::io::Error::from(std::io::ErrorKind::ConnectionReset)),
                });
            }
            dispatcher.dispatch_bytes(&verified, now_ms).await
        })
    }
}
