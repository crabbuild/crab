//! Scheduled arrivals retain overload and verify every resulting Cell ledger.

use super::*;
use crate::reference_application::fleet::{balancer_round_trip, start_balancer};
use crate::reference_application::process_performance::Controller;
use crab_cell_runtime::{
    Receipt,
    cell::executor::{Resolution, StoredOutcome},
};
use std::{
    fs::File,
    io::{BufWriter, Write},
};
use tokio::task::JoinSet;

const WINDOW_SECONDS: usize = 10;

struct Window {
    id: usize,
    nodes: usize,
    shape: &'static str,
    rate_per_node: usize,
    concurrency: usize,
}

impl Window {
    fn label(&self) -> String {
        format!(
            "entities-{}-{}-{}",
            self.nodes, self.shape, self.rate_per_node
        )
    }
}

struct Sample {
    arrival: usize,
    scheduled_us: u64,
    started_us: u64,
    elapsed_us: u64,
    entity: usize,
    write: bool,
    outcome: &'static str,
    sequence: u64,
    read_sequence: u64,
    count: u64,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "Compose controller required for scheduled entity traffic on 3/5/10/20 nodes"]
async fn entity_process_scaling() {
    let sync = env::var("CRAB_CELL_PERF_PROCESS_SYNC").unwrap();
    let sync = Path::new(&sync);
    let mut controller = Controller::new(sync);
    let application = compiled_entities();
    let layout = CellStorageLayout::new(
        rustfs_store(),
        env::var("CRAB_CELL_PERF_PROCESS_ROOT").unwrap().into(),
        *ApplicationId::from_bytes([82; 16]).as_bytes(),
    );
    let authority = CellAuthority::new(layout.clone());
    let mut owners = BufWriter::new(File::create(sync.join("entity-owners.tsv")).unwrap());
    writeln!(owners, "stage\tentity\tcell\towner\tepoch\tincarnation").unwrap();
    let mut expected = Vec::new();
    let mut window_id = 0;
    for nodes in [3, 5, 10, 20] {
        assert_eq!(
            controller.command("scale", nodes).await,
            (0..nodes).collect::<Vec<_>>()
        );
        for node in 0..nodes {
            wait_for_marker(&sync.join(format!("node-{node}.serving"))).await;
        }
        let addresses = endpoints(sync, nodes).await;
        publish_marker(&sync.join("entity-stage.request"), nodes.to_string());
        for node in 0..nodes {
            wait_for_marker(&sync.join(format!("node-{node}-stage-{nodes}.ready"))).await;
        }
        assert_eq!(
            process_node::directory(&layout, &application.registry())
                .live(now_ms(), 32)
                .await
                .unwrap()
                .len(),
            nodes
        );
        let (address, server, ingress) = start_balancer(addresses.clone()).await;
        let handle = ApplicationHandle::new(
            peer_client(&application, balancer_round_trip(address)),
            application.clone(),
            TenantId::from_bytes([81; 16]),
            ApplicationId::from_bytes([82; 16]),
        )
        .unwrap();
        let client = Arc::new(EntityReferenceClient::new(handle).unwrap());
        expected.resize(nodes * ENTITIES_PER_NODE, 0_u64);
        for (entity, count) in expected.iter().enumerate() {
            let target = entity_target(&application, entity);
            let control = authority.load(target.cell_id()).await.unwrap().unwrap();
            let owner = control.value().owner.as_ref().unwrap();
            let node = entity / ENTITIES_PER_NODE;
            assert_eq!(owner.session, node_session(node));
            assert_eq!(owner.endpoint, format!("https://{}", addresses[node]));
            writeln!(
                owners,
                "{nodes}\t{entity}\t{:?}\t{node}\t{}\t{:?}",
                target.cell_id(),
                control.value().epoch,
                control.value().incarnation
            )
            .unwrap();
            let order = client.orders(&entity_key(entity)).unwrap();
            assert_eq!(order.target(), &target);
            assert_eq!(order.receipt_count(None, ()).await.unwrap().output, *count);
        }
        owners.flush().unwrap();
        for shape in ["uniform", "hot", "skewed"] {
            for (rate_per_node, concurrency) in [(1, 4), (4, 16), (16, 64)] {
                let window = Window {
                    id: window_id,
                    nodes,
                    shape,
                    rate_per_node,
                    concurrency,
                };
                run_window(sync, &window, client.clone(), &mut expected).await;
                window_id += 1;
            }
        }
        let mut roots =
            BufWriter::new(File::create(sync.join(format!("entity-roots-{nodes}.tsv"))).unwrap());
        writeln!(
            roots,
            "entity\tcell\towner\tepoch\tincarnation\troot_sequence\troot_digest"
        )
        .unwrap();
        for entity in 0..expected.len() {
            let target = entity_target(&application, entity);
            let control = authority.load(target.cell_id()).await.unwrap().unwrap();
            let owner = control.value().owner.as_ref().unwrap();
            let node = entity / ENTITIES_PER_NODE;
            assert_eq!(owner.session, node_session(node));
            let root = control.value().root.as_ref().unwrap();
            writeln!(
                roots,
                "{entity}\t{:?}\t{node}\t{}\t{:?}\t{}\t{:?}",
                target.cell_id(),
                control.value().epoch,
                control.value().incarnation,
                root.commit_sequence,
                root.digest
            )
            .unwrap();
        }
        roots.flush().unwrap();
        let counts = ingress.counts();
        assert!(counts.iter().all(|count| *count > 0));
        assert!(counts.iter().max().unwrap() - counts.iter().min().unwrap() <= 1);
        publish_marker(
            &sync.join(format!("entity-ingress-{nodes}.txt")),
            counts
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(" "),
        );
        println!(
            "ENTITY_SCALE nodes={nodes} cells={} ingress={counts:?}",
            expected.len()
        );
        server.abort();
        let _ = server.await;
    }
    publish_marker(&sync.join("stop"), []);
    for node in 0..20 {
        wait_for_marker(&sync.join(format!("node-{node}.done"))).await;
    }
    assert!(
        process_node::directory(&layout, &application.registry())
            .live(now_ms(), 32)
            .await
            .unwrap()
            .is_empty()
    );
}

