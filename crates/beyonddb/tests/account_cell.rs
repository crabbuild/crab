use std::{collections::HashMap, sync::Arc, time::UNIX_EPOCH};

use beyonddb::{
    Beyonddb, CellStorage, CreateTable, CreateTableOutcome, DeleteItem, DeleteItemInput,
    DescribeTable, GetItem, GetItemInput, GetItemOutcome, ItemMutationOutcome, Json, ListTables,
    ListTablesInput, ListTablesOutcome, PutItem, PutItemInput, TableSpec, TransactGet,
    TransactWrite, TransactWriteInput, TransactionGetOutcome, TransactionOutcome, TransactionWrite,
    account_target, initialize_account,
};
use crab_cell_app::CellApplication;
use crab_cell_host::CellNodeBuilder;
use crab_cell_runtime::cell::catalog::{CatalogEntry, CatalogRole, CellCatalog};
use crab_cell_runtime::client::CellClient;
use crab_cell_runtime::client::InvocationError;
use crab_cell_runtime::control::{Owner, authority::CellAuthority};
use crab_cell_runtime::identity::{Digest, IncarnationId, RequestId, SessionId};
use crab_cell_runtime::ltx::{CellStorageLayout, DiskBudget, Host, Limits};
use crab_cell_runtime::registry::BuildDescriptor;
use crab_cell_runtime::{MutationIdentity, SqlWorkerPool};
use crab_ltx::CellReplica;
use crab_storage::Store;
use extenddb_core::expression::{Expr, ExpressionMaps, KeyCondition, PathElement, UpdateAction};
use extenddb_core::types::{
    AttributeDefinition, AttributeValue, BillingMode, CreateTableInput, DeleteTableInput, Item,
    KeySchemaElement, KeyType, ReturnValuesOnConditionCheckFailure, ScalarAttributeType,
    TableStatus, UpdateTableInput,
};
use extenddb_storage::{
    DataEngine, TableEngine, TransactGetOp, TransactWriteOp, error::StorageError,
};
use object_store::memory::InMemory;

