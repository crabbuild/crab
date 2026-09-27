//! Generated snapshot queries across independent public host processes.

use super::performance_fixture::{PerfFixture, identity, node_session, now_ms, rustfs_store};
use super::process_node;
use crate::*;
use crab_cell_runtime::client::{ReadPolicy, ReplicaReadRouter};
use crab_cell_runtime::peer::{
    PeerPrincipal, PeerRoundTrip, PeerSigner, ReplicaPeerClient, decode_peer_reply, wire,
};
use crab_cell_runtime::read_policy::ReadPolicyStore;
use std::{
    net::SocketAddr,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

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

async fn wait_for_readers(sync: &Path) {
    for node in 0..3 {
        super::process_performance::wait_for_marker(
            &sync.join(format!("node-{node}-readers.ready")),
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
    ReadPolicyStore::new(layout.clone())
        .create(target.cell_id(), control.value().incarnation, 2)
        .await
        .unwrap();
    wait_for_readers(sync).await;
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
    // The host must discover the new root without a fixture-issued refresh hint.
    std::fs::write(
        sync.join("readers.minimum"),
        committed.receipt.commit_sequence.to_string(),
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        for node in 1..3 {
            super::process_performance::wait_for_marker(
                &sync.join(format!("node-{node}-readers.refreshed")),
            )
            .await;
        }
    })
    .await
    .expect("automatic reader refresh timed out");
    for _ in 0..6 {
        let observed = order
            .receipt_count(Some(committed.receipt), ())
            .await
            .unwrap();
        assert_eq!(observed.output, before.output + 1);
        assert_eq!(observed.receipt, committed.receipt);
    }
    let policy = ReadPolicyStore::new(layout);
    let current = policy.load(target.cell_id()).await.unwrap().unwrap();
    policy.update(&current, 0).await.unwrap();
    std::fs::write(sync.join("readers.evicted"), []).unwrap();
    for node in 0..3 {
        super::process_performance::wait_for_marker(
            &sync.join(format!("node-{node}-readers.evicted")),
        )
        .await;
    }
    assert!(matches!(
        order.receipt_count(None, ()).await,
        Err(InvocationError::NotStarted(Error::ReplicaUnavailable))
    ));
    let counts = successes
        .each_ref()
        .map(|count| count.load(Ordering::Relaxed));
    assert_eq!(counts[0], 0);
    assert_eq!(counts[1] + counts[2], 12);
    assert!(counts[1].abs_diff(counts[2]) <= 1);
    println!(
        "PERF generated_replica_reads: successful_by_node={counts:?} unavailable_before_open=1 automatic_recruitment=2 automatic_refresh=2 evicted_readers=2 duplicate_effects=0"
    );
}
