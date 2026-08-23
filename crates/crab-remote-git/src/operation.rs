use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crab_metadata::git_object_locator::GitObjectLocatorSession;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tokio_util::task::task_tracker::TaskTrackerToken;
use tracing::Instrument as _;

use crate::budget::{BudgetUsage, OperationBudget};
use crate::objects::{materialize_tree, parse_commit, parse_tag, parse_tree_raw};
use crate::reader::{GitObject, RemoteGitObjectMetadata};
use crate::state::RepositoryState;
use crate::{
    AnnotatedTag, Blame, BudgetDimension, Commit, Error, GitPath, MetricKind, MetricObservation,
    MetricOutcome, Result, TreeEntry,
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

impl OperationKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Repository => "repository",
            Self::Resolve => "resolve",
            Self::Snapshot => "snapshot",
            Self::Commit => "commit",
            Self::Tree => "tree",
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

    pub(crate) fn coverage(&self) -> Option<crab_metadata::git_object_locator::GitLocatorCoverage> {
        self.session
            .as_ref()
            .and_then(GitObjectLocatorSession::coverage)
    }

    fn session(&self) -> Option<&GitObjectLocatorSession> {
        self.session.as_ref()
    }

    pub(crate) async fn close(mut self) -> crab_metadata::error::Result<()> {
        let Some(session) = self.session.take() else {
            return Ok(());
        };
        let (sender, receiver) = oneshot::channel();
        self.runtime.track_cleanup(async move {
            let _ = sender.send(session.close().await);
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
    cancellation: CancellationToken,
    deadline: tokio::time::Instant,
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

static NEXT_CORRELATION_ID: AtomicU64 = AtomicU64::new(1);

impl OperationContext {
    pub(crate) async fn open(
        state: Arc<RepositoryState>,
        kind: OperationKind,
        cancellation: &CancellationToken,
    ) -> Result<Self> {
        let task_token = state.runtime.operation_token();
        let runtime_cancellation = state.runtime.background_cancellation();
        let started = Instant::now();
        let max_duration = state.options.operation_limits().max_duration;
        let deadline = tokio::time::Instant::now()
            .checked_add(max_duration)
            .ok_or(Error::InvalidLimit {
                name: "operation duration",
            })?;
        let correlation_id = NEXT_CORRELATION_ID.fetch_add(1, Ordering::Relaxed);
        let span = operation_span(correlation_id, kind);
        check_cancelled(cancellation)?;
        check_cancelled(&runtime_cancellation)?;
        let session = if let Some(required) = state.coverage {
            let session = tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(Error::Cancelled),
                () = runtime_cancellation.cancelled() => return Err(Error::Cancelled),
                session = tokio::time::timeout_at(
                    deadline,
                    GitObjectLocatorSession::open_for_operation(
                        Arc::clone(state.store.inner()),
                        state.layout.repo_prefix(),
                        max_duration,
                    ),
                ) => session.map_err(|_| Error::Timeout {
                    operation: "open locator",
                })??,
            };
            let session = TrackedLocatorSession::new(session, Arc::clone(&state.runtime));
            if session.coverage() != Some(required) {
                let observed = session.coverage().map(|coverage| coverage.generation);
                session.close().await?;
                return Err(Error::RepositoryIndexing {
                    observed,
                    required: required.generation,
                });
            }
            Some(session)
        } else {
            None
        };
        if cancellation.is_cancelled() || runtime_cancellation.is_cancelled() {
            if let Some(session) = session {
                session.close().await?;
            }
            return Err(Error::Cancelled);
        }
        if tokio::time::Instant::now() >= deadline {
            if let Some(session) = session {
                session.close().await?;
            }
            return Err(Error::Timeout {
                operation: "open locator",
            });
        }
        let operation_cancellation = cancellation.child_token();
        let deadline_stop = CancellationToken::new();
        let deadline_cancellation = operation_cancellation.clone();
        let deadline_finished = deadline_stop.clone();
        let shutdown_cancellation = runtime_cancellation;
        state.runtime.track_cleanup(async move {
            tokio::select! {
                biased;
                () = deadline_finished.cancelled() => {}
                () = shutdown_cancellation.cancelled() => deadline_cancellation.cancel(),
                () = tokio::time::sleep_until(deadline) => deadline_cancellation.cancel(),
            }
        });
        Ok(Self {
            budget: OperationBudget::new(
                state.options.operation_limits(),
                Arc::clone(&state.runtime),
                correlation_id,
            ),
            state,
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
    /// protected diagnostics.
    pub async fn finish<T>(mut self, operation: Result<T>) -> Result<T> {
        let timed_out = tokio::time::Instant::now() >= self.deadline;
        self.deadline_stop.cancel();
        let operation = match operation {
            Ok(_) if timed_out => Err(Error::Timeout {
                operation: "repository operation",
            }),
            Err(Error::Cancelled) if timed_out => Err(Error::Timeout {
                operation: "repository operation",
            }),
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
        self.state.options.operation_limits().max_logical_objects
    }

    /// Return the maximum complete pack response size for this operation.
    #[must_use]
    pub fn max_response_bytes(&self) -> u64 {
        self.state.options.operation_limits().max_response_bytes
    }

    /// Read one verified Git object from the pinned repository generation.
    pub async fn read_object(&self, oid: gix_hash::ObjectId) -> Result<crate::RemoteGitObject> {
        check_cancelled(&self.cancellation)?;
        self.budget
            .charge(BudgetDimension::LogicalObjects, 1)
            .await?;
        self.read_object_uncharged(oid).await
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
        reader
            .read_with_session(session, oid, &self.budget, &self.cancellation)
            .instrument(self.span.clone())
            .await
    }

    pub(crate) async fn read_commit(&self, oid: gix_hash::ObjectId) -> Result<Commit> {
        self.budget
            .charge(BudgetDimension::LogicalObjects, 1)
            .await?;
        let key =
            crate::runtime::ObjectCacheKey::new(&self.state.identity, self.state.generation, oid);
        let maximum = self.state.options.object_limits().max_object_bytes;
        if let Some(commit) = self.state.runtime.cached_commit(&key, maximum).await {
            return Ok(commit.as_ref().clone());
        }
        let object = self.read_object_uncharged(oid).await?;
        let source_bytes = object.data.len() as u64;
        let commit = Arc::new(parse_commit(&object)?);
        self.state
            .runtime
            .insert_commit(key, Arc::clone(&commit), source_bytes)
            .await;
        Ok(commit.as_ref().clone())
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
        let key =
            crate::runtime::ObjectCacheKey::new(&self.state.identity, self.state.generation, oid);
        let maximum = self.state.options.object_limits().max_object_bytes;
        let tree = match self.state.runtime.cached_tree(&key, maximum).await {
            Some(tree) => tree,
            None => {
                let object = self.read_object_uncharged(oid).await?;
                let source_bytes = object.data.len() as u64;
                let tree = Arc::new(parse_tree_raw(&object)?);
                self.state
                    .runtime
                    .insert_tree(key, Arc::clone(&tree), source_bytes)
                    .await;
                tree
            }
        };
        materialize_tree(&tree, parent)
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
                    self.state.options.operation_limits(),
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

    pub(crate) async fn charge(&self, dimension: BudgetDimension, amount: u64) -> Result<()> {
        self.budget.charge(dimension, amount).await
    }

    pub(crate) async fn budget_usage(&self) -> BudgetUsage {
        self.budget.usage().await
    }

    pub(crate) async fn charge_cached(&self, usage: BudgetUsage) -> Result<()> {
        self.budget.charge_cached(usage).await
    }

    pub(crate) async fn cached_blame(
        &self,
        commit: gix_hash::ObjectId,
        path: &GitPath,
    ) -> Option<crate::runtime::CachedBlame> {
        self.state
            .runtime
            .cached_blame(&self.state.identity, self.state.generation, commit, path)
            .await
    }

    pub(crate) async fn insert_blame(
        &self,
        commit: gix_hash::ObjectId,
        path: GitPath,
        value: Arc<Blame>,
        usage: BudgetUsage,
    ) {
        self.state
            .runtime
            .insert_blame(
                self.state.identity.clone(),
                self.state.generation,
                commit,
                path,
                value,
                usage,
            )
            .await;
    }

    pub(crate) fn object_limits(&self) -> crate::ObjectLimits {
        self.state.options.object_limits()
    }

    pub(crate) fn ensure_active(&self) -> Result<()> {
        if tokio::time::Instant::now() >= self.deadline {
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
    use std::fmt::Write as _;
    use std::sync::{Arc, Mutex};

    use tracing::Subscriber;
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing_subscriber::Layer;
    use tracing_subscriber::prelude::*;

    use super::*;

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
