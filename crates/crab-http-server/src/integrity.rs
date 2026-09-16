use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use crab_storage::{ETag, Store, StoreLayout};
use futures_util::{StreamExt as _, stream};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::server::{Repository, Server};

const SCRUB_INTERVAL: Duration = Duration::from_secs(60 * 60);
const REPORT_RETRY_INTERVAL: Duration = Duration::from_secs(30);
const SCRUB_BUDGET: Duration = Duration::from_secs(3 * 60);
const MAX_GIT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const CONCURRENT_REPOSITORIES: usize = 2;
const REPORT_SCHEMA_VERSION: u32 = 1;
const MAX_REPORT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_REPOSITORIES: usize = 10_000;
const MAX_ERROR_BYTES: usize = 4 * 1024;
const MAX_CLOCK_SKEW: Duration = Duration::from_secs(5 * 60);
const SCRUB_RESOURCE: &str = "http-server-integrity-scrub";
const REPORT_RELATIVE_PATH: &str = ".crab/http-server/v1/integrity.json";

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
    #[error("repository integrity coordination failed")]
    Coordination(#[from] crab_coordination::CoordinationError),
    #[error("repository integrity report storage failed")]
    Storage(#[from] crab_storage::StorageError),
    #[error("repository integrity report encoding failed")]
    Json(#[from] serde_json::Error),
    #[error("repository integrity report is invalid: {0}")]
    InvalidReport(&'static str),
}

pub(crate) type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum State {
    Pending,
    Running,
    Complete,
    Failed,
    Superseded,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompleteProof {
    pub(crate) state_digest: String,
    pub(crate) completed_at_unix: u64,
    pub(crate) catalog_files: u64,
    pub(crate) catalog_shards: u64,
    pub(crate) catalog_xorbs: u64,
    pub(crate) reachable_crab_pointers: u64,
    pub(crate) reachable_lfs_objects: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Snapshot {
    pub(crate) state: State,
    pub(crate) attempted_at_unix: Option<u64>,
    pub(crate) last_complete: Option<CompleteProof>,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DeploymentReport {
    schema_version: u32,
    generated_at_unix: u64,
    repositories: Vec<RepositoryReport>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RepositoryReport {
    id: Uuid,
    placement_generation: u64,
    proof: Snapshot,
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
        snapshot.error = error.map(|error| bounded_error(&error));
    }

    fn replace(&self, snapshot: Snapshot) {
        *self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot;
    }
}

fn bounded_error(error: &str) -> String {
    if error.len() <= MAX_ERROR_BYTES {
        return error.to_owned();
    }
    let mut end = MAX_ERROR_BYTES;
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    error[..end].to_owned()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn validate_snapshot(snapshot: &Snapshot, maximum_time: u64) -> Result<()> {
    if !snapshot
        .attempted_at_unix
        .is_some_and(|attempted| attempted <= maximum_time)
        || snapshot
            .error
            .as_ref()
            .is_some_and(|error| error.len() > MAX_ERROR_BYTES)
        || snapshot.last_complete.as_ref().is_some_and(|proof| {
            !valid_digest(&proof.state_digest) || proof.completed_at_unix > maximum_time
        })
    {
        return Err(Error::InvalidReport("malformed proof fields"));
    }
    let valid = match snapshot.state {
        State::Complete => snapshot.last_complete.is_some() && snapshot.error.is_none(),
        State::Failed => snapshot.error.is_some(),
        State::Superseded => snapshot.error.is_none(),
        State::Pending | State::Running | State::Cancelled => false,
    };
    if !valid {
        return Err(Error::InvalidReport("unsupported durable proof state"));
    }
    Ok(())
}

impl DeploymentReport {
    fn capture(repositories: &[Arc<Repository>]) -> Result<Self> {
        let report = Self {
            schema_version: REPORT_SCHEMA_VERSION,
            generated_at_unix: unix_now(),
            repositories: repositories
                .iter()
                .map(|repository| RepositoryReport {
                    id: repository.id,
                    placement_generation: repository.identity.placement_generation(),
                    proof: repository.integrity.snapshot(),
                })
                .collect(),
        };
        report.validate()?;
        Ok(report)
    }

    fn validate(&self) -> Result<()> {
        if self.schema_version != REPORT_SCHEMA_VERSION
            || self.repositories.len() > MAX_REPOSITORIES
        {
            return Err(Error::InvalidReport(
                "unsupported schema or repository count",
            ));
        }
        let maximum_time = unix_now().saturating_add(MAX_CLOCK_SKEW.as_secs());
        if self.generated_at_unix > maximum_time {
            return Err(Error::InvalidReport("report timestamp is in the future"));
        }
        let mut identities = HashSet::new();
        for repository in &self.repositories {
            if repository.placement_generation == 0 || !identities.insert(repository.id) {
                return Err(Error::InvalidReport("repository identities are invalid"));
            }
            validate_snapshot(&repository.proof, maximum_time)?;
        }
        Ok(())
    }

    fn is_fresh(&self) -> bool {
        unix_now().saturating_sub(self.generated_at_unix) < SCRUB_INTERVAL.as_secs()
            && !self.needs_retry()
    }

    fn remaining_lifetime(&self) -> Duration {
        SCRUB_INTERVAL.saturating_sub(Duration::from_secs(
            unix_now().saturating_sub(self.generated_at_unix),
        ))
    }

    fn needs_retry(&self) -> bool {
        self.repositories
            .iter()
            .any(|repository| repository.proof.state == State::Superseded)
    }

    fn covers(&self, server: &Server) -> bool {
        self.repositories.len() == server.repositories.len()
            && self.repositories.iter().all(|entry| {
                server
                    .repositories
                    .by_id(entry.id)
                    .is_some_and(|repository| {
                        repository.identity.placement_generation() == entry.placement_generation
                    })
            })
    }

    fn apply(&self, server: &Server) {
        for entry in &self.repositories {
            if let Some(repository) = server.repositories.by_id(entry.id)
                && repository.identity.placement_generation() == entry.placement_generation
            {
                repository.integrity.replace(entry.proof.clone());
            }
        }
    }
}

async fn read_report(root: &crate::storage_root::StorageRoot) -> Result<Option<(Bytes, ETag)>> {
    let path = root.path(REPORT_RELATIVE_PATH);
    match root
        .store
        .get_with_etag_bounded(&path, MAX_REPORT_BYTES)
        .await
    {
        Ok(value) => Ok(Some(value)),
        Err(crab_storage::StorageError::NotFound { .. }) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn decode_report(body: &[u8]) -> Result<DeploymentReport> {
    let report: DeploymentReport = serde_json::from_slice(body)?;
    report.validate()?;
    Ok(report)
}

#[cfg(test)]
async fn load_report(root: &crate::storage_root::StorageRoot) -> Result<Option<DeploymentReport>> {
    read_report(root)
        .await?
        .map(|(body, _)| decode_report(&body))
        .transpose()
}

async fn store_report(
    root: &crate::storage_root::StorageRoot,
    report: &DeploymentReport,
    expected: Option<ETag>,
) -> Result<()> {
    let body = serde_json::to_vec(report)?;
    if body.len() as u64 > MAX_REPORT_BYTES {
        return Err(Error::InvalidReport(
            "encoded report exceeds its size limit",
        ));
    }
    let path = root.path(REPORT_RELATIVE_PATH);
    match expected {
        Some(etag) => {
            root.store.update(&path, Bytes::from(body), etag).await?;
        }
        None => {
            root.store.create_strict(&path, Bytes::from(body)).await?;
        }
    }
    Ok(())
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

async fn scrub_cycle(
    server: &Arc<Server>,
    repositories: &[Arc<Repository>],
    cancellation: &CancellationToken,
) -> Result<()> {
    stream::iter(repositories.iter().cloned())
        .for_each_concurrent(CONCURRENT_REPOSITORIES, |repository| {
            let server = Arc::clone(server);
            let cancellation = cancellation.clone();
            async move {
                match scrub(
                    &repository,
                    Arc::clone(&server.maintenance_admission),
                    &cancellation,
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
    if cancellation.is_cancelled() {
        return Err(Error::Cancelled);
    }
    Ok(())
}

async fn coordinated_cycle(
    server: &Arc<Server>,
    root: &crate::storage_root::StorageRoot,
    locks: &mut crab_coordination::PushLockAcquireContext,
) -> Result<Duration> {
    let stored = read_report(root).await?;
    let expected = stored.as_ref().map(|(_, etag)| etag.clone());
    let previous = match stored.as_ref().map(|(body, _)| decode_report(body)) {
        Some(Ok(report)) => Some(report),
        Some(Err(Error::Json(_) | Error::InvalidReport(_))) => {
            tracing::warn!("ignoring an invalid repository integrity report");
            None
        }
        Some(Err(error)) => return Err(error),
        None => None,
    };
    if let Some(report) = &previous
        && report.covers(server)
    {
        report.apply(server);
        if report.is_fresh() {
            return Ok(report.remaining_lifetime());
        }
    }
    let lock = match locks
        .try_acquire_internal(
            &root.prefix,
            SCRUB_RESOURCE,
            crab_coordination::DEFAULT_PUSH_LOCK_TTL,
        )
        .await
    {
        Ok(lock) => lock,
        Err(crab_coordination::CoordinationError::PushLockHeld { .. }) => {
            return Ok(REPORT_RETRY_INTERVAL);
        }
        Err(error) => return Err(error.into()),
    };
    let cancellation = server.cancellation.child_token();
    let lease = crab_coordination::RenewingPushLock::start(lock, &cancellation);
    let repositories = server.repositories.values();
    let result = async {
        scrub_cycle(server, &repositories, &cancellation).await?;
        let report = DeploymentReport::capture(&repositories)?;
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        store_report(root, &report, expected).await?;
        report.apply(server);
        Ok(if report.needs_retry() {
            REPORT_RETRY_INTERVAL
        } else {
            SCRUB_INTERVAL
        })
    }
    .await;
    lease.release().await;
    result
}

pub(crate) async fn run(server: Arc<Server>) {
    let Some(catalog) = server.catalog.as_ref() else {
        return;
    };
    let root = catalog.root().clone();
    let mut locks = crab_coordination::PushLockAcquireContext::new(root.store.inner().clone());
    loop {
        let interval = match coordinated_cycle(&server, &root, &mut locks).await {
            Ok(interval) => interval,
            Err(Error::Cancelled) => return,
            Err(error) => {
                tracing::warn!(%error, "repository integrity scheduler failed");
                REPORT_RETRY_INTERVAL
            }
        };
        tokio::select! {
            () = server.cancellation.cancelled() => return,
            () = tokio::time::sleep(interval) => {}
        }
    }
}

#[cfg(test)]
#[path = "integrity_tests.rs"]
mod tests;
