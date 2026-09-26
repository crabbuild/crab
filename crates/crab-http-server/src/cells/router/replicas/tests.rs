use super::*;
use crab_cell_runtime::{
    Digest, SessionId,
    cell::worker::SqlWorkerPool,
    identity::{ApplicationId, IncarnationId, TenantId},
    node::{NodeCapacity, NodeFailureDomain},
    peer::PeerVerifier,
};
use crab_storage::Store;
use ed25519_dalek::SigningKey;
use object_store::{ObjectStoreExt, memory::InMemory, path::Path as ObjectPath};
use std::{future::Future, pin::Pin, sync::Mutex as StdMutex};

#[derive(Default)]
struct ActivationProbe {
    stalled: StdMutex<HashSet<SessionId>>,
    received: StdMutex<Vec<(crab_cell_runtime::CellId, SessionId)>>,
}

impl PeerRoundTrip for ActivationProbe {
    fn send(
        &self,
        _: CellTarget,
        _: Vec<u8>,
        _: u32,
    ) -> Pin<Box<dyn Future<Output = crab_cell_runtime::Result<Vec<u8>>> + Send + 'static>> {
        Box::pin(async { Err(crab_cell_runtime::Error::CellNotActive) })
    }

    fn send_to_node(
        &self,
        target: CellTarget,
        node: NodeAdvertisement,
        request: Vec<u8>,
        _: u32,
    ) -> Pin<Box<dyn Future<Output = crab_cell_runtime::Result<Vec<u8>>> + Send + 'static>> {
        let now = crate::cells::unix_now_ms().unwrap();
        assert!(
            node.expires_at_ms() > now,
            "activation used expired discovery"
        );
        PeerVerifier::new(
            SessionId::from_bytes([1; 16]),
            node.release(),
            SigningKey::from_bytes(&[7; 32]).verifying_key(),
        )
        .verify(&request, now)
        .unwrap();
        let stalled = self.stalled.lock().unwrap().contains(&node.session());
        self.received
            .lock()
            .unwrap()
            .push((target.cell_id(), node.session()));
        Box::pin(async move {
            if stalled {
                return std::future::pending().await;
            }
            crab_cell_runtime::peer::encode_peer_reply(&peer_wire::PeerReply {
                outcome: Some(peer_wire::peer_reply::Outcome::Read(peer_wire::ReadReply {
                    receipt: Some(peer_wire::Receipt {
                        cell_id: target.cell_id().as_bytes().to_vec(),
                        incarnation: vec![9; 16],
                        commit_sequence: 1,
                    }),
                    result: Some(peer_wire::read_reply::Result::ReplicaReady(true)),
                })),
            })
        })
    }
}

