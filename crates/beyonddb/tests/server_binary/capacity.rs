use crate::*;

use std::sync::Arc;

use beyonddb::{APPLICATION_ID, Beyonddb};
use crab_cell_app::CellApplication;
use crab_cell_peer_http::LoadedPeerTls;
use crab_cell_runtime::{
    Digest, fleet::placement::PlacementObservation, ltx::CellStorageLayout, node::NodeDirectory,
    registry::BuildDescriptor,
};
use crab_storage::Store;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker, aws CLI, and observable host memory limits"]
async fn process_publishes_measured_signed_placement() {
    let mut fixture = process_fixture(1).await;
    fixture.sdk.list_tables().send().await.unwrap();
    let root = fixture.root.path();
    let tls = LoadedPeerTls::load(
        &root.join("peer.crt"),
        &root.join("peer.key"),
        &root.join("ca.crt"),
        "localhost",
    )
    .unwrap();
    let application = Beyonddb::compile(BuildDescriptor {
        source_revision: option_env!("BEYONDDB_SOURCE_REVISION")
            .map(str::to_owned)
            .unwrap_or_else(|| {
                blake3::hash(include_bytes!("../../src/bin/beyonddb.rs"))
                    .to_hex()
                    .to_string()
            }),
        cargo_lock_digest: Digest::from_bytes(
            *blake3::hash(include_bytes!("../../../../Cargo.lock")).as_bytes(),
        ),
    })
    .unwrap();
    let store = object_store::aws::AmazonS3Builder::new()
        .with_bucket_name("beyonddb-test")
        .with_region("us-east-1")
        .with_endpoint(format!("http://{}", fixture.s3))
        .with_allow_http(true)
        .with_access_key_id("crab")
        .with_secret_access_key("crab")
        .build()
        .unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(store)),
        object_store::path::Path::from("beyonddb"),
        *APPLICATION_ID.as_bytes(),
    );
    let directory = NodeDirectory::new(
        layout,
        tls.fleet(),
        application.descriptor_digest(),
        application.registry().release_digest(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let observed = loop {
        let now = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        // Loading through the directory verifies identity and both signatures;
        // reading a JSON field alone would not prove planner eligibility.
        let nodes = directory.live(now, 10).await.unwrap();
        assert_eq!(nodes.len(), 1);
        if let Ok(sample) = PlacementObservation::from_signed_advertisement(&nodes[0], now, false)
            && sample.active_cells >= 2
        {
            break sample;
        }
        assert!(
            Instant::now() < deadline,
            "no measured placement after bootstrap: {}",
            fs::read_to_string(&fixture.log).unwrap()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(observed.memory_capacity_bytes > 0);
    assert!(observed.free_memory_bytes <= observed.memory_capacity_bytes);
    assert_eq!(observed.disk_capacity_bytes, 1 << 30);
    assert!(observed.free_disk_bytes < observed.disk_capacity_bytes);
    assert!(observed.active_cells <= observed.max_active_cells);
    assert!(observed.running_jobs <= observed.job_capacity);
    stop(&mut fixture.child, &fixture.log);
}
