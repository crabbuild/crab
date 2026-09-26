use std::collections::HashMap;

use aws_sdk_dynamodb::{
    Client,
    types::{
        AttributeDefinition, AttributeValue, BillingMode, Delete, KeySchemaElement, KeyType,
        LocalSecondaryIndex, Projection, ProjectionType, Put, ScalarAttributeType, Select,
        TransactWriteItem, Update,
    },
};

const TABLE: &str = "ProcessLocalIndex";
const INDEX: &str = "ByScore";

fn key(pk: &str, sk: &str) -> HashMap<String, AttributeValue> {
    HashMap::from([
        ("pk".into(), AttributeValue::S(pk.into())),
        ("sk".into(), AttributeValue::N(sk.into())),
    ])
}

fn transaction() -> Vec<TransactWriteItem> {
    let mut created = key("other", "1");
    created.insert("score".into(), AttributeValue::N("7".into()));
    vec![
        TransactWriteItem::builder()
            .update(
                Update::builder()
                    .table_name(TABLE)
                    .set_key(Some(key("same", "2")))
                    .update_expression("SET score = :next")
                    .expression_attribute_values(":next", AttributeValue::N("-5".into()))
                    .build()
                    .unwrap(),
            )
            .build(),
        TransactWriteItem::builder()
            .delete(
                Delete::builder()
                    .table_name(TABLE)
                    .set_key(Some(key("same", "-2")))
                    .build()
                    .unwrap(),
            )
            .build(),
        TransactWriteItem::builder()
            .put(
                Put::builder()
                    .table_name(TABLE)
                    .set_item(Some(created))
                    .build()
                    .unwrap(),
            )
            .build(),
    ]
}

