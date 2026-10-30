//! Explicit CPU-scaling diagnosis; this is not a RustFS qualification substitute.
use super::tests::{capsule_fixture, coordinated_put, read};
use super::{Coordinator, Metrics};
use crate::attributes::ManifestListingItem;
use std::{
    collections::BTreeSet,
    sync::{Arc, atomic::Ordering},
    time::Instant,
};

const WRITERS: usize = 16;
const ROUNDS: usize = 7680;
fn payload(writer: usize, round: usize) -> Vec<u8> {
    let mut bytes = vec![b'x'; 4096];
    bytes[..8].copy_from_slice(&(writer as u64).to_le_bytes());
    bytes[8..16].copy_from_slice(&(round as u64).to_le_bytes());
    bytes
}
fn key(writer: usize, round: usize) -> String {
    format!("qualification/small-write/profile/writer-{writer:02}/object-{round:08}")
}
fn summary(samples: &[f64]) -> serde_json::Value {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    serde_json::json!({"batches":sorted.len(),"mean_ms":sorted.iter().sum::<f64>()/sorted.len() as f64,"p50_ms":sorted[sorted.len()/2],"p95_ms":sorted[(sorted.len()-1)*95/100],"max_ms":sorted.last()})
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "explicit122880-write in-memory CPU scaling diagnostic; requires CRAB_S3_GATEWAY_PROFILE_ROOT"]
async fn native_namespace_cpu_scaling_at_ci_object_count() {
    let root = std::path::PathBuf::from(
        std::env::var_os("CRAB_S3_GATEWAY_PROFILE_ROOT")
            .expect("explicit private profile root required"),
    );
    assert!(root.is_dir());
    let log = std::fs::File::create(root.join("phases.log")).unwrap();
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_ansi(false)
        .with_target(false)
        .with_writer(std::sync::Mutex::new(log))
        .try_init();
    let (repository, runtime, cancel) = capsule_fixture().await;
    let coordinator = Coordinator::new(
        Arc::clone(&runtime),
        crab_remote_git::RepositoryOptions::default(),
        cancel.child_token(),
        Metrics::new().unwrap(),
    );
    let mut samples = Vec::with_capacity(ROUNDS);
    let started = Instant::now();
    for round in 0..ROUNDS {
        let before = Instant::now();
        let coordinator = &coordinator;
        let repository = &repository;
        let cancel = &cancel;
        futures_util::future::join_all((0..WRITERS).map(|writer| async move {
            coordinated_put(
                coordinator,
                repository,
                cancel,
                &key(writer, round),
                &payload(writer, round),
            )
            .await;
        }))
        .await;
        samples.push(before.elapsed().as_secs_f64() * 1000.0);
        if (round + 1) % 500 == 0 || round + 1 == ROUNDS {
            let row = serde_json::json!({"acknowledged":(round+1)*WRITERS,"elapsed_ms":started.elapsed().as_millis(),"warm_state_bytes":coordinator.admission.warm_bytes.load(Ordering::Acquire),"recent_batches":summary(&samples[samples.len().saturating_sub(500)..])});
            std::fs::write(
                root.join("progress.json"),
                serde_json::to_vec_pretty(&row).unwrap(),
            )
            .unwrap();
            println!("{row}");
        }
    }
    let elapsed = started.elapsed().as_millis();
    coordinator.shutdown().await;
    runtime.shutdown().await;
    let cold = crate::gateway::Repository::new(repository.config.clone(), repository.store.clone())
        .unwrap();
    let view = crab_read::capsule_protocol::open_view(
        &cold.layout,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: super::MAX_CAPSULE_READ_BYTES,
            max_frontier_bytes: super::MAX_CAPSULE_READ_BYTES,
        },
    )
    .await
    .unwrap();
    let tip = view.refs()["refs/heads/main"]
        .parse::<gix_hash::ObjectId>()
        .unwrap();
    let manifest = cold.attributes.load(tip).await.unwrap();
    let mut after = None;
    let mut paths = BTreeSet::new();
    loop {
        let page = manifest
            .complete_listing(
                "qualification/small-write/profile/",
                after.as_deref(),
                None,
                1000,
            )
            .unwrap();
        for item in page.items {
            let ManifestListingItem::Object { path, attributes } = item else {
                panic!("no delimiter means exact objects")
            };
            assert_eq!(attributes.size, 4096);
            assert!(paths.insert(path.clone()));
            after = Some(path);
        }
        if !page.has_more {
            break;
        }
    }
    let expected = (0..ROUNDS)
        .flat_map(|round| (0..WRITERS).map(move |writer| key(writer, round)))
        .collect::<BTreeSet<_>>();
    assert_eq!(paths, expected);
    let cold_runtime = Arc::new(crab_remote_git::RemoteGitRuntime::default());
    for writer in 0..WRITERS {
        for round in [0, ROUNDS - 1] {
            assert_eq!(
                read(
                    &cold,
                    Arc::clone(&cold_runtime),
                    &cancel,
                    &key(writer, round)
                )
                .await
                .as_ref(),
                payload(writer, round)
            );
        }
    }
    cold_runtime.shutdown().await;
    let cohort = samples.len() / 3;
    let evidence = serde_json::json!({"schema":"crab.native-gateway-cpu-scaling-diagnostic","status":"passed","provider":"in-memory","writers":WRITERS,"object_bytes":4096,"acknowledged":WRITERS*ROUNDS,"batches":ROUNDS,"elapsed_ms":elapsed,"early":summary(&samples[..cohort]),"late":summary(&samples[samples.len()-cohort..]),"all_batches_ms":samples,"checks":["all ordered gateway mutations acknowledged","exact full cold attribute namespace including every key","cold first and last payload for all16 writers"],"scope":"CPU scaling diagnosis at CI object count using the real coordinator with an in-memory backend. No HTTP/RustFS throughput or production gate success is claimed."});
    std::fs::write(
        root.join("report.json"),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
}
