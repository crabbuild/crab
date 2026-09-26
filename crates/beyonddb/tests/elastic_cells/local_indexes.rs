use crate::*;
use extenddb_core::types::TableKeyInfo;

fn item(sk: &str, score: Option<&str>) -> Item {
    let mut item = Item::from([
        ("pk".into(), AttributeValue::S("same".into())),
        ("sk".into(), AttributeValue::N(sk.into())),
    ]);
    if let Some(score) = score {
        item.insert("score".into(), AttributeValue::N(score.into()));
    }
    item
}

async fn indexed(storage: &CellStorage, base: &TableKeyInfo) -> TableKeyInfo {
    let index = storage
        .index_info(&base.account_id, &base.table_name, "Score0")
        .await
        .unwrap();
    TableKeyInfo {
        key_schema: index.key_schema,
        ..base.clone()
    }
}

async fn query(
    storage: &CellStorage,
    info: &TableKeyInfo,
    forward: bool,
    start: Option<&Item>,
    limit: Option<i64>,
) -> Result<(Vec<Item>, Option<Item>), StorageError> {
    let condition = KeyCondition {
        pk_path: vec![PathElement::Attribute("pk".into())],
        pk_value: Expr::Placeholder("p".into()),
        extra_pk_conditions: vec![],
        sk_condition: None,
        extra_sk_conditions: vec![],
    };
    let maps = ExpressionMaps::new(
        HashMap::new(),
        HashMap::from([("p".into(), AttributeValue::S("same".into()))]),
    );
    storage
        .query(
            info,
            &condition,
            &maps,
            forward,
            limit,
            start,
            Some("Score0"),
        )
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_indexes_follow_mutations_transactions_and_splits() {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "local-index-test".into(),
            cargo_lock_digest: Digest::from_bytes([1; 32]),
        })
        .unwrap(),
    );
    let account_id = "123456789012";
    let account = account_target(account_id).unwrap();
    let session = SessionId::from_bytes([91; 16]);
    let directory = tempfile::tempdir().unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("local-index-test"),
        *account.application().as_bytes(),
    );
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
            "http://local-index.internal".into(),
            directory.path().join("data"),
        )
        .unwrap(),
    );
    let account_handle = provisioner.admit_account(account_id).await.unwrap();
    let client = CellClient::local_runtime(registry, host.runtime(), layout);
    let storage = CellStorage::new(client.clone(), "us-east-1")
        .with_transaction_coordinators(provisioner.clone());
    let routed = CellStorage::new(client.clone(), "us-east-1")
        .with_initial_partitions(provisioner.clone())
        .with_transaction_coordinators(provisioner.clone());
    let mut infos = Vec::new();
    let maps = ExpressionMaps::default();
    for (name, creator) in [("LocalAccount", &storage), ("LocalRouted", &routed)] {
        let indexes: Vec<_> = (0..5).map(|i| serde_json::json!({
            "IndexName": format!("Score{i}"), "KeySchema": [{"AttributeName":"pk","KeyType":"HASH"},{"AttributeName":"score","KeyType":"RANGE"}],
            "Projection": {"ProjectionType":"ALL"}
        })).collect();
        let create = serde_json::from_value(serde_json::json!({
            "TableName": name, "BillingMode": "PAY_PER_REQUEST",
            "KeySchema": [{"AttributeName":"pk","KeyType":"HASH"},{"AttributeName":"sk","KeyType":"RANGE"}],
            "AttributeDefinitions": [{"AttributeName":"pk","AttributeType":"S"},{"AttributeName":"sk","AttributeType":"N"},{"AttributeName":"score","AttributeType":"N"}],
            "LocalSecondaryIndexes": indexes
        })).unwrap();
        let description = creator.create_table(account_id, create).await.unwrap();
        assert_eq!(description.local_secondary_indexes.unwrap().len(), 5);
        let base = storage.table_key_info(account_id, name).await.unwrap();
        assert!(base.has_lsi);
        assert_eq!(base.local_secondary_indexes.len(), 5);
        for value in [
            item("-2", Some("10")),
            item("2", Some("2")),
            item("10", Some("2")),
            item("20", None),
        ] {
            storage
                .put_item(&base, value, false, None, &maps, None)
                .await
                .unwrap();
        }
        let info = indexed(&storage, &base).await;
        for (forward, expected) in [(true, ["2", "10", "-2"]), (false, ["-2", "10", "2"])] {
            let mut cursor = None;
            let mut found = Vec::new();
            loop {
                let (page, next) = query(&storage, &info, forward, cursor.as_ref(), Some(1))
                    .await
                    .unwrap();
                found.extend(page.iter().map(|item| item["sk"].clone()));
                if let Some(key) = &next {
                    assert_eq!(key.len(), 3);
                }
                cursor = next;
                if cursor.is_none() {
                    break;
                }
            }
            assert_eq!(found, expected.map(|v| AttributeValue::N(v.into())));
        }
        let mut oversized = item("2", Some("2"));
        oversized.insert("payload".into(), AttributeValue::S("x".repeat(70 * 1024)));
        assert!(matches!(
            storage
                .put_item(&base, oversized, false, None, &maps, None)
                .await,
            Err(StorageError::Validation(_))
        ));
        let mut invalid = item("2", None);
        invalid.insert("score".into(), AttributeValue::S(String::new()));
        assert!(matches!(
            storage
                .put_item(&base, invalid, false, None, &maps, None)
                .await,
            Err(StorageError::Validation(_))
        ));
        let update = [UpdateAction::Remove {
            path: vec![PathElement::Attribute("score".into())],
        }];
        storage
            .update_item(
                &base,
                &item("10", None),
                &update,
                false,
                false,
                None,
                &maps,
                None,
            )
            .await
            .unwrap();
        storage
            .delete_item(&base, &item("-2", None), false, None, &maps, None)
            .await
            .unwrap();
        assert_eq!(
            query(&storage, &info, true, None, None).await.unwrap().0,
            vec![item("2", Some("2"))]
        );
        infos.push(base);
    }
    let images = [item("2", Some("-3")), item("20", Some("4"))];
    let operations: Vec<_> = infos
        .iter()
        .flat_map(|info| {
            images.iter().map(|item| TransactWriteOp::Put {
                key_info: info,
                item,
                condition: None,
                maps: &maps,
                return_values_on_ccf: Default::default(),
                stream: None,
            })
        })
        .collect();
    let token = || IdempotencyKey {
        account_id,
        token: "local-index-mixed",
        fingerprint: "two-tables-two-items",
    };
    storage
        .transact_write_items(&operations, Some(token()))
        .await
        .unwrap();
    assert!(matches!(
        storage
            .transact_write_items(&operations, Some(token()))
            .await,
        Err(StorageError::IdempotentReplay)
    ));
    for info in &infos {
        let indexed = indexed(&storage, info).await;
        assert_eq!(
            query(&storage, &indexed, true, None, None).await.unwrap().0,
            images
        );
        let mut cursor = None;
        let mut scanned = Vec::new();
        loop {
            let (page, next) = storage
                .scan(
                    &indexed,
                    Some(1),
                    cursor.as_ref(),
                    None,
                    None,
                    Some("Score0"),
                )
                .await
                .unwrap();
            scanned.extend(page);
            cursor = next;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(scanned.len(), 2);
        for item in &images {
            assert!(scanned.contains(item));
        }
    }
    for commit in [false, true] {
        assert_prepared_index_barriers(&client, &storage, &provisioner, &infos, commit).await;
    }
    let route = client
        .query::<ReadTableRoute>(&account, None, Json(infos[1].table_id.clone()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    provisioner
        .split_partition(
            account_id,
            account_handle,
            &infos[1].table_id,
            route.partitions[0].partition_id,
        )
        .await
        .unwrap();
    let indexed = indexed(&storage, &infos[1]).await;
    assert_eq!(
        query(&storage, &indexed, true, None, None).await.unwrap().0,
        images
    );
    host.shutdown().await.unwrap();
}

async fn assert_prepared_index_barriers(
    client: &CellClient,
    storage: &CellStorage,
    provisioner: &CellInitialPartitionProvisioner,
    infos: &[TableKeyInfo],
    commit: bool,
) {
    use beyonddb::{PrepareAccountTransaction, PrepareAccountTransactionInput};
    let account_id = &infos[0].account_id;
    let account = account_target(account_id).unwrap();
    let phase = if commit { 95 } else { 93 };
    let id = [phase; 16];
    provisioner
        .admit_coordinator(account_id, &id)
        .await
        .unwrap();
    let coordinator = coordinator_target(account_id, &id).unwrap();
    let mut participants = Vec::new();
    for (i, info) in infos.iter().enumerate() {
        let operations = [
            TransactionOperation::Put(PutItemInput {
                table_name: info.table_name.clone(),
                table_id: info.table_id.clone(),
                item: item("2", Some("12")),
                condition: None,
            }),
            TransactionOperation::Put(PutItemInput {
                table_name: info.table_name.clone(),
                table_id: info.table_id.clone(),
                item: item("30", Some("12")),
                condition: None,
            }),
            TransactionOperation::Delete(DeleteItemInput {
                table_name: info.table_name.clone(),
                table_id: info.table_id.clone(),
                key: item("20", None),
                condition: None,
                return_old: false,
            }),
        ];
        let route = client
            .query::<ReadPartitionRoute>(
                &account,
                None,
                Json(PartitionLookupInput {
                    table_id: info.table_id.clone(),
                    hash: data_key_hash(&info.table_id, &item("2", None), &info.base_key_schema)
                        .unwrap(),
                }),
            )
            .await
            .unwrap()
            .output
            .0;
        let target = match route {
            PartitionLookupOutcome::Unrouted => CoordinatorParticipantTarget::Account,
            PartitionLookupOutcome::Routed {
                partition_id,
                epoch,
            } => CoordinatorParticipantTarget::Data {
                table_id: info.table_id.clone(),
                partition_id,
                epoch,
            },
        };
        participants.push(CoordinatorParticipant {
            target,
            operations: operations
                .into_iter()
                .enumerate()
                .map(|(position, operation)| IndexedTransactionOperation {
                    index: (i * 3 + position) as u8,
                    operation,
                })
                .collect(),
        });
    }
    participants.sort_by_key(|participant| {
        let target = match &participant.target {
            CoordinatorParticipantTarget::Account => account.clone(),
            CoordinatorParticipantTarget::Data {
                table_id,
                partition_id,
                ..
            } => data_target(account_id, table_id, partition_id).unwrap(),
        };
        *target.cell_id().as_bytes()
    });
    transaction_command!(
        client,
        BeginCrossCellTransaction,
        &coordinator,
        mutation(),
        Json(BeginCrossCellTransactionInput {
            account_id: account_id.clone(),
            transaction_id: id,
            token: None,
            participants: participants.clone(),
        })
    )
    .await
    .unwrap();
    for (position, participant) in participants.into_iter().enumerate() {
        let target = match &participant.target {
            CoordinatorParticipantTarget::Account => account.clone(),
            CoordinatorParticipantTarget::Data {
                table_id,
                partition_id,
                ..
            } => data_target(account_id, table_id, partition_id).unwrap(),
        };
        let operations = participant
            .operations
            .into_iter()
            .map(|operation| operation.operation)
            .collect();
        let prepared = match participant.target {
            CoordinatorParticipantTarget::Account => transaction_command!(
                client,
                PrepareAccountTransaction,
                &account,
                mutation(),
                Json(PrepareAccountTransactionInput {
                    transaction_id: id,
                    coordinator_cell: *coordinator.cell_id().as_bytes(),
                    coordinator_key: id.to_vec(),
                    operations,
                })
            )
            .await
            .unwrap(),
            CoordinatorParticipantTarget::Data {
                table_id,
                partition_id,
                epoch,
            } => {
                let target = data_target(account_id, &table_id, &partition_id).unwrap();
                transaction_command!(
                    client,
                    PreparePartitionTransaction,
                    &target,
                    mutation(),
                    Json(PreparePartitionTransactionInput {
                        table_id,
                        epoch,
                        transaction_id: id,
                        coordinator_cell: *coordinator.cell_id().as_bytes(),
                        coordinator_key: id.to_vec(),
                        operations,
                    })
                )
                .await
                .unwrap()
            }
        };
        assert_eq!(prepared.output.0, PrepareTransactionOutcome::Prepared);
        client
            .command::<RecordParticipantPrepare>(
                &coordinator,
                mutation(),
                Json(CoordinatorPhaseInput {
                    account_id: account_id.clone(),
                    transaction_id: id,
                    routing_key: id.to_vec(),
                    position: position as u8,
                    participant_cell: *target.cell_id().as_bytes(),
                    sequence: prepared.receipt.commit_sequence,
                }),
            )
            .await
            .unwrap();
    }
    for info in infos {
        let indexed = indexed(storage, info).await;
        let condition = KeyCondition {
            pk_path: vec![PathElement::Attribute("pk".into())],
            pk_value: Expr::Placeholder("p".into()),
            extra_pk_conditions: vec![],
            sk_condition: Some(SortKeyCondition::Compare {
                path: vec![PathElement::Attribute("score".into())],
                op: CompareOp::Gt,
                value: Expr::Placeholder("s".into()),
            }),
            extra_sk_conditions: vec![],
        };
        let maps = ExpressionMaps::new(
            HashMap::new(),
            HashMap::from([
                ("p".into(), AttributeValue::S("same".into())),
                ("s".into(), AttributeValue::N("9".into())),
            ]),
        );
        // No live row matches this index range. The prepared create and sort-key
        // move must still fence it, including a cursor beyond the old index rows.
        assert!(matches!(
            storage
                .query(
                    &indexed,
                    &condition,
                    &maps,
                    true,
                    Some(1),
                    Some(&item("20", Some("10"))),
                    Some("Score0")
                )
                .await,
            Err(StorageError::Transient(_))
        ));
        assert!(matches!(
            storage
                .scan(
                    &indexed,
                    Some(1),
                    Some(&item("2", Some("-3"))),
                    None,
                    None,
                    Some("Score0")
                )
                .await,
            Err(StorageError::Transient(_))
        ));
    }
    client
        .command::<DecideCrossCellTransaction>(
            &coordinator,
            mutation(),
            Json(DecideCrossCellTransactionInput {
                account_id: account_id.clone(),
                transaction_id: id,
                routing_key: id.to_vec(),
                decision: if commit {
                    CoordinatorDecision::Commit
                } else {
                    CoordinatorDecision::Abort {
                        index: None,
                        reason: None,
                    }
                },
            }),
        )
        .await
        .unwrap();
    // The first indexed read must help a published decision and repeat its read.
    for info in infos {
        let indexed = indexed(storage, info).await;
        assert_eq!(
            query(storage, &indexed, true, None, None).await.unwrap().0,
            if commit {
                [item("2", Some("12")), item("30", Some("12"))]
            } else {
                [item("2", Some("-3")), item("20", Some("4"))]
            }
        );
        if commit {
            let maps = ExpressionMaps::default();
            storage
                .delete_item(info, &item("30", None), false, None, &maps, None)
                .await
                .unwrap();
            for image in [item("2", Some("-3")), item("20", Some("4"))] {
                storage
                    .put_item(info, image, false, None, &maps, None)
                    .await
                    .unwrap();
            }
        }
    }
}

fn mutation() -> MutationIdentity {
    MutationIdentity {
        request_id: RequestId::from_bytes(*uuid::Uuid::now_v7().as_bytes()),
        ..identity(91)
    }
}
