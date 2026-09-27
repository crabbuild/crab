use super::*;
use aws_sdk_dynamodb::types::{
    AttributeDefinition, BillingMode, GlobalSecondaryIndex, KeySchemaElement, KeyType, Projection,
    ProjectionType, ScalarAttributeType,
};
use beyonddb::{
    GlobalIndexState, PartitionState, ReadGlobalIndexState, ReadPartitionState, data_target,
    global_index_target,
};

const ACCOUNT: &str = "123456789012";

async fn create(sdk: &aws_sdk_dynamodb::Client, name: &str, index: bool) {
    let key = KeySchemaElement::builder()
        .attribute_name("id")
        .key_type(KeyType::Hash)
        .build()
        .unwrap();
    let mut request = sdk
        .create_table()
        .table_name(name)
        .billing_mode(BillingMode::PayPerRequest)
        .key_schema(key.clone())
        .attribute_definitions(
            AttributeDefinition::builder()
                .attribute_name("id")
                .attribute_type(ScalarAttributeType::S)
                .build()
                .unwrap(),
        );
    if index {
        request = request.global_secondary_indexes(
            GlobalSecondaryIndex::builder()
                .index_name("ById")
                .key_schema(key)
                .projection(
                    Projection::builder()
                        .projection_type(ProjectionType::All)
                        .build(),
                )
                .build()
                .unwrap(),
        );
    }
    request.send().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_split_sources_release_capacity_and_retain_recoverable_roots() {
    let fixture = Fixture::with_partition_count(1).await;
    let sdk = aws_sdk_dynamodb::Client::from_conf(
        fixture
            .sdk
            .config()
            .to_builder()
            .retry_config(aws_sdk_dynamodb::config::retry::RetryConfig::disabled())
            .build(),
    );
    create(&sdk, "ResidencyIndexed", true).await;
    let account = account_target(ACCOUNT).unwrap();
    let mut records = Vec::new();
    for name in ["Residency", "ResidencyIndexed"] {
        records.push(
            fixture
                .client
                .query::<DescribeTable>(&account, None, Json(name.into()))
                .await
                .unwrap()
                .output
                .0
                .unwrap(),
        );
    }
    let table = &records[0];
    let range = fixture
        .client
        .query::<ReadTableRoute>(&account, None, Json(table.id.clone()))
        .await
        .unwrap()
        .output
        .0
        .unwrap()
        .partitions
        .remove(0);
    let base = fixture
        .provisioner
        .split_partition(
            ACCOUNT,
            fixture.client.clone(),
            &table.id,
            range.partition_id,
        )
        .await
        .unwrap();
    assert_eq!(fixture.node.runtime().stats().active_cells(), 7);
    let indexed = &records[1];
    let index = &indexed.global_secondary_indexes[0];
    let index_range = fixture
        .client
        .query::<ReadTableRoute>(&account, None, Json(indexed.id.clone()))
        .await
        .unwrap()
        .output
        .0
        .unwrap()
        .partitions
        .remove(0);
    let indexed_item = SdkItem::from([("id".into(), AwsAttributeValue::S("indexed".into()))]);
    sdk.put_item()
        .table_name("ResidencyIndexed")
        .set_item(Some(indexed_item.clone()))
        .send()
        .await
        .unwrap();
    let index_base = data_target(ACCOUNT, &indexed.id, &index_range.partition_id).unwrap();
    assert!(
        beyonddb::CellStorage::new(fixture.client.clone(), "us-east-1")
            .project_index_changes(ACCOUNT, &index_base, &indexed.id)
            .await
            .unwrap()
    );
    // Only one slot remains, but this split needs two children. The already
    // published base split must yield its sealed source's slot at admission.
    let split = fixture
        .provisioner
        .split_global_index_partition(
            ACCOUNT,
            fixture.client.clone(),
            &index.id,
            index_range.partition_id,
        )
        .await
        .unwrap();
    let source = data_target(ACCOUNT, &table.id, &base.source.partition_id).unwrap();
    let index_source = global_index_target(ACCOUNT, &index.id, &split.source.partition_id).unwrap();
    let authority = CellAuthority::new(fixture.layout.clone());
    let released = authority.load(source.cell_id()).await.unwrap().unwrap();
    assert!(released.value().owner.is_none());
    assert!(released.value().root.is_some());
    assert_eq!(fixture.node.runtime().stats().active_cells(), 8);
    // Finished-plan replay must not reacquire historical sources when every
    // slot belongs to a live range. Only unfinished transfers need their exports.
    fixture
        .provisioner
        .resume_split(ACCOUNT, fixture.client.clone(), &base)
        .await
        .unwrap();
    fixture
        .provisioner
        .resume_global_index_split(ACCOUNT, fixture.client.clone(), &split)
        .await
        .unwrap();
    assert!(
        authority
            .load(source.cell_id())
            .await
            .unwrap()
            .unwrap()
            .value()
            .owner
            .is_none()
    );
    // Reuse the completed index source's slot through an ordinary SDK action.
    create(&sdk, "ResidencyAfterSplit", false).await;
    let released = authority
        .load(index_source.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert!(released.value().owner.is_none());
    assert!(released.value().root.is_some());
    for (_, item) in &fixture.data {
        assert_eq!(
            sdk.get_item()
                .table_name("Residency")
                .key("id", item["id"].clone())
                .consistent_read(true)
                .send()
                .await
                .unwrap()
                .item
                .as_ref(),
            Some(item)
        );
    }
    // Free two current owners, then restore both old source roots through
    // routed queries. Their durable seals must survive local reclamation.
    for child in &base.children {
        fixture
            .provisioner
            .admit_existing_partition(ACCOUNT, &table.id, &child.partition_id)
            .await
            .unwrap()
            .drain()
            .await
            .unwrap();
    }
    let state = fixture
        .client
        .query::<ReadPartitionState>(&source, None, Json(()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    assert!(matches!(state.state, PartitionState::Sealed(_)));
    let state = fixture
        .client
        .query::<ReadGlobalIndexState>(&index_source, None, Json(()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    assert_eq!(state, GlobalIndexState::Sealed(split.clone()));
    // Restore the serving ranges too, proving SDK data remains readable from
    // durable children after both historical and current owners have closed.
    for target in [&source, &index_source].into_iter().chain(
        split
            .children
            .iter()
            .map(|child| global_index_target(ACCOUNT, &index.id, &child.partition_id).unwrap())
            .collect::<Vec<_>>()
            .iter(),
    ) {
        let observed = authority.load(target.cell_id()).await.unwrap().unwrap();
        let proof = crab_cell_runtime::cell::catalog::CellCatalog::new(
            fixture.layout.clone(),
            target.tenant(),
        )
        .lookup(target.cell_id())
        .await
        .unwrap()
        .unwrap();
        fixture
            .node
            .runtime()
            .local_handle(proof, &observed)
            .await
            .unwrap()
            .unwrap()
            .drain()
            .await
            .unwrap();
    }
    assert_eq!(
        sdk.scan()
            .table_name("ResidencyIndexed")
            .index_name("ById")
            .send()
            .await
            .unwrap()
            .items,
        Some(vec![indexed_item])
    );
    for (_, item) in &fixture.data {
        assert_eq!(
            sdk.get_item()
                .table_name("Residency")
                .key("id", item["id"].clone())
                .consistent_read(true)
                .send()
                .await
                .unwrap()
                .item
                .as_ref(),
            Some(item)
        );
    }
    fixture.shutdown().await;
}
