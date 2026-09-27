//! Generated snapshot queries across independent public host processes.

use super::performance_fixture::{PerfFixture, identity, node_session, now_ms, rustfs_store};
use super::process_node;
use crate::*;
use crab_cell_runtime::client::{CellReadReplica, ReadPolicy, ReplicaReadRouter};
use crab_cell_runtime::peer::{
    PeerPrincipal, PeerReplicaResolver, PeerRoundTrip, PeerSigner, ReplicaPeerClient,
    decode_peer_reply, wire,
};
use crab_cell_runtime::read_policy::ReadPolicyStore;
use std::{
    net::SocketAddr,
    path::Path,
    sync::{
        RwLock,
        atomic::{AtomicUsize, Ordering},
    },
};

pub(super) struct Reader {
    target: CellTarget,
    view: RwLock<Option<CellReadReplica>>,
}

impl Reader {
    pub(super) fn new(target: CellTarget) -> Self {
        Self {
            target,
            view: RwLock::new(None),
        }
    }

    pub(super) async fn refresh(
        &self,
        runtime: &CellRuntime,
        registry: &Arc<Registry>,
        layout: &CellStorageLayout,
        destination: &Path,
    ) {
        let current = self.view.read().unwrap().clone();
        if let Some(view) = current {
            view.refresh(destination).await.unwrap();
            return;
        }
        let authority = CellAuthority::new(layout.clone());
        let control = authority
            .load(self.target.cell_id())
            .await
            .unwrap()
            .unwrap();
        let replica = CellReplica::new(
            layout.clone(),
            *self.target.cell_id().as_bytes(),
            *control.value().incarnation.as_bytes(),
            Limits::default(),
        )
        .unwrap();
        let view = CellReadReplica::open(
            runtime.clone(),
            Arc::clone(registry),
            authority,
            process_node::directory(layout, registry),
            replica,
            self.target.clone(),
            destination,
        )
        .await
        .unwrap();
        *self.view.write().unwrap() = Some(view);
    }

    pub(super) fn close(&self) {
        if let Some(view) = self.view.write().unwrap().take() {
            view.close();
        }
    }
}

impl PeerReplicaResolver for Reader {
    fn resolve(
        &self,
        target: CellTarget,
    ) -> Pin<Box<dyn Future<Output = Result<CellReadReplica>> + Send + 'static>> {
        // Snapshot installation is controlled by the node loop, so a lagging
        // read cannot silently refresh itself or activate the writable owner.
        let reader = self.view.read().unwrap().clone();
        let matches = target == self.target;
        Box::pin(async move { reader.filter(|_| matches).ok_or(Error::ReplicaUnavailable) })
    }
}

struct ReaderTransport {
    nodes: [SocketAddr; 3],
    successes: Arc<[AtomicUsize; 3]>,
}

impl PeerRoundTrip for ReaderTransport {
    fn send(
        &self,
        _target: CellTarget,
        _request: Vec<u8>,
        _remaining_ms: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'static>> {
        Box::pin(async { Err(Error::Peer("replica proof requires a selected node")) })
    }

    fn send_to_node(
        &self,
        _target: CellTarget,
        node: NodeAdvertisement,
        request: Vec<u8>,
        remaining_ms: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'static>> {
        let index = (1..3).find(|index| node.session() == node_session(*index));
        let nodes = self.nodes;
        let successes = Arc::clone(&self.successes);
        Box::pin(async move {
            let index = index.ok_or(Error::Peer(
                "replica routing selected the owner or an unknown node",
            ))?;
            assert_eq!(node.node().as_bytes(), node_session(index).as_bytes());
            let reply = super::fleet::send_tcp(nodes[index], request, remaining_ms).await?;
            if matches!(decode_peer_reply(&reply)?.outcome, Some(wire::peer_reply::Outcome::Read(read))
                if matches!(read.result, Some(wire::read_reply::Result::CommandOutput(_))))
            {
                successes[index].fetch_add(1, Ordering::Relaxed);
            }
            Ok(reply)
        })
    }
}

async fn refresh(sync: &Path, round: usize) {
    std::fs::write(sync.join(format!("readers-{round}.refresh")), []).unwrap();
    for node in 0..3 {
        super::process_performance::wait_for_marker(
            &sync.join(format!("node-{node}-readers-{round}.ready")),
        )
        .await;
    }
}

