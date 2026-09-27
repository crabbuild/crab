use super::*;
use aws_sdk_dynamodb::types::{
    AttributeDefinition, BillingMode, GlobalSecondaryIndex, KeySchemaElement, KeyType, Projection,
    ProjectionType, ScalarAttributeType,
};
use beyonddb::{
    ApplyGlobalIndexMutation, BeginGlobalIndexSplit, GlobalIndexApplyOutcome, GlobalIndexMutation,
    GlobalIndexPartitionInput, GlobalIndexSplitPlan, GlobalIndexState, GlobalIndexUsage,
    PartitionUsage, ProjectionVersion, ReadGlobalIndexPartition, ReadGlobalIndexSplitPlan,
    ReadGlobalIndexState, RoutePageInput, RoutePageOutcome, data_target, global_index_target,
};
use crab_cell_runtime::{MutationIdentity, cell::catalog::CellCatalog, identity::RequestId};

const ACCOUNT: &str = "123456789012";
const TABLE: &str = "ResidencyIndexSplit";

fn mutation() -> MutationIdentity {
    MutationIdentity {
        request_id: RequestId::from_bytes(*uuid::Uuid::now_v7().as_bytes()),
        issued_at_ms: now_ms(),
        expires_at_ms: now_ms() + 60_000,
    }
}

async fn scan(sdk: &aws_sdk_dynamodb::Client) -> Vec<SdkItem> {
    let mut cursor = None;
    let mut items = Vec::new();
    loop {
        let page = sdk
            .scan()
            .table_name(TABLE)
            .index_name("ByBucket")
            .limit(2)
            .set_exclusive_start_key(cursor)
            .send()
            .await
            .unwrap();
        items.extend(page.items.unwrap_or_default());
        let Some(next) = page.last_evaluated_key else {
            break;
        };
        cursor = Some(next);
    }
    items.sort_by_key(|item| item["id"].as_s().unwrap().clone());
    items
}

