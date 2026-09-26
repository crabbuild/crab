use std::{collections::HashMap, time::Duration};

use aws_sdk_dynamodb::{
    Client,
    types::{
        AttributeDefinition, AttributeValue, BillingMode, Delete, GlobalSecondaryIndex,
        KeySchemaElement, KeyType, Projection, ProjectionType, Put, ScalarAttributeType,
        TransactWriteItem, Update,
    },
};

const TABLE: &str = "ProcessGlobalIndex";
const ALL: &str = "ByBucketScore";
const KEYS: &str = "ByBucket";
const INCLUDE: &str = "BucketPayload";

fn key(id: &str) -> HashMap<String, AttributeValue> {
    HashMap::from([("id".into(), AttributeValue::S(id.into()))])
}

fn item(id: &str, bucket: &str, score: &str) -> HashMap<String, AttributeValue> {
    let mut item = key(id);
    item.extend([
        ("bucket".into(), AttributeValue::S(bucket.into())),
        ("score".into(), AttributeValue::N(score.into())),
        ("payload".into(), AttributeValue::S(format!("row-{id}"))),
        ("private".into(), AttributeValue::S("base-only".into())),
    ]);
    item
}

fn transaction() -> Vec<TransactWriteItem> {
    vec![
        TransactWriteItem::builder()
            .update(
                Update::builder()
                    .table_name(TABLE)
                    .set_key(Some(key("a")))
                    .update_expression("SET #bucket = :next, score = :score")
                    .expression_attribute_names("#bucket", "bucket")
                    .expression_attribute_values(":next", AttributeValue::S("new".into()))
                    .expression_attribute_values(":score", AttributeValue::N("-1".into()))
                    .build()
                    .unwrap(),
            )
            .build(),
        TransactWriteItem::builder()
            .delete(
                Delete::builder()
                    .table_name(TABLE)
                    .set_key(Some(key("b")))
                    .build()
                    .unwrap(),
            )
            .build(),
        TransactWriteItem::builder()
            .put(
                Put::builder()
                    .table_name(TABLE)
                    .set_item(Some(item("f", "new", "7")))
                    .build()
                    .unwrap(),
            )
            .build(),
    ]
}

async fn query(
    sdk: &Client,
    index: &str,
    bucket: &str,
    forward: bool,
) -> Vec<HashMap<String, AttributeValue>> {
    let mut cursor = None;
    let mut items = Vec::new();
    loop {
        let page = sdk
            .query()
            .table_name(TABLE)
            .index_name(index)
            .key_condition_expression("#bucket = :b")
            .expression_attribute_names("#bucket", "bucket")
            .expression_attribute_values(":b", AttributeValue::S(bucket.into()))
            .scan_index_forward(forward)
            .limit(1)
            .set_exclusive_start_key(cursor)
            .send()
            .await
            .unwrap();
        items.extend(page.items().iter().cloned());
        cursor = page.last_evaluated_key;
        if let Some(cursor) = &cursor {
            assert_eq!(cursor.len(), if index == KEYS { 2 } else { 3 });
        } else {
            return items;
        }
    }
}

