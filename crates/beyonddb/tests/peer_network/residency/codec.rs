use super::provisioning::Remote;
use super::*;
use crab_cell_runtime::peer::{PeerOperation, wire};
use crab_storage::test_support::CountingObjectStore;
use object_store::throttle::{ThrottleConfig, ThrottledStore};
use std::time::Duration;

fn peer(
    fixture: &Fixture,
    remote: &Remote,
) -> (Arc<PeerHttpRoundTrip>, Arc<PeerSigner>, PeerPrincipal) {
    let transport = Arc::new(PeerHttpRoundTrip::new(
        Arc::new(BeyonddbPeerScope),
        CellAuthority::new(fixture.layout.clone()),
        fixture.directory.clone(),
        Arc::new(fixture.remote_tls.client_identity()),
        remote.session,
    ));
    let signer = Arc::new(PeerSigner::new(
        remote.session,
        fixture.application.registry().release_digest(),
        fixture.remote_tls.signing_key().clone(),
    ));
    let hex = |bytes: &[u8]| {
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    let principal = PeerPrincipal {
        issuer: format!(
            "beyonddb-peer:{}",
            hex(fixture.directory.fleet().as_bytes())
        ),
        subject: hex(remote.session.as_bytes()),
        actions: vec!["beyonddb.cell.invoke".into()],
    };
    (transport, signer, principal)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_read_waits_for_codec_capacity_within_its_deadline() {
    let fixture = Fixture::new().await;
    let remote = Remote::new(&fixture).await;
    let (transport, signer, principal) = peer(&fixture, &remote);
    let client = CellClient::peer(fixture.application.registry(), signer, principal, transport);
    let runtime = fixture.node.runtime();
    let held = runtime.try_reserve_worker_job().unwrap().unwrap();
    let request = tokio::spawn(async move {
        client
            .query::<DescribeTable>(
                &account_target("123456789012").unwrap(),
                None,
                Json("Residency".into()),
            )
            .await
    });
    // Occupy only codec capacity for longer than the transport's single paced
    // retry, while leaving most of the signed request budget available.
    tokio::time::sleep(Duration::from_secs(2)).await;
    drop(held);
    let result = request.await.unwrap();
    remote.shutdown().await;
    fixture.shutdown().await;
    assert!(result.unwrap().output.0.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_catalog_io_leaves_cpu_capacity_for_unrelated_work() {
    let slow = Arc::new(ThrottledStore::new(
        InMemory::new(),
        ThrottleConfig::default(),
    ));
    let counted = Arc::new(CountingObjectStore::new(slow.clone()));
    let fixture = Fixture::with_store(2, counted.clone()).await;
    let remote = super::provisioning::Remote::new(&fixture).await;
    let (transport, signer, principal) = peer(&fixture, &remote);
    let client = CellClient::peer(fixture.application.registry(), signer, principal, transport);
    let account = account_target("123456789012").unwrap();
    let catalog = fixture
        .layout
        .catalog_head_path(account.tenant().as_bytes(), account.cell_id().as_bytes()[0])
        .to_string();
    counted.reset();
    slow.config_mut(|config| config.wait_get_per_call = Duration::from_secs(1));
    let request = tokio::spawn(async move {
        client
            .query::<DescribeTable>(&account, None, Json("Residency".into()))
            .await
    });
    // Observe the actual catalog GET after mTLS enrollment and signature checks.
    // The one-worker node must lend its CPU slot while this storage read waits.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if counted
                .requests()
                .iter()
                .any(|request| request.location == catalog)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let cpu = fixture.node.runtime().try_reserve_worker_job().unwrap();
    let available = cpu.is_some();
    drop(cpu);
    slow.config_mut(|config| config.wait_get_per_call = Duration::ZERO);
    let response = request.await.unwrap().unwrap();
    assert!(response.output.0.is_some());
    remote.shutdown().await;
    fixture.shutdown().await;
    assert!(available, "peer catalog I/O retained the only CPU slot");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_pre_dispatch_waits_obey_signed_request_deadline() {
    for codec in [false, true] {
        let slow = Arc::new(ThrottledStore::new(
            InMemory::new(),
            ThrottleConfig::default(),
        ));
        let counted = Arc::new(CountingObjectStore::new(slow.clone()));
        let fixture = Fixture::with_store(2, counted.clone()).await;
        let remote = Remote::new(&fixture).await;
        let (transport, signer, principal) = peer(&fixture, &remote);
        let account = account_target("123456789012").unwrap();
        let destination = fixture
            .directory
            .load(fixture.session, now_ms())
            .await
            .unwrap()
            .unwrap()
            .advertisement()
            .clone();
        let now = now_ms();
        let request = signer
            .sign(
                principal,
                now,
                now + 60_000,
                100,
                PeerOperation::Read(wire::ReadRequest {
                    target: Some(wire::Target {
                        tenant_id: account.tenant().as_bytes().to_vec(),
                        application_id: account.application().as_bytes().to_vec(),
                        namespace_id: account.namespace().as_bytes().to_vec(),
                        partition: account.partition().to_vec(),
                    }),
                    timeout_ms: 100,
                    minimum: None,
                    expected: None,
                    operation: Some(wire::read_request::Operation::Describe(true)),
                }),
            )
            .unwrap();
        let catalog = fixture
            .layout
            .catalog_head_path(account.tenant().as_bytes(), account.cell_id().as_bytes()[0])
            .to_string();
        counted.reset();
        let held = if codec {
            Some(
                fixture
                    .node
                    .runtime()
                    .try_reserve_worker_job()
                    .unwrap()
                    .unwrap(),
            )
        } else {
            slow.config_mut(|config| config.wait_get_per_call = Duration::from_secs(5));
            None
        };
        // Queueing and enrollment consume the original 100-ms request budget.
        // Releasing codec capacity afterward must not restart it or reach dispatch.
        let ((), result) = tokio::join!(
            async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                drop(held);
            },
            tokio::time::timeout(
                Duration::from_secs(2),
                transport.send_to_node(account, destination, request, 5_000),
            ),
        );
        let dispatched = counted
            .requests()
            .iter()
            .any(|request| request.location == catalog);
        slow.config_mut(|config| config.wait_get_per_call = Duration::ZERO);
        remote.shutdown().await;
        fixture.shutdown().await;
        assert!(matches!(
            result.expect("receiver ignored its signed deadline"),
            Err(crab_cell_runtime::Error::PeerTransportUnknown { .. })
        ));
        assert!(
            !dispatched,
            "expired request reached Cell catalog resolution"
        );
    }
}
