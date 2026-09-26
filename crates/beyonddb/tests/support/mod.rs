use std::collections::HashMap;

use aws_sdk_dynamodb::{
    Client,
    types::{AttributeValue, Get, Put, TransactGetItem, TransactWriteItem, Update},
};

/// Exercise SQL payloads larger than 1 MiB through the signed SDK boundary.
pub struct LargeTransaction {
    cases: Vec<PayloadTransaction>,
}

struct PayloadTransaction {
    put_token: &'static str,
    update_token: &'static str,
    puts: Vec<TransactWriteItem>,
    updates: Vec<TransactWriteItem>,
    reads: Vec<TransactGetItem>,
    expected: Vec<HashMap<String, AttributeValue>>,
}

impl LargeTransaction {
    pub async fn write(sdk: &Client, keys: Vec<(String, String)>) -> Self {
        assert_eq!(keys.len(), 10);
        let escaped_keys = vec![keys[0].clone(), keys[9].clone()]
            .into_iter()
            .map(|(table, id)| (table, format!("escaped-{id}")))
            .collect();
        let cases = vec![
            PayloadTransaction::new(
                keys,
                "x".repeat(380 * 1024),
                "large-payload-put",
                "large-payload-update",
            ),
            PayloadTransaction::new(
                escaped_keys,
                format!("{}{}", "\0".repeat(160 * 1024), "🙂".repeat(20 * 1024)),
                "escaped-payload-put",
                "escaped-payload-update",
            ),
        ];
        let test = Self { cases };
        test.assert_recovered(sdk).await;
        test
    }

    pub async fn assert_recovered(&self, sdk: &Client) {
        for case in &self.cases {
            case.assert_recovered(sdk).await;
        }
    }
}

impl PayloadTransaction {
    fn new(
        keys: Vec<(String, String)>,
        payload: String,
        put_token: &'static str,
        update_token: &'static str,
    ) -> Self {
        let mut puts = Vec::new();
        let mut updates = Vec::new();
        let mut reads = Vec::new();
        let mut expected = Vec::new();
        for (table, id) in keys {
            let key = HashMap::from([("id".into(), AttributeValue::S(id))]);
            let mut item = key.clone();
            item.insert("payload".into(), AttributeValue::S(payload.clone()));
            puts.push(
                TransactWriteItem::builder()
                    .put(
                        Put::builder()
                            .table_name(&table)
                            .set_item(Some(item.clone()))
                            .condition_expression("attribute_not_exists(id)")
                            .build()
                            .unwrap(),
                    )
                    .build(),
            );
            updates.push(
                TransactWriteItem::builder()
                    .update(
                        Update::builder()
                            .table_name(&table)
                            .set_key(Some(key.clone()))
                            .update_expression("SET #v = :v")
                            .expression_attribute_names("#v", "version")
                            .expression_attribute_values(":v", AttributeValue::N("1".into()))
                            .build()
                            .unwrap(),
                    )
                    .build(),
            );
            reads.push(
                TransactGetItem::builder()
                    .get(
                        Get::builder()
                            .table_name(table)
                            .set_key(Some(key))
                            .build()
                            .unwrap(),
                    )
                    .build(),
            );
            item.insert("version".into(), AttributeValue::N("1".into()));
            expected.push(item);
        }
        Self {
            put_token,
            update_token,
            puts,
            updates,
            reads,
            expected,
        }
    }

    pub async fn assert_recovered(&self, sdk: &Client) {
        // Replaying the original Put must not revert the later Update, even
        // after replacing an owner or hard-restarting the serving process.
        sdk.transact_write_items()
            .client_request_token(self.put_token)
            .set_transact_items(Some(self.puts.clone()))
            .send()
            .await
            .unwrap();
        sdk.transact_write_items()
            .client_request_token(self.update_token)
            .set_transact_items(Some(self.updates.clone()))
            .send()
            .await
            .unwrap();
        if self.expected.len() == 2 {
            let get = self.reads[0].get().unwrap();
            let failed = TransactWriteItem::builder()
                .condition_check(
                    aws_sdk_dynamodb::types::ConditionCheck::builder()
                        .table_name(get.table_name())
                        .set_key(Some(get.key().clone()))
                        .condition_expression("attribute_not_exists(id)")
                        .return_values_on_condition_check_failure(
                            aws_sdk_dynamodb::types::ReturnValuesOnConditionCheckFailure::AllOld,
                        )
                        .build()
                        .unwrap(),
                )
                .build();
            let other = self.reads[1].get().unwrap();
            let changed = TransactWriteItem::builder()
                .update(
                    Update::builder()
                        .table_name(other.table_name())
                        .set_key(Some(other.key().clone()))
                        .update_expression("SET #v = :v")
                        .expression_attribute_names("#v", "version")
                        .expression_attribute_values(":v", AttributeValue::N("2".into()))
                        .build()
                        .unwrap(),
                )
                .build();
            let error = sdk
                .transact_write_items()
                .client_request_token("escaped-payload-abort")
                .transact_items(failed)
                .transact_items(changed)
                .send()
                .await
                .unwrap_err()
                .into_service_error();
            let aws_sdk_dynamodb::operation::transact_write_items::TransactWriteItemsError::TransactionCanceledException(error) = error else {
                panic!("expected a canceled transaction, got {error:?}");
            };
            assert_eq!(
                error.cancellation_reasons()[0].item(),
                Some(&self.expected[0])
            );
        }
        let result = sdk
            .transact_get_items()
            .set_transact_items(Some(self.reads.clone()))
            .send()
            .await
            .unwrap();
        assert_eq!(result.responses().len(), self.expected.len());
        for (response, expected) in result.responses().iter().zip(&self.expected) {
            assert_eq!(response.item(), Some(expected));
        }
        if self.expected.len() == 2 {
            for (read, expected) in self.reads.iter().zip(&self.expected) {
                let get = read.get().unwrap();
                let query = sdk
                    .query()
                    .table_name(get.table_name())
                    .key_condition_expression("id = :id")
                    .expression_attribute_values(":id", expected["id"].clone())
                    .send()
                    .await
                    .unwrap();
                assert_eq!(query.items(), std::slice::from_ref(expected));
                let mut after = None;
                loop {
                    let scan = sdk
                        .scan()
                        .table_name(get.table_name())
                        .filter_expression("id = :id")
                        .expression_attribute_values(":id", expected["id"].clone())
                        .set_exclusive_start_key(after)
                        .send()
                        .await
                        .unwrap();
                    if !scan.items().is_empty() {
                        assert_eq!(scan.items(), std::slice::from_ref(expected));
                        break;
                    }
                    after = scan.last_evaluated_key().cloned();
                    assert!(after.is_some(), "Scan skipped the escaped item");
                }
            }
        }
    }
}
