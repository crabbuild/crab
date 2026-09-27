//! Private-disk entity owners for the constrained Compose workload.

use super::super::fleet::{GatewayStats, start_gateway_peer_server};
use super::super::performance_fixture::{node_session, now_ms, rustfs_store};
use super::super::process_node;
use super::super::process_performance::{publish_address, wait_for_marker};
use super::*;
use crab_cell_runtime::peer::PeerVerifier;
use std::{
    env,
    net::SocketAddr,
    path::Path,
    sync::RwLock,
    time::{Duration, Instant},
};
use tokio::net::TcpListener;

mod driver;
mod observation;

async fn endpoints(sync: &Path, count: usize) -> Vec<SocketAddr> {
    let mut endpoints = Vec::new();
    for node in 0..count {
        let ready = sync.join(format!("node-{node}.ready"));
        wait_for_marker(&ready).await;
        endpoints.push(std::fs::read_to_string(ready).unwrap().parse().unwrap());
    }
    endpoints
}

fn publish_marker(path: &Path, value: impl AsRef<[u8]>) {
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, value).unwrap();
    std::fs::rename(temporary, path).unwrap();
}

fn boot_ms() -> u64 {
    // Linux exposes the shared boot clock with centisecond precision. Wall
    // time can step during a window and cannot align resource observations.
    let uptime = std::fs::read_to_string("/proc/uptime").unwrap();
    let (seconds, centiseconds) = uptime
        .split_whitespace()
        .next()
        .unwrap()
        .split_once('.')
        .unwrap();
    assert_eq!(centiseconds.len(), 2);
    seconds.parse::<u64>().unwrap() * 1_000 + centiseconds.parse::<u64>().unwrap() * 10
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "Compose entity node: Linux cgroups, private disk and real RustFS required"]
async fn entity_process_node() {
    tracing_subscriber::fmt()
        .with_max_level(tracing_subscriber::filter::LevelFilter::WARN)
        .with_ansi(false)
        .try_init()
        .unwrap();
    let node: usize = env::var("CRAB_CELL_PERF_PROCESS_NODE")
        .unwrap()
        .parse()
        .unwrap();
    assert!(node < 20);
    let sync = env::var("CRAB_CELL_PERF_PROCESS_SYNC").unwrap();
    let sync = Path::new(&sync);
    let application = compiled_entities();
    let registry = application.registry();
    let storage = Arc::new(observation::StorageCounters::default());
    let store = rustfs_store().with_storage_observer(storage.clone());
    let layout = CellStorageLayout::new(
        store,
        env::var("CRAB_CELL_PERF_PROCESS_ROOT").unwrap().into(),
        *ApplicationId::from_bytes([82; 16]).as_bytes(),
    );
    let directory = tempfile::TempDir::new().unwrap();
    let listener = TcpListener::bind(env::var("CRAB_CELL_PERF_PROCESS_BIND").unwrap())
        .await
        .unwrap();
    let address = tokio::net::lookup_host(env::var("CRAB_CELL_PERF_PROCESS_ADVERTISE").unwrap())
        .await
        .unwrap()
        .next()
        .unwrap();
    let endpoint = format!("https://{address}");
    let (host, durability, readers) = process_node::start(
        node,
        application.clone(),
        &layout,
        directory.path(),
        endpoint.clone(),
    )
    .await;
    let mut handles = Vec::new();
    for entity in node * ENTITIES_PER_NODE..(node + 1) * ENTITIES_PER_NODE {
        handles.push(
            provision_entity(
                &host,
                &layout,
                directory.path(),
                node,
                entity,
                endpoint.clone(),
            )
            .await,
        );
    }
    let signer = PeerSigner::new(
        crab_cell_runtime::SessionId::from_bytes([77; 16]),
        registry.release_digest(),
        SigningKey::from_bytes(&[78; 32]),
    );
    let verifier = Arc::new(PeerVerifier::new(
        crab_cell_runtime::SessionId::from_bytes([77; 16]),
        registry.release_digest(),
        signer.verifying_key(),
    ));
    let routes = Arc::new(RwLock::new(HashMap::new()));
    let stats = Arc::new(GatewayStats::default());
    let server = start_gateway_peer_server(
        listener,
        &registry,
        verifier,
        handles,
        routes.clone(),
        stats.clone(),
        Some((
            Arc::new(readers),
            process_node::directory(&layout, &registry),
        )),
    );
    publish_address(&sync.join(format!("node-{node}.ready")), address);
    publish_marker(&sync.join(format!("node-{node}.serving")), []);
    let mut observations = observation::NodeObservations::new(sync, node);
    let mut next_sample = Instant::now();
    let mut stage = 0;
    while !sync.join("stop").exists() {
        let requested = sync.join("entity-stage.request");
        if requested.exists() {
            let count: usize = std::fs::read_to_string(requested).unwrap().parse().unwrap();
            if count > stage && node < count {
                assert!([3, 5, 10, 20].contains(&count));
                let addresses = endpoints(sync, count).await;
                let expanded = (0..count * ENTITIES_PER_NODE)
                    .map(|entity| {
                        (
                            entity_target(&application, entity).cell_id(),
                            addresses[entity / ENTITIES_PER_NODE],
                        )
                    })
                    .collect();
                // Publish one complete routing table before admitting the next
                // stage, so old gateways can reach newly provisioned owners.
                *routes.write().unwrap() = expanded;
                stage = count;
                publish_marker(&sync.join(format!("node-{node}-stage-{stage}.ready")), []);
            }
        }
        if Instant::now() >= next_sample {
            observations.sample(stage, &host, directory.path(), &storage, &stats);
            next_sample = Instant::now() + Duration::from_secs(1);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    observations.sample(stage, &host, directory.path(), &storage, &stats);
    assert_eq!(host.stats().active_cells(), ENTITIES_PER_NODE);
    host.shutdown().await.unwrap();
    observations.finish(&storage, &durability.object_waits());
    let (local, forwarded) = stats.counts();
    assert!(local > 0 && forwarded > 0);
    publish_marker(
        &sync.join(format!("node-{node}.counts")),
        format!("{local} {forwarded}"),
    );
    publish_marker(&sync.join(format!("node-{node}.done")), []);
    server.abort();
    let _ = server.await;
}
