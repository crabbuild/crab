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

pub(super) async fn create(sdk: &aws_sdk_dynamodb::Client, name: &str, index: bool) {
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
    // Base roots for both initial tables add two owners to the prior nine-slot workload.
    let fixture = Fixture::with_capacity(1, 11).await;
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
    let range = crate::single_leaf_route(&fixture.client, &account, &table.id.clone())
        .await
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
            range.lower.unwrap_or([0; 16]),
        )
        .await
        .unwrap();
    assert_eq!(fixture.node.runtime().stats().active_cells(), 10);
    let indexed = &records[1];
    let index = &indexed.global_secondary_indexes[0];
    let index_range = crate::single_leaf_route(&fixture.client, &account, &indexed.id.clone())
        .await
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
            index_range.lower.unwrap_or([0; 16]),
        )
        .await
        .unwrap();
    let source = data_target(ACCOUNT, &table.id, &base.source.partition_id).unwrap();
    let index_source = global_index_target(ACCOUNT, &index.id, &split.source.partition_id).unwrap();
    let authority = CellAuthority::new(fixture.layout.clone());
    let released = authority.load(source.cell_id()).await.unwrap().unwrap();
    assert!(released.value().owner.is_none());
    assert!(released.value().root.is_some());
    assert_eq!(fixture.node.runtime().stats().active_cells(), 11);
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
    for child in &split.children {
        let target = global_index_target(ACCOUNT, &index.id, &child.partition_id).unwrap();
        let observed = authority.load(target.cell_id()).await.unwrap().unwrap();
        let proof = crab_cell_runtime::cell::catalog::CellCatalog::new(
            fixture.layout.clone(),
            target.tenant(),
        )
        .lookup(target.cell_id())
        .await
        .unwrap()
        .unwrap();
        if let Some(handle) = fixture
            .node
            .runtime()
            .local_handle(proof, &observed)
            .await
            .unwrap()
        {
            handle.drain().await.unwrap();
        }
        // Admission may already have released a child while restoring history.
        // Both paths must leave a recoverable root for the following SDK scan.
        let released = authority.load(target.cell_id()).await.unwrap().unwrap();
        assert!(released.value().owner.is_none() && released.value().root.is_some());
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
    // Every slot is occupied again, including both retired sources. Ordinary
    // reads must reclaim those slots before placement can restore live children.
    assert_eq!(fixture.node.runtime().stats().active_cells(), 11);
    assert_eq!(
        sdk.scan()
            .table_name("Residency")
            .consistent_read(true)
            .send()
            .await
            .unwrap()
            .items,
        Some(fixture.data.iter().map(|(_, item)| item.clone()).collect())
    );
    for target in [&source, &index_source] {
        let control = authority.load(target.cell_id()).await.unwrap().unwrap();
        assert!(control.value().owner.is_none());
        assert!(control.value().root.is_some());
    }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_reads_restore_data_and_live_directory_with_one_available_slot() {
    let fixture = Fixture::with_capacity(1, 4).await;
    let account = account_target(ACCOUNT).unwrap();
    let table = fixture
        .client
        .query::<DescribeTable>(&account, None, Json("Residency".into()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let directory =
        beyonddb::directory_target(ACCOUNT, &beyonddb::DirectorySpec::root(table.id)).unwrap();
    let before = fixture
        .client
        .query::<beyonddb::ReadDirectory>(&directory, None, Json(()))
        .await
        .unwrap()
        .output;
    let (data, item) = &fixture.data[0];
    data.drain().await.unwrap();
    let blocker = fixture
        .provisioner
        .admit_credential("AKIADIRECTORYRESIDENCY")
        .await
        .unwrap();
    assert_eq!(fixture.node.runtime().stats().active_cells(), 4);
    let sdk = super::provisioning::sdk_without_retries(&fixture);
    let authority = CellAuthority::new(fixture.layout.clone());
    // Only one slot remains after account and credential ownership. Each SDK
    // read needs the directory, then data, without keeping either owner pinned.
    for _ in 0..2 {
        let result = sdk
            .get_item()
            .table_name("Residency")
            .key("id", item["id"].clone())
            .consistent_read(true)
            .send()
            .await
            .unwrap();
        assert_eq!(result.item.as_ref(), Some(item));
        let released = authority.load(directory.cell_id()).await.unwrap().unwrap();
        assert!(released.value().owner.is_none());
        assert!(released.value().root.is_some());
        let restored = fixture
            .client
            .query::<beyonddb::ReadDirectory>(&directory, None, Json(()))
            .await
            .unwrap()
            .output;
        assert_eq!(restored, before);
    }
    blocker.drain().await.unwrap();
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn participant_restore_at_capacity_does_not_require_resident_account_metadata() {
    let fixture = Fixture::with_capacity(2, 5).await;
    let source = fixture.data[0].0.clone();
    let account = account_target(ACCOUNT).unwrap();
    let table = fixture
        .client
        .query::<DescribeTable>(&account, None, Json("Residency".into()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let range = crate::single_leaf_route(&fixture.client, &account, &table.id)
        .await
        .unwrap()
        .partitions
        .remove(0);
    let target = data_target(ACCOUNT, &table.id, &range.partition_id).unwrap();
    fixture
        .provisioner
        .admit_account(ACCOUNT)
        .await
        .unwrap()
        .drain()
        .await
        .unwrap();
    source.drain().await.unwrap();
    let mut blockers = Vec::new();
    for key in ["AKIAPARTICIPANTRESTOREONE", "AKIAPARTICIPANTRESTORETWO"] {
        blockers.push(fixture.provisioner.admit_credential(key).await.unwrap());
    }
    assert_eq!(fixture.node.runtime().stats().active_cells(), 5);
    // Persisted transactions address original participants directly. Restoring
    // one must not depend on catalog residency or recursively admit metadata.
    let restored = fixture
        .client
        .query::<ReadPartitionState>(&target, None, Json(()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    assert_eq!(restored.spec.partition_id, range.partition_id);
    assert!(
        CellAuthority::new(fixture.layout.clone())
            .load(account.cell_id())
            .await
            .unwrap()
            .unwrap()
            .value()
            .owner
            .is_none()
    );
    for blocker in blockers {
        blocker.drain().await.unwrap();
    }
    let sdk = super::provisioning::sdk_without_retries(&fixture);
    for (_, item) in &fixture.data {
        let observed = sdk
            .get_item()
            .table_name("Residency")
            .key("id", item["id"].clone())
            .consistent_read(true)
            .send()
            .await
            .unwrap();
        assert_eq!(observed.item.as_ref(), Some(item));
    }
    fixture.shutdown().await;
}

struct ReleaseRangesDuringMetadataRead {
    account: crab_cell_runtime::identity::CellTarget,
    ranges: Vec<CellHandle>,
    fired: Arc<std::sync::atomic::AtomicBool>,
}

impl crab_cell_runtime::client::LocalCellResolver for ReleaseRangesDuringMetadataRead {
    fn resolve(
        &self,
        target: crab_cell_runtime::identity::CellTarget,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = crab_cell_runtime::Result<Option<CellHandle>>>
                + Send
                + 'static,
        >,
    > {
        let selected = target == self.account;
        let ranges = self.ranges.clone();
        let fired = self.fired.clone();
        Box::pin(async move {
            if selected && !fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
                for handle in ranges {
                    handle.drain().await?;
                }
            }
            Ok(None)
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn creation_reclamation_tolerates_owner_release_during_metadata_lookup() {
    let fixture = Fixture::with_capacity(2, 5).await;
    let sdk = super::provisioning::sdk_without_retries(&fixture);
    assert!(
        super::provisioning::create(&sdk, "ResidencyPending", false)
            .send()
            .await
            .is_err()
    );
    let account = account_target(ACCOUNT).unwrap();
    let table = fixture
        .client
        .query::<DescribeTable>(&account, None, Json("ResidencyPending".into()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let client =
        fixture
            .client
            .clone()
            .with_local_resolver(Arc::new(ReleaseRangesDuringMetadataRead {
                account,
                ranges: fixture
                    .data
                    .iter()
                    .map(|(handle, _)| handle.clone())
                    .collect(),
                fired: fired.clone(),
            }));
    // A concurrent release, or metadata restoration itself, can close ranges
    // after discovery. Reclamation must not read them from a stale local list.
    let partitions = fixture
        .provisioner
        .provision(&client, ACCOUNT, &table)
        .await
        .unwrap();
    assert_eq!(partitions.len(), 2);
    assert!(fired.load(std::sync::atomic::Ordering::SeqCst));
    for (_, item) in &fixture.data {
        let observed = sdk
            .get_item()
            .table_name("Residency")
            .key("id", item["id"].clone())
            .consistent_read(true)
            .send()
            .await
            .unwrap();
        assert_eq!(observed.item.as_ref(), Some(item));
    }
    fixture.shutdown().await;
}
