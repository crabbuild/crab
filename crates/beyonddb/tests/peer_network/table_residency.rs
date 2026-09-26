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
            .unwrap();
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
            .unwrap();
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
