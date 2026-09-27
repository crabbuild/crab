use aws_sdk_dynamodb::{Client, config::retry::RetryConfig, types::AttributeValue};

pub(crate) async fn increment_without_client_retries(client: &Client) {
    let client = Client::from_conf(
        client
            .config()
            .to_builder()
            .retry_config(RetryConfig::disabled())
            .build(),
    );
    client
        .put_item()
        .table_name("NetworkData")
        .item("id", AttributeValue::S("concurrent-counter".into()))
        .item("value", AttributeValue::N("0".into()))
        .send()
        .await
        .unwrap();
    let mut writers = tokio::task::JoinSet::new();
    for _ in 0..50 {
        let client = client.clone();
        writers.spawn(async move {
            client
                .update_item()
                .table_name("NetworkData")
                .key("id", AttributeValue::S("concurrent-counter".into()))
                .update_expression("ADD #value :one")
                .expression_attribute_names("#value", "value")
                .expression_attribute_values(":one", AttributeValue::N("1".into()))
                .send()
                .await
        });
    }
    while let Some(result) = writers.join_next().await {
        result.unwrap().unwrap();
    }
    assert_counter(&client).await;
}

pub(crate) async fn assert_counter(client: &Client) {
    let stored = client
        .get_item()
        .table_name("NetworkData")
        .key("id", AttributeValue::S("concurrent-counter".into()))
        .consistent_read(true)
        .send()
        .await
        .unwrap();
    assert_eq!(
        stored.item().unwrap().get("value"),
        Some(&AttributeValue::N("50".into()))
    );
}
