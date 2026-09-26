use crate::*;
use beyonddb::{
    PrepareAccountTransaction, PrepareAccountTransactionInput, ReadAccountTransaction,
    ResolveAccountTransaction, TableRecord, TransactionFailure,
};
use extenddb_storage::{TransactGetOp, TransactWriteOp};

fn mutation() -> MutationIdentity {
    let mut value = crate::identity(211);
    value.request_id = RequestId::from_bytes(*uuid::Uuid::now_v7().as_bytes());
    value
}

fn key(value: &str) -> Item {
    Item::from([("id".into(), AttributeValue::S(value.into()))])
}

fn put(table: &TableRecord, value: &str) -> TransactionOperation {
    TransactionOperation::Put(PutItemInput {
        table_name: table.table_name.clone(),
        table_id: table.id.clone(),
        item: key(value),
        condition: None,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_participants_preserve_locks_and_finish_after_owner_restart() {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "account-participant".into(),
            cargo_lock_digest: Digest::from_bytes([1; 32]),
        })
        .unwrap(),
    );
    let account_id = "123456789012";
    let account = account_target(account_id).unwrap();
    let session = SessionId::from_bytes([211; 16]);
    let directory = tempfile::TempDir::new().unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("account-participant"),
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
    let account_handle = bootstrap
        .cell(
            &account,
            "beyonddb-account",
            211,
            &directory.path().join("account.sqlite"),
            initialize_account,
        )
        .await;
    let client = CellClient::local_runtime(registry.clone(), host.runtime(), layout.clone());
    let provisioner = Arc::new(
        CellInitialPartitionProvisioner::new(
            host.runtime(),
            application.clone(),
            layout.clone(),
            session,
            "https://beyonddb-partition.internal:8081".into(),
            directory.path().join("admitted"),
        )
        .unwrap(),
    );
    let storage =
        CellStorage::new(client.clone(), "us-east-1").with_transaction_coordinators(provisioner);
    let mut tables = Vec::new();
    for name in ["AccountItems", "DataItems", "OtherItems", "EmptyItems"] {
        let result = client
            .command::<CreateTable>(
                &account,
                mutation(),
                Json(TableSpec {
                    table_name: name.into(),
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
        let CreateTableOutcome::Created(table) = result.output.0 else {
            panic!("create table")
        };
        tables.push(table);
    }
    let table = &tables[0];
    let data_table = &tables[1];
    let empty_table = &tables[3];
    let data = data_target(account_id, &data_table.id, &[212; 16]).unwrap();
    let data_handle = bootstrap
        .cell(
            &data,
            "beyonddb-data",
            212,
            &directory.path().join("data.sqlite"),
            initialize_partition,
        )
        .await;
    let spec = PartitionSpec {
        table: data_table.clone(),
        partition_id: [212; 16],
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
                table_id: data_table.id.clone(),
                epoch: 1,
                partitions: vec![spec],
            }),
        )
        .await
        .unwrap();
    let info = storage
        .table_key_info(account_id, &table.table_name)
        .await
        .unwrap();
    let maps = ExpressionMaps::default();
    for value in ["deleted", "checked", "updated"] {
        storage
            .put_item(&info, key(value), false, None, &maps, None)
            .await
            .unwrap();
    }
    let operations = vec![
        put(table, "created"),
        TransactionOperation::Delete(DeleteItemInput { table_name: table.table_name.clone(), table_id: table.id.clone(), key: key("deleted"), condition: None }),
        serde_json::from_value(serde_json::json!({"ConditionCheck": {
            "table_name": table.table_name, "table_id": table.id, "key": key("checked"),
            "condition": {"expression": {"Function": {"name": "attribute_exists", "args": [{"Path": [{"Attribute": "id"}]}]}}, "maps": {"names": {}, "values": {}}}
        }})).unwrap(),
        serde_json::from_value(serde_json::json!({"Update": {
            "table_name": table.table_name, "table_id": table.id, "key": key("updated"), "condition": null,
            "update": {"actions": [{"Set": {"path": [{"Attribute": "value"}], "value": {"Placeholder": "v"}}}], "maps": {"names": {}, "values": {"v": {"S": "new"}}}}
        }})).unwrap(),
        put(empty_table, "created"),
    ];
    let transaction_id = [213; 16];
    let coordinator = coordinator_target(account_id, &transaction_id).unwrap();
    let coordinator_handle = bootstrap
        .cell(
            &coordinator,
            "beyonddb-coordinator",
            213,
            &directory.path().join("coordinator.sqlite"),
            initialize_coordinator,
        )
        .await;
    let account_participant = CoordinatorParticipant {
        target: CoordinatorParticipantTarget::Account,
        operations: operations
            .iter()
            .enumerate()
            .map(|(index, operation)| IndexedTransactionOperation {
                index: u8::try_from(index).unwrap(),
                operation: operation.clone(),
            })
            .collect(),
    };
    let data_participant = CoordinatorParticipant {
        target: CoordinatorParticipantTarget::Data {
            table_id: data_table.id.clone(),
            partition_id: [212; 16],
            epoch: 1,
        },
        operations: vec![IndexedTransactionOperation {
            index: 5,
            operation: put(data_table, "created"),
        }],
    };
    let mut participants = vec![
        (account.cell_id(), account_participant),
        (data.cell_id(), data_participant),
    ];
    participants.sort_by_key(|(cell, _)| *cell.as_bytes());
    transaction_command!(
        client,
        BeginCrossCellTransaction,
        &coordinator,
        mutation(),
        Json(BeginCrossCellTransactionInput {
            account_id: account_id.into(),
            transaction_id,
            token: None,
            participants: participants
                .into_iter()
                .map(|(_, participant)| participant)
                .collect(),
        }),
    )
    .await
    .unwrap();
    let prepare = PrepareAccountTransactionInput {
        transaction_id,
        coordinator_cell: *coordinator.cell_id().as_bytes(),
        coordinator_key: transaction_id.to_vec(),
        operations,
    };
    assert_eq!(
        transaction_command!(
            client,
            PrepareAccountTransaction,
            &account,
            mutation(),
            Json(prepare.clone())
        )
        .await
        .unwrap()
        .output
        .0,
        PrepareTransactionOutcome::Prepared
    );
    assert!(
        matches!(transaction_command!(client, PrepareAccountTransaction,&account, mutation(), Json(prepare.clone())).await,
        Err(InvocationError::Rejected(result)) if result.output.0 == PrepareTransactionOutcome::Replay)
    );
    let mut mismatch = prepare.clone();
    mismatch.operations[0] = put(table, "different");
    assert!(
        matches!(transaction_command!(client, PrepareAccountTransaction,&account, mutation(), Json(mismatch)).await,
        Err(InvocationError::Rejected(result)) if result.output.0 == PrepareTransactionOutcome::Mismatch)
    );
    for value in ["created", "deleted", "checked", "updated"] {
        assert!(matches!(
            storage.get_item(&info, &key(value)).await,
            Err(StorageError::Transient(_))
        ));
        assert!(matches!(
            storage
                .put_item(&info, key(value), false, None, &maps, None)
                .await,
            Err(StorageError::TransactionConflict(_))
        ));
        assert!(matches!(
            storage
                .delete_item(&info, &key(value), false, None, &maps, None)
                .await,
            Err(StorageError::TransactionConflict(_))
        ));
    }
    let update = [UpdateAction::Set {
        path: vec![PathElement::Attribute("other".into())],
        value: Expr::Placeholder("v".into()),
    }];
    let update_maps = ExpressionMaps::new(
        HashMap::new(),
        HashMap::from([("v".into(), AttributeValue::S("overwrite".into()))]),
    );
    assert!(matches!(
        storage
            .update_item(
                &info,
                &key("updated"),
                &update,
                false,
                false,
                None,
                &update_maps,
                None
            )
            .await,
        Err(StorageError::TransactionConflict(_))
    ));
    assert!(matches!(storage.transact_get_items(&[
        TransactGetOp { key_info: &info, key: &key("absent") },
        TransactGetOp { key_info: &info, key: &key("created") },
    ]).await, Err(StorageError::TransactionCanceled(reasons)) if reasons[0].code == "None" && reasons[1].code == "TransactionConflict"));
    assert!(matches!(storage.transact_write_items(&[
        TransactWriteOp::Put { key_info: &info, item: &key("rollback"), condition: None, maps: &maps, return_values_on_ccf: Default::default(), stream: None },
        TransactWriteOp::Put { key_info: &info, item: &key("created"), condition: None, maps: &maps, return_values_on_ccf: Default::default(), stream: None },
    ], None).await, Err(StorageError::TransactionCanceled(reasons)) if reasons[1].code == "TransactionConflict"));
    assert_eq!(
        storage.get_item(&info, &key("rollback")).await.unwrap(),
        None
    );
    assert!(matches!(
        storage.scan(&info, None, None, None, None, None).await,
        Err(StorageError::Transient(_))
    ));
    assert_eq!(
        storage
            .scan(&info, None, Some(&key("zz")), None, None, None)
            .await
            .unwrap(),
        (vec![], None)
    );
    let query = KeyCondition {
        pk_path: vec![PathElement::Attribute("id".into())],
        pk_value: Expr::Placeholder("id".into()),
        extra_pk_conditions: vec![],
        sk_condition: None,
        extra_sk_conditions: vec![],
    };
    let query_maps = ExpressionMaps::new(
        HashMap::new(),
        HashMap::from([("id".into(), AttributeValue::S("created".into()))]),
    );
    assert!(matches!(
        storage
            .query(&info, &query, &query_maps, true, None, None, None)
            .await,
        Err(StorageError::Transient(_))
    ));
    let other = storage
        .table_key_info(account_id, "OtherItems")
        .await
        .unwrap();
    storage
        .put_item(&other, key("created"), false, None, &maps, None)
        .await
        .unwrap();
    assert_eq!(
        storage.get_item(&other, &key("created")).await.unwrap(),
        Some(key("created"))
    );
    for name in ["AccountItems", "EmptyItems"] {
        assert!(
            matches!(client.command::<DeleteTable>(&account, mutation(), Json(name.into())).await,
            Err(InvocationError::Rejected(result)) if result.output.0 == DeleteTableOutcome::TransactionConflict)
        );
    }
    // The table is empty in the live image; only the prepared create fences activation.
    assert!(
        matches!(client.command::<ActivateTableRoute>(&account, mutation(), Json(TableRoute {
        table_id: empty_table.id.clone(), epoch: 1, partitions: vec![PartitionSpec {
            table: empty_table.clone(), partition_id: [219; 16], lower: None, upper: None, epoch: 1,
        }],
    })).await, Err(InvocationError::Rejected(result)) if result.output.0 == ActivateTableRouteOutcome::TransactionConflict)
    );
    for handle in [account_handle, data_handle, coordinator_handle] {
        handle.drain().await.unwrap();
    }
    host.shutdown().await.unwrap();

    let next_session = SessionId::from_bytes([214; 16]);
    let restored = CellNodeBuilder::new(application.clone())
        .with_runtime(SqlWorkerPool::new(1, 16).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(Host::default().with_local_disk_budget(DiskBudget::new(1 << 30)))
        .with_session(next_session)
        .build_unleased_for_maintenance()
        .unwrap();
    for (target, incarnation) in [(&account, 211), (&data, 212), (&coordinator, 213)] {
        let proof = CellCatalog::new(layout.clone(), target.tenant())
            .lookup(target.cell_id())
            .await
            .unwrap()
            .unwrap();
        let authority = CellAuthority::new(layout.clone());
        let idle = authority.load(target.cell_id()).await.unwrap().unwrap();
        restored
            .runtime()
            .acquire_idle_restored(
                proof,
                CellReplica::new(
                    layout.clone(),
                    *target.cell_id().as_bytes(),
                    [incarnation; 16],
                    Limits::default(),
                )
                .unwrap(),
                authority,
                idle,
                directory
                    .path()
                    .join(format!("restored-{incarnation}.sqlite")),
                Owner {
                    session: next_session,
                    endpoint: "https://restored.internal:8081".into(),
                },
            )
            .await
            .unwrap();
    }
    let client = CellClient::local_runtime(registry.clone(), restored.runtime(), layout.clone());
    let storage = CellStorage::new(client.clone(), "us-east-1");
    assert!(matches!(
        storage.get_item(&info, &key("created")).await,
        Err(StorageError::Transient(_))
    ));
    assert_eq!(
        client
            .query::<ReadAccountTransaction>(
                &account,
                None,
                Json(ReadTransactionInput {
                    transaction_id,
                    coordinator_cell: *coordinator.cell_id().as_bytes(),
                })
            )
            .await
            .unwrap()
            .output
            .0,
        ParticipantTransactionState::Prepared
    );
    let (first, second) = tokio::join!(
        storage.resume_cross_cell_transaction(account_id, &transaction_id, transaction_id),
        storage.resume_cross_cell_transaction(account_id, &transaction_id, transaction_id),
    );
    assert_eq!(first.unwrap(), CoordinatorDecision::Commit);
    assert_eq!(second.unwrap(), CoordinatorDecision::Commit);
    assert_eq!(
        storage.get_item(&info, &key("created")).await.unwrap(),
        Some(key("created"))
    );
    assert_eq!(
        storage.get_item(&info, &key("deleted")).await.unwrap(),
        None
    );
    let mut updated = key("updated");
    updated.insert("value".into(), AttributeValue::S("new".into()));
    assert_eq!(
        storage.get_item(&info, &key("updated")).await.unwrap(),
        Some(updated)
    );
    assert_eq!(
        client
            .query::<PartitionGet>(
                &data,
                None,
                Json(PartitionGetInput {
                    table_id: data_table.id.clone(),
                    epoch: 1,
                    key: key("created"),
                })
            )
            .await
            .unwrap()
            .output
            .0,
        PartitionGetOutcome::Found(Some(key("created")))
    );
    let resolve = ResolveTransactionInput {
        transaction_id,
        coordinator_cell: *coordinator.cell_id().as_bytes(),
        commit: false,
    };
    assert!(
        matches!(client.command::<ResolveAccountTransaction>(&account, mutation(), Json(resolve.clone())).await,
        Err(InvocationError::Rejected(result)) if result.output.0 == ResolveTransactionOutcome::DecisionConflict)
    );
    let abort = ResolveTransactionInput {
        transaction_id: [215; 16],
        ..resolve
    };
    client
        .command::<ResolveAccountTransaction>(&account, mutation(), Json(abort.clone()))
        .await
        .unwrap();
    let delayed = PrepareAccountTransactionInput {
        transaction_id: abort.transaction_id,
        ..prepare
    };
    assert!(
        matches!(transaction_command!(client, PrepareAccountTransaction,&account, mutation(), Json(delayed)).await,
        Err(InvocationError::Rejected(result)) if result.output.0 == PrepareTransactionOutcome::Aborted)
    );

    // A mixed request whose account condition fails must leave its data write absent.
    let failed_id = [216; 16];
    let failed_coordinator = coordinator_target(account_id, &failed_id).unwrap();
    let bootstrap = Bootstrap {
        runtime: restored.runtime(),
        registry: &registry,
        layout: &layout,
        session: next_session,
    };
    bootstrap
        .cell(
            &failed_coordinator,
            "beyonddb-coordinator",
            216,
            &directory.path().join("failed.sqlite"),
            initialize_coordinator,
        )
        .await;
    let mut failing = put(table, "created");
    let TransactionOperation::Put(input) = &mut failing else {
        unreachable!()
    };
    input.condition = Some(serde_json::from_value(serde_json::json!({
        "expression": {"Function": {"name": "attribute_not_exists", "args": [{"Path": [{"Attribute": "id"}]}]}}, "maps": {"names": {}, "values": {}}
    })).unwrap());
    let mut participants = vec![
        (
            account.cell_id(),
            CoordinatorParticipant {
                target: CoordinatorParticipantTarget::Account,
                operations: vec![IndexedTransactionOperation {
                    index: 1,
                    operation: failing,
                }],
            },
        ),
        (
            data.cell_id(),
            CoordinatorParticipant {
                target: CoordinatorParticipantTarget::Data {
                    table_id: data_table.id.clone(),
                    partition_id: [212; 16],
                    epoch: 1,
                },
                operations: vec![IndexedTransactionOperation {
                    index: 0,
                    operation: put(data_table, "rollback"),
                }],
            },
        ),
    ];
    participants.sort_by_key(|(cell, _)| *cell.as_bytes());
    transaction_command!(
        client,
        BeginCrossCellTransaction,
        &failed_coordinator,
        mutation(),
        Json(BeginCrossCellTransactionInput {
            account_id: account_id.into(),
            transaction_id: failed_id,
            token: None,
            participants: participants
                .into_iter()
                .map(|(_, participant)| participant)
                .collect(),
        }),
    )
    .await
    .unwrap();
    assert!(matches!(
        storage
            .resume_cross_cell_transaction(account_id, &failed_id, failed_id)
            .await
            .unwrap(),
        CoordinatorDecision::Abort {
            index: Some(1),
            reason: Some(TransactionFailure::ConditionFailed(Some(_)))
        }
    ));
    assert_eq!(
        client
            .query::<PartitionGet>(
                &data,
                None,
                Json(PartitionGetInput {
                    table_id: data_table.id.clone(),
                    epoch: 1,
                    key: key("rollback")
                })
            )
            .await
            .unwrap()
            .output
            .0,
        PartitionGetOutcome::Found(None)
    );
    // Aborting a prepared account participant preserves old images and releases
    // its item locks, including locks on missing rows.
    let prepared_abort = PrepareAccountTransactionInput {
        transaction_id: [217; 16],
        coordinator_cell: *coordinator.cell_id().as_bytes(),
        coordinator_key: vec![213; 16],
        operations: vec![
            put(table, "aborted-create"),
            TransactionOperation::Delete(DeleteItemInput {
                table_name: table.table_name.clone(),
                table_id: table.id.clone(),
                key: key("checked"),
                condition: None,
            }),
        ],
    };
    transaction_command!(
        client,
        PrepareAccountTransaction,
        &account,
        mutation(),
        Json(prepared_abort.clone())
    )
    .await
    .unwrap();
    let abort = ResolveTransactionInput {
        transaction_id: prepared_abort.transaction_id,
        coordinator_cell: prepared_abort.coordinator_cell,
        commit: false,
    };
    for _ in 0..2 {
        assert_eq!(
            client
                .command::<ResolveAccountTransaction>(&account, mutation(), Json(abort.clone()))
                .await
                .unwrap()
                .output
                .0,
            ResolveTransactionOutcome::Aborted
        );
    }
    assert_eq!(
        storage
            .get_item(&info, &key("aborted-create"))
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        storage.get_item(&info, &key("checked")).await.unwrap(),
        Some(key("checked"))
    );
    storage
        .put_item(&info, key("aborted-create"), false, None, &maps, None)
        .await
        .unwrap();
    let data_info = storage
        .table_key_info(account_id, &data_table.table_name)
        .await
        .unwrap();
    let admission = Arc::new(
        CellInitialPartitionProvisioner::new(
            restored.runtime(),
            application.clone(),
            layout.clone(),
            next_session,
            "https://restored.internal:8081".into(),
            directory.path().join("public-admission"),
        )
        .unwrap(),
    );
    super::public_transactions::assert_lost_replies_and_canceled_token_reuse(
        &restored,
        registry,
        layout,
        admission,
        [info, data_info],
    )
    .await;
    restored.shutdown().await.unwrap();
}
