use crate::*;
use beyonddb::{
    ReadAccountTransactionResult, ReadPartitionTransactionResult, ReadTransactionResultInput,
    TransactionReadResult,
};
use extenddb_core::types::TableKeyInfo;

fn mutation() -> MutationIdentity {
    let mut identity = identity(242);
    identity.request_id = RequestId::from_bytes(*uuid::Uuid::now_v7().as_bytes());
    identity
}

pub(super) async fn assert_shared_snapshots(
    client: &CellClient,
    storage: &CellStorage,
    provisioner: &CellInitialPartitionProvisioner,
    infos: &[TableKeyInfo; 2],
    key: &Item,
) {
    let account_id = &infos[0].account_id;
    let account = account_target(account_id).unwrap();
    let mut participants = Vec::new();
    for (index, info) in infos.iter().enumerate() {
        let route = client
            .query::<ReadPartitionRoute>(
                &account,
                None,
                Json(PartitionLookupInput {
                    table_id: info.table_id.clone(),
                    hash: data_key_hash(&info.table_id, key, &info.base_key_schema).unwrap(),
                }),
            )
            .await
            .unwrap()
            .output
            .0;
        let (target, participant) = match route {
            PartitionLookupOutcome::Unrouted => {
                (account.clone(), CoordinatorParticipantTarget::Account)
            }
            PartitionLookupOutcome::Routed {
                partition_id,
                epoch,
            } => (
                data_target(account_id, &info.table_id, &partition_id).unwrap(),
                CoordinatorParticipantTarget::Data {
                    table_id: info.table_id.clone(),
                    partition_id,
                    epoch,
                },
            ),
        };
        participants.push((
            target,
            CoordinatorParticipant {
                target: participant,
                operations: vec![IndexedTransactionOperation {
                    index: u8::try_from(index).unwrap(),
                    operation: TransactionOperation::Read(GetItemInput {
                        table_name: info.table_name.clone(),
                        table_id: info.table_id.clone(),
                        key: key.clone(),
                    }),
                }],
            },
        ));
    }
    participants.sort_by_key(|(target, _)| *target.cell_id().as_bytes());
    for byte in [242, 243] {
        let id = [byte; 16];
        provisioner
            .admit_coordinator(account_id, &id)
            .await
            .unwrap();
        let coordinator = coordinator_target(account_id, &id).unwrap();
        client
            .command::<BeginCrossCellTransaction>(
                &coordinator,
                mutation(),
                Json(BeginCrossCellTransactionInput {
                    account_id: account_id.clone(),
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
        for (target, participant) in &participants {
            let operations = participant
                .operations
                .iter()
                .map(|op| op.operation.clone())
                .collect();
            let result = match &participant.target {
                CoordinatorParticipantTarget::Account => {
                    client
                        .command::<beyonddb::PrepareAccountTransaction>(
                            target,
                            mutation(),
                            Json(beyonddb::PrepareAccountTransactionInput {
                                transaction_id: id,
                                coordinator_cell: *coordinator.cell_id().as_bytes(),
                                operations,
                            }),
                        )
                        .await
                }
                CoordinatorParticipantTarget::Data {
                    table_id, epoch, ..
                } => {
                    client
                        .command::<PreparePartitionTransaction>(
                            target,
                            mutation(),
                            Json(PreparePartitionTransactionInput {
                                table_id: table_id.clone(),
                                epoch: *epoch,
                                transaction_id: id,
                                coordinator_cell: *coordinator.cell_id().as_bytes(),
                                operations,
                            }),
                        )
                        .await
                }
            };
            assert_eq!(
                result.unwrap().output.0,
                PrepareTransactionOutcome::Prepared
            );
        }
    }
    let maps = ExpressionMaps::default();
    for info in infos {
        assert_eq!(
            storage.get_item(info, key).await.unwrap(),
            Some(key.clone())
        );
        assert!(matches!(
            storage
                .put_item(info, key.clone(), false, None, &maps, None)
                .await,
            Err(StorageError::TransactionConflict(_))
        ));
    }
    let readers = infos
        .iter()
        .map(|info| TransactGetOp {
            key_info: info,
            key,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        storage.transact_get_items(&readers).await.unwrap(),
        vec![Some(key.clone()), Some(key.clone())]
    );
    let committed = coordinator_target(account_id, &[242; 16]).unwrap();
    for (target, participant) in &participants {
        assert_eq!(
            saved(client, target, &participant.target, &committed, [242; 16]).await,
            TransactionReadResult::Unavailable
        );
    }
    assert_eq!(
        storage
            .resume_cross_cell_transaction(account_id, &[242; 16], [242; 16])
            .await
            .unwrap(),
        CoordinatorDecision::Commit
    );
    // Releasing one reader must leave the other reader's locks intact.
    for info in infos {
        assert!(matches!(
            storage
                .delete_item(info, key, false, None, &maps, None)
                .await,
            Err(StorageError::TransactionConflict(_))
        ));
    }
    let aborted = coordinator_target(account_id, &[243; 16]).unwrap();
    client
        .command::<DecideCrossCellTransaction>(
            &aborted,
            mutation(),
            Json(DecideCrossCellTransactionInput {
                account_id: account_id.clone(),
                transaction_id: [243; 16],
                routing_key: vec![243; 16],
                decision: CoordinatorDecision::Abort {
                    index: None,
                    reason: None,
                },
            }),
        )
        .await
        .unwrap();
    storage
        .finish_decided_cross_cell_transaction(account_id, &[243; 16], [243; 16])
        .await
        .unwrap();
    let mut changed = key.clone();
    changed.insert("newer".into(), AttributeValue::Bool(true));
    for info in infos {
        storage
            .put_item(info, changed.clone(), false, None, &maps, None)
            .await
            .unwrap();
    }
    for (target, participant) in &participants {
        assert_eq!(
            saved(client, target, &participant.target, &committed, [242; 16]).await,
            TransactionReadResult::Item(Some(key.clone()))
        );
        assert_eq!(
            saved(client, target, &participant.target, &aborted, [243; 16]).await,
            TransactionReadResult::Unavailable
        );
    }
}

async fn saved(
    client: &CellClient,
    target: &CellTarget,
    participant: &CoordinatorParticipantTarget,
    coordinator: &CellTarget,
    transaction_id: [u8; 16],
) -> TransactionReadResult {
    let input = Json(ReadTransactionResultInput {
        transaction: ReadTransactionInput {
            transaction_id,
            coordinator_cell: *coordinator.cell_id().as_bytes(),
        },
        position: 0,
    });
    match participant {
        CoordinatorParticipantTarget::Account => {
            client
                .query::<ReadAccountTransactionResult>(target, None, input)
                .await
                .unwrap()
                .output
                .0
        }
        CoordinatorParticipantTarget::Data { .. } => {
            client
                .query::<ReadPartitionTransactionResult>(target, None, input)
                .await
                .unwrap()
                .output
                .0
        }
    }
}
