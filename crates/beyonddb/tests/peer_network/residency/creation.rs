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
    // One directory owner is additional to the original eight-slot workload.
    let fixture = Fixture::with_capacity(2, 9).await;
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
    super::provisioning::complete_deletion(&fixture, &sdk, "ResidencyPending").await;
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
    // One directory owner is additional to the original eight-slot workload.
    let fixture = Fixture::with_capacity(2, 9).await;
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
    let index_before = beyonddb::read_global_index_route_page(
        &fixture.client,
        "123456789012",
        beyonddb::RoutePageInput {
            table_id: pending.global_secondary_indexes[0].id.clone(),
            start_hash: None,
            after_lower: None,
            expected_epoch: None,
        },
    )
    .await
    .unwrap();
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
    let index_after = beyonddb::read_global_index_route_page(
        &fixture.client,
        "123456789012",
        beyonddb::RoutePageInput {
            table_id: pending.global_secondary_indexes[0].id.clone(),
            start_hash: None,
            after_lower: None,
            expected_epoch: None,
        },
    )
    .await
    .unwrap();
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

async fn partially_installed_table(base_installed: bool) -> (Fixture, beyonddb::TableRecord) {
    // One directory owner is additional to the original eight-slot workload.
    let fixture = Fixture::with_capacity(2, 9).await;
    let mut blockers = Vec::new();
    for ordinal in 0..if base_installed { 1 } else { 4 } {
        blockers.push(
            fixture
                .provisioner
                .admit_credential(&format!("AKIACREATIONCAPACITY{ordinal}"))
                .await
                .unwrap(),
        );
    }
    let sdk = sdk_without_retries(&fixture);
    // Leave room for one base owner or one GSI owner, interrupting installation.
    assert!(create(&sdk, "ResidencyPending", true).send().await.is_err());
    let account = account_target("123456789012").unwrap();
    let table = fixture
        .client
        .query::<DescribeTable>(&account, None, Json("ResidencyPending".into()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let first = if base_installed {
        beyonddb::data_target("123456789012", &table.id, &[0; 16])
    } else {
        beyonddb::global_index_target(
            "123456789012",
            &table.global_secondary_indexes[0].id,
            &[0; 16],
        )
    }
    .unwrap();
    assert!(
        CellAuthority::new(fixture.layout.clone())
            .load(first.cell_id())
            .await
            .unwrap()
            .unwrap()
            .value()
            .root
            .is_some()
    );
    for blocker in blockers {
        blocker.drain().await.unwrap();
    }
    fixture
        .provisioner
        .admit_account("123456789012")
        .await
        .unwrap()
        .drain()
        .await
        .unwrap();
    (fixture, table)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_creation_resumes_original_partition_count_after_restore() {
    for base_installed in [false, true] {
        let (fixture, table) = partially_installed_table(base_installed).await;
        let recovery = CellInitialPartitionProvisioner::new(
            fixture.node.runtime(),
            fixture.application.clone(),
            fixture.layout.clone(),
            fixture.session,
            fixture.endpoint.clone(),
            fixture._files.path().join("recovery"),
        )
        .unwrap()
        .with_initial_partition_count(1)
        .unwrap();
        let mut cursor = None;
        for _ in 0..3 {
            recovery
                .reconcile_account_capacity(
                    "123456789012",
                    fixture.client.clone(),
                    u64::MAX,
                    &mut cursor,
                )
                .await
                .unwrap();
        }
        let route = fixture
            .client
            .query::<ReadTableRoute>(
                &account_target("123456789012").unwrap(),
                None,
                Json(table.id.clone()),
            )
            .await
            .unwrap()
            .output
            .0
            .unwrap();
        assert_eq!(route.partitions.len(), 2);
        let index = beyonddb::read_global_index_route_page(
            &fixture.client,
            "123456789012",
            beyonddb::RoutePageInput {
                table_id: table.global_secondary_indexes[0].id.clone(),
                start_hash: None,
                after_lower: None,
                expected_epoch: None,
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(index, beyonddb::RoutePageOutcome::Page { partitions, .. } if partitions.len() == 2)
        );
        let sdk = sdk_without_retries(&fixture);
        sdk.put_item()
            .table_name("ResidencyPending")
            .item("id", AwsAttributeValue::S("original-layout".into()))
            .send()
            .await
            .unwrap();
        for range in &route.partitions {
            recovery
                .admit_existing_partition("123456789012", &table.id, &range.partition_id)
                .await
                .unwrap()
                .drain()
                .await
                .unwrap();
        }
        let item = sdk
            .get_item()
            .table_name("ResidencyPending")
            .key("id", AwsAttributeValue::S("original-layout".into()))
            .consistent_read(true)
            .send()
            .await
            .unwrap()
            .item
            .unwrap();
        assert_eq!(item["id"], AwsAttributeValue::S("original-layout".into()));
        fixture.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_update_rejects_incomplete_creation() {
    use extenddb_core::types::{DescribeTableInput, TableKeyInfo, TableStatus};
    use extenddb_storage::{DataEngine, TableEngine, error::StorageError};

    let (fixture, pending) = partially_installed_table(true).await;
    // A maintenance/read client without a provisioner must observe the same
    // creation state and must never route unpublished items into the account.
    let storage = beyonddb::CellStorage::new(fixture.client.clone(), "us-east-1");
    let description = storage
        .describe_table(
            "123456789012",
            DescribeTableInput {
                table_name: pending.table_name.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(description.table_status, TableStatus::Creating);
    let info = TableKeyInfo {
        account_id: "123456789012".into(),
        table_name: pending.table_name,
        table_id: pending.id,
        base_key_schema: pending.key_schema.clone(),
        key_schema: pending.key_schema,
        attribute_definitions: pending.attribute_definitions,
        ..TableKeyInfo::default()
    };
    let key = Item::from([("id".into(), AttributeValue::S("unpublished".into()))]);
    assert!(matches!(
        storage.get_item(&info, &key).await,
        Err(StorageError::TableNotActive(_))
    ));
    assert!(matches!(
        storage.scan(&info, None, None, None, None, None).await,
        Err(StorageError::TableNotActive(_))
    ));
    let sdk = sdk_without_retries(&fixture);
    let error = sdk
        .update_table()
        .table_name("ResidencyPending")
        .deletion_protection_enabled(true)
        .send()
        .await
        .unwrap_err();
    assert!(
        error
            .as_service_error()
            .unwrap()
            .is_resource_in_use_exception()
    );
    let mut cursor = None;
    for _ in 0..3 {
        fixture
            .provisioner
            .reconcile_account_capacity(
                "123456789012",
                fixture.client.clone(),
                u64::MAX,
                &mut cursor,
            )
            .await
            .unwrap();
    }
    let table = sdk
        .update_table()
        .table_name("ResidencyPending")
        .deletion_protection_enabled(true)
        .send()
        .await
        .unwrap()
        .table_description
        .unwrap();
    assert_eq!(table.deletion_protection_enabled, Some(true));
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_delete_fences_unpublished_directory_installers() {
    const ACCOUNT: &str = "123456789012";
    const NAME: &str = "ResidencyPending";
    for copied in [false, true] {
        let (fixture, table) = partially_installed_table(false).await;
        let sdk = sdk_without_retries(&fixture);
        let index = &table.global_secondary_indexes[0];
        let ranges: Vec<_> = fixture
            .provisioner
            .provision_global_index(&fixture.client, ACCOUNT, &table, index)
            .await
            .unwrap()
            .into_iter()
            .map(|range| beyonddb::RoutePagePartition {
                partition_id: range.partition_id,
                lower: range.lower.unwrap_or([0; 16]),
                upper: range.upper,
                epoch: range.epoch,
            })
            .collect();
        let spec = beyonddb::DirectorySpec::root(index.id.clone());
        let target = beyonddb::directory_target(ACCOUNT, &spec).unwrap();
        if copied {
            fixture
                .provisioner
                .provision_global_index_directory(
                    &fixture.client,
                    ACCOUNT,
                    &index.id,
                    ranges.clone(),
                )
                .await
                .unwrap();
        }
        // Interrupt creation before anchor publication, on either side of the
        // independent root commit. The public generation remains CREATING.
        assert!(
            fixture
                .client
                .query::<beyonddb::ReadGlobalIndexDirectory>(
                    &account_target(ACCOUNT).unwrap(),
                    None,
                    Json(index.id.clone()),
                )
                .await
                .unwrap()
                .output
                .0
                .is_none()
        );
        sdk.delete_table().table_name(NAME).send().await.unwrap();
        super::provisioning::complete_deletion(&fixture, &sdk, NAME).await;
        assert_eq!(
            fixture
                .client
                .query::<beyonddb::ReadDirectory>(&target, None, Json(()))
                .await
                .unwrap()
                .output
                .0
                .unwrap()
                .mode,
            beyonddb::DirectoryMode::Retired
        );
        let mutation = crab_cell_runtime::MutationIdentity {
            request_id: crab_cell_runtime::identity::RequestId::from_bytes(
                *uuid::Uuid::now_v7().as_bytes(),
            ),
            issued_at_ms: now_ms(),
            expires_at_ms: now_ms() + 60_000,
        };
        assert!(matches!(
            fixture
                .client
                .command::<beyonddb::InstallDirectory>(
                    &target,
                    mutation,
                    Json(beyonddb::DirectoryInstall {
                        spec,
                        ranges,
                        source: None,
                    }),
                )
                .await,
            Err(crab_cell_runtime::client::InvocationError::Rejected(_))
        ));
        create(&sdk, NAME, true).send().await.unwrap();
        assert_ne!(table_id(&fixture, NAME).await, table.id);
        sdk.put_item()
            .table_name(NAME)
            .item("id", AwsAttributeValue::S("new-generation".into()))
            .item("bucket", AwsAttributeValue::S("new".into()))
            .send()
            .await
            .unwrap();
        assert!(
            sdk.get_item()
                .table_name(NAME)
                .key("id", AwsAttributeValue::S("new-generation".into()))
                .consistent_read(true)
                .send()
                .await
                .unwrap()
                .item
                .is_some()
        );
        fixture.shutdown().await;
    }
}
