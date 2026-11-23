use std::sync::Arc;
use std::time::Instant;

use crab_metadata::git_object_locator::{
    GitObjectKind, GitObjectLocator, GitObjectLocatorSession, GitObjectLookup, GitObjectMetadata,
    GitObjectOrdinal,
};
use futures_util::{FutureExt, future::Either};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tokio_util::task::task_tracker::TaskTrackerToken;
use tracing::Instrument as _;

use crate::budget::{BudgetUsage, OperationBudget};
use crate::objects::{
    RawTreeEntry, find_tree_entry, materialize_tree, parse_commit, parse_tag, parse_tree_raw,
};
use crate::reader::{GitObject, RemoteGitObjectMetadata, RemoteGitPackedEntry};
use crate::state::RepositoryState;
use crate::{
    AnnotatedTag, BudgetDimension, Commit, CorruptionStage, Error, GitPath, MetricKind,
    MetricObservation, MetricOutcome, OperationLimits, Result, TreeEntry,
};

/// Bounded semantic operation name used only for metrics and traces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OperationKind {
    /// General repository work before a more specific operation is selected.
    Repository,
    /// Revision resolution.
    Resolve,
    /// Snapshot creation.
    Snapshot,
    /// Commit metadata.
    Commit,
    /// Immediate directory listing.
    Tree,
    /// Last-change attribution for one directory page.
    DirectoryAttribution,
    /// Exact path metadata.
    Entry,
    /// Blob metadata without logical materialization.
    ContentMetadata,
    /// Verified Git blob content.
    Content,
    /// Repository commit history.
    History,
    /// History for one exact path.
    PathHistory,
    /// Recursive tree comparison.
    Compare,
    /// Textual or classified file difference.
    Diff,
    /// Line attribution.
    Blame,
    /// Snapshot archive traversal.
    Archive,
    /// Protocol-v2 upload-pack generation.
    UploadPack,
    /// Rebuild of generation-bound ref visibility from canonical objects.
    Visibility,
    /// Symbolic-link target metadata.
    Symlink,
    /// Submodule gitlink metadata.
    Submodule,
}

/// Exact object selection and shallow boundaries for one indexed fetch depth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShallowClosureSelection {
    /// Objects included in the shallow clone.
    pub object_ids: Vec<gix_hash::ObjectId>,
    /// Commits that the client must retain as shallow boundaries.
    pub shallow: Vec<gix_hash::ObjectId>,
}

impl OperationKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Repository => "repository",
            Self::Resolve => "resolve",
            Self::Snapshot => "snapshot",
            Self::Commit => "commit",
            Self::Tree => "tree",
            Self::DirectoryAttribution => "directory_attribution",
            Self::Entry => "entry",
            Self::ContentMetadata => "content_metadata",
            Self::Content => "content",
            Self::History => "history",
            Self::PathHistory => "path_history",
            Self::Compare => "compare",
            Self::Diff => "diff",
            Self::Blame => "blame",
            Self::Archive => "archive",
            Self::UploadPack => "upload_pack",
            Self::Visibility => "visibility",
            Self::Symlink => "symlink",
            Self::Submodule => "submodule",
        }
    }
}

pub(crate) struct TrackedLocatorSession {
    session: Option<GitObjectLocatorSession>,
    runtime: Arc<crate::RemoteGitRuntime>,
}

impl TrackedLocatorSession {
    pub(crate) fn new(
        session: GitObjectLocatorSession,
        runtime: Arc<crate::RemoteGitRuntime>,
    ) -> Self {
        Self {
            session: Some(session),
            runtime,
        }
    }

    fn session(&self) -> Option<&GitObjectLocatorSession> {
        self.session.as_ref()
    }

    pub(crate) async fn close(mut self) -> crab_metadata::error::Result<()> {
        let Some(session) = self.session.take() else {
            return Ok(());
        };
        let mut close = Box::pin(session.close());
        if let std::task::Poll::Ready(result) = futures_util::poll!(&mut close) {
            return result;
        }
        // Resource-free sessions close immediately. A pending close transfers
        // to runtime ownership before yielding, so caller cancellation cannot
        // abandon a live metadata reader.
        let (sender, receiver) = oneshot::channel();
        self.runtime.track_cleanup(async move {
            let _ = sender.send(close.await);
        });
        receiver.await.map_err(|_| {
            crab_metadata::error::MetadataError::Internal(
                "tracked locator close ended without a result".to_owned(),
            )
        })?
    }
}

impl Drop for TrackedLocatorSession {
    fn drop(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        self.runtime.track_cleanup(async move {
            let _ = session.close().await;
        });
    }
}

/// One bounded, cancellation-aware repository operation.
///
/// The context owns one locator session pinned to the repository generation.
/// Callers must pass the same context through dependent reads and finish it on
/// success or failure so the underlying metadata reader is closed explicitly.
pub struct OperationContext {
    state: Arc<RepositoryState>,
    limits: OperationLimits,
    cancellation: CancellationToken,
    deadline: Option<tokio::time::Instant>,
    deadline_stop: CancellationToken,
    budget: OperationBudget,
    session: Option<TrackedLocatorSession>,
    started: Instant,
    kind: OperationKind,
    finished: bool,
    correlation_id: u64,
    span: tracing::Span,
    _task_token: TaskTrackerToken,
}

impl OperationContext {
    pub(crate) async fn open(
        state: Arc<RepositoryState>,
        kind: OperationKind,
        cancellation: &CancellationToken,
        limits: OperationLimits,
    ) -> Result<Self> {
        Self::open_inner(state, kind, cancellation, limits, true, None).await
    }

    pub(crate) async fn open_with_budget(
        state: Arc<RepositoryState>,
        kind: OperationKind,
        cancellation: &CancellationToken,
        limits: OperationLimits,
        budget: OperationBudget,
    ) -> Result<Self> {
        Self::open_inner(state, kind, cancellation, limits, true, Some(budget)).await
    }

    pub(crate) async fn open_shared(
        state: Arc<RepositoryState>,
        cancellation: &CancellationToken,
        shared: Arc<crate::budget::SharedBudget>,
    ) -> Result<Self> {
        // Repository limits supply batching hints, not a producer ceiling.
        // Every charge and remaining allowance comes from the live waiters.
        let hints = state.options.operation_limits();
        let budget = OperationBudget::shared(shared, state.runtime.clone());
        Self::open_inner(
            state,
            OperationKind::Blame,
            cancellation,
            hints,
            true,
            Some(budget),
        )
        .await
    }

    pub(crate) fn budget(&self) -> &OperationBudget {
        &self.budget
    }

    pub(crate) fn metadata_store(&self) -> crab_storage::Store {
        self.state
            .store
            .clone()
            .with_read_admission(self.read_admission())
    }

    pub(crate) async fn read_metadata<T>(
        &self,
        read: impl std::future::Future<Output = crab_metadata::error::Result<T>>,
    ) -> Result<T> {
        self.ensure_active()?;
        let result = tokio::select! {
            biased;
            () = self.cancellation.cancelled() => {
                self.ensure_active()?;
                return Err(Error::Cancelled);
            }
            result = read => result?,
        };
        self.ensure_active()?;
        Ok(result)
    }

    pub(crate) async fn open_pack_transfer(
        state: Arc<RepositoryState>,
        cancellation: &CancellationToken,
        limits: OperationLimits,
    ) -> Result<Self> {
        Self::open_inner(
            state,
            OperationKind::UploadPack,
            cancellation,
            limits,
            false,
            None,
        )
        .await
    }

