//! Shared measurements for qualification binaries, including locator-internal I/O.
use std::error::Error;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crab_remote_git::{
    RemoteGitRepository, RemoteGitRuntime, RepositoryIdentity, RepositoryOptions,
};
use crab_storage::{
    StorageObservation, StorageObserver, StorageOperation, StorageOutcome, Store, StoreLayout,
};
use serde_json::json;
use sha1::{Digest, Sha1};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::prelude::*;

pub type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Default)]
pub struct Measurements {
    requests: [AtomicU64; 11],
    outcomes: [AtomicU64; 9],
    read_bytes: AtomicU64,
    written_bytes: AtomicU64,
    active: AtomicU64,
    decoded_blocks: [AtomicU64; 4],
    decoded_bytes: [AtomicU64; 4],
}

#[derive(Clone, Copy)]
struct Snapshot {
    requests: [u64; 11],
    outcomes: [u64; 9],
    read_bytes: u64,
    written_bytes: u64,
    decoded_blocks: [u64; 4],
    decoded_bytes: [u64; 4],
}

impl StorageObserver for Measurements {
    fn started(&self, operation: StorageOperation) {
        self.requests[operation.index()].fetch_add(1, Ordering::Relaxed);
        self.active.fetch_add(1, Ordering::Relaxed);
    }

    fn finished(&self, observation: StorageObservation) {
        self.outcomes[observation.outcome.index()].fetch_add(1, Ordering::Relaxed);
        self.read_bytes
            .fetch_add(observation.bytes_read, Ordering::Relaxed);
        self.written_bytes
            .fetch_add(observation.bytes_written, Ordering::Relaxed);
        self.active.fetch_sub(1, Ordering::Relaxed);
    }
}

struct DecodeLayer(Arc<Measurements>);

#[derive(Default)]
struct DecodeEvent {
    format: Option<usize>,
    bytes: u64,
}

impl tracing::field::Visit for DecodeEvent {
    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "index_format" {
            self.format = match value {
                "commit_graph" => Some(0),
                "path_state" => Some(1),
                "git_object_index" => Some(2),
                "git_visibility" => Some(3),
                _ => None,
            };
        }
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        if field.name() == "decoded_bytes" {
            self.bytes = value;
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for DecodeLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() != "crab_metadata::index_decode" {
            return;
        }
        let mut decoded = DecodeEvent::default();
        event.record(&mut decoded);
        if let Some(format) = decoded.format {
            self.0.decoded_blocks[format].fetch_add(1, Ordering::Relaxed);
            self.0.decoded_bytes[format].fetch_add(decoded.bytes, Ordering::Relaxed);
        }
    }
}

