use crate::*;
use beyonddb::{
    BeginCrossCellTransaction, BeginCrossCellTransactionInput, CellStorage, CoordinatorDecision,
    CoordinatorParticipant, CoordinatorParticipantTarget, CoordinatorPhaseInput,
    CoordinatorProvisioner, DecideCrossCellTransaction, DecideCrossCellTransactionInput,
    IndexedTransactionOperation, PreparePartitionTransaction, PreparePartitionTransactionInput,
    PutItemInput, RecordParticipantPrepare, ResolvePartitionTransaction, ResolveTransactionInput,
    TransactionOperation, coordinator_target, data_target,
};
use extenddb_core::types::{AttributeValue, Item};

async fn abandon_commit(
    provisioner: &Arc<CellInitialPartitionProvisioner>,
    client: &CellClient,
    transaction_id: [u8; 16],
    key: &str,
) -> (crab_cell_runtime::identity::CellTarget, String) {
    let account_id = "123456789012";
    let account = account_target(account_id).unwrap();
    provisioner
        .ensure(client, account_id, &transaction_id)
        .await
        .unwrap();
    let coordinator = coordinator_target(account_id, &transaction_id).unwrap();
    let mut participants = Vec::new();
    for (index, name) in ["NetworkData", "RemoteTable"].into_iter().enumerate() {
        let table = client
            .query::<DescribeTable>(&account, None, Json(name.into()))
            .await
            .unwrap()
            .output
            .0
            .unwrap();
        let route = client
            .query::<ReadTableRoute>(&account, None, Json(table.id.clone()))
            .await
            .unwrap()
            .output
            .0
            .unwrap();
        assert_eq!(route.partitions.len(), 1);
        let partition = &route.partitions[0];
        let target = data_target(account_id, &table.id, &partition.partition_id).unwrap();
        participants.push((
            target,
            CoordinatorParticipant {
                target: CoordinatorParticipantTarget::Data {
                    table_id: table.id.clone(),
                    partition_id: partition.partition_id,
                    epoch: partition.epoch,
                },
                operations: vec![IndexedTransactionOperation {
                    index: u8::try_from(index).unwrap(),
                    operation: TransactionOperation::Put(PutItemInput {
                        table_name: name.into(),
                        table_id: table.id,
                        item: Item::from([
                            ("id".into(), AttributeValue::S(key.into())),
                            ("value".into(), AttributeValue::S("recovered".into())),
                        ]),
                        condition: None,
                    }),
                }],
            },
        ));
    }
    participants.sort_by_key(|(target, _)| *target.cell_id().as_bytes());
    transaction_command!(
        client,
        BeginCrossCellTransaction,
        &coordinator,
        identity(),
        Json(BeginCrossCellTransactionInput {
            account_id: account_id.into(),
            transaction_id,
            token: None,
            participants: participants
                .iter()
                .map(|(_, participant)| participant.clone())
                .collect(),
        }),
    )
    .await
    .unwrap();
    for (position, (target, participant)) in participants.iter().enumerate() {
        let CoordinatorParticipantTarget::Data {
            table_id, epoch, ..
        } = &participant.target
        else {
            unreachable!()
        };
        let prepare = transaction_command!(
            client,
            PreparePartitionTransaction,
            target,
            identity(),
            Json(PreparePartitionTransactionInput {
                table_id: table_id.clone(),
                epoch: *epoch,
                transaction_id,
                coordinator_cell: *coordinator.cell_id().as_bytes(),
                coordinator_key: transaction_id.to_vec(),
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
                &coordinator,
                identity(),
                Json(CoordinatorPhaseInput {
                    account_id: account_id.into(),
                    transaction_id,
                    routing_key: transaction_id.to_vec(),
                    position: u8::try_from(position).unwrap(),
                    participant_cell: *target.cell_id().as_bytes(),
                    sequence: prepare.receipt.commit_sequence,
                }),
            )
            .await
            .unwrap();
    }
    client
        .command::<DecideCrossCellTransaction>(
            &coordinator,
            identity(),
            Json(DecideCrossCellTransactionInput {
                account_id: account_id.into(),
                transaction_id,
                routing_key: transaction_id.to_vec(),
                decision: CoordinatorDecision::Commit,
            }),
        )
        .await
        .unwrap();
    // The request disappears after one apply but before recording its receipt.
    // The caller chooses whether a public read or the worker completes it.
    client
        .command::<ResolvePartitionTransaction>(
            &participants[0].0,
            identity(),
            Json(ResolveTransactionInput {
                transaction_id,
                coordinator_cell: *coordinator.cell_id().as_bytes(),
                commit: true,
            }),
        )
        .await
        .unwrap();
    let pending_table = match &participants[1].1.operations[0].operation {
        TransactionOperation::Put(input) => input.table_name.clone(),
        _ => unreachable!(),
    };
    (coordinator, pending_table)
}

pub(crate) async fn assert_abandoned_commit(
    provisioner: &Arc<CellInitialPartitionProvisioner>,
    tasks: &CellNodeTaskGroup,
    client: &CellClient,
    sdk: &aws_sdk_dynamodb::Client,
) {
    let account_id = "123456789012";
    let transaction_id = [105; 16];
    let (coordinator, _) = abandon_commit(provisioner, client, transaction_id, "abandoned").await;
    provisioner
        .install_transaction_recovery_loop(tasks, CellStorage::new(client.clone(), "us-east-1"))
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let status = client
                .query::<beyonddb::ReadCrossCellTransaction>(
                    &coordinator,
                    None,
                    Json(beyonddb::ReadCrossCellTransactionInput {
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
            if status.resolved_count == status.participant_count {
                assert_eq!(status.decision, CoordinatorDecision::Commit);
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_recovered_images(sdk).await;
}

pub(crate) async fn assert_read_triggered_commit(
    provisioner: &Arc<CellInitialPartitionProvisioner>,
    client: &CellClient,
    sdk: &aws_sdk_dynamodb::Client,
) {
    let transaction_id = [106; 16];
    let (coordinator, pending_table) =
        abandon_commit(provisioner, client, transaction_id, "read-help").await;
    // No recovery worker is installed yet. This signed SDK read must resolve
    // both participant receipts, including the first apply's missing receipt.
    let result = sdk
        .get_item()
        .table_name(pending_table)
        .key("id", AwsAttributeValue::S("read-help".into()))
        .consistent_read(true)
        .send()
        .await
        .unwrap();
    assert_eq!(
        result.item().unwrap().get("value"),
        Some(&AwsAttributeValue::S("recovered".into()))
    );
    let status = client
        .query::<beyonddb::ReadCrossCellTransaction>(
            &coordinator,
            None,
            Json(beyonddb::ReadCrossCellTransactionInput {
                account_id: "123456789012".into(),
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
}

pub(crate) async fn assert_recovered_images(sdk: &aws_sdk_dynamodb::Client) {
    for key in ["abandoned", "read-help"] {
        let read = sdk
            .transact_get_items()
            .set_transact_items(Some(
                ["RemoteTable", "NetworkData"]
                    .into_iter()
                    .map(|table| {
                        aws_sdk_dynamodb::types::TransactGetItem::builder()
                            .get(
                                aws_sdk_dynamodb::types::Get::builder()
                                    .table_name(table)
                                    .key("id", AwsAttributeValue::S(key.into()))
                                    .build()
                                    .unwrap(),
                            )
                            .build()
                    })
                    .collect(),
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(read.responses().len(), 2);
        assert!(
            read.responses()
                .iter()
                .all(|response| response.item().unwrap().get("value")
                    == Some(&AwsAttributeValue::S("recovered".into())))
        );

        for table in ["NetworkData", "RemoteTable"] {
            let result = sdk
                .get_item()
                .table_name(table)
                .key("id", AwsAttributeValue::S(key.into()))
                .send()
                .await
                .unwrap();
            assert_eq!(
                result.item().unwrap().get("value"),
                Some(&AwsAttributeValue::S("recovered".into()))
            );
        }
    }
}

fn identity() -> crab_cell_runtime::MutationIdentity {
    let issued_at_ms = now_ms();
    crab_cell_runtime::MutationIdentity {
        request_id: crab_cell_runtime::identity::RequestId::from_bytes(
            *uuid::Uuid::now_v7().as_bytes(),
        ),
        issued_at_ms,
        expires_at_ms: issued_at_ms + 60_000,
    }
}
