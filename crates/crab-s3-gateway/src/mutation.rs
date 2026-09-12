use std::{
    collections::{HashMap, VecDeque},
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
use tokio::sync::{Semaphore, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{
    attributes,
    gateway::Repository,
    metrics::{Metrics, ScratchFailure, ScratchPurpose},
};

const LOCK_TTL: Duration = Duration::from_secs(300);
const REF_LOCK_TTL: Duration = Duration::from_secs(30);
const MAX_GENERATED_PACK_BYTES: u64 = 512 * 1024 * 1024;
const PACK_SCRATCH_BASE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_QUEUED_WRITES_PER_REF: usize = 64;
const MAX_TRACKED_REFS: usize = 256;
const MAX_WARM_BRANCH_STATE_BYTES: usize = 128 * 1024 * 1024;
const ATTRIBUTE_CHECKPOINT_BATCHES: usize = 64;
const WRITE_QUEUE_TIMEOUT: Duration = Duration::from_secs(60);
const BATCH_COLLECTION_DELAY: Duration = Duration::from_millis(10);
const MAX_MUTATIONS_PER_BATCH: usize = 32;
const MAX_MUTATION_BATCH_BYTES: usize = 32 * 1024 * 1024;
const MAX_REPREPARE_ATTEMPTS: usize = 8;
const TREE_PAGE_SIZE: usize = 4_096;

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

#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum PutCondition {
    #[default]
    None,
    IfNoneMatchAny,
    IfMatch(ObjectId),
}

#[derive(Clone, Debug, Default)]
pub(crate) enum DeleteCondition {
    #[default]
    None,
    IfMatchAny,
    IfMatch {
        object: ObjectId,
        etag: String,
        attributes_present: bool,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct Outcome {
    pub(crate) etag: Option<String>,
}

pub(crate) struct Coordinator {
    runtime: Arc<crab_remote_git::RemoteGitRuntime>,
    options: crab_remote_git::RepositoryOptions,
    admission: WriteAdmission,
    metrics: Metrics,
}

impl Coordinator {
    pub(crate) fn new(
        runtime: Arc<crab_remote_git::RemoteGitRuntime>,
        options: crab_remote_git::RepositoryOptions,
        metrics: Metrics,
    ) -> Self {
        Self {
            runtime,
            options,
            admission: WriteAdmission::default(),
            metrics,
        }
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
                let runtime = Arc::clone(&self.runtime);
                let options = self.options;
                let metrics = self.metrics.clone();
                let branch = branch.to_owned();
                let queue = Arc::clone(&queued.queue);
                let worker_cancel = cancel.clone();
                // A drained batch owns every accepted mutation. Detaching it
                // prevents one disconnected HTTP caller from cancelling peers.
                tokio::spawn(async move {
                    Self::apply_batch(
                        &repository,
                        runtime,
                        options,
                        &metrics,
                        &branch,
                        &queue,
                        batch,
                        &worker_cancel,
                    )
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

    async fn apply_batch(
        repository: &Repository,
        runtime: Arc<crab_remote_git::RemoteGitRuntime>,
        options: crab_remote_git::RepositoryOptions,
        metrics: &Metrics,
        branch: &str,
        queue: &RefQueue,
        batch: Vec<PendingMutation>,
        cancel: &CancellationToken,
    ) {
        let input_bytes = batch.iter().fold(0usize, |total, pending| {
            total.saturating_add(pending.mutation.estimated_bytes())
        });
        for pending in &batch {
            metrics.record_mutation_queue_wait(pending.queued_at.elapsed().as_secs_f64());
        }
        let _observation = metrics.start_mutation_batch(batch.len(), input_bytes);
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
                        Arc::clone(&runtime),
                        options,
                        mutation,
                        ApplyRequest {
                            branch,
                            plan_id: None,
                            metrics,
                            state: Some(&queue.state),
                        },
                        cancel,
                    )
                    .await,
                ]),
                None => Err(std::io::Error::other("planned mutation batch is empty").into()),
            }
        } else {
            apply_batch_admitted(
                repository,
                Arc::clone(&runtime),
                options,
                mutations,
                ApplyRequest {
                    branch,
                    plan_id: None,
                    metrics,
                    state: Some(&queue.state),
                },
                cancel,
            )
            .await
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
        if let Some((epoch, maintenance_cancel)) = repository.maintenance.finish(cancel) {
            crate::repository::schedule_catalog_maintenance(repository, cancel, epoch);
            crate::repository::schedule_readability(
                repository,
                runtime,
                options,
                maintenance_cancel,
                epoch,
            );
        }
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
    Coordinator::new(runtime, options, Metrics::new().unwrap())
        .apply(repository, branch, path, change, principal, cancel)
        .await
}

#[derive(Default)]
struct WriteAdmission {
    refs: std::sync::Mutex<HashMap<String, Arc<RefQueue>>>,
    warm_bytes: Arc<AtomicUsize>,
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
        let queue = {
            let mut refs = self.refs.lock().map_err(|_| Error::AdmissionState)?;
            for (candidate, queue) in refs.iter() {
                if candidate != &key && queue.admitted.load(Ordering::Acquire) == 0 {
                    queue.state.clear();
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
    let mut fences = Vec::new();
    let result = async {
        for domain in [
            repository.layout.global_prefix(),
            repository.layout.repo_prefix(),
        ] {
            check_cancelled(cancel)?;
            let fence =
                GcFenceLease::acquire_writer(repository.store.inner(), domain, LOCK_TTL).await?;
            let heartbeat = GcFenceHeartbeat::spawn(&fence, cancel.clone(), LOCK_TTL / 3);
            fences.push((fence, heartbeat));
        }
        Box::pin(apply_with_fences(
            repository,
            Arc::clone(&runtime),
            options,
            mutations,
            request,
            cancel,
        ))
        .await
    }
    .await;
    for (fence, heartbeat) in fences.into_iter().rev() {
        heartbeat.stop().await;
        if let Err(error) = fence.release().await {
            tracing::warn!(%error, "S3 GC fence cleanup failed");
        }
    }
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
                if let Some(state) = request.state {
                    state.store(
                        prepared.tip,
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
    checkpoint: Option<attributes::PreparedCheckpoint>,
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
                    cancel,
                    request.metrics,
                )
                .await;
            }
            Err(Error::WarmStateMiss) => {}
            Err(error) => return Err(error),
        }
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
    let checkpoint = has_commits && next_checkpoint >= ATTRIBUTE_CHECKPOINT_BATCHES;
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
    cancel: &CancellationToken,
    metrics: &Metrics,
) -> Result<PreparedBatch> {
    let BuiltBatch {
        outcomes,
        commits,
        tip,
        attributes,
        tree,
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
        });
    };
    let commit_count = commits.len();
    let parent = commits.first().and_then(|commit| commit.parent);
    let final_commit = commits
        .last()
        .map(|commit| commit.commit)
        .ok_or_else(|| std::io::Error::other("mutation batch has no commit"))?;
    let checkpoint = if checkpoint {
        check_cancelled(cancel)?;
        attributes::prepare_checkpoint(branch, final_commit, &attributes)?
    } else {
        None
    };
    let publication = upload_built_batch(
        repository,
        parent,
        commits,
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
    })
}

async fn upload_built_batch(
    repository: &Repository,
    parent: Option<ObjectId>,
    commits: Vec<BuiltCommit>,
    expected_transaction: Option<String>,
    checkpoint: Option<attributes::PreparedCheckpoint>,
    cancel: &CancellationToken,
    metrics: &Metrics,
) -> Result<UploadedMutation> {
    let mut objects = Vec::new();
    let mut seen = HashMap::new();
    let final_commit = commits
        .last()
        .map(|built| built.commit)
        .ok_or_else(|| std::io::Error::other("mutation batch has no commit"))?;
    let checkpoint_slot = checkpoint
        .as_ref()
        .map(|checkpoint| checkpoint.slot().to_owned());
    let attribute_deltas = commits
        .iter()
        .map(|built| {
            (
                built.commit,
                built.parent,
                built.path.clone(),
                built.attributes.clone(),
                built.commit == final_commit && checkpoint_slot.is_some(),
                (built.commit == final_commit)
                    .then(|| checkpoint_slot.clone())
                    .flatten(),
            )
        })
        .collect::<Vec<_>>();
    for built in commits {
        for (kind, bytes) in built.objects {
            let oid = object_id(kind, &bytes)?;
            if seen.insert(oid, ()).is_none() {
                objects.push((kind, bytes));
            }
        }
    }
    let scratch_bytes = pack_scratch_reservation(&objects);
    let capacity = metrics.reserve_scratch(scratch_bytes)?;
    let mut scratch = metrics.start_scratch(ScratchPurpose::GitPack);
    scratch.reserve(scratch_bytes);
    let visible_objects = objects
        .iter()
        .map(|(kind, bytes)| object_id(*kind, bytes).map(|oid| oid.to_string()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let prepared = prepare_pack(objects, cancel).await;
    if matches!(
        &prepared,
        Err(Error::Io(_)
            | Error::Pack(crab_git::incoming_pack::IncomingPackError::Io(_))
            | Error::Prepare(crab_git::incoming_pack::PreparePackError::Io(_)))
    ) {
        scratch.record_failure(ScratchFailure::Write);
    }
    let (pack_owner, pack) = prepared?;
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
        repository
            .store
            .put_multipart_file_retry(
                &repository.layout.pack_path(&pack_id),
                pack.pack_path(),
                pack.size(),
                *pack.content_hash().as_bytes(),
                8 * 1024 * 1024,
                cancel,
                None,
            )
            .await?;
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
            |(commit, parent, path, attributes, checkpoint, checkpoint_slot)| {
                attributes::save_delta(
                    repository,
                    commit,
                    parent,
                    path,
                    attributes,
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
        checkpoint,
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
    // Direct generated-pack preparation retains one decoded spool and two
    // normalized packs at peak. The fourth copy, extra quarter, and fixed
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
    if let Some(checkpoint) = &prepared.checkpoint
        && let Err(error) = attributes::publish_checkpoint(repository, checkpoint).await
    {
        // The journal is already authoritative. A missing checkpoint falls
        // back to immutable deltas, so retrying this mutation would duplicate it.
        tracing::warn!(%error, "S3 attribute checkpoint publication failed");
    }
    Ok(Publish::Committed(committed.transaction_id))
}

struct BuiltCommit {
    parent: Option<ObjectId>,
    commit: ObjectId,
    etag: Option<String>,
    objects: Vec<(Kind, Vec<u8>)>,
    path: String,
    attributes: Option<attributes::ObjectAttributes>,
}

enum Build {
    Noop(Outcome),
    Commit(Box<BuiltCommit>),
}

struct TreeFrame {
    path: Vec<u8>,
    old_oid: Option<ObjectId>,
    new_oid: Option<ObjectId>,
    entries: Vec<tree::Entry>,
}

#[derive(Clone)]
struct DirectoryState {
    oid: Option<ObjectId>,
    entries: Vec<tree::Entry>,
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

    async fn frames(
        &mut self,
        directories: &[Vec<u8>],
        operation: Option<&crab_remote_git::OperationContext>,
    ) -> Result<Vec<TreeFrame>> {
        let mut frames = Vec::with_capacity(directories.len() + 1);
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
            frames.push(TreeFrame {
                path: key,
                old_oid: state.oid,
                new_oid: state.oid,
                entries: state.entries.clone(),
            });
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
        Ok(frames)
    }

    fn commit(&mut self, frames: Vec<TreeFrame>, objects: &[(Kind, Vec<u8>)]) -> Result<()> {
        for frame in &frames {
            let attributes = find_entry(&frame.entries, b".gitattributes");
            match attributes {
                Some(attributes) => {
                    let mut generated = None;
                    for (kind, bytes) in objects {
                        if *kind == Kind::Blob && object_id(*kind, bytes)? == attributes.oid {
                            generated = Some(bytes);
                            break;
                        }
                    }
                    if let Some(bytes) = generated {
                        self.replace_generated_attributes(
                            frame.path.clone(),
                            Some(GeneratedAttributeBlob {
                                oid: attributes.oid,
                                bytes: bytes.clone(),
                            }),
                        );
                    } else if self
                        .generated_attribute_blobs
                        .get(&frame.path)
                        .is_some_and(|blob| blob.oid != attributes.oid)
                    {
                        self.replace_generated_attributes(frame.path.clone(), None);
                    }
                }
                None => self.replace_generated_attributes(frame.path.clone(), None),
            }
        }
        for frame in frames {
            if frame.path.is_empty() {
                self.root_oid = frame.new_oid;
            }
            let state = DirectoryState {
                oid: frame.new_oid,
                entries: frame.entries,
            };
            let next_bytes = estimated_directory_bytes(&frame.path, &state);
            if let Some(previous) = self.directories.insert(frame.path.clone(), state) {
                self.estimated_bytes = self
                    .estimated_bytes
                    .saturating_sub(estimated_directory_bytes(&frame.path, &previous));
            }
            self.estimated_bytes = self.estimated_bytes.saturating_add(next_bytes);
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
        total
            .saturating_add(std::mem::size_of::<tree::Entry>())
            .saturating_add(entry.filename.len())
    })
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
    let mut frames = tree_state.frames(directories, operation).await?;
    let mut directory_path = crab_remote_git::GitPath::root();
    for component in directories {
        directory_path = push_component(&directory_path, component)?;
    }
    let leaf_entries = &mut frames
        .last_mut()
        .ok_or_else(|| std::io::Error::other("object path has no parent tree"))?
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
    if let Change::Delete { condition } = &change
        && !delete_condition_matches(condition, old.as_ref(), current_attributes)
    {
        return Err(Error::PreconditionFailed);
    }
    let (etag, changed, pending_attributes) = match change {
        Change::Put {
            bytes,
            track_lfs: _,
            attributes,
            condition,
        } => {
            match condition {
                PutCondition::None => {}
                PutCondition::IfNoneMatchAny if old.is_some() => {
                    return Err(Error::PreconditionFailed);
                }
                PutCondition::IfMatch(expected) if old.is_none_or(|(oid, _)| oid != expected) => {
                    return Err(Error::PreconditionFailed);
                }
                PutCondition::IfNoneMatchAny | PutCondition::IfMatch(_) => {}
            }
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
            replace_entry(
                leaf_entries,
                name,
                Some(tree::Entry {
                    mode: tree::EntryKind::Blob.into(),
                    filename: BString::from(name.clone()),
                    oid,
                }),
            );
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
            replace_entry(leaf_entries, name, None);
            (None, None, None)
        }
    };
    let mut objects = Vec::new();
    if let Some(bytes) = changed {
        objects.push((Kind::Blob, bytes));
    }
    if let Some(attributes) = lfs_attributes {
        replace_entry(
            leaf_entries,
            b".gitattributes",
            Some(tree::Entry {
                mode: attributes.mode,
                filename: BString::from(b".gitattributes".to_vec()),
                oid: attributes.oid,
            }),
        );
        objects.push((Kind::Blob, attributes.bytes));
    }
    let mut tree_oid = None;
    for depth in (0..frames.len()).rev() {
        let frame = &mut frames[depth];
        let empty = frame.entries.is_empty();
        let oid = if depth > 0 && empty {
            None
        } else {
            let oid = encode_tree(frame.old_oid, &frame.entries, &mut objects)?;
            Some(oid)
        };
        frame.new_oid = oid;
        if depth == 0 {
            tree_oid = oid;
            break;
        }
        let directory_name = &directories[depth - 1];
        let entry = oid.map(|oid| tree::Entry {
            mode: tree::EntryKind::Tree.into(),
            filename: BString::from(directory_name.clone()),
            oid,
        });
        replace_entry(&mut frames[depth - 1].entries, directory_name, entry);
    }
    let tree = tree_oid.ok_or_else(|| std::io::Error::other("root tree was not encoded"))?;
    let seconds = now_seconds()?;
    let object_attributes = pending_attributes.map(|(oid, pending, size)| {
        attributes::ObjectAttributes::new(
            oid,
            etag.clone().unwrap_or_default(),
            size,
            seconds,
            *pending,
        )
    });
    let attribute_identity = serde_json::to_vec(&(
        path_string,
        parent.map(|oid| oid.to_string()),
        &object_attributes,
    ))
    .map_err(|source| Error::Attributes(Box::new(crate::Error::Attributes { source })))?;
    let attribute_digest = blake3::hash(&attribute_identity).to_hex();
    let commit_bytes = commit_bytes(tree, parent, principal, seconds, attribute_digest.as_str());
    let commit = object_id(Kind::Commit, &commit_bytes)?;
    objects.push((Kind::Commit, commit_bytes));
    tree_state.commit(frames, &objects)?;
    manifest.update(path_string.to_owned(), object_attributes.clone());
    Ok(Build::Commit(Box::new(BuiltCommit {
        parent,
        commit,
        etag,
        objects,
        path: path_string.to_owned(),
        attributes: object_attributes,
    })))
}

fn delete_condition_matches(
    condition: &DeleteCondition,
    old: Option<&(ObjectId, EntryMode)>,
    current_attributes: Option<&attributes::ObjectAttributes>,
) -> bool {
    match condition {
        DeleteCondition::None => true,
        DeleteCondition::IfMatchAny => old.is_some(),
        DeleteCondition::IfMatch {
            object,
            etag,
            attributes_present,
        } => {
            old.is_some_and(|(oid, _)| oid == object)
                && match (attributes_present, current_attributes) {
                    (_, Some(value)) => value.etag == *etag,
                    (true, None) => false,
                    (false, None) => true,
                }
        }
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

fn replace_entry(entries: &mut Vec<tree::Entry>, name: &[u8], replacement: Option<tree::Entry>) {
    if let Some(index) = entry_index(entries, name) {
        entries.remove(index);
    }
    if let Some(replacement) = replacement {
        let index = entries
            .binary_search(&replacement)
            .unwrap_or_else(|index| index);
        entries.insert(index, replacement);
    }
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

fn encode_tree(
    old_oid: Option<ObjectId>,
    entries: &[tree::Entry],
    objects: &mut Vec<(Kind, Vec<u8>)>,
) -> Result<ObjectId> {
    let mut bytes = Vec::new();
    let mut mode = [0; 6];
    for entry in entries {
        bytes.extend_from_slice(entry.mode.as_bytes(&mut mode));
        bytes.push(b' ');
        bytes.extend_from_slice(&entry.filename);
        bytes.push(0);
        bytes.extend_from_slice(entry.oid.as_bytes());
    }
    let oid = object_id(Kind::Tree, &bytes)?;
    if old_oid != Some(oid) {
        objects.push((Kind::Tree, bytes));
    }
    Ok(oid)
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
            .prepare(owner.path(), MAX_GENERATED_PACK_BYTES, &worker_flag)?
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

        let oid = encode_tree(None, &entries, &mut objects).unwrap();

        assert_eq!(oid, object_id(Kind::Tree, &expected).unwrap());
        assert_eq!(objects, vec![(Kind::Tree, expected)]);
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
        let mutation = |path: &[u8], upload_id: Option<&str>| Mutation {
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
        };
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

        assert_eq!(
            read(
                &repository,
                Arc::clone(&runtime),
                &cancel,
                "nested/.gitattributes",
            )
            .await,
            format!(
                "README.md text\n{}\n",
                lfs_attributes_line(filename.as_bytes())
            )
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
            PutCondition::IfMatch(first_oid),
        )
        .await
        .unwrap();
        let stale = put(
            Bytes::from_static(b"third"),
            PutCondition::IfMatch(first_oid),
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
                mutation(Bytes::from_static(b"third"), PutCondition::IfMatch(first)),
            ],
            ApplyRequest {
                branch: "refs/heads/main",
                plan_id: None,
                metrics: &metrics,
                state: None,
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
            Metrics::new().unwrap(),
        ));
        let second = Arc::new(Coordinator::new(
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
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
    async fn write_during_canonical_maintenance_retains_visibility_proof() {
        let (repository, runtime, cancel) = fixture().await;
        let coordinator = Coordinator::new(
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
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
        for path in ["first.txt", "second.txt"] {
            apply(
                &repository,
                Arc::clone(&runtime),
                crab_remote_git::RepositoryOptions::default(),
                "refs/heads/main",
                &crab_remote_git::GitPath::new(path.as_bytes().to_vec()).unwrap(),
                Change::Put {
                    bytes: Bytes::from(path.to_owned()),
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
        let commit = tip(&repository, Arc::clone(&runtime), &cancel).await;
        let path = repository
            .layout
            .repo_path(&format!("s3/attributes/{commit}.json"));
        let (bytes, _) = repository.store.get_with_etag(&path).await.unwrap();
        let stored: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(stored["version"], 2);
        assert_eq!(stored["checkpoint"], true);
        assert!(stored.get("objects").is_none());
        assert_eq!(stored["changes"].as_object().unwrap().len(), 1);
        assert!(stored["changes"].get("second.txt").is_some());
        let checkpoint_slot = stored["checkpoint_slot"].as_str().unwrap();
        let checkpoint = repository.layout.repo_path(&format!(
            "s3/attribute-checkpoints/branches/{checkpoint_slot}.json"
        ));
        repository.store.head(&checkpoint).await.unwrap();

        let manifest = attributes::load(&repository, commit).await.unwrap();
        let stale = attributes::prepare_checkpoint(
            "refs/heads/main",
            ObjectId::empty_tree(gix_hash::Kind::Sha1),
            &manifest,
        )
        .unwrap()
        .unwrap();
        attributes::publish_checkpoint(&repository, &stale)
            .await
            .unwrap();
        let fallback = attributes::load(&repository, commit).await.unwrap();
        for path in ["first.txt", "second.txt"] {
            let oid = object_id(Kind::Blob, path.as_bytes()).unwrap();
            assert!(fallback.object(path, oid).is_some());
        }
        let current = attributes::prepare_checkpoint("refs/heads/main", commit, &manifest)
            .unwrap()
            .unwrap();
        attributes::publish_checkpoint(&repository, &current)
            .await
            .unwrap();

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
