use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_split_epochs_follow_source_lineage_across_independent_branches() {
    const ACCOUNT: &str = "123456789012";
    // Five final ranges plus the splitting source, account, credential and directory.
    let fixture = Fixture::with_capacity(2, 9).await;
    let account = account_target(ACCOUNT).unwrap();
    let table = fixture
        .client
        .query::<DescribeTable>(&account, None, Json("Residency".into()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let original = crate::single_leaf_route(&fixture.client, &account, &table.id.clone())
        .await
        .unwrap();
    let first = fixture
        .provisioner
        .split_partition(
            ACCOUNT,
            fixture.client.clone(),
            &table.id,
            original.partitions[0].partition_id,
            original.partitions[0].lower.unwrap_or([0; 16]),
        )
        .await
        .unwrap();
    let first_key = Item::from([(
        "id".into(),
        AttributeValue::S(fixture.data[0].1["id"].as_s().unwrap().clone()),
    )]);
    let hash = beyonddb::data_key_hash(&table.id, &first_key, &table.key_schema).unwrap();
    let nested_source = first
        .children
        .iter()
        .find(|range| {
            range.lower.is_none_or(|lower| lower <= hash)
                && range.upper.is_none_or(|upper| hash < upper)
        })
        .unwrap();
    let nested = fixture
        .provisioner
        .split_partition(
            ACCOUNT,
            fixture.client.clone(),
            &table.id,
            nested_source.partition_id,
            nested_source.lower.unwrap_or([0; 16]),
        )
        .await
        .unwrap();
    assert_eq!(nested.children[0].epoch, nested_source.epoch + 1);
    // This sibling has never moved, even though another branch split twice.
    let sibling = fixture
        .provisioner
        .split_partition(
            ACCOUNT,
            fixture.client.clone(),
            &table.id,
            original.partitions[1].partition_id,
            original.partitions[1].lower.unwrap_or([0; 16]),
        )
        .await
        .unwrap();
    assert_eq!(sibling.children[0].epoch, original.partitions[1].epoch + 1);
    assert_eq!(sibling.children[1].epoch, sibling.children[0].epoch);
    fixture
        .provisioner
        .admit_account(ACCOUNT)
        .await
        .unwrap()
        .drain()
        .await
        .unwrap();
    let replay = fixture
        .provisioner
        .split_partition(
            ACCOUNT,
            fixture.client.clone(),
            &table.id,
            sibling.source.partition_id,
            sibling.source.lower.unwrap_or([0; 16]),
        )
        .await
        .unwrap();
    assert_eq!(replay, sibling);
    let current = crate::single_leaf_route(&fixture.client, &account, &table.id.clone())
        .await
        .unwrap();
    assert_eq!(
        current.epoch,
        original.epoch + 3,
        "scan version still advances for every publication"
    );
    // Transaction admission must preserve each participant's own epoch even
    // when its peer belongs to a shallower branch of the same table.
    let mut transaction = fixture
        .sdk
        .transact_write_items()
        .client_request_token("independent-split-lineage");
    for (_, item) in &fixture.data {
        transaction = transaction.transact_items(
            aws_sdk_dynamodb::types::TransactWriteItem::builder()
                .update(
                    aws_sdk_dynamodb::types::Update::builder()
                        .table_name("Residency")
                        .key("id", item["id"].clone())
                        .update_expression("SET #v = :v")
                        .expression_attribute_names("#v", "version")
                        .expression_attribute_values(":v", AwsAttributeValue::S("committed".into()))
                        .build()
                        .unwrap(),
                )
                .build(),
        );
    }
    transaction.clone().send().await.unwrap();
    transaction.send().await.unwrap();
    for (_, item) in &fixture.data {
        let result = fixture
            .sdk
            .get_item()
            .table_name("Residency")
            .key("id", item["id"].clone())
            .consistent_read(true)
            .send()
            .await
            .unwrap();
        let mut expected = item.clone();
        expected.insert("version".into(), AwsAttributeValue::S("committed".into()));
        assert_eq!(result.item.as_ref(), Some(&expected));
    }
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_base_directory_tree_preserves_pages_and_transaction_replay_after_restore() {
    use beyonddb::{DirectorySpec, ReadDirectory, directory_target};
    use extenddb_storage::MetadataEngine;
    const ACCOUNT: &str = "123456789012";
    // Account, credential, three metadata nodes, serving ranges and coordinators
    // must coexist here; owner eviction is covered by the residency tests.
    let fixture = Fixture::with_capacity(2, 16).await;
    let account = account_target(ACCOUNT).unwrap();
    let table = fixture
        .client
        .query::<DescribeTable>(&account, None, Json("Residency".into()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let initial = crate::single_leaf_route(&fixture.client, &account, &table.id)
        .await
        .unwrap();
    let root = DirectorySpec::root(table.id.clone());
    let tree = fixture
        .provisioner
        .split_directory(&fixture.client, ACCOUNT, &root)
        .await
        .unwrap();
    let sdk = aws_sdk_dynamodb::Client::from_conf(
        fixture
            .sdk
            .config()
            .to_builder()
            .retry_config(aws_sdk_dynamodb::config::retry::RetryConfig::disabled())
            .build(),
    );
    let mut transaction = sdk
        .transact_write_items()
        .client_request_token("base-directory-restore");
    for (_, item) in &fixture.data {
        transaction = transaction.transact_items(
            aws_sdk_dynamodb::types::TransactWriteItem::builder()
                .update(
                    aws_sdk_dynamodb::types::Update::builder()
                        .table_name("Residency")
                        .key("id", item["id"].clone())
                        .update_expression("ADD visits :one")
                        .expression_attribute_values(":one", AwsAttributeValue::N("1".into()))
                        .build()
                        .unwrap(),
                )
                .build(),
        );
    }
    transaction.clone().send().await.unwrap();
    let source = &initial.partitions[0];
    let transfer = fixture
        .provisioner
        .split_partition(
            ACCOUNT,
            fixture.client.clone(),
            &table.id,
            source.partition_id,
            source.lower.unwrap_or([0; 16]),
        )
        .await
        .unwrap();
    let versions = futures_util::future::join_all(tree.children.iter().map(|spec| async {
        fixture
            .client
            .query::<ReadDirectory>(&directory_target(ACCOUNT, spec).unwrap(), None, Json(()))
            .await
            .unwrap()
            .output
            .0
            .unwrap()
            .version
    }))
    .await;
    assert_eq!(versions[0], versions[1] + 1);
    // Crossing a leaf boundary must adopt its neighbour's version, while a page
    // within a leaf remains pinned to that leaf's membership.
    let mut input = RoutePageInput {
        table_id: table.id.clone(),
        start_hash: None,
        after_lower: None,
        expected_epoch: None,
    };
    let mut ranges = Vec::new();
    loop {
        let RoutePageOutcome::Page {
            epoch,
            partitions,
            has_more,
        } = beyonddb::read_route_page(&fixture.client, &account, input.clone())
            .await
            .unwrap()
        else {
            panic!("stable tree must remain pageable")
        };
        input.after_lower = partitions.last().map(|range| range.lower);
        input.expected_epoch = Some(epoch);
        ranges.extend(partitions);
        if !has_more {
            break;
        }
    }
    assert_eq!(
        ranges
            .iter()
            .map(|range| range.partition_id)
            .collect::<Vec<_>>(),
        vec![
            transfer.children[0].partition_id,
            transfer.children[1].partition_id,
            initial.partitions[1].partition_id
        ]
    );
    for range in &ranges {
        fixture
            .provisioner
            .admit_existing_partition(ACCOUNT, &table.id, &range.partition_id)
            .await
            .unwrap()
            .drain()
            .await
            .unwrap();
    }
    for spec in tree.children.iter().chain(std::iter::once(&root)) {
        fixture
            .provisioner
            .admit_existing_directory(ACCOUNT, spec)
            .await
            .unwrap()
            .drain()
            .await
            .unwrap();
    }
    fixture
        .provisioner
        .admit_account(ACCOUNT)
        .await
        .unwrap()
        .drain()
        .await
        .unwrap();
    // The admitted token keeps its original participants even after their range
    // splits. Replaying it must not increment the copied children a second time.
    transaction.send().await.unwrap();
    let mut last = None;
    let mut found = Vec::new();
    loop {
        let page = sdk
            .scan()
            .table_name("Residency")
            .consistent_read(true)
            .limit(1)
            .set_exclusive_start_key(last)
            .send()
            .await
            .unwrap();
        found.extend(page.items.unwrap_or_default());
        last = page.last_evaluated_key;
        if last.as_ref().is_none_or(HashMap::is_empty) {
            break;
        }
    }
    assert_eq!(found.len(), fixture.data.len());
    for (_, item) in &fixture.data {
        let mut expected = item.clone();
        expected.insert("visits".into(), AwsAttributeValue::N("1".into()));
        assert!(found.contains(&expected));
        let read = sdk
            .get_item()
            .table_name("Residency")
            .key("id", item["id"].clone())
            .consistent_read(true)
            .send()
            .await
            .unwrap();
        assert_eq!(read.item, Some(expected));
    }
    beyonddb::CellStorage::new(fixture.client.clone(), "us-east-1")
        .refresh_table_size(ACCOUNT, "Residency")
        .await
        .unwrap();
    let description = sdk
        .describe_table()
        .table_name("Residency")
        .send()
        .await
        .unwrap();
    assert_eq!(
        description.table.unwrap().item_count,
        Some(fixture.data.len() as i64)
    );
    fixture.shutdown().await;
}
