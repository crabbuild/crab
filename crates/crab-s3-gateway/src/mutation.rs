use std::{
    collections::HashMap,
    io::Write as _,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use crab_coordination::{GcFenceHeartbeat, GcFenceLease, PushLock};
use crab_metadata::{git_visibility, manifests::PackManifestEntry, ref_journal::RefJournalEdit};
use crab_remote_git::{EntryKind, EntryMode, OperationKind, RemoteGitRepository, Revision};
use gix_hash::ObjectId;
use gix_object::{Kind, WriteTo as _, bstr::BString, tree};
use md5::Digest as _;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::{attributes, gateway::Repository};

const LOCK_TTL: Duration = Duration::from_secs(300);
const MAX_GENERATED_PACK_BYTES: u64 = 512 * 1024 * 1024;
const MAX_QUEUED_WRITES_PER_REF: usize = 64;
const WRITE_QUEUE_TIMEOUT: Duration = Duration::from_secs(60);
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
    Delete,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum PutCondition {
    #[default]
    None,
    IfNoneMatchAny,
    IfMatch(ObjectId),
}

#[derive(Clone, Debug)]
pub(crate) struct Outcome {
    pub(crate) etag: Option<String>,
}

pub(crate) struct Coordinator {
    runtime: Arc<crab_remote_git::RemoteGitRuntime>,
    options: crab_remote_git::RepositoryOptions,
    admission: WriteAdmission,
}

impl Coordinator {
    pub(crate) fn new(
        runtime: Arc<crab_remote_git::RemoteGitRuntime>,
        options: crab_remote_git::RepositoryOptions,
    ) -> Self {
        Self {
            runtime,
            options,
            admission: WriteAdmission::default(),
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
        repository.maintenance.begin();
        let result = async {
            let _admitted = self
                .admission
                .acquire(&repository.config.name, branch, cancel)
                .await?;
            apply_admitted(
                repository,
                Arc::clone(&self.runtime),
                self.options,
                branch,
                path,
                change,
                principal,
                cancel,
            )
            .await
        }
        .await;
        if let Some((epoch, maintenance_cancel)) = repository.maintenance.finish(cancel) {
            crate::repository::schedule_readability(
                repository,
                Arc::clone(&self.runtime),
                self.options,
                maintenance_cancel,
                epoch,
            );
        }
        result
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
    Coordinator::new(runtime, options)
        .apply(repository, branch, path, change, principal, cancel)
        .await
}

#[derive(Default)]
struct WriteAdmission {
    refs: std::sync::Mutex<HashMap<String, Weak<RefQueue>>>,
}

struct RefQueue {
    gate: Arc<Semaphore>,
    admitted: AtomicUsize,
}

struct WritePermit {
    queue: Arc<RefQueue>,
    _permit: OwnedSemaphorePermit,
}

impl Drop for WritePermit {
    fn drop(&mut self) {
        self.queue.admitted.fetch_sub(1, Ordering::AcqRel);
    }
}

impl WriteAdmission {
    async fn acquire(
        &self,
        repository: &str,
        branch: &str,
        cancel: &CancellationToken,
    ) -> Result<WritePermit> {
        check_cancelled(cancel)?;
        let key = format!("{repository}\0{branch}");
        let queue = {
            let mut refs = self.refs.lock().map_err(|_| Error::AdmissionState)?;
            refs.retain(|_, queue| queue.strong_count() > 0);
            match refs.get(&key).and_then(Weak::upgrade) {
                Some(queue) => queue,
                None => {
                    let queue = Arc::new(RefQueue {
                        gate: Arc::new(Semaphore::new(1)),
                        admitted: AtomicUsize::new(0),
                    });
                    refs.insert(key, Arc::downgrade(&queue));
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
        let acquired = tokio::select! {
            () = cancel.cancelled() => Err(Error::Cancelled),
            result = tokio::time::timeout(WRITE_QUEUE_TIMEOUT, Arc::clone(&queue.gate).acquire_owned()) => {
                match result {
                    Ok(Ok(permit)) => Ok(permit),
                    Ok(Err(_)) => Err(Error::AdmissionState),
                    Err(_) => Err(Error::AdmissionTimeout),
                }
            }
        };
        match acquired {
            Ok(permit) => Ok(WritePermit {
                queue,
                _permit: permit,
            }),
            Err(error) => {
                queue.admitted.fetch_sub(1, Ordering::AcqRel);
                Err(error)
            }
        }
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
                LOCK_TTL,
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
    path: &'a crab_remote_git::GitPath,
    principal: &'a str,
    plan_id: Option<&'a str>,
}

async fn apply_admitted(
    repository: &Repository,
    runtime: Arc<crab_remote_git::RemoteGitRuntime>,
    options: crab_remote_git::RepositoryOptions,
    branch: &str,
    path: &crab_remote_git::GitPath,
    change: Change,
    principal: &str,
    cancel: &CancellationToken,
) -> Result<Outcome> {
    let completion_plan = match &change {
        Change::Put { attributes, .. } => {
            attributes
                .completion_upload_id
                .as_deref()
                .map(|id| CompletionPlan {
                    id: crate::multipart::publication_plan_id(id),
                    etag: attributes.etag_override.clone(),
                })
        }
        Change::Attributes { .. } | Change::Delete => None,
    };
    let Some(completion_plan) = completion_plan else {
        return apply_with_gc_fences(
            repository,
            runtime,
            options,
            change,
            ApplyRequest {
                branch,
                path,
                principal,
                plan_id: None,
            },
            cancel,
        )
        .await;
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
                change,
                ApplyRequest {
                    branch,
                    path,
                    principal,
                    plan_id: Some(&executing_plan_id),
                },
                &scoped,
            )
            .await
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
    change: Change,
    request: ApplyRequest<'_>,
    cancel: &CancellationToken,
) -> Result<Outcome> {
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
            change,
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
    change: Change,
    request: ApplyRequest<'_>,
    cancel: &CancellationToken,
) -> Result<Outcome> {
    for _ in 0..MAX_REPREPARE_ATTEMPTS {
        check_cancelled(cancel)?;
        let prepared = prepare_and_upload(
            repository,
            Arc::clone(&runtime),
            options,
            request.branch,
            request.path,
            change.clone(),
            request.principal,
            cancel,
        )
        .await?;
        let prepared = match prepared {
            Prepared::Noop(outcome) => return Ok(outcome),
            Prepared::Commit(prepared) => prepared,
        };
        let lease = RefLease::acquire(repository, request.branch, cancel).await?;
        let published = publish_prepared(
            repository,
            request.branch,
            &lease.holder,
            &prepared,
            request.plan_id,
            cancel,
        )
        .await;
        lease.release().await;
        match published? {
            Publish::Committed => {
                return Ok(Outcome {
                    etag: prepared.etag,
                });
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
    commit: ObjectId,
    etag: Option<String>,
    pack: PackManifestEntry,
    evidence_hash: String,
}

enum Prepared {
    Noop(Outcome),
    Commit(UploadedMutation),
}

enum Publish {
    Committed,
    Reprepare,
}

async fn prepare_and_upload(
    repository: &Repository,
    runtime: Arc<crab_remote_git::RemoteGitRuntime>,
    options: crab_remote_git::RepositoryOptions,
    branch: &str,
    path: &crab_remote_git::GitPath,
    change: Change,
    principal: &str,
    cancel: &CancellationToken,
) -> Result<Prepared> {
    let view = repository
        .read_views
        .current(repository, runtime, options, cancel)
        .await?;
    let parent = view
        .remote()
        .refs()
        .find(branch)
        .map(|reference| reference.target);
    let operation = view
        .remote()
        .operation(OperationKind::Repository, cancel)
        .await?;
    let built = async {
        let snapshot = match parent {
            Some(parent) => Some(view.snapshot(&parent.to_string(), &operation).await?),
            None => None,
        };
        let path_string = std::str::from_utf8(path.as_bytes())
            .map_err(|_| std::io::Error::other("S3 object path is not UTF-8"))?;
        let current_attributes = match &snapshot {
            Some(snapshot) => match snapshot.entry(path, &operation).await? {
                Some(entry) if entry.kind == EntryKind::Blob => {
                    view.object_attributes(
                        repository,
                        snapshot.commit_oid(),
                        path_string,
                        entry.oid,
                    )
                    .await?
                }
                Some(_) | None => None,
            },
            None => None,
        };
        build_commit(
            view.remote(),
            &operation,
            parent,
            path,
            change,
            principal,
            current_attributes.as_ref(),
        )
        .await
    }
    .await;
    let built = match operation.finish(Ok(())).await {
        Ok(()) => built?,
        Err(error) => return Err(error.into()),
    };
    let built = match built {
        Build::Noop(outcome) => return Ok(Prepared::Noop(outcome)),
        Build::Commit(built) => *built,
    };
    upload_built(repository, parent, built, cancel)
        .await
        .map(Prepared::Commit)
}

async fn upload_built(
    repository: &Repository,
    parent: Option<ObjectId>,
    built: BuiltCommit,
    cancel: &CancellationToken,
) -> Result<UploadedMutation> {
    let (pack_owner, pack) = prepare_pack(built.objects.clone(), cancel).await?;
    check_cancelled(cancel)?;
    let pack_id = pack.content_hash().to_hex().to_string();
    let visible_objects = built
        .objects
        .iter()
        .map(|(kind, bytes)| object_id(*kind, bytes).map(|oid| oid.to_string()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let evidence = match parent {
        Some(parent) => git_visibility::GitVisibilityEdit::from_delta_objects(
            Some(parent.to_string()),
            built.commit.to_string(),
            visible_objects,
            vec![],
        ),
        None => git_visibility::GitVisibilityEdit::from_replacement_objects(
            None,
            built.commit.to_string(),
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
        attributes::save_delta(
            repository,
            built.commit,
            parent,
            built.path.clone(),
            built.attributes.clone(),
        )
        .await
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
        commit: built.commit,
        etag: built.etag,
        pack: PackManifestEntry {
            pack_id: pack_id.clone(),
            content_hash: pack_id,
            size: pack.size(),
            object_count: pack.object_count().into(),
            ref_tips: vec![built.commit.to_string()],
        },
        evidence_hash,
    };
    drop(pack);
    drop(pack_owner);
    Ok(uploaded)
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
    let options = crab_write::journal::CommitOptions::new(LOCK_TTL, cancel);
    let options = match plan_id {
        Some(plan_id) => options.with_plan(plan_id),
        None => options,
    };
    crab_write::journal::commit_edits(
        &repository.store,
        &repository.layout,
        &snapshot,
        vec![RefJournalEdit {
            ref_name: branch.to_owned(),
            old_oid: prepared.parent.map(|oid| oid.to_string()),
            new_oid: Some(prepared.commit.to_string()),
            peeled_oid: None,
            lock_holder: Some(holder.to_owned()),
            visibility_evidence_hash: Some(prepared.evidence_hash.clone()),
        }],
        prepared.parent.is_none().then(|| branch.to_owned()),
        vec![prepared.pack.clone()],
        vec![],
        options,
    )
    .await?;
    Ok(Publish::Committed)
}

struct BuiltCommit {
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
    old_oid: Option<ObjectId>,
    entries: Vec<tree::Entry>,
}

async fn build_commit(
    remote: &RemoteGitRepository,
    operation: &crab_remote_git::OperationContext,
    parent: Option<ObjectId>,
    path: &crab_remote_git::GitPath,
    change: Change,
    principal: &str,
    current_attributes: Option<&attributes::ObjectAttributes>,
) -> Result<Build> {
    let components = path.components().map(<[u8]>::to_vec).collect::<Vec<_>>();
    let (name, directories) = components.split_last().ok_or(Error::IsDirectory)?;
    let snapshot = match parent {
        Some(parent) => Some(
            remote
                .snapshot(&Revision::Commit(parent), operation)
                .await?,
        ),
        None => None,
    };
    let mut frames = Vec::with_capacity(directories.len() + 1);
    let mut directory_path = crab_remote_git::GitPath::root();
    let mut directory_oid = snapshot.as_ref().map(|value| value.root_tree_oid());
    for depth in 0..=directories.len() {
        let entries = match (&snapshot, directory_oid) {
            (Some(snapshot), Some(_)) => {
                load_directory(snapshot, &directory_path, operation).await?
            }
            (None, None) | (Some(_), None) => Vec::new(),
            (None, Some(_)) => {
                return Err(std::io::Error::other("empty repository has a root tree").into());
            }
        };
        frames.push(TreeFrame {
            old_oid: directory_oid,
            entries,
        });
        if depth == directories.len() {
            break;
        }
        let component = &directories[depth];
        directory_oid = match find_entry(&frames[depth].entries, component) {
            Some(entry) if entry.mode == tree::EntryKind::Tree.into() => Some(entry.oid),
            Some(_) => return Err(Error::NotDirectory),
            None => None,
        };
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
    let lfs_attributes = match &change {
        Change::Put {
            track_lfs: true, ..
        } if name.as_slice() == b".gitattributes" => return Err(Error::InvalidAttributes),
        Change::Put {
            track_lfs: true, ..
        } => {
            prepare_lfs_attributes(
                snapshot.as_ref(),
                &directory_path,
                leaf_entries,
                name,
                operation,
            )
            .await?
        }
        Change::Put { .. } | Change::Attributes { .. } | Change::Delete => None,
    };
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
        Change::Delete => {
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
            Some(encode_tree(
                frame.old_oid,
                std::mem::take(&mut frame.entries),
                &mut objects,
            )?)
        };
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
    Ok(Build::Commit(Box::new(BuiltCommit {
        commit,
        etag,
        objects,
        path: path_string.to_owned(),
        attributes: object_attributes,
    })))
}

struct LfsAttributesChange {
    oid: ObjectId,
    mode: gix_object::tree::EntryMode,
    bytes: Vec<u8>,
}

async fn prepare_lfs_attributes(
    snapshot: Option<&crab_remote_git::RemoteGitSnapshot>,
    directory: &crab_remote_git::GitPath,
    entries: &[tree::Entry],
    filename: &[u8],
    operation: &crab_remote_git::OperationContext,
) -> Result<Option<LfsAttributesChange>> {
    let attributes_entry = find_entry(entries, b".gitattributes");
    let (mut bytes, mode, old_oid) = match (snapshot, attributes_entry) {
        (Some(snapshot), Some(entry)) => {
            let mode = entry_mode(entry.mode)?;
            if !matches!(mode, EntryMode::Regular | EntryMode::Executable) {
                return Err(Error::InvalidAttributes);
            }
            let path = push_component(directory, b".gitattributes")?;
            let blob = snapshot.read_blob(&path, operation).await?;
            if !matches!(
                crab_git::classify(&blob.bytes),
                crab_git::PointerKind::NotAPointer
            ) {
                return Err(Error::InvalidAttributes);
            }
            (blob.bytes.to_vec(), entry.mode, Some(entry.oid))
        }
        (_, None) => (Vec::new(), tree::EntryKind::Blob.into(), None),
        (None, Some(_)) => {
            return Err(
                std::io::Error::other("empty repository contains a .gitattributes entry").into(),
            );
        }
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
    entries
        .iter()
        .find(|entry| <BString as AsRef<[u8]>>::as_ref(&entry.filename) == name)
}

fn replace_entry(entries: &mut Vec<tree::Entry>, name: &[u8], replacement: Option<tree::Entry>) {
    entries.retain(|entry| <BString as AsRef<[u8]>>::as_ref(&entry.filename) != name);
    if let Some(replacement) = replacement {
        entries.push(replacement);
    }
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
    mut entries: Vec<tree::Entry>,
    objects: &mut Vec<(Kind, Vec<u8>)>,
) -> Result<ObjectId> {
    entries.sort();
    let mut bytes = Vec::new();
    gix_object::Tree { entries }.write_to(&mut bytes)?;
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
        let input = owner.path().join("generated.pack");
        write_pack(&input, &objects)?;
        let incoming = crab_git::incoming_pack::quarantine(
            std::io::BufReader::new(std::fs::File::open(&input)?),
            owner.path(),
            crab_git::incoming_pack::ReceiveLimits {
                max_pack_bytes: MAX_GENERATED_PACK_BYTES,
                max_objects: 1_000_000,
                max_object_bytes: 256 * 1024 * 1024,
                max_inflated_bytes: MAX_GENERATED_PACK_BYTES,
                max_delta_depth: 1,
            },
            || worker_flag.load(Ordering::Acquire),
            |_| Ok(None),
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

fn write_pack(path: &std::path::Path, objects: &[(Kind, Vec<u8>)]) -> Result<()> {
    let count = u32::try_from(objects.len())
        .map_err(|_| std::io::Error::other("too many generated Git objects"))?;
    let mut bytes = b"PACK\0\0\0\x02".to_vec();
    bytes.extend_from_slice(&count.to_be_bytes());
    for (kind, data) in objects {
        pack_header(*kind, data.len(), &mut bytes);
        let mut encoder =
            flate2::write::ZlibEncoder::new(&mut bytes, flate2::Compression::default());
        encoder.write_all(data)?;
        encoder.finish()?;
    }
    let mut hasher = gix_hash::hasher(gix_hash::Kind::Sha1);
    hasher.update(&bytes);
    bytes.extend_from_slice(hasher.try_finalize()?.as_bytes());
    std::fs::write(path, bytes)?;
    Ok(())
}

fn pack_header(kind: Kind, size: usize, output: &mut Vec<u8>) {
    let kind = match kind {
        Kind::Commit => 1,
        Kind::Tree => 2,
        Kind::Blob => 3,
        Kind::Tag => 4,
    };
    let mut remaining = size >> 4;
    let mut byte = (kind << 4) | (size as u8 & 0x0f);
    while remaining != 0 {
        output.push(byte | 0x80);
        byte = (remaining as u8) & 0x7f;
        remaining >>= 7;
    }
    output.push(byte);
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
    use std::collections::BTreeMap;

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
            Change::Delete,
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_ref_writes_queue_and_all_commit() {
        let (repository, runtime, cancel) = fixture().await;
        let repository = Arc::new(repository);
        let coordinator = Arc::new(Coordinator::new(
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
        ));
        let mut writes = tokio::task::JoinSet::new();
        for index in 0..8 {
            let repository = Arc::clone(&repository);
            let coordinator = Arc::clone(&coordinator);
            let cancel = cancel.clone();
            writes.spawn(async move {
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
        while let Some(result) = writes.join_next().await {
            result.unwrap().unwrap();
        }
        let mut canonical = None;
        for _ in 0..100 {
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
        assert!(stored.get("objects").is_none());
        assert_eq!(stored["changes"].as_object().unwrap().len(), 1);
        assert!(stored["changes"].get("second.txt").is_some());

        let manifest = attributes::load(&repository, commit).await.unwrap();
        for path in ["first.txt", "second.txt"] {
            let oid = object_id(Kind::Blob, path.as_bytes()).unwrap();
            assert!(manifest.object(path, oid).is_some());
        }
        runtime.shutdown().await;
    }
}
