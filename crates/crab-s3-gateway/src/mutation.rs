use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use crab_coordination::{GcFenceHeartbeat, GcFenceLease, PushLock};
use crab_metadata::{git_visibility, manifests::PackManifestEntry, ref_journal::RefJournalEdit};
#[cfg(test)]
use crab_remote_git::RemoteGitRepository;
use crab_remote_git::{EntryMode, OperationKind};
use gix_hash::ObjectId;
use gix_object::{Kind, bstr::BString, tree};
use md5::Digest as _;
use tokio::sync::{Notify, Semaphore, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{
    attributes,
    gateway::Repository,
    metrics::{Metrics, ScratchFailure, ScratchPurpose},
};

const LOCK_TTL: Duration = Duration::from_secs(300);
const REF_LOCK_TTL: Duration = Duration::from_secs(30);
const MAX_GENERATED_PACK_BYTES: u64 = 512 * 1024 * 1024;
const GENERATED_PACK_SINGLE_PUT_MAX_BYTES: u64 = 8 * 1024 * 1024;
const PACK_SCRATCH_BASE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_QUEUED_WRITES_PER_REF: usize = 64;
const MAX_TRACKED_REFS: usize = 256;
const MAX_WARM_BRANCH_STATE_BYTES: usize = 128 * 1024 * 1024;
const WARM_BRANCH_STATE_EVICTION_BYTES: usize = MAX_WARM_BRANCH_STATE_BYTES * 3 / 4;
const ATTRIBUTE_CHECKPOINT_BATCHES: usize = 64;
const WRITE_QUEUE_TIMEOUT: Duration = Duration::from_secs(60);
const BATCH_COLLECTION_DELAY: Duration = Duration::from_millis(10);
const MAX_MUTATIONS_PER_BATCH: usize = 32;
const MAX_BATCHES_PER_FENCE_BURST: usize = 8;
const MAX_MUTATION_BATCH_BYTES: usize = 32 * 1024 * 1024;
const MAX_REPREPARE_ATTEMPTS: usize = 8;
const TREE_PAGE_SIZE: usize = 4_096;
const CHECKPOINT_PUBLICATION_CONCURRENCY: usize = 4;
const GENERATED_TREE_DELTA_DEPTH: u32 = 8;
const MAX_GENERATED_TREE_DELTA_BYTES: usize = 32 * 1024 * 1024;

pub(crate) type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("object path resolves through a non-directory")]
    NotDirectory,
    #[error("object path names a directory")]
    IsDirectory,
    #[error("the nearest .gitattributes entry cannot carry an LFS tracking rule")]
    InvalidAttributes,
    #[error("object write precondition failed")]
    PreconditionFailed,
    #[error("conditional delete target does not exist")]
    ConditionalTargetMissing,
    #[error("repository mutation was cancelled")]
    Cancelled,
    #[error("repository write queue is full")]
    Overloaded,
    #[error("repository write queue wait timed out")]
    AdmissionTimeout,
    #[error("repository write admission state is unavailable")]
    AdmissionState,
    #[error("warm branch state does not cover the requested Git path")]
    WarmStateMiss,
    #[error("batched repository mutation failed")]
    Batch(#[source] Arc<Error>),
    #[error("system clock is before the Unix epoch")]
    Clock(#[from] std::time::SystemTimeError),
    #[error("repository read failed")]
    Remote(#[from] crab_remote_git::Error),
    #[error("Git object encoding failed")]
    Object(#[from] gix_object::encode::Error),
    #[error("Git object hashing failed")]
    Hash(#[from] gix_hash::hasher::Error),
    #[error("temporary pack I/O failed")]
    Io(#[from] std::io::Error),
    #[error("temporary pack capacity is unavailable")]
    Capacity(#[from] crate::metrics::ScratchCapacityError),
    #[error("generated pack validation failed")]
    Pack(#[from] crab_git::incoming_pack::IncomingPackError),
    #[error("generated pack preparation failed")]
    Prepare(#[from] crab_git::incoming_pack::PreparePackError),
    #[error("repository storage failed")]
    Storage(#[from] crab_storage::StorageError),
    #[error("repository metadata failed")]
    Metadata(#[from] crab_metadata::error::MetadataError),
    #[error("repository coordination failed")]
    Coordination(#[from] crab_coordination::CoordinationError),
    #[error("repository publication admission failed")]
    Publication(#[from] crab_remote::publication::Error),
    #[error("repository publication failed")]
    Write(#[from] crab_write::WriteError),
    #[error("mutation worker failed")]
    Worker(#[from] tokio::task::JoinError),
    #[error("S3 attribute persistence failed")]
    Attributes(#[source] Box<crate::Error>),
}

impl From<crate::Error> for Error {
    fn from(error: crate::Error) -> Self {
        match error {
            crate::Error::Remote(source) => Self::Remote(source),
            crate::Error::Metadata(source) => Self::Metadata(source),
            crate::Error::Write(source) => Self::Write(source),
            error => Self::Attributes(Box::new(error)),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Change {
    Put {
        bytes: Bytes,
        // Large S3 bodies are committed as LFS pointers. The same commit must
        // teach Git how to materialize that pointer for ordinary clones.
        track_lfs: bool,
        attributes: Box<attributes::PutAttributes>,
        condition: PutCondition,
    },
    Attributes {
        expected: ObjectId,
        attributes: Box<attributes::PutAttributes>,
    },
    Delete {
        condition: DeleteCondition,
    },
}

#[derive(Clone, Debug, Default)]
pub(crate) enum PutCondition {
    #[default]
    None,
    IfNoneMatchAny,
    IfMatch {
        object: ObjectId,
        etag: String,
        attributes_present: bool,
    },
}

#[derive(Clone, Debug, Default)]
pub(crate) enum DeleteCondition {
    #[default]
    None,
    IfMatchAny {
        missing: MissingDeleteResult,
    },
    IfMatch {
        object: ObjectId,
        etag: String,
        attributes_present: bool,
        missing: MissingDeleteResult,
    },
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum MissingDeleteResult {
    PreconditionFailed,
    NotFound,
}

#[derive(Clone, Debug)]
pub(crate) struct Outcome {
    pub(crate) etag: Option<String>,
}

pub(crate) struct Coordinator {
    runtime: Arc<crab_remote_git::RemoteGitRuntime>,
    options: crab_remote_git::RepositoryOptions,
    admission: WriteAdmission,
    checkpoints: Arc<CheckpointScheduler>,
    metrics: Metrics,
}

struct BatchContext {
    runtime: Arc<crab_remote_git::RemoteGitRuntime>,
    options: crab_remote_git::RepositoryOptions,
    checkpoints: Arc<CheckpointScheduler>,
    metrics: Metrics,
}

impl Coordinator {
    pub(crate) fn new(
        runtime: Arc<crab_remote_git::RemoteGitRuntime>,
        options: crab_remote_git::RepositoryOptions,
        cancellation: CancellationToken,
        metrics: Metrics,
    ) -> Self {
        Self {
            runtime,
            options,
            admission: WriteAdmission::default(),
            checkpoints: Arc::new(CheckpointScheduler::new(cancellation, metrics.clone())),
            metrics,
        }
    }

    pub(crate) async fn shutdown(&self) {
        self.checkpoints.shutdown().await;
    }

    pub(crate) async fn apply(
        &self,
        repository: &Repository,
        branch: &str,
        path: &crab_remote_git::GitPath,
        change: Change,
        principal: &str,
        cancel: &CancellationToken,
    ) -> Result<Outcome> {
        async {
            let mutation = Mutation {
                path: path.clone(),
                change,
                principal: principal.to_owned(),
            };
            let mut queued = self
                .admission
                .enqueue(&repository.config.name, branch, mutation)?;
            let deadline = tokio::time::Instant::now() + WRITE_QUEUE_TIMEOUT;
            loop {
                let permit = tokio::select! {
                    result = &mut queued.result => {
                        break result.map_err(|_| Error::AdmissionState)?;
                    }
                    () = cancel.cancelled() => break Err(Error::Cancelled),
                    () = tokio::time::sleep_until(deadline) => break Err(Error::AdmissionTimeout),
                    permit = Arc::clone(&queued.queue.gate).acquire_owned() => {
                        permit.map_err(|_| Error::AdmissionState)?
                    }
                };
                match queued.result.try_recv() {
                    Ok(result) => break result,
                    Err(oneshot::error::TryRecvError::Closed) => {
                        break Err(Error::AdmissionState);
                    }
                    Err(oneshot::error::TryRecvError::Empty) => {}
                }
                tokio::select! {
                    () = cancel.cancelled() => break Err(Error::Cancelled),
                    () = tokio::time::sleep_until(deadline) => break Err(Error::AdmissionTimeout),
                    () = tokio::time::sleep(BATCH_COLLECTION_DELAY) => {}
                }
                let batch = queued.queue.take_batch()?;
                if batch.is_empty() {
                    drop(permit);
                    continue;
                }
                let repository = Repository::clone(repository);
                repository.maintenance.begin();
                let context = BatchContext {
                    runtime: Arc::clone(&self.runtime),
                    options: self.options,
                    metrics: self.metrics.clone(),
                    checkpoints: Arc::clone(&self.checkpoints),
                };
                let branch = branch.to_owned();
                let queue = Arc::clone(&queued.queue);
                let worker_cancel = cancel.clone();
                // A drained batch owns every accepted mutation. Detaching it
                // prevents one disconnected HTTP caller from cancelling peers.
                tokio::spawn(async move {
                    Self::apply_batch(&repository, context, &branch, &queue, batch, &worker_cancel)
                        .await;
                    drop(permit);
                });
            }
        }
        .await
    }

    pub(crate) fn manifest(
        &self,
        repository: &str,
        branch: &str,
        tip: ObjectId,
    ) -> Option<Arc<attributes::Manifest>> {
        self.admission.manifest(repository, branch, Some(tip))
    }

    pub(crate) async fn complete_manifest(
        &self,
        repository: &Repository,
        branch: &str,
        tip: ObjectId,
    ) -> crate::Result<Option<Arc<attributes::Manifest>>> {
        if let Some(manifest) = self.manifest(&repository.config.name, branch, tip)
            && manifest.is_complete()
        {
            return Ok(Some(manifest));
        }
        if let Some(manifest) =
            self.admission
                .complete_manifest(&repository.config.name, branch, tip)
        {
            return Ok(Some(manifest));
        }
        let manifest = attributes::load(repository, tip).await?;
        if !manifest.is_complete() {
            return Ok(None);
        }
        let manifest = Arc::new(manifest);
        self.admission.store_complete_manifest(
            &repository.config.name,
            branch,
            tip,
            Arc::clone(&manifest),
        );
        Ok(Some(manifest))
    }

    async fn apply_batch(
        repository: &Repository,
        context: BatchContext,
        branch: &str,
        queue: &RefQueue,
        mut batch: Vec<PendingMutation>,
        cancel: &CancellationToken,
    ) {
        let mut deferred_planned = None;
        if batch
            .first()
            .is_some_and(|pending| pending.mutation.is_planned())
        {
            Self::apply_one_batch(
                repository,
                &context,
                branch,
                queue,
                batch,
                FenceUse::Acquire,
                cancel,
            )
            .await;
        } else {
            match GcWriterFences::acquire(repository, cancel).await {
                Ok(fences) => {
                    let mut burst_batches = 0usize;
                    for index in 0..MAX_BATCHES_PER_FENCE_BURST {
                        Self::apply_one_batch(
                            repository,
                            &context,
                            branch,
                            queue,
                            batch,
                            FenceUse::Held,
                            cancel,
                        )
                        .await;
                        burst_batches += 1;
                        if index + 1 == MAX_BATCHES_PER_FENCE_BURST {
                            break;
                        }
                        tokio::select! {
                            () = cancel.cancelled() => break,
                            () = tokio::time::sleep(BATCH_COLLECTION_DELAY) => {}
                        }
                        batch = match queue.take_batch() {
                            Ok(batch) if batch.is_empty() => break,
                            Ok(batch) => batch,
                            Err(error) => {
                                tracing::warn!(%error, "S3 write queue could not continue its fence burst");
                                break;
                            }
                        };
                        if batch
                            .first()
                            .is_some_and(|pending| pending.mutation.is_planned())
                        {
                            deferred_planned = Some(batch);
                            break;
                        }
                    }
                    fences.release().await;
                    context.metrics.record_mutation_fence_burst(burst_batches);
                    if let Some(batch) = deferred_planned {
                        Self::apply_one_batch(
                            repository,
                            &context,
                            branch,
                            queue,
                            batch,
                            FenceUse::Acquire,
                            cancel,
                        )
                        .await;
                    }
                }
                Err(error) => reject_batch(batch, error),
            }
        }
        let idle_maintenance = repository.maintenance.finish(cancel);
        crate::repository::schedule_catalog_maintenance(
            repository,
            cancel,
            repository.maintenance.current_epoch(),
        );
        if let Some((epoch, maintenance_cancel)) = idle_maintenance {
            crate::repository::schedule_readability(
                repository,
                context.runtime,
                context.options,
                maintenance_cancel,
                epoch,
            );
        }
    }

    async fn apply_one_batch(
        repository: &Repository,
        context: &BatchContext,
        branch: &str,
        queue: &RefQueue,
        batch: Vec<PendingMutation>,
        fences: FenceUse,
        cancel: &CancellationToken,
    ) {
        let input_bytes = batch.iter().fold(0usize, |total, pending| {
            total.saturating_add(pending.mutation.estimated_bytes())
        });
        for pending in &batch {
            context
                .metrics
                .record_mutation_queue_wait(pending.queued_at.elapsed().as_secs_f64());
        }
        let _observation = context
            .metrics
            .start_mutation_batch(batch.len(), input_bytes);
        let mutations = batch
            .iter()
            .map(|pending| pending.mutation.clone())
            .collect::<Vec<_>>();
        let planned = mutations.len() == 1 && mutations[0].is_planned();
        let results = if planned {
            match mutations.into_iter().next() {
                Some(mutation) => Ok(vec![
                    apply_admitted(
                        repository,
                        Arc::clone(&context.runtime),
                        context.options,
                        mutation,
                        ApplyRequest {
                            branch,
                            plan_id: None,
                            metrics: &context.metrics,
                            state: Some(&queue.state),
                            checkpoints: Some(&context.checkpoints),
                        },
                        cancel,
                    )
                    .await,
                ]),
                None => Err(std::io::Error::other("planned mutation batch is empty").into()),
            }
        } else {
            let request = ApplyRequest {
                branch,
                plan_id: None,
                metrics: &context.metrics,
                state: Some(&queue.state),
                checkpoints: Some(&context.checkpoints),
            };
            match fences {
                FenceUse::Acquire => {
                    apply_batch_admitted(
                        repository,
                        Arc::clone(&context.runtime),
                        context.options,
                        mutations,
                        request,
                        cancel,
                    )
                    .await
                }
                FenceUse::Held => {
                    let result = apply_with_fences(
                        repository,
                        Arc::clone(&context.runtime),
                        context.options,
                        mutations,
                        request,
                        cancel,
                    )
                    .await;
                    if result.is_ok() {
                        repository.read_views.invalidate().await;
                    }
                    result
                }
            }
        };
        match results {
            Ok(results) => {
                for (pending, result) in batch.into_iter().zip(results) {
                    let _ = pending.result.send(result);
                }
            }
            Err(error) => {
                let error = Arc::new(error);
                for pending in batch {
                    let _ = pending.result.send(Err(Error::Batch(Arc::clone(&error))));
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum FenceUse {
    Acquire,
    Held,
}

fn reject_batch(batch: Vec<PendingMutation>, error: Error) {
    let error = Arc::new(error);
    for pending in batch {
        let _ = pending.result.send(Err(Error::Batch(Arc::clone(&error))));
    }
}

#[cfg(test)]
async fn apply(
    repository: &Repository,
    runtime: Arc<crab_remote_git::RemoteGitRuntime>,
    options: crab_remote_git::RepositoryOptions,
    branch: &str,
    path: &crab_remote_git::GitPath,
    change: Change,
    principal: &str,
    cancel: &CancellationToken,
) -> Result<Outcome> {
    Coordinator::new(runtime, options, cancel.clone(), Metrics::new().unwrap())
        .apply(repository, branch, path, change, principal, cancel)
        .await
}

#[derive(Default)]
struct WriteAdmission {
    refs: std::sync::Mutex<HashMap<String, Arc<RefQueue>>>,
    complete_manifests: std::sync::Mutex<HashMap<String, CachedCompleteManifest>>,
    warm_bytes: Arc<AtomicUsize>,
}

struct CachedCompleteManifest {
    tip: ObjectId,
    manifest: Arc<attributes::Manifest>,
    bytes: usize,
}

struct RefQueue {
    gate: Arc<Semaphore>,
    admitted: AtomicUsize,
    pending: std::sync::Mutex<VecDeque<PendingMutation>>,
    state: BranchState,
}

struct CachedBranchState {
    tip: Option<ObjectId>,
    transaction: Option<String>,
    manifest: Arc<attributes::Manifest>,
    tree: WarmTree,
    bytes: usize,
    batches_since_checkpoint: usize,
}

struct BranchState {
    cached: std::sync::Mutex<Option<CachedBranchState>>,
    warm_bytes: Arc<AtomicUsize>,
}

struct CheckpointJob {
    repository: Repository,
    branch: String,
    commit: ObjectId,
    manifest: attributes::Manifest,
}

struct CheckpointRefState {
    running: bool,
    pending: Option<CheckpointJob>,
}

struct CheckpointScheduler {
    refs: std::sync::Mutex<HashMap<String, CheckpointRefState>>,
    permits: Arc<Semaphore>,
    cancellation: CancellationToken,
    active: AtomicUsize,
    idle: Notify,
    metrics: Metrics,
}

impl CheckpointScheduler {
    fn new(cancellation: CancellationToken, metrics: Metrics) -> Self {
        Self {
            refs: std::sync::Mutex::new(HashMap::new()),
            permits: Arc::new(Semaphore::new(CHECKPOINT_PUBLICATION_CONCURRENCY)),
            cancellation,
            active: AtomicUsize::new(0),
            idle: Notify::new(),
            metrics,
        }
    }

    fn schedule(
        self: &Arc<Self>,
        repository: &Repository,
        branch: &str,
        commit: ObjectId,
        manifest: &attributes::Manifest,
    ) {
        if self.cancellation.is_cancelled() {
            return;
        }
        let key = format!("{}\0{branch}", repository.config.name);
        let start = {
            let mut refs = self
                .refs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let state = refs.entry(key.clone()).or_insert(CheckpointRefState {
                running: false,
                pending: None,
            });
            let replaced = state.pending.replace(CheckpointJob {
                repository: repository.clone(),
                branch: branch.to_owned(),
                commit,
                manifest: manifest.clone(),
            });
            self.metrics.record_checkpoint_scheduled(replaced.is_some());
            if state.running {
                false
            } else {
                state.running = true;
                self.active.fetch_add(1, Ordering::AcqRel);
                true
            }
        };
        if start {
            let scheduler = Arc::clone(self);
            tokio::spawn(async move { scheduler.run_ref(key).await });
        }
    }

    async fn run_ref(self: Arc<Self>, key: String) {
        loop {
            let job = {
                let mut refs = self
                    .refs
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match refs.get_mut(&key).and_then(|state| state.pending.take()) {
                    Some(job) => Some(job),
                    None => {
                        refs.remove(&key);
                        self.active.fetch_sub(1, Ordering::AcqRel);
                        self.idle.notify_waiters();
                        None
                    }
                }
            };
            let Some(job) = job else {
                return;
            };
            if self.cancellation.is_cancelled() {
                continue;
            }
            let permit = tokio::select! {
                permit = Arc::clone(&self.permits).acquire_owned() => permit,
                () = self.cancellation.cancelled() => continue,
            };
            let Ok(_permit) = permit else {
                continue;
            };
            let commit = job.commit;
            let repository = job.repository;
            let branch = job.branch;
            let manifest = job.manifest;
            let preparation = self.metrics.observe_checkpoint_prepare(async {
                tokio::task::spawn_blocking(move || {
                    attributes::prepare_checkpoint(&branch, commit, &manifest)
                })
                .await
                .map_err(Error::Worker)?
                .map_err(Error::from)
            });
            // A blocking serializer cannot be cancelled by dropping its join
            // handle. Await it so shutdown does not leave detached work behind.
            let prepared = preparation.await;
            let prepared = match prepared {
                Ok(Some(prepared)) => prepared,
                Ok(None) => continue,
                Err(error) => {
                    self.metrics.record_checkpoint_failure();
                    tracing::warn!(%error, "S3 attribute checkpoint preparation failed");
                    continue;
                }
            };
            if self.cancellation.is_cancelled() {
                continue;
            }
            let newer_pending = self
                .refs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&key)
                .is_some_and(|state| state.pending.is_some());
            if newer_pending {
                self.metrics.record_checkpoint_superseded();
                continue;
            }
            let publication = self
                .metrics
                .observe_checkpoint_publish(attributes::publish_checkpoint(&repository, &prepared));
            let result = tokio::select! {
                result = publication => result,
                () = self.cancellation.cancelled() => continue,
            };
            match result {
                Ok(true) => self.metrics.record_checkpoint_published(),
                Ok(false) => self.metrics.record_checkpoint_superseded(),
                Err(error) => {
                    self.metrics.record_checkpoint_failure();
                    tracing::warn!(%error, "S3 attribute checkpoint publication failed");
                }
            }
        }
    }

    async fn wait_idle(&self) {
        loop {
            let notified = self.idle.notified();
            if self.active.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }

    async fn shutdown(&self) {
        self.cancellation.cancel();
        self.wait_idle().await;
    }
}

impl BranchState {
    fn new(warm_bytes: Arc<AtomicUsize>) -> Self {
        Self {
            cached: std::sync::Mutex::new(None),
            warm_bytes,
        }
    }

    #[cfg(test)]
    fn take(&self, tip: Option<ObjectId>) -> Option<CachedBranchState> {
        self.take_latest().filter(|state| state.tip == tip)
    }

    fn take_latest(&self) -> Option<CachedBranchState> {
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = cached.take();
        if let Some(state) = &state {
            self.warm_bytes.fetch_sub(state.bytes, Ordering::AcqRel);
        }
        state
    }

    fn store(
        &self,
        tip: Option<ObjectId>,
        transaction: Option<String>,
        manifest: attributes::Manifest,
        tree: WarmTree,
        batches_since_checkpoint: usize,
    ) {
        self.clear();
        let bytes = manifest
            .estimated_bytes()
            .saturating_add(tree.estimated_bytes());
        if self
            .warm_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|next| *next <= MAX_WARM_BRANCH_STATE_BYTES)
            })
            .is_err()
        {
            return;
        }
        let state = CachedBranchState {
            tip,
            transaction,
            manifest: Arc::new(manifest),
            tree,
            bytes,
            batches_since_checkpoint,
        };
        *self
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(state);
    }

    fn manifest(&self, tip: Option<ObjectId>) -> Option<Arc<attributes::Manifest>> {
        self.cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|state| state.tip == tip)
            .map(|state| Arc::clone(&state.manifest))
    }

    fn clear(&self) {
        let state = self
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(state) = state {
            self.warm_bytes.fetch_sub(state.bytes, Ordering::AcqRel);
        }
    }
}

impl Drop for BranchState {
    fn drop(&mut self) {
        let state = self
            .cached
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(state) = state {
            self.warm_bytes.fetch_sub(state.bytes, Ordering::AcqRel);
        }
    }
}

struct QueuedMutation {
    queue: Arc<RefQueue>,
    result: oneshot::Receiver<Result<Outcome>>,
}

impl Drop for QueuedMutation {
    fn drop(&mut self) {
        self.queue.admitted.fetch_sub(1, Ordering::AcqRel);
    }
}

struct PendingMutation {
    mutation: Mutation,
    result: oneshot::Sender<Result<Outcome>>,
    queued_at: Instant,
}

#[derive(Clone)]
struct Mutation {
    path: crab_remote_git::GitPath,
    change: Change,
    principal: String,
}

impl Mutation {
    fn is_planned(&self) -> bool {
        matches!(
            &self.change,
            Change::Put { attributes, .. } if attributes.completion_upload_id.is_some()
        )
    }

    fn completion_plan(&self) -> Option<CompletionPlan> {
        match &self.change {
            Change::Put { attributes, .. } => {
                attributes
                    .completion_upload_id
                    .as_deref()
                    .map(|id| CompletionPlan {
                        id: crate::multipart::publication_plan_id(id),
                        etag: attributes.etag_override.clone(),
                    })
            }
            Change::Attributes { .. } | Change::Delete { .. } => None,
        }
    }

    fn estimated_bytes(&self) -> usize {
        match &self.change {
            Change::Put { bytes, .. } => bytes.len(),
            Change::Attributes { .. } | Change::Delete { .. } => 0,
        }
    }
}

impl RefQueue {
    fn take_batch(&self) -> Result<Vec<PendingMutation>> {
        let mut pending = self.pending.lock().map_err(|_| Error::AdmissionState)?;
        while pending.front().is_some_and(|item| item.result.is_closed()) {
            pending.pop_front();
        }
        let planned = pending
            .front()
            .is_some_and(|item| item.mutation.is_planned());
        let mut batch = Vec::new();
        let mut bytes = 0usize;
        while batch.len() < MAX_MUTATIONS_PER_BATCH {
            let Some(next) = pending.front() else {
                break;
            };
            if next.result.is_closed() {
                pending.pop_front();
                continue;
            }
            if !batch.is_empty()
                && (planned
                    || next.mutation.is_planned()
                    || bytes.saturating_add(next.mutation.estimated_bytes())
                        > MAX_MUTATION_BATCH_BYTES)
            {
                break;
            }
            let next = pending
                .pop_front()
                .ok_or_else(|| std::io::Error::other("write queue front disappeared"))?;
            bytes = bytes.saturating_add(next.mutation.estimated_bytes());
            batch.push(next);
            if planned {
                break;
            }
        }
        Ok(batch)
    }
}

impl WriteAdmission {
    fn complete_manifest(
        &self,
        repository: &str,
        branch: &str,
        tip: ObjectId,
    ) -> Option<Arc<attributes::Manifest>> {
        self.complete_manifests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&format!("{repository}\0{branch}"))
            .filter(|cached| cached.tip == tip)
            .map(|cached| Arc::clone(&cached.manifest))
    }

    fn store_complete_manifest(
        &self,
        repository: &str,
        branch: &str,
        tip: ObjectId,
        manifest: Arc<attributes::Manifest>,
    ) {
        let key = format!("{repository}\0{branch}");
        let mut cached = self
            .complete_manifests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(previous) = cached.remove(&key) {
            self.warm_bytes.fetch_sub(previous.bytes, Ordering::AcqRel);
        }
        let bytes = manifest.estimated_bytes();
        if cached.len() >= MAX_TRACKED_REFS
            || self
                .warm_bytes
                .load(Ordering::Acquire)
                .saturating_add(bytes)
                > MAX_WARM_BRANCH_STATE_BYTES
        {
            let removed = cached.drain().fold(0usize, |total, (_, entry)| {
                total.saturating_add(entry.bytes)
            });
            self.warm_bytes.fetch_sub(removed, Ordering::AcqRel);
        }
        if self
            .warm_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|next| *next <= MAX_WARM_BRANCH_STATE_BYTES)
            })
            .is_ok()
        {
            cached.insert(
                key,
                CachedCompleteManifest {
                    tip,
                    manifest,
                    bytes,
                },
            );
        }
    }

    fn clear_complete_manifest(&self, key: &str) {
        let removed = self
            .complete_manifests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(key);
        if let Some(removed) = removed {
            self.warm_bytes.fetch_sub(removed.bytes, Ordering::AcqRel);
        }
    }

    fn clear_complete_manifests(&self) {
        let removed = self
            .complete_manifests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain()
            .fold(0usize, |total, (_, entry)| {
                total.saturating_add(entry.bytes)
            });
        self.warm_bytes.fetch_sub(removed, Ordering::AcqRel);
    }

    fn manifest(
        &self,
        repository: &str,
        branch: &str,
        tip: Option<ObjectId>,
    ) -> Option<Arc<attributes::Manifest>> {
        let key = format!("{repository}\0{branch}");
        self.refs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .and_then(|queue| queue.state.manifest(tip))
    }

    fn enqueue(
        &self,
        repository: &str,
        branch: &str,
        mutation: Mutation,
    ) -> Result<QueuedMutation> {
        let key = format!("{repository}\0{branch}");
        self.clear_complete_manifest(&key);
        if self.warm_bytes.load(Ordering::Acquire) > WARM_BRANCH_STATE_EVICTION_BYTES {
            self.clear_complete_manifests();
        }
        let queue = {
            let mut refs = self.refs.lock().map_err(|_| Error::AdmissionState)?;
            if self.warm_bytes.load(Ordering::Acquire) > WARM_BRANCH_STATE_EVICTION_BYTES {
                for (candidate, queue) in refs.iter() {
                    if self.warm_bytes.load(Ordering::Acquire) <= WARM_BRANCH_STATE_EVICTION_BYTES {
                        break;
                    }
                    if candidate != &key && queue.admitted.load(Ordering::Acquire) == 0 {
                        queue.state.clear();
                    }
                }
            }
            match refs.get(&key) {
                Some(queue) => Arc::clone(queue),
                None => {
                    if refs.len() >= MAX_TRACKED_REFS {
                        // The map is only a warm scheduling cache. Retain active
                        // queues and discard every idle derived manifest at once.
                        refs.retain(|_, queue| Arc::strong_count(queue) > 1);
                    }
                    if refs.len() >= MAX_TRACKED_REFS {
                        return Err(Error::Overloaded);
                    }
                    let queue = Arc::new(RefQueue {
                        gate: Arc::new(Semaphore::new(1)),
                        admitted: AtomicUsize::new(0),
                        pending: std::sync::Mutex::new(VecDeque::new()),
                        state: BranchState::new(Arc::clone(&self.warm_bytes)),
                    });
                    refs.insert(key, Arc::clone(&queue));
                    queue
                }
            }
        };
        queue
            .admitted
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                (value < MAX_QUEUED_WRITES_PER_REF).then_some(value + 1)
            })
            .map_err(|_| Error::Overloaded)?;
        let (result, receiver) = oneshot::channel();
        let pushed = queue
            .pending
            .lock()
            .map(|mut pending| {
                pending.push_back(PendingMutation {
                    mutation,
                    result,
                    queued_at: Instant::now(),
                })
            })
            .map_err(|_| Error::AdmissionState);
        if let Err(error) = pushed {
            queue.admitted.fetch_sub(1, Ordering::AcqRel);
            return Err(error);
        }
        Ok(QueuedMutation {
            queue,
            result: receiver,
        })
    }
}

struct RefLease {
    holder: String,
    stop: CancellationToken,
    worker: tokio::task::JoinHandle<std::result::Result<(), crab_coordination::CoordinationError>>,
}

struct GcWriterFences {
    fences: Vec<(GcFenceLease, GcFenceHeartbeat)>,
}

impl GcWriterFences {
    async fn acquire(repository: &Repository, cancel: &CancellationToken) -> Result<Self> {
        let mut fences = Vec::new();
        for domain in [
            repository.layout.global_prefix(),
            repository.layout.repo_prefix(),
        ] {
            let fence = match async {
                check_cancelled(cancel)?;
                GcFenceLease::acquire_writer(repository.store.inner(), domain, LOCK_TTL)
                    .await
                    .map_err(Error::from)
            }
            .await
            {
                Ok(fence) => fence,
                Err(error) => {
                    Self { fences }.release().await;
                    return Err(error);
                }
            };
            let heartbeat = GcFenceHeartbeat::spawn(&fence, cancel.clone(), LOCK_TTL / 3);
            fences.push((fence, heartbeat));
        }
        Ok(Self { fences })
    }

    async fn release(self) {
        for (fence, heartbeat) in self.fences.into_iter().rev() {
            heartbeat.stop().await;
            if let Err(error) = fence.release().await {
                tracing::warn!(%error, "S3 GC fence cleanup failed");
            }
        }
    }
}

impl RefLease {
    async fn acquire(
        repository: &Repository,
        branch: &str,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        check_cancelled(cancel)?;
        let deadline = tokio::time::Instant::now() + WRITE_QUEUE_TIMEOUT;
        let mut attempt = 0u32;
        let mut lock = loop {
            match PushLock::acquire_ref(
                repository.store.inner(),
                repository.layout.repo_prefix(),
                branch,
                REF_LOCK_TTL,
            )
            .await
            {
                Ok(lock) => break lock,
                Err(crab_coordination::CoordinationError::PushLockHeld { .. })
                    if tokio::time::Instant::now() < deadline =>
                {
                    let delay = Duration::from_millis(
                        25u64.saturating_mul(1u64.checked_shl(attempt.min(4)).unwrap_or(16)),
                    );
                    attempt = attempt.saturating_add(1);
                    tokio::select! {
                        () = cancel.cancelled() => return Err(Error::Cancelled),
                        () = tokio::time::sleep(delay) => {}
                    }
                }
                Err(crab_coordination::CoordinationError::PushLockHeld { .. }) => {
                    return Err(Error::AdmissionTimeout);
                }
                Err(error) => return Err(error.into()),
            }
        };
        let holder = lock.holder().to_owned();
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let cancel = cancel.clone();
        let worker = tokio::spawn(async move {
            let result = crab_coordination::while_renewing(&mut lock, Some(&cancel), async {
                stopped.cancelled().await;
                Ok::<_, crab_coordination::CoordinationError>(())
            })
            .await;
            result.and(lock.release().await)
        });
        Ok(Self {
            holder,
            stop,
            worker,
        })
    }

    async fn release(self) {
        self.stop.cancel();
        if let result @ (Err(_) | Ok(Err(_))) = self.worker.await {
            tracing::warn!(?result, "S3 ref lease cleanup failed");
        }
    }
}

#[derive(Clone, Copy)]
struct ApplyRequest<'a> {
    branch: &'a str,
    plan_id: Option<&'a str>,
    metrics: &'a Metrics,
    state: Option<&'a BranchState>,
    checkpoints: Option<&'a Arc<CheckpointScheduler>>,
}

async fn apply_admitted(
    repository: &Repository,
    runtime: Arc<crab_remote_git::RemoteGitRuntime>,
    options: crab_remote_git::RepositoryOptions,
    mutation: Mutation,
    request: ApplyRequest<'_>,
    cancel: &CancellationToken,
) -> Result<Outcome> {
    let completion_plan = mutation.completion_plan();
    let Some(completion_plan) = completion_plan else {
        return take_single_result(
            apply_with_gc_fences(
                repository,
                runtime,
                options,
                vec![mutation],
                request,
                cancel,
            )
            .await?,
        );
    };
    if let Some(outcome) = resolved_completion_plan(repository, &completion_plan).await? {
        return Ok(outcome);
    }
    let executing_plan_id = completion_plan.id.clone();
    let result = crab_remote::publication::with_plan(
        &repository.store,
        &repository.layout,
        &completion_plan.id,
        LOCK_TTL,
        cancel,
        |scoped| async move {
            apply_with_gc_fences(
                repository,
                runtime,
                options,
                vec![mutation],
                ApplyRequest {
                    plan_id: Some(&executing_plan_id),
                    ..request
                },
                &scoped,
            )
            .await
            .and_then(take_single_result)
        },
    )
    .await;
    match result {
        Err(
            error @ Error::Metadata(crab_metadata::error::MetadataError::PlanAlreadyAttempted {
                ..
            }),
        ) => resolved_completion_plan(repository, &completion_plan)
            .await?
            .ok_or(error),
        result => result,
    }
}

async fn apply_batch_admitted(
    repository: &Repository,
    runtime: Arc<crab_remote_git::RemoteGitRuntime>,
    options: crab_remote_git::RepositoryOptions,
    mutations: Vec<Mutation>,
    request: ApplyRequest<'_>,
    cancel: &CancellationToken,
) -> Result<Vec<Result<Outcome>>> {
    apply_with_gc_fences(repository, runtime, options, mutations, request, cancel).await
}

fn take_single_result(mut results: Vec<Result<Outcome>>) -> Result<Outcome> {
    if results.len() != 1 {
        return Err(
            std::io::Error::other("singleton mutation returned the wrong result count").into(),
        );
    }
    results.pop().ok_or_else(|| {
        Error::Io(std::io::Error::other(
            "singleton mutation result disappeared",
        ))
    })?
}

struct CompletionPlan {
    id: String,
    etag: Option<String>,
}

async fn resolved_completion_plan(
    repository: &Repository,
    plan: &CompletionPlan,
) -> Result<Option<Outcome>> {
    crab_metadata::plan_receipt::resolve_plan_receipt(
        &repository.store,
        &repository.layout,
        &plan.id,
    )
    .await
    .map(|receipt| {
        receipt.map(|_| Outcome {
            etag: plan.etag.clone(),
        })
    })
    .map_err(Into::into)
}

async fn apply_with_gc_fences(
    repository: &Repository,
    runtime: Arc<crab_remote_git::RemoteGitRuntime>,
    options: crab_remote_git::RepositoryOptions,
    mutations: Vec<Mutation>,
    request: ApplyRequest<'_>,
    cancel: &CancellationToken,
) -> Result<Vec<Result<Outcome>>> {
    let fences = GcWriterFences::acquire(repository, cancel).await?;
    let result = apply_with_fences(
        repository,
        Arc::clone(&runtime),
        options,
        mutations,
        request,
        cancel,
    )
    .await;
    fences.release().await;
    if result.is_ok() {
        repository.read_views.invalidate().await;
    }
    result
}

async fn apply_with_fences(
    repository: &Repository,
    runtime: Arc<crab_remote_git::RemoteGitRuntime>,
    options: crab_remote_git::RepositoryOptions,
    mutations: Vec<Mutation>,
    request: ApplyRequest<'_>,
    cancel: &CancellationToken,
) -> Result<Vec<Result<Outcome>>> {
    for _ in 0..MAX_REPREPARE_ATTEMPTS {
        check_cancelled(cancel)?;
        let prepared = prepare_and_upload_batch(
            repository,
            Arc::clone(&runtime),
            options,
            &mutations,
            request,
            cancel,
        )
        .await?;
        let Some(publication) = prepared.publication.as_ref() else {
            if prepared.requires_revalidation {
                let lease = RefLease::acquire(repository, request.branch, cancel).await?;
                let current = prepared_parent_is_current(
                    repository,
                    request.branch,
                    prepared.tip,
                    prepared.transaction.as_deref(),
                )
                .await;
                lease.release().await;
                if !current? {
                    repository.read_views.invalidate().await;
                    continue;
                }
            }
            if let Some(state) = request.state {
                state.store(
                    prepared.tip,
                    prepared.transaction,
                    prepared.attributes,
                    prepared.tree,
                    prepared.batches_since_checkpoint,
                );
            }
            request
                .metrics
                .record_mutation_commits(prepared.commit_count);
            return Ok(prepared.outcomes);
        };
        let lease = RefLease::acquire(repository, request.branch, cancel).await?;
        let published = publish_prepared(
            repository,
            request.branch,
            &lease.holder,
            publication,
            request.plan_id,
            cancel,
        )
        .await;
        lease.release().await;
        match published? {
            Publish::Committed(transaction) => {
                let checkpoint = prepared.checkpoint;
                let tip = prepared.tip;
                if checkpoint && let (Some(checkpoints), Some(commit)) = (request.checkpoints, tip)
                {
                    checkpoints.schedule(repository, request.branch, commit, &prepared.attributes);
                }
                if let Some(state) = request.state {
                    state.store(
                        tip,
                        Some(transaction),
                        prepared.attributes,
                        prepared.tree,
                        prepared.batches_since_checkpoint,
                    );
                }
                request
                    .metrics
                    .record_mutation_commits(prepared.commit_count);
                return Ok(prepared.outcomes);
            }
            Publish::Reprepare => repository.read_views.invalidate().await,
        }
    }
    Err(crab_write::WriteError::RefChanged {
        ref_name: request.branch.to_owned(),
        path: repository.layout.repo_prefix().to_owned(),
    }
    .into())
}

struct UploadedMutation {
    parent: Option<ObjectId>,
    expected_transaction: Option<String>,
    commit: ObjectId,
    pack: PackManifestEntry,
    evidence_hash: String,
}

struct PreparedBatch {
    outcomes: Vec<Result<Outcome>>,
    publication: Option<UploadedMutation>,
    commit_count: usize,
    tip: Option<ObjectId>,
    transaction: Option<String>,
    attributes: attributes::Manifest,
    tree: WarmTree,
    batches_since_checkpoint: usize,
    checkpoint: bool,
    requires_revalidation: bool,
}

struct BuiltBatch {
    outcomes: Vec<Result<Outcome>>,
    commits: Vec<BuiltCommit>,
    tip: Option<ObjectId>,
    attributes: attributes::Manifest,
    tree: WarmTree,
    batches_since_checkpoint: usize,
    checkpoint: bool,
}

enum Publish {
    Committed(String),
    Reprepare,
}

async fn prepare_and_upload_batch(
    repository: &Repository,
    runtime: Arc<crab_remote_git::RemoteGitRuntime>,
    options: crab_remote_git::RepositoryOptions,
    mutations: &[Mutation],
    request: ApplyRequest<'_>,
    cancel: &CancellationToken,
) -> Result<PreparedBatch> {
    if let Some(cached) = request.state.and_then(BranchState::take_latest) {
        // The ref lease revalidates this optimistic parent before publication;
        // a competing process forces a cold rebuild against object-store state.
        match build_batch(
            None,
            None,
            cached.tip,
            Arc::try_unwrap(cached.manifest).unwrap_or_else(|manifest| (*manifest).clone()),
            Some(cached.tree),
            cached.batches_since_checkpoint,
            mutations,
        )
        .await
        {
            Ok(batch) => {
                return upload_batch(
                    repository,
                    request.branch,
                    batch,
                    cached.transaction,
                    true,
                    cancel,
                    request.metrics,
                )
                .await;
            }
            Err(Error::WarmStateMiss) => {}
            Err(error) => return Err(error),
        }
    }
    let snapshot = crab_metadata::manifest_store::read_repository_snapshot(
        &repository.store,
        &repository.layout,
    )
    .await?;
    if !snapshot.journal.refs.contains_key(request.branch) {
        // An unborn branch has no Git objects to reconstruct. Publication
        // rechecks absence under the ref lease, so avoid opening the
        // repository-wide locator merely to prove the empty starting tree.
        let batch = build_batch(
            None,
            None,
            None,
            attributes::Manifest::empty_complete(),
            None,
            ATTRIBUTE_CHECKPOINT_BATCHES,
            mutations,
        )
        .await?;
        return upload_batch(
            repository,
            request.branch,
            batch,
            None,
            false,
            cancel,
            request.metrics,
        )
        .await;
    }
    let view = repository
        .read_views
        .current(repository, runtime, options, cancel)
        .await?;
    let original_parent = view
        .remote()
        .refs()
        .find(request.branch)
        .map(|reference| reference.target);
    let operation = view
        .remote()
        .operation(OperationKind::Repository, cancel)
        .await?;
    let batch = async {
        let snapshot = match original_parent {
            Some(parent) => Some(view.snapshot(&parent.to_string(), &operation).await?),
            None => None,
        };
        let manifest = match original_parent {
            Some(parent) => (*view.attributes(repository, parent).await?).clone(),
            None => attributes::Manifest::default(),
        };
        build_batch(
            snapshot,
            Some(&operation),
            original_parent,
            manifest,
            None,
            ATTRIBUTE_CHECKPOINT_BATCHES,
            mutations,
        )
        .await
    }
    .await;
    let batch = match operation.finish(Ok(())).await {
        Ok(()) => batch?,
        Err(error) => return Err(error.into()),
    };
    upload_batch(
        repository,
        request.branch,
        batch,
        None,
        false,
        cancel,
        request.metrics,
    )
    .await
}

async fn build_batch(
    snapshot: Option<crab_remote_git::RemoteGitSnapshot>,
    operation: Option<&crab_remote_git::OperationContext>,
    original_parent: Option<ObjectId>,
    mut manifest: attributes::Manifest,
    warm_tree: Option<WarmTree>,
    batches_since_checkpoint: usize,
    mutations: &[Mutation],
) -> Result<BuiltBatch> {
    let mut tree_state = MutableTree::new(snapshot, warm_tree);
    let mut parent = original_parent;
    let mut outcomes = Vec::with_capacity(mutations.len());
    let mut commits = Vec::with_capacity(mutations.len());
    for mutation in mutations {
        match build_commit(
            &mut tree_state,
            operation,
            parent,
            &mutation.path,
            mutation.change.clone(),
            &mutation.principal,
            &mut manifest,
        )
        .await
        {
            Ok(Build::Noop(outcome)) => outcomes.push(Ok(outcome)),
            Ok(Build::Commit(mut built)) => {
                built.parent = parent;
                parent = Some(built.commit);
                outcomes.push(Ok(Outcome {
                    etag: built.etag.clone(),
                }));
                commits.push(*built);
            }
            Err(Error::WarmStateMiss) => return Err(Error::WarmStateMiss),
            Err(error) => outcomes.push(Err(error)),
        }
    }
    let next_checkpoint = batches_since_checkpoint.saturating_add(1);
    let has_commits = !commits.is_empty();
    let checkpoint = has_commits
        && next_checkpoint >= ATTRIBUTE_CHECKPOINT_BATCHES
        && attributes::checkpoint_eligible(&manifest);
    if checkpoint {
        let commit =
            parent.ok_or_else(|| std::io::Error::other("checkpoint batch has no final commit"))?;
        manifest.mark_checkpoint(commit);
    }
    let batches_since_checkpoint = if !has_commits {
        batches_since_checkpoint
    } else if checkpoint {
        0
    } else {
        next_checkpoint
    };
    Ok(BuiltBatch {
        outcomes,
        commits,
        tip: parent,
        attributes: manifest,
        tree: tree_state.into_warm(),
        batches_since_checkpoint,
        checkpoint,
    })
}

async fn upload_batch(
    repository: &Repository,
    branch: &str,
    batch: BuiltBatch,
    expected_transaction: Option<String>,
    requires_revalidation: bool,
    cancel: &CancellationToken,
    metrics: &Metrics,
) -> Result<PreparedBatch> {
    let BuiltBatch {
        outcomes,
        commits,
        tip,
        attributes,
        mut tree,
        batches_since_checkpoint,
        checkpoint,
    } = batch;
    if commits.is_empty() {
        return Ok(PreparedBatch {
            outcomes,
            publication: None,
            commit_count: 0,
            tip,
            transaction: expected_transaction,
            attributes,
            tree,
            batches_since_checkpoint,
            checkpoint: false,
            requires_revalidation,
        });
    };
    let commit_count = commits.len();
    let publication = upload_built_batch(
        repository,
        branch,
        commits,
        &mut tree,
        expected_transaction.clone(),
        checkpoint,
        cancel,
        metrics,
    )
    .await?;
    Ok(PreparedBatch {
        outcomes,
        publication: Some(publication),
        commit_count,
        tip,
        transaction: expected_transaction,
        attributes,
        tree,
        batches_since_checkpoint,
        checkpoint,
        requires_revalidation,
    })
}

async fn prepared_parent_is_current(
    repository: &Repository,
    branch: &str,
    expected_tip: Option<ObjectId>,
    expected_transaction: Option<&str>,
) -> Result<bool> {
    if let Some(expected_transaction) = expected_transaction {
        let head = crab_metadata::ref_journal::read_ref_head(
            &repository.store,
            &repository.layout,
            branch,
        )
        .await?;
        return Ok(head.visible_transaction.as_deref() == Some(expected_transaction));
    }
    let snapshot = crab_metadata::manifest_store::read_repository_snapshot(
        &repository.store,
        &repository.layout,
    )
    .await?;
    let current = snapshot
        .journal
        .refs
        .get(branch)
        .map(|oid| oid.parse::<ObjectId>())
        .transpose()
        .map_err(|_| std::io::Error::other("repository ref contains an invalid object ID"))?;
    Ok(current == expected_tip)
}

async fn upload_built_batch(
    repository: &Repository,
    branch: &str,
    commits: Vec<BuiltCommit>,
    tree: &mut WarmTree,
    expected_transaction: Option<String>,
    checkpoint: bool,
    cancel: &CancellationToken,
    metrics: &Metrics,
) -> Result<UploadedMutation> {
    let mut objects = Vec::new();
    let mut seen = HashMap::new();
    let mut delta_candidates = BTreeMap::<ObjectId, Vec<TreeDeltaBase>>::new();
    let final_commit = commits
        .last()
        .map(|built| built.commit)
        .ok_or_else(|| std::io::Error::other("mutation batch has no commit"))?;
    let parent = commits.first().and_then(|built| built.parent);
    let checkpoint_slot = checkpoint.then(|| attributes::checkpoint_slot(branch));
    let attribute_deltas = commits
        .iter()
        .map(|built| {
            (
                built.commit,
                built.parent,
                built.attribute_changes.clone(),
                built.commit == final_commit && checkpoint,
                (built.commit == final_commit)
                    .then(|| checkpoint_slot.clone())
                    .flatten(),
            )
        })
        .collect::<Vec<_>>();
    for built in commits {
        for candidate in built.tree_delta_bases {
            delta_candidates
                .entry(candidate.object)
                .or_default()
                .push(candidate);
        }
        for (kind, bytes) in built.objects {
            let oid = object_id(kind, &bytes)?;
            if seen.insert(oid, ()).is_none() {
                objects.push((kind, bytes));
            }
        }
    }
    let mut delta_bases = BTreeMap::new();
    let mut external_delta_bases = BTreeMap::new();
    let mut external_delta_bytes = 0usize;
    for (object, candidates) in delta_candidates {
        let candidate = candidates
            .into_iter()
            .filter(|candidate| seen.contains_key(&candidate.base) || candidate.external.is_some())
            .min_by_key(|candidate| (!seen.contains_key(&candidate.base), candidate.base));
        let Some(candidate) = candidate else {
            continue;
        };
        if !seen.contains_key(&candidate.base) {
            let external = candidate.external.ok_or_else(|| {
                std::io::Error::other("external tree delta base bytes disappeared")
            })?;
            let Some(next_external_bytes) = external_delta_bytes.checked_add(external.bytes.len())
            else {
                continue;
            };
            if next_external_bytes > MAX_GENERATED_PACK_BYTES as usize {
                continue;
            }
            external_delta_bytes = next_external_bytes;
            external_delta_bases.insert(
                object,
                crab_git::incoming_pack::ExternalDeltaBase::new(
                    candidate.base,
                    Kind::Tree,
                    external.bytes,
                    external.depth,
                ),
            );
        }
        delta_bases.insert(object, candidate.base);
    }
    let scratch_bytes = pack_scratch_reservation(&objects);
    let capacity = metrics.reserve_scratch(scratch_bytes)?;
    let mut scratch = metrics.start_scratch(ScratchPurpose::GitPack);
    scratch.reserve(scratch_bytes);
    let visible_objects = objects
        .iter()
        .map(|(kind, bytes)| object_id(*kind, bytes).map(|oid| oid.to_string()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let generated_object_bytes = objects.iter().fold(0_usize, |total, (_, bytes)| {
        total.saturating_add(bytes.len())
    });
    let generated_tree_bytes = objects
        .iter()
        .filter(|(kind, _)| *kind == Kind::Tree)
        .fold(0_usize, |total, (_, bytes)| {
            total.saturating_add(bytes.len())
        });
    let prepared = prepare_pack(objects, delta_bases, external_delta_bases, cancel).await;
    if matches!(
        &prepared,
        Err(Error::Io(_)
            | Error::Pack(crab_git::incoming_pack::IncomingPackError::Io(_))
            | Error::Prepare(crab_git::incoming_pack::PreparePackError::Io(_)))
    ) {
        scratch.record_failure(ScratchFailure::Write);
    }
    let (pack_owner, pack) = prepared?;
    metrics.record_generated_pack(
        generated_object_bytes,
        generated_tree_bytes,
        pack.size(),
        external_delta_bytes,
        pack.external_delta_count(),
    );
    tree.bind_prepared_pack(&pack)?;
    // The prepared files are now reflected by statvfs. Release their
    // conservative pre-write claim while ownership metrics remain live.
    drop(capacity);
    check_cancelled(cancel)?;
    let pack_id = pack.content_hash().to_hex().to_string();
    let evidence = match parent {
        Some(parent) => git_visibility::GitVisibilityEdit::from_delta_objects(
            Some(parent.to_string()),
            final_commit.to_string(),
            visible_objects,
            vec![],
        ),
        None => git_visibility::GitVisibilityEdit::from_replacement_objects(
            None,
            final_commit.to_string(),
            visible_objects,
        ),
    };
    let pack_upload = async {
        let path = repository.layout.pack_path(&pack_id);
        if pack.size() <= GENERATED_PACK_SINGLE_PUT_MAX_BYTES {
            check_cancelled(cancel)?;
            repository
                .store
                .put_exact(&path, tokio::fs::read(pack.pack_path()).await?.into())
                .await?;
        } else {
            repository
                .store
                .put_multipart_file_retry(
                    &path,
                    pack.pack_path(),
                    pack.size(),
                    *pack.content_hash().as_bytes(),
                    GENERATED_PACK_SINGLE_PUT_MAX_BYTES as usize,
                    cancel,
                    None,
                )
                .await?;
        }
        Ok::<(), Error>(())
    };
    let sidecar_upload = |source: &std::path::Path, target| {
        let source = source.to_owned();
        async move {
            check_cancelled(cancel)?;
            repository
                .store
                .put_exact(&target, tokio::fs::read(source).await?.into())
                .await?;
            Ok::<(), Error>(())
        }
    };
    let evidence_upload = async {
        git_visibility::upload_edit(&repository.store, &repository.layout, &evidence)
            .await
            .map_err(Error::from)
    };
    let attributes_upload = async {
        futures_util::future::try_join_all(attribute_deltas.into_iter().map(
            |(commit, parent, changes, checkpoint, checkpoint_slot)| {
                attributes::save_delta(
                    repository,
                    commit,
                    parent,
                    changes,
                    checkpoint,
                    checkpoint_slot,
                )
            },
        ))
        .await
        .map(|_| ())
        .map_err(Error::from)
    };
    let (_, _, _, _, evidence_hash, _) = tokio::try_join!(
        pack_upload,
        sidecar_upload(
            pack.index_path(),
            repository.layout.pack_index_path(&pack_id)
        ),
        sidecar_upload(
            pack.reverse_path(),
            repository.layout.pack_reverse_index_path(&pack_id)
        ),
        sidecar_upload(
            pack.kinds_path(),
            repository.layout.pack_kind_metadata_path(&pack_id)
        ),
        evidence_upload,
        attributes_upload,
    )?;
    check_cancelled(cancel)?;
    let uploaded = UploadedMutation {
        parent,
        expected_transaction,
        commit: final_commit,
        pack: PackManifestEntry {
            pack_id: pack_id.clone(),
            content_hash: pack_id,
            size: pack.size(),
            object_count: pack.object_count().into(),
            ref_tips: vec![final_commit.to_string()],
        },
        evidence_hash,
    };
    drop(pack);
    drop(pack_owner);
    drop(scratch);
    Ok(uploaded)
}

fn pack_scratch_reservation(objects: &[(Kind, Vec<u8>)]) -> u64 {
    let bytes = objects.iter().fold(0_u64, |total, (_, object)| {
        total.saturating_add(object.len() as u64)
    });
    let sidecars = (objects.len() as u64).saturating_mul(64);
    // Direct generated-pack preparation retains one decoded spool and one
    // normalized pack. The remaining copies, extra quarter, and fixed
    // allowance conservatively cover indexing and sidecar overhead.
    bytes
        .saturating_mul(4)
        .saturating_add(bytes / 4)
        .saturating_add(sidecars)
        .saturating_add(PACK_SCRATCH_BASE_BYTES)
}

async fn publish_prepared(
    repository: &Repository,
    branch: &str,
    holder: &str,
    prepared: &UploadedMutation,
    plan_id: Option<&str>,
    cancel: &CancellationToken,
) -> Result<Publish> {
    check_cancelled(cancel)?;
    let options = crab_write::journal::CommitOptions::new(LOCK_TTL, cancel);
    let options = match plan_id {
        Some(plan_id) => options.with_plan(plan_id),
        None => options,
    };
    let edit = RefJournalEdit {
        ref_name: branch.to_owned(),
        old_oid: prepared.parent.map(|oid| oid.to_string()),
        new_oid: Some(prepared.commit.to_string()),
        peeled_oid: None,
        lock_holder: Some(holder.to_owned()),
        visibility_evidence_hash: Some(prepared.evidence_hash.clone()),
    };
    let committed = if let (Some(expected), Some(_)) =
        (prepared.expected_transaction.as_deref(), prepared.parent)
    {
        crab_write::journal::commit_existing_ref_edit(
            &repository.store,
            &repository.layout,
            expected,
            edit,
            vec![prepared.pack.clone()],
            options,
        )
        .await
    } else {
        let snapshot = crab_metadata::manifest_store::read_repository_snapshot(
            &repository.store,
            &repository.layout,
        )
        .await?;
        let current = snapshot
            .journal
            .refs
            .get(branch)
            .map(|oid| oid.parse::<ObjectId>())
            .transpose()
            .map_err(|_| std::io::Error::other("repository ref contains an invalid object ID"))?;
        if current != prepared.parent {
            return Ok(Publish::Reprepare);
        }
        crab_write::journal::commit_edits(
            &repository.store,
            &repository.layout,
            &snapshot,
            vec![edit],
            prepared.parent.is_none().then(|| branch.to_owned()),
            vec![prepared.pack.clone()],
            vec![],
            options,
        )
        .await
    };
    let committed = match committed {
        Ok(committed) => committed,
        Err(crab_write::WriteError::RefChanged { .. }) => return Ok(Publish::Reprepare),
        Err(error) => return Err(error.into()),
    };
    Ok(Publish::Committed(committed.transaction_id))
}

struct BuiltCommit {
    parent: Option<ObjectId>,
    commit: ObjectId,
    etag: Option<String>,
    objects: Vec<(Kind, Vec<u8>)>,
    tree_delta_bases: Vec<TreeDeltaBase>,
    attribute_changes: std::collections::BTreeMap<String, Option<attributes::ObjectAttributes>>,
}

struct TreeDeltaBase {
    object: ObjectId,
    base: ObjectId,
    external: Option<ExternalTreeDeltaBase>,
}

struct ExternalTreeDeltaBase {
    bytes: Vec<u8>,
    depth: u32,
}

enum Build {
    Noop(Outcome),
    Commit(Box<BuiltCommit>),
}

struct TreeEntryEdit {
    name: Vec<u8>,
    replacement: Option<tree::Entry>,
}

struct DirectoryEdit {
    path: Vec<u8>,
    oid: Option<ObjectId>,
    entries: Vec<TreeEntryEdit>,
    generated: bool,
}

#[derive(Clone)]
struct DirectoryState {
    oid: Option<ObjectId>,
    entries: Vec<tree::Entry>,
    delta_depth: Option<u32>,
    generated_in_batch: bool,
}

struct GeneratedAttributeBlob {
    oid: ObjectId,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct WarmTree {
    root_oid: Option<ObjectId>,
    directories: HashMap<Vec<u8>, DirectoryState>,
    generated_attribute_blobs: HashMap<Vec<u8>, GeneratedAttributeBlob>,
    estimated_bytes: usize,
}

impl WarmTree {
    fn estimated_bytes(&self) -> usize {
        self.estimated_bytes
    }

    fn bind_prepared_pack(&mut self, pack: &crab_git::incoming_pack::PreparedPack) -> Result<()> {
        for directory in self
            .directories
            .values_mut()
            .filter(|directory| directory.generated_in_batch)
        {
            let oid = directory.oid.ok_or_else(|| {
                std::io::Error::other("generated directory has no object identity")
            })?;
            directory.delta_depth = Some(pack.delta_depth(&oid).ok_or_else(|| {
                std::io::Error::other("generated directory is absent from prepared pack")
            })?);
            directory.generated_in_batch = false;
        }
        Ok(())
    }
}

struct MutableTree {
    snapshot: Option<crab_remote_git::RemoteGitSnapshot>,
    root_oid: Option<ObjectId>,
    directories: HashMap<Vec<u8>, DirectoryState>,
    generated_attribute_blobs: HashMap<Vec<u8>, GeneratedAttributeBlob>,
    estimated_bytes: usize,
}

impl MutableTree {
    fn new(snapshot: Option<crab_remote_git::RemoteGitSnapshot>, warm: Option<WarmTree>) -> Self {
        let snapshot_root = snapshot.as_ref().map(|value| value.root_tree_oid());
        let warm = warm.filter(|warm| snapshot_root.is_none() || warm.root_oid == snapshot_root);
        let (root_oid, directories, generated_attribute_blobs, estimated_bytes) = match warm {
            Some(warm) => (
                warm.root_oid,
                warm.directories,
                warm.generated_attribute_blobs,
                warm.estimated_bytes,
            ),
            None => (snapshot_root, HashMap::new(), HashMap::new(), 0),
        };
        Self {
            snapshot,
            root_oid,
            directories,
            generated_attribute_blobs,
            estimated_bytes,
        }
    }

    fn into_warm(self) -> WarmTree {
        WarmTree {
            root_oid: self.root_oid,
            directories: self.directories,
            generated_attribute_blobs: self.generated_attribute_blobs,
            estimated_bytes: self.estimated_bytes,
        }
    }

    async fn paths(
        &mut self,
        directories: &[Vec<u8>],
        operation: Option<&crab_remote_git::OperationContext>,
    ) -> Result<Vec<Vec<u8>>> {
        let mut paths = Vec::with_capacity(directories.len() + 1);
        let mut directory_path = crab_remote_git::GitPath::root();
        let mut directory_oid = self.root_oid;
        for depth in 0..=directories.len() {
            let key = directory_path.as_bytes().to_vec();
            if !self.directories.contains_key(&key) {
                let entries = match (self.snapshot.as_ref(), directory_oid) {
                    (Some(snapshot), Some(_)) => {
                        load_directory(
                            snapshot,
                            &directory_path,
                            operation.ok_or(Error::WarmStateMiss)?,
                        )
                        .await?
                    }
                    (None, None) | (Some(_), None) => Vec::new(),
                    (None, Some(_)) => return Err(Error::WarmStateMiss),
                };
                let state = DirectoryState {
                    oid: directory_oid,
                    entries,
                    delta_depth: None,
                    generated_in_batch: false,
                };
                self.estimated_bytes = self
                    .estimated_bytes
                    .saturating_add(estimated_directory_bytes(&key, &state));
                self.directories.insert(key.clone(), state);
            }
            let state = self
                .directories
                .get(&key)
                .ok_or_else(|| std::io::Error::other("mutation directory state disappeared"))?;
            if state.oid != directory_oid {
                return Err(std::io::Error::other("mutation directory state diverged").into());
            }
            paths.push(key);
            if depth == directories.len() {
                break;
            }
            let component = &directories[depth];
            directory_oid = match find_entry(&state.entries, component) {
                Some(entry) if entry.mode == tree::EntryKind::Tree.into() => Some(entry.oid),
                Some(_) => return Err(Error::NotDirectory),
                None => None,
            };
            directory_path = push_component(&directory_path, component)?;
        }
        Ok(paths)
    }

    fn commit(
        &mut self,
        edits: Vec<DirectoryEdit>,
        generated_attributes: Option<(Vec<u8>, Option<GeneratedAttributeBlob>)>,
    ) -> Result<()> {
        if edits
            .iter()
            .any(|edit| !self.directories.contains_key(&edit.path))
        {
            return Err(std::io::Error::other("mutation directory state disappeared").into());
        }
        for edit in edits {
            let Some(state) = self.directories.get_mut(&edit.path) else {
                return Err(std::io::Error::other("mutation directory state disappeared").into());
            };
            for entry in edit.entries {
                if let Some(index) = entry_index(&state.entries, &entry.name) {
                    let previous = state.entries.remove(index);
                    self.estimated_bytes = self
                        .estimated_bytes
                        .saturating_sub(estimated_tree_entry_bytes(&previous));
                }
                if let Some(replacement) = entry.replacement {
                    self.estimated_bytes = self
                        .estimated_bytes
                        .saturating_add(estimated_tree_entry_bytes(&replacement));
                    let index = state
                        .entries
                        .binary_search(&replacement)
                        .unwrap_or_else(|index| index);
                    state.entries.insert(index, replacement);
                }
            }
            state.oid = edit.oid;
            if edit.oid.is_none() {
                state.delta_depth = None;
                state.generated_in_batch = false;
            } else if edit.generated {
                state.delta_depth = None;
                state.generated_in_batch = true;
            }
            if edit.path.is_empty() {
                self.root_oid = edit.oid;
            }
        }
        if let Some((path, replacement)) = generated_attributes {
            self.replace_generated_attributes(path, replacement);
        }
        Ok(())
    }

    fn replace_generated_attributes(
        &mut self,
        path: Vec<u8>,
        replacement: Option<GeneratedAttributeBlob>,
    ) {
        if let Some(previous) = self.generated_attribute_blobs.remove(&path) {
            self.estimated_bytes = self
                .estimated_bytes
                .saturating_sub(estimated_attribute_blob_bytes(&path, &previous));
        }
        if let Some(replacement) = replacement {
            self.estimated_bytes = self
                .estimated_bytes
                .saturating_add(estimated_attribute_blob_bytes(&path, &replacement));
            self.generated_attribute_blobs.insert(path, replacement);
        }
    }

    async fn blob(
        &self,
        path: &crab_remote_git::GitPath,
        oid: ObjectId,
        operation: Option<&crab_remote_git::OperationContext>,
    ) -> Result<Vec<u8>> {
        let directory = path
            .as_bytes()
            .strip_suffix(b"/.gitattributes")
            .or_else(|| (path.as_bytes() == b".gitattributes").then_some(b"".as_slice()))
            .ok_or(Error::WarmStateMiss)?;
        if let Some(blob) = self.generated_attribute_blobs.get(directory)
            && blob.oid == oid
        {
            return Ok(blob.bytes.clone());
        }
        let snapshot = self.snapshot.as_ref().ok_or(Error::WarmStateMiss)?;
        let blob = snapshot
            .read_blob(path, operation.ok_or(Error::WarmStateMiss)?)
            .await?;
        if blob.metadata.oid != oid {
            return Err(std::io::Error::other("mutation blob identity diverged").into());
        }
        Ok(blob.bytes.to_vec())
    }
}

fn estimated_directory_bytes(path: &[u8], directory: &DirectoryState) -> usize {
    directory.entries.iter().fold(path.len(), |total, entry| {
        total.saturating_add(estimated_tree_entry_bytes(entry))
    })
}

fn estimated_tree_entry_bytes(entry: &tree::Entry) -> usize {
    std::mem::size_of::<tree::Entry>().saturating_add(entry.filename.len())
}

fn estimated_attribute_blob_bytes(path: &[u8], blob: &GeneratedAttributeBlob) -> usize {
    path.len()
        .saturating_add(std::mem::size_of::<ObjectId>())
        .saturating_add(blob.bytes.len())
}

async fn build_commit(
    tree_state: &mut MutableTree,
    operation: Option<&crab_remote_git::OperationContext>,
    parent: Option<ObjectId>,
    path: &crab_remote_git::GitPath,
    change: Change,
    principal: &str,
    manifest: &mut attributes::Manifest,
) -> Result<Build> {
    let components = path.components().map(<[u8]>::to_vec).collect::<Vec<_>>();
    let (name, directories) = components.split_last().ok_or(Error::IsDirectory)?;
    let paths = tree_state.paths(directories, operation).await?;
    let mut directory_path = crab_remote_git::GitPath::root();
    for component in directories {
        directory_path = push_component(&directory_path, component)?;
    }
    let leaf_path = paths
        .last()
        .ok_or_else(|| std::io::Error::other("object path has no parent tree"))?;
    let leaf_entries = &tree_state
        .directories
        .get(leaf_path)
        .ok_or_else(|| std::io::Error::other("object path parent tree disappeared"))?
        .entries;
    let old = match find_entry(leaf_entries, name) {
        Some(entry) if entry.mode == tree::EntryKind::Tree.into() => {
            return Err(Error::IsDirectory);
        }
        Some(entry) => Some((entry.oid, entry_mode(entry.mode)?)),
        None => None,
    };
    let path_string = std::str::from_utf8(path.as_bytes())
        .map_err(|_| std::io::Error::other("S3 object path is not UTF-8"))?;
    let current_attributes = old
        .as_ref()
        .and_then(|(oid, _)| manifest.object(path_string, *oid));
    match &change {
        Change::Put { condition, .. }
            if !put_condition_matches(condition, old.as_ref(), current_attributes) =>
        {
            return Err(Error::PreconditionFailed);
        }
        Change::Delete { condition } => {
            validate_delete_condition(condition, old.as_ref(), current_attributes)?;
        }
        Change::Put { .. } | Change::Attributes { .. } => {}
    }
    let lfs_attributes = match &change {
        Change::Put {
            track_lfs: true, ..
        } if name.as_slice() == b".gitattributes" => return Err(Error::InvalidAttributes),
        Change::Put {
            track_lfs: true, ..
        } => {
            prepare_lfs_attributes(tree_state, &directory_path, leaf_entries, name, operation)
                .await?
        }
        Change::Put { .. } | Change::Attributes { .. } | Change::Delete { .. } => None,
    };
    let mut leaf_edits = Vec::with_capacity(2);
    let (etag, changed, pending_attributes) = match change {
        Change::Put {
            bytes,
            track_lfs: _,
            attributes,
            condition: _,
        } => {
            let oid = object_id(Kind::Blob, &bytes)?;
            let digest = md5::Md5::digest(&bytes);
            let logical_size = attributes.logical_size.unwrap_or(bytes.len() as u64);
            let etag = attributes
                .etag_override
                .clone()
                .unwrap_or_else(|| digest.iter().map(|byte| format!("{byte:02x}")).collect());
            if attributes.completion_upload_id.is_some()
                && old.is_some_and(|(old_oid, mode)| old_oid == oid && mode == EntryMode::Regular)
                && current_attributes
                    .is_some_and(|stored| stored.matches_pending(&attributes, &etag, logical_size))
                && lfs_attributes.is_none()
            {
                return Ok(Build::Noop(Outcome { etag: Some(etag) }));
            }
            leaf_edits.push(TreeEntryEdit {
                name: name.clone(),
                replacement: Some(tree::Entry {
                    mode: tree::EntryKind::Blob.into(),
                    filename: BString::from(name.clone()),
                    oid,
                }),
            });
            let changed = old
                .filter(|(old_oid, _)| *old_oid == oid)
                .map(|_| None)
                .unwrap_or_else(|| Some(bytes.to_vec()));
            (Some(etag), changed, Some((oid, attributes, logical_size)))
        }
        Change::Attributes {
            expected,
            attributes,
        } => {
            if old.is_none_or(|(oid, mode)| {
                oid != expected || !matches!(mode, EntryMode::Regular | EntryMode::Executable)
            }) {
                return Err(Error::PreconditionFailed);
            }
            let logical_size = attributes.logical_size.ok_or_else(|| {
                std::io::Error::other("attribute-only mutation is missing object size")
            })?;
            let etag = attributes.etag_override.clone().ok_or_else(|| {
                std::io::Error::other("attribute-only mutation is missing object ETag")
            })?;
            if current_attributes
                .is_some_and(|stored| stored.matches_pending(&attributes, &etag, logical_size))
            {
                return Ok(Build::Noop(Outcome { etag: Some(etag) }));
            }
            (Some(etag), None, Some((expected, attributes, logical_size)))
        }
        Change::Delete { .. } => {
            if old.is_none() {
                return Ok(Build::Noop(Outcome { etag: None }));
            }
            leaf_edits.push(TreeEntryEdit {
                name: name.clone(),
                replacement: None,
            });
            (None, None, None)
        }
    };
    let seconds = now_seconds()?;
    let mut objects = Vec::new();
    let mut tree_delta_bases = Vec::new();
    let mut attribute_changes = std::collections::BTreeMap::new();
    if let Some(bytes) = changed {
        objects.push((Kind::Blob, bytes));
    }
    if let Some(attributes) = lfs_attributes {
        let attributes_path = push_component(&directory_path, b".gitattributes")?;
        let attributes_path = std::str::from_utf8(attributes_path.as_bytes())
            .map_err(|_| std::io::Error::other("S3 attribute path is not UTF-8"))?
            .to_owned();
        let generated_attributes = attributes::ObjectAttributes::new(
            attributes.oid,
            crate::gateway::md5_hex(&attributes.bytes),
            attributes.bytes.len() as u64,
            seconds,
            attributes::PutAttributes::default(),
        );
        leaf_edits.push(TreeEntryEdit {
            name: b".gitattributes".to_vec(),
            replacement: Some(tree::Entry {
                mode: attributes.mode,
                filename: BString::from(b".gitattributes".to_vec()),
                oid: attributes.oid,
            }),
        });
        attribute_changes.insert(attributes_path, Some(generated_attributes));
        objects.push((Kind::Blob, attributes.bytes));
    }
    let object_attributes = pending_attributes.map(|(oid, pending, size)| {
        attributes::ObjectAttributes::new(
            oid,
            etag.clone().unwrap_or_default(),
            size,
            seconds,
            *pending,
        )
    });
    attribute_changes.insert(path_string.to_owned(), object_attributes.clone());
    let attribute_identity = serde_json::to_vec(&attribute_changes)
        .map_err(|source| Error::Attributes(Box::new(crate::Error::Attributes { source })))?;
    let attribute_digest = blake3::hash(&attribute_identity).to_hex();

    let generated_attributes =
        generated_attribute_update(tree_state, leaf_path, leaf_entries, &leaf_edits, &objects)?;
    let mut directory_edits = Vec::with_capacity(paths.len());
    let mut tree_oid = None;
    let mut entries = leaf_edits;
    for depth in (0..paths.len()).rev() {
        let state = tree_state
            .directories
            .get(&paths[depth])
            .ok_or_else(|| std::io::Error::other("mutation directory state disappeared"))?;
        let changed = tree_entries_changed(&state.entries, &entries);
        let empty = tree_entry_count_after_edits(&state.entries, &entries) == 0;
        let oid = if !changed {
            state.oid
        } else if depth > 0 && empty {
            None
        } else {
            let oid = encode_tree_with_edits(
                state,
                &state.entries,
                &entries,
                &mut objects,
                &mut tree_delta_bases,
            )?;
            Some(oid)
        };
        directory_edits.push(DirectoryEdit {
            path: paths[depth].clone(),
            oid,
            entries,
            generated: changed && !(depth > 0 && empty),
        });
        if depth == 0 {
            tree_oid = oid;
            break;
        }
        let directory_name = &directories[depth - 1];
        entries = vec![TreeEntryEdit {
            name: directory_name.clone(),
            replacement: oid.map(|oid| tree::Entry {
                mode: tree::EntryKind::Tree.into(),
                filename: BString::from(directory_name.clone()),
                oid,
            }),
        }];
    }
    let tree = tree_oid.ok_or_else(|| std::io::Error::other("root tree was not encoded"))?;
    let commit_bytes = commit_bytes(tree, parent, principal, seconds, attribute_digest.as_str());
    let commit = object_id(Kind::Commit, &commit_bytes)?;
    objects.push((Kind::Commit, commit_bytes));
    tree_state.commit(directory_edits, generated_attributes)?;
    for (path, attributes) in &attribute_changes {
        manifest.update(path.clone(), attributes.clone());
    }
    Ok(Build::Commit(Box::new(BuiltCommit {
        parent,
        commit,
        etag,
        objects,
        tree_delta_bases,
        attribute_changes,
    })))
}

fn put_condition_matches(
    condition: &PutCondition,
    old: Option<&(ObjectId, EntryMode)>,
    current_attributes: Option<&attributes::ObjectAttributes>,
) -> bool {
    match condition {
        PutCondition::None => true,
        PutCondition::IfNoneMatchAny => old.is_none(),
        PutCondition::IfMatch {
            object,
            etag,
            attributes_present,
        } => object_condition_matches(*object, etag, *attributes_present, old, current_attributes),
    }
}

fn validate_delete_condition(
    condition: &DeleteCondition,
    old: Option<&(ObjectId, EntryMode)>,
    current_attributes: Option<&attributes::ObjectAttributes>,
) -> Result<()> {
    match condition {
        DeleteCondition::None => Ok(()),
        DeleteCondition::IfMatchAny { missing } if old.is_none() => {
            Err(missing_delete_error(*missing))
        }
        DeleteCondition::IfMatchAny { .. } => Ok(()),
        DeleteCondition::IfMatch { missing, .. } if old.is_none() => {
            Err(missing_delete_error(*missing))
        }
        DeleteCondition::IfMatch {
            object,
            etag,
            attributes_present,
            missing: _,
        } if object_condition_matches(
            *object,
            etag,
            *attributes_present,
            old,
            current_attributes,
        ) =>
        {
            Ok(())
        }
        DeleteCondition::IfMatch { .. } => Err(Error::PreconditionFailed),
    }
}

fn missing_delete_error(result: MissingDeleteResult) -> Error {
    match result {
        MissingDeleteResult::PreconditionFailed => Error::PreconditionFailed,
        MissingDeleteResult::NotFound => Error::ConditionalTargetMissing,
    }
}

fn object_condition_matches(
    object: ObjectId,
    etag: &str,
    attributes_present: bool,
    old: Option<&(ObjectId, EntryMode)>,
    current_attributes: Option<&attributes::ObjectAttributes>,
) -> bool {
    old.is_some_and(|(oid, _)| *oid == object)
        && match (attributes_present, current_attributes) {
            (_, Some(value)) => value.etag == etag,
            (true, None) => false,
            (false, None) => true,
        }
}

struct LfsAttributesChange {
    oid: ObjectId,
    mode: gix_object::tree::EntryMode,
    bytes: Vec<u8>,
}

async fn prepare_lfs_attributes(
    tree_state: &MutableTree,
    directory: &crab_remote_git::GitPath,
    entries: &[tree::Entry],
    filename: &[u8],
    operation: Option<&crab_remote_git::OperationContext>,
) -> Result<Option<LfsAttributesChange>> {
    let attributes_entry = find_entry(entries, b".gitattributes");
    let (mut bytes, mode, old_oid) = match attributes_entry {
        Some(entry) => {
            let mode = entry_mode(entry.mode)?;
            if !matches!(mode, EntryMode::Regular | EntryMode::Executable) {
                return Err(Error::InvalidAttributes);
            }
            let path = push_component(directory, b".gitattributes")?;
            let bytes = tree_state.blob(&path, entry.oid, operation).await?;
            if !matches!(
                crab_git::classify(&bytes),
                crab_git::PointerKind::NotAPointer
            ) {
                return Err(Error::InvalidAttributes);
            }
            (bytes, entry.mode, Some(entry.oid))
        }
        None => (Vec::new(), tree::EntryKind::Blob.into(), None),
    };
    let line = lfs_attributes_line(filename);
    if bytes
        .split(|byte| *byte == b'\n')
        .any(|existing| existing.strip_suffix(b"\r").unwrap_or(existing) == line.as_bytes())
    {
        return Ok(None);
    }
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    bytes.extend_from_slice(line.as_bytes());
    bytes.push(b'\n');
    let oid = object_id(Kind::Blob, &bytes)?;
    if old_oid == Some(oid) {
        return Ok(None);
    }
    Ok(Some(LfsAttributesChange { oid, mode, bytes }))
}

fn lfs_attributes_line(filename: &[u8]) -> String {
    format!(
        "{} filter=lfs diff=lfs merge=lfs -text",
        quote_literal_attribute_pattern(filename)
    )
}

fn quote_literal_attribute_pattern(filename: &[u8]) -> String {
    let mut pattern = Vec::with_capacity(filename.len() + 1);
    pattern.push(b'/');
    for byte in filename {
        if matches!(byte, b'\\' | b'*' | b'?' | b'[' | b']') {
            pattern.push(b'\\');
        }
        pattern.push(*byte);
    }

    let mut quoted = String::with_capacity(pattern.len() + 2);
    quoted.push('"');
    for byte in pattern {
        match byte {
            b'"' => quoted.push_str("\\\""),
            b'\\' => quoted.push_str("\\\\"),
            0x20..=0x7e => quoted.push(char::from(byte)),
            _ => {
                quoted.push('\\');
                quoted.push(char::from(b'0' + ((byte >> 6) & 0o7)));
                quoted.push(char::from(b'0' + ((byte >> 3) & 0o7)));
                quoted.push(char::from(b'0' + (byte & 0o7)));
            }
        }
    }
    quoted.push('"');
    quoted
}

async fn load_directory(
    snapshot: &crab_remote_git::RemoteGitSnapshot,
    path: &crab_remote_git::GitPath,
    operation: &crab_remote_git::OperationContext,
) -> Result<Vec<tree::Entry>> {
    let mut cursor = None;
    let mut entries = Vec::new();
    loop {
        let page = snapshot
            .list_directory(
                path,
                &crab_remote_git::PageRequest::new(TREE_PAGE_SIZE, cursor)?,
                operation,
            )
            .await?;
        entries.extend(page.items.into_iter().map(tree_entry));
        let Some(next) = page.next else {
            break;
        };
        cursor = Some(next);
    }
    entries.sort();
    Ok(entries)
}

fn tree_entry(entry: crab_remote_git::TreeEntry) -> tree::Entry {
    let name = entry
        .path
        .components()
        .last()
        .map(<[u8]>::to_vec)
        .unwrap_or_default();
    tree::Entry {
        mode: git_entry_mode(entry.mode),
        filename: BString::from(name),
        oid: entry.oid,
    }
}

fn entry_mode(mode: gix_object::tree::EntryMode) -> Result<EntryMode> {
    Ok(match mode.kind() {
        tree::EntryKind::Tree => EntryMode::Tree,
        tree::EntryKind::Blob => EntryMode::Regular,
        tree::EntryKind::BlobExecutable => EntryMode::Executable,
        tree::EntryKind::Link => EntryMode::Symlink,
        tree::EntryKind::Commit => EntryMode::Submodule,
    })
}

fn git_entry_mode(mode: EntryMode) -> gix_object::tree::EntryMode {
    match mode {
        EntryMode::Tree => tree::EntryKind::Tree,
        EntryMode::Regular => tree::EntryKind::Blob,
        EntryMode::Executable => tree::EntryKind::BlobExecutable,
        EntryMode::Symlink => tree::EntryKind::Link,
        EntryMode::Submodule => tree::EntryKind::Commit,
    }
    .into()
}

fn find_entry<'a>(entries: &'a [tree::Entry], name: &[u8]) -> Option<&'a tree::Entry> {
    entry_index(entries, name).map(|index| &entries[index])
}

fn entry_index(entries: &[tree::Entry], name: &[u8]) -> Option<usize> {
    [tree::EntryKind::Blob, tree::EntryKind::Tree]
        .into_iter()
        .find_map(|kind| {
            entries
                .binary_search(&tree::Entry {
                    mode: kind.into(),
                    filename: BString::from(name),
                    oid: ObjectId::empty_blob(gix_hash::Kind::Sha1),
                })
                .ok()
        })
}

fn push_component(
    parent: &crab_remote_git::GitPath,
    component: &[u8],
) -> Result<crab_remote_git::GitPath> {
    let mut path = parent.as_bytes().to_vec();
    if !path.is_empty() {
        path.push(b'/');
    }
    path.extend_from_slice(component);
    crab_remote_git::GitPath::new(path).map_err(Error::Remote)
}

fn tree_entries_changed(entries: &[tree::Entry], edits: &[TreeEntryEdit]) -> bool {
    edits
        .iter()
        .any(|edit| find_entry(entries, &edit.name) != edit.replacement.as_ref())
}

fn tree_entry_count_after_edits(entries: &[tree::Entry], edits: &[TreeEntryEdit]) -> usize {
    edits.iter().fold(entries.len(), |count, edit| {
        match (
            find_entry(entries, &edit.name).is_some(),
            edit.replacement.is_some(),
        ) {
            (true, false) => count.saturating_sub(1),
            (false, true) => count.saturating_add(1),
            (true, true) | (false, false) => count,
        }
    })
}

fn tree_entry_after_edits<'a>(
    entries: &'a [tree::Entry],
    edits: &'a [TreeEntryEdit],
    name: &[u8],
) -> Option<&'a tree::Entry> {
    edits
        .iter()
        .rev()
        .find(|edit| edit.name == name)
        .map_or_else(
            || find_entry(entries, name),
            |edit| edit.replacement.as_ref(),
        )
}

fn generated_attribute_update(
    tree_state: &MutableTree,
    path: &[u8],
    entries: &[tree::Entry],
    edits: &[TreeEntryEdit],
    objects: &[(Kind, Vec<u8>)],
) -> Result<Option<(Vec<u8>, Option<GeneratedAttributeBlob>)>> {
    let current = tree_state.generated_attribute_blobs.get(path);
    let Some(attributes) = tree_entry_after_edits(entries, edits, b".gitattributes") else {
        return Ok(current.is_some().then(|| (path.to_vec(), None)));
    };
    for (kind, bytes) in objects {
        if *kind == Kind::Blob && object_id(*kind, bytes)? == attributes.oid {
            return Ok(Some((
                path.to_vec(),
                Some(GeneratedAttributeBlob {
                    oid: attributes.oid,
                    bytes: bytes.clone(),
                }),
            )));
        }
    }
    Ok(match current {
        Some(current) if current.oid == attributes.oid => None,
        Some(_) => Some((path.to_vec(), None)),
        None => None,
    })
}

fn encode_tree_with_edits(
    old: &DirectoryState,
    entries: &[tree::Entry],
    edits: &[TreeEntryEdit],
    objects: &mut Vec<(Kind, Vec<u8>)>,
    delta_bases: &mut Vec<TreeDeltaBase>,
) -> Result<ObjectId> {
    let mut replacements = edits
        .iter()
        .filter_map(|edit| edit.replacement.as_ref())
        .collect::<Vec<_>>();
    replacements.sort_unstable();
    let mut existing = entries
        .iter()
        .filter(|entry| {
            let filename: &[u8] = entry.filename.as_ref();
            !edits.iter().any(|edit| edit.name == filename)
        })
        .peekable();
    let mut replacements = replacements.into_iter().peekable();
    let mut bytes = Vec::new();
    loop {
        let entry = match (existing.peek(), replacements.peek()) {
            (Some(existing), Some(replacement)) if replacement < existing => replacements.next(),
            (Some(_), Some(_)) | (Some(_), None) => existing.next(),
            (None, Some(_)) => replacements.next(),
            (None, None) => break,
        };
        let entry = entry.ok_or_else(|| std::io::Error::other("tree merge lost an entry"))?;
        append_tree_entry(&mut bytes, entry);
    }
    let oid = object_id(Kind::Tree, &bytes)?;
    objects.push((Kind::Tree, bytes));
    if let Some(base) = old.oid {
        let external = if old.generated_in_batch {
            None
        } else {
            old.delta_depth
                .filter(|depth| *depth < GENERATED_TREE_DELTA_DEPTH)
                .map(|depth| ExternalTreeDeltaBase {
                    bytes: encode_tree(entries),
                    depth,
                })
        };
        if old.generated_in_batch || external.is_some() {
            delta_bases.push(TreeDeltaBase {
                object: oid,
                base,
                external,
            });
        }
    }
    Ok(oid)
}

fn encode_tree(entries: &[tree::Entry]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for entry in entries {
        append_tree_entry(&mut bytes, entry);
    }
    bytes
}

fn append_tree_entry(bytes: &mut Vec<u8>, entry: &tree::Entry) {
    let mut mode = [0; 6];
    bytes.extend_from_slice(entry.mode.as_bytes(&mut mode));
    bytes.push(b' ');
    bytes.extend_from_slice(&entry.filename);
    bytes.push(0);
    bytes.extend_from_slice(entry.oid.as_bytes());
}

fn commit_bytes(
    tree: ObjectId,
    parent: Option<ObjectId>,
    principal: &str,
    seconds: u64,
    attribute_digest: &str,
) -> Vec<u8> {
    let name: String = principal
        .chars()
        .filter(|character| !matches!(character, '<' | '>' | '\n' | '\r' | '\0'))
        .take(160)
        .collect();
    let name = if name.trim().is_empty() {
        "Crab S3 user"
    } else {
        name.trim()
    };
    let email = blake3::hash(principal.as_bytes()).to_hex();
    let parent = parent
        .map(|oid| format!("parent {oid}\n"))
        .unwrap_or_default();
    format!(
        "tree {tree}\n{parent}author {name} <{email}@users.crab.invalid> {seconds} +0000\ncommitter {name} <{email}@users.crab.invalid> {seconds} +0000\n\nUpdate object through Crab S3 gateway\n\nCrab-S3-Attributes: {attribute_digest}\n"
    )
    .into_bytes()
}

fn object_id(kind: Kind, bytes: &[u8]) -> std::result::Result<ObjectId, gix_hash::hasher::Error> {
    gix_object::compute_hash(gix_hash::Kind::Sha1, kind, bytes)
}

async fn prepare_pack(
    objects: Vec<(Kind, Vec<u8>)>,
    delta_bases: BTreeMap<ObjectId, ObjectId>,
    external_delta_bases: BTreeMap<ObjectId, crab_git::incoming_pack::ExternalDeltaBase>,
    cancel: &CancellationToken,
) -> Result<(tempfile::TempDir, crab_git::incoming_pack::PreparedPack)> {
    let cancelled = Arc::new(AtomicBool::new(cancel.is_cancelled()));
    let watched = cancel.clone();
    let flag = Arc::clone(&cancelled);
    let watcher = tokio::spawn(async move {
        watched.cancelled().await;
        flag.store(true, Ordering::Release);
    });
    let worker_flag = Arc::clone(&cancelled);
    let result = tokio::task::spawn_blocking(move || {
        let owner = tempfile::tempdir()?;
        let incoming = crab_git::incoming_pack::IncomingPack::from_generated_objects(
            objects,
            owner.path(),
            crab_git::incoming_pack::ReceiveLimits {
                max_pack_bytes: MAX_GENERATED_PACK_BYTES,
                max_objects: 1_000_000,
                max_object_bytes: 256 * 1024 * 1024,
                max_inflated_bytes: MAX_GENERATED_PACK_BYTES,
                max_delta_depth: 1,
            },
            || worker_flag.load(Ordering::Acquire),
        )?;
        let prepared = incoming
            .prepare_with_external_delta_bases(
                owner.path(),
                MAX_GENERATED_PACK_BYTES,
                &worker_flag,
                &delta_bases,
                &external_delta_bases,
                GENERATED_TREE_DELTA_DEPTH,
                MAX_GENERATED_TREE_DELTA_BYTES,
            )?
            .ok_or_else(|| std::io::Error::other("generated pack contains no objects"))?;
        Ok::<_, Error>((owner, prepared))
    })
    .await?;
    watcher.abort();
    result
}

fn now_seconds() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RepositoryAccess, RepositoryConfig};
    use crab_coordination::GIT_OBJECT_LOCATOR_RESOURCE;
    use gix_object::WriteTo as _;
    use std::collections::BTreeMap;

    fn mutation(path: &[u8], upload_id: Option<&str>) -> Mutation {
        Mutation {
            path: crab_remote_git::GitPath::new(path.to_vec()).unwrap(),
            change: Change::Put {
                bytes: Bytes::from_static(b"content"),
                track_lfs: false,
                attributes: Box::new(attributes::PutAttributes {
                    completion_upload_id: upload_id.map(str::to_owned),
                    ..Default::default()
                }),
                condition: PutCondition::None,
            },
            principal: "user".to_owned(),
        }
    }

    #[test]
    fn generated_pack_reservation_covers_peak_temporary_copies() {
        let objects = vec![
            (Kind::Blob, vec![0; 64 * 1024]),
            (Kind::Tree, vec![0; 1024]),
        ];
        let bytes = 64 * 1024 + 1024;

        assert_eq!(
            pack_scratch_reservation(&objects),
            bytes * 4 + bytes / 4 + 2 * 64 + PACK_SCRATCH_BASE_BYTES
        );
    }

    #[test]
    fn tree_encoder_matches_the_canonical_git_encoding() {
        let mut entries = vec![
            tree::Entry {
                mode: tree::EntryKind::Blob.into(),
                filename: BString::from("z.txt"),
                oid: ObjectId::empty_blob(gix_hash::Kind::Sha1),
            },
            tree::Entry {
                mode: tree::EntryKind::Tree.into(),
                filename: BString::from("a"),
                oid: ObjectId::empty_tree(gix_hash::Kind::Sha1),
            },
        ];
        entries.sort();
        let mut expected = Vec::new();
        gix_object::Tree {
            entries: entries.clone(),
        }
        .write_to(&mut expected)
        .unwrap();
        let mut objects = Vec::new();
        let mut delta_bases = Vec::new();

        let oid = encode_tree_with_edits(
            &DirectoryState {
                oid: None,
                entries: entries.clone(),
                delta_depth: None,
                generated_in_batch: false,
            },
            &entries,
            &[],
            &mut objects,
            &mut delta_bases,
        )
        .unwrap();

        assert_eq!(oid, object_id(Kind::Tree, &expected).unwrap());
        assert_eq!(objects, vec![(Kind::Tree, expected)]);
        assert!(delta_bases.is_empty());
    }

    #[test]
    fn virtual_tree_edits_match_canonical_insert_replace_and_delete() {
        let original = vec![
            tree::Entry {
                mode: tree::EntryKind::Blob.into(),
                filename: BString::from("a.txt"),
                oid: ObjectId::from([1; 20]),
            },
            tree::Entry {
                mode: tree::EntryKind::Tree.into(),
                filename: BString::from("folder"),
                oid: ObjectId::from([2; 20]),
            },
            tree::Entry {
                mode: tree::EntryKind::Blob.into(),
                filename: BString::from("z.txt"),
                oid: ObjectId::from([3; 20]),
            },
        ];
        let edits = vec![
            TreeEntryEdit {
                name: b"a.txt".to_vec(),
                replacement: None,
            },
            TreeEntryEdit {
                name: b"middle.txt".to_vec(),
                replacement: Some(tree::Entry {
                    mode: tree::EntryKind::Blob.into(),
                    filename: BString::from("middle.txt"),
                    oid: ObjectId::from([4; 20]),
                }),
            },
            TreeEntryEdit {
                name: b"z.txt".to_vec(),
                replacement: Some(tree::Entry {
                    mode: tree::EntryKind::BlobExecutable.into(),
                    filename: BString::from("z.txt"),
                    oid: ObjectId::from([5; 20]),
                }),
            },
        ];
        let mut expected_entries = vec![
            original[1].clone(),
            edits[1].replacement.clone().unwrap(),
            edits[2].replacement.clone().unwrap(),
        ];
        expected_entries.sort();
        let mut expected = Vec::new();
        gix_object::Tree {
            entries: expected_entries,
        }
        .write_to(&mut expected)
        .unwrap();
        let mut objects = Vec::new();
        let mut delta_bases = Vec::new();

        let oid = encode_tree_with_edits(
            &DirectoryState {
                oid: Some(ObjectId::from([9; 20])),
                entries: original.clone(),
                delta_depth: Some(0),
                generated_in_batch: true,
            },
            &original,
            &edits,
            &mut objects,
            &mut delta_bases,
        )
        .unwrap();

        assert_eq!(tree_entry_count_after_edits(&original, &edits), 3);
        assert!(tree_entries_changed(&original, &edits));
        assert_eq!(oid, object_id(Kind::Tree, &expected).unwrap());
        assert_eq!(objects, vec![(Kind::Tree, expected)]);
        assert_eq!(delta_bases.len(), 1);
        assert_eq!(delta_bases[0].object, oid);
        assert_eq!(delta_bases[0].base, ObjectId::from([9; 20]));
        assert!(delta_bases[0].external.is_none());
        assert_eq!(original[0].oid, ObjectId::from([1; 20]));
    }

    #[test]
    fn warm_tree_delta_base_is_external_and_resets_at_the_depth_limit() {
        let original = vec![tree::Entry {
            mode: tree::EntryKind::Blob.into(),
            filename: BString::from("existing"),
            oid: ObjectId::from([1; 20]),
        }];
        let edit = TreeEntryEdit {
            name: b"added".to_vec(),
            replacement: Some(tree::Entry {
                mode: tree::EntryKind::Blob.into(),
                filename: BString::from("added"),
                oid: ObjectId::from([2; 20]),
            }),
        };
        let old_oid = object_id(Kind::Tree, &encode_tree(&original)).unwrap();
        let mut objects = Vec::new();
        let mut bases = Vec::new();
        let state = DirectoryState {
            oid: Some(old_oid),
            entries: original.clone(),
            delta_depth: Some(0),
            generated_in_batch: false,
        };

        let target = encode_tree_with_edits(
            &state,
            &original,
            std::slice::from_ref(&edit),
            &mut objects,
            &mut bases,
        )
        .unwrap();

        assert_eq!(bases.len(), 1);
        assert_eq!(bases[0].object, target);
        assert_eq!(bases[0].base, old_oid);
        let external = bases[0].external.as_ref().unwrap();
        assert_eq!(external.bytes, encode_tree(&original));
        assert_eq!(external.depth, 0);

        let mut objects = Vec::new();
        let mut bases = Vec::new();
        encode_tree_with_edits(
            &DirectoryState {
                delta_depth: Some(GENERATED_TREE_DELTA_DEPTH),
                ..state.clone()
            },
            &original,
            std::slice::from_ref(&edit),
            &mut objects,
            &mut bases,
        )
        .unwrap();
        assert!(bases.is_empty());

        encode_tree_with_edits(
            &DirectoryState {
                delta_depth: None,
                ..state
            },
            &original,
            &[edit],
            &mut objects,
            &mut bases,
        )
        .unwrap();
        assert!(bases.is_empty());
    }

    #[test]
    fn entry_lookup_preserves_git_tree_order_around_directories() {
        let mut entries = vec![
            tree::Entry {
                mode: tree::EntryKind::Tree.into(),
                filename: BString::from("name"),
                oid: ObjectId::empty_tree(gix_hash::Kind::Sha1),
            },
            tree::Entry {
                mode: tree::EntryKind::Blob.into(),
                filename: BString::from("name.ext"),
                oid: ObjectId::empty_blob(gix_hash::Kind::Sha1),
            },
        ];
        entries.sort();

        assert_eq!(
            find_entry(&entries, b"name").map(|entry| entry.mode.kind()),
            Some(tree::EntryKind::Tree)
        );
        assert!(find_entry(&entries, b"missing").is_none());
    }

    #[test]
    fn multipart_completion_stays_outside_ordinary_batches() {
        let admission = WriteAdmission::default();
        let first = admission
            .enqueue("repo", "refs/heads/main", mutation(b"first", None))
            .unwrap();
        let planned = admission
            .enqueue(
                "repo",
                "refs/heads/main",
                mutation(b"planned", Some("upload")),
            )
            .unwrap();
        let last = admission
            .enqueue("repo", "refs/heads/main", mutation(b"last", None))
            .unwrap();
        assert!(Arc::ptr_eq(&first.queue, &planned.queue));
        assert!(Arc::ptr_eq(&first.queue, &last.queue));

        let ordinary = first.queue.take_batch().unwrap();
        assert_eq!(ordinary.len(), 1);
        assert!(ordinary[0].mutation.completion_plan().is_none());
        let completion = first.queue.take_batch().unwrap();
        assert_eq!(completion.len(), 1);
        assert!(completion[0].mutation.completion_plan().is_some());
        let trailing = first.queue.take_batch().unwrap();
        assert_eq!(trailing.len(), 1);
        assert!(trailing[0].mutation.completion_plan().is_none());
    }

    #[test]
    fn warm_branch_state_is_reused_only_for_the_exact_tip() {
        let cache = BranchState::new(Arc::new(AtomicUsize::new(0)));
        let tip = ObjectId::empty_tree(gix_hash::Kind::Sha1);
        let blob = ObjectId::empty_blob(gix_hash::Kind::Sha1);
        let mut manifest = attributes::Manifest::default();
        manifest.update(
            "object".to_owned(),
            Some(attributes::ObjectAttributes::new(
                blob,
                "etag".to_owned(),
                0,
                0,
                attributes::PutAttributes::default(),
            )),
        );
        cache.store(Some(tip), None, manifest, WarmTree::default(), 0);

        assert!(
            cache
                .manifest(Some(tip))
                .unwrap()
                .object("object", blob)
                .is_some()
        );
        let state = cache.take(Some(tip)).unwrap();
        assert!(state.manifest.object("object", blob).is_some());
        cache.store(
            Some(tip),
            state.transaction,
            Arc::try_unwrap(state.manifest).unwrap(),
            state.tree,
            0,
        );
        assert!(cache.take(None).is_none());
    }

    #[test]
    fn idle_branch_state_is_retained_below_memory_pressure() {
        let admission = WriteAdmission::default();
        let first = admission
            .enqueue("repo", "refs/heads/first", mutation(b"first", None))
            .unwrap();
        let first_queue = Arc::clone(&first.queue);
        drop(first);
        let tip = ObjectId::empty_tree(gix_hash::Kind::Sha1);
        first_queue.state.store(
            Some(tip),
            None,
            attributes::Manifest::default(),
            WarmTree::default(),
            0,
        );

        let second = admission
            .enqueue("repo", "refs/heads/second", mutation(b"second", None))
            .unwrap();

        assert!(first_queue.state.take(Some(tip)).is_some());
        drop(second);
    }

    async fn fixture() -> (
        Repository,
        Arc<crab_remote_git::RemoteGitRuntime>,
        CancellationToken,
    ) {
        let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
        let layout = crab_storage::StoreLayout::new(store.clone(), "s3-test".to_owned());
        crab_write::initialize::initialize_repository(&store, &layout, "refs/heads/main")
            .await
            .unwrap();
        let repository = Repository::new(
            RepositoryConfig {
                name: "repo".to_owned(),
                provider: crab_storage::StorageProviderKind::Local,
                bucket: "memory".to_owned(),
                prefix: "s3-test".to_owned(),
                default_branch: "main".to_owned(),
                members: vec![crate::RepositoryMember {
                    principal: "user".to_owned(),
                    access: RepositoryAccess::Write,
                }],
                protected_branches: vec![],
                git_blob_max_bytes: 1024 * 1024,
                max_active_multipart_uploads: 16,
                multipart_staging_bytes_per_upload: 50_000_000_000_000,
                multipart_upload_ttl_seconds: 604_800,
            },
            store,
        )
        .unwrap();
        (
            repository,
            Arc::new(crab_remote_git::RemoteGitRuntime::default()),
            CancellationToken::new(),
        )
    }

    async fn read(
        repository: &Repository,
        runtime: Arc<crab_remote_git::RemoteGitRuntime>,
        cancel: &CancellationToken,
        path: &str,
    ) -> Bytes {
        let view = repository
            .read_views
            .current(
                repository,
                runtime,
                crab_remote_git::RepositoryOptions::default(),
                cancel,
            )
            .await
            .unwrap();
        let operation = view
            .remote()
            .operation(OperationKind::Repository, cancel)
            .await
            .unwrap();
        let snapshot = view.snapshot("refs/heads/main", &operation).await.unwrap();
        let blob = snapshot
            .read_blob(
                &crab_remote_git::GitPath::new(path.as_bytes().to_vec()).unwrap(),
                &operation,
            )
            .await
            .unwrap();
        operation.finish(Ok(())).await.unwrap();
        blob.bytes
    }

    async fn tip(
        repository: &Repository,
        runtime: Arc<crab_remote_git::RemoteGitRuntime>,
        cancel: &CancellationToken,
    ) -> ObjectId {
        repository
            .read_views
            .current(
                repository,
                runtime,
                crab_remote_git::RepositoryOptions::default(),
                cancel,
            )
            .await
            .unwrap()
            .remote()
            .refs()
            .entries
            .iter()
            .find(|reference| reference.name == "refs/heads/main")
            .unwrap()
            .target
    }

    async fn coordinated_put(
        coordinator: &Coordinator,
        repository: &Repository,
        cancel: &CancellationToken,
        path: &str,
        body: &'static [u8],
    ) {
        coordinator
            .apply(
                repository,
                "refs/heads/main",
                &crab_remote_git::GitPath::new(path.as_bytes().to_vec()).unwrap(),
                Change::Put {
                    bytes: Bytes::from_static(body),
                    track_lfs: false,
                    attributes: Box::new(attributes::PutAttributes::default()),
                    condition: PutCondition::None,
                },
                "user",
                cancel,
            )
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sustained_write_maintenance_bounds_the_journal_and_advances_catalog() {
        let (repository, runtime, cancel) = fixture().await;
        let coordinator = Coordinator::new(
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            cancel.clone(),
            Metrics::new().unwrap(),
        );
        coordinated_put(&coordinator, &repository, &cancel, "object", b"value").await;
        let before = crab_metadata::manifest_store::read_repository_snapshot(
            &repository.store,
            &repository.layout,
        )
        .await
        .unwrap();
        assert_eq!(before.journal.transactions.len(), 1);

        crate::repository::schedule_catalog_maintenance(&repository, &cancel, 64);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = crab_metadata::manifest_store::read_repository_snapshot(
                    &repository.store,
                    &repository.layout,
                )
                .await
                .unwrap();
                if snapshot.journal.transactions.is_empty()
                    && !repository.maintenance.catalog_maintenance_is_running()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let read_view = repository
            .read_views
            .current(
                &repository,
                Arc::clone(&runtime),
                crab_remote_git::RepositoryOptions::default(),
                &cancel,
            )
            .await
            .unwrap();
        assert!(
            read_view
                .remote()
                .catalog_visibility_available(&cancel)
                .await
                .unwrap()
        );
        assert_eq!(
            read(&repository, Arc::clone(&runtime), &cancel, "object").await,
            Bytes::from_static(b"value")
        );
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn warm_existing_ref_publication_does_not_scan_repository_manifest() {
        let (repository, runtime, cancel) = fixture().await;
        let coordinator = Coordinator::new(
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            cancel.clone(),
            Metrics::new().unwrap(),
        );
        let manifest_lock = PushLock::acquire_internal(
            repository.store.inner(),
            repository.layout.repo_prefix(),
            crab_coordination::GIT_MANIFEST_RESOURCE,
            LOCK_TTL,
        )
        .await
        .unwrap();
        coordinated_put(&coordinator, &repository, &cancel, "first.txt", b"first").await;
        let first = crab_metadata::ref_journal::read_ref_head(
            &repository.store,
            &repository.layout,
            "refs/heads/main",
        )
        .await
        .unwrap()
        .visible_transaction
        .unwrap();
        let manifest_path = repository.layout.manifest_path();
        let (manifest, _) = repository
            .store
            .get_with_etag(&manifest_path)
            .await
            .unwrap();
        repository
            .store
            .put_overwrite(&manifest_path, Bytes::from_static(b"{"))
            .await
            .unwrap();

        coordinated_put(&coordinator, &repository, &cancel, "second.txt", b"second").await;
        let second = crab_metadata::ref_journal::read_ref_head(
            &repository.store,
            &repository.layout,
            "refs/heads/main",
        )
        .await
        .unwrap()
        .visible_transaction
        .unwrap();
        let transaction = crab_metadata::ref_journal::read_transaction(
            &repository.store,
            &repository.layout,
            &second,
        )
        .await
        .unwrap();

        assert_eq!(
            transaction.parents.get("refs/heads/main"),
            Some(&Some(first))
        );
        repository
            .store
            .put_overwrite(&manifest_path, manifest)
            .await
            .unwrap();
        manifest_lock.release().await.unwrap();
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sustained_warm_directory_writes_publish_readable_bounded_cross_pack_deltas() {
        let (repository, runtime, cancel) = fixture().await;
        let coordinator = Coordinator::new(
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            cancel.clone(),
            Metrics::new().unwrap(),
        );
        for index in 0..16 {
            coordinated_put(
                &coordinator,
                &repository,
                &cancel,
                &format!("objects/item-{index:02}"),
                b"value",
            )
            .await;
        }
        assert_eq!(
            read(
                &repository,
                Arc::clone(&runtime),
                &cancel,
                "objects/item-15"
            )
            .await,
            "value"
        );

        let view = repository
            .read_views
            .current(
                &repository,
                Arc::clone(&runtime),
                crab_remote_git::RepositoryOptions::default(),
                &cancel,
            )
            .await
            .unwrap();
        let operation = view
            .remote()
            .operation(OperationKind::Repository, &cancel)
            .await
            .unwrap();
        let snapshot = view.snapshot("refs/heads/main", &operation).await.unwrap();
        let directory = snapshot
            .entry(
                &crab_remote_git::GitPath::new(b"objects".to_vec()).unwrap(),
                &operation,
            )
            .await
            .unwrap()
            .unwrap();
        let head = crab_metadata::ref_journal::read_ref_head(
            &repository.store,
            &repository.layout,
            "refs/heads/main",
        )
        .await
        .unwrap();
        let transaction = crab_metadata::ref_journal::read_transaction(
            &repository.store,
            &repository.layout,
            head.visible_transaction.as_deref().unwrap(),
        )
        .await
        .unwrap();
        let pack = transaction.packs.first().unwrap();
        let (pack_bytes, _) = repository
            .store
            .get_with_etag(&repository.layout.pack_path(&pack.pack_id))
            .await
            .unwrap();
        let (index_bytes, _) = repository
            .store
            .get_with_etag(&repository.layout.pack_index_path(&pack.pack_id))
            .await
            .unwrap();
        let (reverse_bytes, _) = repository
            .store
            .get_with_etag(&repository.layout.pack_reverse_index_path(&pack.pack_id))
            .await
            .unwrap();
        let files = tempfile::tempdir().unwrap();
        let index_path = files.path().join("pack.idx");
        let reverse_path = files.path().join("pack.rev");
        std::fs::write(&index_path, index_bytes).unwrap();
        std::fs::write(&reverse_path, reverse_bytes).unwrap();
        let mut locations =
            crab_git::PackLocationIter::open(&index_path, &reverse_path, pack_bytes.len() as u64)
                .unwrap();
        let location = locations
            .find_map(|location| {
                let location = location.unwrap();
                (location.oid == directory.oid).then_some(location)
            })
            .unwrap();
        let start = usize::try_from(location.pack_offset).unwrap();
        let end = usize::try_from(location.pack_offset + location.entry_len).unwrap();
        let entry =
            gix_pack::data::Entry::from_bytes(&pack_bytes[start..end], location.pack_offset, 20)
                .unwrap();
        let gix_pack::data::entry::Header::RefDelta { base_id } = entry.header else {
            panic!("warm broad-directory tree must use a cross-pack REF_DELTA")
        };
        let base = operation.read_object(base_id).await.unwrap();
        assert_eq!(base.kind, Kind::Tree);
        let target = operation.read_object(directory.oid).await.unwrap();
        assert_eq!(target.kind, Kind::Tree);
        assert_eq!(object_id(Kind::Tree, &target.data).unwrap(), directory.oid);
        operation.finish(Ok(())).await.unwrap();
        coordinator.shutdown().await;
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn writes_initialize_branch_and_preserve_unrelated_files() {
        let (repository, runtime, cancel) = fixture().await;
        for (path, body) in [("a.txt", "alpha"), ("nested/b.txt", "bravo")] {
            apply(
                &repository,
                Arc::clone(&runtime),
                crab_remote_git::RepositoryOptions::default(),
                "refs/heads/main",
                &crab_remote_git::GitPath::new(path.as_bytes().to_vec()).unwrap(),
                Change::Put {
                    bytes: Bytes::copy_from_slice(body.as_bytes()),
                    track_lfs: false,
                    attributes: Box::new(attributes::PutAttributes::default()),
                    condition: PutCondition::None,
                },
                "user",
                &cancel,
            )
            .await
            .unwrap();
        }
        assert_eq!(
            read(&repository, Arc::clone(&runtime), &cancel, "a.txt").await,
            "alpha"
        );
        assert_eq!(
            read(&repository, Arc::clone(&runtime), &cancel, "nested/b.txt").await,
            "bravo"
        );
        apply(
            &repository,
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            "refs/heads/main",
            &crab_remote_git::GitPath::new(b"a.txt".to_vec()).unwrap(),
            Change::Delete {
                condition: DeleteCondition::default(),
            },
            "user",
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(
            read(&repository, Arc::clone(&runtime), &cancel, "nested/b.txt").await,
            "bravo"
        );
        runtime.shutdown().await;
    }

    #[test]
    fn literal_lfs_attribute_pattern_matches_only_the_exact_filename() {
        let directory = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(directory.path())
            .status()
            .unwrap();
        let filename = "model [v1] #*?.bin";
        std::fs::write(
            directory.path().join(".gitattributes"),
            format!("{}\n", lfs_attributes_line(filename.as_bytes())),
        )
        .unwrap();

        let exact = std::process::Command::new("git")
            .args(["check-attr", "filter", "--", filename])
            .current_dir(directory.path())
            .output()
            .unwrap();
        let similar = std::process::Command::new("git")
            .args(["check-attr", "filter", "--", "model av1] #xx.bin"])
            .current_dir(directory.path())
            .output()
            .unwrap();
        let descendant = std::process::Command::new("git")
            .args(["check-attr", "filter", "--", &format!("deeper/{filename}")])
            .current_dir(directory.path())
            .output()
            .unwrap();

        assert_eq!(
            String::from_utf8(exact.stdout).unwrap(),
            format!("{filename}: filter: lfs\n")
        );
        assert_eq!(
            String::from_utf8(similar.stdout).unwrap(),
            "model av1] #xx.bin: filter: unspecified\n"
        );
        assert_eq!(
            String::from_utf8(descendant.stdout).unwrap(),
            format!("deeper/{filename}: filter: unspecified\n")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lfs_put_commits_a_same_directory_tracking_rule() {
        let (repository, runtime, cancel) = fixture().await;
        let attributes_path =
            crab_remote_git::GitPath::new(b"nested/.gitattributes".to_vec()).unwrap();
        apply(
            &repository,
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            "refs/heads/main",
            &attributes_path,
            Change::Put {
                bytes: Bytes::from_static(b"README.md text\n"),
                track_lfs: false,
                attributes: Box::new(attributes::PutAttributes::default()),
                condition: PutCondition::None,
            },
            "user",
            &cancel,
        )
        .await
        .unwrap();
        let filename = "model [v1] #*?.bin";
        let path =
            crab_remote_git::GitPath::new(format!("nested/{filename}").into_bytes()).unwrap();
        let pointer = crab_git::LfsPointer {
            oid: [7; 32],
            size: 10 * 1024 * 1024,
            extensions: Vec::new(),
        }
        .serialize();
        let change = Change::Put {
            bytes: Bytes::from(pointer.clone()),
            track_lfs: true,
            attributes: Box::new(attributes::PutAttributes {
                logical_size: Some(10 * 1024 * 1024),
                ..Default::default()
            }),
            condition: PutCondition::None,
        };
        apply(
            &repository,
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            "refs/heads/main",
            &path,
            change.clone(),
            "user",
            &cancel,
        )
        .await
        .unwrap();
        apply(
            &repository,
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            "refs/heads/main",
            &path,
            change,
            "user",
            &cancel,
        )
        .await
        .unwrap();

        let stored_attributes = read(
            &repository,
            Arc::clone(&runtime),
            &cancel,
            "nested/.gitattributes",
        )
        .await;
        assert_eq!(
            stored_attributes,
            format!(
                "README.md text\n{}\n",
                lfs_attributes_line(filename.as_bytes())
            )
        );
        let commit = tip(&repository, Arc::clone(&runtime), &cancel).await;
        let manifest = attributes::load(&repository, commit).await.unwrap();
        let attributes_oid = object_id(Kind::Blob, &stored_attributes).unwrap();
        let attributes_etag = crate::gateway::md5_hex(&stored_attributes);
        assert!(manifest.is_complete());
        assert_eq!(
            manifest
                .object("nested/.gitattributes", attributes_oid)
                .map(|attributes| attributes.etag.as_str()),
            Some(attributes_etag.as_str())
        );
        assert_eq!(
            read(
                &repository,
                Arc::clone(&runtime),
                &cancel,
                &format!("nested/{filename}"),
            )
            .await,
            pointer
        );
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn batched_lfs_puts_extend_the_generated_attribute_blob() {
        let (repository, runtime, cancel) = fixture().await;
        let metrics = Metrics::new().unwrap();
        let mutation = |name: &str, marker: u8| Mutation {
            path: crab_remote_git::GitPath::new(format!("nested/{name}").into_bytes()).unwrap(),
            change: Change::Put {
                bytes: Bytes::from(
                    crab_git::LfsPointer {
                        oid: [marker; 32],
                        size: 10 * 1024 * 1024,
                        extensions: Vec::new(),
                    }
                    .serialize(),
                ),
                track_lfs: true,
                attributes: Box::new(attributes::PutAttributes {
                    logical_size: Some(10 * 1024 * 1024),
                    ..Default::default()
                }),
                condition: PutCondition::None,
            },
            principal: "user".to_owned(),
        };
        let results = apply_batch_admitted(
            &repository,
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            vec![mutation("first.bin", 1), mutation("second.bin", 2)],
            ApplyRequest {
                branch: "refs/heads/main",
                plan_id: None,
                metrics: &metrics,
                state: None,
                checkpoints: None,
            },
            &cancel,
        )
        .await
        .unwrap();
        assert!(results.iter().all(Result::is_ok));

        let stored = read(
            &repository,
            Arc::clone(&runtime),
            &cancel,
            "nested/.gitattributes",
        )
        .await;
        assert_eq!(
            stored,
            format!(
                "{}\n{}\n",
                lfs_attributes_line(b"first.bin"),
                lfs_attributes_line(b"second.bin")
            )
        );
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lfs_put_rejects_pointer_backed_attributes() {
        let (repository, runtime, cancel) = fixture().await;
        let attributes_path =
            crab_remote_git::GitPath::new(b"nested/.gitattributes".to_vec()).unwrap();
        let attributes_pointer = crab_git::LfsPointer {
            oid: [3; 32],
            size: 1,
            extensions: Vec::new(),
        }
        .serialize();
        apply(
            &repository,
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            "refs/heads/main",
            &attributes_path,
            Change::Put {
                bytes: Bytes::from(attributes_pointer),
                track_lfs: false,
                attributes: Box::new(attributes::PutAttributes::default()),
                condition: PutCondition::None,
            },
            "user",
            &cancel,
        )
        .await
        .unwrap();

        let result = apply(
            &repository,
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            "refs/heads/main",
            &crab_remote_git::GitPath::new(b"nested/object.bin".to_vec()).unwrap(),
            Change::Put {
                bytes: Bytes::from_static(b"pointer"),
                track_lfs: true,
                attributes: Box::new(attributes::PutAttributes::default()),
                condition: PutCondition::None,
            },
            "user",
            &cancel,
        )
        .await;

        assert!(matches!(result, Err(Error::InvalidAttributes)));
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn multipart_completion_token_makes_publication_retry_idempotent() {
        let (repository, runtime, cancel) = fixture().await;
        let path = crab_remote_git::GitPath::new(b"object.bin".to_vec()).unwrap();
        let change = Change::Put {
            bytes: Bytes::from_static(b"multipart content"),
            track_lfs: false,
            attributes: Box::new(attributes::PutAttributes {
                etag_override: Some("multipart-etag-1".to_owned()),
                completion_upload_id: Some("upload-id".to_owned()),
                ..Default::default()
            }),
            condition: PutCondition::None,
        };
        apply(
            &repository,
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            "refs/heads/main",
            &path,
            change.clone(),
            "user",
            &cancel,
        )
        .await
        .unwrap();
        let first = tip(&repository, Arc::clone(&runtime), &cancel).await;
        apply(
            &repository,
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            "refs/heads/main",
            &path,
            Change::Put {
                bytes: Bytes::from_static(b"newer content"),
                track_lfs: false,
                attributes: Box::default(),
                condition: PutCondition::None,
            },
            "user",
            &cancel,
        )
        .await
        .unwrap();
        let overwritten = tip(&repository, Arc::clone(&runtime), &cancel).await;
        apply(
            &repository,
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            "refs/heads/main",
            &path,
            change,
            "user",
            &cancel,
        )
        .await
        .unwrap();
        let recovered = tip(&repository, Arc::clone(&runtime), &cancel).await;
        assert!(first != overwritten && recovered == overwritten);
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn attribute_only_mutation_preserves_object_bytes() {
        let (repository, runtime, cancel) = fixture().await;
        let path = crab_remote_git::GitPath::new(b"object.bin".to_vec()).unwrap();
        let bytes = Bytes::from_static(b"stable content");
        apply(
            &repository,
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            "refs/heads/main",
            &path,
            Change::Put {
                bytes: bytes.clone(),
                track_lfs: false,
                attributes: Box::new(attributes::PutAttributes::default()),
                condition: PutCondition::None,
            },
            "user",
            &cancel,
        )
        .await
        .unwrap();
        let oid = object_id(Kind::Blob, &bytes).unwrap();
        let mut tags = BTreeMap::new();
        tags.insert("project".to_owned(), "crab".to_owned());
        apply(
            &repository,
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            "refs/heads/main",
            &path,
            Change::Attributes {
                expected: oid,
                attributes: Box::new(attributes::PutAttributes {
                    etag_override: Some(crate::gateway::md5_hex(&bytes)),
                    logical_size: Some(bytes.len() as u64),
                    tags,
                    ..Default::default()
                }),
            },
            "user",
            &cancel,
        )
        .await
        .unwrap();

        assert_eq!(
            read(&repository, Arc::clone(&runtime), &cancel, "object.bin").await,
            bytes
        );
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn if_match_rejects_a_stale_object_identity() {
        let (repository, runtime, cancel) = fixture().await;
        let path = crab_remote_git::GitPath::new(b"manifest".to_vec()).unwrap();
        let put = |bytes, condition| {
            apply(
                &repository,
                Arc::clone(&runtime),
                crab_remote_git::RepositoryOptions::default(),
                "refs/heads/main",
                &path,
                Change::Put {
                    bytes,
                    track_lfs: false,
                    attributes: Box::new(attributes::PutAttributes::default()),
                    condition,
                },
                "user",
                &cancel,
            )
        };
        put(Bytes::from_static(b"first"), PutCondition::None)
            .await
            .unwrap();
        let first_oid = object_id(Kind::Blob, b"first").unwrap();
        put(
            Bytes::from_static(b"second"),
            PutCondition::IfMatch {
                object: first_oid,
                etag: crate::gateway::md5_hex(b"first"),
                attributes_present: true,
            },
        )
        .await
        .unwrap();
        let stale = put(
            Bytes::from_static(b"third"),
            PutCondition::IfMatch {
                object: first_oid,
                etag: crate::gateway::md5_hex(b"first"),
                attributes_present: true,
            },
        )
        .await;

        assert!(matches!(stale, Err(Error::PreconditionFailed)));
        assert_eq!(
            read(&repository, Arc::clone(&runtime), &cancel, "manifest").await,
            "second"
        );
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn if_match_rejects_a_stale_etag_for_unchanged_bytes() {
        let (repository, runtime, cancel) = fixture().await;
        let path = crab_remote_git::GitPath::new(b"manifest".to_vec()).unwrap();
        let bytes = Bytes::from_static(b"same bytes");
        let oid = object_id(Kind::Blob, &bytes).unwrap();
        let apply_with = |etag: &str, condition| {
            apply(
                &repository,
                Arc::clone(&runtime),
                crab_remote_git::RepositoryOptions::default(),
                "refs/heads/main",
                &path,
                Change::Put {
                    bytes: bytes.clone(),
                    track_lfs: false,
                    attributes: Box::new(attributes::PutAttributes {
                        etag_override: Some(etag.to_owned()),
                        ..Default::default()
                    }),
                    condition,
                },
                "user",
                &cancel,
            )
        };
        apply_with("first-etag", PutCondition::None).await.unwrap();
        apply_with("replacement-etag", PutCondition::None)
            .await
            .unwrap();

        let stale = apply_with(
            "third-etag",
            PutCondition::IfMatch {
                object: oid,
                etag: "first-etag".to_owned(),
                attributes_present: true,
            },
        )
        .await;

        assert!(matches!(stale, Err(Error::PreconditionFailed)));
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn if_none_match_wildcard_preserves_an_existing_object() {
        let (repository, runtime, cancel) = fixture().await;
        let path = crab_remote_git::GitPath::new(b"manifest".to_vec()).unwrap();
        let put = |bytes, condition| {
            apply(
                &repository,
                Arc::clone(&runtime),
                crab_remote_git::RepositoryOptions::default(),
                "refs/heads/main",
                &path,
                Change::Put {
                    bytes,
                    track_lfs: false,
                    attributes: Box::new(attributes::PutAttributes::default()),
                    condition,
                },
                "user",
                &cancel,
            )
        };
        put(Bytes::from_static(b"first"), PutCondition::IfNoneMatchAny)
            .await
            .unwrap();
        let result = put(
            Bytes::from_static(b"replacement"),
            PutCondition::IfNoneMatchAny,
        )
        .await;

        assert!(matches!(result, Err(Error::PreconditionFailed)));
        assert_eq!(
            read(&repository, Arc::clone(&runtime), &cancel, "manifest").await,
            "first"
        );
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn batch_conditions_observe_prior_successes_in_fifo_order() {
        let (repository, runtime, cancel) = fixture().await;
        let path = crab_remote_git::GitPath::new(b"manifest".to_vec()).unwrap();
        let mutation = |bytes, condition| Mutation {
            path: path.clone(),
            change: Change::Put {
                bytes,
                track_lfs: false,
                attributes: Box::new(attributes::PutAttributes::default()),
                condition,
            },
            principal: "user".to_owned(),
        };
        let first = object_id(Kind::Blob, b"first").unwrap();
        let metrics = Metrics::new().unwrap();
        let results = apply_batch_admitted(
            &repository,
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            vec![
                mutation(Bytes::from_static(b"first"), PutCondition::IfNoneMatchAny),
                mutation(
                    Bytes::from_static(b"rejected"),
                    PutCondition::IfNoneMatchAny,
                ),
                mutation(
                    Bytes::from_static(b"third"),
                    PutCondition::IfMatch {
                        object: first,
                        etag: crate::gateway::md5_hex(b"first"),
                        attributes_present: true,
                    },
                ),
            ],
            ApplyRequest {
                branch: "refs/heads/main",
                plan_id: None,
                metrics: &metrics,
                state: None,
                checkpoints: None,
            },
            &cancel,
        )
        .await
        .unwrap();

        assert!(results[0].is_ok());
        assert!(matches!(&results[1], Err(Error::PreconditionFailed)));
        assert!(results[2].is_ok());
        assert_eq!(
            read(&repository, Arc::clone(&runtime), &cancel, "manifest").await,
            "third"
        );
        let snapshot = crab_metadata::manifest_store::read_repository_snapshot(
            &repository.store,
            &repository.layout,
        )
        .await
        .unwrap();
        assert_eq!(snapshot.journal.transactions.len(), 1);
        assert_eq!(snapshot.journal.packs.len(), 1);
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn conditional_delete_rejects_a_missing_object() {
        let (repository, runtime, cancel) = fixture().await;
        let path = crab_remote_git::GitPath::new(b"missing".to_vec()).unwrap();
        let result = apply(
            &repository,
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            "refs/heads/main",
            &path,
            Change::Delete {
                condition: DeleteCondition::IfMatch {
                    object: object_id(Kind::Blob, b"missing").unwrap(),
                    etag: crate::gateway::md5_hex(b"missing"),
                    attributes_present: false,
                    missing: MissingDeleteResult::PreconditionFailed,
                },
            },
            "user",
            &cancel,
        )
        .await;

        assert!(matches!(result, Err(Error::PreconditionFailed)));
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn conditional_multi_delete_reports_a_missing_object() {
        let (repository, runtime, cancel) = fixture().await;
        let path = crab_remote_git::GitPath::new(b"missing".to_vec()).unwrap();
        let result = apply(
            &repository,
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            "refs/heads/main",
            &path,
            Change::Delete {
                condition: DeleteCondition::IfMatchAny {
                    missing: MissingDeleteResult::NotFound,
                },
            },
            "user",
            &cancel,
        )
        .await;

        assert!(matches!(result, Err(Error::ConditionalTargetMissing)));
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn conditional_delete_rejects_a_replaced_object() {
        let (repository, runtime, cancel) = fixture().await;
        let path = crab_remote_git::GitPath::new(b"manifest".to_vec()).unwrap();
        apply(
            &repository,
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            "refs/heads/main",
            &path,
            Change::Put {
                bytes: Bytes::from_static(b"first"),
                track_lfs: false,
                attributes: Box::new(attributes::PutAttributes::default()),
                condition: PutCondition::None,
            },
            "user",
            &cancel,
        )
        .await
        .unwrap();
        let first = object_id(Kind::Blob, b"first").unwrap();
        apply(
            &repository,
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            "refs/heads/main",
            &path,
            Change::Put {
                bytes: Bytes::from_static(b"second"),
                track_lfs: false,
                attributes: Box::new(attributes::PutAttributes::default()),
                condition: PutCondition::None,
            },
            "user",
            &cancel,
        )
        .await
        .unwrap();
        let stale = apply(
            &repository,
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            "refs/heads/main",
            &path,
            Change::Delete {
                condition: DeleteCondition::IfMatch {
                    object: first,
                    etag: crate::gateway::md5_hex(b"first"),
                    attributes_present: false,
                    missing: MissingDeleteResult::PreconditionFailed,
                },
            },
            "user",
            &cancel,
        )
        .await;
        assert!(matches!(stale, Err(Error::PreconditionFailed)));
        assert_eq!(
            read(&repository, Arc::clone(&runtime), &cancel, "manifest").await,
            "second"
        );

        let second = object_id(Kind::Blob, b"second").unwrap();
        apply(
            &repository,
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            "refs/heads/main",
            &path,
            Change::Delete {
                condition: DeleteCondition::IfMatch {
                    object: second,
                    etag: crate::gateway::md5_hex(b"second"),
                    attributes_present: false,
                    missing: MissingDeleteResult::PreconditionFailed,
                },
            },
            "user",
            &cancel,
        )
        .await
        .unwrap();
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_ref_writes_share_one_pack_and_keep_one_commit_each() {
        let (repository, runtime, cancel) = fixture().await;
        let repository = Arc::new(repository);
        let blocker = PushLock::acquire_internal(
            repository.store.inner(),
            repository.layout.repo_prefix(),
            GIT_OBJECT_LOCATOR_RESOURCE,
            LOCK_TTL,
        )
        .await
        .unwrap();
        let coordinator = Arc::new(Coordinator::new(
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            cancel.clone(),
            Metrics::new().unwrap(),
        ));
        let start = Arc::new(tokio::sync::Barrier::new(9));
        let mut writes = tokio::task::JoinSet::new();
        for index in 0..8 {
            let repository = Arc::clone(&repository);
            let coordinator = Arc::clone(&coordinator);
            let cancel = cancel.clone();
            let start = Arc::clone(&start);
            writes.spawn(async move {
                start.wait().await;
                let path = format!("queued/{index}.txt");
                coordinator
                    .apply(
                        &repository,
                        "refs/heads/main",
                        &crab_remote_git::GitPath::new(path.into_bytes()).unwrap(),
                        Change::Put {
                            bytes: Bytes::from(format!("value-{index}")),
                            track_lfs: false,
                            attributes: Box::new(attributes::PutAttributes::default()),
                            condition: PutCondition::None,
                        },
                        "user",
                        &cancel,
                    )
                    .await
            });
        }
        start.wait().await;
        while let Some(result) = writes.join_next().await {
            result.unwrap().unwrap();
        }
        let journal = crab_metadata::manifest_store::read_repository_snapshot(
            &repository.store,
            &repository.layout,
        )
        .await
        .unwrap()
        .journal;
        assert_eq!(journal.transactions.len(), 1);
        assert_eq!(journal.packs.len(), 1);
        let transaction = crab_metadata::ref_journal::read_transaction(
            &repository.store,
            &repository.layout,
            &journal.transactions[0],
        )
        .await
        .unwrap();
        assert_eq!(transaction.edits.len(), 1);
        assert_eq!(transaction.packs.len(), 1);

        let view = repository
            .read_views
            .current(
                &repository,
                Arc::clone(&runtime),
                crab_remote_git::RepositoryOptions::default(),
                &cancel,
            )
            .await
            .unwrap();
        let tip = view.remote().refs().find("refs/heads/main").unwrap().target;
        let operation = view
            .remote()
            .operation(OperationKind::Repository, &cancel)
            .await
            .unwrap();
        let snapshot = view
            .remote()
            .snapshot(&crab_remote_git::Revision::Commit(tip), &operation)
            .await
            .unwrap();
        let history = snapshot
            .history(
                crab_remote_git::HistoryTraversal::FirstParent,
                &crab_remote_git::PageRequest::new(16, None).unwrap(),
                &operation,
            )
            .await
            .unwrap();
        assert_eq!(history.items.len(), 8);
        operation.finish(Ok(())).await.unwrap();
        blocker.release().await.unwrap();

        let mut canonical = None;
        for _ in 0..700 {
            let snapshot = crab_metadata::manifest_store::read_repository_snapshot(
                &repository.store,
                &repository.layout,
            )
            .await
            .unwrap();
            if snapshot.journal.transactions.is_empty() {
                canonical = RemoteGitRepository::open(
                    repository.store.clone(),
                    repository.layout.clone(),
                    repository.identity.clone(),
                    Arc::clone(&runtime),
                    crab_remote_git::RepositoryOptions::default(),
                    &cancel,
                )
                .await
                .ok();
                if canonical.is_some() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let canonical = canonical.expect("background maintenance should publish a readable view");
        assert!(canonical.refs().find("refs/heads/main").is_some());
        for index in 0..8 {
            assert_eq!(
                read(
                    &repository,
                    Arc::clone(&runtime),
                    &cancel,
                    &format!("queued/{index}.txt"),
                )
                .await,
                format!("value-{index}")
            );
        }
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fence_burst_preserves_batch_transactions_and_commit_history() {
        let (repository, runtime, cancel) = fixture().await;
        let blocker = PushLock::acquire_internal(
            repository.store.inner(),
            repository.layout.repo_prefix(),
            GIT_OBJECT_LOCATOR_RESOURCE,
            LOCK_TTL,
        )
        .await
        .unwrap();
        let metrics = Metrics::new().unwrap();
        let coordinator = Coordinator::new(
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            cancel.clone(),
            metrics.clone(),
        );
        let mut queued = Vec::new();
        for index in 0..40 {
            let path = format!("burst/{index}.txt");
            queued.push(
                coordinator
                    .admission
                    .enqueue(
                        &repository.config.name,
                        "refs/heads/main",
                        mutation(path.as_bytes(), None),
                    )
                    .unwrap(),
            );
        }
        let queue = Arc::clone(&queued[0].queue);
        let first = queue.take_batch().unwrap();
        assert_eq!(first.len(), MAX_MUTATIONS_PER_BATCH);

        repository.maintenance.begin();
        Coordinator::apply_batch(
            &repository,
            BatchContext {
                runtime: Arc::clone(&runtime),
                options: crab_remote_git::RepositoryOptions::default(),
                checkpoints: Arc::clone(&coordinator.checkpoints),
                metrics: metrics.clone(),
            },
            "refs/heads/main",
            &queue,
            first,
            &cancel,
        )
        .await;
        for mut queued in queued {
            (&mut queued.result).await.unwrap().unwrap();
        }

        let journal = crab_metadata::manifest_store::read_repository_snapshot(
            &repository.store,
            &repository.layout,
        )
        .await
        .unwrap()
        .journal;
        assert_eq!(journal.transactions.len(), 2);
        assert_eq!(journal.packs.len(), 2);

        let view = repository
            .read_views
            .current(
                &repository,
                Arc::clone(&runtime),
                crab_remote_git::RepositoryOptions::default(),
                &cancel,
            )
            .await
            .unwrap();
        let tip = view.remote().refs().find("refs/heads/main").unwrap().target;
        let operation = view
            .remote()
            .operation(OperationKind::Repository, &cancel)
            .await
            .unwrap();
        let snapshot = view
            .remote()
            .snapshot(&crab_remote_git::Revision::Commit(tip), &operation)
            .await
            .unwrap();
        let history = snapshot
            .history(
                crab_remote_git::HistoryTraversal::FirstParent,
                &crab_remote_git::PageRequest::new(64, None).unwrap(),
                &operation,
            )
            .await
            .unwrap();
        assert_eq!(history.items.len(), 40);
        operation.finish(Ok(())).await.unwrap();

        let admission = crate::admission::Admission::new(8, cancel.clone(), metrics.clone());
        let body = metrics.render(&admission);
        assert!(body.contains("crab_s3_gateway_mutation_fence_bursts_total 1"));
        assert!(body.contains("crab_s3_gateway_mutation_fence_burst_batches_total 2"));

        blocker.release().await.unwrap();
        coordinator.shutdown().await;
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn admitted_batch_survives_all_callers_being_dropped() {
        let (repository, runtime, cancel) = fixture().await;
        let repository = Arc::new(repository);
        let lock = PushLock::acquire_ref(
            repository.store.inner(),
            repository.layout.repo_prefix(),
            "refs/heads/main",
            LOCK_TTL,
        )
        .await
        .unwrap();
        let coordinator = Arc::new(Coordinator::new(
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            cancel.clone(),
            Metrics::new().unwrap(),
        ));
        let mut callers = Vec::new();
        for index in 0..2 {
            let repository = Arc::clone(&repository);
            let coordinator = Arc::clone(&coordinator);
            let cancel = cancel.clone();
            callers.push(tokio::spawn(async move {
                coordinated_put(
                    &coordinator,
                    &repository,
                    &cancel,
                    &format!("detached/{index}.txt"),
                    b"durable",
                )
                .await;
            }));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        for caller in callers {
            caller.abort();
        }
        lock.release().await.unwrap();

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = crab_metadata::manifest_store::read_repository_snapshot(
                    &repository.store,
                    &repository.layout,
                )
                .await
                .unwrap();
                if snapshot.journal.transactions.len() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            read(&repository, Arc::clone(&runtime), &cancel, "detached/0.txt").await,
            "durable"
        );
        assert_eq!(
            read(&repository, Arc::clone(&runtime), &cancel, "detached/1.txt").await,
            "durable"
        );
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn independent_coordinators_reprepare_against_the_winning_tip() {
        let (repository, runtime, cancel) = fixture().await;
        let repository = Arc::new(repository);
        let first = Arc::new(Coordinator::new(
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            cancel.clone(),
            Metrics::new().unwrap(),
        ));
        let second = Arc::new(Coordinator::new(
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            cancel.clone(),
            Metrics::new().unwrap(),
        ));
        let start = Arc::new(tokio::sync::Barrier::new(3));
        let mut writes = tokio::task::JoinSet::new();
        for (coordinator, path, bytes) in [
            (first, "first.txt", Bytes::from_static(b"first")),
            (second, "second.txt", Bytes::from_static(b"second")),
        ] {
            let repository = Arc::clone(&repository);
            let cancel = cancel.clone();
            let start = Arc::clone(&start);
            writes.spawn(async move {
                start.wait().await;
                coordinator
                    .apply(
                        &repository,
                        "refs/heads/main",
                        &crab_remote_git::GitPath::new(path.as_bytes().to_vec()).unwrap(),
                        Change::Put {
                            bytes,
                            track_lfs: false,
                            attributes: Box::new(attributes::PutAttributes::default()),
                            condition: PutCondition::None,
                        },
                        "user",
                        &cancel,
                    )
                    .await
            });
        }
        start.wait().await;
        while let Some(result) = writes.join_next().await {
            result.unwrap().unwrap();
        }

        assert_eq!(
            read(&repository, Arc::clone(&runtime), &cancel, "first.txt").await,
            "first"
        );
        assert_eq!(
            read(&repository, Arc::clone(&runtime), &cancel, "second.txt").await,
            "second"
        );
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn warm_precondition_failure_revalidates_the_object_store_ref() {
        let (repository, runtime, cancel) = fixture().await;
        let local = Coordinator::new(
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            cancel.clone(),
            Metrics::new().unwrap(),
        );
        let competing = Coordinator::new(
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            cancel.clone(),
            Metrics::new().unwrap(),
        );
        coordinated_put(&local, &repository, &cancel, "seed.txt", b"seed").await;
        coordinated_put(
            &competing,
            &repository,
            &cancel,
            "created-elsewhere.txt",
            b"winner",
        )
        .await;

        let result = local
            .apply(
                &repository,
                "refs/heads/main",
                &crab_remote_git::GitPath::new(b"created-elsewhere.txt".to_vec()).unwrap(),
                Change::Put {
                    bytes: Bytes::from_static(b"loser"),
                    track_lfs: false,
                    attributes: Box::new(attributes::PutAttributes::default()),
                    condition: PutCondition::IfNoneMatchAny,
                },
                "user",
                &cancel,
            )
            .await;

        assert!(matches!(result, Err(Error::PreconditionFailed)));
        assert_eq!(
            read(
                &repository,
                Arc::clone(&runtime),
                &cancel,
                "created-elsewhere.txt",
            )
            .await,
            "winner"
        );
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn write_during_canonical_maintenance_retains_visibility_proof() {
        let (repository, runtime, cancel) = fixture().await;
        let coordinator = Coordinator::new(
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            cancel.clone(),
            Metrics::new().unwrap(),
        );
        coordinated_put(
            &coordinator,
            &repository,
            &cancel,
            "baseline.txt",
            b"baseline",
        )
        .await;
        let baseline = wait_for_visibility(&repository).await;
        let blocker = loop {
            match PushLock::acquire_internal(
                repository.store.inner(),
                repository.layout.repo_prefix(),
                GIT_OBJECT_LOCATOR_RESOURCE,
                LOCK_TTL,
            )
            .await
            {
                Ok(blocker) => break blocker,
                Err(crab_coordination::CoordinationError::PushLockHeld { .. }) => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => panic!("failed to acquire catalog blocker: {error}"),
            }
        };

        coordinated_put(&coordinator, &repository, &cancel, "first.txt", b"first").await;
        wait_for_generation_after(&repository, baseline).await;
        coordinated_put(&coordinator, &repository, &cancel, "second.txt", b"second").await;
        blocker.release().await.unwrap();
        let final_generation = wait_for_visibility(&repository).await;

        assert!(final_generation > baseline);
        runtime.shutdown().await;
    }

    async fn wait_for_generation_after(repository: &Repository, generation: u64) {
        for _ in 0..500 {
            let (manifest, _) =
                crab_metadata::manifest_store::read_manifest(&repository.store, &repository.layout)
                    .await
                    .unwrap();
            if manifest.generation > generation {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("background maintenance did not advance the manifest");
    }

    async fn wait_for_visibility(repository: &Repository) -> u64 {
        for _ in 0..500 {
            let snapshot = crab_metadata::manifest_store::read_repository_snapshot(
                &repository.store,
                &repository.layout,
            )
            .await
            .unwrap();
            if snapshot.journal.transactions.is_empty()
                && crab_metadata::git_visibility::read_for_manifest(
                    &repository.store,
                    &repository.layout,
                    &snapshot.manifest,
                )
                .await
                .unwrap()
                .is_some()
            {
                return snapshot.manifest.generation;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("background maintenance did not publish a visibility proof");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn each_commit_persists_only_its_path_attribute_delta() {
        let (repository, runtime, cancel) = fixture().await;
        let first = Coordinator::new(
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            cancel.clone(),
            Metrics::new().unwrap(),
        );
        first
            .apply(
                &repository,
                "refs/heads/main",
                &crab_remote_git::GitPath::new(b"first.txt".to_vec()).unwrap(),
                Change::Put {
                    bytes: Bytes::from_static(b"first.txt"),
                    track_lfs: false,
                    attributes: Box::new(attributes::PutAttributes::default()),
                    condition: PutCondition::None,
                },
                "user",
                &cancel,
            )
            .await
            .unwrap();
        first.checkpoints.wait_idle().await;
        let first_commit = tip(&repository, Arc::clone(&runtime), &cancel).await;
        let first_manifest = attributes::load(&repository, first_commit).await.unwrap();
        let stale =
            attributes::prepare_checkpoint("refs/heads/main", first_commit, &first_manifest)
                .unwrap()
                .unwrap();

        let second = Coordinator::new(
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            cancel.clone(),
            Metrics::new().unwrap(),
        );
        second
            .apply(
                &repository,
                "refs/heads/main",
                &crab_remote_git::GitPath::new(b"second.txt".to_vec()).unwrap(),
                Change::Put {
                    bytes: Bytes::from_static(b"second.txt"),
                    track_lfs: false,
                    attributes: Box::new(attributes::PutAttributes::default()),
                    condition: PutCondition::None,
                },
                "user",
                &cancel,
            )
            .await
            .unwrap();
        second.checkpoints.wait_idle().await;
        let commit = tip(&repository, Arc::clone(&runtime), &cancel).await;
        let path = repository
            .layout
            .repo_path(&format!("s3/attributes/{commit}.json"));
        let (bytes, _) = repository.store.get_with_etag(&path).await.unwrap();
        let stored: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(stored["version"], 2);
        assert_eq!(stored["checkpoint"], true);
        assert!(stored.get("checkpoint_parent").is_none());
        assert!(stored.get("objects").is_none());
        assert_eq!(stored["changes"].as_object().unwrap().len(), 1);
        assert!(stored["changes"].get("second.txt").is_some());
        let checkpoint_slot = stored["checkpoint_slot"].as_str().unwrap();
        let checkpoint = repository.layout.repo_path(&format!(
            "s3/attribute-checkpoints/branches/{checkpoint_slot}.json"
        ));
        repository.store.head(&checkpoint).await.unwrap();

        let manifest = attributes::load(&repository, commit).await.unwrap();
        let current = attributes::prepare_checkpoint("refs/heads/main", commit, &manifest)
            .unwrap()
            .unwrap();
        repository
            .store
            .put_overwrite(&checkpoint, stale.bytes())
            .await
            .unwrap();
        let fallback = attributes::load(&repository, commit).await.unwrap();
        for path in ["first.txt", "second.txt"] {
            let oid = object_id(Kind::Blob, path.as_bytes()).unwrap();
            assert!(fallback.object(path, oid).is_some());
        }
        assert!(
            attributes::publish_checkpoint(&repository, &current)
                .await
                .unwrap()
        );
        assert!(
            !attributes::publish_checkpoint(&repository, &stale)
                .await
                .unwrap()
        );

        let parent = stored["parent"].as_str().unwrap();
        let parent_delta = repository
            .layout
            .repo_path(&format!("s3/attributes/{parent}.json"));
        repository.store.delete(&parent_delta).await.unwrap();

        let manifest = attributes::load(&repository, commit).await.unwrap();
        for path in ["first.txt", "second.txt"] {
            let oid = object_id(Kind::Blob, path.as_bytes()).unwrap();
            assert!(manifest.object(path, oid).is_some());
        }
        runtime.shutdown().await;
    }
}
