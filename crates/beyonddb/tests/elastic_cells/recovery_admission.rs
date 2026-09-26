use crate::*;
use crab_storage::test_support::CountingObjectStore;

const ACCOUNT: &str = "123456789012";

fn mutation() -> MutationIdentity {
    MutationIdentity {
        request_id: RequestId::from_bytes(*uuid::Uuid::now_v7().as_bytes()),
        ..identity(249)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_resolves_healthy_participants_but_keeps_readiness_failed() {
    for decision in decisions() {
        recovery_case(true, decision).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serving_resolves_decisions_but_does_not_prepare_past_failed_admission() {
    for decision in decisions() {
        recovery_case(false, decision).await;
    }
}

fn decisions() -> [CoordinatorDecision; 3] {
    [
        CoordinatorDecision::Begin,
        CoordinatorDecision::Commit,
        CoordinatorDecision::Abort {
            index: None,
            reason: None,
        },
    ]
}

async fn recovery_case(startup: bool, decision: CoordinatorDecision) {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "recovery-admission".into(),
            cargo_lock_digest: Digest::from_bytes([1; 32]),
        })
        .unwrap(),
    );
    let account = account_target(ACCOUNT).unwrap();
    let directory = tempfile::TempDir::new().unwrap();
    let objects = Arc::new(CountingObjectStore::new(Arc::new(InMemory::new())));
    let layout = CellStorageLayout::new(
        Store::new(objects.clone()),
        object_store::path::Path::from("recovery-admission"),
        *account.application().as_bytes(),
    );
    let session = SessionId::from_bytes([249; 16]);
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
            "https://admission.internal".into(),
            directory.path().into(),
        )
        .unwrap(),
    );
    let account_handle = provisioner.admit_account(ACCOUNT).await.unwrap();
    let client = CellClient::local_runtime(registry.clone(), host.runtime(), layout.clone());
    let storage =
        CellStorage::new(client.clone(), "us-east-1").with_initial_partitions(provisioner.clone());
    let mut participants = Vec::new();
    for name in ["FirstTable", "SecondTable"] {
        let table = storage
            .create_table(
                ACCOUNT,
                serde_json::from_value(serde_json::json!({
                    "TableName": name, "KeySchema": [{"AttributeName": "id", "KeyType": "HASH"}],
                    "AttributeDefinitions": [{"AttributeName": "id", "AttributeType": "S"}],
                    "BillingMode": "PAY_PER_REQUEST"
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        let route = client
            .query::<ReadTableRoute>(&account, None, Json(table.table_id))
            .await
            .unwrap()
            .output
            .0
            .unwrap();
        let spec = &route.partitions[0];
        let target = data_target(ACCOUNT, &spec.table.id, &spec.partition_id).unwrap();
        participants.push((
            target,
            CoordinatorParticipant {
                target: CoordinatorParticipantTarget::Data {
                    table_id: spec.table.id.clone(),
                    partition_id: spec.partition_id,
                    epoch: spec.epoch,
                },
                operations: vec![IndexedTransactionOperation {
                    index: 0,
                    operation: TransactionOperation::Put(PutItemInput {
                        table_name: name.into(),
                        table_id: spec.table.id.clone(),
                        item: Item::from([("id".into(), AttributeValue::S("first".into()))]),
                        condition: None,
                    }),
                }],
            },
        ));
    }
    participants.sort_by_key(|(target, _)| *target.cell_id().as_bytes());
    for (index, (_, participant)) in participants.iter_mut().enumerate() {
        participant.operations[0].index = index as u8;
    }
    let id = [249; 16];
    provisioner.admit_coordinator(ACCOUNT, &id).await.unwrap();
    let coordinator = coordinator_target(ACCOUNT, &id).unwrap();
    let prepare_count = if !startup && decision == CoordinatorDecision::Begin {
        1
    } else {
        2
    };
    prepare(
        &client,
        &coordinator,
        id,
        &participants,
        &decision,
        prepare_count,
    )
    .await;
    // A second record in the same shard proves that startup's bounded pass
    // continues after a resolution error, as serving's cursor already does.
    let later_id = (1_u128..100_000)
        .map(u128::to_be_bytes)
        .find(|id| coordinator_target(ACCOUNT, id).unwrap() == coordinator)
        .unwrap();
    let mut later = participants[1].clone();
    later.1.operations[0].index = 0;
    let TransactionOperation::Put(input) = &mut later.1.operations[0].operation else {
        unreachable!()
    };
    input
        .item
        .insert("id".into(), AttributeValue::S("later".into()));
    prepare(&client, &coordinator, later_id, &[later], &decision, 1).await;

    // A fixed local handle lets assertions inspect the inaccessible owner's
    // durable state without removing the control-store fault or helping reads.
    let authority = CellAuthority::new(layout.clone());
    let control = authority
        .load(participants[0].0.cell_id())
        .await
        .unwrap()
        .unwrap();
    let proof = CellCatalog::new(layout.clone(), account.tenant())
        .lookup(participants[0].0.cell_id())
        .await
        .unwrap()
        .unwrap();
    let handle = host
        .runtime()
        .local_handle(proof, &control)
        .await
        .unwrap()
        .unwrap();
    let first_client = CellClient::local(registry.clone(), handle);
    let nodes = NodeDirectory::new(
        layout.clone(),
        Digest::from_bytes([201; 32]),
        Digest::from_bytes([202; 32]),
        registry.release_digest(),
    );
    let blocked = layout.control_path(participants[0].0.cell_id().as_bytes());
    objects.reset();
    objects.block_body_reads_for(&blocked);
    if startup {
        assert!(
            provisioner
                .recover_registered_account(
                    ACCOUNT,
                    account_handle.clone(),
                    &client,
                    &storage,
                    &nodes
                )
                .await
                .is_err(),
            "startup cannot report ready after failed participant admission"
        );
    } else {
        let tasks = host
            .install_task_group(CancellationToken::new(), CancellationToken::new())
            .unwrap();
        provisioner
            .install_transaction_recovery_loop(
                &tasks,
                CellStorage::new(client.clone(), "us-east-1"),
                nodes.clone(),
                vec![],
            )
            .unwrap();
        wait_resolved(&client, later_id, 1).await;
    }
    assert!(
        objects
            .requests()
            .iter()
            .any(|read| read.location == blocked.as_ref()),
        "the admission fault must be exercised"
    );
    let pending = status(&client, id).await;
    let expected = if startup && decision == CoordinatorDecision::Begin {
        CoordinatorDecision::Abort {
            index: None,
            reason: None,
        }
    } else {
        decision.clone()
    };
    let terminal = expected != CoordinatorDecision::Begin;
    assert_eq!(
        (pending.decision, pending.resolved_count),
        (expected.clone(), u8::from(terminal)),
        "startup={startup}, decision={decision:?}"
    );
    for (position, (target, _)) in participants.iter().enumerate() {
        let observer = if position == 0 {
            &first_client
        } else {
            &client
        };
        let state = observer
            .query::<ReadPartitionTransaction>(
                target,
                None,
                Json(ReadTransactionInput {
                    transaction_id: id,
                    coordinator_cell: *coordinator.cell_id().as_bytes(),
                }),
            )
            .await
            .unwrap()
            .output
            .0;
        let expected_state = if position == 0 {
            ParticipantTransactionState::Prepared
        } else if !terminal {
            ParticipantTransactionState::Missing
        } else if expected == CoordinatorDecision::Commit {
            ParticipantTransactionState::Committed
        } else {
            ParticipantTransactionState::Aborted
        };
        assert_eq!(
            state, expected_state,
            "healthy participant must progress independently"
        );
    }
    let later_status = status(&client, later_id).await;
    assert_eq!(
        later_status.resolved_count, 1,
        "later records must not remain blocked"
    );
    // Write after healthy resolution; completing the old transaction later
    // must preserve this newer value instead of applying its staged image twice.
    let (healthy_target, healthy) = &participants[1];
    let TransactionOperation::Put(input) = &healthy.operations[0].operation else {
        unreachable!()
    };
    let CoordinatorParticipantTarget::Data { epoch, .. } = healthy.target else {
        unreachable!()
    };
    let mut newer = input.item.clone();
    newer.insert("value".into(), AttributeValue::S("newer".into()));
    if terminal {
        let image = client
            .query::<PartitionGet>(
                healthy_target,
                None,
                Json(PartitionGetInput {
                    table_id: input.table_id.clone(),
                    epoch,
                    key: input.item.clone(),
                }),
            )
            .await
            .unwrap()
            .output
            .0;
        assert_eq!(
            image,
            PartitionGetOutcome::Found(
                (expected == CoordinatorDecision::Commit).then(|| input.item.clone())
            )
        );
        client
            .command::<PartitionPut>(
                healthy_target,
                mutation(),
                Json(PartitionPutInput {
                    table_id: input.table_id.clone(),
                    epoch,
                    item: newer.clone(),
                    condition: None,
                }),
            )
            .await
            .unwrap();
    }
    objects.unblock_body_reads_for(&blocked);
    if startup {
        provisioner
            .recover_registered_account(ACCOUNT, account_handle, &client, &storage, &nodes)
            .await
            .unwrap();
    } else {
        wait_resolved(&client, id, 2).await;
    }
    let completed = status(&client, id).await;
    let final_decision = if expected == CoordinatorDecision::Begin {
        CoordinatorDecision::Commit
    } else {
        expected
    };
    assert_eq!(
        (completed.decision, completed.resolved_count),
        (final_decision.clone(), 2)
    );
    if terminal {
        assert_eq!(
            client
                .query::<PartitionGet>(
                    healthy_target,
                    None,
                    Json(PartitionGetInput {
                        table_id: input.table_id.clone(),
                        epoch,
                        key: input.item.clone(),
                    })
                )
                .await
                .unwrap()
                .output
                .0,
            PartitionGetOutcome::Found(Some(newer))
        );
    }
    host.shutdown().await.unwrap();
}

async fn prepare(
    client: &CellClient,
    coordinator: &CellTarget,
    id: [u8; 16],
    participants: &[(CellTarget, CoordinatorParticipant)],
    decision: &CoordinatorDecision,
    prepare_count: usize,
) {
    transaction_command!(
        client,
        BeginCrossCellTransaction,
        coordinator,
        mutation(),
        Json(BeginCrossCellTransactionInput {
            account_id: ACCOUNT.into(),
            transaction_id: id,
            token: None,
            participants: participants
                .iter()
                .map(|(_, participant)| participant.clone())
                .collect(),
        }),
    )
    .await
    .unwrap();
    for (position, (target, participant)) in participants.iter().take(prepare_count).enumerate() {
        let CoordinatorParticipantTarget::Data {
            table_id, epoch, ..
        } = &participant.target
        else {
            unreachable!()
        };
        let result = transaction_command!(
            client,
            PreparePartitionTransaction,
            target,
            mutation(),
            Json(PreparePartitionTransactionInput {
                table_id: table_id.clone(),
                epoch: *epoch,
                transaction_id: id,
                coordinator_cell: *coordinator.cell_id().as_bytes(),
                coordinator_key: id.to_vec(),
                operations: participant
                    .operations
                    .iter()
                    .map(|op| op.operation.clone())
                    .collect(),
            }),
        )
        .await
        .unwrap();
        client
            .command::<RecordParticipantPrepare>(
                coordinator,
                mutation(),
                Json(CoordinatorPhaseInput {
                    account_id: ACCOUNT.into(),
                    transaction_id: id,
                    routing_key: id.to_vec(),
                    position: position as u8,
                    participant_cell: *target.cell_id().as_bytes(),
                    sequence: result.receipt.commit_sequence,
                }),
            )
            .await
            .unwrap();
    }
    if *decision != CoordinatorDecision::Begin {
        client
            .command::<DecideCrossCellTransaction>(
                coordinator,
                mutation(),
                Json(DecideCrossCellTransactionInput {
                    account_id: ACCOUNT.into(),
                    transaction_id: id,
                    routing_key: id.to_vec(),
                    decision: decision.clone(),
                }),
            )
            .await
            .unwrap();
    }
}

async fn status(client: &CellClient, id: [u8; 16]) -> beyonddb::CrossCellTransactionStatus {
    client
        .query::<ReadCrossCellTransaction>(
            &coordinator_target(ACCOUNT, &id).unwrap(),
            None,
            Json(ReadCrossCellTransactionInput {
                account_id: ACCOUNT.into(),
                transaction_id: id,
                routing_key: id.to_vec(),
            }),
        )
        .await
        .unwrap()
        .output
        .0
        .unwrap()
}

async fn wait_resolved(client: &CellClient, id: [u8; 16], count: u8) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if status(client, id).await.resolved_count == count {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
}