fn destination(shape: &str, arrival: usize, cells: usize) -> (usize, bool) {
    match shape {
        "uniform" => (arrival % cells, true),
        "hot" => (
            if arrival % 5 == 4 {
                1 + (arrival / 5) % (cells - 1)
            } else {
                0
            },
            true,
        ),
        "skewed" => {
            if arrival.is_multiple_of(5) {
                ((arrival / 5) % cells, true)
            } else {
                (arrival % ENTITIES_PER_NODE, false)
            }
        }
        _ => panic!("undeclared traffic shape"),
    }
}

async fn run_window(
    sync: &Path,
    window: &Window,
    client: Arc<EntityReferenceClient>,
    expected: &mut [u64],
) {
    let rate = window.nodes * window.rate_per_node;
    let planned = rate * WINDOW_SECONDS;
    let label = window.label();
    let mut output = BufWriter::new(File::create(sync.join(format!("{label}.tsv"))).unwrap());
    writeln!(
        output,
        "arrival\tscheduled_us\tstarted_us\telapsed_us\tentity\tkind\toutcome\tsequence\tread_sequence\tcount"
    )
    .unwrap();
    output.flush().unwrap();
    let started_ms = now_ms();
    let started = Instant::now();
    let mut jobs = JoinSet::new();
    let mut samples = Vec::with_capacity(planned);
    for arrival in 0..planned {
        let scheduled_us = (arrival as u64 * 1_000_000) / rate as u64;
        tokio::time::sleep_until((started + Duration::from_micros(scheduled_us)).into()).await;
        while let Some(result) = jobs.try_join_next() {
            retain_sample(&mut output, &mut samples, result.unwrap());
        }
        let started_us = started.elapsed().as_micros() as u64;
        let (entity, write) = destination(window.shape, arrival, expected.len());
        let mut sample = Sample {
            arrival,
            scheduled_us,
            started_us,
            elapsed_us: 0,
            entity,
            write,
            outcome: "client_full",
            sequence: 0,
            read_sequence: 0,
            count: 0,
        };
        if started_us >= (arrival as u64 + 1) * 1_000_000 / rate as u64 {
            sample.outcome = "scheduler_late";
        }
        if sample.outcome == "scheduler_late" || jobs.len() >= window.concurrency {
            retain_sample(&mut output, &mut samples, sample);
            continue;
        }
        let client = client.clone();
        let request = window.id * 1_000_000 + arrival;
        let admitted = started + Duration::from_micros(sample.started_us);
        jobs.spawn(async move { execute(client, sample, request, admitted).await });
    }
    tokio::time::sleep_until((started + Duration::from_secs(WINDOW_SECONDS as u64)).into()).await;
    while let Some(result) = jobs.join_next().await {
        retain_sample(&mut output, &mut samples, result.unwrap());
    }
    let elapsed_us = started.elapsed().as_micros() as u64;
    let ended_ms = now_ms();
    samples.sort_by_key(|sample| sample.arrival);
    assert_eq!(samples.len(), planned);
    for sample in &samples {
        if sample.write && sample.sequence > 0 {
            expected[sample.entity] += 1;
        }
    }
    let mut checks =
        BufWriter::new(File::create(sync.join(format!("{label}-readback.tsv"))).unwrap());
    writeln!(checks, "entity\texpected\tactual\tsequence").unwrap();
    for (entity, expected) in expected.iter().enumerate() {
        let actual = client
            .orders(&entity_key(entity))
            .unwrap()
            .receipt_count(None, ())
            .await
            .unwrap();
        writeln!(
            checks,
            "{entity}\t{expected}\t{}\t{}",
            actual.output, actual.receipt.commit_sequence
        )
        .unwrap();
        assert_eq!(
            actual.output, *expected,
            "{label}: Cell {entity} lost or duplicated a mutation"
        );
    }
    checks.flush().unwrap();
    publish_marker(
        &sync.join(format!("{label}-window.tsv")),
        format!(
            "window_id\tnodes\tshape\trate_per_node\tconcurrency\tseconds\tstarted_ms\tended_ms\telapsed_us\n{}\t{}\t{}\t{}\t{}\t{WINDOW_SECONDS}\t{started_ms}\t{ended_ms}\t{elapsed_us}\n",
            window.id, window.nodes, window.shape, window.rate_per_node, window.concurrency
        ),
    );
    let complete = samples
        .iter()
        .filter(|sample| sample.outcome == "ok" || sample.outcome == "resolved")
        .count();
    println!(
        "ENTITY_WINDOW label={label} planned={planned} complete={complete} elapsed_us={elapsed_us}"
    );
}