async fn settled(sdk: &aws_sdk_dynamodb::Client, expected: &[SdkItem]) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if scan(sdk).await == expected {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "index did not converge"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_capacity_splits_indexes_and_preserves_tombstones_after_owner_restore() {
    // One directory owner is additional to the original eight-slot workload.
    let fixture = Fixture::with_capacity(2, 9).await;
    let sdk = aws_sdk_dynamodb::Client::from_conf(
        fixture
            .sdk
            .config()
            .to_builder()
            .retry_config(aws_sdk_dynamodb::config::retry::RetryConfig::disabled())
            .build(),
    );
    let key = |name: &str| {
        KeySchemaElement::builder()
            .attribute_name(name)
            .key_type(KeyType::Hash)
            .build()
            .unwrap()
    };
    sdk.create_table()
        .table_name(TABLE)
        .billing_mode(BillingMode::PayPerRequest)
        .key_schema(key("id"))
        .attribute_definitions(
            AttributeDefinition::builder()
                .attribute_name("id")
                .attribute_type(ScalarAttributeType::S)
                .build()
                .unwrap(),
        )
        .attribute_definitions(
            AttributeDefinition::builder()
                .attribute_name("bucket")
                .attribute_type(ScalarAttributeType::S)
                .build()
                .unwrap(),
        )
        .global_secondary_indexes(
            GlobalSecondaryIndex::builder()
                .index_name("ByBucket")
                .key_schema(key("bucket"))
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
    let base = fixture
        .client
        .query::<ReadTableRoute>(&account, None, Json(table.id.clone()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let index = &table.global_secondary_indexes[0];
    let index_directory =
        beyonddb::directory_target(ACCOUNT, &beyonddb::DirectorySpec::root(index.id.clone()))
            .unwrap();
    let page = RoutePageInput {
        table_id: index.id.clone(),
        start_hash: None,
        after_lower: None,
        expected_epoch: None,
    };
    let RoutePageOutcome::Page {
        partitions, epoch, ..
    } = beyonddb::read_global_index_route_page(&fixture.client, "123456789012", page.clone())
        .await
        .unwrap()
    else {
        panic!("missing index route");
    };
    let sources = partitions
        .iter()
        .map(|part| global_index_target(ACCOUNT, &index.id, &part.partition_id).unwrap())
        .collect::<Vec<_>>();
    let mut boundary = [0; 16];
    boundary[0] = 0x40;
    let mut buckets = [None, None];
    for n in 0..1000 {
        let name = format!("bucket-{n}");
        let key = Item::from([("bucket".into(), AttributeValue::S(name.clone()))]);
        let hash =
            beyonddb::data_key_hash(&index.id, &key, &index.specification.key_schema).unwrap();
        if hash < partitions[0].upper.unwrap() {
            buckets[usize::from(hash >= boundary)].get_or_insert(name);
        }
        if buckets.iter().all(Option::is_some) {
            break;
        }
    }
    let buckets = buckets.map(Option::unwrap);
    beyonddb::CellStorage::new(fixture.client.clone(), "us-east-1")
        .install_global_index_loop(
            &fixture.tasks,
            vec![ACCOUNT.into()],
            fixture.provisioner.clone(),
            fixture.directory.clone(),
        )
        .unwrap();
    let mut items = Vec::new();
    for n in 0..12 {
        let range = &base.partitions[n % 2];
        let id = (0..1000)
            .map(|i| format!("row-{n}-{i}"))
            .find(|id| {
                let hash = beyonddb::data_key_hash(
                    &table.id,
                    &Item::from([("id".into(), AttributeValue::S(id.clone()))]),
                    &table.key_schema,
                )
                .unwrap();
                range.lower.is_none_or(|lower| hash >= lower)
                    && range.upper.is_none_or(|upper| hash < upper)
            })
            .unwrap();
        let item = SdkItem::from([
            ("id".into(), AwsAttributeValue::S(id)),
            (
                "bucket".into(),
                AwsAttributeValue::S(buckets[n % 2].clone()),
            ),
            (
                "payload".into(),
                AwsAttributeValue::S("x".repeat(32 * 1024)),
            ),
        ]);
        sdk.put_item()
            .table_name(TABLE)
            .set_item(Some(item.clone()))
            .send()
            .await
            .unwrap();
        items.push(item);
    }
    items.sort_by_key(|item| item["id"].as_s().unwrap().clone());
    settled(&sdk, &items).await;
    let deleted = items.remove(0);
    sdk.delete_item()
        .table_name(TABLE)
        .key("id", deleted["id"].clone())
        .send()
        .await
        .unwrap();
    settled(&sdk, &items).await;
    // Reserve the unrelated empty source before the busy source changes the
    // directory epoch. Its plan must survive the first publication.
    let empty = fixture
        .client
        .query::<ReadGlobalIndexPartition>(&sources[1], None, Json(()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let mut empty_children = [empty.clone(), empty.clone()];
    let mut empty_boundary = [0; 16];
    empty_boundary[0] = 0xc0;
    empty_children[0].partition_id = [113; 16];
    empty_children[0].upper = Some(empty_boundary);
    empty_children[0].epoch = epoch + 1;
    empty_children[1].partition_id = [114; 16];
    empty_children[1].lower = Some(empty_boundary);
    empty_children[1].epoch = epoch + 1;
    let pending = GlobalIndexSplitPlan {
        source: empty,
        children: empty_children,
        expected_epoch: epoch,
    };
    assert!(
        fixture
            .client
            .command::<BeginGlobalIndexSplit>(&index_directory, mutation(), Json(pending.clone()))
            .await
            .unwrap()
            .output
            .0
    );
    let active = fixture
        .client
        .query::<ReadGlobalIndexPartition>(&sources[0], None, Json(()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let mut colliding_children = [active.clone(), active.clone()];
    colliding_children[0].partition_id = pending.children[0].partition_id;
    colliding_children[0].upper = Some(boundary);
    colliding_children[0].epoch = epoch + 1;
    colliding_children[1].partition_id = [115; 16];
    colliding_children[1].lower = Some(boundary);
    colliding_children[1].epoch = epoch + 1;
    let collision = GlobalIndexSplitPlan {
        source: active,
        children: colliding_children,
        expected_epoch: epoch,
    };
    assert!(matches!(
        fixture
            .client
            .command::<BeginGlobalIndexSplit>(&index_directory, mutation(), Json(collision))
            .await,
        Err(crab_cell_runtime::client::InvocationError::Rejected(_))
    ));
    // Administrative metadata must not strand an immutable range plan.
    sdk.update_table()
        .table_name(TABLE)
        .deletion_protection_enabled(true)
        .send()
        .await
        .unwrap();
    let index_bytes = fixture
        .client
        .query::<GlobalIndexUsage>(&sources[0], None, Json(()))
        .await
        .unwrap()
        .output
        .0;
    let mut base_bytes = 0;
    for range in &base.partitions {
        let target = data_target(ACCOUNT, &table.id, &range.partition_id).unwrap();
        base_bytes = base_bytes.max(
            fixture
                .client
                .query::<PartitionUsage>(&target, None, Json(()))
                .await
                .unwrap()
                .output
                .0
                .database_bytes,
        );
    }
    assert!(
        index_bytes > base_bytes,
        "fixture must put pressure on the index alone: {index_bytes} <= {base_bytes}"
    );
    let threshold = base_bytes + (index_bytes - base_bytes) / 2;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let node = fixture
                .directory
                .load(fixture.session, now_ms())
                .await
                .unwrap()
                .unwrap();
            let capacity = node.advertisement().placement_capacity().unwrap();
            if capacity.active_cells == capacity.max_active_cells {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("fixture should advertise all nine slots occupied");
    let mut cursor = None;
    let mut deferred = false;
    for _ in 0..12 {
        match fixture
            .provisioner
            .reconcile_account_capacity(ACCOUNT, fixture.client.clone(), threshold, &mut cursor)
            .await
        {
            Ok(false) => {}
            Err(extenddb_storage::error::StorageError::LimitExceeded(_)) => {
                deferred = true;
                break;
            }
            result => panic!("full fleet should defer the oversized index: {result:?}"),
        }
    }
    assert!(deferred);
    let plan = fixture
        .client
        .query::<ReadGlobalIndexSplitPlan>(
            &index_directory,
            None,
            Json(GlobalIndexPartitionInput {
                index_id: index.id.clone(),
                partition_id: partitions[0].partition_id,
            }),
        )
        .await
        .unwrap()
        .output
        .0
        .expect("capacity refusal must retain the plan");
    fixture
        .provisioner
        .install_account_capacity_loop(
            &fixture.tasks,
            ACCOUNT.into(),
            fixture.client.clone(),
            u64::MAX,
            std::time::Duration::from_millis(20),
        )
        .unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
    while tokio::time::Instant::now() < deadline {
        assert!(
            fixture.node.is_ready(),
            "capacity pressure stopped the serving node"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let remote = super::provisioning::Remote::new(&fixture).await;
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let mut done = true;
            for plan in [&plan, &pending] {
                done &= fixture
                    .client
                    .query::<ReadGlobalIndexSplitPlan>(
                        &index_directory,
                        None,
                        Json(GlobalIndexPartitionInput {
                            index_id: index.id.clone(),
                            partition_id: plan.source.partition_id,
                        }),
                    )
                    .await
                    .unwrap()
                    .output
                    .0
                    .is_none();
            }
            if done {
                break;
            }
            assert!(fixture.node.is_ready());
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("added capacity must resume both durable plans");
    let Some(GlobalIndexState::Sealed(plan)) = fixture
        .client
        .query::<ReadGlobalIndexState>(&sources[0], None, Json(()))
        .await
        .unwrap()
        .output
        .0
    else {
        panic!("oversized index was not split");
    };
    fixture
        .provisioner
        .resume_global_index_split(ACCOUNT, fixture.client.clone(), &plan)
        .await
        .unwrap();
    let RoutePageOutcome::Page {
        partitions: ranges,
        epoch: current,
        ..
    } = beyonddb::read_global_index_route_page(&fixture.client, "123456789012", page.clone())
        .await
        .unwrap()
    else {
        panic!("split index directory missing");
    };
    assert_eq!((ranges.len(), current), (4, epoch + 2));
    assert_eq!(
        beyonddb::read_global_index_route_page(
            &fixture.client,
            "123456789012",
            RoutePageInput {
                expected_epoch: Some(epoch),
                ..page
            }
        )
        .await
        .unwrap(),
        RoutePageOutcome::Changed
    );
    // Drain both children and metadata. SDK reads must restore their published
    // roots through the serving admission path with SDK retries disabled.
    for range in &plan.children {
        let target = global_index_target(ACCOUNT, &index.id, &range.partition_id).unwrap();
        let proof = CellCatalog::new(fixture.layout.clone(), target.tenant())
            .lookup(target.cell_id())
            .await
            .unwrap()
            .unwrap();
        let control = CellAuthority::new(fixture.layout.clone())
            .load(target.cell_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            control.value().owner.as_ref().unwrap().session,
            remote.session
        );
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
    }
    fixture
        .provisioner
        .admit_account(ACCOUNT)
        .await
        .unwrap()
        .drain()
        .await
        .unwrap();
    assert_eq!(scan(&sdk).await, items);
    for bucket in &buckets {
        let rows = sdk
            .query()
            .table_name(TABLE)
            .index_name("ByBucket")
            .key_condition_expression("#bucket = :b")
            .expression_attribute_names("#bucket", "bucket")
            .expression_attribute_values(":b", AwsAttributeValue::S(bucket.clone()))
            .send()
            .await
            .unwrap();
        assert_eq!(
            rows.items().len(),
            items
                .iter()
                .filter(|item| item["bucket"].as_s().unwrap() == bucket)
                .count()
        );
    }
    let key = Item::from([
        (
            "id".into(),
            AttributeValue::S(deleted["id"].as_s().unwrap().clone()),
        ),
        (
            "bucket".into(),
            AttributeValue::S(deleted["bucket"].as_s().unwrap().clone()),
        ),
    ]);
    let hash = beyonddb::data_key_hash(&index.id, &key, &index.specification.key_schema).unwrap();
    let child = &plan.children[usize::from(hash >= boundary)];
    let target = global_index_target(ACCOUNT, &index.id, &child.partition_id).unwrap();
    assert_eq!(
        fixture
            .client
            .command::<ApplyGlobalIndexMutation>(
                &target,
                mutation(),
                Json(GlobalIndexMutation {
                    index_id: index.id.clone(),
                    epoch: child.epoch,
                    key: key.clone(),
                    version: ProjectionVersion {
                        source_epoch: 0,
                        sequence: 1
                    },
                    item: Some(key)
                })
            )
            .await
            .unwrap()
            .output
            .0,
        GlobalIndexApplyOutcome::Superseded
    );
    assert_eq!(scan(&sdk).await, items);
    // New projections follow the replacement route after cutover.
    let row = &mut items[0];
    let next = if row["bucket"].as_s().unwrap() == &buckets[0] {
        &buckets[1]
    } else {
        &buckets[0]
    };
    row.insert("bucket".into(), AwsAttributeValue::S(next.clone()));
    sdk.put_item()
        .table_name(TABLE)
        .set_item(Some(row.clone()))
        .send()
        .await
        .unwrap();
    settled(&sdk, &items).await;
    sdk.update_table()
        .table_name(TABLE)
        .deletion_protection_enabled(false)
        .send()
        .await
        .unwrap();
    let source = pending.children[0].clone();
    let mut children = [source.clone(), source.clone()];
    let mut middle = [0; 16];
    middle[0] = 0xa0;
    children[0].partition_id = [115; 16];
    children[0].upper = Some(middle);
    children[0].epoch = source.epoch + 1;
    children[1].partition_id = [116; 16];
    children[1].lower = Some(middle);
    children[1].epoch = source.epoch + 1;
    let retiring = GlobalIndexSplitPlan {
        expected_epoch: source.epoch,
        source,
        children,
    };
    assert!(
        fixture
            .client
            .command::<BeginGlobalIndexSplit>(&index_directory, mutation(), Json(retiring.clone()))
            .await
            .unwrap()
            .output
            .0
    );
    sdk.delete_table().table_name(TABLE).send().await.unwrap();
    // The installed capacity worker retires the independent directory before
    // its retained split reservations become invisible.
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let state = fixture
                .client
                .query::<beyonddb::ReadDirectory>(&index_directory, None, Json(()))
                .await
                .unwrap()
                .output
                .0
                .unwrap();
            if state.mode == beyonddb::DirectoryMode::Retired {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    for spec in [
        &retiring.source,
        &retiring.children[0],
        &retiring.children[1],
    ] {
        assert!(
            fixture
                .client
                .query::<ReadGlobalIndexSplitPlan>(
                    &index_directory,
                    None,
                    Json(GlobalIndexPartitionInput {
                        index_id: index.id.clone(),
                        partition_id: spec.partition_id
                    })
                )
                .await
                .unwrap()
                .output
                .0
                .is_none()
        );
    }
    assert!(fixture.node.is_ready());
    remote.shutdown().await;
    fixture.shutdown().await;
}
