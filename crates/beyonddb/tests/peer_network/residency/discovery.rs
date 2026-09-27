use super::provisioning::{Remote, create, sdk_without_retries};
use super::*;
use object_store::throttle::{ThrottleConfig, ThrottledStore};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_placement_accepts_heartbeats_renewed_during_discovery() {
    let slow = Arc::new(ThrottledStore::new(
        InMemory::new(),
        ThrottleConfig::default(),
    ));
    let fixture = Fixture::with_store(2, slow.clone()).await;
    let remote = Remote::new(&fixture).await;
    let sdk = sdk_without_retries(&fixture);
    // Discovery crosses a real three-second heartbeat renewal. Both valid
    // owners will advertise a sample newer than the listing's start time.
    slow.config_mut(|config| config.wait_list_per_call = Duration::from_secs(4));
    let created = create(&sdk, "ResidencyDiscovery", false).send().await;
    slow.config_mut(|config| config.wait_list_per_call = Duration::ZERO);
    created.unwrap();
    let item = HashMap::from([
        ("id".into(), AwsAttributeValue::S("after-discovery".into())),
        ("value".into(), AwsAttributeValue::S("durable".into())),
    ]);
    sdk.put_item()
        .table_name("ResidencyDiscovery")
        .set_item(Some(item.clone()))
        .send()
        .await
        .unwrap();
    let table = super::provisioning::table_id(&fixture, "ResidencyDiscovery").await;
    let account = account_target("123456789012").unwrap();
    let route = fixture
        .client
        .query::<ReadTableRoute>(&account, None, Json(table.clone()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let mut remote_ranges = 0;
    for range in route.partitions {
        let target = beyonddb::data_target("123456789012", &table, &range.partition_id).unwrap();
        let proof = crab_cell_runtime::cell::catalog::CellCatalog::new(
            fixture.layout.clone(),
            target.tenant(),
        )
        .lookup(target.cell_id())
        .await
        .unwrap()
        .unwrap();
        let control = CellAuthority::new(fixture.layout.clone())
            .load(target.cell_id())
            .await
            .unwrap()
            .unwrap();
        let runtime = if control.value().owner.as_ref().unwrap().session == remote.session {
            remote_ranges += 1;
            remote.node.runtime()
        } else {
            fixture.node.runtime()
        };
        runtime
            .local_handle(proof, &control)
            .await
            .unwrap()
            .unwrap()
            .drain()
            .await
            .unwrap();
    }
    assert!(remote_ranges > 0, "new capacity must receive a range");
    slow.config_mut(|config| config.wait_list_per_call = Duration::from_secs(4));
    let restored = sdk
        .get_item()
        .table_name("ResidencyDiscovery")
        .key("id", item["id"].clone())
        .consistent_read(true)
        .send()
        .await;
    slow.config_mut(|config| config.wait_list_per_call = Duration::ZERO);
    let restored = restored.unwrap();
    assert_eq!(restored.item, Some(item));
    remote.shutdown().await;
    fixture.shutdown().await;
}
