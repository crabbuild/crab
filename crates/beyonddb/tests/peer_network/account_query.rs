use aws_sdk_dynamodb::types::AttributeValue;

const TABLE: &str = "AccountOrdered";

pub(crate) async fn write_and_assert(sdk: &aws_sdk_dynamodb::Client) {
    for value in ["10", "-2", "2"] {
        sdk.put_item()
            .table_name(TABLE)
            .item("pk", AttributeValue::S("same".into()))
            .item("sk", AttributeValue::N(value.into()))
            .send()
            .await
            .unwrap();
    }
    assert_restored(sdk).await;
}

pub(crate) async fn assert_restored(sdk: &aws_sdk_dynamodb::Client) {
    for (forward, expected) in [(true, ["-2", "2", "10"]), (false, ["10", "2", "-2"])] {
        let mut cursor = None;
        let mut actual = Vec::new();
        loop {
            let page = sdk
                .query()
                .table_name(TABLE)
                .consistent_read(true)
                .key_condition_expression("pk = :pk")
                .expression_attribute_values(":pk", AttributeValue::S("same".into()))
                .scan_index_forward(forward)
                .limit(2)
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
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(actual, expected);
    }
}