struct Fixture {
    router: RepositoryCellRouter,
    probe: Arc<ActivationProbe>,
    targets: Vec<CellTarget>,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn new(cells: u8) -> Self {
        let identity = ApplicationIdentity::new(
            TenantId::from_bytes([2; 16]),
            ApplicationId::from_bytes([3; 16]),
        );
        let layout = CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            ObjectPath::from("reader-reconciliation"),
            *identity.application().as_bytes(),
        );
        let registry = Arc::new(crate::cells::compiled_registry().unwrap());
        crate::cells::bootstrap_release_at(
            &layout,
            identity,
            &registry,
            &format!("sha256:{}", "a".repeat(64)),
        )
        .await
        .unwrap();
        let session = SessionId::from_bytes([1; 16]);
        let runtime =
            CellRuntime::new(SqlWorkerPool::new(1, 8).unwrap(), 16 << 20, session).unwrap();
        let directory = tempfile::TempDir::new().unwrap();
        let probe = Arc::new(ActivationProbe::default());
        let router = RepositoryCellRouter::new(
            identity,
            layout.clone(),
            Arc::clone(&registry),
            runtime,
            RepositoryCellPeer::new(
                NodeDirectory::new(
                    layout.clone(),
                    Digest::from_bytes([4; 32]),
                    Digest::from_bytes([5; 32]),
                    registry.release_digest(),
                ),
                Arc::new(PeerSigner::new(
                    session,
                    registry.release_digest(),
                    SigningKey::from_bytes(&[7; 32]),
                )),
                probe.clone(),
                Owner {
                    session,
                    endpoint: "https://node-1.internal:8081".into(),
                },
            ),
            directory.path().to_path_buf(),
        )
        .unwrap();
        let mut targets = Vec::new();
        for cell in 1..=cells {
            let target = router
                .repository_target(Uuid::from_bytes([cell; 16]))
                .unwrap();
            let (proof, authority) =
                crate::cells::provision_repository(&layout, identity, &registry, &target)
                    .await
                    .unwrap();
            let observed = authority
                .create_initial(
                    &proof,
                    IncarnationId::from_bytes([9; 16]),
                    router.peer.owner.clone(),
                )
                .await
                .unwrap();
            router
                .runtime
                .bootstrap(
                    proof,
                    CellReplica::new(
                        layout.clone(),
                        *target.cell_id().as_bytes(),
                        [9; 16],
                        repository_replica_limits(),
                    )
                    .unwrap(),
                    authority,
                    observed,
                    directory.path().join(format!("{cell}.sqlite")),
                    |tx| crate::cells::initialize_repository_schema(tx).map_err(Into::into),
                )
                .await
                .unwrap();
            ReadPolicyStore::new(layout.clone())
                .create(target.cell_id(), IncarnationId::from_bytes([9; 16]), 2)
                .await
                .unwrap();
            targets.push(target);
        }
        targets.sort_by_key(|target| *target.cell_id().as_bytes());
        let fixture = Self {
            router,
            probe,
            targets,
            _directory: directory,
        };
        let now = crate::cells::unix_now_ms().unwrap();
        for node in 1..=3 {
            fixture
                .router
                .peer
                .directory
                .create(fixture.advertisement(node, now), now)
                .await
                .unwrap();
        }
        fixture
    }

    fn advertisement(&self, node: u8, now: i64) -> NodeAdvertisement {
        NodeAdvertisement::sign(
            NodeId::from_bytes([node; 16]),
            SessionId::from_bytes([node; 16]),
            format!("https://node-{node}.internal:8081"),
            Digest::from_bytes([4; 32]),
            Digest::from_bytes([node; 32]),
            Digest::from_bytes([5; 32]),
            self.router.registry.release_digest(),
            &SigningKey::from_bytes(&[7; 32]),
            1,
            now,
            now + 10_000,
            self.router.registry.module_digests(),
            vec![1],
            NodeFailureDomain::default(),
            NodeCapacity {
                free_memory_bytes: 64 << 20,
                free_disk_bytes: 1 << 30,
                job_credits: 8,
                ..NodeCapacity::default()
            },
        )
        .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_cell_policy_does_not_starve_other_cells() {
    let fixture = Fixture::new(3).await;
    let first = fixture.targets[0].cell_id();
    let path = fixture.router.layout.read_policy_path(first.as_bytes());
    fixture
        .router
        .layout
        .store()
        .inner()
        .put(&path, bytes::Bytes::from_static(b"broken").into())
        .await
        .unwrap();
    let result = fixture.router.reconcile_readers_once(&mut 0).await;
    let received = fixture.probe.received.lock().unwrap().clone();
    fixture.router.runtime.shutdown().await.unwrap();
    assert!(
        result.is_ok(),
        "one corrupt policy aborted the batch: {result:?}"
    );
    for target in &fixture.targets[1..] {
        assert_eq!(
            received
                .iter()
                .filter(|(cell, _)| *cell == target.cell_id())
                .count(),
            2
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stalled_reader_does_not_block_healthy_reader_activation() {
    let fixture = Fixture::new(1).await;
    let target = fixture.targets[0].clone();
    let (_, selected) = fixture
        .router
        .replica_routing
        .selected(&target)
        .await
        .unwrap();
    let stalled = selected[0].session();
    let healthy = selected[1].session();
    fixture.probe.stalled.lock().unwrap().insert(stalled);
    let _ = tokio::time::timeout(
        Duration::from_millis(500),
        fixture.router.reconcile_reader_target(target),
    )
    .await;
    let received = fixture.probe.received.lock().unwrap().clone();
    fixture.router.runtime.shutdown().await.unwrap();
    assert!(
        received.iter().any(|(_, session)| *session == healthy),
        "healthy reader waited behind stalled peer"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn activation_reloads_advertisement_after_discovery() {
    let fixture = Fixture::new(1).await;
    let now = crate::cells::unix_now_ms().unwrap();
    let stale = fixture.advertisement(2, now - 20_000);
    fixture
        .router
        .peer
        .activate_read_replica(fixture.targets[0].clone(), stale)
        .await
        .unwrap();
    fixture.router.runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupted_batch_resumes_at_the_next_cell() {
    let fixture = Fixture::new(3).await;
    fixture.probe.stalled.lock().unwrap().extend([
        SessionId::from_bytes([2; 16]),
        SessionId::from_bytes([3; 16]),
    ]);
    let mut cursor = 0;
    let interrupted = tokio::time::timeout(
        Duration::from_millis(200),
        fixture.router.reconcile_readers_once(&mut cursor),
    )
    .await;
    assert!(interrupted.is_err());
    fixture.probe.stalled.lock().unwrap().clear();
    fixture.probe.received.lock().unwrap().clear();
    fixture
        .router
        .reconcile_readers_once(&mut cursor)
        .await
        .unwrap();
    let first = fixture.probe.received.lock().unwrap()[0].0;
    fixture.router.runtime.shutdown().await.unwrap();
    assert_eq!(first, fixture.targets[1].cell_id());
}
