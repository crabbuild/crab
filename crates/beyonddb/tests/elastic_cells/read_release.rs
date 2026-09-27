use super::coordinator_checkpoints::owner;
use crate::*;
use beyonddb::{
    BeginReadResultRelease, ReadAccountTransactionResult, ReadPartitionTransactionResult,
    ReadTransactionResultInput, ReleaseAccountTransactionReads, TransactionReadResult,
};

const ACCOUNT: &str = "123456789012";

fn mutation() -> MutationIdentity {
    let mut value = identity(251);
    value.request_id = RequestId::from_bytes(*uuid::Uuid::now_v7().as_bytes());
    value
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acknowledged_read_cleanup_survives_restart_before_and_after_participant_delete() {
    for delete_first in [false, true] {
        cleanup_restart(delete_first).await;
    }
}

async fn cleanup_restart(delete_first: bool) {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "read-release".into(),
            cargo_lock_digest: Digest::from_bytes([1; 32]),
        })
        .unwrap(),
    );
    let directory = tempfile::tempdir().unwrap();
    let account = account_target(ACCOUNT).unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("read-release"),
        *account.application().as_bytes(),
    );
    let nodes = NodeDirectory::new(
        layout.clone(),
        Digest::from_bytes([201; 32]),
        Digest::from_bytes([202; 32]),
        application.registry().release_digest(),
    );
    let (host, provisioner, client, storage) = owner(
        &application,
        &layout,
        SessionId::from_bytes([251; 16]),
        &directory.path().join("first"),
    );
    provisioner.admit_account(ACCOUNT).await.unwrap();
    let routed =
        CellStorage::new(client.clone(), "us-east-1").with_initial_partitions(provisioner.clone());
    let key = Item::from([("id".into(), AttributeValue::S("saved".into()))]);
    let mut participants = Vec::new();
    let mut infos = Vec::new();
    for (index, backend) in [&storage, &routed].into_iter().enumerate() {
        let name = format!("Images{index}");
        let table = backend
            .create_table(
                ACCOUNT,
                serde_json::from_value(serde_json::json!({
                    "TableName": name, "KeySchema": [{"AttributeName":"id","KeyType":"HASH"}],
                    "AttributeDefinitions":[{"AttributeName":"id","AttributeType":"S"}],
                    "BillingMode":"PAY_PER_REQUEST"
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        let info = backend.table_key_info(ACCOUNT, &name).await.unwrap();
        backend
            .put_item(
                &info,
                key.clone(),
                false,
                None,
                &ExpressionMaps::default(),
                None,
            )
            .await
            .unwrap();
        let route = crate::single_leaf_route(&client, &account, &table.table_id).await;
        let (target, participant) = match route {
            None => (account.clone(), CoordinatorParticipantTarget::Account),
            Some(route) => {
                let spec = &route.partitions[0];
                (
                    data_target(ACCOUNT, &info.table_id, &spec.partition_id).unwrap(),
                    CoordinatorParticipantTarget::Data {
                        table_id: info.table_id.clone(),
                        partition_id: spec.partition_id,
                        epoch: spec.epoch,
                    },
                )
            }
        };
        infos.push(info.clone());
        participants.push((
            target,
            CoordinatorParticipant {
                target: participant,
                operations: vec![IndexedTransactionOperation {
                    index: index as u8,
                    operation: TransactionOperation::Read(GetItemInput {
                        table_name: name,
                        table_id: info.table_id.clone(),
                        key: key.clone(),
                    }),
                }],
            },
        ));
    }
    participants.sort_by_key(|(target, _)| *target.cell_id().as_bytes());
    let reads = infos
        .iter()
        .map(|info| TransactGetOp {
            key_info: info,
            key: &key,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        storage.transact_get_items(&reads).await.unwrap(),
        vec![Some(key.clone()), Some(key.clone())]
    );
    for (target, _) in &participants {
        let cell_directory = directory.path().join("first").join(
            blake3::Hash::from_bytes(*target.cell_id().as_bytes())
                .to_hex()
                .as_str(),
        );
        let files: Vec<_> = std::fs::read_dir(cell_directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "sqlite"))
            .collect();
        assert_eq!(files.len(), 1);
        let database = rusqlite::Connection::open_with_flags(
            &files[0],
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        assert_eq!(
            database
                .query_row("SELECT COUNT(*) FROM ddb_transaction_reads", [], |row| row
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            0,
            "a returned public transaction must release saved images"
        );
    }

    let id = [251; 16];
    provisioner.admit_coordinator(ACCOUNT, &id).await.unwrap();
    let coordinator = coordinator_target(ACCOUNT, &id).unwrap();
    let read = ReadCrossCellTransactionInput {
        account_id: ACCOUNT.into(),
        transaction_id: id,
        routing_key: id.to_vec(),
    };
    transaction_command!(
        client,
        BeginCrossCellTransaction,
        &coordinator,
        mutation(),
        Json(BeginCrossCellTransactionInput {
            account_id: ACCOUNT.into(),
            transaction_id: id,
            token: None,
            participants: participants.iter().map(|(_, p)| p.clone()).collect(),
        })
    )
    .await
    .unwrap();
    assert!(matches!(
        client
            .command::<BeginReadResultRelease>(&coordinator, mutation(), Json(read.clone()))
            .await,
        Err(InvocationError::Rejected(_))
    ));
    storage
        .resume_cross_cell_transaction(ACCOUNT, &id, id)
        .await
        .unwrap();
    for (target, _) in &participants {
        assert_eq!(
            saved(&client, target, &coordinator, id).await,
            TransactionReadResult::Item(Some(key.clone()))
        );
    }
    // A mismatched coordinator cannot discard another reader's saved image.
    assert!(matches!(
        client
            .command::<ReleaseAccountTransactionReads>(
                &account,
                mutation(),
                Json(ReadTransactionInput {
                    transaction_id: id,
                    coordinator_cell: [0; 32],
                })
            )
            .await,
        Err(InvocationError::Rejected(_))
    ));
    // A later shard forces recovery to release this settled coordinator in the
    // four-slot fixture, regardless of the public read's random routing key.
    let later_key = (0_u32..100_000)
        .map(u32::to_be_bytes)
        .find(|key| coordinator_target(ACCOUNT, key).unwrap().partition() > coordinator.partition())
        .unwrap();
    provisioner
        .admit_coordinator(ACCOUNT, &later_key)
        .await
        .unwrap();
    // Record a settled root before acknowledgement: the later cleanup intent
    // must invalidate that checkpoint and become visible to startup recovery.
    provisioner
        .recover_registered_coordinators(ACCOUNT, &client, &storage, &nodes)
        .await
        .unwrap();
    let released = CellAuthority::new(layout.clone())
        .load(coordinator.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        released.value().state,
        crab_cell_runtime::control::ControlState::Idle
    );
    assert!(released.value().owner.is_none());
    // local_runtime never restores Idle roots; raw commands must arrange
    // ownership, just as the serving resolver does before dispatch.
    provisioner.admit_coordinator(ACCOUNT, &id).await.unwrap();
    client
        .command::<BeginReadResultRelease>(&coordinator, mutation(), Json(read.clone()))
        .await
        .unwrap();
    let pending = client
        .query::<ReadPendingCrossCellTransactions>(
            &coordinator,
            None,
            Json(ReadPendingCrossCellTransactionsInput {
                after: None,
                limit: 100,
            }),
        )
        .await
        .unwrap()
        .output
        .0;
    assert_eq!(
        pending.iter().map(|p| p.transaction_id).collect::<Vec<_>>(),
        vec![id]
    );
    if delete_first {
        client
            .command::<ReleaseAccountTransactionReads>(
                &account,
                mutation(),
                Json(ReadTransactionInput {
                    transaction_id: id,
                    coordinator_cell: *coordinator.cell_id().as_bytes(),
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            saved(&client, &account, &coordinator, id).await,
            TransactionReadResult::Unavailable
        );
        // Deliberately omit the coordinator receipt, as if the response was lost.
    }
    host.shutdown().await.unwrap();
    let (host, provisioner, client, storage) = owner(
        &application,
        &layout,
        SessionId::from_bytes([252; 16]),
        &directory.path().join("restored"),
    );
    let account_handle = provisioner.admit_account(ACCOUNT).await.unwrap();
    provisioner
        .recover_registered_account(ACCOUNT, account_handle, &client, &storage, &nodes)
        .await
        .unwrap();
    provisioner.admit_coordinator(ACCOUNT, &id).await.unwrap();
    let status = client
        .query::<ReadCrossCellTransaction>(&coordinator, None, Json(read.clone()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    assert_eq!(status.unreleased_read_results, 0);
    assert_eq!(status.decision, CoordinatorDecision::Commit);
    for (position, (target, participant)) in participants.iter().enumerate() {
        assert_eq!(
            saved(&client, target, &coordinator, id).await,
            TransactionReadResult::Unavailable
        );
        assert!(
            client
                .query::<ReadCoordinatorParticipant>(
                    &coordinator,
                    None,
                    Json(ReadCoordinatorParticipantInput {
                        account_id: ACCOUNT.into(),
                        transaction_id: id,
                        routing_key: id.to_vec(),
                        position: position as u8,
                        chunk: 0,
                    })
                )
                .await
                .unwrap()
                .output
                .is_none()
        );
        let operations = participant
            .operations
            .iter()
            .map(|op| op.operation.clone())
            .collect();
        let replay = match &participant.target {
            CoordinatorParticipantTarget::Account => {
                transaction_command!(
                    client,
                    beyonddb::PrepareAccountTransaction,
                    target,
                    mutation(),
                    Json(beyonddb::PrepareAccountTransactionInput {
                        transaction_id: id,
                        coordinator_cell: *coordinator.cell_id().as_bytes(),
                        coordinator_key: id.to_vec(),
                        operations,
                    })
                )
                .await
            }
            CoordinatorParticipantTarget::Data {
                table_id, epoch, ..
            } => {
                transaction_command!(
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
                        operations,
                    })
                )
                .await
            }
        };
        assert!(
            matches!(replay, Err(InvocationError::Rejected(result)) if result.output.0 == PrepareTransactionOutcome::Committed)
        );
        assert_eq!(
            saved(&client, target, &coordinator, id).await,
            TransactionReadResult::Unavailable
        );
    }
    client
        .command::<BeginReadResultRelease>(&coordinator, mutation(), Json(read))
        .await
        .unwrap();
    assert!(
        client
            .query::<beyonddb::ReadPendingTransactionBoundary>(&coordinator, None, Json(()))
            .await
            .unwrap()
            .output
            .0
            .is_none()
    );
    host.shutdown().await.unwrap();
}

async fn saved(
    client: &CellClient,
    target: &CellTarget,
    coordinator: &CellTarget,
    id: [u8; 16],
) -> TransactionReadResult {
    let input = Json(ReadTransactionResultInput {
        transaction: ReadTransactionInput {
            transaction_id: id,
            coordinator_cell: *coordinator.cell_id().as_bytes(),
        },
        position: 0,
    });
    if *target == account_target(ACCOUNT).unwrap() {
        client
            .query::<ReadAccountTransactionResult>(target, None, input)
            .await
            .unwrap()
            .output
            .0
    } else {
        client
            .query::<ReadPartitionTransactionResult>(target, None, input)
            .await
            .unwrap()
            .output
            .0
    }
}
