//! Signed owner recruitment and replacement after a selected process is killed.

use super::performance_fixture::{PerfFixture, identity, node_session, now_ms, rustfs_store};
use super::process_node::{EnrolledReplicaTransport, directory};
use super::process_performance::{ChildGuard, wait_for_marker};
use crate::*;
use crab_cell_runtime::{
    client::{ReadPolicy, Receipt, ReplicaReadRouter},
    peer::{PeerPrincipal, PeerRoundTrip, PeerSigner, ReplicaPeerClient, decode_peer_reply, wire},
    read_policy::ReadPolicyStore,
};
use std::{
    collections::{HashMap, HashSet},
    env,
    net::SocketAddr,
    path::Path,
    process::{Command, Stdio},
    sync::Mutex,
    time::Duration,
};

pub(super) struct ObservedReads(
    pub(super) Arc<Mutex<HashMap<crab_cell_runtime::SessionId, usize>>>,
);

impl PeerRoundTrip for ObservedReads {
    fn send(
        &self,
        _: CellTarget,
        _: Vec<u8>,
        _: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'static>> {
        Box::pin(async {
            Err(Error::Peer(
                "replacement proof requires selected-node routing",
            ))
        })
    }

    fn send_to_node(
        &self,
        target: CellTarget,
        node: NodeAdvertisement,
        request: Vec<u8>,
        remaining_ms: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'static>> {
        let observed = Arc::clone(&self.0);
        Box::pin(async move {
            let session = node.session();
            let reply = EnrolledReplicaTransport
                .send_to_node(target, node, request, remaining_ms)
                .await?;
            if matches!(decode_peer_reply(&reply)?.outcome, Some(wire::peer_reply::Outcome::Read(read))
                if matches!(read.result, Some(wire::read_reply::Result::CommandOutput(_))))
            {
                *observed.lock().unwrap().entry(session).or_default() += 1;
            }
            Ok(reply)
        })
    }
}

async fn spawn(node: usize, root: &str, sync: &Path) -> (ChildGuard, SocketAddr) {
    let child = Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "reference_application::process_performance::fleet_process_role",
            "--ignored",
            "--nocapture",
        ])
        .env("CRAB_CELL_PERF_PROCESS_NODE", node.to_string())
        .env("CRAB_CELL_PERF_PROCESS_ROOT", root)
        .env("CRAB_CELL_PERF_PROCESS_SYNC", sync)
        .env_remove("CRAB_CELL_PERF_PROCESS_GATEWAY")
        .env_remove("CRAB_CELL_PERF_PROCESS_BIND")
        .env_remove("CRAB_CELL_PERF_PROCESS_ADVERTISE")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let child = ChildGuard(child);
    let marker = sync.join(format!("node-{node}.ready"));
    wait_for_marker(&marker).await;
    let address = std::fs::read_to_string(marker).unwrap().parse().unwrap();
    (child, address)
}

