use crate::*;
use beyonddb::{PrepareAccountTransaction, PrepareAccountTransactionInput};

fn mutation() -> MutationIdentity {
    MutationIdentity {
        request_id: RequestId::from_bytes(*uuid::Uuid::now_v7().as_bytes()),
        ..identity(250)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn committed_participants_reuse_staged_space_after_unrelated_writes() {
    capacity_case(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn committed_participants_resolve_after_unrelated_writes_exhaust_capacity() {
    capacity_case(true).await;
}

async fn capacity_case(exhaust: bool) {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "transaction-capacity".into(),
            cargo_lock_digest: Digest::from_bytes([1; 32]),
        })
        .unwrap(),
    );
    let account_id = "123456789012";
    let account = account_target(account_id).unwrap();
    let session = SessionId::from_bytes([250; 16]);
    let directory = tempfile::tempdir().unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("transaction-capacity"),
        *account.application().as_bytes(),
    );
    // Exercise the compiled application handlers with a smaller runtime storage
    // budget. Production Cell type declarations remain at 512 MiB.
    let runtime = CellRuntime::new_with_replica_host(
        SqlWorkerPool::new(1, 8).unwrap(),
        16 * 1024 * 1024,
        session,
        Host::default().with_local_disk_budget(DiskBudget::new(1 << 30)),
    )
    .unwrap();
    let registry = application.registry();
    let bootstrap = Bootstrap {
        runtime: runtime.clone(),
        registry: &registry,
        layout: &layout,
        session,
    };
    let limits = Limits {
        max_database_bytes: 2 * 1024 * 1024,
        max_capture_bytes: 1024 * 1024,
        ..Limits::default()
    };
    let account_file = directory.path().join("account.sqlite");
    bootstrap
        .cell_with_storage(
            &account,
            "beyonddb-account",
            250,
            (&account_file, limits),
            initialize_account,
        )
        .await;
    let client = CellClient::local_runtime(registry.clone(), runtime.clone(), layout.clone());
    let storage = CellStorage::new(client.clone(), "us-east-1");
    let mut tables = Vec::new();
    for name in ["CapacityAccount", "CapacityData"] {
        storage
            .create_table(
                account_id,
                serde_json::from_value(serde_json::json!({
                    "TableName": name,
                    "KeySchema": [{"AttributeName": "id", "KeyType": "HASH"}],
                    "AttributeDefinitions": [{"AttributeName": "id", "AttributeType": "S"}],
                    "BillingMode": "PAY_PER_REQUEST"
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        tables.push(
            client
                .query::<DescribeTable>(&account, None, Json(name.into()))
                .await
                .unwrap()
                .output
                .0
                .unwrap(),
        );
    }
    let data = data_target(account_id, &tables[1].id, &[251; 16]).unwrap();
    let data_file = directory.path().join("data.sqlite");
    bootstrap
        .cell_with_storage(
            &data,
            "beyonddb-data",
            251,
            (&data_file, limits),
            initialize_partition,
        )
        .await;
    let spec = PartitionSpec {
        table: tables[1].clone(),
        partition_id: [251; 16],
        lower: None,
        upper: None,
        epoch: 1,
    };
    client
        .command::<InstallPartition>(
            &data,
            mutation(),
            Json(PartitionInstall::Serving(spec.clone())),
        )
        .await
        .unwrap();
    client
        .command::<ActivateTableRoute>(
            &account,
            mutation(),
            Json(TableRoute {
                table_id: tables[1].id.clone(),
                epoch: 1,
                partitions: vec![spec],
            }),
        )
        .await
        .unwrap();
    let transaction_id = [252; 16];
    let coordinator = coordinator_target(account_id, &transaction_id).unwrap();
    bootstrap
        .cell(
            &coordinator,
            "beyonddb-coordinator",
            252,
            &directory.path().join("coordinator.sqlite"),
            initialize_coordinator,
        )
        .await;
    let targets = [&account, &data];
    let operations: Vec<Vec<_>> = tables
        .iter()
        .map(|table| {
            (0..4)
                .map(|index| {
                    TransactionOperation::Put(PutItemInput {
                        table_name: table.table_name.clone(),
                        table_id: table.id.clone(),
                        item: Item::from([
                            ("id".into(), AttributeValue::S(format!("committed-{index}"))),
                            ("payload".into(), AttributeValue::S("x".repeat(380 * 1024))),
                        ]),
                        condition: None,
                    })
                })
                .collect()
        })
        .collect();
    let mut participants: Vec<_> = [
        CoordinatorParticipantTarget::Account,
        CoordinatorParticipantTarget::Data {
            table_id: tables[1].id.clone(),
            partition_id: [251; 16],
            epoch: 1,
        },
    ]
    .into_iter()
    .enumerate()
    .map(|(position, target)| CoordinatorParticipant {
        target,
        operations: operations[position]
            .iter()
            .enumerate()
            .map(|(index, operation)| IndexedTransactionOperation {
                index: (position * 4 + index) as u8,
                operation: operation.clone(),
            })
            .collect(),
    })
    .collect();
    participants.sort_by_key(|participant| {
        *targets[usize::from(participant.operations[0].index) / 4]
            .cell_id()
            .as_bytes()
    });
    transaction_command!(
        client,
        BeginCrossCellTransaction,
        &coordinator,
        mutation(),
        Json(BeginCrossCellTransactionInput {
            account_id: account_id.into(),
            transaction_id,
            token: None,
            participants: participants.clone()
        })
    )
    .await
    .unwrap();
    let coordinator_cell = *coordinator.cell_id().as_bytes();
    for (position, participant) in participants.iter().enumerate() {
        let index = usize::from(participant.operations[0].index) / 4;
        let prepared = if index == 0 {
            transaction_command!(
                client,
                PrepareAccountTransaction,
                &account,
                mutation(),
                Json(PrepareAccountTransactionInput {
                    transaction_id,
                    coordinator_cell,
                    coordinator_key: transaction_id.to_vec(),
                    operations: operations[0].clone(),
                })
            )
            .await
            .unwrap()
        } else {
            transaction_command!(
                client,
                PreparePartitionTransaction,
                &data,
                mutation(),
                Json(PreparePartitionTransactionInput {
                    table_id: tables[1].id.clone(),
                    epoch: 1,
                    transaction_id,
                    coordinator_cell,
                    coordinator_key: transaction_id.to_vec(),
                    operations: operations[1].clone(),
                })
            )
            .await
            .unwrap()
        };
        client
            .command::<RecordParticipantPrepare>(
                &coordinator,
                mutation(),
                Json(CoordinatorPhaseInput {
                    account_id: account_id.into(),
                    transaction_id,
                    routing_key: transaction_id.to_vec(),
                    position: position as u8,
                    participant_cell: *targets[index].cell_id().as_bytes(),
                    sequence: prepared.receipt.commit_sequence,
                }),
            )
            .await
            .unwrap();
    }
    for table in &tables {
        let info = storage
            .table_key_info(account_id, &table.table_name)
            .await
            .unwrap();
        storage
            .put_item(
                &info,
                Item::from([
                    ("id".into(), AttributeValue::S("unrelated".into())),
                    ("payload".into(), AttributeValue::S("y".repeat(100 * 1024))),
                ]),
                false,
                None,
                &ExpressionMaps::default(),
                None,
            )
            .await
            .unwrap();
    }
    if exhaust {
        for table in &tables {
            let info = storage
                .table_key_info(account_id, &table.table_name)
                .await
                .unwrap();
            let mut refused = false;
            for i in 0..128 {
                let result = storage
                    .put_item(
                        &info,
                        Item::from([
                            ("id".into(), AttributeValue::S(format!("fill-{i}"))),
                            ("payload".into(), AttributeValue::S("z".repeat(8 * 1024))),
                        ]),
                        false,
                        None,
                        &ExpressionMaps::default(),
                        None,
                    )
                    .await;
                if let Err(error) = result {
                    assert!(
                        matches!(error, StorageError::Transient(_)),
                        "capacity refusal must be retryable: {error:?}"
                    );
                    refused = true;
                    break;
                }
            }
            assert!(refused, "fixture must exhaust the Cell budget");
        }
    }
    client
        .command::<DecideCrossCellTransaction>(
            &coordinator,
            mutation(),
            Json(DecideCrossCellTransactionInput {
                account_id: account_id.into(),
                transaction_id,
                routing_key: transaction_id.to_vec(),
                decision: CoordinatorDecision::Commit,
            }),
        )
        .await
        .unwrap();
    storage
        .finish_decided_cross_cell_transaction(account_id, &transaction_id, transaction_id)
        .await
        .unwrap();
    for (index, table) in tables.iter().enumerate() {
        let info = storage
            .table_key_info(account_id, &table.table_name)
            .await
            .unwrap();
        for operation in &operations[index] {
            let TransactionOperation::Put(input) = operation else {
                unreachable!()
            };
            let key = Item::from([("id".into(), input.item["id"].clone())]);
            assert_eq!(
                storage.get_item(&info, &key).await.unwrap(),
                Some(input.item.clone())
            );
        }
    }
    runtime.shutdown().await.unwrap();
}
