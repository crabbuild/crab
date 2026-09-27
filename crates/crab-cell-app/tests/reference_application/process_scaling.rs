//! Constrained reader scaling; the external controller owns container faults.

use super::fleet::start_balancer;
use super::performance::{report_samples, run_reference_primitive_performance};
use super::performance_fixture::{PerfFixture, identity, node_session, now_ms, rustfs_store};
use super::process_node::directory;
use super::process_performance::wait_for_marker;
use super::process_recruitment::{ObservedReads, ready_readers};
use crate::*;
use crab_cell_runtime::{
    client::{ReadPolicy, ReplicaReadRouter},
    peer::{PeerPrincipal, PeerSigner, ReplicaPeerClient},
    read_policy::ReadPolicyStore,
};
use std::{collections::HashMap, env, net::SocketAddr, path::Path, sync::Mutex, time::Instant};

struct Controller<'a> {
    sync: &'a Path,
    sequence: usize,
}

impl Controller<'_> {
    async fn command(&mut self, action: &str, count: usize) -> Vec<usize> {
        let path = self.sync.join(format!("fleet-{}.request", self.sequence));
        let temporary = path.with_extension("tmp");
        std::fs::write(&temporary, format!("{action} {count}")).unwrap();
        std::fs::rename(temporary, &path).unwrap();
        let done = path.with_extension("done");
        wait_for_marker(&done).await;
        self.sequence += 1;
        std::fs::read_to_string(done)
            .unwrap()
            .split_whitespace()
            .map(|node| node.parse().unwrap())
            .collect()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "Compose controller required: 3/5/10/20 constrained nodes and reader SIGKILL"]