fn identity(byte: u8) -> MutationIdentity {
    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    MutationIdentity {
        request_id: RequestId::from_bytes([byte; 16]),
        issued_at_ms: now_ms,
        expires_at_ms: now_ms + 60_000,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_items_replay_rollback_and_restore_on_new_host() {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "account-cell-test".into(),
            cargo_lock_digest: Digest::from_bytes([1; 32]),
        })
        .unwrap(),
    );
    let target = account_target("123456789012").unwrap();
    let session = SessionId::from_bytes([2; 16]);
    let directory = tempfile::TempDir::new().unwrap();
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = CellStorageLayout::new(
        store,
        object_store::path::Path::from("beyonddb-account-test"),
        *target.application().as_bytes(),
    );
    let host = CellNodeBuilder::new(Arc::clone(&application))
        .with_runtime(SqlWorkerPool::new(1, 8).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(Host::default().with_local_disk_budget(DiskBudget::new(1 << 30)))
        .with_session(session)
        .build_unleased_for_maintenance()
        .unwrap();
    let registry = application.registry();
    let catalog = CellCatalog::new(layout.clone(), target.tenant());
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &target,
                CatalogRole::Sql,
                registry.module_code("beyonddb-account").unwrap(),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(layout.clone());
    let incarnation = IncarnationId::from_bytes([3; 16]);
    let control = authority
        .create_initial(
            &proof,
            incarnation,
            Owner {
                session,
                endpoint: "https://beyonddb.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let handle = host
        .runtime()
        .bootstrap(
            proof,
            CellReplica::new(
                layout.clone(),
                *target.cell_id().as_bytes(),
                *incarnation.as_bytes(),
                Limits::default(),
            )
            .unwrap(),
            authority,
            control,
            directory.path().join("account.sqlite"),
            initialize_account,
        )
        .await
        .unwrap();
    let cell_client = CellClient::local(registry, handle.clone());
    let storage = CellStorage::new(cell_client.clone(), "us-east-1");
    let client = host
        .application_handle::<Beyonddb>(cell_client, target.tenant(), target.application())
        .unwrap();
    let schema = TableSpec {
        table_name: "Books".into(),
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
    };
    let created = client
        .command::<CreateTable>(&target, identity(4), Json(schema.clone()))
        .await
        .unwrap();
    let book_table_id = match created.output.0 {
        CreateTableOutcome::Created(record) => record.id,
        _ => panic!("expected table creation"),
    };
    let described = storage
        .describe_table(
            "123456789012",
            extenddb_core::types::DescribeTableInput {
                table_name: "Books".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(described.table_status, TableStatus::Active);
    assert_eq!(
        described.billing_mode_summary.unwrap().billing_mode,
        BillingMode::PayPerRequest
    );

    let key = Item::from([("id".into(), AttributeValue::S("book-1".into()))]);
    let mut item = key.clone();
    item.insert("title".into(), AttributeValue::S("Cell Systems".into()));
    let put = PutItemInput {
        table_name: "Books".into(),
        table_id: book_table_id.clone(),
        item: item.clone(),
        condition: None,
    };
    let mutation = identity(5);
    let written = client
        .command::<PutItem>(&target, mutation, Json(put.clone()))
        .await
        .unwrap();
    assert_eq!(written.output.0, ItemMutationOutcome::Applied(None));
    let replay = client
        .command::<PutItem>(&target, mutation, Json(put))
        .await
        .unwrap();
    assert_eq!(replay.receipt, written.receipt);

    let read = client
        .query::<GetItem>(
            &target,
            Some(written.receipt),
            Json(GetItemInput {
                table_name: "Books".into(),
                table_id: book_table_id.clone(),
                key: key.clone(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(read.output.0, GetItemOutcome::Found(Some(item.clone())));

    let deleted = client
        .command::<DeleteItem>(
            &target,
            identity(6),
            Json(DeleteItemInput {
                table_name: "Books".into(),
                table_id: book_table_id.clone(),
                key,
                condition: None,
            }),
        )
        .await
        .unwrap();
    assert_eq!(deleted.output.0, ItemMutationOutcome::Applied(Some(item)));

    let author_table = storage
        .create_table(
            "123456789012",
            CreateTableInput {
                table_name: "Authors".into(),
                key_schema: schema.key_schema.clone(),
                attribute_definitions: schema.attribute_definitions.clone(),
                billing_mode: Some(BillingMode::PayPerRequest),
                deletion_protection_enabled: Some(true),
                ..CreateTableInput::default()
            },
        )
        .await
        .unwrap();
    assert!(author_table.deletion_protection_enabled);
    let author_table_id = author_table.table_id.clone();
    assert!(matches!(
        storage
            .index_info_by_table_id(&author_table.table_id, "missing")
            .await,
        Err(StorageError::IndexNotFound(_))
    ));
    let books = storage
        .table_key_info("123456789012", "Books")
        .await
        .unwrap();
    let authors = storage
        .table_key_info("123456789012", "Authors")
        .await
        .unwrap();
    let exists_condition = Expr::Function {
        name: "attribute_not_exists".into(),
        args: vec![Expr::Path(vec![PathElement::Attribute("id".into())])],
    };
    let conditional_key = Item::from([("id".into(), AttributeValue::S("conditioned".into()))]);
    let original = storage
        .put_item(
            &books,
            conditional_key.clone(),
            true,
            Some(&exists_condition),
            &ExpressionMaps::default(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(original, None);
    let rejected = storage
        .put_item(
            &books,
            conditional_key.clone(),
            true,
            Some(&exists_condition),
            &ExpressionMaps::default(),
            None,
        )
        .await;
    assert!(
        matches!(rejected, Err(StorageError::ConditionFailed(Some(old))) if old == conditional_key)
    );
    let update_maps = ExpressionMaps::new(
        HashMap::new(),
        HashMap::from([("title".into(), AttributeValue::S("Updated".into()))]),
    );
    let actions = [UpdateAction::Set {
        path: vec![PathElement::Attribute("title".into())],
        value: Expr::Placeholder("title".into()),
    }];
    let (old, updated) = storage
        .update_item(
            &books,
            &conditional_key,
            &actions,
            true,
            true,
            None,
            &update_maps,
            None,
        )
        .await
        .unwrap();
    let mut expected_updated = conditional_key.clone();
    expected_updated.insert("title".into(), AttributeValue::S("Updated".into()));
    assert_eq!(
        (old, updated),
        (
            Some(conditional_key.clone()),
            Some(expected_updated.clone())
        )
    );
    assert_eq!(
        storage.get_item(&books, &conditional_key).await.unwrap(),
        Some(expected_updated.clone())
    );
    let rejected_key_update = [UpdateAction::Set {
        path: vec![PathElement::Attribute("id".into())],
        value: Expr::Placeholder("title".into()),
    }];
    assert!(matches!(
        storage
            .update_item(
                &books,
                &conditional_key,
                &rejected_key_update,
                false,
                false,
                None,
                &update_maps,
                None
            )
            .await,
        Err(StorageError::Validation(_))
    ));
    let temporary = Item::from([("id".into(), AttributeValue::S("temporary".into()))]);
    storage
        .put_item(
            &books,
            temporary.clone(),
            false,
            None,
            &ExpressionMaps::default(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        storage.get_item(&books, &temporary).await.unwrap(),
        Some(temporary.clone())
    );
    assert!(matches!(
        storage
            .delete_item(
                &books,
                &temporary,
                true,
                Some(&exists_condition),
                &ExpressionMaps::default(),
                None,
            )
            .await,
        Err(StorageError::ConditionFailed(Some(old))) if old == temporary
    ));
    assert_eq!(
        storage
            .delete_item(
                &books,
                &temporary,
                true,
                None,
                &ExpressionMaps::default(),
                None
            )
            .await
            .unwrap(),
        Some(temporary)
    );
    let tx_book = Item::from([("id".into(), AttributeValue::S("book-tx".into()))]);
    let tx_author = Item::from([("id".into(), AttributeValue::S("author-tx".into()))]);
    let maps = ExpressionMaps::default();
    storage
        .transact_write_items(
            &[
                TransactWriteOp::Put {
                    key_info: &books,
                    item: &tx_book,
                    condition: None,
                    maps: &maps,
                    return_values_on_ccf: Default::default(),
                    stream: None,
                },
                TransactWriteOp::Put {
                    key_info: &authors,
                    item: &tx_author,
                    condition: None,
                    maps: &maps,
                    return_values_on_ccf: Default::default(),
                    stream: None,
                },
            ],
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        storage
            .transact_get_items(&[
                TransactGetOp {
                    key_info: &books,
                    key: &tx_book,
                },
                TransactGetOp {
                    key_info: &authors,
                    key: &tx_author,
                },
            ])
            .await
            .unwrap(),
        vec![Some(tx_book.clone()), Some(tx_author)]
    );

    let book_key = Item::from([("id".into(), AttributeValue::S("book-2".into()))]);
    let author_key = Item::from([("id".into(), AttributeValue::S("author-1".into()))]);
    let invalid_key = Item::from([("wrong".into(), AttributeValue::S("author-1".into()))]);
    let attempted = client
        .command::<TransactWrite>(
            &target,
            identity(8),
            Json(TransactWriteInput {
                operations: vec![
                    TransactionWrite::Put(PutItemInput {
                        table_name: "Books".into(),
                        table_id: book_table_id.clone(),
                        item: book_key.clone(),
                        condition: None,
                    }),
                    TransactionWrite::Put(PutItemInput {
                        table_name: "Authors".into(),
                        table_id: author_table_id.clone(),
                        item: invalid_key,
                        condition: None,
                    }),
                ],
            }),
        )
        .await;
    assert!(matches!(
        attempted,
        Err(InvocationError::Rejected(committed))
            if matches!(committed.output.0, TransactionOutcome::Rejected { index: 1, .. })
    ));

    let absent = client
        .query::<GetItem>(
            &target,
            None,
            Json(GetItemInput {
                table_name: "Books".into(),
                table_id: book_table_id.clone(),
                key: book_key.clone(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(absent.output.0, GetItemOutcome::Found(None));

    let committed = client
        .command::<TransactWrite>(
            &target,
            identity(9),
            Json(TransactWriteInput {
                operations: vec![
                    TransactionWrite::Put(PutItemInput {
                        table_name: "Books".into(),
                        table_id: book_table_id.clone(),
                        item: book_key.clone(),
                        condition: None,
                    }),
                    TransactionWrite::Put(PutItemInput {
                        table_name: "Authors".into(),
                        table_id: author_table_id.clone(),
                        item: author_key.clone(),
                        condition: None,
                    }),
                ],
            }),
        )
        .await
        .unwrap();
    assert_eq!(committed.output.0, TransactionOutcome::Applied);
    let read = client
        .query::<TransactGet>(
            &target,
            Some(committed.receipt),
            Json(vec![
                GetItemInput {
                    table_name: "Books".into(),
                    table_id: book_table_id.clone(),
                    key: book_key.clone(),
                },
                GetItemInput {
                    table_name: "Authors".into(),
                    table_id: author_table_id.clone(),
                    key: author_key.clone(),
                },
            ]),
        )
        .await
        .unwrap();
    assert_eq!(
        read.output.0,
        TransactionGetOutcome::Found(vec![Some(book_key.clone()), Some(author_key)])
    );
    let rolled_back = Item::from([("id".into(), AttributeValue::S("rolled-back".into()))]);
    let conditional_tx = storage
        .transact_write_items(
            &[
                TransactWriteOp::Put {
                    key_info: &books,
                    item: &rolled_back,
                    condition: None,
                    maps: &maps,
                    return_values_on_ccf: Default::default(),
                    stream: None,
                },
                TransactWriteOp::Put {
                    key_info: &books,
                    item: &book_key,
                    condition: Some(&exists_condition),
                    maps: &maps,
                    return_values_on_ccf: ReturnValuesOnConditionCheckFailure::AllOld,
                    stream: None,
                },
            ],
            None,
        )
        .await;
    assert!(matches!(
        conditional_tx,
        Err(StorageError::TransactionCanceled(reasons))
            if reasons[1].code == "ConditionalCheckFailed" && reasons[1].item == Some(book_key.clone())
    ));
    assert_eq!(storage.get_item(&books, &rolled_back).await.unwrap(), None);
    let (first_items, continuation) = storage
        .scan(&books, Some(2), None, None, None, None)
        .await
        .unwrap();
    assert_eq!(first_items.len(), 2);
    let continuation = continuation.expect("scan should have another page");
    let (second_items, end) = storage
        .scan(&books, None, Some(&continuation), None, None, None)
        .await
        .unwrap();
    assert_eq!(second_items.len(), 1);
    assert_eq!(end, None);
    let scanned_ids: Vec<_> = first_items
        .into_iter()
        .chain(second_items)
        .map(|item| item["id"].clone())
        .collect();
    assert_eq!(
        scanned_ids,
        vec![
            AttributeValue::S("book-2".into()),
            AttributeValue::S("book-tx".into()),
            AttributeValue::S("conditioned".into()),
        ]
    );
    let query = KeyCondition {
        pk_path: vec![PathElement::Attribute("id".into())],
        pk_value: Expr::Placeholder("id".into()),
        extra_pk_conditions: Vec::new(),
        sk_condition: None,
        extra_sk_conditions: Vec::new(),
    };
    let query_maps = ExpressionMaps::new(
        HashMap::new(),
        HashMap::from([("id".into(), AttributeValue::S("book-tx".into()))]),
    );
    assert_eq!(
        storage
            .query(&books, &query, &query_maps, true, Some(1), None, None)
            .await
            .unwrap(),
        (vec![tx_book.clone()], None)
    );
    assert_eq!(
        storage
            .query(
                &books,
                &query,
                &query_maps,
                true,
                Some(1),
                Some(&tx_book),
                None,
            )
            .await
            .unwrap(),
        (Vec::new(), None)
    );
    let bulk_writes = (0..70)
        .map(|index| {
            TransactionWrite::Put(PutItemInput {
                table_name: "Books".into(),
                table_id: book_table_id.clone(),
                item: Item::from([("id".into(), AttributeValue::S(format!("bulk-{index:03}")))]),
                condition: None,
            })
        })
        .collect();
    client
        .command::<TransactWrite>(
            &target,
            identity(11),
            Json(TransactWriteInput {
                operations: bulk_writes,
            }),
        )
        .await
        .unwrap();
    let (bulk_first, bulk_cursor) = storage
        .scan(&books, Some(70), None, None, None, None)
        .await
        .unwrap();
    assert_eq!(bulk_first.len(), 70);
    let (bulk_second, bulk_end) = storage
        .scan(&books, None, bulk_cursor.as_ref(), None, None, None)
        .await
        .unwrap();
    assert_eq!((bulk_second.len(), bulk_end), (3, None));
    let first_page = client
        .query::<ListTables>(
            &target,
            Some(committed.receipt),
            Json(ListTablesInput {
                limit: 1,
                exclusive_start: None,
            }),
        )
        .await
        .unwrap();
    let ListTablesOutcome::Page(first_page) = first_page.output.0 else {
        panic!("expected first table page");
    };
    assert_eq!(first_page.names, vec!["Authors"]);
    assert_eq!(first_page.last_evaluated.as_deref(), Some("Authors"));
    let second_page = client
        .query::<ListTables>(
            &target,
            Some(committed.receipt),
            Json(ListTablesInput {
                limit: 1,
                exclusive_start: first_page.last_evaluated,
            }),
        )
        .await
        .unwrap();
    let ListTablesOutcome::Page(second_page) = second_page.output.0 else {
        panic!("expected second table page");
    };
    assert_eq!(second_page.names, vec!["Books"]);
    assert_eq!(second_page.last_evaluated, None);
    let protected = storage
        .delete_table(
            "123456789012",
            DeleteTableInput {
                table_name: "Authors".into(),
            },
        )
        .await;
    assert!(matches!(
        protected,
        Err(StorageError::DeletionProtected(name)) if name == "Authors"
    ));
    let update: UpdateTableInput = serde_json::from_value(serde_json::json!({
        "TableName": "Authors",
        "DeletionProtectionEnabled": false
    }))
    .unwrap();
    let updated = storage.update_table("123456789012", update).await.unwrap();
    assert!(!updated.deletion_protection_enabled);
    let removed = storage
        .delete_table(
            "123456789012",
            DeleteTableInput {
                table_name: "Authors".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(removed.table_status, TableStatus::Deleting);
    handle.drain().await.unwrap();
    host.shutdown().await.unwrap();

    let next_session = SessionId::from_bytes([10; 16]);
    let restored_host = CellNodeBuilder::new(Arc::clone(&application))
        .with_runtime(SqlWorkerPool::new(1, 8).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(Host::default().with_local_disk_budget(DiskBudget::new(1 << 30)))
        .with_session(next_session)
        .build_unleased_for_maintenance()
        .unwrap();
    let catalog = CellCatalog::new(layout.clone(), target.tenant());
    let proof = catalog.lookup(target.cell_id()).await.unwrap().unwrap();
    let authority = CellAuthority::new(layout.clone());
    let idle = authority.load(target.cell_id()).await.unwrap().unwrap();
    let restored = restored_host
        .runtime()
        .acquire_idle_restored(
            proof,
            CellReplica::new(
                layout,
                *target.cell_id().as_bytes(),
                *incarnation.as_bytes(),
                Limits::default(),
            )
            .unwrap(),
            authority,
            idle,
            directory.path().join("restored-account.sqlite"),
            Owner {
                session: next_session,
                endpoint: "https://restored-beyonddb.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let restored_client = restored_host
        .application_handle::<Beyonddb>(
            CellClient::local(application.registry(), restored),
            target.tenant(),
            target.application(),
        )
        .unwrap();
    let persisted = restored_client
        .query::<GetItem>(
            &target,
            Some(committed.receipt),
            Json(GetItemInput {
                table_name: "Books".into(),
                table_id: book_table_id.clone(),
                key: book_key.clone(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(persisted.output.0, GetItemOutcome::Found(Some(book_key)));
    let persisted_update = restored_client
        .query::<GetItem>(
            &target,
            None,
            Json(GetItemInput {
                table_name: "Books".into(),
                table_id: book_table_id,
                key: conditional_key,
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        persisted_update.output.0,
        GetItemOutcome::Found(Some(expected_updated))
    );
    let absent_table = restored_client
        .query::<DescribeTable>(&target, None, Json("Authors".into()))
        .await
        .unwrap();
    assert_eq!(absent_table.output.0, None);
    restored_host.shutdown().await.unwrap();
}
