use crate::*;
use extenddb_core::types::DeleteTableInput;

const ACCOUNT: &str = "123456789012";

fn table(name: &str) -> extenddb_core::types::CreateTableInput {
    serde_json::from_value(serde_json::json!({
        "TableName": name,
        "BillingMode": "PAY_PER_REQUEST",
        "KeySchema": [{"AttributeName":"id", "KeyType":"HASH"}],
        "AttributeDefinitions": [{"AttributeName":"id", "AttributeType":"S"}],
        "GlobalSecondaryIndexes": [{
            "IndexName":"ById",
            "KeySchema":[{"AttributeName":"id", "KeyType":"HASH"}],
            "Projection":{"ProjectionType":"ALL"}
        }]
    }))
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleted_table_ranges_release_residency_without_releasing_live_ranges() {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "table-residency".into(),
            cargo_lock_digest: Digest::from_bytes([1; 32]),
        })
        .unwrap(),
    );
    let directory = tempfile::tempdir().unwrap();
    let account = account_target(ACCOUNT).unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("table-residency"),
        *account.application().as_bytes(),
    );
    let session = SessionId::from_bytes([198; 16]);
    let host = CellNodeBuilder::new(application.clone())
        .with_runtime(SqlWorkerPool::new(1, 5).unwrap(), 16 * 1024 * 1024)
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
            "https://table-residency.internal".into(),
            directory.path().into(),
        )
        .unwrap(),
    );
    provisioner.admit_account(ACCOUNT).await.unwrap();
    let client = CellClient::local_runtime(application.registry(), host.runtime(), layout.clone());
    let storage =
        CellStorage::new(client.clone(), "us-east-1").with_initial_partitions(provisioner.clone());
    storage
        .create_table(ACCOUNT, table("KeepAlive"))
        .await
        .unwrap();
    let live = storage.table_key_info(ACCOUNT, "KeepAlive").await.unwrap();
    let item = Item::from([("id".into(), AttributeValue::S("present".into()))]);
    storage
        .put_item(
            &live,
            item.clone(),
            false,
            None,
            &ExpressionMaps::default(),
            None,
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(layout.clone());
    let mut retired = Vec::new();
    let mut history = None;
    for _ in 0..4 {
        storage
            .create_table(ACCOUNT, table("Recreated"))
            .await
            .unwrap();
        for target in &retired {
            let control = authority.load(*target).await.unwrap().unwrap();
            assert!(control.value().owner.is_none());
            assert!(
                control.value().root.is_some(),
                "reclamation must retain published history"
            );
        }
        assert_eq!(
            storage.get_item(&live, &item).await.unwrap(),
            Some(item.clone())
        );
        let record = client
            .query::<DescribeTable>(&account, None, Json("Recreated".into()))
            .await
            .unwrap()
            .output
            .0
            .unwrap();
        let range = client
            .query::<ReadTableRoute>(&account, None, Json(record.id.clone()))
            .await
            .unwrap()
            .output
            .0
            .unwrap()
            .partitions
            .remove(0);
        let info = storage.table_key_info(ACCOUNT, "Recreated").await.unwrap();
        assert!(storage.get_item(&info, &item).await.unwrap().is_none());
        storage
            .put_item(
                &info,
                item.clone(),
                false,
                None,
                &ExpressionMaps::default(),
                None,
            )
            .await
            .unwrap();
        history.get_or_insert((record.clone(), range.clone()));
        retired = vec![
            data_target(ACCOUNT, &record.id, &range.partition_id)
                .unwrap()
                .cell_id(),
            beyonddb::global_index_target(
                ACCOUNT,
                &record.global_secondary_indexes[0].id,
                &range.partition_id,
            )
            .unwrap()
            .cell_id(),
        ];
        assert_eq!(host.runtime().stats().active_cells(), 5);
        storage
            .delete_table(
                ACCOUNT,
                DeleteTableInput {
                    table_name: "Recreated".into(),
                },
            )
            .await
            .unwrap();
    }
    // Recovery can still restore an original participant after its public
    // table name has been deleted and reused for other generations.
    let (record, range) = history.unwrap();
    provisioner
        .provision(&client, ACCOUNT, &record)
        .await
        .unwrap();
    let target = data_target(ACCOUNT, &record.id, &range.partition_id).unwrap();
    let restored = client
        .query::<PartitionGet>(
            &target,
            None,
            Json(PartitionGetInput {
                table_id: record.id,
                epoch: range.epoch,
                key: item.clone(),
            }),
        )
        .await
        .unwrap()
        .output
        .0;
    assert_eq!(restored, PartitionGetOutcome::Found(Some(item)));
    host.shutdown().await.unwrap();
}