impl Measurements {
    pub fn install(self: &Arc<Self>) -> Result<()> {
        tracing_subscriber::registry()
            .with(DecodeLayer(self.clone()))
            .try_init()?;
        Ok(())
    }
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            requests: self
                .requests
                .each_ref()
                .map(|counter| counter.load(Ordering::Relaxed)),
            outcomes: self
                .outcomes
                .each_ref()
                .map(|counter| counter.load(Ordering::Relaxed)),
            read_bytes: self.read_bytes.load(Ordering::Relaxed),
            written_bytes: self.written_bytes.load(Ordering::Relaxed),
            decoded_blocks: self
                .decoded_blocks
                .each_ref()
                .map(|counter| counter.load(Ordering::Relaxed)),
            decoded_bytes: self
                .decoded_bytes
                .each_ref()
                .map(|counter| counter.load(Ordering::Relaxed)),
        }
    }

    pub async fn measure<T>(
        &self,
        name: &str,
        cache: &str,
        future: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        let before = self.snapshot();
        let started = Instant::now();
        let result = future.await;
        let after = self.snapshot();
        let operations = StorageOperation::ALL
            .into_iter()
            .map(|operation| {
                (
                    operation.label().to_owned(),
                    json!(
                        after.requests[operation.index()]
                            .saturating_sub(before.requests[operation.index()])
                    ),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        let outcomes = StorageOutcome::ALL
            .into_iter()
            .map(|outcome| {
                (
                    outcome.label().to_owned(),
                    json!(
                        after.outcomes[outcome.index()]
                            .saturating_sub(before.outcomes[outcome.index()])
                    ),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        let decoded = [
            "commit_graph",
            "path_state",
            "git_object_index",
            "git_visibility",
        ]
        .into_iter()
        .enumerate()
        .map(|(index, name)| {
            let blocks = after.decoded_blocks[index].saturating_sub(before.decoded_blocks[index]);
            let bytes = after.decoded_bytes[index].saturating_sub(before.decoded_bytes[index]);
            (name.to_owned(), json!({"blocks":blocks, "bytes":bytes}))
        })
        .collect::<serde_json::Map<_, _>>();
        println!(
            "{}",
            json!({
                "kind": "measurement", "schema": 1, "operation": name, "cache": cache,
                "outcome": if result.is_ok() { "ok" } else { "error" },
                "elapsed_us": started.elapsed().as_micros(), "backend_operations": operations,
                "backend_outcomes": outcomes,
                "backend_read_bytes": after.read_bytes.saturating_sub(before.read_bytes),
                "backend_written_bytes": after.written_bytes.saturating_sub(before.written_bytes),
                "active_backend_operations": self.active.load(Ordering::Relaxed),
                "decoded_index_blocks": decoded,
            })
        );
        result
    }
}

// Qualification uses the same capsule opening path as the repository service.
// A legacy manifest open would measure different visibility and index behavior.
pub async fn open_repository(
    layout: StoreLayout<Store>,
    identity: RepositoryIdentity,
    runtime: Arc<RemoteGitRuntime>,
    options: RepositoryOptions,
    cancel: &CancellationToken,
) -> Result<RemoteGitRepository> {
    let root = crab_metadata::capsule_protocol::load_root(&layout).await?;
    let indexes = crab_metadata::capsule_protocol::load_browse_indexes(&layout).await?;
    let view = crab_read::capsule_protocol::open_view_from_root_with_metadata(
        &layout,
        root,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 2 * 1024 * 1024 * 1024,
            max_frontier_bytes: 2 * 1024 * 1024 * 1024,
        },
    )
    .await?
    .with_browse_indexes(indexes);
    Ok(view
        .git_repository_from_store(
            layout,
            identity,
            runtime,
            options,
            2 * 1024 * 1024 * 1024,
            cancel,
        )
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use object_store::ObjectStoreExt;
    use object_store::memory::InMemory;
    use object_store::path::Path;

    #[test]
    fn decoder_counts_ignore_unrelated_events() {
        let counters = Arc::new(Measurements::default());
        let subscriber = tracing_subscriber::registry().with(DecodeLayer(counters.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(target:"crab_metadata::index_decode", index_format="commit_graph", decoded_bytes=41_u64);
            tracing::debug!(target:"crab_metadata::index_decode", index_format="path_state", decoded_bytes=123_u64);
            tracing::debug!(target:"crab_metadata::index_decode", index_format="git_object_index", decoded_bytes=67_u64);
            tracing::debug!(target:"crab_metadata::index_decode", index_format="git_visibility", decoded_bytes=89_u64);
            tracing::debug!(target:"unrelated", index_format="path_state", decoded_bytes=999_u64);
        });
        let snapshot = counters.snapshot();
        assert_eq!(
            (snapshot.decoded_blocks, snapshot.decoded_bytes),
            ([1, 1, 1, 1], [41, 123, 67, 89])
        );
    }

    #[tokio::test]
    async fn observation_includes_reads_through_inner_store_and_missing_objects() {
        let counters = Arc::new(Measurements::default());
        let store = Store::new(Arc::new(InMemory::new())).with_storage_observer(counters.clone());
        store
            .put(&Path::from("body"), Bytes::from_static(b"payload"))
            .await
            .unwrap();
        let before = counters.snapshot();
        // SlateDB uses inner(), bypassing canonical Store read callbacks.
        let body = store
            .inner()
            .get(&Path::from("body"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(body.len(), 7);
        assert!(store.inner().get(&Path::from("missing")).await.is_err());
        let after = counters.snapshot();
        assert_eq!(
            after.requests[StorageOperation::Get.index()]
                - before.requests[StorageOperation::Get.index()],
            2
        );
        assert_eq!(after.read_bytes - before.read_bytes, 7);
        assert_eq!(after.outcomes[StorageOutcome::NotFound.index()], 1);
        assert_eq!(counters.active.load(Ordering::Relaxed), 0);
    }
}

pub fn object_oid(kind: &str, data: &[u8]) -> String {
    let mut hash = Sha1::new();
    hash.update(format!("{kind} {}\0", data.len()));
    hash.update(data);
    format!("{:x}", hash.finalize())
}
