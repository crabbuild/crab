use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_split_epochs_follow_source_lineage_across_independent_branches() {
    const ACCOUNT: &str = "123456789012";
    let fixture = Fixture::new().await;
    let account = account_target(ACCOUNT).unwrap();
    let table = fixture
        .client
        .query::<DescribeTable>(&account, None, Json("Residency".into()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let original = fixture
        .client
        .query::<ReadTableRoute>(&account, None, Json(table.id.clone()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let first = fixture
        .provisioner
        .split_partition(
            ACCOUNT,
            fixture.client.clone(),
            &table.id,
            original.partitions[0].partition_id,
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
        )
        .await
        .unwrap();
    assert_eq!(replay, sibling);
    let current = fixture
        .client
        .query::<ReadTableRoute>(&account, None, Json(table.id.clone()))
        .await
        .unwrap()
        .output
        .0
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
