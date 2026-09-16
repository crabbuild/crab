use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crab_storage::{Store, StoreLayout};
use futures_util::{StreamExt as _, stream};
use serde::Serialize;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::server::{Repository, Server};

const SCRUB_INTERVAL: Duration = Duration::from_secs(60 * 60);
const SCRUB_BUDGET: Duration = Duration::from_secs(3 * 60);
const MAX_GIT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const CONCURRENT_REPOSITORIES: usize = 2;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("repository integrity scrub cancelled")]
    Cancelled,
    #[error("repository integrity scrub exceeded its three-minute budget")]
    Timeout,
    #[error("repository integrity scrub failed: {0}")]
    Read(#[from] crab_read::ReadError),
    #[error("repository changed during integrity scrub")]
    Superseded,
}

pub(crate) type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum State {
    Pending,
    Running,
    Complete,
    Failed,
    Superseded,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct CompleteProof {
    pub(crate) state_digest: String,
    pub(crate) completed_at_unix: u64,
    pub(crate) catalog_files: u64,
    pub(crate) catalog_shards: u64,
    pub(crate) catalog_xorbs: u64,
    pub(crate) reachable_crab_pointers: u64,
    pub(crate) reachable_lfs_objects: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Snapshot {
    pub(crate) state: State,
    pub(crate) attempted_at_unix: Option<u64>,
    pub(crate) last_complete: Option<CompleteProof>,
    pub(crate) error: Option<String>,
}

pub(crate) struct Status {
    inner: RwLock<Snapshot>,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            inner: RwLock::new(Snapshot {
                state: State::Pending,
                attempted_at_unix: None,
                last_complete: None,
                error: None,
            }),
        }
    }
}

impl Status {
    pub(crate) fn snapshot(&self) -> Snapshot {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn start(&self) {
        let mut snapshot = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snapshot.state = State::Running;
        snapshot.attempted_at_unix = Some(unix_now());
        snapshot.error = None;
    }

    fn complete(
        &self,
        state_digest: String,
        proof: crab_read::capsule_protocol::CapsuleDependencyProof,
    ) {
        let mut snapshot = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snapshot.state = State::Complete;
        snapshot.last_complete = Some(CompleteProof {
            state_digest,
            completed_at_unix: unix_now(),
            catalog_files: proof.catalog_files,
            catalog_shards: proof.catalog_shards,
            catalog_xorbs: proof.catalog_xorbs,
            reachable_crab_pointers: proof.reachable_crab_pointers,
            reachable_lfs_objects: proof.reachable_lfs_objects,
        });
        snapshot.error = None;
    }

    fn finish(&self, state: State, error: Option<String>) {
        let mut snapshot = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snapshot.state = state;
        snapshot.error = error;
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

async fn prove(
    layout: &StoreLayout<Store>,
    admission: Arc<Semaphore>,
    cancellation: &CancellationToken,
) -> Result<(String, crab_read::capsule_protocol::CapsuleDependencyProof)> {
    let _permit = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(Error::Cancelled),
        permit = admission.acquire_owned() => permit.map_err(|_| Error::Cancelled)?,
    };
    let view = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(Error::Cancelled),
        result = crab_read::capsule_protocol::open_view(
            layout,
            crab_read::capsule_protocol::CapsuleReadLimits {
                max_capsule_bytes: MAX_GIT_BYTES,
                max_frontier_bytes: MAX_GIT_BYTES,
            },
        ) => result?,
    };
    let state_digest = view.state_digest();
    let proof = crab_read::capsule_protocol::verify_reachable_dependencies(
        layout,
        &view,
        crab_read::capsule_protocol::CapsuleDependencyLimits {
            max_git_bytes: MAX_GIT_BYTES,
            pointer_scan: crab_git::walk::PointerScanLimits {
                objects: 2_000_000,
                lookups: 8_000_000,
                allocation_bytes: 64 * 1024 * 1024,
            },
        },
        cancellation,
    )
    .await?;
    let current_root = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(Error::Cancelled),
        result = crab_metadata::capsule_protocol::load_root(layout) => {
            result.map_err(crab_read::ReadError::from)?
        }
    };
    let activity = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(Error::Cancelled),
        result = crab_read::capsule_protocol::read_activity_from_root(layout, &current_root) => {
            result?
        }
    };
    if activity.state_digest() != state_digest {
        return Err(Error::Superseded);
    }
    Ok((state_digest, proof))
}

