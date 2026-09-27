use super::fleet::{GatewayStats, start_balancer, start_gateway_peer_server, start_peer_server};
use super::performance::run_reference_primitive_performance;
use super::performance_fixture::{
    PerfFixture, node_session, owner_routes, perf_cells, rustfs_store,
};
use crate::*;
use std::{
    env,
    net::SocketAddr,
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

use crab_cell_runtime::peer::{PeerReplicaResolver, PeerSigner, PeerVerifier};
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "child role for manual three-process performance run"]
async fn fleet_process_role() {
    let node = env::var(ROLE_ENV).unwrap();
    let node: usize = node.parse().unwrap();
    assert!(node < 3);
    let root = env::var(ROOT_ENV).unwrap();
    let sync = env::var(SYNC_ENV).unwrap();
    let store = rustfs_store();
    let application = Arc::new(compiled());
    let registry = application.registry();
    let tenant = TenantId::from_bytes([81; 16]);
    let application_id = ApplicationId::from_bytes([82; 16]);
    let layout = CellStorageLayout::new(
        store,
        object_store::path::Path::from(root),
        *application_id.as_bytes(),
    );
    // Only control markers are shared; SQLite, WAL and cache files belong to
    // this process/container and cannot be used by another owner.
    let directory = tempfile::TempDir::new().unwrap();
    let started = std::time::Instant::now();
    let (host, durability, readers) =
        super::process_node::start(node, Arc::clone(&application), &layout, directory.path()).await;
    let runtime = host.runtime();
    let reader = Arc::new(readers);
    let read_target = CellTarget::new(
        tenant,
        application_id,
        SQL_NAMESPACE,
        &partition_for_shard(0),
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
        let bind = env::var("CRAB_CELL_PERF_PROCESS_BIND").unwrap_or_else(|_| "127.0.0.1:0".into());
        let listener = TcpListener::bind(&bind).await.unwrap();
        let advertised = match env::var("CRAB_CELL_PERF_PROCESS_ADVERTISE") {
            Ok(endpoint) => tokio::net::lookup_host(endpoint)
                .await
                .unwrap()
                .next()
                .unwrap(),
            Err(_) => listener.local_addr().unwrap(),
        };
        publish_address(&marker, advertised);
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
            Some(reader.clone()),
        );
        std::fs::write(Path::new(&sync).join(format!("node-{node}.serving")), []).unwrap();
        server
    } else {
        let (address, server) =
            start_peer_server(&registry, verifier, handles, Some(reader.clone())).await;
        publish_address(&marker, address);
        server
    };
    let stop = Path::new(&sync).join("stop");
    let activate = Path::new(&sync).join("readers.activate");
    let activated = Path::new(&sync).join(format!("node-{node}-readers.ready"));
    let evict = Path::new(&sync).join("readers.evicted");
    let evicted = Path::new(&sync).join(format!("node-{node}-readers.evicted"));
    let minimum = Path::new(&sync).join("readers.minimum");
    let refreshed = Path::new(&sync).join(format!("node-{node}-readers.refreshed"));
    while !stop.exists() {
        if activate.exists() && !activated.exists() {
            if node != 0 {
                assert!(matches!(
                    reader
                        .activate(read_target.clone(), node_session(node))
                        .await,
                    Err(Error::Fenced)
                ));
                reader
                    .activate(read_target.clone(), node_session(0))
                    .await
                    .unwrap();
            }
            std::fs::write(&activated, []).unwrap();
        }
        if node != 0 && minimum.exists() && !refreshed.exists() {
            let sequence: u64 = std::fs::read_to_string(&minimum).unwrap().parse().unwrap();
            let (receipt, ready) = reader.status(read_target.clone()).await.unwrap();
            if ready && receipt.commit_sequence >= sequence {
                // This only observes the supervisor; no refresh hint is sent.
                std::fs::write(&refreshed, []).unwrap();
            }
        }
        if evict.exists()
            && !evicted.exists()
            && matches!(
                reader.resolve(read_target.clone()).await,
                Err(Error::ReplicaUnavailable)
            )
        {
            // Observe the host supervisor's eviction; the fixture does not remove the view.
            std::fs::write(&evicted, []).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    server.abort();
    println!(
        "PERF node_{node}: active_cells={} retained_bytes={} local_disk_reserved_bytes={}",
        host.stats().active_cells(),
        host.stats().retained_bytes(),
        host.stats().local_disk_reserved_bytes()
    );
    host.shutdown().await.unwrap();
    assert!(matches!(
        reader.activate(read_target.clone(), node_session(0)).await,
        Err(Error::RuntimeClosed)
    ));
    assert!(matches!(
        reader.resolve(read_target).await,
        Err(Error::RuntimeClosed)
    ));
    println!("PERF node_{node}_reader_drained: activation_closed=1 resolver_closed=1");
    let mut waits = durability.object_waits();
    assert!(
        !waits.is_empty(),
        "node {node} did not prove an object-backed mutation"
    );
    super::performance::report_samples(
        &format!("node_{node}_object_proof_wait"),
        &mut waits,
        started.elapsed(),
    );
    if gateway {
        let (local, forwarded) = stats.counts();
        std::fs::write(
            Path::new(&sync).join(format!("node-{node}.counts")),
            format!("{local} {forwarded}"),
        )
        .unwrap();
    }
    std::fs::write(Path::new(&sync).join(format!("node-{node}.done")), []).unwrap();
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
    let root = format!(
        "{}/process-{}-{}",
        env::var("CRAB_CELL_TEST_PREFIX").unwrap(),
        std::process::id(),
        directory.path().file_name().unwrap().to_str().unwrap()
    );
    let store = rustfs_store();
    println!("PERF backend=rustfs object_prefix={root}");
    let binary = env::current_exe().unwrap();
    // Libtest omits the crate name; preserve the remaining module path when
    // this suite moves so a child cannot silently select zero tests.
    let module = module_path!().split_once("::").unwrap().1;
    let child_test = format!("{module}::fleet_process_role");
    let mut children = Vec::new();
    let mut owners = Vec::new();
    for node in 0..3 {
        let mut command = Command::new(&binary);
        command
            .args(["--exact", &child_test, "--ignored", "--nocapture"])
            .env(ROLE_ENV, node.to_string())
            .env(ROOT_ENV, &root)
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
    generated_action(&fixture).await;
    super::process_replica::verify(
        &fixture,
        &sync_dir,
        &root,
        [owners[0], owners[1], owners[2]],
    )
    .await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "Compose driver for three constrained public CellNode processes and RustFS"]
async fn reference_compose_fleet_end_to_end_performance() {
    let sync = env::var(SYNC_ENV).unwrap();
    let sync = Path::new(&sync);
    let mut owners = Vec::new();
    for node in 0..3 {
        wait_for_marker(&sync.join(format!("node-{node}.serving"))).await;
        owners.push(
            std::fs::read_to_string(sync.join(format!("node-{node}.ready")))
                .unwrap()
                .parse()
                .unwrap(),
        );
    }
    let owners = [owners[0], owners[1], owners[2]];
    let (balancer, server, stats) = start_balancer(owners).await;
    let fixture = PerfFixture::from_processes(
        tempfile::TempDir::new().unwrap(),
        rustfs_store(),
        owners,
        Some(balancer),
    );
    run_reference_primitive_performance(&fixture, true, "compose_three_node_mixed").await;
    generated_action(&fixture).await;
    super::process_replica::verify(&fixture, sync, &env::var(ROOT_ENV).unwrap(), owners).await;
    server.abort();
    let counts = stats.counts();
    assert!(counts.iter().all(|count| *count > 0));
    assert!(counts.iter().max().unwrap() - counts.iter().min().unwrap() <= 1);
    println!("PERF balancer_entries={counts:?}");
    std::fs::write(sync.join("stop"), []).unwrap();
    for node in 0..3 {
        wait_for_marker(&sync.join(format!("node-{node}.done"))).await;
        let counts = std::fs::read_to_string(sync.join(format!("node-{node}.counts"))).unwrap();
        let (local, forwarded) = counts.trim().split_once(' ').unwrap();
        assert!(local.parse::<usize>().unwrap() > 0 && forwarded.parse::<usize>().unwrap() > 0);
        println!("PERF gateway_node_{node}: local={local} forwarded={forwarded}");
    }
}

pub(super) async fn wait_for_marker(marker: &Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    while !marker.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "node marker timed out: {marker:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn generated_action(fixture: &PerfFixture) {
    let client = ReferenceClient::new(fixture.typed.clone()).unwrap();
    let order = client.orders(&OrderId(b"process-proof".to_vec())).unwrap();
    let before = order.receipt_count(None, ()).await.unwrap().output;
    let identity = super::performance_fixture::identity(103, 0, 0);
    let input = CronInvocation {
        schedule_id: [103; 16],
        generation: 1,
        occurrence: 1,
        scheduled_at_ms: super::performance_fixture::now_ms(),
        payload: b"process-proof".to_vec(),
    };
    let prepared = order
        .prepare_receive_cron(identity, input.clone())
        .await
        .unwrap();
    let committed = prepared.execute().await.unwrap();
    let duplicate = order.receive_cron(identity, input).await.unwrap();
    assert_eq!(duplicate.receipt, committed.receipt);
    assert_eq!(
        order
            .receipt_count(Some(committed.receipt), ())
            .await
            .unwrap()
            .output,
        before + 1
    );
    println!("PERF generated_action: committed=1 duplicate_deliveries=1 visible_effects=1");
}