pub(crate) async fn create(sdk: &Client) {
    let schema = |name: &str, kind| {
        KeySchemaElement::builder()
            .attribute_name(name)
            .key_type(kind)
            .build()
            .unwrap()
    };
    let created = sdk
        .create_table()
        .table_name(TABLE)
        .billing_mode(BillingMode::PayPerRequest)
        .key_schema(schema("pk", KeyType::Hash))
        .key_schema(schema("sk", KeyType::Range))
        .set_attribute_definitions(Some(
            [
                ("pk", ScalarAttributeType::S),
                ("sk", ScalarAttributeType::N),
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
        .local_secondary_indexes(
            LocalSecondaryIndex::builder()
                .index_name(INDEX)
                .key_schema(schema("pk", KeyType::Hash))
                .key_schema(schema("score", KeyType::Range))
                .projection(
                    Projection::builder()
                        .projection_type(ProjectionType::All)
                        .build(),
                )
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        created
            .table_description()
            .unwrap()
            .local_secondary_indexes()[0]
            .index_name(),
        Some(INDEX)
    );
    for (sk, score) in [
        ("-2", Some("10")),
        ("2", Some("2")),
        ("10", Some("2")),
        ("20", None),
    ] {
        let mut item = key("same", sk);
        item.insert("payload".into(), AttributeValue::S(format!("row-{sk}")));
        if let Some(score) = score {
            item.insert("score".into(), AttributeValue::N(score.into()));
        }
        sdk.put_item()
            .table_name(TABLE)
            .set_item(Some(item))
            .send()
            .await
            .unwrap();
    }
    for (forward, expected) in [(true, ["2", "10", "-2"]), (false, ["-2", "10", "2"])] {
        let mut cursor = None;
        let mut actual = Vec::new();
        loop {
            let page = sdk
                .query()
                .table_name(TABLE)
                .index_name(INDEX)
                .consistent_read(true)
                .key_condition_expression("pk = :p")
                .expression_attribute_values(":p", AttributeValue::S("same".into()))
                .scan_index_forward(forward)
                .limit(1)
                .set_exclusive_start_key(cursor)
                .send()
                .await
                .unwrap();
            actual.extend(
                page.items()
                    .iter()
                    .map(|item| item["sk"].as_n().unwrap().clone()),
            );
            cursor = page.last_evaluated_key;
            if let Some(key) = &cursor {
                assert_eq!(key.len(), 3);
            }
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(actual, expected);
    }
    let filtered = sdk
        .query()
        .table_name(TABLE)
        .index_name(INDEX)
        .key_condition_expression("pk = :p AND score BETWEEN :low AND :high")
        .expression_attribute_values(":p", AttributeValue::S("same".into()))
        .expression_attribute_values(":low", AttributeValue::N("2.0".into()))
        .expression_attribute_values(":high", AttributeValue::N("2".into()))
        .projection_expression("payload")
        .send()
        .await
        .unwrap();
    assert_eq!(filtered.items().len(), 2);
    assert!(
        filtered
            .items()
            .iter()
            .all(|item| item.len() == 1 && item.contains_key("payload"))
    );
    let bad = sdk
        .update_item()
        .table_name(TABLE)
        .set_key(Some(key("same", "2")))
        .update_expression("SET score = :bad")
        .expression_attribute_values(":bad", AttributeValue::S("invalid".into()))
        .send()
        .await
        .unwrap_err();
    assert!(format!("{bad:?}").contains("ValidationException"));
    sdk.transact_write_items()
        .client_request_token("local-index-process")
        .set_transact_items(Some(transaction()))
        .send()
        .await
        .unwrap();
    sdk.update_item()
        .table_name(TABLE)
        .set_key(Some(key("same", "10")))
        .update_expression("REMOVE score")
        .send()
        .await
        .unwrap();
    assert_recovered(sdk).await;
}

pub(crate) async fn assert_recovered(sdk: &Client) {
    sdk.transact_write_items()
        .client_request_token("local-index-process")
        .set_transact_items(Some(transaction()))
        .send()
        .await
        .unwrap();
    let described = sdk.describe_table().table_name(TABLE).send().await.unwrap();
    assert_eq!(
        described.table().unwrap().local_secondary_indexes()[0]
            .projection()
            .unwrap()
            .projection_type(),
        Some(&ProjectionType::All)
    );
    let page = sdk
        .query()
        .table_name(TABLE)
        .index_name(INDEX)
        .consistent_read(true)
        .key_condition_expression("pk = :p")
        .expression_attribute_values(":p", AttributeValue::S("same".into()))
        .select(Select::AllProjectedAttributes)
        .send()
        .await
        .unwrap();
    assert_eq!(page.items().len(), 1);
    assert_eq!(page.items()[0]["sk"], AttributeValue::N("2".into()));
    assert_eq!(page.items()[0]["score"], AttributeValue::N("-5".into()));
    assert_eq!(
        page.items()[0]["payload"],
        AttributeValue::S("row-2".into())
    );
    let mut found = Vec::new();
    for segment in 0..3 {
        let mut cursor = None;
        loop {
            let page = sdk
                .scan()
                .table_name(TABLE)
                .index_name(INDEX)
                .limit(1)
                .segment(segment)
                .total_segments(3)
                .set_exclusive_start_key(cursor)
                .send()
                .await
                .unwrap();
            found.extend(page.items().iter().map(|item| {
                (
                    item["pk"].as_s().unwrap().clone(),
                    item["sk"].as_n().unwrap().clone(),
                )
            }));
            cursor = page.last_evaluated_key;
            if cursor.is_none() {
                break;
            }
        }
    }
    found.sort();
    assert_eq!(
        found,
        [("other".into(), "1".into()), ("same".into(), "2".into())]
    );
    assert!(
        sdk.get_item()
            .table_name(TABLE)
            .set_key(Some(key("same", "-2")))
            .send()
            .await
            .unwrap()
            .item()
            .is_none()
    );
    assert!(
        !sdk.get_item()
            .table_name(TABLE)
            .set_key(Some(key("same", "10")))
            .send()
            .await
            .unwrap()
            .item()
            .unwrap()
            .contains_key("score")
    );
}
