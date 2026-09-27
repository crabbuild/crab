use super::*;
use aws_sdk_dynamodb::types::{Put, TransactWriteItem};
use crab_cell_runtime::identity::CellTarget;
use extenddb_storage::MetadataEngine;

const ACCOUNT: &str = "123456789012";

async fn usage(fixture: &Fixture, target: &CellTarget) -> (u64, u64) {
    let usage = fixture
        .client
        .query::<beyonddb::PartitionUsage>(target, None, Json(()))
        .await
        .unwrap()
        .output
        .0;
    (usage.item_count, usage.item_bytes)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_range_usage_survives_mutation_replay_split_and_owner_restore() {
    let fixture = Fixture::with_partition_count(1).await;
    let sdk = aws_sdk_dynamodb::Client::from_conf(
        fixture
            .sdk
            .config()
            .to_builder()
            .retry_config(aws_sdk_dynamodb::config::retry::RetryConfig::disabled())
            .build(),
    );
    let account = account_target(ACCOUNT).unwrap();
    let table = fixture
        .client
        .query::<DescribeTable>(&account, None, Json("Residency".into()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let source = beyonddb::data_target(ACCOUNT, &table.id, &[0; 16]).unwrap();
    let before = usage(&fixture, &source).await;
    assert_eq!(before.0, 1);
    let mut item = fixture.data[0].1.clone();
    let id = item["id"].clone();
    let original_len = item["value"].as_s().unwrap().len() as u64;
    // This image crosses the bounded SQL BLOB chunk size.
    sdk.update_item()
        .table_name("Residency")
        .key("id", id.clone())
        .update_expression("SET #v = :value")
        .expression_attribute_names("#v", "value")
        .expression_attribute_values(":value", AwsAttributeValue::S("x".repeat(300_000)))
        .send()
        .await
        .unwrap();
    let large = (1, before.1 - original_len + 300_000);
    assert_eq!(usage(&fixture, &source).await, large);
    assert!(
        sdk.delete_item()
            .table_name("Residency")
            .key("id", id.clone())
            .condition_expression("attribute_not_exists(id)")
            .send()
            .await
            .is_err()
    );
    assert_eq!(usage(&fixture, &source).await, large);
    item.insert("value".into(), AwsAttributeValue::S("transaction".into()));
    let write = TransactWriteItem::builder()
        .put(
            Put::builder()
                .table_name("Residency")
                .set_item(Some(item.clone()))
                .build()
                .unwrap(),
        )
        .build();
    for _ in 0..2 {
        sdk.transact_write_items()
            .client_request_token("usage-replay")
            .transact_items(write.clone())
            .send()
            .await
            .unwrap();
    }
    let committed = (1, before.1 - original_len + "transaction".len() as u64);
    assert_eq!(usage(&fixture, &source).await, committed);
    let plan = fixture
        .provisioner
        .split_partition(ACCOUNT, fixture.client.clone(), &table.id, [0; 16])
        .await
        .unwrap();
    fixture
        .provisioner
        .resume_split(ACCOUNT, fixture.client.clone(), &plan)
        .await
        .unwrap();
    let mut children = Vec::new();
    let mut after = (0, 0);
    for child in &plan.children {
        let target = beyonddb::data_target(ACCOUNT, &table.id, &child.partition_id).unwrap();
        let value = usage(&fixture, &target).await;
        after.0 += value.0;
        after.1 += value.1;
        fixture
            .provisioner
            .admit_existing_partition(ACCOUNT, &table.id, &child.partition_id)
            .await
            .unwrap()
            .drain()
            .await
            .unwrap();
        children.push(target);
    }
    assert_eq!(after, committed);
    assert_eq!(usage(&fixture, &source).await, committed);
    // SDK reads restore the serving child; query the persisted counters in both.
    assert_eq!(
        sdk.get_item()
            .table_name("Residency")
            .key("id", id.clone())
            .consistent_read(true)
            .send()
            .await
            .unwrap()
            .item,
        Some(item)
    );
    let mut restored = (0, 0);
    for target in &children {
        let value = usage(&fixture, target).await;
        restored.0 += value.0;
        restored.1 += value.1;
    }
    assert_eq!(restored, committed);
    let storage = beyonddb::CellStorage::new(fixture.client.clone(), "us-east-1");
    storage
        .refresh_table_size(ACCOUNT, "Residency")
        .await
        .unwrap();
    let described = sdk
        .describe_table()
        .table_name("Residency")
        .send()
        .await
        .unwrap()
        .table
        .unwrap();
    assert_eq!(described.item_count, Some(1));
    let expected = 2 + id.as_s().unwrap().len() + 5 + "transaction".len();
    assert_eq!(described.table_size_bytes, Some(expected as i64));
    fixture
        .provisioner
        .admit_account(ACCOUNT)
        .await
        .unwrap()
        .drain()
        .await
        .unwrap();
    let restored = sdk
        .describe_table()
        .table_name("Residency")
        .send()
        .await
        .unwrap()
        .table
        .unwrap();
    assert_eq!(restored.item_count, described.item_count);
    assert_eq!(restored.table_size_bytes, described.table_size_bytes);
    for _ in 0..2 {
        sdk.delete_item()
            .table_name("Residency")
            .key("id", id.clone())
            .send()
            .await
            .unwrap();
    }
    for target in &children {
        assert_eq!(usage(&fixture, target).await, (0, 0));
    }
    assert!(
        sdk.get_item()
            .table_name("Residency")
            .key("id", id)
            .consistent_read(true)
            .send()
            .await
            .unwrap()
            .item
            .is_none()
    );
    storage
        .refresh_table_size(ACCOUNT, "Residency")
        .await
        .unwrap();
    let empty = sdk
        .describe_table()
        .table_name("Residency")
        .send()
        .await
        .unwrap()
        .table
        .unwrap();
    assert_eq!(
        (empty.item_count, empty.table_size_bytes),
        (Some(0), Some(0))
    );
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_compacted_write_replays_after_owner_restore_without_reapplying_images() {
    use aws_sdk_dynamodb::types::{Get, TransactGetItem};
    let fixture = Fixture::new().await;
    let sdk = aws_sdk_dynamodb::Client::from_conf(
        fixture
            .sdk
            .config()
            .to_builder()
            .retry_config(aws_sdk_dynamodb::config::retry::RetryConfig::disabled())
            .build(),
    );
    let writes = fixture
        .data
        .iter()
        .map(|(_, original)| {
            let mut item = original.clone();
            item.insert("value".into(), AwsAttributeValue::S("transaction".into()));
            TransactWriteItem::builder()
                .put(
                    Put::builder()
                        .table_name("Residency")
                        .set_item(Some(item))
                        .build()
                        .unwrap(),
                )
                .build()
        })
        .collect::<Vec<_>>();
    let request = sdk
        .transact_write_items()
        .client_request_token("compacted-write")
        .set_transact_items(Some(writes));
    request.clone().send().await.unwrap();
    sdk.update_item()
        .table_name("Residency")
        .key("id", fixture.data[0].1["id"].clone())
        .update_expression("SET #v = :v")
        .expression_attribute_names("#v", "value")
        .expression_attribute_values(":v", AwsAttributeValue::S("later-write".into()))
        .send()
        .await
        .unwrap();
    fixture
        .provisioner
        .admit_coordinator(ACCOUNT, b"compacted-write")
        .await
        .unwrap()
        .drain()
        .await
        .unwrap();
    for (handle, _) in &fixture.data {
        handle.drain().await.unwrap();
    }
    // Restored token/decision records must return the original success without
    // overwriting the later mutation, even though operation images are gone.
    request.send().await.unwrap();
    let reads = fixture
        .data
        .iter()
        .map(|(_, item)| {
            TransactGetItem::builder()
                .get(
                    Get::builder()
                        .table_name("Residency")
                        .key("id", item["id"].clone())
                        .build()
                        .unwrap(),
                )
                .build()
        })
        .collect();
    let result = sdk
        .transact_get_items()
        .set_transact_items(Some(reads))
        .send()
        .await
        .unwrap();
    let values = result
        .responses()
        .iter()
        .map(|result| result.item().unwrap()["value"].as_s().unwrap().as_str())
        .collect::<Vec<_>>();
    assert_eq!(values, ["later-write", "transaction"]);
    fixture.shutdown().await;
}