pub(super) async fn verify(fixture: &PerfFixture, sync: &Path, root: &str, nodes: [SocketAddr; 3]) {
    let target = &fixture.sql_target;
    let layout = CellStorageLayout::new(
        rustfs_store(),
        object_store::path::Path::from(root),
        *target.application().as_bytes(),
    );
    let authority = CellAuthority::new(layout.clone());
    let control = authority.load(target.cell_id()).await.unwrap().unwrap();
    assert_eq!(
        control.value().owner.as_ref().unwrap().session,
        node_session(0)
    );
    ReadPolicyStore::new(layout.clone())
        .create(target.cell_id(), control.value().incarnation, 2)
        .await
        .unwrap();
    let successes = Arc::new(std::array::from_fn(|_| AtomicUsize::new(0)));
    let transport = Arc::new(ReaderTransport {
        nodes,
        successes: Arc::clone(&successes),
    });
    let peer = ReplicaPeerClient::new(
        Arc::clone(&fixture.registry),
        Arc::new(PeerSigner::new(
            crab_cell_runtime::SessionId::from_bytes([77; 16]),
            fixture.registry.release_digest(),
            SigningKey::from_bytes(&[78; 32]),
        )),
        PeerPrincipal {
            issuer: "reference-performance".into(),
            subject: "replica-driver".into(),
            actions: vec!["cell.read".into()],
        },
        transport,
    );
    let client = fixture
        .client
        .with_read_replicas(
            ReplicaReadRouter::new(
                authority,
                process_node::directory(&layout, &fixture.registry),
            ),
            peer,
            None,
        )
        .unwrap();
    let typed = ApplicationHandle::new(
        client,
        Arc::new(compiled()),
        target.tenant(),
        target.application(),
    )
    .unwrap();
    let generated = ReferenceClient::new(typed.with_read_policy(ReadPolicy::Replica)).unwrap();
    let order = generated
        .orders(&OrderId(b"process-replica-proof".to_vec()))
        .unwrap();
    assert!(matches!(
        order.receipt_count(None, ()).await,
        Err(InvocationError::NotStarted(Error::ReplicaUnavailable))
    ));
    refresh(sync, 0).await;
    let before = order.receipt_count(None, ()).await.unwrap();
    for _ in 0..5 {
        assert_eq!(
            order.receipt_count(Some(before.receipt), ()).await.unwrap(),
            before
        );
    }
    let identity = identity(104, 0, 0);
    let input = CronInvocation {
        schedule_id: [104; 16],
        generation: 1,
        occurrence: 1,
        scheduled_at_ms: now_ms(),
        payload: b"replica-process-proof".to_vec(),
    };
    let committed = order.receive_cron(identity, input.clone()).await.unwrap();
    let duplicate = order.receive_cron(identity, input).await.unwrap();
    assert_eq!(duplicate.receipt, committed.receipt);
    assert!(committed.receipt.commit_sequence > before.receipt.commit_sequence);
    assert_eq!(order.receipt_count(None, ()).await.unwrap(), before);
    assert!(
        matches!(order.receipt_count(Some(committed.receipt), ()).await,
        Err(InvocationError::NotStarted(Error::ReplicaBehind { observed_sequence, minimum_sequence }))
            if observed_sequence == before.receipt.commit_sequence && minimum_sequence == committed.receipt.commit_sequence)
    );
    refresh(sync, 1).await;
    for _ in 0..6 {
        let observed = order
            .receipt_count(Some(committed.receipt), ())
            .await
            .unwrap();
        assert_eq!(observed.output, before.output + 1);
        assert_eq!(observed.receipt, committed.receipt);
    }
    let counts = successes
        .each_ref()
        .map(|count| count.load(Ordering::Relaxed));
    assert_eq!(counts[0], 0);
    assert_eq!(counts[1] + counts[2], 13);
    assert!(counts[1].abs_diff(counts[2]) <= 1);
    println!(
        "PERF generated_replica_reads: successful_by_node={counts:?} unavailable_before_open=1 behind_rejected=1 refresh_rounds=2 duplicate_effects=0"
    );
}