pub(super) async fn ready_readers(
    router: &ReplicaReadRouter,
    peer: &ReplicaPeerClient,
    target: &CellTarget,
    minimum: Receipt,
    desired: usize,
    excluded: Option<crab_cell_runtime::SessionId>,
) -> HashSet<crab_cell_runtime::SessionId> {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let (expected, selected) = router.selected(target).await.unwrap();
            let mut ready = HashSet::new();
            for node in selected {
                let session = node.session();
                if Some(session) == excluded {
                    continue;
                }
                if let Ok((receipt, true)) = peer.status(target, node, expected).await
                    && receipt.commit_sequence >= minimum.commit_sequence
                {
                    ready.insert(session);
                }
            }
            if ready.len() == desired {
                return ready;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("owner did not automatically recruit the desired current readers")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live RustFS three-to-five-process recruitment and abrupt reader loss"]
async fn owner_replaces_killed_reader_through_public_hosts() {
    let sync = tempfile::TempDir::new().unwrap();
    let root = format!(
        "{}/reader-replacement-{}-{}",
        env::var("CRAB_CELL_TEST_PREFIX").unwrap(),
        std::process::id(),
        sync.path().file_name().unwrap().to_str().unwrap()
    );
    let mut children = Vec::new();
    let mut endpoints = Vec::new();
    for node in 0..3 {
        let (child, address) = spawn(node, &root, sync.path()).await;
        children.push(child);
        endpoints.push(address);
    }
    let fixture = PerfFixture::from_processes(
        tempfile::TempDir::new().unwrap(),
        rustfs_store(),
        [endpoints[0], endpoints[1], endpoints[2]],
        None,
    );
    super::performance::run_reference_primitive_performance(
        &fixture,
        true,
        "reader_recruitment_initial",
    )
    .await;
    let target = &fixture.sql_target;
    let layout = CellStorageLayout::new(
        rustfs_store(),
        object_store::path::Path::from(root.clone()),
        *target.application().as_bytes(),
    );
    let authority = CellAuthority::new(layout.clone());
    let before = authority.load(target.cell_id()).await.unwrap().unwrap();
    let owner = ReferenceClient::new(fixture.typed.clone()).unwrap();
    let order = owner
        .orders(&OrderId(b"reader-replacement".to_vec()))
        .unwrap();
    let written = order
        .receive_cron(
            identity(105, 0, 0),
            CronInvocation {
                schedule_id: [105; 16],
                generation: 1,
                occurrence: 1,
                scheduled_at_ms: now_ms(),
                payload: b"survive-reader-loss".to_vec(),
            },
        )
        .await
        .unwrap();
    let expected_output = order
        .receipt_count(Some(written.receipt), ())
        .await
        .unwrap();
    let observed = Arc::new(Mutex::new(HashMap::new()));
    let peer = ReplicaPeerClient::new(
        Arc::clone(&fixture.registry),
        Arc::new(PeerSigner::new(
            crab_cell_runtime::SessionId::from_bytes([77; 16]),
            fixture.registry.release_digest(),
            SigningKey::from_bytes(&[78; 32]),
        )),
        PeerPrincipal {
            issuer: "reference-performance".into(),
            subject: "reader-replacement".into(),
            actions: vec!["cell.read".into(), "cell.replica.status".into()],
        },
        Arc::new(ObservedReads(Arc::clone(&observed))),
    );
    let router = ReplicaReadRouter::new(authority.clone(), directory(&layout, &fixture.registry));
    let client = fixture
        .client
        .with_read_replicas(router.clone(), peer.clone(), None)
        .unwrap();
    let handle = ApplicationHandle::new(
        client,
        Arc::new(compiled()),
        target.tenant(),
        target.application(),
    )
    .unwrap();
    let generated = ReferenceClient::new(handle.with_read_policy(ReadPolicy::Replica)).unwrap();
    let reader = generated
        .orders(&OrderId(b"reader-replacement".to_vec()))
        .unwrap();
    ReadPolicyStore::new(layout.clone())
        .create(target.cell_id(), before.value().incarnation, 2)
        .await
        .unwrap();
    let initial = ready_readers(&router, &peer, target, written.receipt, 2, None).await;
    assert_eq!(initial, HashSet::from([node_session(1), node_session(2)]));
    let (expected, selected) = router.selected(target).await.unwrap();
    assert!(
        matches!(
            peer.activate(
                target,
                &directory(&layout, &fixture.registry),
                selected[0].clone(),
                expected,
            )
            .await,
            Err(Error::PeerAuthorization(_))
        ),
        "read/status capability admitted an activation"
    );

    for _ in 0..8 {
        assert_eq!(
            reader
                .receipt_count(Some(written.receipt), ())
                .await
                .unwrap(),
            expected_output
        );
    }
    assert_eq!(
        observed
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect::<HashSet<_>>(),
        initial
    );

    for node in 3..5 {
        let (child, _) = spawn(node, &root, sync.path()).await;
        children.push(child);
    }
    assert_eq!(
        directory(&layout, &fixture.registry)
            .live(now_ms(), 10)
            .await
            .unwrap()
            .len(),
        5
    );
    let selected = ready_readers(&router, &peer, target, written.receipt, 2, None).await;
    let lost = *selected
        .iter()
        .min_by_key(|session| *session.as_bytes())
        .unwrap();
    let lost_node = (1..5).find(|node| node_session(*node) == lost).unwrap();
    children[lost_node].0.kill().unwrap();
    let exit = children[lost_node].0.wait().unwrap();
    assert!(!exit.success(), "reader fault did not kill the process");
    let started = std::time::Instant::now();
    let replacement = ready_readers(&router, &peer, target, written.receipt, 2, Some(lost)).await;
    let recovery_ms = started.elapsed().as_millis();
    assert!(
        replacement
            .iter()
            .any(|session| !selected.contains(session))
    );
    observed.lock().unwrap().clear();
    for _ in 0..12 {
        assert_eq!(
            reader
                .receipt_count(Some(written.receipt), ())
                .await
                .unwrap(),
            expected_output
        );
    }
    assert_eq!(
        observed
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect::<HashSet<_>>(),
        replacement
    );
    let after = authority.load(target.cell_id()).await.unwrap().unwrap();
    assert_eq!(after.value().owner, before.value().owner);
    assert_eq!(after.value().epoch, before.value().epoch);
    assert_eq!(after.value().incarnation, written.receipt.incarnation);
    println!(
        "PERF reader_replacement: initial_nodes=3 expanded_nodes=5 killed_node={lost_node} ready_readers=2 owner_unchanged=1 exact_queries=12 recovery_ms={}",
        recovery_ms
    );
    std::fs::write(sync.path().join("stop"), []).unwrap();
    for (node, child) in children.iter_mut().enumerate() {
        if node == lost_node {
            continue;
        }
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if let Some(status) = child.0.try_wait().unwrap() {
                    assert!(status.success(), "node {node} failed drain: {status}");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("surviving node did not finish drain");
        assert!(sync.path().join(format!("node-{node}.done")).exists());
    }
}
