//! Three public `CellNode` hosts exercising the reference application's client contract.

use super::fleet::{GatewayStats, peer_round_trip, start_gateway_peer_server, start_peer_server};
use super::performance::report_samples;
use super::performance_fixture::{PerfFixture, identity, node_session, now_ms};
use crate::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crab_cell_runtime::cell::catalog::CellCatalog;
use crab_cell_runtime::cell::executor::Resolution;
use crab_cell_runtime::peer::{PeerPrincipal, PeerRoundTrip, PeerSigner, PeerVerifier};
use crab_cell_runtime::recovery::manifest::RecoveryManifestStore;
use tokio::net::TcpListener;

struct LoseMutationReply {
    inner: Arc<dyn PeerRoundTrip>,
    verifier: Arc<PeerVerifier>,
    dropped: AtomicBool,
}

impl PeerRoundTrip for LoseMutationReply {
    fn send(
        &self,
        target: CellTarget,
        request: Vec<u8>,
        remaining_ms: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'static>> {
        let inner = Arc::clone(&self.inner);
        let is_mutation = self
            .verifier
            .verify(&request, now_ms())
            .unwrap()
            .operation_tag()
            == 10;
        let drop_reply = is_mutation && !self.dropped.swap(true, Ordering::SeqCst);
        Box::pin(async move {
            let reply = inner.send(target, request, remaining_ms).await?;
            if drop_reply {
                return Err(Error::PeerTransportUnknown {
                    context: "test mutation reply lost after commit",
                    source: Box::new(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "reply dropped",
                    )),
                });
            }
            Ok(reply)
        })
    }
}

fn invocation(occurrence: u64) -> CronInvocation {
    CronInvocation {
        schedule_id: [72; 16],
        generation: 1,
        occurrence,
        scheduled_at_ms: now_ms(),
        payload: b"public-host".to_vec(),
    }
}

