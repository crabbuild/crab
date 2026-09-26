use crate::*;
use beyonddb::{PrepareAccountTransaction, PrepareAccountTransactionInput};
use extenddb_core::types::TableKeyInfo;

fn mutation() -> MutationIdentity {
    let mut id = identity(248);
    id.request_id = RequestId::from_bytes(*uuid::Uuid::now_v7().as_bytes());
    id
}

pub(super) async fn assert_reads_finish_terminal_decisions(
    client: &CellClient,
    storage: &CellStorage,
    provisioner: &CellInitialPartitionProvisioner,
    infos: &[TableKeyInfo; 2],
) {
    let account_id = &infos[0].account_id;
    for (index, (read_side, scan, commit)) in [
        (0, false, true),
        (1, false, true),
        (0, true, true),
        (1, true, true),
        (0, false, false),
        (1, false, false),
        (0, true, false),
        (1, true, false),
    ]
    .into_iter()
    .enumerate()
    {
        // A token differs from the transaction ID, so participant recovery must
        // retain the routing key rather than guess its coordinator from the ID.
        let token = format!("read-help-{index}");
        let id = *uuid::Uuid::now_v7().as_bytes();
        let key = Item::from([("id".into(), AttributeValue::S(token.clone()))]);
        provisioner
            .admit_coordinator(account_id, token.as_bytes())
            .await
            .unwrap();
        let coordinator = coordinator_target(account_id, token.as_bytes()).unwrap();
        let mut participants = Vec::new();
        for (position, info) in infos.iter().enumerate() {
            let account = account_target(account_id).unwrap();
            let route = client
                .query::<ReadTableRoute>(&account, None, Json(info.table_id.clone()))
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
                None => (account, CoordinatorParticipantTarget::Account),
            };
            participants.push((
                target,
                CoordinatorParticipant {
                    target: participant,
                    operations: vec![IndexedTransactionOperation {
                        index: position as u8,
                        operation: TransactionOperation::Put(PutItemInput {
                            table_name: info.table_name.clone(),
                            table_id: info.table_id.clone(),
                            item: key.clone(),
                            condition: None,
                        }),
                    }],
                },
            ));
        }
        participants.sort_by_key(|(target, _)| *target.cell_id().as_bytes());
        client
            .command::<BeginCrossCellTransaction>(
                &coordinator,
                mutation(),
                Json(BeginCrossCellTransactionInput {
                    account_id: account_id.clone(),
                    transaction_id: id,
                    token: Some(TransactionToken {
                        account_id: account_id.clone(),
                        token: token.clone(),
                        fingerprint: token.clone(),
                    }),
                    participants: participants
                        .iter()
                        .map(|(_, participant)| participant.clone())
                        .collect(),
                }),
            )
            .await
            .unwrap();
        for (position, (target, participant)) in participants.iter().enumerate() {
            let operations = participant
                .operations
                .iter()
                .map(|op| op.operation.clone())
                .collect();
            let prepared = match &participant.target {
                CoordinatorParticipantTarget::Account => client
                    .command::<PrepareAccountTransaction>(
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
                } => client
                    .command::<PreparePartitionTransaction>(
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
                        account_id: account_id.clone(),
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
        let read = || async {
            if scan {
                storage
                    .scan(&infos[read_side], None, None, None, None, None)
                    .await
                    .map(|(items, _)| items.contains(&key))
            } else {
                storage
                    .get_item(&infos[read_side], &key)
                    .await
                    .map(|item| item == Some(key.clone()))
            }
        };
        assert!(
            matches!(read().await, Err(StorageError::Transient(_))),
            "BEGIN must not be guessed or completed by a read"
        );
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
                    account_id: account_id.clone(),
                    transaction_id: id,
                    routing_key: token.as_bytes().to_vec(),
                    decision: decision.clone(),
                }),
            )
            .await
            .unwrap();
        // No worker or explicit resolver runs: this read must finish both owners.
        assert_eq!(read().await.unwrap(), commit);
        let status = client
            .query::<ReadCrossCellTransaction>(
                &coordinator,
                None,
                Json(ReadCrossCellTransactionInput {
                    account_id: account_id.clone(),
                    transaction_id: id,
                    routing_key: token.as_bytes().to_vec(),
                }),
            )
            .await
            .unwrap()
            .output
            .0
            .unwrap();
        assert_eq!(status.decision, decision);
        assert_eq!(status.resolved_count, 2);
        for info in infos {
            assert_eq!(
                storage.get_item(info, &key).await.unwrap(),
                commit.then(|| key.clone())
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_resolves_committed_create_in_intent_range() {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "read-resolution".into(),
            cargo_lock_digest: Digest::from_bytes([1; 32]),
        })
        .unwrap(),
    );
    let account_id = "123456789012";
    let account = account_target(account_id).unwrap();
    let directory = tempfile::TempDir::new().unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("read-resolution"),
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
            "http://resolution.internal".into(),
            directory.path().into(),
        )
        .unwrap(),
    );
    provisioner.admit_account(account_id).await.unwrap();
    let client = CellClient::local_runtime(application.registry(), host.runtime(), layout);
    let storage = CellStorage::new(client.clone(), "us-east-1")
        .with_initial_partitions(provisioner.clone())
        .with_transaction_coordinators(provisioner.clone());
    let table = storage.create_table(account_id, serde_json::from_value(serde_json::json!({
        "TableName": "ReadResolution", "KeySchema": [{"AttributeName": "pk", "KeyType": "HASH"}, {"AttributeName": "sk", "KeyType": "RANGE"}],
        "AttributeDefinitions": [{"AttributeName": "pk", "AttributeType": "S"}, {"AttributeName": "sk", "AttributeType": "N"}], "BillingMode": "PAY_PER_REQUEST"
    })).unwrap()).await.unwrap();
    let info = storage
        .table_key_info(account_id, &table.table_name)
        .await
        .unwrap();
    let route = client
        .query::<ReadTableRoute>(&account, None, Json(table.table_id))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let partition = &route.partitions[0];
    let target = data_target(account_id, &info.table_id, &partition.partition_id).unwrap();
    assert_query_finishes_commit(
        &storage,
        &client,
        &target,
        partition.epoch,
        &info,
        &provisioner,
    )
    .await;
    host.shutdown().await.unwrap();
}

