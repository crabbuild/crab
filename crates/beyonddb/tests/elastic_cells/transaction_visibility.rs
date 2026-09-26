use crate::*;
use beyonddb::{
    PartitionQuery, PartitionQueryInput, PartitionQueryOutcome, SortComparison, SortPredicate,
};
use extenddb_core::types::TableKeyInfo;

pub(crate) async fn assert_range_read_barriers(
    storage: &CellStorage,
    client: &CellClient,
    target: &CellTarget,
    epoch: u64,
    key_info: &TableKeyInfo,
) {
    let key = |pk: &str, sk: &str| {
        Item::from([
            ("pk".into(), AttributeValue::S(pk.into())),
            ("sk".into(), AttributeValue::N(sk.into())),
        ])
    };
    let created = key("same", "9");
    let deleted = key("same", "2");
    let untouched = key("same", "1.5");
    let transaction_id = [201; 16];
    let coordinator_cell = *account_target(&key_info.account_id)
        .unwrap()
        .cell_id()
        .as_bytes();
    transaction_command!(
        client,
        PreparePartitionTransaction,
        target,
        identity(201),
        Json(PreparePartitionTransactionInput {
            table_id: key_info.table_id.clone(),
            epoch,
            transaction_id,
            coordinator_cell,
            coordinator_key: transaction_id.to_vec(),
            operations: vec![
                TransactionOperation::Put(PutItemInput {
                    table_name: key_info.table_name.clone(),
                    table_id: key_info.table_id.clone(),
                    item: created.clone(),
                    condition: None,
                }),
                TransactionOperation::Delete(DeleteItemInput {
                    return_old: false,
                    table_name: key_info.table_name.clone(),
                    table_id: key_info.table_id.clone(),
                    key: deleted.clone(),
                    condition: None,
                }),
            ],
        }),
    )
    .await
    .unwrap();

    // Neither an absent create nor an existing delete may escape the barrier.
    for locked in [&created, &deleted] {
        assert!(matches!(
            storage.get_item(key_info, locked).await,
            Err(StorageError::Transient(_))
        ));
        let error = storage
            .transact_get_items(&[
                TransactGetOp {
                    key_info,
                    key: &untouched,
                },
                TransactGetOp {
                    key_info,
                    key: locked,
                },
            ])
            .await
            .unwrap_err();
        let StorageError::TransactionCanceled(reasons) = error else {
            panic!("unexpected read failure: {error:?}");
        };
        assert_eq!(
            reasons
                .iter()
                .map(|reason| reason.code.as_str())
                .collect::<Vec<_>>(),
            vec!["None", "TransactionConflict"]
        );
        assert!(reasons.iter().all(|reason| reason.item.is_none()));
    }
    assert_eq!(
        storage.get_item(key_info, &untouched).await.unwrap(),
        Some(untouched.clone())
    );
    assert!(matches!(
        storage
            .scan(key_info, Some(1), None, None, None, None)
            .await,
        Err(StorageError::Transient(_))
    ));
    // The scan continuation excludes already-visited locks, including itself.
    let last_locked = &created;
    assert!(
        storage
            .scan(key_info, None, Some(last_locked), None, None, None)
            .await
            .is_ok()
    );

    let base = PartitionQueryInput {
        table_id: key_info.table_id.clone(),
        epoch,
        partition_key: Item::from([("pk".into(), AttributeValue::S("same".into()))]),
        sort: None,
        extra_range_equals: vec![],
        forward: true,
        limit: 1,
        exclusive_start_key: None,
    };
    let compare = |op, n: &str| {
        Some(SortPredicate::Compare {
            attribute: "sk".into(),
            op,
            value: serde_json::from_value(serde_json::json!({"N": n})).unwrap(),
        })
    };
    let cases = [
        ("whole HASH", base.clone(), true),
        (
            "absent create",
            PartitionQueryInput {
                sort: compare(SortComparison::Eq, "9"),
                ..base.clone()
            },
            true,
        ),
        (
            "delete with equivalent numeric key",
            PartitionQueryInput {
                sort: compare(SortComparison::Eq, "2.0"),
                ..base.clone()
            },
            true,
        ),
        (
            "disjoint sort range",
            PartitionQueryInput {
                sort: compare(SortComparison::Lt, "2"),
                ..base.clone()
            },
            false,
        ),
        (
            "different HASH",
            PartitionQueryInput {
                partition_key: Item::from([("pk".into(), AttributeValue::S("other".into()))]),
                ..base.clone()
            },
            false,
        ),
        (
            "forward before intent",
            PartitionQueryInput {
                exclusive_start_key: Some(key("same", "8")),
                ..base.clone()
            },
            true,
        ),
        (
            "forward after intent",
            PartitionQueryInput {
                exclusive_start_key: Some(created.clone()),
                ..base.clone()
            },
            false,
        ),
        (
            "reverse before intent",
            PartitionQueryInput {
                forward: false,
                exclusive_start_key: Some(key("same", "10")),
                ..base.clone()
            },
            true,
        ),
        (
            "reverse after intent",
            PartitionQueryInput {
                forward: false,
                exclusive_start_key: Some(deleted.clone()),
                ..base.clone()
            },
            false,
        ),
    ];
    for (name, input, conflicts) in cases {
        let result = client
            .query::<PartitionQuery>(target, None, Json(input))
            .await
            .unwrap()
            .output
            .0;
        if conflicts {
            assert!(
                matches!(result, PartitionQueryOutcome::Conflict(conflict) if conflict.transaction.transaction_id == transaction_id && conflict.transaction.coordinator_cell == coordinator_cell && conflict.coordinator_key == transaction_id),
                "{name}"
            );
        } else {
            assert!(
                matches!(result, PartitionQueryOutcome::Page { .. }),
                "{name}: {result:?}"
            );
        }
    }
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
    assert!(matches!(
        storage
            .query(key_info, &condition, &maps, true, Some(1), None, None)
            .await,
        Err(StorageError::Transient(_))
    ));
    client
        .command::<ResolvePartitionTransaction>(
            target,
            identity(202),
            Json(ResolveTransactionInput {
                transaction_id,
                coordinator_cell,
                commit: false,
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        storage
            .transact_get_items(&[
                TransactGetOp {
                    key_info,
                    key: &created
                },
                TransactGetOp {
                    key_info,
                    key: &deleted
                },
            ])
            .await
            .unwrap(),
        vec![None, Some(deleted)]
    );
    assert!(
        storage
            .query(key_info, &condition, &maps, true, Some(1), None, None)
            .await
            .is_ok()
    );
    assert!(
        storage
            .scan(key_info, None, None, None, None, None)
            .await
            .is_ok()
    );
}

pub(crate) async fn assert_sdk_read_barrier(
    sdk: &aws_sdk_dynamodb::Client,
    client: &CellClient,
    partition: &PartitionSpec,
    account_id: &str,
) {
    let target = data_target(account_id, &partition.table.id, &partition.partition_id).unwrap();
    let transaction_id = [203; 16];
    let coordinator_cell = *account_target(account_id).unwrap().cell_id().as_bytes();
    let key = Item::from([
        ("pk".into(), AttributeValue::S("same".into())),
        ("sk".into(), AttributeValue::N("8".into())),
    ]);
    transaction_command!(
        client,
        PreparePartitionTransaction,
        &target,
        identity(203),
        Json(PreparePartitionTransactionInput {
            table_id: partition.table.id.clone(),
            epoch: partition.epoch,
            transaction_id,
            coordinator_cell,
            coordinator_key: transaction_id.to_vec(),
            operations: vec![TransactionOperation::Delete(DeleteItemInput {
                return_old: false,
                table_name: partition.table.table_name.clone(),
                table_id: partition.table.id.clone(),
                key,
                condition: None,
            })],
        }),
    )
    .await
    .unwrap();
    let request = || {
        sdk.transact_get_items().set_transact_items(Some(
            ["1.5", "8"]
                .into_iter()
                .map(|sk| {
                    aws_sdk_dynamodb::types::TransactGetItem::builder()
                        .get(
                            aws_sdk_dynamodb::types::Get::builder()
                                .table_name(&partition.table.table_name)
                                .key("pk", AwsAttributeValue::S("same".into()))
                                .key("sk", AwsAttributeValue::N(sk.into()))
                                .build()
                                .unwrap(),
                        )
                        .build()
                })
                .collect(),
        ))
    };
    let error = request().send().await.unwrap_err().into_service_error();
    let aws_sdk_dynamodb::operation::transact_get_items::TransactGetItemsError::TransactionCanceledException(error) = error else {
        panic!("unexpected SDK error: {error:?}");
    };
    assert_eq!(
        error
            .cancellation_reasons()
            .iter()
            .map(|reason| reason.code())
            .collect::<Vec<_>>(),
        vec![Some("None"), Some("TransactionConflict")]
    );
    let write_error = sdk
        .transact_write_items()
        .set_transact_items(Some(
            ["1.5", "8"]
                .into_iter()
                .map(|sk| {
                    aws_sdk_dynamodb::types::TransactWriteItem::builder()
                        .put(
                            aws_sdk_dynamodb::types::Put::builder()
                                .table_name(&partition.table.table_name)
                                .item("pk", AwsAttributeValue::S("same".into()))
                                .item("sk", AwsAttributeValue::N(sk.into()))
                                .item("uncommitted", AwsAttributeValue::Bool(true))
                                .build()
                                .unwrap(),
                        )
                        .build()
                })
                .collect(),
        ))
        .send()
        .await
        .unwrap_err()
        .into_service_error();
    let aws_sdk_dynamodb::operation::transact_write_items::TransactWriteItemsError::TransactionCanceledException(write_error) = write_error else {
        panic!("unexpected SDK write error: {write_error:?}");
    };
    assert_eq!(
        write_error
            .cancellation_reasons()
            .iter()
            .map(|reason| reason.code())
            .collect::<Vec<_>>(),
        vec![Some("None"), Some("TransactionConflict")]
    );
    client
        .command::<ResolvePartitionTransaction>(
            &target,
            identity(204),
            Json(ResolveTransactionInput {
                transaction_id,
                coordinator_cell,
                commit: false,
            }),
        )
        .await
        .unwrap();
    let response = request().send().await.unwrap();
    assert!(
        response
            .responses()
            .iter()
            .all(|item| !item.item().unwrap().contains_key("uncommitted"))
    );
    assert_eq!(
        response
            .responses()
            .iter()
            .map(|item| item.item().unwrap()["sk"].clone())
            .collect::<Vec<_>>(),
        vec![
            AwsAttributeValue::N("1.5".into()),
            AwsAttributeValue::N("8".into())
        ]
    );
}
