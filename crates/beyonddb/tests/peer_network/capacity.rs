use crate::*;
use beyonddb::{
    BeginCrossCellTransaction, BeginCrossCellTransactionInput, CellStorage, CoordinatorDecision,
    CoordinatorParticipant, CoordinatorParticipantTarget, CoordinatorProvisioner,
    DecideCrossCellTransaction, DecideCrossCellTransactionInput, IndexedTransactionOperation,
    PreparePartitionTransaction, PreparePartitionTransactionInput, PutItemInput,
    TransactionOperation, coordinator_target, data_target,
};
use extenddb_core::types::{AttributeValue, Item};

const KEY: &str = "capacity-canceled-retry";
const TOKEN: &str = "capacity-canceled-token";

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

fn writes() -> Vec<aws_sdk_dynamodb::types::TransactWriteItem> {
    ["NetworkData", "RemoteTable"]
        .into_iter()
        .map(|table| {
            aws_sdk_dynamodb::types::TransactWriteItem::builder()
                .put(
                    aws_sdk_dynamodb::types::Put::builder()
                        .table_name(table)
                        .item("id", AwsAttributeValue::S(KEY.into()))
                        .item(
                            "value",
                            AwsAttributeValue::S("after-capacity-relief".into()),
                        )
                        .build()
                        .unwrap(),
                )
                .build()
        })
        .collect()
}

fn reads() -> Vec<aws_sdk_dynamodb::types::TransactGetItem> {
    ["NetworkData", "RemoteTable"]
        .into_iter()
        .map(|table| {
            aws_sdk_dynamodb::types::TransactGetItem::builder()
                .get(
                    aws_sdk_dynamodb::types::Get::builder()
                        .table_name(table)
                        .key("id", AwsAttributeValue::S(KEY.into()))
                        .build()
                        .unwrap(),
                )
                .build()
        })
        .collect()
}

