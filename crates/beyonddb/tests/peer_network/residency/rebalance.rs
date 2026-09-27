use super::provisioning::{Remote, sdk_without_retries};
use super::*;
use crab_cell_runtime::identity::CellTarget;
use std::time::Duration;

async fn wait_for_owner(
    fixture: &Fixture,
    targets: &[CellTarget],
    owner: Option<SessionId>,
) -> CellTarget {
    let authority = CellAuthority::new(fixture.layout.clone());
    tokio::time::timeout(Duration::from_secs(100), async {
        loop {
            for target in targets {
                let control = authority.load(target.cell_id()).await.unwrap().unwrap();
                let expected_state = if owner.is_some() {
                    crab_cell_runtime::control::ControlState::Serving
                } else {
                    crab_cell_runtime::control::ControlState::Idle
                };
                if control.value().owner.as_ref().map(|owner| owner.session) == owner
                    && control.value().state == expected_state
                {
                    return target.clone();
                }
            }
            assert!(fixture.node.is_ready());
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("range movement must progress within the residence window")
}

async fn drain(fixture: &Fixture, node: &CellNode, target: &CellTarget) {
    let proof =
        crab_cell_runtime::cell::catalog::CellCatalog::new(fixture.layout.clone(), target.tenant())
            .lookup(target.cell_id())
            .await
            .unwrap()
            .unwrap();
    let control = CellAuthority::new(fixture.layout.clone())
        .load(target.cell_id())
        .await
        .unwrap()
        .unwrap();
    node.runtime()
        .local_handle(proof, &control)
        .await
        .unwrap()
        .unwrap()
        .drain()
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_reads_index_moved_automatically_to_added_capacity() {
    let fixture = Fixture::with_partition_count(1).await;
    let sdk = sdk_without_retries(&fixture);
    let name = "ResidencyMovingIndex";
    super::provisioning::create(&sdk, name, true)
        .send()
        .await
        .unwrap();
    let item = HashMap::from([
        ("id".into(), AwsAttributeValue::S("one".into())),
        ("bucket".into(), AwsAttributeValue::S("move".into())),
    ]);
    sdk.put_item()
        .table_name(name)
        .set_item(Some(item.clone()))
        .send()
        .await
        .unwrap();
    let account = account_target("123456789012").unwrap();
    let table = fixture
        .client
        .query::<DescribeTable>(&account, None, Json(name.into()))
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
    let base =
        beyonddb::data_target("123456789012", &table.id, &base.partitions[0].partition_id).unwrap();
    let storage = beyonddb::CellStorage::new(fixture.client.clone(), "us-east-1");
    while storage
        .project_index_changes("123456789012", &base, &table.id)
        .await
        .unwrap()
    {}
    let index = &table.global_secondary_indexes[0];
    let page = beyonddb::read_global_index_route_page(
        &fixture.client,
        "123456789012",
        beyonddb::RoutePageInput {
            table_id: index.id.clone(),
            start_hash: None,
            after_lower: None,
            expected_epoch: None,
        },
    )
    .await
    .unwrap();
    let beyonddb::RoutePageOutcome::Page { partitions, .. } = page else {
        panic!("index route missing")
    };
    let index_target =
        beyonddb::global_index_target("123456789012", &index.id, &partitions[0].partition_id)
            .unwrap();
    // Retain only metadata and this index so an ownership-count donation must
    // exercise a GSI, independent of the planner's Cell-ID tie breaker.
    let active = fixture.node.runtime().active_cell_targets().await.unwrap();
    for target in active
        .iter()
        .filter(|target| target.namespace() == base.namespace())
    {
        drain(&fixture, &fixture.node, target).await;
    }
    let remote = Remote::new(&fixture).await;
    fixture
        .provisioner
        .install_range_rebalance_loop(&fixture.tasks)
        .unwrap();
    wait_for_owner(
        &fixture,
        std::slice::from_ref(&index_target),
        Some(remote.session),
    )
    .await;
    let query = || {
        sdk.query()
            .table_name(name)
            .index_name("ByBucket")
            .key_condition_expression("#bucket = :b")
            .expression_attribute_names("#bucket", "bucket")
            .expression_attribute_values(":b", AwsAttributeValue::S("move".into()))
    };
    assert_eq!(
        query().send().await.unwrap().items(),
        std::slice::from_ref(&item)
    );
    drain(&fixture, &remote.node, &index_target).await;
    assert_eq!(query().send().await.unwrap().items(), &[item]);
    remote.shutdown().await;
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_restores_released_range_when_rebalance_receiver_is_unreachable() {
    let fixture = Fixture::new().await;
    let remote = Remote::new(&fixture).await;
    remote.stop_listener();
    fixture
        .provisioner
        .install_range_rebalance_loop(&fixture.tasks)
        .unwrap();
    let targets = fixture
        .node
        .runtime()
        .active_cell_targets()
        .await
        .unwrap()
        .into_iter()
        .filter(|target| {
            fixture
                .data
                .iter()
                .any(|(handle, _)| handle.cell_id() == target.cell_id())
        })
        .collect::<Vec<_>>();
    let released = wait_for_owner(&fixture, &targets, None).await;
    let item = fixture
        .data
        .iter()
        .find(|(handle, _)| handle.cell_id() == released.cell_id())
        .unwrap()
        .1
        .clone();
    let session = remote.session;
    remote.shutdown().await;
    let advertisement = fixture
        .directory
        .load(session, now_ms())
        .await
        .unwrap()
        .unwrap();
    fixture
        .directory
        .withdraw(&advertisement, now_ms())
        .await
        .unwrap();
    let restored = sdk_without_retries(&fixture)
        .get_item()
        .table_name("Residency")
        .key("id", item["id"].clone())
        .consistent_read(true)
        .send()
        .await
        .unwrap();
    assert_eq!(restored.item, Some(item));
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_reads_ranges_moved_automatically_to_added_capacity() {
    let fixture = Fixture::new().await;
    fixture
        .provisioner
        .install_range_rebalance_loop(&fixture.tasks)
        .unwrap();
    let remote = Remote::new(&fixture).await;
    let authority = CellAuthority::new(fixture.layout.clone());
    let moved = tokio::time::timeout(Duration::from_secs(100), async {
        loop {
            for (handle, item) in &fixture.data {
                let control = authority.load(handle.cell_id()).await.unwrap().unwrap();
                if control
                    .value()
                    .owner
                    .as_ref()
                    .is_some_and(|owner| owner.session == remote.session)
                    && control.value().state == crab_cell_runtime::control::ControlState::Serving
                {
                    return (handle.cell_id(), item.clone());
                }
            }
            assert!(fixture.node.is_ready() && remote.node.is_ready());
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("new capacity must receive an existing settled range");
    let sdk = sdk_without_retries(&fixture);
    let read = sdk
        .get_item()
        .table_name("Residency")
        .key("id", moved.1["id"].clone())
        .consistent_read(true)
        .send()
        .await
        .unwrap();
    assert_eq!(read.item, Some(moved.1.clone()));
    let entries = remote
        .node
        .runtime()
        .active_catalog_entries()
        .await
        .unwrap();
    let entry = entries
        .iter()
        .find(|entry| entry.cell() == moved.0)
        .unwrap();
    let target = crab_cell_runtime::identity::CellTarget::new(
        account_target("123456789012").unwrap().tenant(),
        beyonddb::APPLICATION_ID,
        entry.namespace(),
        entry.partition(),
    )
    .unwrap();
    let proof =
        crab_cell_runtime::cell::catalog::CellCatalog::new(fixture.layout.clone(), target.tenant())
            .lookup(moved.0)
            .await
            .unwrap()
            .unwrap();
    let control = authority.load(moved.0).await.unwrap().unwrap();
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
    let restored = sdk
        .get_item()
        .table_name("Residency")
        .key("id", moved.1["id"].clone())
        .consistent_read(true)
        .send()
        .await
        .unwrap();
    assert_eq!(restored.item, Some(moved.1));
    remote.shutdown().await;
    fixture.shutdown().await;
}