async fn wait_count(sdk: &Client, index: &str, bucket: &str, expected: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let count = query(sdk, index, bucket, true).await.len();
        if count == expected {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{index}/{bucket}: expected {expected}, found {count}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub(crate) async fn create(sdk: &Client) {
    let schema = |name: &str, kind| {
        KeySchemaElement::builder()
            .attribute_name(name)
            .key_type(kind)
            .build()
            .unwrap()
    };
    let indexes = [
        (ALL, ProjectionType::All),
        (KEYS, ProjectionType::KeysOnly),
        (INCLUDE, ProjectionType::Include),
    ]
    .into_iter()
    .map(|(name, projection)| {
        let mut projected = Projection::builder().projection_type(projection);
        if name == INCLUDE {
            projected = projected.non_key_attributes("payload");
        }
        let mut index = GlobalSecondaryIndex::builder()
            .index_name(name)
            .key_schema(schema("bucket", KeyType::Hash))
            .projection(projected.build());
        if name != KEYS {
            index = index.key_schema(schema("score", KeyType::Range));
        }
        index.build().unwrap()
    })
    .collect();
    let created = sdk
        .create_table()
        .table_name(TABLE)
        .billing_mode(BillingMode::PayPerRequest)
        .key_schema(schema("id", KeyType::Hash))
        .set_attribute_definitions(Some(
            [
                ("id", ScalarAttributeType::S),
                ("bucket", ScalarAttributeType::S),
                ("score", ScalarAttributeType::N),
            ]
            .into_iter()
            .map(|(name, kind)| {
                AttributeDefinition::builder()
                    .attribute_name(name)
                    .attribute_type(kind)
                    .build()
                    .unwrap()
            })
            .collect(),
        ))
        .set_global_secondary_indexes(Some(indexes))
        .send()
        .await
        .unwrap();
    assert_eq!(
        created
            .table_description()
            .unwrap()
            .global_secondary_indexes()
            .len(),
        3
    );
    for row in [
        item("a", "old", "2"),
        item("b", "old", "2"),
        item("c", "old", "10"),
        key("sparse"),
    ] {
        sdk.put_item()
            .table_name(TABLE)
            .set_item(Some(row))
            .send()
            .await
            .unwrap();
    }
    for index in [ALL, KEYS, INCLUDE] {
        wait_count(sdk, index, "old", 3).await;
    }
    for (forward, expected) in [(true, ["a", "b", "c"]), (false, ["c", "b", "a"])] {
        let rows = query(sdk, ALL, "old", forward).await;
        assert_eq!(
            rows.iter()
                .map(|item| item["id"].as_s().unwrap().as_str())
                .collect::<Vec<_>>(),
            expected
        );
    }
    assert!(
        query(sdk, KEYS, "old", true)
            .await
            .iter()
            .all(|row| row.len() == 2)
    );
    assert!(
        query(sdk, INCLUDE, "old", true)
            .await
            .iter()
            .all(|row| row.len() == 4
                && row.contains_key("payload")
                && !row.contains_key("private"))
    );
    let consistent = sdk
        .query()
        .table_name(TABLE)
        .index_name(ALL)
        .consistent_read(true)
        .key_condition_expression("#bucket = :b")
        .expression_attribute_names("#bucket", "bucket")
        .expression_attribute_values(":b", AttributeValue::S("old".into()))
        .send()
        .await
        .unwrap_err();
    assert!(format!("{consistent:?}").contains("ValidationException"));
    let wrong_type = sdk
        .query()
        .table_name(TABLE)
        .index_name(ALL)
        .key_condition_expression("#bucket = :b AND score = :score")
        .expression_attribute_names("#bucket", "bucket")
        .expression_attribute_values(":b", AttributeValue::S("old".into()))
        .expression_attribute_values(":score", AttributeValue::S("2".into()))
        .send()
        .await
        .unwrap_err();
    assert!(format!("{wrong_type:?}").contains("ValidationException"));
    let failed = sdk
        .transact_write_items()
        .transact_items(
            TransactWriteItem::builder()
                .update(
                    Update::builder()
                        .table_name(TABLE)
                        .set_key(Some(key("a")))
                        .condition_expression("attribute_not_exists(id)")
                        .update_expression("SET #bucket = :bad")
                        .expression_attribute_names("#bucket", "bucket")
                        .expression_attribute_values(":bad", AttributeValue::S("aborted".into()))
                        .build()
                        .unwrap(),
                )
                .build(),
        )
        .send()
        .await
        .unwrap_err();
    assert!(format!("{failed:?}").contains("TransactionCanceledException"));
    sdk.transact_write_items()
        .client_request_token("global-index-process")
        .set_transact_items(Some(transaction()))
        .send()
        .await
        .unwrap();
    sdk.update_item()
        .table_name(TABLE)
        .set_key(Some(key("c")))
        .update_expression("REMOVE #bucket")
        .expression_attribute_names("#bucket", "bucket")
        .send()
        .await
        .unwrap();
    assert_recovered(sdk).await;
}

pub(crate) async fn assert_recovered(sdk: &Client) {
    sdk.transact_write_items()
        .client_request_token("global-index-process")
        .set_transact_items(Some(transaction()))
        .send()
        .await
        .unwrap();
    for index in [ALL, KEYS, INCLUDE] {
        wait_count(sdk, index, "old", 0).await;
        wait_count(sdk, index, "new", 2).await;
        assert!(query(sdk, index, "aborted", true).await.is_empty());
        let mut actual = Vec::new();
        for segment in 0..3 {
            let mut cursor = None;
            loop {
                let page = sdk
                    .scan()
                    .table_name(TABLE)
                    .index_name(index)
                    .segment(segment)
                    .total_segments(3)
                    .limit(1)
                    .set_exclusive_start_key(cursor)
                    .send()
                    .await
                    .unwrap();
                actual.extend(
                    page.items()
                        .iter()
                        .map(|row| row["id"].as_s().unwrap().clone()),
                );
                cursor = page.last_evaluated_key;
                if cursor.is_none() {
                    break;
                }
            }
        }
        actual.sort();
        assert_eq!(actual, ["a", "f"]);
    }
    let rows = query(sdk, ALL, "new", true).await;
    assert_eq!(rows[0]["score"], AttributeValue::N("-1".into()));
    assert_eq!(rows[1]["score"], AttributeValue::N("7".into()));
}