async fn reference_compose_reader_scaling() {
    let sync = env::var("CRAB_CELL_PERF_PROCESS_SYNC").unwrap();
    let sync = Path::new(&sync);
    let mut controller = Controller { sync, sequence: 0 };
    let application = Arc::new(compiled());
    let registry = application.registry();
    let layout = CellStorageLayout::new(
        rustfs_store(),
        object_store::path::Path::from(env::var("CRAB_CELL_PERF_PROCESS_ROOT").unwrap()),
        *ApplicationId::from_bytes([82; 16]).as_bytes(),
    );
    let authority = CellAuthority::new(layout.clone());
    let membership = directory(&layout, &registry);
    let router = ReplicaReadRouter::new(authority.clone(), membership.clone());
    let observed = Arc::new(Mutex::new(HashMap::new()));
    let peer = ReplicaPeerClient::new(
        registry.clone(),
        Arc::new(PeerSigner::new(
            crab_cell_runtime::SessionId::from_bytes([77; 16]),
            registry.release_digest(),
            SigningKey::from_bytes(&[78; 32]),
        )),
        PeerPrincipal {
            issuer: "reference-performance".into(),
            subject: "reader-scaling".into(),
            actions: vec!["cell.read".into(), "cell.replica.status".into()],
        },
        Arc::new(ObservedReads(observed.clone())),
    );
    let policy = ReadPolicyStore::new(layout);
    let mut original_owner = None;
    let mut survivors = Vec::new();
    for count in [3, 5, 10, 20] {
        survivors = controller.command("scale", count).await;
        assert_eq!(survivors.len(), count);
        let mut endpoints = Vec::new();
        for node in &survivors {
            wait_for_marker(&sync.join(format!("node-{node}.serving"))).await;
            endpoints.push(
                std::fs::read_to_string(sync.join(format!("node-{node}.ready")))
                    .unwrap()
                    .parse::<SocketAddr>()
                    .unwrap(),
            );
        }
        assert_eq!(membership.live(now_ms(), 32).await.unwrap().len(), count);
        let owners = [endpoints[0], endpoints[1], endpoints[2]];
        let (balancer, server, ingress) = start_balancer(endpoints).await;
        let fixture = PerfFixture::from_processes(
            tempfile::TempDir::new().unwrap(),
            rustfs_store(),
            owners,
            Some(balancer),
        );
        if count == 3 {
            run_reference_primitive_performance(&fixture, true, "reader_scaling_initial").await;
        }
        let target = &fixture.sql_target;
        let owner = ReferenceClient::new(fixture.typed.clone()).unwrap();
        let owner = owner.orders(&OrderId(b"compose-scaling".to_vec())).unwrap();
        let client = fixture
            .client
            .clone()
            .with_read_replicas(router.clone(), peer.clone(), None)
            .unwrap();
        let handle = ApplicationHandle::new(
            client,
            application.clone(),
            target.tenant(),
            target.application(),
        )
        .unwrap();
        let generated = ReferenceClient::new(handle.with_read_policy(ReadPolicy::Replica)).unwrap();
        let reader = generated
            .orders(&OrderId(b"compose-scaling".to_vec()))
            .unwrap();
        let control = authority.load(target.cell_id()).await.unwrap().unwrap();
        let ownership = (
            control.value().owner.clone(),
            control.value().epoch,
            control.value().incarnation,
        );
        assert_eq!(original_owner.get_or_insert(ownership.clone()), &ownership);
        // One spare at five nodes lets expiry recruit a replacement without
        // a fixture activation or a new process masking the failure.
        let desired = if count == 5 { 3 } else { count - 1 };
        match policy.load(target.cell_id()).await.unwrap() {
            Some(current) => {
                policy.update(&current, desired as u16).await.unwrap();
            }
            None => {
                policy
                    .create(
                        target.cell_id(),
                        control.value().incarnation,
                        desired as u16,
                    )
                    .await
                    .unwrap();
            }
        }
        let mut selected = Default::default();
        let mut expected = owner.receipt_count(None, ()).await.unwrap();
        for round in 0..2 {
            let input = CronInvocation {
                schedule_id: [106; 16],
                generation: 1,
                occurrence: (count * 2 + round) as u64,
                scheduled_at_ms: now_ms(),
                payload: b"compose-reader-scaling".to_vec(),
            };
            let identity = identity(106, count * 2 + round, 0);
            let committed = owner.receive_cron(identity, input.clone()).await.unwrap();
            let started = Instant::now();
            let duplicate = owner.receive_cron(identity, input).await.unwrap();
            assert_eq!(duplicate.receipt, committed.receipt);
            let output = owner
                .receipt_count(Some(committed.receipt), ())
                .await
                .unwrap();
            assert_eq!(output.output, expected.output + 1);
            expected = output;
            selected =
                ready_readers(&router, &peer, target, committed.receipt, desired, None).await;
            println!(
                "PERF reader_readiness: nodes={count} readers={desired} round={round} elapsed_ms={}",
                started.elapsed().as_millis()
            );
        }
        observed.lock().unwrap().clear();
        let started = Instant::now();
        let mut samples = Vec::new();
        for _ in 0..desired * 30 {
            let call = Instant::now();
            assert_eq!(
                reader
                    .receipt_count(Some(expected.receipt), ())
                    .await
                    .unwrap(),
                expected
            );
            samples.push(call.elapsed());
        }
        report_samples(
            &format!("replica_reads_{count}_nodes"),
            &mut samples,
            started.elapsed(),
        );
        let reads = observed.lock().unwrap().clone();
        assert_eq!(
            reads
                .keys()
                .copied()
                .collect::<std::collections::HashSet<_>>(),
            selected
        );
        assert!(reads.values().all(|count| *count == 30));
        assert!(!reads.contains_key(&node_session(0)));
        let by_node = survivors
            .iter()
            .map(|node| (*node, reads.get(&node_session(*node)).copied().unwrap_or(0)))
            .collect::<Vec<_>>();
        // Exercise every ingress independently of selected replica placement.
        for _ in 0..count * 2 {
            assert_eq!(
                owner
                    .receipt_count(Some(expected.receipt), ())
                    .await
                    .unwrap(),
                expected
            );
        }
        let entries = ingress.counts();
        assert!(entries.iter().all(|count| *count > 0));
        assert!(entries.iter().max().unwrap() - entries.iter().min().unwrap() <= 1);
        println!(
            "PERF reader_scale: nodes={count} readers={desired} by_node={by_node:?} ingress={entries:?} owner_unchanged=1"
        );
        if count == 5 {
            let lost = *survivors
                .iter()
                .find(|node| **node >= 3 && selected.contains(&node_session(**node)))
                .unwrap();
            let started = Instant::now();
            survivors = controller.command("kill", lost).await;
            assert!(!survivors.contains(&lost));
            let fault_command_ms = started.elapsed().as_millis();
            let replacement = ready_readers(
                &router,
                &peer,
                target,
                expected.receipt,
                desired,
                Some(node_session(lost)),
            )
            .await;
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
                        .receipt_count(Some(expected.receipt), ())
                        .await
                        .unwrap(),
                    expected
                );
            }
            assert_eq!(
                observed
                    .lock()
                    .unwrap()
                    .keys()
                    .copied()
                    .collect::<std::collections::HashSet<_>>(),
                replacement
            );
            println!(
                "PERF constrained_reader_replacement: nodes_before=5 nodes_after=4 killed_node={lost} ready_readers={desired} exact_queries=12 fault_command_ms={fault_command_ms} recovery_ms={recovery_ms}"
            );
        }
        let after = authority.load(target.cell_id()).await.unwrap().unwrap();
        assert_eq!(
            (
                after.value().owner.clone(),
                after.value().epoch,
                after.value().incarnation
            ),
            ownership
        );
        if count == 20 {
            let current = policy.load(target.cell_id()).await.unwrap().unwrap();
            policy.update(&current, 0).await.unwrap();
            std::fs::write(sync.join("readers.evicted"), []).unwrap();
            for node in &survivors {
                wait_for_marker(&sync.join(format!("node-{node}-readers.evicted"))).await;
            }
            assert!(matches!(
                reader.receipt_count(None, ()).await,
                Err(InvocationError::NotStarted(Error::ReplicaUnavailable))
            ));
        }
        server.abort();
    }
    std::fs::write(sync.join("stop"), []).unwrap();
    for node in survivors {
        wait_for_marker(&sync.join(format!("node-{node}.done"))).await;
    }
}
