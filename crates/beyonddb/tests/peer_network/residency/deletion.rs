use super::provisioning::{create, sdk_without_retries};
use super::*;
use extenddb_core::types::{
    AttributeDefinition, BillingMode, CreateTableInput, KeySchemaElement, KeyType, LsiInput,
    Projection, ProjectionType, ScalarAttributeType,
};
use extenddb_storage::TableEngine;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_large_deletion_resumes_after_account_restore() {
    let fixture = Fixture::new().await;
    let sdk = sdk_without_retries(&fixture);
    // Seed an account-local generation so public item writes create enough
    // catalog-owned rows to require multiple cleanup commands.
    let original = beyonddb::CellStorage::new(fixture.client.clone(), "us-east-1")
        .create_table(
            "123456789012",
            CreateTableInput {
                table_name: "ResidencyDelete".into(),
                key_schema: vec![
                    KeySchemaElement {
                        attribute_name: "id".into(),
                        key_type: KeyType::Hash,
                    },
                    KeySchemaElement {
                        attribute_name: "sort".into(),
                        key_type: KeyType::Range,
                    },
                ],
                attribute_definitions: ["id", "sort", "alternate"]
                    .into_iter()
                    .map(|name| AttributeDefinition {
                        attribute_name: name.into(),
                        attribute_type: ScalarAttributeType::S,
                    })
                    .collect(),
                local_secondary_indexes: Some(vec![LsiInput {
                    index_name: "ByAlternate".into(),
                    key_schema: vec![
                        KeySchemaElement {
                            attribute_name: "id".into(),
                            key_type: KeyType::Hash,
                        },
                        KeySchemaElement {
                            attribute_name: "alternate".into(),
                            key_type: KeyType::Range,
                        },
                    ],
                    projection: Projection {
                        projection_type: ProjectionType::All,
                        non_key_attributes: None,
                    },
                }]),
                billing_mode: Some(BillingMode::PayPerRequest),
                ..CreateTableInput::default()
            },
        )
        .await
        .unwrap();
    for ordinal in 0..160 {
        sdk.put_item()
            .table_name("ResidencyDelete")
            .item("id", AwsAttributeValue::S(format!("item-{ordinal}")))
            .item("sort", AwsAttributeValue::S("row".into()))
            .item("alternate", AwsAttributeValue::S("index-row".into()))
            .send()
            .await
            .unwrap();
    }
    sdk.delete_table()
        .table_name("ResidencyDelete")
        .send()
        .await
        .unwrap();
    let deleting = sdk
        .describe_table()
        .table_name("ResidencyDelete")
        .send()
        .await
        .unwrap()
        .table
        .unwrap();
    assert_eq!(
        deleting.table_status(),
        Some(&aws_sdk_dynamodb::types::TableStatus::Deleting)
    );
    assert!(
        create(&sdk, "ResidencyDelete", false)
            .send()
            .await
            .unwrap_err()
            .as_service_error()
            .unwrap()
            .is_resource_in_use_exception()
    );
    assert!(
        sdk.update_table()
            .table_name("ResidencyDelete")
            .deletion_protection_enabled(true)
            .send()
            .await
            .unwrap_err()
            .as_service_error()
            .unwrap()
            .is_resource_in_use_exception()
    );
    assert!(
        sdk.put_item()
            .table_name("ResidencyDelete")
            .item("id", AwsAttributeValue::S("late".into()))
            .send()
            .await
            .unwrap_err()
            .as_service_error()
            .unwrap()
            .is_resource_not_found_exception()
    );
    fixture
        .provisioner
        .admit_account("123456789012")
        .await
        .unwrap()
        .drain()
        .await
        .unwrap();
    fixture
        .provisioner
        .install_account_capacity_loop(
            &fixture.tasks,
            "123456789012".into(),
            fixture.client.clone(),
            u64::MAX,
            Duration::from_millis(100),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match sdk
                .describe_table()
                .table_name("ResidencyDelete")
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
                Err(error) => panic!("deletion recovery failed: {error}"),
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    create(&sdk, "ResidencyDelete", false).send().await.unwrap();
    sdk.put_item()
        .table_name("ResidencyDelete")
        .item("id", AwsAttributeValue::S("replacement".into()))
        .send()
        .await
        .unwrap();
    let items = sdk
        .scan()
        .table_name("ResidencyDelete")
        .send()
        .await
        .unwrap()
        .items
        .unwrap();
    assert_eq!(
        items,
        vec![SdkItem::from([(
            "id".into(),
            AwsAttributeValue::S("replacement".into())
        )])]
    );
    let account = account_target("123456789012").unwrap();
    let mutation = || {
        let now = now_ms();
        crab_cell_runtime::MutationIdentity {
            request_id: crab_cell_runtime::identity::RequestId::from_bytes(
                *uuid::Uuid::now_v7().as_bytes(),
            ),
            issued_at_ms: now,
            expires_at_ms: now + 60_000,
        }
    };
    assert!(
        fixture
            .client
            .command::<beyonddb::ContinueTableDeletion>(
                &account,
                mutation(),
                Json(original.table_id.clone())
            )
            .await
            .unwrap()
            .output
            .0
    );
    assert!(
        matches!(fixture.client.command::<beyonddb::DeleteTable>(&account, mutation(), Json(beyonddb::TableGeneration {
        table_name: "ResidencyDelete".into(), table_id: original.table_id,
    })).await, Err(crab_cell_runtime::client::InvocationError::Rejected(result)) if result.output.0 == beyonddb::DeleteTableOutcome::TableNotFound)
    );
    assert_eq!(
        sdk.scan()
            .table_name("ResidencyDelete")
            .send()
            .await
            .unwrap()
            .items
            .unwrap(),
        items
    );
    fixture.shutdown().await;
}