async fn scrub_target(
    layout: &StoreLayout<Store>,
    status: &Status,
    admission: Arc<Semaphore>,
    parent: &CancellationToken,
) -> Result<()> {
    status.start();
    let cancellation = parent.child_token();
    let operation = prove(layout, admission, &cancellation);
    tokio::pin!(operation);
    let result = tokio::select! {
        biased;
        result = &mut operation => result,
        () = parent.cancelled() => {
            cancellation.cancel();
            let _ = operation.await;
            Err(Error::Cancelled)
        }
        () = tokio::time::sleep(SCRUB_BUDGET) => {
            cancellation.cancel();
            let _ = operation.await;
            Err(Error::Timeout)
        }
    };
    match &result {
        Ok((state_digest, proof)) => status.complete(state_digest.clone(), *proof),
        Err(Error::Superseded) => status.finish(State::Superseded, None),
        Err(Error::Cancelled) => status.finish(State::Cancelled, None),
        Err(error) => status.finish(State::Failed, Some(error.to_string())),
    }
    result.map(|_| ())
}

pub(crate) async fn scrub(
    repository: &Repository,
    admission: Arc<Semaphore>,
    parent: &CancellationToken,
) -> Result<()> {
    scrub_target(&repository.layout, &repository.integrity, admission, parent).await
}

async fn cycle(server: &Arc<Server>) {
    stream::iter(server.repositories.values())
        .for_each_concurrent(CONCURRENT_REPOSITORIES, |repository| {
            let server = Arc::clone(server);
            async move {
                match scrub(
                    &repository,
                    Arc::clone(&server.maintenance_admission),
                    &server.cancellation,
                )
                .await
                {
                    Ok(()) | Err(Error::Cancelled | Error::Superseded) => {}
                    Err(error) => tracing::warn!(
                        repository_id = %repository.id,
                        %error,
                        "repository integrity scrub failed"
                    ),
                }
            }
        })
        .await;
}

pub(crate) async fn run(server: Arc<Server>) {
    loop {
        cycle(&server).await;
        tokio::select! {
            () = server.cancellation.cancelled() => return,
            () = tokio::time::sleep(SCRUB_INTERVAL) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use sha2::Digest as _;

    use super::*;

    #[tokio::test]
    async fn scrub_reports_dependency_loss_without_erasing_the_last_complete_proof() {
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let layout = StoreLayout::new(store.clone(), "integrity".into());
        let content = Bytes::from_static(b"content");
        let oid: [u8; 32] = sha2::Sha256::digest(&content).into();
        let pointer = crab_git::LfsPointer {
            oid,
            size: content.len() as u64,
            extensions: Vec::new(),
        };
        crate::test_git::publish_blob(&layout, &pointer.serialize()).await;
        let lfs = crab_lfs::LfsObjectStore::new(store, layout.repo_prefix());
        lfs.put(&oid, content).await.unwrap();
        let status = Status::default();
        let admission = Arc::new(Semaphore::new(1));
        let cancellation = CancellationToken::new();

        scrub_target(&layout, &status, Arc::clone(&admission), &cancellation)
            .await
            .unwrap();
        let complete = status.snapshot();
        assert_eq!(complete.state, State::Complete);
        assert_eq!(
            complete
                .last_complete
                .as_ref()
                .unwrap()
                .reachable_lfs_objects,
            1
        );

        lfs.delete(&oid).await.unwrap();
        assert!(matches!(
            scrub_target(&layout, &status, admission, &cancellation).await,
            Err(Error::Read(crab_read::ReadError::Lfs(
                crab_lfs::LfsError::ObjectMissing { .. }
            )))
        ));
        let failed = status.snapshot();
        assert_eq!(failed.state, State::Failed);
        assert!(failed.error.is_some());
        assert_eq!(failed.last_complete, complete.last_complete);
    }
}
