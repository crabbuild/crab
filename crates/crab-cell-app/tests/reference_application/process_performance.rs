use super::fleet::{GatewayStats, start_balancer, start_gateway_peer_server, start_peer_server};
use super::performance::run_reference_primitive_performance;
use super::performance_fixture::{PerfFixture, node_session, owner_routes, perf_cells};
use super::*;
use std::{
    env,
    net::SocketAddr,
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

use crab_cell_runtime::{PeerSigner, PeerVerifier};
use tokio::net::TcpListener;

const ROLE_ENV: &str = "CRAB_CELL_PERF_PROCESS_NODE";
const ROOT_ENV: &str = "CRAB_CELL_PERF_PROCESS_ROOT";
const SYNC_ENV: &str = "CRAB_CELL_PERF_PROCESS_SYNC";
const GATEWAY_ENV: &str = "CRAB_CELL_PERF_PROCESS_GATEWAY";

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn shared_store(root: &Path) -> Store {
    Store::new(Arc::new(
        process_store::FilesystemCasStore::new(root).unwrap(),
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "child role for manual three-process performance run"]
async fn fleet_process_role() {
    let Ok(node) = env::var(ROLE_ENV) else {
        return;
    };
    let node: usize = node.parse().unwrap();
    assert!(node < 3);
    let root = env::var(ROOT_ENV).unwrap();
    let sync = env::var(SYNC_ENV).unwrap();
    let store = shared_store(Path::new(&root));
    let application = Arc::new(compiled());
    let registry = application.registry();
    let tenant = TenantId::from_bytes([81; 16]);
    let application_id = ApplicationId::from_bytes([82; 16]);
    let layout = CellStorageLayout::new(
        store,
        object_store::path::Path::from("reference-performance"),
        *application_id.as_bytes(),
    );
    let directory = tempfile::Builder::new()
        .prefix(&format!("node-{node}-"))
        .tempdir_in(&sync)
        .unwrap();
    let runtime = CellRuntime::new_with_replica_host(
        SqlWorkerPool::new(4, 32).unwrap(),
        64 * 1024 * 1024,
        node_session(node),
        reference_host(),
    )
    .unwrap();
    let mut handles = Vec::new();
    for (index, (namespace, role, module, incarnation, schema)) in
        perf_cells().into_iter().enumerate()
    {
        if index % 3 != node {
            continue;
        }
        handles.push(
            bootstrap_reference_cell(
                &runtime,
                &registry,
                &layout,
                &directory,
                tenant,
                application_id,
                node_session(node),
                namespace,
                role,
                module,
                incarnation,
                schema,
            )
            .await
            .unwrap(),
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
    let marker = Path::new(&sync).join(format!("node-{node}.ready"));
    let gateway = env::var_os(GATEWAY_ENV).is_some();
    let stats = Arc::new(GatewayStats::default());
    let server = if gateway {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        publish_address(&marker, listener.local_addr().unwrap());
        let mut owners = Vec::new();
        for owner in 0..3 {
            let ready = Path::new(&sync).join(format!("node-{owner}.ready"));
            let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
            while !ready.exists() {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "fleet routing timed out"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            owners.push(std::fs::read_to_string(ready).unwrap().parse().unwrap());
        }
        let routes = owner_routes(tenant, application_id, [owners[0], owners[1], owners[2]]);
        let server = start_gateway_peer_server(
            listener,
            &registry,
            verifier,
            handles,
            routes,
            Arc::clone(&stats),
        );
        std::fs::write(Path::new(&sync).join(format!("node-{node}.serving")), []).unwrap();
        server
    } else {
        let (address, server) = start_peer_server(&registry, verifier, handles).await;
        publish_address(&marker, address);
        server
    };
    let stop = Path::new(&sync).join("stop");
    while !stop.exists() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    server.abort();
    runtime.shutdown().await.unwrap();
    if gateway {
        let (local, forwarded) = stats.counts();
        std::fs::write(
            Path::new(&sync).join(format!("node-{node}.counts")),
            format!("{local} {forwarded}"),
        )
        .unwrap();
    }
}

fn publish_address(marker: &Path, address: SocketAddr) {
    let temporary = marker.with_extension("tmp");
    std::fs::write(&temporary, address.to_string()).unwrap();
    std::fs::rename(temporary, marker).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "manual three-process end-to-end performance run"]
async fn reference_three_process_fleet_end_to_end_performance() {
    run_three_process_fleet(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "manual load-balanced three-process end-to-end performance run"]
async fn reference_balanced_three_process_fleet_end_to_end_performance() {
    run_three_process_fleet(true).await;
}

async fn run_three_process_fleet(balanced: bool) {
    let directory = tempfile::TempDir::new().unwrap();
    let objects = directory.path().join("objects");
    std::fs::create_dir_all(&objects).unwrap();
    let store = shared_store(&objects);
    let binary = env::current_exe().unwrap();
    let mut children = Vec::new();
    let mut owners = Vec::new();
    for node in 0..3 {
        let mut command = Command::new(&binary);
        command
            .args([
                "--exact",
                "process_performance::fleet_process_role",
                "--ignored",
                "--nocapture",
            ])
            .env(ROLE_ENV, node.to_string())
            .env(ROOT_ENV, &objects)
            .env(SYNC_ENV, directory.path())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        if balanced {
            command.env(GATEWAY_ENV, "1");
        }
        let child = command.spawn().unwrap();
        children.push(ChildGuard(child));
        let marker = directory.path().join(format!("node-{node}.ready"));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            if marker.exists() {
                owners.push(
                    std::fs::read_to_string(&marker)
                        .unwrap()
                        .parse::<SocketAddr>()
                        .unwrap(),
                );
                break;
            }
            assert!(
                children[node].0.try_wait().unwrap().is_none(),
                "fleet node {node} exited before readiness"
            );
            assert!(
                tokio::time::Instant::now() < deadline,
                "fleet node {node} timed out during bootstrap"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    if balanced {
        for node in 0..3 {
            let serving = directory.path().join(format!("node-{node}.serving"));
            let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
            while !serving.exists() {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "fleet gateway timed out"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    let stop = directory.path().join("stop");
    let sync_dir = directory.path().to_path_buf();
    let balancer = if balanced {
        Some(start_balancer([owners[0], owners[1], owners[2]]).await)
    } else {
        None
    };
    let fixture = PerfFixture::from_processes(
        directory,
        store,
        [owners[0], owners[1], owners[2]],
        balancer.as_ref().map(|(address, _, _)| *address),
    );
    let label = if balanced {
        "fleet_balanced_three_process_mixed"
    } else {
        "fleet_three_process_mixed"
    };
    run_reference_primitive_performance(&fixture, true, label).await;
    if let Some((_, server, stats)) = balancer {
        server.abort();
        let counts = stats.counts();
        println!(
            "PERF balancer_entries: node_0={} node_1={} node_2={}",
            counts[0], counts[1], counts[2]
        );
        assert!(counts.into_iter().all(|count| count > 0));
    }
    std::fs::write(stop, []).unwrap();
    for (node, child) in children.iter_mut().enumerate() {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "fleet node {node} shutdown failed: {status}"
                );
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "fleet node {node} shutdown timed out"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    if balanced {
        for node in 0..3 {
            let counts =
                std::fs::read_to_string(sync_dir.join(format!("node-{node}.counts"))).unwrap();
            let (local, forwarded) = counts.trim().split_once(' ').unwrap();
            let local: usize = local.parse().unwrap();
            let forwarded: usize = forwarded.parse().unwrap();
            println!("PERF gateway_node_{node}: local={local} forwarded={forwarded}");
            assert!(
                local > 0 && forwarded > 0,
                "node {node} missed an ingress path"
            );
        }
    }
    drop(children);
    drop(fixture);
}
