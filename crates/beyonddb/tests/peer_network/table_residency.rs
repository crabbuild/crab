use aws_sdk_dynamodb::{
    Client,
    types::{
        AttributeDefinition, AttributeValue, BillingMode, GlobalSecondaryIndex, KeySchemaElement,
        KeyType, Projection, ProjectionType, ScalarAttributeType,
    },
};

pub(crate) async fn recreate_with_remote_account(sdk: &Client) {
    let key = KeySchemaElement::builder()
        .attribute_name("id")
        .key_type(KeyType::Hash)
        .build()
        .unwrap();
    let definition = AttributeDefinition::builder()
        .attribute_name("id")
        .attribute_type(ScalarAttributeType::S)
        .build()
        .unwrap();
    let index = GlobalSecondaryIndex::builder()
        .index_name("ById")
        .key_schema(key.clone())
        .projection(
            Projection::builder()
                .projection_type(ProjectionType::All)
                .build(),
        )
        .build()
        .unwrap();
    // This node has eight slots; the account lives on its peer. Deleted data
    // and index generations must stop occupying slots without a process restart.
    for generation in 0..6 {
        sdk.create_table()
            .table_name("RecreatedTable")
            .billing_mode(BillingMode::PayPerRequest)
            .key_schema(key.clone())
            .attribute_definitions(definition.clone())
            .global_secondary_indexes(index.clone())
            .send()
            .await
            .unwrap_or_else(|error| panic!("generation {generation} create failed: {error:?}"));
        let empty = sdk
            .scan()
            .table_name("RecreatedTable")
            .consistent_read(true)
            .send()
            .await
            .unwrap();
        assert!(empty.items().is_empty());
        sdk.put_item()
            .table_name("RecreatedTable")
            .item("id", AttributeValue::S(generation.to_string()))
            .send()
            .await
            .unwrap();
        sdk.delete_table()
            .table_name("RecreatedTable")
            .send()
            .await
            .unwrap_or_else(|error| panic!("generation {generation} delete failed: {error:?}"));
        // DeleteTable returns DELETING until directory retirement completes.
        // Require public absence before reusing the name for a new generation.
        tokio::time::timeout(std::time::Duration::from_secs(45), async {
            loop {
                match sdk
                    .describe_table()
                    .table_name("RecreatedTable")
                    .send()
                    .await
                {
                    Ok(table) => assert_eq!(
                        table.table.unwrap().table_status(),
                        Some(&aws_sdk_dynamodb::types::TableStatus::Deleting)
                    ),
                    Err(error)
                        if error
                            .as_service_error()
                            .is_some_and(|error| error.is_resource_not_found_exception()) =>
                    {
                        break;
                    }
                    Err(error) => panic!("generation {generation} deletion failed: {error:?}"),
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|error| panic!("generation {generation} deletion stalled: {error:?}"));
    }
    let live = sdk
        .get_item()
        .table_name("NetworkData")
        .key("id", AttributeValue::S("remote".into()))
        .consistent_read(true)
        .send()
        .await
        .unwrap();
    assert_eq!(
        live.item().unwrap().get("value"),
        Some(&AttributeValue::S("committed".into()))
    );
}
