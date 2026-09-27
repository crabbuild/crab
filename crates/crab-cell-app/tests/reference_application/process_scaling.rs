//! Constrained reader scaling; the external controller owns container faults.

use super::fleet::start_balancer;
use super::performance::{report_samples, run_reference_primitive_performance};
use super::performance_fixture::{PerfFixture, identity, node_session, now_ms, rustfs_store};
use super::process_node::directory;
use super::process_performance::{Controller, wait_for_marker};
use super::process_recruitment::{ObservedReads, ready_readers};
use crate::*;
use crab_cell_runtime::{
    client::{Observed, ReadPolicy, ReplicaReadRouter},
    peer::{PeerPrincipal, PeerSigner, ReplicaPeerClient},
    read_policy::ReadPolicyStore,
};
use std::{
    collections::{BTreeMap, HashMap},
    env,
    fs::File,
    io::{BufWriter, Write},
    net::SocketAddr,
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};

struct LoadWindow<'a> {
    label: &'a str,
    request_domain: u8,
    started: Instant,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "Compose controller required: 3/5/10/20 constrained nodes and reader SIGKILL"]
async fn reference_compose_reader_scaling() {
    let sync = env::var("CRAB_CELL_PERF_PROCESS_SYNC").unwrap();
    let sync = Path::new(&sync);
    let mut controller = Controller::new(sync);
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
        observed.lock().unwrap().clear();
        let window = LoadWindow {
            label: "mixed",
            request_domain: 107,
            started: Instant::now(),
        };
        expected = mixed_load(&owner, &reader, sync, count, expected, window).await;
        let reads = observed.lock().unwrap().clone();
        assert_eq!(
            reads
                .keys()
                .copied()
                .collect::<std::collections::HashSet<_>>(),
            selected
        );
        assert!(!reads.contains_key(&node_session(0)));
        let entries = ingress
            .counts()
            .iter()
            .zip(entries)
            .map(|(after, before)| after - before)
            .collect::<Vec<_>>();
        assert!(entries.iter().all(|count| *count > 0));
        assert!(entries.iter().max().unwrap() - entries.iter().min().unwrap() <= 1);
        println!(
            "PERF mixed_reader_distribution: nodes={count} by_session={reads:?} writer_ingress={entries:?}"
        );
        ready_readers(&router, &peer, target, expected.receipt, desired, None).await;
        if count == 5 {
            let lost = *survivors
                .iter()
                .find(|node| **node >= 3 && selected.contains(&node_session(**node)))
                .unwrap();
            // Keep owner ingress on the original three nodes while faulting a
            // reader. Gateway withdrawal is a separate product fleet invariant.
            let (address, fault_server, _) = start_balancer(owners).await;
            let fault_fixture = PerfFixture::from_processes(
                tempfile::TempDir::new().unwrap(),
                rustfs_store(),
                owners,
                Some(address),
            );
            let fault_client = ReferenceClient::new(fault_fixture.typed.clone()).unwrap();
            let fault_owner = fault_client
                .orders(&OrderId(b"compose-scaling".to_vec()))
                .unwrap();
            observed.lock().unwrap().clear();
            let window = LoadWindow {
                label: "reader_loss",
                request_domain: 108,
                started: Instant::now(),
            };
            let started = window.started;
            let minimum = expected.receipt;
            let fault = async {
                tokio::time::sleep_until((started + Duration::from_secs(10)).into()).await;
                assert!(
                    observed
                        .lock()
                        .unwrap()
                        .get(&node_session(lost))
                        .copied()
                        .unwrap_or(0)
                        > 0
                );
                let requested_us = started.elapsed().as_micros();
                survivors = controller.command("kill", lost).await;
                assert!(!survivors.contains(&lost));
                let killed_us = started.elapsed().as_micros();
                let replacement = ready_readers(
                    &router,
                    &peer,
                    target,
                    minimum,
                    desired,
                    Some(node_session(lost)),
                )
                .await;
                let ready_us = started.elapsed().as_micros();
                let added = replacement
                    .difference(&selected)
                    .copied()
                    .collect::<Vec<_>>();
                assert!(!added.is_empty());
                loop {
                    // Only load lanes issue queries here. Readiness alone cannot
                    // prove that a newly recruited reader served live traffic.
                    if added.iter().all(|session| {
                        observed.lock().unwrap().get(session).copied().unwrap_or(0) > 0
                    }) {
                        break;
                    }
                    assert!(
                        started.elapsed() < Duration::from_secs(50),
                        "replacement did not serve during the load window"
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                let served_us = started.elapsed().as_micros();
                assert!(
                    served_us < 50_000_000,
                    "replacement left no post-recovery load interval"
                );
                std::fs::write(sync.join("reader-loss.tsv"), format!(
                    "killed_node\trequested_us\tkilled_us\tready_us\tserved_us\n{lost}\t{requested_us}\t{killed_us}\t{ready_us}\t{served_us}\n"
                )).unwrap();
                (
                    replacement,
                    killed_us - requested_us,
                    served_us - requested_us,
                )
            };
            let load = mixed_load(&fault_owner, &reader, sync, count, expected, window);
            let (output, (replacement, fault_command_us, recovery_us)) = tokio::join!(load, fault);
            expected = output;
            fault_server.abort();
            assert_eq!(
                ready_readers(
                    &router,
                    &peer,
                    target,
                    expected.receipt,
                    desired,
                    Some(node_session(lost))
                )
                .await,
                replacement
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
                "PERF constrained_reader_replacement: nodes_before=5 nodes_after=4 killed_node={lost} ready_readers={desired} exact_queries=12 fault_command_us={fault_command_us} recovery_us={recovery_us} during_arrivals=1"
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

async fn mixed_load(
    owner: &ReferenceOrderCell,
    reader: &ReferenceOrderCell,
    sync: &Path,
    nodes: usize,
    baseline: Observed<u64>,
    window: LoadWindow<'_>,
) -> Observed<u64> {
    let LoadWindow {
        label,
        request_domain,
        started,
    } = window;
    let window = Duration::from_secs(60);
    let latest = Mutex::new(baseline.clone());
    let writes = async {
        let mut raw =
            BufWriter::new(File::create(sync.join(format!("{label}-{nodes}-writes.tsv"))).unwrap());
        writeln!(
            raw,
            "arrival\tscheduled_us\tstarted_us\telapsed_us\toutcome\tsequence\tcount"
        )
        .unwrap();
        writeln!(
            raw,
            "baseline\t0\t0\t0\tbaseline\t{}\t{}",
            baseline.receipt.commit_sequence, baseline.output
        )
        .unwrap();
        let mut acknowledged =
            BTreeMap::from([(baseline.receipt.commit_sequence, baseline.output)]);
        let mut samples = Vec::new();
        let mut missed = 0;
        for arrival in 0..300 {
            let offset = Duration::from_millis(arrival * 200);
            tokio::time::sleep_until((started + offset).into()).await;
            let dispatched = started.elapsed();
            // A slow writer must not lower the advertised rate or catch up in
            // a burst. Missing arrivals remain part of the capacity result.
            if dispatched.saturating_sub(offset) >= Duration::from_millis(200) {
                missed += 1;
                writeln!(
                    raw,
                    "{arrival}\t{}\t{}\t0\tscheduler_late\t0\t0",
                    offset.as_micros(),
                    dispatched.as_micros()
                )
                .unwrap();
                continue;
            }
            let input = CronInvocation {
                schedule_id: [request_domain; 16],
                generation: 1,
                occurrence: (nodes as u64 * 300) + arrival,
                scheduled_at_ms: now_ms(),
                payload: b"sustained-replica-refresh".to_vec(),
            };
            let call = Instant::now();
            let committed = owner
                .receive_cron(
                    identity(request_domain, nodes * 300 + arrival as usize, 0),
                    input,
                )
                .await
                .unwrap();
            let elapsed = call.elapsed();
            let count = baseline.output + samples.len() as u64 + 1;
            assert_eq!(committed.receipt.cell, baseline.receipt.cell);
            assert_eq!(committed.receipt.incarnation, baseline.receipt.incarnation);
            assert!(committed.receipt.commit_sequence > *acknowledged.last_key_value().unwrap().0);
            acknowledged.insert(committed.receipt.commit_sequence, count);
            *latest.lock().unwrap() = Observed {
                output: count,
                receipt: committed.receipt,
            };
            writeln!(
                raw,
                "{arrival}\t{}\t{}\t{}\tcommitted\t{}\t{count}",
                offset.as_micros(),
                dispatched.as_micros(),
                elapsed.as_micros(),
                committed.receipt.commit_sequence
            )
            .unwrap();
            raw.flush().unwrap();
            samples.push(elapsed);
        }
        raw.flush().unwrap();
        assert!(!samples.is_empty());
        println!(
            "PERF {label}_writes: nodes={nodes} planned=300 committed={} missed={missed} arrival_seconds=60",
            samples.len()
        );
        report_samples(
            &format!("{label}_writes_{nodes}_nodes"),
            &mut samples,
            started.elapsed().max(window),
        );
        acknowledged
    };
    let reads = futures_util::future::join_all((0..8).map(|lane| {
        let latest = &latest;
        async move {
            let mut raw = BufWriter::new(
                File::create(sync.join(format!("{label}-{nodes}-reader-{lane}.tsv"))).unwrap(),
            );
            writeln!(
                raw,
                "started_us\telapsed_us\tminimum_sequence\tlatest_count\toutcome\tsequence\tcount"
            )
            .unwrap();
            let mut samples = Vec::new();
            let mut behind = 0;
            while started.elapsed() < window {
                // Four lanes request the last acknowledged receipt; four
                // permit older snapshots and report their observed lag.
                let known = latest.lock().unwrap().clone();
                let minimum = (lane % 2 == 0).then_some(known.receipt);
                let floor = minimum.map_or(0, |receipt| receipt.commit_sequence);
                let dispatched = started.elapsed();
                let call = Instant::now();
                match reader.receipt_count(minimum, ()).await {
                    Ok(observed) => {
                        let elapsed = call.elapsed();
                        assert_eq!(observed.receipt.cell, baseline.receipt.cell);
                        assert_eq!(observed.receipt.incarnation, baseline.receipt.incarnation);
                        assert!(observed.receipt.commit_sequence >= floor);
                        writeln!(
                            raw,
                            "{}\t{}\t{floor}\t{}\tok\t{}\t{}",
                            dispatched.as_micros(),
                            elapsed.as_micros(),
                            known.output,
                            observed.receipt.commit_sequence,
                            observed.output
                        )
                        .unwrap();
                        samples.push((observed, elapsed, known.output));
                        assert!(
                            samples.len() <= 100_000,
                            "reader evidence exceeded its memory bound"
                        );
                    }
                    Err(InvocationError::NotStarted(Error::ReplicaBehind { .. }))
                        if minimum.is_some() =>
                    {
                        behind += 1;
                        writeln!(
                            raw,
                            "{}\t{}\t{floor}\t{}\tbehind\t0\t0",
                            dispatched.as_micros(),
                            call.elapsed().as_micros(),
                            known.output
                        )
                        .unwrap();
                    }
                    Err(error) => panic!("mixed replica read failed: {error}"),
                }
            }
            raw.flush().unwrap();
            assert!(!samples.is_empty(), "reader lane made no progress");
            (samples, behind)
        }
    }));
    let (acknowledged, readers) = tokio::join!(writes, reads);
    let mut latencies = Vec::new();
    let mut lag = 0;
    let mut behind = 0;
    for (samples, rejected) in readers {
        behind += rejected;
        for (observed, elapsed, known_count) in samples {
            // A read can finish before its overlapping write is acknowledged.
            // Join after both lanes finish, using exact commit positions.
            let (_, count) = acknowledged
                .range(..=observed.receipt.commit_sequence)
                .next_back()
                .unwrap();
            assert_eq!(
                observed.output, *count,
                "snapshot value disagrees with its receipt"
            );
            lag = lag.max(known_count.saturating_sub(observed.output));
            latencies.push(elapsed);
        }
    }
    println!(
        "PERF {label}_reads: nodes={nodes} clients=8 behind={behind} max_acknowledged_count_lag={lag} window_seconds=60"
    );
    report_samples(
        &format!("{label}_replica_reads_{nodes}_nodes"),
        &mut latencies,
        started.elapsed(),
    );
    let expected = latest.into_inner().unwrap();
    assert_eq!(
        owner
            .receipt_count(Some(expected.receipt), ())
            .await
            .unwrap(),
        expected
    );
    expected
}
