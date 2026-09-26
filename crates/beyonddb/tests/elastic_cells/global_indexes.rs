use crate::*;
use beyonddb::{
    ApplyGlobalIndexMutation, GlobalIndexApplyOutcome, GlobalIndexMutation, IndexChangeChunk,
    ProjectionVersion, ReadPartitionIndexChange, ReadPartitionIndexChangeChunk,
    global_index_target,
};
use extenddb_core::types::TableKeyInfo;

const ACCOUNT: &str = "123456789012";

fn mutation() -> MutationIdentity {
    MutationIdentity {
        request_id: RequestId::from_bytes(*uuid::Uuid::now_v7().as_bytes()),
        ..identity(89)
    }
}

fn owner(
    application: &Arc<crab_cell_app::CompiledApplication>,
    layout: &CellStorageLayout,
    session: SessionId,
    directory: &std::path::Path,
) -> (
    crab_cell_host::CellNode,
    Arc<CellInitialPartitionProvisioner>,
    CellClient,
    CellStorage,
) {
    let host = CellNodeBuilder::new(application.clone())
        .with_runtime(SqlWorkerPool::new(1, 16).unwrap(), 16 * 1024 * 1024)
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
            "http://index-recovery.internal".into(),
            directory.into(),
        )
        .unwrap()
        .with_initial_partition_count(2)
        .unwrap(),
    );
    let client = CellClient::local_runtime(application.registry(), host.runtime(), layout.clone());
    let storage = CellStorage::new(client.clone(), "us-east-1")
        .with_initial_partitions(provisioner.clone())
        .with_transaction_coordinators(provisioner.clone());
    (host, provisioner, client, storage)
}

async fn journal(
    client: &CellClient,
    source: &CellTarget,
    table: &str,
) -> Option<serde_json::Value> {
    let header = client
        .query::<ReadPartitionIndexChange>(source, None, Json(table.into()))
        .await
        .unwrap()
        .output
        .0?;
    let mut bytes = Vec::new();
    while bytes.len() < header.bytes as usize {
        let part = client
            .query::<ReadPartitionIndexChangeChunk>(
                source,
                None,
                Json(IndexChangeChunk {
                    id: header.id,
                    offset: bytes.len() as u32,
                }),
            )
            .await
            .unwrap()
            .output
            .0
            .unwrap();
        assert!(!part.is_empty() && part.len() <= 128 * 1024);
        bytes.extend(part);
    }
    assert_eq!(bytes.len(), header.bytes as usize);
    Some(serde_json::from_slice(&bytes).unwrap())
}

