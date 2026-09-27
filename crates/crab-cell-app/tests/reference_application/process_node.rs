//! Public host and authoritative session lifecycle for the process fixture.

use super::performance_fixture::{DurabilityRecorder, node_session, now_ms};
use crate::*;
use crab_cell_host::{CellNode, CellNodeBuilder};
use crab_cell_runtime::node::lease::NodeLeaseGuard;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub(super) fn directory(layout: &CellStorageLayout, registry: &Registry) -> NodeDirectory {
    NodeDirectory::new(
        layout.clone(),
        Digest::from_bytes([90; 32]),
        Digest::from_bytes([91; 32]),
        registry.release_digest(),
    )
}

pub(super) async fn start(
    node: usize,
    application: Arc<crab_cell_app::CompiledApplication>,
    layout: &CellStorageLayout,
    root: &std::path::Path,
) -> (
    CellNode,
    Arc<DurabilityRecorder>,
    crab_cell_host::read_replicas::ReadReplicaManager,
) {
    let registry = application.registry();
    // Keep writer admission at 32 Cells while charging both the old and new
    // immutable snapshots during a refresh under the same node ledger.
    let pool = SqlWorkerPool::new(4, 32)
        .unwrap()
        .with_native_memory_limit(32 << 20)
        .unwrap();
    let host = CellNodeBuilder::new(application)
        .with_runtime(pool, 64 * 1024 * 1024)
        .with_session(node_session(node))
        .with_replica_host(reference_host())
        .build()
        .unwrap();
    let durability = Arc::new(DurabilityRecorder::default());
    host.install_telemetry(durability.clone()).unwrap();
    let directory = directory(layout, &registry);
    let signer = SigningKey::from_bytes(&[93; 32]);
    let advertisement = move |now: i64, progress| {
        NodeAdvertisement::sign(
            NodeId::from_bytes(*node_session(node).as_bytes()),
            node_session(node),
            format!("https://reference-{node}.internal:8081"),
            Digest::from_bytes([90; 32]),
            Digest::from_bytes([94; 32]),
            Digest::from_bytes([91; 32]),
            registry.release_digest(),
            &signer,
            progress,
            now,
            now + 15_000,
            registry.module_digests(),
            vec![1],
            NodeFailureDomain::default(),
            NodeCapacity {
                free_memory_bytes: 64 * 1024 * 1024,
                free_disk_bytes: 1 << 30,
                job_credits: 32,
                ..NodeCapacity::default()
            },
        )
    };
    let now = now_ms();
    let mut observed = directory
        .create(advertisement(now, 1).unwrap(), now)
        .await
        .unwrap();
    let lease = NodeLeaseGuard::new(now_ms(), observed.advertisement().expires_at_ms()).unwrap();
    let shutdown = CancellationToken::new();
    let tasks = host
        .install_task_group(CancellationToken::new(), shutdown.clone())
        .unwrap();
    host.install_node_lease_for_startup(lease.clone()).unwrap();
    let (renewed, first_renewal) = tokio::sync::oneshot::channel();
    tasks
        .spawn_lease_maintenance(async move {
            let mut renewed = Some(renewed);
            let result: Result<()> = async {
                loop {
                    tokio::select! {
                        () = shutdown.cancelled() => break,
                        () = tokio::time::sleep(Duration::from_secs(5)) => {}
                    }
                    let now = now_ms();
                    let next = advertisement(now, observed.advertisement().progress() + 1)?;
                    // Finish the conditional write before observing shutdown so
                    // withdrawal always uses the latest acknowledged generation.
                    observed = tokio::time::timeout(
                        lease.remaining(),
                        directory.refresh(&observed, next, now),
                    )
                    .await
                    .map_err(|_| Error::Deadline)??;
                    lease.renew(now_ms(), observed.advertisement().expires_at_ms())?;
                    if let Some(renewed) = renewed.take() {
                        let _ = renewed.send(());
                    }
                }
                tokio::time::timeout(
                    Duration::from_secs(20),
                    directory.withdraw(&observed, now_ms()),
                )
                .await
                .map_err(|_| Error::Deadline)??;
                println!(
                    "PERF node_{node}_session_withdrawn: generation={}",
                    observed.advertisement().generation()
                );
                Ok(())
            }
            .await;
            lease.fence();
            result
        })
        .unwrap();
    // Even a short smoke must exercise provider-backed renewal before it can
    // report readiness; successful drain then proves withdrawal of that version.
    first_renewal.await.unwrap();
    let readers = host
        .install_read_replicas(
            layout.clone(),
            self::directory(layout, &host.application().registry()),
            root.join("readers"),
            Limits::default(),
        )
        .unwrap();
    host.start().unwrap();
    (host, durability, readers)
}
