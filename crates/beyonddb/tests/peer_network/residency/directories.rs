use super::provisioning::{Remote, wait_for_expiry};
use super::*;
use beyonddb::{
    DirectoryMode, DirectoryPage, DirectoryPageInput, DirectorySpec, ReadDirectory,
    ReadDirectoryPage, directory_target,
};
use crab_cell_runtime::{MutationIdentity, cell::catalog::CellCatalog, identity::RequestId};
use std::time::Duration;

fn mutation() -> MutationIdentity {
    MutationIdentity {
        request_id: RequestId::from_bytes(*uuid::Uuid::now_v7().as_bytes()),
        issued_at_ms: now_ms(),
        expires_at_ms: now_ms() + 60_000,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn directory_split_and_retirement_follow_remote_owners() {
    const ACCOUNT: &str = "123456789012";
    let fixture = Fixture::with_partition_count(4).await;
    let table = fixture
        .client
        .query::<DescribeTable>(
            &account_target(ACCOUNT).unwrap(),
            None,
            Json("Residency".into()),
        )
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let root = DirectorySpec {
        table_id: table.id,
        node_id: [0; 16],
        lower: [0; 16],
        upper: None,
        depth: 0,
    };
    let target = directory_target(ACCOUNT, &root).unwrap();
    let remote = Remote::new(&fixture).await;
    // Use real signed capacity to place the copies on the empty node. The
    // controller must create, copy and open through authenticated admission.
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let node = fixture
                .directory
                .load(fixture.session, now_ms())
                .await
                .unwrap()
                .unwrap();
            if node
                .advertisement()
                .placement_capacity()
                .unwrap()
                .active_cells
                >= 5
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let split = fixture
        .provisioner
        .split_directory(&fixture.client, ACCOUNT, &root)
        .await
        .unwrap();
    let authority = CellAuthority::new(fixture.layout.clone());
    for child in &split.children {
        let target = directory_target(ACCOUNT, child).unwrap();
        let control = authority.load(target.cell_id()).await.unwrap().unwrap();
        assert_eq!(
            control.value().owner.as_ref().unwrap().session,
            remote.session
        );
    }
    // Releasing a copied child must restore its published contents through the
    // normal peer resolver, without the controller re-installing the copy.
    let child = &split.children[0];
    let child_target = directory_target(ACCOUNT, child).unwrap();
    let proof = CellCatalog::new(fixture.layout.clone(), child_target.tenant())
        .lookup(child_target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let control = authority
        .load(child_target.cell_id())
        .await
        .unwrap()
        .unwrap();
    remote
        .node
        .runtime()
        .local_handle(proof, &control)
        .await
        .unwrap()
        .unwrap()
        .drain()
        .await
        .unwrap();
    let page = fixture
        .client
        .query::<ReadDirectoryPage>(
            &child_target,
            None,
            Json(DirectoryPageInput {
                hash: child.lower,
                expected_version: Some(split.version + 1),
            }),
        )
        .await
        .unwrap()
        .output
        .0;
    assert!(matches!(page, DirectoryPage::Leaf { ranges, .. } if ranges.len() == 2));
    assert_eq!(
        authority
            .load(child_target.cell_id())
            .await
            .unwrap()
            .unwrap()
            .value()
            .owner
            .as_ref()
            .unwrap()
            .session,
        remote.session
    );

    // The first step must retire a live remote child and acknowledge it locally.
    assert!(
        !fixture
            .provisioner
            .retire_directory_step(&fixture.client, ACCOUNT, &root)
            .await
            .unwrap()
    );
    assert!(matches!(
        fixture
            .client
            .query::<ReadDirectory>(&target, None, Json(()))
            .await
            .unwrap()
            .output
            .0
            .unwrap()
            .mode,
        DirectoryMode::Retiring {
            acknowledged: 1,
            ..
        }
    ));
    // Drop the other child's receipt, then lose its owner. Recovery must observe
    // the retained terminal fence and finish the parent without another copy.
    let second = directory_target(ACCOUNT, &split.children[1]).unwrap();
    fixture
        .client
        .command::<beyonddb::RetireDirectory>(&second, mutation(), Json(split.children[1].clone()))
        .await
        .unwrap();
    let former = remote.session;
    remote.shutdown().await;
    // This fixture stops the runtime without the server's session-retirement
    // hook. Its last signed advertisement remains eligible until lease expiry.
    wait_for_expiry(&fixture, former).await;
    assert!(
        fixture
            .provisioner
            .retire_directory_step(&fixture.client, ACCOUNT, &root)
            .await
            .unwrap()
    );
    for spec in std::iter::once(&root).chain(split.children.iter()) {
        let target = directory_target(ACCOUNT, spec).unwrap();
        assert_eq!(
            fixture
                .client
                .query::<ReadDirectory>(&target, None, Json(()))
                .await
                .unwrap()
                .output
                .0
                .unwrap()
                .mode,
            DirectoryMode::Retired
        );
    }
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_index_reads_follow_split_metadata_and_generation_retirement() {
    const ACCOUNT: &str = "123456789012";
    const TABLE: &str = "ResidencyDirectory";
    let fixture = Fixture::with_partition_count(4).await;
    let remote = Remote::new(&fixture).await;
    let sdk = super::provisioning::sdk_without_retries(&fixture);
    super::provisioning::create(&sdk, TABLE, true)
        .send()
        .await
        .unwrap();
    let account = account_target(ACCOUNT).unwrap();
    let table = fixture
        .client
        .query::<DescribeTable>(&account, None, Json(TABLE.into()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let index = &table.global_secondary_indexes[0];
    let root = DirectorySpec::root(index.id.clone());
    let ranges = crate::single_leaf_route(&fixture.client, &account, &table.id.clone())
        .await
        .unwrap()
        .partitions;
    let mut expected = Vec::new();
    for ordinal in 0..8 {
        let item = SdkItem::from([
            ("id".into(), AwsAttributeValue::S(format!("item-{ordinal}"))),
            (
                "bucket".into(),
                AwsAttributeValue::S(format!("bucket-{ordinal}")),
            ),
        ]);
        sdk.put_item()
            .table_name(TABLE)
            .set_item(Some(item.clone()))
            .send()
            .await
            .unwrap();
        expected.push(item);
    }
    let storage = beyonddb::CellStorage::new(fixture.client.clone(), "us-east-1");
    for range in ranges {
        let target = beyonddb::data_target(ACCOUNT, &table.id, &range.partition_id).unwrap();
        while storage
            .project_index_changes(ACCOUNT, &target, &table.id)
            .await
            .unwrap()
        {}
    }
    let split = fixture
        .provisioner
        .split_directory(&fixture.client, ACCOUNT, &root)
        .await
        .unwrap();
    // Mutate one leaf after metadata cutover: versions now differ across leaves.
    // An index Scan must check its previous leaf, then cross the immutable bound.
    let leaf = beyonddb::read_directory_leaf(&fixture.client, account.tenant(), &index.id, [0; 16])
        .await
        .unwrap();
    let source = &leaf.ranges[0];
    fixture
        .provisioner
        .split_global_index_partition(
            ACCOUNT,
            fixture.client.clone(),
            &index.id,
            source.partition_id,
            source.lower,
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(fixture.layout.clone());
    for spec in std::iter::once(&root).chain(split.children.iter()) {
        let target = directory_target(ACCOUNT, spec).unwrap();
        let proof = CellCatalog::new(fixture.layout.clone(), target.tenant())
            .lookup(target.cell_id())
            .await
            .unwrap()
            .unwrap();
        let control = authority.load(target.cell_id()).await.unwrap().unwrap();
        let node = if control.value().owner.as_ref().unwrap().session == fixture.session {
            &fixture.node
        } else {
            &remote.node
        };
        node.runtime()
            .local_handle(proof, &control)
            .await
            .unwrap()
            .unwrap()
            .drain()
            .await
            .unwrap();
    }
    let mut actual = Vec::new();
    let mut cursor = None;
    loop {
        let page = sdk
            .scan()
            .table_name(TABLE)
            .index_name("ByBucket")
            .limit(1)
            .set_exclusive_start_key(cursor)
            .send()
            .await
            .unwrap();
        actual.extend(page.items.unwrap_or_default());
        cursor = page.last_evaluated_key;
        if cursor.is_none() {
            break;
        }
    }
    actual.sort_by_key(|item| item["id"].as_s().unwrap().clone());
    assert_eq!(actual, expected);
    for item in &expected {
        let query = sdk
            .query()
            .table_name(TABLE)
            .index_name("ByBucket")
            .key_condition_expression("#b = :b")
            .expression_attribute_names("#b", "bucket")
            .expression_attribute_values(":b", item["bucket"].clone())
            .send()
            .await
            .unwrap();
        assert_eq!(query.items(), std::slice::from_ref(item));
    }
    sdk.delete_table().table_name(TABLE).send().await.unwrap();
    let mut complete = false;
    for _ in 0..8 {
        if fixture
            .provisioner
            .continue_table_deletion(&fixture.client, ACCOUNT, &table.id)
            .await
            .unwrap()
        {
            complete = true;
            break;
        }
    }
    assert!(complete);
    for spec in std::iter::once(&root).chain(split.children.iter()) {
        let target = directory_target(ACCOUNT, spec).unwrap();
        assert_eq!(
            fixture
                .client
                .query::<ReadDirectory>(&target, None, Json(()))
                .await
                .unwrap()
                .output
                .0
                .unwrap()
                .mode,
            DirectoryMode::Retired
        );
    }
    assert!(
        fixture
            .client
            .query::<DescribeTable>(&account, None, Json(TABLE.into()))
            .await
            .unwrap()
            .output
            .0
            .is_none()
    );
    remote.shutdown().await;
    fixture.shutdown().await;
}

struct UnavailableDirectory {
    target: crab_cell_runtime::identity::CellTarget,
    attempts: std::sync::atomic::AtomicUsize,
    enabled: std::sync::atomic::AtomicBool,
}

impl crab_cell_runtime::client::LocalCellResolver for UnavailableDirectory {
    fn resolve(
        &self,
        target: crab_cell_runtime::identity::CellTarget,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = crab_cell_runtime::Result<Option<CellHandle>>> + Send>,
    > {
        let unavailable =
            target == self.target && self.enabled.load(std::sync::atomic::Ordering::SeqCst);
        if unavailable {
            self.attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        Box::pin(async move {
            if unavailable {
                Err(crab_cell_runtime::Error::CellNotActive)
            } else {
                Ok(None)
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_index_directory_does_not_starve_healthy_index_maintenance() {
    use aws_sdk_dynamodb::types::{
        GlobalSecondaryIndex, KeySchemaElement, KeyType, Projection, ProjectionType,
    };
    const ACCOUNT: &str = "123456789012";
    const TABLE: &str = "ResidencyDirectoryOutage";
    // Leave room for two index directories, data owners and a range split.
    let fixture = Fixture::with_capacity(1, 12).await;
    let sdk = super::provisioning::sdk_without_retries(&fixture);
    super::provisioning::create(&sdk, TABLE, true)
        .global_secondary_indexes(
            GlobalSecondaryIndex::builder()
                .index_name("HealthyBucket")
                .key_schema(
                    KeySchemaElement::builder()
                        .attribute_name("bucket")
                        .key_type(KeyType::Hash)
                        .build()
                        .unwrap(),
                )
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
    let account = account_target(ACCOUNT).unwrap();
    let table = fixture
        .client
        .query::<DescribeTable>(&account, None, Json(TABLE.into()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    assert_eq!(
        table.global_secondary_indexes[0].specification.index_name,
        "ByBucket"
    );
    let healthy = &table.global_secondary_indexes[1];
    let unavailable = Arc::new(UnavailableDirectory {
        target: directory_target(
            ACCOUNT,
            &DirectorySpec::root(table.global_secondary_indexes[0].id.clone()),
        )
        .unwrap(),
        attempts: std::sync::atomic::AtomicUsize::new(0),
        enabled: std::sync::atomic::AtomicBool::new(true),
    });
    let client = fixture
        .client
        .clone()
        .with_local_resolver(unavailable.clone());
    let mut cursor = Some(beyonddb::CapacityCursor {
        table_name: TABLE.into(),
        after_lower: None,
        index: Some(0),
    });
    assert!(
        fixture
            .provisioner
            .reconcile_account_capacity(ACCOUNT, client.clone(), 1, &mut cursor)
            .await
            .is_err()
    );
    assert_eq!(cursor.as_ref().unwrap().index, Some(1));
    assert!(
        fixture
            .provisioner
            .reconcile_account_capacity(ACCOUNT, client.clone(), 1, &mut cursor)
            .await
            .unwrap()
    );
    let leaf =
        beyonddb::read_directory_leaf(&fixture.client, account.tenant(), &healthy.id, [0; 16])
            .await
            .unwrap();
    assert_eq!(
        leaf.ranges.len(),
        2,
        "healthy index split must publish despite the other directory outage"
    );

    let route = crate::single_leaf_route(&fixture.client, &account, &table.id.clone())
        .await
        .unwrap();
    let source =
        beyonddb::data_target(ACCOUNT, &table.id, &route.partitions[0].partition_id).unwrap();
    let item = |version: &str| {
        SdkItem::from([
            ("id".into(), AwsAttributeValue::S("row".into())),
            ("bucket".into(), AwsAttributeValue::S("same".into())),
            ("version".into(), AwsAttributeValue::S(version.into())),
        ])
    };
    sdk.put_item()
        .table_name(TABLE)
        .set_item(Some(item("first")))
        .send()
        .await
        .unwrap();
    beyonddb::CellStorage::new(client, "us-east-1")
        .install_global_index_loop(
            &fixture.tasks,
            vec![ACCOUNT.into()],
            fixture.provisioner.clone(),
            fixture.directory.clone(),
        )
        .unwrap();
    for version in ["first", "second", "third"] {
        if version == "third" {
            // Retry ordering must survive owner release with older work pending.
            let proof = CellCatalog::new(fixture.layout.clone(), source.tenant())
                .lookup(source.cell_id())
                .await
                .unwrap()
                .unwrap();
            let control = CellAuthority::new(fixture.layout.clone())
                .load(source.cell_id())
                .await
                .unwrap()
                .unwrap();
            fixture
                .node
                .runtime()
                .local_handle(proof, &control)
                .await
                .unwrap()
                .unwrap()
                .drain()
                .await
                .unwrap();
        }
        if version != "first" {
            sdk.put_item()
                .table_name(TABLE)
                .set_item(Some(item(version)))
                .send()
                .await
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let result = sdk
                    .query()
                    .table_name(TABLE)
                    .index_name("HealthyBucket")
                    .key_condition_expression("#b = :b")
                    .expression_attribute_names("#b", "bucket")
                    .expression_attribute_values(":b", AwsAttributeValue::S("same".into()))
                    .send()
                    .await
                    .unwrap();
                if result.items() == [item(version)] {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("healthy index must keep advancing during metadata outage");
    }
    assert!(
        unavailable
            .attempts
            .load(std::sync::atomic::Ordering::SeqCst)
            > 1
    );
    assert!(
        fixture
            .client
            .query::<beyonddb::ReadPartitionIndexChange>(&source, None, Json(table.id.clone()))
            .await
            .unwrap()
            .output
            .0
            .is_some(),
        "failed index delivery must retain the source journal"
    );
    unavailable
        .enabled
        .store(false, std::sync::atomic::Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if fixture
                .client
                .query::<beyonddb::ReadPartitionIndexChange>(&source, None, Json(table.id.clone()))
                .await
                .unwrap()
                .output
                .0
                .is_none()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("all retained work must acknowledge after metadata recovery");
    for name in ["ByBucket", "HealthyBucket"] {
        let result = sdk
            .query()
            .table_name(TABLE)
            .index_name(name)
            .key_condition_expression("#b = :b")
            .expression_attribute_names("#b", "bucket")
            .expression_attribute_values(":b", AwsAttributeValue::S("same".into()))
            .send()
            .await
            .unwrap();
        assert_eq!(result.items(), [item("third")]);
    }
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_base_directory_does_not_starve_next_table_capacity() {
    const ACCOUNT: &str = "123456789012";
    let fixture = Fixture::with_capacity(2, 12).await;
    let sdk = super::provisioning::sdk_without_retries(&fixture);
    super::reclamation::create(&sdk, "ResidencyHealthy", false).await;
    let account = account_target(ACCOUNT).unwrap();
    let mut tables = Vec::new();
    for name in ["Residency", "ResidencyHealthy"] {
        tables.push(
            fixture
                .client
                .query::<DescribeTable>(&account, None, Json(name.into()))
                .await
                .unwrap()
                .output
                .0
                .unwrap(),
        );
    }
    let unavailable = Arc::new(UnavailableDirectory {
        target: directory_target(ACCOUNT, &DirectorySpec::root(tables[0].id.clone())).unwrap(),
        attempts: std::sync::atomic::AtomicUsize::new(0),
        enabled: std::sync::atomic::AtomicBool::new(true),
    });
    let client = fixture.client.clone().with_local_resolver(unavailable);
    let mut cursor = None;
    assert!(
        fixture
            .provisioner
            .reconcile_account_capacity(ACCOUNT, client.clone(), 1, &mut cursor)
            .await
            .is_err()
    );
    assert_eq!(
        cursor.as_ref().map(|cursor| cursor.table_name.as_str()),
        Some("Residency")
    );
    assert!(
        fixture
            .provisioner
            .reconcile_account_capacity(ACCOUNT, client, 1, &mut cursor)
            .await
            .unwrap()
    );
    let route = crate::single_leaf_route(&fixture.client, &account, &tables[1].id)
        .await
        .unwrap();
    assert_eq!(route.partitions.len(), 3);
    sdk.put_item()
        .table_name("ResidencyHealthy")
        .item("id", AwsAttributeValue::S("after-split".into()))
        .send()
        .await
        .unwrap();
    let result = sdk
        .get_item()
        .table_name("ResidencyHealthy")
        .consistent_read(true)
        .key("id", AwsAttributeValue::S("after-split".into()))
        .send()
        .await
        .unwrap();
    assert_eq!(
        result.item,
        Some(HashMap::from([(
            "id".into(),
            AwsAttributeValue::S("after-split".into())
        )]))
    );
    fixture.shutdown().await;
}