fn retain_sample(output: &mut BufWriter<File>, samples: &mut Vec<Sample>, sample: Sample) {
    writeln!(
        output,
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        sample.arrival,
        sample.scheduled_us,
        sample.started_us,
        sample.elapsed_us,
        sample.entity,
        if sample.write { "write" } else { "read" },
        sample.outcome,
        sample.sequence,
        sample.read_sequence,
        sample.count
    )
    .unwrap();
    output.flush().unwrap();
    samples.push(sample);
}

async fn execute(
    client: Arc<EntityReferenceClient>,
    mut sample: Sample,
    request: usize,
    started: Instant,
) -> Sample {
    let order = client.orders(&entity_key(sample.entity)).unwrap();
    let minimum = if sample.write {
        let input = CronInvocation {
            schedule_id: [117; 16],
            generation: 1,
            occurrence: request as u64 + 1,
            scheduled_at_ms: now_ms(),
            payload: b"distributed-entity-invoice".to_vec(),
        };
        match order
            .receive_cron(qualification_identity(request as u64, now_ms()), input)
            .await
        {
            Ok(committed) => {
                sample.outcome = "ok";
                Some(committed.receipt)
            }
            Err(InvocationError::NotStarted(error)) => {
                eprintln!("ENTITY_NOT_STARTED request={request} error={error}");
                sample.outcome = "not_started";
                sample.elapsed_us = started.elapsed().as_micros() as u64;
                return sample;
            }
            Err(InvocationError::Pending(pending)) => {
                let deadline = Instant::now() + Duration::from_secs(30);
                loop {
                    match client.resolve(&pending).await {
                        Ok(Resolution::Committed(StoredOutcome::Success {
                            commit_sequence,
                            ..
                        })) => {
                            sample.outcome = "resolved";
                            break Some(Receipt {
                                cell: pending.target().cell_id(),
                                incarnation: pending.incarnation(),
                                commit_sequence,
                            });
                        }
                        Ok(Resolution::Absent) => {
                            sample.outcome = "absent";
                            sample.elapsed_us = started.elapsed().as_micros() as u64;
                            return sample;
                        }
                        Ok(Resolution::Unknown) | Err(InvocationError::NotStarted(_)) => {
                            assert!(
                                Instant::now() < deadline,
                                "unresolved request {request}: {pending:?}"
                            );
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                        other => panic!("unexpected request resolution {request}: {other:?}"),
                    }
                }
            }
            other => panic!("unexpected mutation outcome {request}: {other:?}"),
        }
    } else {
        sample.outcome = "ok";
        None
    };
    if let Some(receipt) = minimum {
        sample.sequence = receipt.commit_sequence;
    }
    match order.receipt_count(minimum, ()).await {
        Ok(observed) => {
            sample.count = observed.output;
            sample.read_sequence = observed.receipt.commit_sequence;
        }
        Err(InvocationError::NotStarted(error)) => {
            eprintln!("ENTITY_READ_FAILED request={request} error={error}");
            sample.outcome = if sample.write {
                "write_only"
            } else {
                "read_failed"
            };
        }
        other => panic!("unexpected read outcome {request}: {other:?}"),
    }
    sample.elapsed_us = started.elapsed().as_micros() as u64;
    sample
}