async fn assert_query_finishes_commit(
    storage: &CellStorage,
    client: &CellClient,
    target: &CellTarget,
    epoch: u64,
    key_info: &TableKeyInfo,
    provisioner: &CellInitialPartitionProvisioner,
) {
    let created = Item::from([
        ("pk".into(), AttributeValue::S("same".into())),
        ("sk".into(), AttributeValue::N("9".into())),
    ]);
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
    let id = [249; 16];
    provisioner
        .admit_coordinator(&key_info.account_id, &id)
        .await
        .unwrap();
    let coordinator = coordinator_target(&key_info.account_id, &id).unwrap();
    let route = client
        .query::<ReadTableRoute>(
            &account_target(&key_info.account_id).unwrap(),
            None,
            Json(key_info.table_id.clone()),
        )
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let participant = CoordinatorParticipant {
        target: CoordinatorParticipantTarget::Data {
            table_id: key_info.table_id.clone(),
            partition_id: route.partitions[0].partition_id,
            epoch,
        },
        operations: vec![IndexedTransactionOperation {
            index: 0,
            operation: TransactionOperation::Put(PutItemInput {
                table_name: key_info.table_name.clone(),
                table_id: key_info.table_id.clone(),
                item: created.clone(),
                condition: None,
            }),
        }],
    };
    client
        .command::<BeginCrossCellTransaction>(
            &coordinator,
            identity(249),
            Json(BeginCrossCellTransactionInput {
                account_id: key_info.account_id.clone(),
                transaction_id: id,
                token: None,
                participants: vec![participant.clone()],
            }),
        )
        .await
        .unwrap();
    let prepared = client
        .command::<PreparePartitionTransaction>(
            target,
            identity(249),
            Json(PreparePartitionTransactionInput {
                table_id: key_info.table_id.clone(),
                epoch,
                transaction_id: id,
                coordinator_cell: *coordinator.cell_id().as_bytes(),
                coordinator_key: id.to_vec(),
                operations: participant
                    .operations
                    .into_iter()
                    .map(|op| op.operation)
                    .collect(),
            }),
        )
        .await
        .unwrap();
    client
        .command::<RecordParticipantPrepare>(
            &coordinator,
            identity(250),
            Json(CoordinatorPhaseInput {
                account_id: key_info.account_id.clone(),
                transaction_id: id,
                routing_key: id.to_vec(),
                position: 0,
                participant_cell: *target.cell_id().as_bytes(),
                sequence: prepared.receipt.commit_sequence,
            }),
        )
        .await
        .unwrap();
    client
        .command::<DecideCrossCellTransaction>(
            &coordinator,
            identity(251),
            Json(DecideCrossCellTransactionInput {
                account_id: key_info.account_id.clone(),
                transaction_id: id,
                routing_key: id.to_vec(),
                decision: CoordinatorDecision::Commit,
            }),
        )
        .await
        .unwrap();
    let resolved = storage
        .query(key_info, &condition, &maps, true, None, None, None)
        .await
        .unwrap()
        .0;
    assert!(
        resolved.contains(&created),
        "Query must resolve the committed create absent from live rows"
    );
    let status = client
        .query::<ReadCrossCellTransaction>(
            &coordinator,
            None,
            Json(ReadCrossCellTransactionInput {
                account_id: key_info.account_id.clone(),
                transaction_id: id,
                routing_key: id.to_vec(),
            }),
        )
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    assert_eq!(status.resolved_count, 1);
}
