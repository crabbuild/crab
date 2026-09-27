use super::provisioning::{create, sdk_without_retries, table_id};
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

struct DeleteDuringInstall {
    sdk: aws_sdk_dynamodb::Client,
    target: crab_cell_runtime::identity::CellTarget,
    deleted: Arc<AtomicBool>,
}

impl crab_cell_runtime::client::LocalCellResolver for DeleteDuringInstall {
    fn resolve(
        &self,
        target: crab_cell_runtime::identity::CellTarget,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = crab_cell_runtime::Result<Option<CellHandle>>>
                + Send
                + 'static,
        >,
    > {
        let sdk = self.sdk.clone();
        let deleted = Arc::clone(&self.deleted);
        let selected = target == self.target;
        Box::pin(async move {
            if selected && !deleted.swap(true, Ordering::SeqCst) {
                sdk.delete_table()
                    .table_name("ResidencyPending")
                    .send()
                    .await
                    .unwrap();
            }
            Ok(None)
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_creation_recovery_tolerates_concurrent_delete() {
    let fixture = Fixture::new().await;
    let sdk = sdk_without_retries(&fixture);
    create(&sdk, "ResidencyFill", false).send().await.unwrap();
    assert!(create(&sdk, "ResidencyPending", true).send().await.is_err());
    let original = table_id(&fixture, "ResidencyPending").await;
    sdk.delete_table()
        .table_name("ResidencyFill")
        .send()
        .await
        .unwrap();
    let deleted = Arc::new(AtomicBool::new(false));
    let interrupted = fixture
        .client
        .clone()
        .with_local_resolver(Arc::new(DeleteDuringInstall {
            sdk: sdk.clone(),
            target: beyonddb::data_target("123456789012", &original, &[0; 16]).unwrap(),
            deleted: Arc::clone(&deleted),
        }));
    let mut cursor = None;
    // Sweep the existing table first, then delete the incomplete generation
    // between its discovery and base-route publication.
    for _ in 0..3 {
        fixture
            .provisioner
            .reconcile_account_capacity("123456789012", interrupted.clone(), u64::MAX, &mut cursor)
            .await
            .unwrap();
        if deleted.load(Ordering::SeqCst) {
            break;
        }
    }
    assert!(deleted.load(Ordering::SeqCst));
    assert!(
        sdk.describe_table()
            .table_name("ResidencyPending")
            .send()
            .await
            .unwrap_err()
            .as_service_error()
            .unwrap()
            .is_resource_not_found_exception()
    );
    create(&sdk, "ResidencyPending", true).send().await.unwrap();
    assert_ne!(table_id(&fixture, "ResidencyPending").await, original);
    sdk.put_item()
        .table_name("ResidencyPending")
        .item("id", AwsAttributeValue::S("recreated".into()))
        .send()
        .await
        .unwrap();
    let item = sdk
        .get_item()
        .table_name("ResidencyPending")
        .key("id", AwsAttributeValue::S("recreated".into()))
        .consistent_read(true)
        .send()
        .await
        .unwrap()
        .item
        .unwrap();
    assert_eq!(item["id"], AwsAttributeValue::S("recreated".into()));
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_worker_finishes_partial_creation_after_account_restore() {
    let fixture = Fixture::new().await;
    let sdk = sdk_without_retries(&fixture);
    create(&sdk, "ResidencyFill", false).send().await.unwrap();
    // Two index owners fit; the base owners do not. The failed public request
    // leaves a durable table generation and published index directory.
    assert!(create(&sdk, "ResidencyPending", true).send().await.is_err());
    let account = account_target("123456789012").unwrap();
    let pending = fixture
        .client
        .query::<DescribeTable>(&account, None, Json("ResidencyPending".into()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let status = sdk
        .describe_table()
        .table_name("ResidencyPending")
        .send()
        .await
        .unwrap()
        .table
        .unwrap();
    assert_eq!(
        status.table_status(),
        Some(&aws_sdk_dynamodb::types::TableStatus::Creating)
    );
    let index_before = fixture
        .client
        .query::<beyonddb::ReadGlobalIndexRoutePage>(
            &account,
            None,
            Json(beyonddb::RoutePageInput {
                table_id: pending.global_secondary_indexes[0].id.clone(),
                start_hash: None,
                after_lower: None,
                expected_epoch: None,
            }),
        )
        .await
        .unwrap()
        .output
        .0;
    assert!(matches!(
        index_before,
        beyonddb::RoutePageOutcome::Page { .. }
    ));
    sdk.delete_table()
        .table_name("ResidencyFill")
        .send()
        .await
        .unwrap();
    fixture
        .provisioner
        .admit_account("123456789012")
        .await
        .unwrap()
        .drain()
        .await
        .unwrap();
    // The production worker discovers the generation from its restored catalog;
    // no CreateTable retry or direct provisioning call completes this table.
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
            let table = sdk
                .describe_table()
                .table_name("ResidencyPending")
                .send()
                .await
                .unwrap()
                .table
                .unwrap();
            if table.table_status() == Some(&aws_sdk_dynamodb::types::TableStatus::Active) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    let index_after = fixture
        .client
        .query::<beyonddb::ReadGlobalIndexRoutePage>(
            &account,
            None,
            Json(beyonddb::RoutePageInput {
                table_id: pending.global_secondary_indexes[0].id.clone(),
                start_hash: None,
                after_lower: None,
                expected_epoch: None,
            }),
        )
        .await
        .unwrap()
        .output
        .0;
    assert_eq!(index_after, index_before);
    assert_eq!(table_id(&fixture, "ResidencyPending").await, pending.id);
    sdk.put_item()
        .table_name("ResidencyPending")
        .item("id", AwsAttributeValue::S("resumed".into()))
        .item("bucket", AwsAttributeValue::S("ready".into()))
        .send()
        .await
        .unwrap();
    let routes = fixture
        .client
        .query::<ReadTableRoute>(&account, None, Json(pending.id.clone()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let storage = beyonddb::CellStorage::new(fixture.client.clone(), "us-east-1");
    for range in &routes.partitions {
        let target =
            beyonddb::data_target("123456789012", &pending.id, &range.partition_id).unwrap();
        storage
            .project_index_changes("123456789012", &target, &pending.id)
            .await
            .unwrap();
        fixture
            .provisioner
            .admit_existing_partition("123456789012", &pending.id, &range.partition_id)
            .await
            .unwrap()
            .drain()
            .await
            .unwrap();
    }
    let item = sdk
        .get_item()
        .table_name("ResidencyPending")
        .key("id", AwsAttributeValue::S("resumed".into()))
        .consistent_read(true)
        .send()
        .await
        .unwrap()
        .item
        .unwrap();
    assert_eq!(item["bucket"], AwsAttributeValue::S("ready".into()));
    let indexed = sdk
        .scan()
        .table_name("ResidencyPending")
        .index_name("ByBucket")
        .send()
        .await
        .unwrap()
        .items
        .unwrap();
    assert_eq!(indexed, vec![item]);
    fixture.shutdown().await;
}