pub(crate) async fn assert_capacity_abort(
    provisioner: &CellInitialPartitionProvisioner,
    client: &CellClient,
    sdk: &aws_sdk_dynamodb::Client,
) {
    let account_id = "123456789012";
    let account = account_target(account_id).unwrap();
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
        let partition = route.partitions.into_iter().next().unwrap();
        let target = data_target(account_id, &table.id, &partition.partition_id).unwrap();
        participants.push((target, table, partition, index));
    }
    participants.sort_by_key(|(target, ..)| *target.cell_id().as_bytes());
    let (target, table, partition, blocked_index) = &participants[1];
    // These small intents reserve nearly the 512-MiB budget without padding
    // data. The public transaction first prepares the other Cell, then reaches
    // this disjoint-key shortage. Its ABORT must release only its own claim.
    let held = [(241_u8, 100), (242, 80), (243, 1)];
    for (byte, count) in held {
        let transaction_id = [byte; 16];
        provisioner
            .ensure(client, account_id, &transaction_id)
            .await
            .unwrap();
        let coordinator = coordinator_target(account_id, &transaction_id).unwrap();
        let operations = (0..count)
            .map(|index| {
                TransactionOperation::Put(PutItemInput {
                    table_name: table.table_name.clone(),
                    table_id: table.id.clone(),
                    item: Item::from([(
                        "id".into(),
                        AttributeValue::S(format!("capacity-holder-{byte}-{index}")),
                    )]),
                    condition: None,
                })
            })
            .collect::<Vec<_>>();
        transaction_command!(
            client,
            BeginCrossCellTransaction,
            &coordinator,
            identity(),
            Json(BeginCrossCellTransactionInput {
                account_id: account_id.into(),
                transaction_id,
                token: None,
                participants: vec![CoordinatorParticipant {
                    target: CoordinatorParticipantTarget::Data {
                        table_id: table.id.clone(),
                        partition_id: partition.partition_id,
                        epoch: partition.epoch
                    },
                    operations: operations
                        .iter()
                        .enumerate()
                        .map(|(index, operation)| IndexedTransactionOperation {
                            index: index as u8,
                            operation: operation.clone()
                        })
                        .collect(),
                }],
            })
        )
        .await
        .unwrap();
        transaction_command!(
            client,
            PreparePartitionTransaction,
            target,
            identity(),
            Json(PreparePartitionTransactionInput {
                table_id: table.id.clone(),
                epoch: partition.epoch,
                transaction_id,
                coordinator_cell: *coordinator.cell_id().as_bytes(),
                coordinator_key: transaction_id.to_vec(),
                operations,
            })
        )
        .await
        .unwrap();
    }
    let error = sdk
        .transact_write_items()
        .client_request_token(TOKEN)
        .set_transact_items(Some(writes()))
        .send()
        .await
        .unwrap_err();
    let error = error.as_service_error().unwrap();
    let aws_sdk_dynamodb::operation::transact_write_items::TransactWriteItemsError::TransactionCanceledException(error) = error else {
        panic!("expected a resolved capacity cancellation, got {error:?}");
    };
    let codes = error
        .cancellation_reasons()
        .iter()
        .map(|reason| reason.code())
        .collect::<Vec<_>>();
    let mut expected = vec![Some("None"), Some("None")];
    expected[*blocked_index] = Some("ThrottlingError");
    assert_eq!(codes, expected);
    let error = sdk
        .transact_get_items()
        .set_transact_items(Some(reads()))
        .send()
        .await
        .unwrap_err();
    let error = error.as_service_error().unwrap();
    let aws_sdk_dynamodb::operation::transact_get_items::TransactGetItemsError::TransactionCanceledException(error) = error else {
        panic!("expected a resolved read-capacity cancellation, got {error:?}");
    };
    let codes = error
        .cancellation_reasons()
        .iter()
        .map(|reason| reason.code())
        .collect::<Vec<_>>();
    assert_eq!(codes, expected);
    for (target, table, partition, _) in &participants {
        // Public GetItem can finish an unresolved decision itself. Query the
        // participant directly so read-triggered recovery cannot hide locks
        // left behind by a prematurely returned cancellation.
        let item = client
            .query::<beyonddb::PartitionGet>(
                target,
                None,
                Json(beyonddb::PartitionGetInput {
                    table_id: table.id.clone(),
                    epoch: partition.epoch,
                    key: Item::from([("id".into(), AttributeValue::S(KEY.into()))]),
                }),
            )
            .await
            .unwrap()
            .output
            .0;
        assert_eq!(item, beyonddb::PartitionGetOutcome::Found(None));
    }
    for (byte, _) in held {
        let transaction_id = [byte; 16];
        let coordinator = coordinator_target(account_id, &transaction_id).unwrap();
        let state = client
            .query::<beyonddb::ReadPartitionTransaction>(
                target,
                None,
                Json(beyonddb::ReadTransactionInput {
                    transaction_id,
                    coordinator_cell: *coordinator.cell_id().as_bytes(),
                }),
            )
            .await
            .unwrap()
            .output
            .0;
        assert_eq!(state, beyonddb::ParticipantTransactionState::Prepared);
        client
            .command::<DecideCrossCellTransaction>(
                &coordinator,
                identity(),
                Json(DecideCrossCellTransactionInput {
                    account_id: account_id.into(),
                    transaction_id,
                    routing_key: transaction_id.to_vec(),
                    decision: CoordinatorDecision::Abort {
                        index: None,
                        reason: None,
                    },
                }),
            )
            .await
            .unwrap();
        CellStorage::new(client.clone(), "us-east-1")
            .finish_decided_cross_cell_transaction(account_id, &transaction_id, transaction_id)
            .await
            .unwrap();
    }
    // A canceled token is reusable only after all ABORT resolutions. The same
    // payload now commits, proving no orphan claim or lock survived cancellation.
    sdk.transact_write_items()
        .client_request_token(TOKEN)
        .set_transact_items(Some(writes()))
        .send()
        .await
        .unwrap();
    assert_restored_retry(sdk).await;
}

pub(crate) async fn assert_restored_retry(sdk: &aws_sdk_dynamodb::Client) {
    let result = sdk
        .transact_get_items()
        .set_transact_items(Some(reads()))
        .send()
        .await
        .unwrap();
    assert_eq!(result.responses().len(), 2);
    for response in result.responses() {
        assert_eq!(
            response.item().unwrap()["value"],
            AwsAttributeValue::S("after-capacity-relief".into())
        );
    }
    sdk.transact_write_items()
        .client_request_token(TOKEN)
        .set_transact_items(Some(writes()))
        .send()
        .await
        .unwrap();
    for table in ["NetworkData", "RemoteTable"] {
        let item = sdk
            .get_item()
            .table_name(table)
            .consistent_read(true)
            .key("id", AwsAttributeValue::S(KEY.into()))
            .send()
            .await
            .unwrap()
            .item
            .unwrap();
        assert_eq!(
            item["value"],
            AwsAttributeValue::S("after-capacity-relief".into())
        );
    }
}