    async fn open_inner(
        state: Arc<RepositoryState>,
        kind: OperationKind,
        cancellation: &CancellationToken,
        limits: OperationLimits,
        open_catalog: bool,
        budget: Option<OperationBudget>,
    ) -> Result<Self> {
        let task_token = state.runtime.operation_token();
        let runtime_cancellation = state.runtime.background_cancellation();
        let started = Instant::now();
        let max_duration = limits.max_duration;
        let shared = budget.as_ref().is_some_and(OperationBudget::is_shared);
        let deadline = if shared {
            None
        } else {
            Some(
                tokio::time::Instant::now()
                    .checked_add(max_duration)
                    .ok_or(Error::InvalidLimit {
                        name: "operation duration",
                    })?,
            )
        };
        let budget =
            budget.unwrap_or_else(|| OperationBudget::new(limits, Arc::clone(&state.runtime)));
        let operation_cancellation = cancellation.child_token();
        let cancel_failed_open = operation_cancellation.clone().drop_guard();
        let correlation_id = budget.id();
        let span = operation_span(correlation_id, kind);
        check_cancelled(cancellation)?;
        check_cancelled(&runtime_cancellation)?;
        let session = if open_catalog && let Some(catalog) = state.lookup_catalog_identity {
            // Keep catalog acquisition and later page reads on the same budget.
            // The pinned repository store remains reusable by other operations.
            let store = state
                .store
                .clone()
                .with_read_admission(budget.read_admission(operation_cancellation.clone()));
            let opening = GitObjectLocatorSession::open_for_catalog(
                Arc::clone(store.inner()),
                state.layout.repo_prefix(),
                catalog,
                max_duration,
            );
            // Selecting before awaiting avoids a second slot for the large
            // opening future in the enclosing async state.
            let opening = match deadline {
                Some(deadline) => Either::Left(tokio::time::timeout_at(deadline, opening)),
                None => Either::Right(opening.map(Ok)),
            };
            let session = tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(Error::Cancelled),
                () = runtime_cancellation.cancelled() => return Err(Error::Cancelled),
                session = opening => session.map_err(|_| Error::Timeout {
                    operation: "open locator",
                })?,
            };
            let session = match session {
                Ok(session) => session,
                Err(error) if state.catalog_identity.is_none() => {
                    tracing::warn!(%error, "snapshot lookup catalog unavailable; using immutable pack indexes");
                    GitObjectLocatorSession::without_catalog()
                }
                Err(error) => return Err(error.into()),
            };
            let session = TrackedLocatorSession::new(session, Arc::clone(&state.runtime));
            // A handle pins the immutable catalog opened with its refs. Using
            // the latest checkpoint here would invalidate it after a push.
            Some(session)
        } else if state.reader.is_some() {
            Some(TrackedLocatorSession::new(
                GitObjectLocatorSession::without_catalog(),
                Arc::clone(&state.runtime),
            ))
        } else {
            None
        };
        if cancellation.is_cancelled() || runtime_cancellation.is_cancelled() {
            if let Some(session) = session {
                session.close().await?;
            }
            return Err(Error::Cancelled);
        }
        if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
            if let Some(session) = session {
                session.close().await?;
            }
            return Err(Error::Timeout {
                operation: "open locator",
            });
        }
        let operation_cancellation = cancel_failed_open.disarm();
        let deadline_stop = CancellationToken::new();
        let deadline_cancellation = operation_cancellation.clone();
        let deadline_finished = deadline_stop.clone();
        let shutdown_cancellation = runtime_cancellation;
        state.runtime.track_cleanup(async move {
            tokio::select! {
                biased;
                () = deadline_finished.cancelled() => {}
                () = shutdown_cancellation.cancelled() => deadline_cancellation.cancel(),
                () = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending::<()>().await,
                    }
                } => deadline_cancellation.cancel(),
            }
        });
        Ok(Self {
            budget,
            state,
            limits,
            cancellation: operation_cancellation,
            deadline,
            deadline_stop,
            session,
            started,
            kind,
            finished: false,
            correlation_id,
            span,
            _task_token: task_token,
        })
    }

    pub(crate) async fn all_catalog_object_ids(&self) -> Result<Vec<gix_hash::ObjectId>> {
        check_cancelled(&self.cancellation)?;
        let session = self
            .session
            .as_ref()
            .and_then(TrackedLocatorSession::session)
            .ok_or(Error::InternalInvariant {
                invariant: "catalog object scan has no locator session",
            })?;
        let object_ids = tokio::select! {
            biased;
            () = self.cancellation.cancelled() => return Err(Error::Cancelled),
            object_ids = session.all_object_ids() => object_ids?,
        };
        Ok(object_ids
            .into_iter()
            .map(gix_hash::ObjectId::from)
            .collect())
    }

    /// Return the cancellation token governing all work in this operation.
    #[must_use]
    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// Return the process-local protected correlation identifier.
    #[must_use]
    pub const fn correlation_id(&self) -> u64 {
        self.correlation_id
    }

    /// Finish the operation and explicitly close its locator session.
    ///
    /// When both the semantic operation and close fail, the semantic error is
    /// retained as the source and the typed close error remains available for
    /// protected diagnostics. Pass the operation result without propagating it
    /// first, so an error cannot bypass explicit cleanup:
    ///
    /// ```no_run
    /// use crab_remote_git::{OperationKind, RemoteGitRepository, RemoteGitSnapshot, Result, Revision};
    /// use tokio_util::sync::CancellationToken;
    ///
    /// # async fn snapshot(repository: &RemoteGitRepository, revision: &Revision, cancel: &CancellationToken) -> Result<RemoteGitSnapshot> {
    /// let operation = repository.operation(OperationKind::Snapshot, cancel).await?;
    /// let result = repository.snapshot(revision, &operation).await;
    /// operation.finish(result).await
    /// # }
    /// ```
    pub async fn finish<T>(mut self, operation: Result<T>) -> Result<T> {
        let timed_out = self
            .deadline
            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline);
        let interrupted = timed_out || self.cancellation.is_cancelled();
        self.deadline_stop.cancel();
        let operation = match operation {
            Ok(_) if timed_out => Err(Error::Timeout {
                operation: "repository operation",
            }),
            // Admission may return wrapped cancellation before a read's outer
            // select observes it. Preserve real failures and report scope reason.
            Err(error) if interrupted => Err(error.after_interruption(timed_out)),
            result => result,
        };
        let close = match self.session.take() {
            Some(session) => session.close().instrument(self.span.clone()).await,
            None => Ok(()),
        };
        let result = finish_with_close(operation, close);
        let outcome = match &result {
            Ok(_) => MetricOutcome::Success,
            Err(Error::Cancelled) => MetricOutcome::Cancelled,
            Err(_) => MetricOutcome::Error,
        };
        let usage = self.budget.usage().await;
        self.state.runtime.metrics().record(MetricObservation {
            kind: MetricKind::Operation,
            value: 1,
            duration: Some(self.started.elapsed()),
            outcome: Some(outcome),
            cache: None,
        });
        record_completion(
            &self.span,
            self.correlation_id,
            outcome,
            result.as_ref().err(),
        );
        tracing::info!(
            target: "crab_remote_git::telemetry",
            parent: &self.span,
            telemetry_event = "operation_summary",
            correlation_id = self.correlation_id,
            operation = self.kind.as_str(),
            outcome = ?outcome,
            duration_ms = self.started.elapsed().as_millis() as u64,
            logical_objects = usage.amount(BudgetDimension::LogicalObjects),
            storage_requests = usage.amount(BudgetDimension::StorageRequests),
            fetched_bytes = usage.amount(BudgetDimension::FetchedBytes),
            inflated_bytes = usage.amount(BudgetDimension::InflatedBytes),
            response_bytes = usage.amount(BudgetDimension::ResponseBytes),
            "remote Git operation summary"
        );
        self.finished = true;
        result
    }

    pub(crate) fn belongs_to(&self, state: &Arc<RepositoryState>) -> bool {
        Arc::ptr_eq(&self.state, state)
    }

    /// Return the maximum number of logical objects this operation may read.
    #[must_use]
    pub fn max_logical_objects(&self) -> u64 {
        self.limits.max_logical_objects
    }

    /// Resolve the published object kinds without reading packed object bodies.
    pub async fn catalog_object_kinds(
        &self,
        object_ids: &[[u8; 20]],
    ) -> Result<Vec<Option<GitObjectKind>>> {
        check_cancelled(&self.cancellation)?;
        let session = self
            .session
            .as_ref()
            .and_then(TrackedLocatorSession::session)
            .ok_or(Error::InternalInvariant {
                invariant: "non-empty operation has no locator session",
            })?;
        let lookups = tokio::select! {
            biased;
            () = self.cancellation.cancelled() => return Err(Error::Cancelled),
            lookups = session.lookup_batch(object_ids, &self.state.inventory) => lookups?,
        };
        if lookups.len() != object_ids.len() {
            return Err(Error::Corrupt {
                stage: CorruptionStage::Locator,
            });
        }
        lookups
            .into_iter()
            .map(|lookup| match lookup {
                GitObjectLookup::Hit(locator) => Ok(locator.metadata.kind),
                GitObjectLookup::Miss => Ok(None),
                GitObjectLookup::Corrupt => Err(Error::Corrupt {
                    stage: CorruptionStage::Locator,
                }),
            })
            .collect()
    }

    /// Resolve OIDs to dense ordinals in the operation's pinned catalog.
    pub async fn catalog_object_ordinals(
        &self,
        object_ids: &[gix_hash::ObjectId],
    ) -> Result<Vec<Option<GitObjectOrdinal>>> {
        self.catalog_object_ordinals_with(object_ids).await
    }

    async fn catalog_object_ordinals_with(
        &self,
        object_ids: &[gix_hash::ObjectId],
    ) -> Result<Vec<Option<GitObjectOrdinal>>> {
        check_cancelled(&self.cancellation)?;
        let session = self
            .session
            .as_ref()
            .and_then(TrackedLocatorSession::session)
            .ok_or(Error::InternalInvariant {
                invariant: "non-empty operation has no locator session",
            })?;
        let object_ids = object_ids
            .iter()
            .map(|object_id| {
                object_id.as_bytes().try_into().map_err(|_| Error::Corrupt {
                    stage: CorruptionStage::Locator,
                })
            })
            .collect::<Result<Vec<[u8; 20]>>>()?;
        let lookups = tokio::select! {
            biased;
            () = self.cancellation.cancelled() => return Err(Error::Cancelled),
            lookups = session.lookup_batch(&object_ids, &self.state.inventory) => lookups?,
        };
        if lookups.len() != object_ids.len() {
            return Err(Error::Corrupt {
                stage: CorruptionStage::Locator,
            });
        }
        lookups
            .into_iter()
            .map(|lookup| match lookup {
                GitObjectLookup::Hit(locator) => Ok(Some(locator.ordinal)),
                GitObjectLookup::Miss => Ok(None),
                GitObjectLookup::Corrupt => Err(Error::Corrupt {
                    stage: CorruptionStage::Locator,
                }),
            })
            .collect()
    }

    /// Resolve dense catalog ordinals to OIDs in the operation's pinned catalog.
    pub async fn catalog_object_ids_by_ordinal(
        &self,
        ordinals: &[GitObjectOrdinal],
    ) -> Result<Vec<Option<gix_hash::ObjectId>>> {
        check_cancelled(&self.cancellation)?;
        let session = self
            .session
            .as_ref()
            .and_then(TrackedLocatorSession::session)
            .ok_or(Error::InternalInvariant {
                invariant: "non-empty operation has no locator session",
            })?;
        let object_ids = tokio::select! {
            biased;
            () = self.cancellation.cancelled() => return Err(Error::Cancelled),
            object_ids = session.object_ids_by_ordinal(ordinals) => object_ids?,
        };
        object_ids
            .into_iter()
            .map(|object_id| Ok(object_id.map(gix_hash::ObjectId::from)))
            .collect()
    }

    /// Resolve complete published object metadata by dense catalog ordinal.
    ///
    /// `None` means the pinned catalog does not have a complete ordinal
    /// metadata sidecar; callers must use their bounded canonical fallback.
    pub async fn catalog_object_metadata_by_ordinal(
        &self,
        ordinals: &[GitObjectOrdinal],
    ) -> Result<Option<Vec<GitObjectMetadata>>> {
        check_cancelled(&self.cancellation)?;
        let session = self
            .session
            .as_ref()
            .and_then(TrackedLocatorSession::session)
            .ok_or(Error::InternalInvariant {
                invariant: "non-empty operation has no locator session",
            })?;
        tokio::select! {
            biased;
            () = self.cancellation.cancelled() => Err(Error::Cancelled),
            metadata = session.metadata_by_ordinal(ordinals) => metadata.map_err(Error::from),
        }
    }

    /// Load an exact generation-bound shallow object closure when available.
    pub async fn shallow_object_closure(
        &self,
        tip: gix_hash::ObjectId,
        depth: u32,
    ) -> Result<Option<ShallowClosureSelection>> {
        check_cancelled(&self.cancellation)?;
        let Some(tip) = tip.as_bytes().try_into().ok() else {
            return Ok(None);
        };
        let Some(index) = self.state.shallow_closure.as_ref() else {
            return Ok(None);
        };
        let Some(reference) = index.entry(&tip, depth) else {
            return Ok(None);
        };
        self.budget
            .charge(
                BudgetDimension::LogicalObjects,
                u64::from(reference.object_count),
            )
            .await?;
        let store = self
            .state
            .store
            .clone()
            .with_read_admission(self.read_admission());
        let entry = tokio::select! {
            biased;
            () = self.cancellation.cancelled() => return Err(Error::Cancelled),
            result = crab_metadata::shallow_closure::load_shallow_closure_entry(
                &store,
                &self.state.layout,
                reference,
                crab_metadata::shallow_closure::DEFAULT_MAX_SHALLOW_CLOSURE_ENTRY_BYTES,
            ) => result?,
        };
        Ok(Some(ShallowClosureSelection {
            object_ids: entry
                .object_ids
                .into_iter()
                .map(gix_hash::ObjectId::from)
                .collect(),
            shallow: entry
                .shallow
                .into_iter()
                .map(gix_hash::ObjectId::from)
                .collect(),
        }))
    }

    /// Return the maximum complete pack response size for this operation.
    #[must_use]
    pub fn max_response_bytes(&self) -> u64 {
        self.limits.max_response_bytes
    }

    /// Return the maximum source bytes this operation may fetch.
    #[must_use]
    pub fn max_fetched_bytes(&self) -> u64 {
        self.limits.max_fetched_bytes
    }

    pub(crate) async fn single_pack_checksum_for_exact_objects(
        &self,
        pack_id: crab_xet::hash::MerkleHash,
        object_ids: &[gix_hash::ObjectId],
        allowed_external_bases: &[gix_hash::ObjectId],
    ) -> Result<Option<[u8; 20]>> {
        let reader = self.state.reader.as_ref().ok_or(Error::EmptyRepository)?;
        let checksum = reader
            .pack_checksum_for_exact_objects(
                pack_id,
                object_ids,
                allowed_external_bases,
                &self.budget,
                &self.cancellation,
            )
            .await?;
        if checksum.is_some() {
            self.budget
                .charge(BudgetDimension::LogicalObjects, object_ids.len() as u64)
                .await?;
        }
        Ok(checksum)
    }

    pub(crate) async fn download_pack_to_path(
        &self,
        pack_id: crab_xet::hash::MerkleHash,
        expected_size: u64,
        destination: &std::path::Path,
        progress: Option<&(dyn Fn(u64) + Send + Sync)>,
    ) -> Result<crab_git::pack::VerifiedPackIdentity> {
        let reader = self.state.reader.as_ref().ok_or(Error::EmptyRepository)?;
        reader
            .download_pack_to_path(
                pack_id,
                expected_size,
                destination,
                &self.budget,
                &self.cancellation,
                progress,
            )
            .await
    }

    pub(crate) async fn download_pack_index_to_path(
        &self,
        pack_id: crab_xet::hash::MerkleHash,
        maximum_size: u64,
        destination: &std::path::Path,
    ) -> Result<()> {
        let reader = self.state.reader.as_ref().ok_or(Error::EmptyRepository)?;
        reader
            .download_pack_index_to_path(
                pack_id,
                maximum_size,
                destination,
                &self.budget,
                &self.cancellation,
            )
            .await
    }

    pub(crate) async fn download_pack_reverse_index_to_path(
        &self,
        pack_id: crab_xet::hash::MerkleHash,
        maximum_size: u64,
        destination: &std::path::Path,
    ) -> Result<()> {
        let reader = self.state.reader.as_ref().ok_or(Error::EmptyRepository)?;
        reader
            .download_pack_reverse_index_to_path(
                pack_id,
                maximum_size,
                destination,
                &self.budget,
                &self.cancellation,
            )
            .await
    }

    /// Read one verified Git object from the pinned repository generation.
    pub async fn read_object(&self, oid: gix_hash::ObjectId) -> Result<crate::RemoteGitObject> {
        check_cancelled(&self.cancellation)?;
        self.budget
            .charge(BudgetDimension::LogicalObjects, 1)
            .await?;
        self.read_object_uncharged(oid).await
    }

    /// Read a verified Git object if its declared final size is at most `maximum`.
    ///
    /// An oversized root returns `None` before its final body is reconstructed.
    /// Supporting delta bases and all fetched data still use the operation's
    /// ordinary limits. Callers must prove that the object is visible.
    pub async fn read_object_at_most(
        &self,
        oid: gix_hash::ObjectId,
        maximum: u64,
    ) -> Result<Option<crate::RemoteGitObject>> {
        check_cancelled(&self.cancellation)?;
        self.budget
            .charge(BudgetDimension::LogicalObjects, 1)
            .await?;
        let reader = self.state.reader.as_ref().ok_or(Error::EmptyRepository)?;
        let session = self
            .session
            .as_ref()
            .and_then(TrackedLocatorSession::session)
            .ok_or(Error::InternalInvariant {
                invariant: "non-empty operation has no locator session",
            })?;
        let read: futures_util::future::BoxFuture<'_, Result<Option<GitObject>>> =
            Box::pin(reader.read_with_session_at_most(
                session,
                oid,
                maximum,
                &self.budget,
                &self.cancellation,
            ));
        read.instrument(self.span.clone()).await
    }

    async fn read_object_uncharged(&self, oid: gix_hash::ObjectId) -> Result<GitObject> {
        let reader = self.state.reader.as_ref().ok_or(Error::EmptyRepository)?;
        let session = self
            .session
            .as_ref()
            .and_then(TrackedLocatorSession::session)
            .ok_or(Error::InternalInvariant {
                invariant: "non-empty operation has no locator session",
            })?;
        // Bound the reader's nested future layout at its operation boundary so
        // downstream async callers fit Rust's default type-depth limit.
        let read: futures_util::future::BoxFuture<'_, Result<GitObject>> =
            Box::pin(reader.read_with_session(session, oid, &self.budget, &self.cancellation));
        read.instrument(self.span.clone()).await
    }

    pub(crate) async fn read_commit(&self, oid: gix_hash::ObjectId) -> Result<Commit> {
        Ok(self.read_commit_shared(oid).await?.value.clone())
    }

    pub(crate) async fn read_commits(&self, oids: &[gix_hash::ObjectId]) -> Result<Vec<Commit>> {
        let shared = self.read_commits_shared(oids).await?;
        let mut commits = Vec::new();
        commits
            .try_reserve_exact(shared.len())
            .map_err(|source| Error::Allocation {
                requested: shared.len().saturating_mul(std::mem::size_of::<Commit>()),
                source,
            })?;
        commits.extend(shared.iter().map(|commit| commit.value.clone()));
        Ok(commits)
    }

    pub(crate) async fn read_commit_shared(
        &self,
        oid: gix_hash::ObjectId,
    ) -> Result<crate::objects::SharedCommit> {
        self.budget
            .charge(BudgetDimension::LogicalObjects, 1)
            .await?;
        if let Some(commit) = self.cached_commit(oid).await {
            return Ok(commit);
        }
        let object = self.read_object_uncharged(oid).await?;
        let source_bytes = object.data.len() as u64;
        let commit = self.parse_shared_commit(&object).await?;
        let key =
            crate::runtime::ObjectCacheKey::new(&self.state.identity, self.state.generation, oid);
        self.state
            .runtime
            .insert_commit(key, Arc::clone(&commit), source_bytes)
            .await;
        Ok(commit)
    }

    pub(crate) async fn cached_commit(
        &self,
        oid: gix_hash::ObjectId,
    ) -> Option<crate::objects::SharedCommit> {
        let key =
            crate::runtime::ObjectCacheKey::new(&self.state.identity, self.state.generation, oid);
        self.state
            .runtime
            .cached_commit(&key, self.state.options.object_limits().max_object_bytes)
            .await
    }

    pub(crate) async fn read_commits_shared(
        &self,
        oids: &[gix_hash::ObjectId],
    ) -> Result<Vec<crate::objects::SharedCommit>> {
        check_cancelled(&self.cancellation)?;
        self.budget
            .charge(BudgetDimension::LogicalObjects, oids.len() as u64)
            .await?;
        let mut retained = Vec::new();
        retained
            .try_reserve_exact(oids.len())
            .map_err(|source| Error::Allocation {
                requested: oids
                    .len()
                    .saturating_mul(std::mem::size_of::<Option<crate::objects::SharedCommit>>()),
                source,
            })?;
        let mut missing = Vec::new();
        missing
            .try_reserve_exact(oids.len())
            .map_err(|source| Error::Allocation {
                requested: oids
                    .len()
                    .saturating_mul(std::mem::size_of::<gix_hash::ObjectId>()),
                source,
            })?;
        // History already uses this verified parsed cache for point reads.
        // Batching must keep those size checks and avoid reparsing warm commits.
        for oid in oids {
            let cached = self.cached_commit(*oid).await;
            if cached.is_none() {
                missing.push(*oid);
            }
            retained.push(cached);
        }
        let objects = if missing.is_empty() {
            Vec::new()
        } else {
            self.read_objects_uncharged(&missing).await?
        };
        let mut objects = objects.into_iter();
        for slot in retained.iter_mut().filter(|slot| slot.is_none()) {
            let object = objects.next().ok_or(Error::InternalInvariant {
                invariant: "commit batch retains every requested missing object",
            })?;
            let source_bytes = object.data.len() as u64;
            let commit = self.parse_shared_commit(&object).await?;
            let key = crate::runtime::ObjectCacheKey::new(
                &self.state.identity,
                self.state.generation,
                commit.oid,
            );
            self.state
                .runtime
                .insert_commit(key, Arc::clone(&commit), source_bytes)
                .await;
            *slot = Some(commit);
        }
        let mut commits = Vec::new();
        commits
            .try_reserve_exact(retained.len())
            .map_err(|source| Error::Allocation {
                requested: retained.len().saturating_mul(std::mem::size_of::<Commit>()),
                source,
            })?;
        for slot in retained {
            let commit = slot.ok_or(Error::InternalInvariant {
                invariant: "commit batch fills every requested position",
            })?;
            commits.push(commit);
        }
        Ok(commits)
    }

    async fn parse_shared_commit(
        &self,
        object: &GitObject,
    ) -> Result<crate::objects::SharedCommit> {
        let headers = object
            .data
            .split(|byte| *byte == b'\n')
            .take_while(|line| !line.is_empty())
            .filter(|line| !line.starts_with(b" "))
            .count();
        let overhead = headers.saturating_mul(std::mem::size_of::<crate::SignatureHeader>() * 2);
        let capacity = object
            .data
            .len()
            .saturating_add(overhead)
            .saturating_add(std::mem::size_of::<Commit>());
        let temporary = self.state.runtime.reserve_bytes(capacity).await?;
        let retained = self.state.runtime.reserve_bytes(capacity).await?;
        let large = object.data.len() >= 64 * 1024;
        let object = object.clone();
        let parse = move || {
            let _temporary = temporary;
            Ok(Arc::new(crate::resident::Retained::new(
                parse_commit(&object)?,
                retained,
            )))
        };
        if large {
            self.state.runtime.run_cpu(&self.cancellation, parse).await
        } else {
            parse()
        }
    }

    pub(crate) async fn parse_tag_object(&self, object: &GitObject) -> Result<AnnotatedTag> {
        let key = crate::runtime::ObjectCacheKey::new(
            &self.state.identity,
            self.state.generation,
            object.oid,
        );
        let source_bytes = object.data.len() as u64;
        if let Some(tag) = self
            .state
            .runtime
            .cached_tag(&key, self.state.options.object_limits().max_object_bytes)
            .await
        {
            return Ok(tag.as_ref().clone());
        }
        let tag = Arc::new(parse_tag(object)?);
        self.state
            .runtime
            .insert_tag(key, Arc::clone(&tag), source_bytes)
            .await;
        Ok(tag.as_ref().clone())
    }

    pub(crate) async fn read_tree(
        &self,
        oid: gix_hash::ObjectId,
        parent: &GitPath,
    ) -> Result<Vec<TreeEntry>> {
        self.budget
            .charge(BudgetDimension::LogicalObjects, 1)
            .await?;
        let tree = self.read_raw_tree(oid).await?;
        materialize_tree(&tree, parent)
    }

    pub(crate) async fn read_tree_entry(
        &self,
        oid: gix_hash::ObjectId,
        parent: &GitPath,
        name: &[u8],
    ) -> Result<Option<TreeEntry>> {
        self.budget
            .charge(BudgetDimension::LogicalObjects, 1)
            .await?;
        let tree = self.read_raw_tree(oid).await?;
        let (entry, comparisons) = find_tree_entry(&tree, parent, name)?;
        self.budget
            .charge(BudgetDimension::Entries, comparisons)
            .await?;
        Ok(entry)
    }

    pub(crate) async fn read_tree_entries(
        &self,
        oids: &[gix_hash::ObjectId],
        parent: &GitPath,
        name: &[u8],
    ) -> Result<Vec<Option<TreeEntry>>> {
        check_cancelled(&self.cancellation)?;
        // Old path histories often repeat directory OIDs across adjacent commits.
        // Charge and decode each tree once while retaining request-order results.
        let mut unique_oids = Vec::new();
        unique_oids
            .try_reserve_exact(oids.len())
            .map_err(|source| Error::Allocation {
                requested: oids
                    .len()
                    .saturating_mul(std::mem::size_of::<gix_hash::ObjectId>()),
                source,
            })?;
        for oid in oids {
            if !unique_oids.contains(oid) {
                unique_oids.push(*oid);
            }
        }
        self.budget
            .charge(BudgetDimension::LogicalObjects, unique_oids.len() as u64)
            .await?;
        let objects = self.read_objects_uncharged(&unique_oids).await?;
        let mut unique_found = Vec::new();
        unique_found
            .try_reserve_exact(objects.len())
            .map_err(|source| Error::Allocation {
                requested: objects
                    .len()
                    .saturating_mul(std::mem::size_of::<Option<TreeEntry>>()),
                source,
            })?;
        let mut comparisons = 0u64;
        for object in objects {
            let source_bytes = object.data.len() as u64;
            let tree = self.parse_shared_tree(&object).await?;
            let key = crate::runtime::ObjectCacheKey::new(
                &self.state.identity,
                self.state.generation,
                object.oid,
            );
            self.state
                .runtime
                .insert_tree(key, Arc::clone(&tree), source_bytes)
                .await;
            let (entry, entry_comparisons) = find_tree_entry(&tree, parent, name)?;
            comparisons = comparisons.saturating_add(entry_comparisons);
            unique_found.push(entry);
        }
        self.budget
            .charge(BudgetDimension::Entries, comparisons)
            .await?;
        let mut found = Vec::new();
        found
            .try_reserve_exact(oids.len())
            .map_err(|source| Error::Allocation {
                requested: oids
                    .len()
                    .saturating_mul(std::mem::size_of::<Option<TreeEntry>>()),
                source,
            })?;
        for oid in oids {
            let position = unique_oids
                .iter()
                .position(|candidate| candidate == oid)
                .ok_or(Error::InternalInvariant {
                    invariant: "tree batch lost a requested object ID",
                })?;
            let entry = unique_found.get(position).ok_or(Error::InternalInvariant {
                invariant: "tree batch result did not match requested object IDs",
            })?;
            found.push(entry.clone());
        }
        Ok(found)
    }

    pub(crate) async fn read_raw_tree(
        &self,
        oid: gix_hash::ObjectId,
    ) -> Result<crate::objects::SharedTree> {
        let key =
            crate::runtime::ObjectCacheKey::new(&self.state.identity, self.state.generation, oid);
        let maximum = self.state.options.object_limits().max_object_bytes;
        let tree = match self.state.runtime.cached_tree(&key, maximum).await {
            Some(tree) => tree,
            None => {
                let object = self.read_object_uncharged(oid).await?;
                let source_bytes = object.data.len() as u64;
                let tree = self.parse_shared_tree(&object).await?;
                self.state
                    .runtime
                    .insert_tree(key, Arc::clone(&tree), source_bytes)
                    .await;
                tree
            }
        };
        Ok(tree)
    }

    async fn parse_shared_tree(&self, object: &GitObject) -> Result<crate::objects::SharedTree> {
        // Even the shortest encoded entry includes a mode, separator, NUL and
        // SHA-1. Bound both parsed arrays before the dependency allocates them.
        let entries = object.data.len() / 23;
        let retained = object
            .data
            .len()
            .saturating_add(entries.saturating_mul(std::mem::size_of::<RawTreeEntry>()))
            .saturating_add(std::mem::size_of::<Vec<RawTreeEntry>>());
        let temporary = entries.saturating_mul(128);
        let memory = self.state.runtime.reserve_bytes(retained).await?;
        let working = self.state.runtime.reserve_bytes(temporary).await?;
        let large = object.data.len() >= 64 * 1024;
        let object = object.clone();
        let parse = move || {
            let _working = working;
            Ok(Arc::new(crate::resident::Retained::new(
                parse_tree_raw(&object)?,
                memory,
            )))
        };
        if large {
            self.state.runtime.run_cpu(&self.cancellation, parse).await
        } else {
            parse()
        }
    }

    /// Read a bounded batch of verified Git objects in request order.
    pub async fn read_objects(
        &self,
        oids: &[gix_hash::ObjectId],
    ) -> Result<Vec<crate::RemoteGitObject>> {
        check_cancelled(&self.cancellation)?;
        self.budget
            .charge(BudgetDimension::LogicalObjects, oids.len() as u64)
            .await?;
        self.read_objects_uncharged(oids).await
    }

    pub(crate) async fn read_objects_uncharged(
        &self,
        oids: &[gix_hash::ObjectId],
    ) -> Result<Vec<crate::RemoteGitObject>> {
        let reader = self.state.reader.as_ref().ok_or(Error::EmptyRepository)?;
        let session = self
            .session
            .as_ref()
            .and_then(TrackedLocatorSession::session)
            .ok_or(Error::InternalInvariant {
                invariant: "non-empty operation has no locator session",
            })?;
        reader
            .read_many_with_session(
                session,
                oids,
                batch_concurrency(
                    self.state.runtime.options(),
                    self.state.options.object_limits(),
                    self.limits,
                ),
                &self.budget,
                &self.cancellation,
            )
            .instrument(self.span.clone())
            .await
    }

    pub(crate) async fn read_packed_entries(
        &self,
        oids: &[gix_hash::ObjectId],
    ) -> Result<Vec<RemoteGitPackedEntry>> {
        check_cancelled(&self.cancellation)?;
        self.budget
            .charge(BudgetDimension::LogicalObjects, oids.len() as u64)
            .await?;
        let reader = self.state.reader.as_ref().ok_or(Error::EmptyRepository)?;
        let session = self
            .session
            .as_ref()
            .and_then(TrackedLocatorSession::session)
            .ok_or(Error::InternalInvariant {
                invariant: "non-empty operation has no locator session",
            })?;
        reader
            .read_packed_many_with_session(
                session,
                oids,
                batch_concurrency(
                    self.state.runtime.options(),
                    self.state.options.object_limits(),
                    self.limits,
                ),
                &self.budget,
                &self.cancellation,
            )
            .instrument(self.span.clone())
            .await
    }

    pub(crate) async fn materialize_packed_entries(
        &self,
        entries: Vec<RemoteGitPackedEntry>,
        oids: &[gix_hash::ObjectId],
    ) -> Result<Vec<crate::RemoteGitObject>> {
        check_cancelled(&self.cancellation)?;
        let reader = self.state.reader.as_ref().ok_or(Error::EmptyRepository)?;
        let session = self
            .session
            .as_ref()
            .and_then(TrackedLocatorSession::session)
            .ok_or(Error::InternalInvariant {
                invariant: "non-empty operation has no locator session",
            })?;
        reader
            .materialize_packed_entries(
                session,
                entries,
                oids,
                batch_concurrency(
                    self.state.runtime.options(),
                    self.state.options.object_limits(),
                    self.limits,
                ),
                &self.budget,
                &self.cancellation,
            )
            .instrument(self.span.clone())
            .await
    }

    pub(crate) async fn lookup_packed_entry_locators(
        &self,
        oids: &[gix_hash::ObjectId],
    ) -> Result<Vec<GitObjectLocator>> {
        check_cancelled(&self.cancellation)?;
        let reader = self.state.reader.as_ref().ok_or(Error::EmptyRepository)?;
        let session = self
            .session
            .as_ref()
            .and_then(TrackedLocatorSession::session)
            .ok_or(Error::InternalInvariant {
                invariant: "non-empty operation has no locator session",
            })?;
        reader
            .lookup_packed_locators_with_session(session, oids, &self.budget, &self.cancellation)
            .instrument(self.span.clone())
            .await
    }

    /// Return metadata authenticated by this snapshot's exact object locators.
    ///
    /// Missing kind or size fields remain `None`; callers must fall back to
    /// bounded object reads when the publication did not prove what they need.
    pub async fn pinned_object_metadata(
        &self,
        oids: &[gix_hash::ObjectId],
    ) -> Result<Vec<crab_metadata::git_object_locator::GitObjectMetadata>> {
        Ok(self
            .lookup_packed_entry_locators(oids)
            .await?
            .into_iter()
            .map(|locator| locator.metadata)
            .collect())
    }

    pub(crate) async fn read_packed_entries_with_locators(
        &self,
        oids: &[gix_hash::ObjectId],
        locators: &[GitObjectLocator],
    ) -> Result<Vec<RemoteGitPackedEntry>> {
        check_cancelled(&self.cancellation)?;
        self.budget
            .charge(BudgetDimension::LogicalObjects, oids.len() as u64)
            .await?;
        let reader = self.state.reader.as_ref().ok_or(Error::EmptyRepository)?;
        reader
            .read_packed_many_with_session_and_locators(
                oids,
                locators,
                batch_concurrency(
                    self.state.runtime.options(),
                    self.state.options.object_limits(),
                    self.limits,
                ),
                &self.budget,
                &self.cancellation,
            )
            .instrument(self.span.clone())
            .await
    }

    pub async fn read_object_metadata(
        &self,
        oid: gix_hash::ObjectId,
    ) -> Result<RemoteGitObjectMetadata> {
        check_cancelled(&self.cancellation)?;
        self.budget
            .charge(BudgetDimension::LogicalObjects, 1)
            .await?;
        let reader = self.state.reader.as_ref().ok_or(Error::EmptyRepository)?;
        let session = self
            .session
            .as_ref()
            .and_then(TrackedLocatorSession::session)
            .ok_or(Error::InternalInvariant {
                invariant: "non-empty operation has no locator session",
            })?;
        reader
            .read_metadata_with_session(session, oid, &self.budget, &self.cancellation)
            .instrument(self.span.clone())
            .await
    }

    pub(crate) async fn read_small_metadata_object(
        &self,
        oid: gix_hash::ObjectId,
    ) -> Result<GitObject> {
        check_cancelled(&self.cancellation)?;
        self.budget
            .charge(BudgetDimension::LogicalObjects, 1)
            .await?;
        let reader = self.state.reader.as_ref().ok_or(Error::EmptyRepository)?;
        let session = self
            .session
            .as_ref()
            .and_then(TrackedLocatorSession::session)
            .ok_or(Error::InternalInvariant {
                invariant: "non-empty operation has no locator session",
            })?;
        reader
            .read_small_metadata_object_with_session(session, oid, &self.budget, &self.cancellation)
            .instrument(self.span.clone())
            .await
    }

    /// Share this operation's budget with additional object-body read owners.
    ///
    /// Use only for reads not already charged by the Git reader. The admission
    /// observes cancellation and reserves bytes before payload consumption.
    /// Complete all admitted reads before finishing this context.
    #[must_use]
    pub fn read_admission(&self) -> Arc<dyn crab_storage::ReadAdmission> {
        self.budget.read_admission(self.cancellation.clone())
    }

    /// Charge additional caller-owned work against this operation's aggregate limits.
    ///
    /// Reserve known work before starting it. Failed charges leave the operation
    /// available for finalization; callers must still observe cancellation.
    pub async fn charge(&self, dimension: BudgetDimension, amount: u64) -> Result<()> {
        self.budget.charge(dimension, amount).await
    }

    pub(crate) async fn budget_usage(&self) -> BudgetUsage {
        self.budget.usage().await
    }

    pub(crate) async fn remaining_response_bytes(&self) -> u64 {
        self.budget.remaining(BudgetDimension::ResponseBytes).await
    }

    pub(crate) async fn charge_cached(&self, usage: BudgetUsage) -> Result<()> {
        self.budget.charge_cached(usage).await
    }

    pub(crate) fn blame_key(
        &self,
        commit: gix_hash::ObjectId,
        path: &GitPath,
    ) -> crate::runtime::BlameCacheKey {
        crate::runtime::BlameCacheKey::Attribution {
            identity: self.state.identity.clone(),
            generation: self.state.generation,
            commit,
            path: path.clone(),
            semantics: crate::runtime::BLAME_SEMANTICS,
            object_limits: self.object_limits(),
        }
    }

    pub(crate) async fn cached_blame(
        &self,
        commit: gix_hash::ObjectId,
        path: &GitPath,
    ) -> Option<crate::runtime::CachedBlame> {
        self.state
            .runtime
            .cached_blame(&self.blame_key(commit, path))
            .await
    }

    pub(crate) async fn insert_blame(
        &self,
        commit: gix_hash::ObjectId,
        path: GitPath,
        value: crate::SharedBlame,
        usage: BudgetUsage,
    ) {
        self.state
            .runtime
            .insert_blame(self.blame_key(commit, &path), value, usage)
            .await;
    }

    pub(crate) async fn cached_blame_comparison(
        &self,
        parent: gix_hash::ObjectId,
        current: gix_hash::ObjectId,
    ) -> Option<crate::runtime::LineMatches> {
        self.state
            .runtime
            .cached_blame_comparison(&self.state.identity, parent, current)
            .await
    }

    pub(crate) async fn insert_blame_comparison(
        &self,
        parent: gix_hash::ObjectId,
        current: gix_hash::ObjectId,
        matches: crate::runtime::LineMatches,
    ) {
        self.state
            .runtime
            .insert_blame_comparison(self.state.identity.clone(), parent, current, matches)
            .await;
    }

    pub(crate) fn object_limits(&self) -> crate::ObjectLimits {
        self.state.options.object_limits()
    }

    pub(crate) fn ensure_active(&self) -> Result<()> {
        if self
            .deadline
            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
        {
            return Err(Error::Timeout {
                operation: "repository operation",
            });
        }
        check_cancelled(&self.cancellation)
    }

    pub(crate) fn runtime(&self) -> &crate::RemoteGitRuntime {
        &self.state.runtime
    }
}

