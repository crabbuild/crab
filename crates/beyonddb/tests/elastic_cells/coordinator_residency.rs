use crate::*;
use beyonddb::{
    ReadAccountTransaction, ReadAccountTransactionResult, ReadTransactionResultInput,
    TransactionReadResult,
};
use extenddb_core::types::TableKeyInfo;

const ACCOUNT: &str = "123456789012";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capacity_sweep_survives_admission_backpressure() {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "capacity-backpressure".into(),
            cargo_lock_digest: Digest::from_bytes([1; 32]),
        })
        .unwrap(),
    );
    let directory = tempfile::TempDir::new().unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("capacity-backpressure"),
        *account_target(ACCOUNT).unwrap().application().as_bytes(),
    );
    let session = SessionId::from_bytes([244; 16]);
    let host = CellNodeBuilder::new(application.clone())
        .with_runtime(SqlWorkerPool::new(1, 2).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(Host::default().with_local_disk_budget(DiskBudget::new(1 << 30)))
        .with_session(session)
        .build()
        .unwrap();
    let tasks = host
        .install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    let lease = published_test_node_lease(&layout, session).await;
    host.install_node_lease(lease.guard()).unwrap();
    let provisioner = Arc::new(
        CellInitialPartitionProvisioner::new(
            host.runtime(),
            application.clone(),
            layout.clone(),
            session,
            "https://beyonddb-sort-query.internal:8081".into(),
            directory.path().into(),
        )
        .unwrap(),
    );
    let account = provisioner.admit_account(ACCOUNT).await.unwrap();
    let client = CellClient::local_runtime(application.registry(), host.runtime(), layout);
    let storage =
        CellStorage::new(client, "us-east-1").with_initial_partitions(provisioner.clone());
    let table = storage
        .create_table(
            ACCOUNT,
            serde_json::from_value(serde_json::json!({
                "TableName": "Pressure",
                "KeySchema": [{"AttributeName": "id", "KeyType": "HASH"}],
                "AttributeDefinitions": [{"AttributeName": "id", "AttributeType": "S"}],
                "BillingMode": "PAY_PER_REQUEST"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    // The account and source occupy both slots: splitting cannot admit a child.
    assert!(matches!(
        provisioner
            .reconcile_account_capacity(ACCOUNT, account.clone(), 1, None)
            .await,
        Err(StorageError::Transient(_))
    ));
    provisioner
        .install_account_capacity_loop(
            &tasks,
            ACCOUNT.into(),
            account.clone(),
            1,
            Duration::from_millis(20),
        )
        .unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        host.is_ready(),
        "temporary capacity pressure stopped serving"
    );
    let pending = CellClient::local(application.registry(), account)
        .query::<ReadSplitPlan>(
            &account_target(ACCOUNT).unwrap(),
            None,
            Json(table.table_id),
        )
        .await
        .unwrap();
    assert!(pending.output.0.is_some(), "split must remain recoverable");
    host.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coordinator_history_outgrows_residency_and_released_read_recovers() {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "coordinator-residency".into(),
            cargo_lock_digest: Digest::from_bytes([1; 32]),
        })
        .unwrap(),
    );
    let directory = tempfile::TempDir::new().unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("coordinator-residency"),
        *account_target(ACCOUNT).unwrap().application().as_bytes(),
    );
    let session = SessionId::from_bytes([245; 16]);
    let host = CellNodeBuilder::new(application.clone())
        .with_runtime(SqlWorkerPool::new(1, 3).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(Host::default().with_local_disk_budget(DiskBudget::new(1 << 30)))
        .with_session(session)
        .build_unleased_for_maintenance()
        .unwrap();
    let provisioner = Arc::new(
        CellInitialPartitionProvisioner::new(
            host.runtime(),
            application.clone(),
            layout.clone(),
            session,
            "http://residency.internal".into(),
            directory.path().into(),
        )
        .unwrap(),
    );
    provisioner.admit_account(ACCOUNT).await.unwrap();
    let client = CellClient::local_runtime(application.registry(), host.runtime(), layout.clone());
    let account = account_target(ACCOUNT).unwrap();
    client
        .command::<CreateTable>(
            &account,
            identity(245),
            Json(TableSpec {
                table_name: "Residency".into(),
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
                initial_tags: vec![],
                resource_arn: None,
            }),
        )
        .await
        .unwrap();
    let storage = CellStorage::new(client.clone(), "us-east-1")
        .with_transaction_coordinators(provisioner.clone());
    let info = storage.table_key_info(ACCOUNT, "Residency").await.unwrap();
    let mut seen = std::collections::HashSet::new();
    let tokens: Vec<_> = (0..100)
        .map(|n| format!("residency-{n}"))
        .filter(|token| {
            seen.insert(
                coordinator_target(ACCOUNT, token.as_bytes())
                    .unwrap()
                    .cell_id(),
            )
        })
        .take(12)
        .collect();
    assert_eq!(tokens.len(), 12);
    for (index, token) in tokens.iter().enumerate() {
        assert!(!write(&storage, &info, token, index).await);
        assert!(host.runtime().stats().active_cells() <= 3);
    }
    // Data admission must also reclaim completed coordinators at the same pool limit.
    let data_storage =
        CellStorage::new(client.clone(), "us-east-1").with_initial_partitions(provisioner.clone());
    let input: CreateTableInput = serde_json::from_value(serde_json::json!({
        "TableName": "AfterHistory",
        "KeySchema": [{"AttributeName": "id", "KeyType": "HASH"}],
        "AttributeDefinitions": [{"AttributeName": "id", "AttributeType": "S"}],
        "BillingMode": "PAY_PER_REQUEST"
    }))
    .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        match data_storage.create_table(ACCOUNT, input.clone()).await {
            Ok(table) => {
                assert_eq!(table.table_status, TableStatus::Active);
                break;
            }
            Err(StorageError::Transient(_)) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await
            }
            Err(error) => panic!("data admission failed: {error}"),
        }
    }
    // The first receipt must restore from object storage, not execute the old
    // write again over the value installed by a later transaction.
    let oldest = coordinator_target(ACCOUNT, tokens[0].as_bytes()).unwrap();
    assert!(
        CellAuthority::new(layout.clone())
            .load(oldest.cell_id())
            .await
            .unwrap()
            .unwrap()
            .value()
            .owner
            .is_none()
    );
    let (first, concurrent) = tokio::join!(
        write(&storage, &info, &tokens[0], 0),
        write(&storage, &info, &tokens[0], 0)
    );
    assert!(first && concurrent);
    let key = Item::from([("id".into(), AttributeValue::S("shared".into()))]);
    let expected = storage.get_item(&info, &key).await.unwrap().unwrap();
    assert_eq!(
        expected.get("version"),
        Some(&AttributeValue::N("11".into()))
    );

    let id = (1_u128..100_000)
        .map(u128::to_be_bytes)
        .find(|id| coordinator_target(ACCOUNT, id).unwrap() == oldest)
        .unwrap();
    transaction_command!(
        client,
        BeginCrossCellTransaction,
        &oldest,
        identity(246),
        Json(BeginCrossCellTransactionInput {
            account_id: ACCOUNT.into(),
            transaction_id: id,
            token: None,
            participants: vec![CoordinatorParticipant {
                target: CoordinatorParticipantTarget::Account,
                operations: vec![IndexedTransactionOperation {
                    index: 0,
                    operation: TransactionOperation::Read(GetItemInput {
                        table_name: info.table_name.clone(),
                        table_id: info.table_id.clone(),
                        key: key.clone(),
                    }),
                }],
            }],
        }),
    )
    .await
    .unwrap();
    transaction_command!(
        client,
        beyonddb::PrepareAccountTransaction,
        &account,
        identity(247),
        Json(beyonddb::PrepareAccountTransactionInput {
            transaction_id: id,
            coordinator_cell: *oldest.cell_id().as_bytes(),
            coordinator_key: id.to_vec(),
            operations: vec![TransactionOperation::Read(GetItemInput {
                table_name: info.table_name.clone(),
                table_id: info.table_id.clone(),
                key: key.clone(),
            })],
        }),
    )
    .await
    .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let candidate = host
            .runtime()
            .idle_transfer_candidates()
            .await
            .unwrap()
            .into_iter()
            .find(|(cell, _, _, _)| *cell == oldest.cell_id());
        if let Some((cell, generation, _, _)) = candidate {
            match host
                .runtime()
                .release_idle_cell(cell, session, generation)
                .await
            {
                Ok(()) => break,
                Err(
                    crab_cell_runtime::Error::Capacity(_) | crab_cell_runtime::Error::CellDraining,
                ) => {}
                Err(error) => panic!("unexpected release failure: {error}"),
            }
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let tasks = host
        .install_task_group(CancellationToken::new(), CancellationToken::new())
        .unwrap();
    provisioner
        .install_transaction_recovery_loop(&tasks, CellStorage::new(client.clone(), "us-east-1"))
        .unwrap();
    let read = ReadTransactionInput {
        transaction_id: id,
        coordinator_cell: *oldest.cell_id().as_bytes(),
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let state = client
            .query::<ReadAccountTransaction>(&account, None, Json(read.clone()))
            .await
            .unwrap()
            .output
            .0;
        if state == ParticipantTransactionState::Committed {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "released read was not recovered: {state:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        client
            .query::<ReadAccountTransactionResult>(
                &account,
                None,
                Json(ReadTransactionResultInput {
                    transaction: read,
                    position: 0
                })
            )
            .await
            .unwrap()
            .output
            .0,
        TransactionReadResult::Item(Some(expected))
    );
    tasks.cancellation_token().cancel();
    host.shutdown().await.unwrap();
}

async fn write(storage: &CellStorage, info: &TableKeyInfo, token: &str, version: usize) -> bool {
    let item = Item::from([
        ("id".into(), AttributeValue::S("shared".into())),
        ("version".into(), AttributeValue::N(version.to_string())),
    ]);
    let maps = ExpressionMaps::default();
    let ops = [TransactWriteOp::Put {
        key_info: info,
        item: &item,
        condition: None,
        maps: &maps,
        return_values_on_ccf: Default::default(),
        stream: None,
    }];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        match storage
            .transact_write_items(
                &ops,
                Some(IdempotencyKey {
                    account_id: ACCOUNT,
                    token,
                    fingerprint: token,
                }),
            )
            .await
        {
            Ok(()) => return false,
            Err(StorageError::IdempotentReplay) => return true,
            Err(StorageError::Transient(_)) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await
            }
            Err(error) => panic!("transaction failed: {error}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn table_creation_waits_for_coordinator_movement_capacity() {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "table-admission-pressure".into(),
            cargo_lock_digest: Digest::from_bytes([1; 32]),
        })
        .unwrap(),
    );
    let directory = tempfile::tempdir().unwrap();
    let account = account_target(ACCOUNT).unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("table-admission-pressure"),
        *account.application().as_bytes(),
    );
    let session = SessionId::from_bytes([248; 16]);
    let host = CellNodeBuilder::new(application.clone())
        .with_runtime(SqlWorkerPool::new(1, 8).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(Host::default().with_local_disk_budget(DiskBudget::new(1 << 30)))
        .with_session(session)
        .build_unleased_for_maintenance()
        .unwrap();
    let provisioner = Arc::new(
        CellInitialPartitionProvisioner::new(
            host.runtime(),
            application.clone(),
            layout.clone(),
            session,
            "https://table-admission.internal".into(),
            directory.path().into(),
        )
        .unwrap()
        .with_initial_partition_count(4)
        .unwrap(),
    );
    provisioner.admit_account(ACCOUNT).await.unwrap();
    let mut seen = std::collections::HashSet::new();
    for n in 0_u64..100 {
        let key = n.to_be_bytes();
        if seen.insert(
            *coordinator_target(ACCOUNT, &key)
                .unwrap()
                .cell_id()
                .as_bytes(),
        ) {
            provisioner.admit_coordinator(ACCOUNT, &key).await.unwrap();
        }
        if seen.len() == 7 {
            break;
        }
    }
    assert_eq!(host.runtime().stats().active_cells(), 8);
    let client = CellClient::local_runtime(application.registry(), host.runtime(), layout);
    let storage =
        CellStorage::new(client.clone(), "us-east-1").with_initial_partitions(provisioner);
    // Four ranges require four coordinator releases. The runtime permits two
    // movements per second; ordinary admission must let that budget replenish.
    let table = storage
        .create_table(
            ACCOUNT,
            serde_json::from_value(serde_json::json!({
                "TableName": "AfterHistory",
                "KeySchema": [{"AttributeName": "id", "KeyType": "HASH"}],
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
    assert_eq!(route.partitions.len(), 4);
    host.shutdown().await.unwrap();
}