async fn indexed_items(storage: &CellStorage, base: &TableKeyInfo) -> Vec<Item> {
    let info = TableKeyInfo {
        key_schema: base.global_secondary_indexes[0].key_schema.clone(),
        ..base.clone()
    };
    let condition = KeyCondition {
        pk_path: vec![PathElement::Attribute("group".into())],
        pk_value: Expr::Placeholder("g".into()),
        extra_pk_conditions: vec![],
        sk_condition: None,
        extra_sk_conditions: vec![],
    };
    let maps = ExpressionMaps::new(
        HashMap::new(),
        HashMap::from([("g".into(), AttributeValue::S("same".into()))]),
    );
    storage
        .query(&info, &condition, &maps, true, None, None, Some("ByGroup"))
        .await
        .unwrap()
        .0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journal_recovers_partial_projection_and_fences_delayed_images() {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "index-recovery".into(),
            cargo_lock_digest: Digest::from_bytes([1; 32]),
        })
        .unwrap(),
    );
    let account = account_target(ACCOUNT).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("index-recovery"),
        *account.application().as_bytes(),
    );
    let nodes = NodeDirectory::new(
        layout.clone(),
        Digest::from_bytes([71; 32]),
        Digest::from_bytes([72; 32]),
        Digest::from_bytes([73; 32]),
    );
    let (host, provisioner, client, storage) = owner(
        &application,
        &layout,
        SessionId::from_bytes([89; 16]),
        &directory.path().join("first"),
    );
    provisioner.admit_account(ACCOUNT).await.unwrap();
    storage.create_table(ACCOUNT, serde_json::from_value(serde_json::json!({
        "TableName":"JournalItems", "BillingMode":"PAY_PER_REQUEST",
        "KeySchema":[{"AttributeName":"id","KeyType":"HASH"}],
        "AttributeDefinitions":[{"AttributeName":"id","AttributeType":"S"},{"AttributeName":"group","AttributeType":"S"},{"AttributeName":"score","AttributeType":"N"},{"AttributeName":"bin","AttributeType":"B"}],
        "GlobalSecondaryIndexes":[{"IndexName":"ByGroup","KeySchema":[{"AttributeName":"group","KeyType":"HASH"},{"AttributeName":"score","KeyType":"RANGE"}],"Projection":{"ProjectionType":"ALL"}}, {"IndexName":"ByBinary","KeySchema":[{"AttributeName":"group","KeyType":"HASH"},{"AttributeName":"bin","KeyType":"RANGE"}],"Projection":{"ProjectionType":"ALL"}}]
    })).unwrap()).await.unwrap();
    let base = storage
        .table_key_info(ACCOUNT, "JournalItems")
        .await
        .unwrap();
    let route = client
        .query::<ReadTableRoute>(&account, None, Json(base.table_id.clone()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let original = Item::from([
        ("id".into(), AttributeValue::S("a".into())),
        ("group".into(), AttributeValue::S("same".into())),
        ("score".into(), AttributeValue::N("1".into())),
        ("bin".into(), AttributeValue::B(vec![0xff, 0xff, 1])),
        ("payload".into(), AttributeValue::S("x".repeat(350 * 1024))),
    ]);
    let hash = data_key_hash(&base.table_id, &original, &base.base_key_schema).unwrap();
    let range = route
        .partitions
        .iter()
        .find(|range| {
            range.lower.is_none_or(|low| hash >= low) && range.upper.is_none_or(|high| hash < high)
        })
        .unwrap();
    let source = data_target(ACCOUNT, &base.table_id, &range.partition_id).unwrap();
    storage
        .put_item(
            &base,
            original.clone(),
            false,
            None,
            &ExpressionMaps::default(),
            None,
        )
        .await
        .unwrap();
    assert!(
        indexed_items(&storage, &base).await.is_empty(),
        "base commit must enqueue durable asynchronous work"
    );
    let first = journal(&client, &source, &base.table_id).await.unwrap();
    let index = &range.table.global_secondary_indexes[0];
    let index_hash = data_key_hash(&index.id, &original, &index.specification.key_schema).unwrap();
    let index_ranges = provisioner
        .provision_global_index(ACCOUNT, &range.table, index)
        .await
        .unwrap();
    let index_range = index_ranges
        .iter()
        .find(|range| {
            range.lower.is_none_or(|low| index_hash >= low)
                && range.upper.is_none_or(|high| index_hash < high)
        })
        .unwrap();
    let destination = global_index_target(ACCOUNT, &index.id, &index_range.partition_id).unwrap();
    let entry_key = |item: &Item| {
        Item::from([
            ("id".into(), item["id"].clone()),
            ("group".into(), item["group"].clone()),
            ("score".into(), item["score"].clone()),
        ])
    };
    let first_mutation = GlobalIndexMutation {
        index_id: index.id.clone(),
        epoch: index_range.epoch,
        key: entry_key(&original),
        version: serde_json::from_value(first["version"].clone()).unwrap(),
        item: Some(original.clone()),
    };
    for expected in [
        GlobalIndexApplyOutcome::Applied,
        GlobalIndexApplyOutcome::Replay,
    ] {
        assert_eq!(
            client
                .command::<ApplyGlobalIndexMutation>(
                    &destination,
                    mutation(),
                    Json(first_mutation.clone())
                )
                .await
                .unwrap()
                .output
                .0,
            expected
        );
    }
    let mut changed = original.clone();
    changed.insert("score".into(), AttributeValue::N("2".into()));
    storage
        .put_item(
            &base,
            changed.clone(),
            false,
            None,
            &ExpressionMaps::default(),
            None,
        )
        .await
        .unwrap();
    // A lost acknowledgement leaves the first image at the head. Replaying it
    // must advance the journal without replacing the already committed row.
    assert!(
        storage
            .project_index_changes(ACCOUNT, &source, &base.table_id)
            .await
            .unwrap()
    );
    let next = journal(&client, &source, &base.table_id).await.unwrap();
    let next_version: ProjectionVersion = serde_json::from_value(next["version"].clone()).unwrap();
    assert!(next_version > first_mutation.version);
    assert!(serde_json::to_vec(&next).unwrap().len() > 700 * 1024);
    let tombstone = GlobalIndexMutation {
        version: next_version,
        item: None,
        ..first_mutation.clone()
    };
    client
        .command::<ApplyGlobalIndexMutation>(&destination, mutation(), Json(tombstone))
        .await
        .unwrap();
    assert!(indexed_items(&storage, &base).await.is_empty());
    let lower = u128::from_be_bytes(range.lower.unwrap_or([0; 16]));
    let upper = range.upper.map(u128::from_be_bytes).unwrap_or(u128::MAX);
    let seal = PartitionSeal {
        table_id: base.table_id.clone(),
        source_partition_id: range.partition_id,
        epoch: range.epoch,
        next_epoch: 2,
        source_lower: range.lower,
        source_upper: range.upper,
        boundary: (lower + (upper - lower) / 2).to_be_bytes(),
        left_partition_id: [81; 16],
        right_partition_id: [82; 16],
    };
    let Err(InvocationError::Rejected(refused)) = client
        .command::<SealPartition>(&source, mutation(), Json(seal))
        .await
    else {
        panic!("split ignored pending projection");
    };
    assert_eq!(refused.output.0, SealPartitionOutcome::PendingIndexChanges);
    host.shutdown().await.unwrap();
    let (host, provisioner, client, storage) = owner(
        &application,
        &layout,
        SessionId::from_bytes([90; 16]),
        &directory.path().join("second"),
    );
    let account_handle = provisioner.admit_account(ACCOUNT).await.unwrap();
    provisioner
        .recover_registered_partitions(ACCOUNT, account_handle, &nodes)
        .await
        .unwrap();
    assert!(
        storage
            .project_index_changes(ACCOUNT, &source, &base.table_id)
            .await
            .unwrap()
    );
    assert!(journal(&client, &source, &base.table_id).await.is_none());
    assert_eq!(indexed_items(&storage, &base).await, vec![changed.clone()]);
    assert_eq!(
        client
            .command::<ApplyGlobalIndexMutation>(&destination, mutation(), Json(first_mutation))
            .await
            .unwrap()
            .output
            .0,
        GlobalIndexApplyOutcome::Superseded
    );
    assert_eq!(indexed_items(&storage, &base).await, vec![changed.clone()]);
    let binary = TableKeyInfo {
        key_schema: base.global_secondary_indexes[1].key_schema.clone(),
        ..base.clone()
    };
    for prefix in [vec![], vec![0xff], vec![0xff, 0xff], vec![0xff, 0xff, 1]] {
        let condition = KeyCondition {
            pk_path: vec![PathElement::Attribute("group".into())],
            pk_value: Expr::Placeholder("g".into()),
            extra_pk_conditions: vec![],
            sk_condition: Some(SortKeyCondition::BeginsWith {
                path: vec![PathElement::Attribute("bin".into())],
                prefix: Expr::Placeholder("prefix".into()),
            }),
            extra_sk_conditions: vec![],
        };
        let maps = ExpressionMaps::new(
            HashMap::new(),
            HashMap::from([
                ("g".into(), AttributeValue::S("same".into())),
                ("prefix".into(), AttributeValue::B(prefix)),
            ]),
        );
        let (items, _) = storage
            .query(
                &binary,
                &condition,
                &maps,
                true,
                None,
                None,
                Some("ByBinary"),
            )
            .await
            .unwrap();
        assert_eq!(items, vec![changed.clone()]);
    }
    host.shutdown().await.unwrap();
}