pub(crate) fn finish_with_close<T>(
    operation: Result<T>,
    close: crab_metadata::error::Result<()>,
) -> Result<T> {
    match (operation, close) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(operation), Ok(())) => Err(operation),
        (Ok(_), Err(close)) => Err(Error::Metadata(close)),
        (Err(operation), Err(close)) => Err(Error::CloseAfterFailure {
            operation: Box::new(operation),
            close,
        }),
    }
}

impl Drop for OperationContext {
    fn drop(&mut self) {
        self.deadline_stop.cancel();
        self.cancellation.cancel();
        if !self.finished {
            self.state.runtime.metrics().record(MetricObservation {
                kind: MetricKind::Operation,
                value: 1,
                duration: Some(self.started.elapsed()),
                outcome: Some(if self.cancellation.is_cancelled() {
                    MetricOutcome::Cancelled
                } else {
                    MetricOutcome::Error
                }),
                cache: None,
            });
            record_drop(&self.span, self.correlation_id);
        }
        drop(self.session.take());
    }
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<()> {
    if cancellation.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod close_fault_tests {
    use super::*;

    #[tokio::test]
    async fn resource_free_locator_close_completes_without_scheduling_cleanup() {
        use futures_util::FutureExt;

        let runtime = Arc::new(crate::RemoteGitRuntime::default());
        let session =
            TrackedLocatorSession::new(GitObjectLocatorSession::without_catalog(), runtime.clone());
        let closed = session.close().now_or_never();
        runtime.shutdown().await;
        assert!(matches!(closed, Some(Ok(()))));
    }

    fn close_failure() -> crab_metadata::error::MetadataError {
        crab_metadata::error::MetadataError::Io {
            source: std::io::Error::other("injected locator close failure"),
        }
    }

    #[test]
    fn close_failure_replaces_an_otherwise_successful_operation() {
        assert!(matches!(
            finish_with_close(Ok(()), Err(close_failure())),
            Err(Error::Metadata(
                crab_metadata::error::MetadataError::Io { .. }
            ))
        ));
    }

    #[test]
    fn close_failure_does_not_hide_the_operation_failure() {
        let error = finish_with_close::<()>(Err(Error::Cancelled), Err(close_failure()))
            .expect_err("both failures must be retained");
        assert!(matches!(
            error,
            Error::CloseAfterFailure {
                operation,
                close: crab_metadata::error::MetadataError::Io { .. },
            } if matches!(*operation, Error::Cancelled)
        ));
    }
}

fn batch_concurrency(
    runtime: crate::RuntimeOptions,
    object: crate::ObjectLimits,
    operation: crate::OperationLimits,
) -> usize {
    [
        runtime.max_origin_concurrency,
        runtime.max_decode_concurrency,
        runtime.max_object_flights,
        bounded_usize(operation.max_logical_objects),
        bounded_usize(operation.max_storage_requests),
        byte_lanes(operation.max_fetched_bytes, object.max_packed_entry_bytes),
        byte_lanes(
            operation.max_inflated_bytes,
            object.max_inflated_entry_bytes,
        ),
    ]
    .into_iter()
    .min()
    .unwrap_or(1)
    .max(1)
}

fn byte_lanes(aggregate: u64, per_object: u64) -> usize {
    bounded_usize((aggregate / per_object).max(1))
}

fn bounded_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

fn operation_span(correlation_id: u64, operation: OperationKind) -> tracing::Span {
    tracing::info_span!(
        target: "crab_remote_git::telemetry",
        "remote_git.operation",
        correlation_id,
        operation = operation.as_str(),
        outcome = tracing::field::Empty,
        error_category = tracing::field::Empty,
    )
}

fn record_completion(
    span: &tracing::Span,
    correlation_id: u64,
    outcome: MetricOutcome,
    error: Option<&Error>,
) {
    let outcome = match outcome {
        MetricOutcome::Success => "success",
        MetricOutcome::Error => "error",
        MetricOutcome::Cancelled => "cancelled",
    };
    span.record("outcome", outcome);
    let Some(error) = error else {
        tracing::debug!(
            parent: span,
            correlation_id,
            outcome,
            "remote Git operation completed"
        );
        return;
    };
    let error_category = error.trace_category();
    span.record("error_category", error_category);
    if error_category == "integrity" {
        tracing::warn!(
            parent: span,
            correlation_id,
            outcome,
            error_category,
            "remote Git integrity validation failed"
        );
    } else {
        tracing::debug!(
            parent: span,
            correlation_id,
            outcome,
            error_category,
            "remote Git operation failed"
        );
    }
}

fn record_drop(span: &tracing::Span, correlation_id: u64) {
    span.record("outcome", "cancelled");
    span.record("error_category", "cancelled");
    tracing::debug!(
        parent: span,
        correlation_id,
        outcome = "cancelled",
        error_category = "cancelled",
        "remote Git operation dropped before explicit finish"
    );
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fmt::Write as _;
    use std::sync::{Arc, Mutex};

    use crab_metadata::git_object_locator::GitObjectCatalogIdentity;
    use crab_storage::{Store, StoreLayout};
    use crab_xet::hash::MerkleHash;
    use object_store::memory::InMemory;
    use tracing::Subscriber;
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing_subscriber::Layer;
    use tracing_subscriber::prelude::*;

    use super::*;

    #[tokio::test]
    async fn finish_normalizes_admission_cancellation_after_read_returns() {
        let store = Store::new(Arc::new(InMemory::new()));
        let runtime = Arc::new(crate::RemoteGitRuntime::default());
        let state = Arc::new(crate::state::RepositoryState {
            store: store.clone(),
            layout: StoreLayout::new(store, "cancel-finish".to_owned()),
            runtime: runtime.clone(),
            identity: crate::RepositoryIdentity::new("memory", "cancel-finish", 1).unwrap(),
            options: crate::RepositoryOptions::default(),
            generation: 1,
            pack_index_hash: Arc::from("a".repeat(64)),
            git_validation_digest: Arc::from("b".repeat(64)),
            manifest_etag: "etag".to_owned(),
            shard_index_hash: Arc::from(""),
            catalog_identity: None,
            lookup_catalog_identity: None,
            inventory: HashMap::new(),
            refs: crate::RepositoryRefs::default(),
            reader: None,
            commit_graph: None,
            path_state_hash: None,
            path_state: tokio::sync::OnceCell::new(),
            shallow_closure: None,
        });
        let cancel = CancellationToken::new();
        let operation =
            OperationContext::open_pack_transfer(state, &cancel, crate::OperationLimits::default())
                .await
                .unwrap();
        let returned: Result<()> = Err(Error::Metadata(
            crab_metadata::error::MetadataError::Storage {
                source: crab_storage::StorageError::ReadRejected {
                    source: Box::new(Error::Cancelled),
                },
            },
        ));
        cancel.cancel();
        assert!(matches!(
            operation.finish(returned).await,
            Err(Error::Cancelled)
        ));
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn pack_transfer_operation_does_not_open_the_catalog() {
        let object_store: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(object_store);
        let state = Arc::new(crate::state::RepositoryState {
            store: store.clone(),
            layout: StoreLayout::new(store, "org/repo".to_owned()),
            runtime: Arc::new(crate::RemoteGitRuntime::default()),
            identity: crate::RepositoryIdentity::new("memory", "org/repo", 1)
                .expect("repository identity"),
            options: crate::RepositoryOptions::default(),
            generation: 1,
            pack_index_hash: Arc::from("pack-index"),
            git_validation_digest: Arc::from("validation"),
            manifest_etag: "etag".to_owned(),
            shard_index_hash: Arc::from("shards"),
            catalog_identity: Some(GitObjectCatalogIdentity {
                generation: 1,
                pack_index_hash: MerkleHash::from([1; 32]),
                object_count: 1,
                catalog_digest: MerkleHash::from([2; 32]),
            }),
            lookup_catalog_identity: Some(GitObjectCatalogIdentity {
                generation: 1,
                pack_index_hash: MerkleHash::from([1; 32]),
                object_count: 1,
                catalog_digest: MerkleHash::from([2; 32]),
            }),
            inventory: HashMap::new(),
            refs: crate::RepositoryRefs::default(),
            reader: None,
            commit_graph: None,
            path_state_hash: None,
            path_state: tokio::sync::OnceCell::new(),
            shallow_closure: None,
        });
        let cancellation = CancellationToken::new();

        let regular = OperationContext::open(
            Arc::clone(&state),
            OperationKind::UploadPack,
            &cancellation,
            crate::OperationLimits::default(),
        )
        .await;
        assert!(regular.is_err());

        let concurrent_cancellation = cancellation.child_token();
        let transfer = OperationContext::open_pack_transfer(
            state,
            &concurrent_cancellation,
            crate::OperationLimits::default(),
        )
        .await
        .expect("pack transfer operation");
        concurrent_cancellation.cancel();
        assert!(transfer.cancellation().is_cancelled());
        assert!(matches!(
            transfer.finish::<()>(Err(Error::Cancelled)).await,
            Err(Error::Cancelled)
        ));
    }

    #[tokio::test]
    async fn snapshot_accelerator_falls_back_when_its_catalog_cannot_open() {
        let object_store: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(object_store);
        let runtime = Arc::new(crate::RemoteGitRuntime::default());
        let state = Arc::new(crate::state::RepositoryState {
            store: store.clone(),
            layout: StoreLayout::new(store, "org/repo".to_owned()),
            runtime: Arc::clone(&runtime),
            identity: crate::RepositoryIdentity::new("memory", "org/repo", 1)
                .expect("repository identity"),
            options: crate::RepositoryOptions::default(),
            generation: 1,
            pack_index_hash: Arc::from("pack-index"),
            git_validation_digest: Arc::from("validation"),
            manifest_etag: "etag".to_owned(),
            shard_index_hash: Arc::from("shards"),
            catalog_identity: None,
            lookup_catalog_identity: Some(GitObjectCatalogIdentity {
                generation: 1,
                pack_index_hash: MerkleHash::from([1; 32]),
                object_count: 1,
                catalog_digest: MerkleHash::from([2; 32]),
            }),
            inventory: HashMap::new(),
            refs: crate::RepositoryRefs::default(),
            reader: None,
            commit_graph: None,
            path_state_hash: None,
            path_state: tokio::sync::OnceCell::new(),
            shallow_closure: None,
        });

        let cancellation = CancellationToken::new();
        let operation = OperationContext::open(
            state,
            OperationKind::Snapshot,
            &cancellation,
            crate::OperationLimits::default(),
        )
        .await
        .expect("canonical pack-index fallback");

        operation.finish(Ok(())).await.expect("close operation");
        runtime.shutdown().await;
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<String>>);

    impl Capture {
        fn output(&self) -> String {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    impl<S> Layer<S> for Capture
    where
        S: Subscriber,
    {
        fn on_new_span(
            &self,
            attributes: &Attributes<'_>,
            _id: &Id,
            _context: tracing_subscriber::layer::Context<'_, S>,
        ) {
            attributes.record(&mut CaptureVisitor(&self.0));
        }

        fn on_record(
            &self,
            _id: &Id,
            values: &Record<'_>,
            _context: tracing_subscriber::layer::Context<'_, S>,
        ) {
            values.record(&mut CaptureVisitor(&self.0));
        }

        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _context: tracing_subscriber::layer::Context<'_, S>,
        ) {
            event.record(&mut CaptureVisitor(&self.0));
        }
    }

    struct CaptureVisitor<'a>(&'a Arc<Mutex<String>>);

    impl Visit for CaptureVisitor<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            let mut output = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _ = writeln!(output, "{}={value:?}", field.name());
        }
    }

    #[test]
    fn operation_traces_emit_only_bounded_fields_and_redacted_failures() {
        const OID: &str = "1111111111111111111111111111111111111111";
        const PATH: &str = "private/source.rs";
        const CONTENT: &str = "CRAB_PRIVATE_OBJECT_CONTENT_CANARY";
        const PREFIX: &str = "tenant/acme/repositories/private";
        const ENDPOINT: &str = "https://storage.internal.example.invalid";
        const CREDENTIAL: &str = "AKIAIOSFODNN7EXAMPLE";

        let source = format!("{OID} {PATH} {CONTENT} {PREFIX} {ENDPOINT} {CREDENTIAL}");
        let error = Error::Storage(crab_storage::StorageError::Internal(source));
        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        tracing::subscriber::with_default(subscriber, || {
            let span = operation_span(73, OperationKind::Content);
            record_completion(&span, 73, MetricOutcome::Error, Some(&error));
        });
        let output = capture.output();

        assert!(output.contains("correlation_id=73"));
        assert!(output.contains("operation=\"content\""));
        assert!(output.contains("outcome=\"error\""));
        assert!(output.contains("error_category=\"storage\""));
        for forbidden in [OID, PATH, CONTENT, PREFIX, ENDPOINT, CREDENTIAL] {
            assert!(!output.contains(forbidden));
        }
        for forbidden_field in [
            "oid=",
            "path=",
            "content=",
            "prefix=",
            "endpoint=",
            "credential=",
        ] {
            assert!(!output.contains(forbidden_field));
        }
    }

    #[test]
    fn integrity_trace_uses_safe_category_and_correlation_id() {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        tracing::subscriber::with_default(subscriber, || {
            let span = operation_span(91, OperationKind::Tree);
            let error = Error::Corrupt {
                stage: crate::CorruptionStage::Tree,
            };
            record_completion(&span, 91, MetricOutcome::Error, Some(&error));
        });
        let output = capture.output();

        assert!(output.contains("correlation_id=91"));
        assert!(output.contains("operation=\"tree\""));
        assert!(output.contains("error_category=\"integrity\""));
    }

    #[test]
    fn batch_concurrency_is_derived_from_aggregate_byte_and_object_limits() {
        let runtime = crate::RuntimeOptions {
            max_origin_concurrency: 64,
            max_decode_concurrency: 16,
            max_object_flights: 256,
            ..crate::RuntimeOptions::default()
        };
        let object = crate::ObjectLimits::default();
        let operation = crate::OperationLimits {
            max_logical_objects: 100,
            max_storage_requests: 100,
            max_fetched_bytes: object.max_packed_entry_bytes * 2,
            max_inflated_bytes: object.max_inflated_entry_bytes * 4,
            ..crate::OperationLimits::default()
        };

        assert_eq!(batch_concurrency(runtime, object, operation), 2);
    }

    #[test]
    fn batch_concurrency_keeps_one_lane_for_a_smaller_aggregate_limit() {
        let object = crate::ObjectLimits::default();
        let operation = crate::OperationLimits {
            max_fetched_bytes: 1,
            max_inflated_bytes: 1,
            ..crate::OperationLimits::default()
        };

        assert_eq!(
            batch_concurrency(crate::RuntimeOptions::default(), object, operation),
            1
        );
    }
}