fn signed_client(
    fixture: &PerfFixture,
    round_trip: Arc<dyn PeerRoundTrip>,
    binding_node: usize,
) -> ReferenceClient {
    let signer = Arc::new(PeerSigner::new(
        crab_cell_runtime::SessionId::from_bytes([77; 16]),
        fixture.registry.release_digest(),
        SigningKey::from_bytes(&[78; 32]),
    ));
    let principal = PeerPrincipal {
        issuer: "reference-performance".into(),
        subject: "fleet-driver".into(),
        actions: vec!["cell.read".into(), "cell.write".into()],
    };
    let client = CellClient::peer(Arc::clone(&fixture.registry), signer, principal, round_trip);
    let handle = fixture.nodes[binding_node]
        .application_handle::<ReferenceApplication>(
            client,
            fixture.sql_target.tenant(),
            fixture.sql_target.application(),
        )
        .unwrap();
    ReferenceClient::new(handle).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_node_host_resolves_ambiguous_result_and_deduplicates_delivery() {
    let fixture = PerfFixture::start(3).await;
    let round_trip = fixture.round_trip.as_ref().unwrap();
    let signer = PeerSigner::new(
        crab_cell_runtime::SessionId::from_bytes([77; 16]),
        fixture.registry.release_digest(),
        SigningKey::from_bytes(&[78; 32]),
    );
    let verifier = Arc::new(PeerVerifier::new(
        crab_cell_runtime::SessionId::from_bytes([77; 16]),
        fixture.registry.release_digest(),
        signer.verifying_key(),
    ));
    let client = signed_client(
        &fixture,
        Arc::new(LoseMutationReply {
            inner: Arc::clone(round_trip),
            verifier,
            dropped: AtomicBool::new(false),
        }),
        0,
    );
    let order = client.orders(&OrderId(b"same-order".to_vec())).unwrap();
    let identity = reference_identity(73, now_ms());
    let input = invocation(1);
    let prepared = order
        .prepare_receive_cron(identity, input.clone())
        .await
        .unwrap();
    let pending = match prepared.execute().await {
        Err(InvocationError::Pending(pending)) => pending,
        other => panic!("expected pending after reply loss, got {other:?}"),
    };
    assert!(matches!(
        client.resolve(&pending).await.unwrap(),
        Resolution::Committed(_)
    ));

    let ordinary = ReferenceClient::new(fixture.typed.clone()).unwrap();
    let order = ordinary.orders(&OrderId(b"same-order".to_vec())).unwrap();
    order.receive_cron(identity, input).await.unwrap();
    assert_eq!(order.receipt_count(None, ()).await.unwrap().output, 1);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_node_host_recovers_published_state_after_owner_loss() {
    let fixture = PerfFixture::start(3).await;
    let original = ReferenceClient::new(fixture.typed.clone()).unwrap();
    let order = original
        .orders(&OrderId(b"surviving-order".to_vec()))
        .unwrap();
    order
        .receive_cron(reference_identity(74, now_ms()), invocation(1))
        .await
        .unwrap();
    assert_eq!(order.receipt_count(None, ()).await.unwrap().output, 1);

    let (recovered, _recovered_directory) = recover_sql_on_second_node(&fixture).await;
    let old_handle = fixture.owned_handles[0]
        .iter()
        .find(|handle| handle.cell_id() == fixture.sql_target.cell_id())
        .unwrap()
        .clone();
    let stale = fixture.nodes[0]
        .application_handle::<ReferenceApplication>(
            CellClient::local(Arc::clone(&fixture.registry), old_handle),
            fixture.sql_target.tenant(),
            fixture.sql_target.application(),
        )
        .unwrap();
    let stale = ReferenceClient::new(stale).unwrap();
    assert!(matches!(
        stale
            .orders(&OrderId(b"stale-owner".to_vec()))
            .unwrap()
            .receipt_count(None, ())
            .await,
        Err(InvocationError::NotStarted(Error::Fenced))
    ));
    let order = recovered
        .orders(&OrderId(b"surviving-order".to_vec()))
        .unwrap();
    assert_eq!(order.receipt_count(None, ()).await.unwrap().output, 1);
    order
        .receive_cron(reference_identity(75, now_ms()), invocation(2))
        .await
        .unwrap();
    assert_eq!(order.receipt_count(None, ()).await.unwrap().output, 2);
    fixture.shutdown().await;
}

async fn recover_sql_on_second_node(fixture: &PerfFixture) -> (ReferenceClient, tempfile::TempDir) {
    fixture.lose_owner(0);
    let layout = fixture.layout.as_ref().unwrap();
    let target = &fixture.sql_target;
    let authority = CellAuthority::new(layout.clone());
    let catalog = CellCatalog::new(layout.clone(), target.tenant());
    let observed = authority.load(target.cell_id()).await.unwrap().unwrap();
    seed_reference_session(layout, node_session(0)).await;
    let fence = fence_reference_session(layout, node_session(0), node_session(1))
        .await
        .direct_takeover()
        .unwrap();
    let limits = reference_limits(target.namespace()).unwrap();
    let recovered_directory = tempfile::TempDir::new().unwrap();
    let recovered = fixture.nodes[1]
        .runtime()
        .takeover_restored(
            catalog.lookup(target.cell_id()).await.unwrap().unwrap(),
            CellReplica::new(
                layout.clone(),
                *target.cell_id().as_bytes(),
                *IncarnationId::from_bytes([40; 16]).as_bytes(),
                limits,
            )
            .unwrap(),
            authority,
            observed,
            fence,
            RecoveryManifestStore::new(layout.clone(), limits),
            recovered_directory.path().join("recovered-sql.sqlite"),
            Owner {
                session: node_session(1),
                endpoint: "https://reference-recovered.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let client = CellClient::local(Arc::clone(&fixture.registry), recovered);
    let handle = fixture.nodes[1]
        .application_handle::<ReferenceApplication>(client, target.tenant(), target.application())
        .unwrap();
    let recovered = ReferenceClient::new(handle).unwrap();
    (recovered, recovered_directory)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_node_host_serves_two_release_ids_with_unchanged_module_contracts() {
    let predecessor = compiled();
    // The build identity changes while executable module contracts stay fixed.
    // Both release digests must bind their own clients during an online overlap.
    let successor = Arc::new(
        ReferenceApplication::compile(BuildDescriptor {
            source_revision: "reference-compatible-successor".into(),
            cargo_lock_digest: Digest::from_bytes([42; 32]),
        })
        .unwrap(),
    );
    assert_ne!(
        predecessor.registry().release_digest(),
        successor.registry().release_digest()
    );
    successor
        .registry()
        .verify_rolling_from(predecessor.registry().release_bytes())
        .unwrap();
    let fixture = PerfFixture::start_with_successor(3, Some(Arc::clone(&successor))).await;
    let sql_handle = fixture.owned_handles[0]
        .iter()
        .find(|handle| handle.cell_id() == fixture.sql_target.cell_id())
        .unwrap()
        .clone();
    let successor_client = CellClient::local(successor.registry(), sql_handle);
    let successor_handle = fixture.nodes[0]
        .application_handle::<ReferenceApplication>(
            successor_client,
            fixture.sql_target.tenant(),
            fixture.sql_target.application(),
        )
        .unwrap();
    let successor_client = ReferenceClient::new(successor_handle).unwrap();
    let order = successor_client
        .orders(&OrderId(b"rollout-order".to_vec()))
        .unwrap();
    order
        .receive_cron(reference_identity(76, now_ms()), invocation(1))
        .await
        .unwrap();

    let predecessor_client = ReferenceClient::new(fixture.typed.clone()).unwrap();
    let old_order = predecessor_client
        .orders(&OrderId(b"rollout-order".to_vec()))
        .unwrap();
    assert_eq!(old_order.receipt_count(None, ()).await.unwrap().output, 1);
    old_order
        .receive_cron(reference_identity(77, now_ms()), invocation(2))
        .await
        .unwrap();
    assert_eq!(order.receipt_count(None, ()).await.unwrap().output, 2);
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual action-level latency and recovery qualification"]
async fn reference_public_host_action_performance() {
    let iterations = std::env::var("CRAB_CELL_PERF_ITERATIONS")
        .ok()
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or(30);
    assert!((1..=1_000).contains(&iterations));
    let fixture = PerfFixture::start(3).await;
    let sql_handle = fixture.owned_handles[0]
        .iter()
        .find(|handle| handle.cell_id() == fixture.sql_target.cell_id())
        .unwrap()
        .clone();
    let local_handle = fixture.nodes[0]
        .application_handle::<ReferenceApplication>(
            CellClient::local(Arc::clone(&fixture.registry), sql_handle.clone()),
            fixture.sql_target.tenant(),
            fixture.sql_target.application(),
        )
        .unwrap();
    let local = ReferenceClient::new(local_handle).unwrap();
    let signer = PeerSigner::new(
        crab_cell_runtime::SessionId::from_bytes([77; 16]),
        fixture.registry.release_digest(),
        SigningKey::from_bytes(&[78; 32]),
    );
    let verifier = Arc::new(PeerVerifier::new(
        crab_cell_runtime::SessionId::from_bytes([77; 16]),
        fixture.registry.release_digest(),
        signer.verifying_key(),
    ));
    let (owner_address, owner_server) =
        start_peer_server(&fixture.registry, Arc::clone(&verifier), vec![sql_handle]).await;
    let gateway_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_address = gateway_listener.local_addr().unwrap();
    let gateway_stats = Arc::new(GatewayStats::default());
    let gateway_server = start_gateway_peer_server(
        gateway_listener,
        &fixture.registry,
        verifier,
        fixture.owned_handles[1].clone(),
        HashMap::from([(fixture.sql_target.cell_id(), owner_address)]),
        Arc::clone(&gateway_stats),
    );
    let forwarded = signed_client(
        &fixture,
        peer_round_trip(HashMap::from([(
            fixture.sql_target.cell_id(),
            gateway_address,
        )])),
        1,
    );
    for (lane, client, phase) in [("local", &local, 101_u8), ("forwarded", &forwarded, 102)] {
        let order = client
            .orders(&OrderId(b"performance-order".to_vec()))
            .unwrap();
        let proof_baseline = fixture.durability[0].object_waits().len();
        let mut actions = Vec::with_capacity(iterations);
        let mut durable_acks = Vec::with_capacity(iterations);
        let lane_started = Instant::now();
        for index in 0..iterations {
            let started = Instant::now();
            let occurrence = (phase as u64) * 10_000 + index as u64;
            let prepared = order
                .prepare_receive_cron(identity(phase, index, 0), invocation(occurrence))
                .await
                .unwrap();
            let ack_started = Instant::now();
            let committed = prepared.execute().await.unwrap();
            durable_acks.push(ack_started.elapsed());
            let observed = order
                .receipt_count(Some(committed.receipt), ())
                .await
                .unwrap();
            assert_eq!(
                observed.output,
                if lane == "local" {
                    index + 1
                } else {
                    iterations + index + 1
                } as u64
            );
            actions.push(started.elapsed());
        }
        let elapsed = lane_started.elapsed();
        report_samples(&format!("{lane}_verified_action"), &mut actions, elapsed);
        report_samples(
            &format!("{lane}_execute_to_durable_ack"),
            &mut durable_acks,
            elapsed,
        );
        let mut proof_waits = fixture.durability[0].object_waits();
        let mut proof_waits = proof_waits.split_off(proof_baseline);
        assert_eq!(proof_waits.len(), iterations);
        report_samples(
            &format!("{lane}_object_durability_proof_wait"),
            &mut proof_waits,
            elapsed,
        );
    }
    let (_, forwarded_count) = gateway_stats.counts();
    assert!(forwarded_count >= iterations * 3);

    let recovery_started = Instant::now();
    let (recovered, _recovered_directory) = recover_sql_on_second_node(&fixture).await;
    let observed = recovered
        .orders(&OrderId(b"performance-order".to_vec()))
        .unwrap()
        .receipt_count(None, ())
        .await
        .unwrap();
    assert_eq!(observed.output, (iterations * 2) as u64);
    let recovery_elapsed = recovery_started.elapsed();
    println!(
        "PERF owner_loss_to_first_verified_read: duration_ms={:.3}",
        recovery_elapsed.as_secs_f64() * 1_000.0
    );

    gateway_server.abort();
    owner_server.abort();
    fixture.shutdown().await;
}
