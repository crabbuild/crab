use super::*;
use aws_sdk_dynamodb::types::{
    AttributeDefinition, BillingMode, GlobalSecondaryIndex, KeySchemaElement, KeyType,
    LocalSecondaryIndex, Projection, ProjectionType, ScalarAttributeType,
};
use extenddb_storage::MetadataEngine;

const ACCOUNT: &str = "123456789012";
const TABLE: &str = "ResidencyStatistics";

fn key(name: &str, kind: KeyType) -> KeySchemaElement {
    KeySchemaElement::builder()
        .attribute_name(name)
        .key_type(kind)
        .build()
        .unwrap()
}

async fn description(sdk: &aws_sdk_dynamodb::Client) -> (i64, i64, i64, i64, i64, i64) {
    let table = sdk
        .describe_table()
        .table_name(TABLE)
        .send()
        .await
        .unwrap()
        .table
        .unwrap();
    let local = &table.local_secondary_indexes()[0];
    let global = &table.global_secondary_indexes()[0];
    (
        table.item_count.unwrap(),
        table.table_size_bytes.unwrap(),
        local.item_count.unwrap(),
        local.index_size_bytes.unwrap(),
        global.item_count.unwrap(),
        global.index_size_bytes.unwrap(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_statistics_count_sparse_indexes_and_survive_restore_and_splits() {
    let fixture = Fixture::with_partition_count(1).await;
    let sdk = aws_sdk_dynamodb::Client::from_conf(
        fixture
            .sdk
            .config()
            .to_builder()
            .retry_config(aws_sdk_dynamodb::config::retry::RetryConfig::disabled())
            .build(),
    );
    let all = Projection::builder()
        .projection_type(ProjectionType::All)
        .build();
    let mut create = sdk
        .create_table()
        .table_name(TABLE)
        .billing_mode(BillingMode::PayPerRequest)
        .key_schema(key("id", KeyType::Hash))
        .key_schema(key("sort", KeyType::Range))
        .local_secondary_indexes(
            LocalSecondaryIndex::builder()
                .index_name("ByLocal")
                .key_schema(key("id", KeyType::Hash))
                .key_schema(key("local", KeyType::Range))
                .projection(all)
                .build()
                .unwrap(),
        )
        .global_secondary_indexes(
            GlobalSecondaryIndex::builder()
                .index_name("ByGlobal")
                .key_schema(key("global", KeyType::Hash))
                .projection(
                    Projection::builder()
                        .projection_type(ProjectionType::KeysOnly)
                        .build(),
                )
                .build()
                .unwrap(),
        );
    for name in ["id", "sort", "local", "global"] {
        create = create.attribute_definitions(
            AttributeDefinition::builder()
                .attribute_name(name)
                .attribute_type(ScalarAttributeType::S)
                .build()
                .unwrap(),
        );
    }
    create.send().await.unwrap();
    let account = account_target(ACCOUNT).unwrap();
    let table = fixture
        .client
        .query::<DescribeTable>(&account, None, Json(TABLE.into()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let source = beyonddb::data_target(ACCOUNT, &table.id, &[0; 16]).unwrap();
    let storage = beyonddb::CellStorage::new(fixture.client.clone(), "us-east-1");
    let mut items = Vec::new();
    for n in 0..4 {
        let mut item = HashMap::from([
            ("id".into(), AwsAttributeValue::S(format!("key-{n}"))),
            ("sort".into(), AwsAttributeValue::S("row".into())),
            (
                "payload".into(),
                AwsAttributeValue::B(aws_sdk_dynamodb::primitives::Blob::new([0, 1, 2, 255])),
            ),
            (
                "nested".into(),
                AwsAttributeValue::L(vec![
                    AwsAttributeValue::Bool(true),
                    AwsAttributeValue::N("12345".into()),
                ]),
            ),
        ]);
        if n < 3 {
            item.insert("local".into(), AwsAttributeValue::S(format!("local-{n}")));
        }
        if n < 2 {
            item.insert("global".into(), AwsAttributeValue::S(format!("global-{n}")));
        }
        sdk.put_item()
            .table_name(TABLE)
            .set_item(Some(item.clone()))
            .send()
            .await
            .unwrap();
        items.push(item);
    }
    while storage
        .project_index_changes(ACCOUNT, &source, &table.id)
        .await
        .unwrap()
    {}
    // Independent expected byte totals: UTF-8 names/strings, raw binary, and
    // nested-list overhead. GSI KEYS_ONLY contains id, sort and global only.
    let base_bytes = 4 * (2 + 5 + 4 + 3 + 7 + 4 + 6 + 3 + 2 + 5) + 3 * (5 + 7) + 2 * (6 + 8);
    let local_bytes = base_bytes - (2 + 5 + 4 + 3 + 7 + 4 + 6 + 3 + 2 + 5);
    let global_bytes = 2 * (2 + 5 + 4 + 3 + 6 + 8);
    let expected = (4, base_bytes, 3, local_bytes, 2, global_bytes);
    // Exercise the installed background worker, without manually refreshing.
    let workers = CellNodeTaskGroup::new(CancellationToken::new(), CancellationToken::new());
    storage
        .install_statistics_loop(&workers, vec![ACCOUNT.into()])
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if description(&sdk).await == expected {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("statistics did not converge");
    workers.drain().await.unwrap();
    // Split after publication, retaining sealed sources. Only serving children
    // contribute to the next sample; source counters remain nonzero.
    let plan = fixture
        .provisioner
        .split_partition(ACCOUNT, fixture.client.clone(), &table.id, [0; 16])
        .await
        .unwrap();
    let index = &table.global_secondary_indexes[0];
    let interrupted = beyonddb::CellStorage::new(
        fixture
            .client
            .clone()
            .with_local_resolver(Arc::new(SplitDuringSample {
                client: fixture.client.clone(),
                provisioner: fixture.provisioner.clone(),
                table_id: table.id.clone(),
                index_id: Some(index.id.clone()),
                source: beyonddb::global_index_target(ACCOUNT, &index.id, &[0; 16]).unwrap(),
                fired: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            })),
        "us-east-1",
    );
    assert!(matches!(
        interrupted.refresh_table_size(ACCOUNT, TABLE).await,
        Err(extenddb_storage::error::StorageError::Transient(_))
    ));
    storage.refresh_table_size(ACCOUNT, TABLE).await.unwrap();
    assert_eq!(description(&sdk).await, expected);
    fixture
        .provisioner
        .admit_account(ACCOUNT)
        .await
        .unwrap()
        .drain()
        .await
        .unwrap();
    assert_eq!(description(&sdk).await, expected);
    sdk.delete_item()
        .table_name(TABLE)
        .key("id", items[0]["id"].clone())
        .key("sort", items[0]["sort"].clone())
        .send()
        .await
        .unwrap();
    sdk.update_item()
        .table_name(TABLE)
        .key("id", items[1]["id"].clone())
        .key("sort", items[1]["sort"].clone())
        .update_expression("REMOVE #l, #g")
        .expression_attribute_names("#l", "local")
        .expression_attribute_names("#g", "global")
        .send()
        .await
        .unwrap();
    for child in &plan.children {
        let target = beyonddb::data_target(ACCOUNT, &table.id, &child.partition_id).unwrap();
        while storage
            .project_index_changes(ACCOUNT, &target, &table.id)
            .await
            .unwrap()
        {}
    }
    storage.refresh_table_size(ACCOUNT, TABLE).await.unwrap();
    let remaining_bytes = base_bytes - 67 - 26;
    assert_eq!(description(&sdk).await, (3, remaining_bytes, 1, 53, 0, 0));
    let deleted = sdk
        .delete_table()
        .table_name(TABLE)
        .send()
        .await
        .unwrap()
        .table_description
        .unwrap();
    assert_eq!(deleted.item_count, Some(3));
    assert_eq!(deleted.table_size_bytes, Some(remaining_bytes));
    assert!(sdk.describe_table().table_name(TABLE).send().await.is_err());
    sdk.create_table()
        .table_name(TABLE)
        .billing_mode(BillingMode::PayPerRequest)
        .key_schema(key("id", KeyType::Hash))
        .attribute_definitions(
            AttributeDefinition::builder()
                .attribute_name("id")
                .attribute_type(ScalarAttributeType::S)
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    let recreated = sdk
        .describe_table()
        .table_name(TABLE)
        .send()
        .await
        .unwrap()
        .table
        .unwrap();
    assert_eq!(
        (recreated.item_count, recreated.table_size_bytes),
        (Some(0), Some(0))
    );
    fixture.shutdown().await;
}

struct SplitDuringSample {
    client: CellClient,
    provisioner: Arc<CellInitialPartitionProvisioner>,
    table_id: String,
    index_id: Option<String>,
    source: crab_cell_runtime::identity::CellTarget,
    fired: Arc<std::sync::atomic::AtomicBool>,
}

impl crab_cell_runtime::client::LocalCellResolver for SplitDuringSample {
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
        let client = self.client.clone();
        let provisioner = self.provisioner.clone();
        let table_id = self.table_id.clone();
        let index_id = self.index_id.clone();
        let split =
            target == self.source && !self.fired.swap(true, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            if split {
                if let Some(index_id) = index_id {
                    provisioner
                        .split_global_index_partition(ACCOUNT, client, &index_id, [0; 16])
                        .await
                        .unwrap();
                } else {
                    provisioner
                        .split_partition(ACCOUNT, client, &table_id, [0; 16])
                        .await
                        .unwrap();
                }
            }
            Ok(None)
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn statistics_reject_sample_when_split_changes_directory_before_publication() {
    let fixture = Fixture::with_partition_count(1).await;
    let account = account_target(ACCOUNT).unwrap();
    let table = fixture
        .client
        .query::<DescribeTable>(&account, None, Json("Residency".into()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let client = fixture
        .client
        .clone()
        .with_local_resolver(Arc::new(SplitDuringSample {
            client: fixture.client.clone(),
            provisioner: fixture.provisioner.clone(),
            source: beyonddb::data_target(ACCOUNT, &table.id, &[0; 16]).unwrap(),
            table_id: table.id,
            index_id: None,
            fired: fired.clone(),
        }));
    let storage = beyonddb::CellStorage::new(client, "us-east-1");
    assert!(matches!(
        storage.refresh_table_size(ACCOUNT, "Residency").await,
        Err(extenddb_storage::error::StorageError::Transient(_))
    ));
    assert!(fired.load(std::sync::atomic::Ordering::SeqCst));
    let describe = fixture
        .sdk
        .describe_table()
        .table_name("Residency")
        .send()
        .await
        .unwrap()
        .table
        .unwrap();
    assert_eq!(describe.item_count, Some(0));
    storage
        .refresh_table_size(ACCOUNT, "Residency")
        .await
        .unwrap();
    let describe = fixture
        .sdk
        .describe_table()
        .table_name("Residency")
        .send()
        .await
        .unwrap()
        .table
        .unwrap();
    assert_eq!(describe.item_count, Some(1));
    fixture.shutdown().await;
}
