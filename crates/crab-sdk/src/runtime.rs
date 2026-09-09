use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::{Error, ErrorKind, OperationId, OperationOptions, Result};

#[derive(Clone, Copy)]
enum Completion {
    Read,
    #[cfg(any(feature = "write", feature = "managed"))]
    Mutation,
}

pub(crate) struct Operations {
    closed: Mutex<bool>,
    cancellation: CancellationToken,
    tasks: TaskTracker,
    unobserved: Arc<UnobservedFailure>,
}

impl Default for Operations {
    fn default() -> Self {
        Self {
            closed: Mutex::new(false),
            cancellation: CancellationToken::new(),
            tasks: TaskTracker::new(),
            unobserved: Arc::default(),
        }
    }
}

impl Operations {
    #[cfg(any(feature = "write", feature = "managed"))]
    pub(crate) async fn run_mutation<T, F, Fut>(
        &self,
        options: OperationOptions,
        operation: F,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        let options = options.with_owner_deadline()?;
        self.start_with_completion(options, operation, Completion::Mutation)?
            .wait()
            .await
    }

    pub(crate) async fn run<T, F, Fut>(&self, options: OperationOptions, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        self.start(options, operation)?.wait().await
    }

    pub(crate) fn start<T, F, Fut>(
        &self,
        options: OperationOptions,
        operation: F,
    ) -> Result<OperationTask<T>>
    where
        T: Send + 'static,
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        self.start_with_completion(options, operation, Completion::Read)
    }

    fn start_with_completion<T, F, Fut>(
        &self,
        options: OperationOptions,
        operation: F,
        completion: Completion,
    ) -> Result<OperationTask<T>>
    where
        T: Send + 'static,
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        let id = OperationId::allocate()?;
        let runtime = tokio::runtime::Handle::try_current().map_err(|source| {
            Error::with_source(
                ErrorKind::InvalidInput,
                "a Tokio runtime is required",
                source,
            )
            .with_operation(id)
        })?;
        let cancellation = self.cancellation.child_token();
        let caller_cancellation = options.cancellation.token().clone();
        let (send, receive) = oneshot::channel();
        let progress = options.progress.clone();
        let worker_cancellation = cancellation.clone();
        let unobserved = self.unobserved.clone();
        {
            let closed = self.closed.lock().map_err(|_| {
                Error::new(ErrorKind::Io, "operation admission is unavailable").with_operation(id)
            })?;
            if *closed {
                return Err(Error::new(ErrorKind::Cancelled, "client is closed").with_operation(id));
            }
            // Registration and admission closure share this lock. Close cannot
            // observe an empty tracker while an admitted worker is unregistered.
            self.tasks.spawn_on(
                async move {
                    let result = execute(id, options, worker_cancellation, operation, completion)
                        .await
                        .map_err(|error| error.with_operation(id));
                    if let Err(result) = send.send(result) {
                        unobserved.record(result);
                    }
                },
                &runtime,
            );
        }
        Ok(OperationTask {
            id,
            progress,
            receive,
            cancellation,
            caller_cancellation,
            unobserved: self.unobserved.clone(),
        })
    }

    pub(crate) async fn drain(&self) {
        {
            let mut closed = self
                .closed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *closed = true;
            self.cancellation.cancel();
            self.tasks.close();
        }
        self.tasks.wait().await;
    }

    pub(crate) fn take_failure(&self) -> Result<()> {
        self.unobserved.take().map_or(Ok(()), Err)
    }
}

async fn execute<T, F, Fut>(
    id: OperationId,
    options: OperationOptions,
    cancellation: CancellationToken,
    operation: F,
    completion: Completion,
) -> Result<T>
where
    F: FnOnce(CancellationToken) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let caller = options.cancellation.token();
    if caller.is_cancelled() || cancellation.is_cancelled() {
        return Err(Error::new(
            ErrorKind::Cancelled,
            "operation cancelled before starting",
        ));
    }
    if options
        .deadline
        .is_some_and(|deadline| deadline <= std::time::Instant::now())
    {
        return Err(Error::new(
            ErrorKind::Timeout,
            "operation deadline expired before starting",
        ));
    }
    if let Some(progress) = &options.progress {
        progress.emit(id, crate::ProgressUpdate::Started);
    }
    let deadline = async {
        match options.deadline {
            Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
            None => std::future::pending().await,
        }
    };
    let worker = operation(cancellation.clone());
    tokio::pin!(worker);
    let expired = tokio::select! {
        biased;
        () = caller.cancelled() => false,
        () = cancellation.cancelled() => false,
        () = deadline => true,
        result = &mut worker => return result,
    };
    if let Some(progress) = &options.progress {
        progress.emit(id, crate::ProgressUpdate::Cancelling);
    }
    cancellation.cancel();
    // Cancellation requests cleanup; it never drops the worker future that owns
    // sessions. Terminal results are returned only after that cleanup completes.
    let result = worker.await;
    let kind = if expired {
        ErrorKind::Timeout
    } else {
        ErrorKind::Cancelled
    };
    match result {
        Ok(value) if !expired => Ok(value),
        Ok(value)
            if match completion {
                Completion::Read => false,
                #[cfg(any(feature = "write", feature = "managed"))]
                Completion::Mutation => true,
            } =>
        {
            // Only mutation workers enter this mode: their result records durable
            // evidence, which a later deadline cannot turn into a failed write.
            Ok(value)
        }
        Ok(_) => Err(Error::new(kind, "operation interrupted")),
        Err(error) if expired && error.kind() == ErrorKind::Cancelled => {
            Err(error.with_deadline_kind())
        }
        Err(error) => Err(error),
    }
}

#[derive(Default)]
struct UnobservedFailure(Mutex<Option<Error>>);

impl UnobservedFailure {
    fn record<T>(&self, result: Result<T>) {
        let Err(error) = result else { return };
        if error.kind() == ErrorKind::Cancelled && error.cleanup_error().is_none() {
            return;
        }
        // Retain one diagnostic regardless of abandoned operation count. Normal
        // drop cancellation is expected; a cleanup failure must remain visible.
        let mut first = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if first.is_none() {
            *first = Some(error);
        }
    }

    fn take(&self) -> Option<Error> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

pub(crate) struct OperationTask<T> {
    progress: Option<crate::Progress>,
    id: OperationId,
    receive: oneshot::Receiver<Result<T>>,
    unobserved: Arc<UnobservedFailure>,
    cancellation: CancellationToken,
    caller_cancellation: CancellationToken,
}

impl<T> OperationTask<T> {
    pub(crate) fn progress(&self) -> Option<crate::Progress> {
        self.progress.clone()
    }

    pub(crate) fn id(&self) -> OperationId {
        self.id
    }

    pub(crate) fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub(crate) fn caller_is_cancelled(&self) -> bool {
        self.caller_cancellation.is_cancelled()
    }

    pub(crate) async fn wait(&mut self) -> Result<T> {
        (&mut self.receive).await.map_err(|source| {
            Error::with_source(ErrorKind::Io, "operation ended without a result", source)
                .with_operation(self.id)
        })?
    }
}

impl<T> Drop for OperationTask<T> {
    fn drop(&mut self) {
        self.cancellation.cancel();
        // Closing first makes every racing send either fail back to the worker
        // or remain available here; a completed but unread result cannot vanish.
        self.receive.close();
        if let Ok(result) = self.receive.try_recv() {
            self.unobserved.record(result);
        }
    }
}

impl Drop for Operations {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use tokio::sync::Notify;

    use super::*;

    #[cfg(feature = "write")]
    #[tokio::test]
    async fn mutation_deadline_retains_commit_evidence_after_cleanup() {
        let operations = Operations::default();
        let token = crate::RecoveryToken::new(
            [1; 32],
            &crate::RepositoryLocator::new("repository").unwrap(),
            crate::RefBatch::new(vec![
                crate::RefUpdate::create(
                    "refs/tags/v1",
                    crate::ObjectId::from_hex(&"a".repeat(40)).unwrap(),
                )
                .unwrap(),
            ])
            .unwrap(),
            None,
            None,
            None,
        )
        .unwrap();
        let options = OperationOptions::default()
            .with_timeout(std::time::Duration::from_millis(10))
            .unwrap();
        let result = operations
            .run_mutation(options, |cancel| async move {
                cancel.cancelled().await;
                Ok(crate::MutationOutcome::Committed {
                    receipt: crate::CommitReceipt {
                        recovery: token,
                        transaction_id: "b".repeat(64),
                    },
                    readiness: crate::Readiness::Pending,
                })
            })
            .await
            .unwrap();
        assert!(matches!(
            result,
            crate::MutationOutcome::Committed {
                readiness: crate::Readiness::Pending,
                ..
            }
        ));
        close(&operations).await.unwrap();
    }

    async fn close(operations: &Operations) -> Result<()> {
        operations.drain().await;
        operations.take_failure()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_request_cancels_worker_and_close_waits_for_cleanup() {
        let operations = Operations::default();
        let started = Arc::new(Notify::new());
        let cancelled = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let worker_started = started.clone();
        let worker_cancelled = cancelled.clone();
        let worker_release = release.clone();
        let mut request = Box::pin(operations.run(
            OperationOptions::default(),
            move |cancel| async move {
                worker_started.notify_one();
                cancel.cancelled().await;
                worker_cancelled.notify_one();
                worker_release.notified().await;
                Ok(())
            },
        ));
        tokio::select! {
            result = &mut request => panic!("worker unexpectedly returned: {result:?}"),
            () = started.notified() => {}
        }
        drop(request);
        tokio::time::timeout(std::time::Duration::from_secs(2), cancelled.notified())
            .await
            .unwrap();
        let mut close = Box::pin(close(&operations));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut close)
                .await
                .is_err()
        );
        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(2), close)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            operations
                .run(OperationOptions::default(), |_| async { Ok(()) })
                .await
                .unwrap_err()
                .kind(),
            ErrorKind::Cancelled
        );
    }

    #[tokio::test]
    async fn close_reports_cleanup_failure_on_either_side_of_result_delivery() {
        for drop_before_send in [true, false] {
            let operations = Operations::default();
            let release = Arc::new(Notify::new());
            let worker_release = release.clone();
            let started = Arc::new(Notify::new());
            let worker_started = started.clone();
            let task = operations
                .start(OperationOptions::default(), move |_| async move {
                    worker_started.notify_one();
                    worker_release.notified().await;
                    Err::<(), _>(Error::new(ErrorKind::Cancelled, "cancelled").with_cleanup(
                        Error::with_source(
                            ErrorKind::Io,
                            "cleanup failed",
                            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
                        ),
                    ))
                })
                .unwrap();
            started.notified().await;
            if drop_before_send {
                drop(task);
                release.notify_one();
            } else {
                release.notify_one();
                operations.tasks.close();
                operations.tasks.wait().await;
                drop(task);
            }
            let error = close(&operations).await.unwrap_err();
            let cleanup = error.cleanup_error().unwrap();
            let source = std::error::Error::source(cleanup)
                .unwrap()
                .downcast_ref::<std::io::Error>()
                .unwrap();
            assert_eq!(
                (error.kind(), cleanup.kind(), source.kind()),
                (
                    ErrorKind::Cancelled,
                    ErrorKind::Io,
                    std::io::ErrorKind::PermissionDenied
                ),
            );
            close(&operations).await.unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn caller_cancellation_and_deadline_wait_for_worker_cleanup() {
        for deadline in [false, true] {
            let operations = Operations::default();
            let caller = crate::Cancellation::default();
            let (progress, mut receiver) = crate::Progress::channel();
            let mut options = OperationOptions::default()
                .with_cancellation(caller.clone())
                .with_progress(progress);
            if deadline {
                options = options
                    .with_deadline(std::time::Instant::now() + std::time::Duration::from_secs(1));
            }
            let started = Arc::new(Notify::new());
            let cancelled = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let worker_started = started.clone();
            let worker_cancelled = cancelled.clone();
            let worker_release = release.clone();
            let mut task = operations
                .start(options, move |cancel| async move {
                    worker_started.notify_one();
                    cancel.cancelled().await;
                    worker_cancelled.notify_one();
                    worker_release.notified().await;
                    Err::<(), _>(
                        Error::new(ErrorKind::Cancelled, "cancelled")
                            .with_cleanup(Error::new(ErrorKind::Io, "cleanup failed")),
                    )
                })
                .unwrap();
            started.notified().await;
            assert_eq!(
                receiver.next().await,
                Some(crate::ProgressEvent {
                    operation_id: task.id(),
                    update: crate::ProgressUpdate::Started
                })
            );
            if !deadline {
                caller.cancel();
            }
            tokio::time::timeout(std::time::Duration::from_secs(3), cancelled.notified())
                .await
                .unwrap();
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(10), task.wait())
                    .await
                    .is_err()
            );
            assert_eq!(
                receiver.next().await,
                Some(crate::ProgressEvent {
                    operation_id: task.id(),
                    update: crate::ProgressUpdate::Cancelling
                })
            );
            release.notify_one();
            let error = task.wait().await.unwrap_err();
            assert_eq!(
                (error.kind(), error.cleanup_error().unwrap().kind()),
                (
                    if deadline {
                        ErrorKind::Timeout
                    } else {
                        ErrorKind::Cancelled
                    },
                    ErrorKind::Io
                ),
            );
            close(&operations).await.unwrap();
        }
    }

    #[tokio::test]
    async fn dropping_one_operation_does_not_cancel_shared_caller_scope() {
        let operations = Operations::default();
        let caller = crate::Cancellation::default();
        let options = OperationOptions::default().with_cancellation(caller.clone());
        let task = operations
            .start(options.clone(), |cancel| async move {
                cancel.cancelled().await;
                Err::<(), _>(Error::new(ErrorKind::Cancelled, "cancelled"))
            })
            .unwrap();
        drop(task);
        let result = operations.run(options, |_| async { Ok(42) }).await.unwrap();
        assert_eq!((caller.is_cancelled(), result), (false, 42));
        close(&operations).await.unwrap();
    }

    #[tokio::test]
    async fn operation_identity_survives_unobserved_cleanup_and_is_unique() {
        let operations = Operations::default();
        let first = operations
            .start(OperationOptions::default(), |_| async {
                Err::<(), _>(
                    Error::new(ErrorKind::Io, "failed")
                        .with_cleanup(Error::new(ErrorKind::Transport, "cleanup failed")),
                )
            })
            .unwrap();
        let id = first.id();
        operations.tasks.close();
        operations.tasks.wait().await;
        drop(first);
        let error = close(&operations).await.unwrap_err();
        assert_eq!(
            (
                error.operation_id(),
                error.cleanup_error().unwrap().operation_id()
            ),
            (Some(id), Some(id))
        );
        let rejected = operations
            .start(OperationOptions::default(), |_| async { Ok(()) })
            .err()
            .unwrap();
        assert!(rejected.operation_id().is_some_and(|next| next != id));
    }

    #[tokio::test]
    async fn ordinary_abandoned_cancellation_is_not_a_close_failure() {
        let operations = Operations::default();
        let task = operations
            .start(OperationOptions::default(), |cancel| async move {
                cancel.cancelled().await;
                Err::<(), _>(Error::new(ErrorKind::Cancelled, "cancelled"))
            })
            .unwrap();
        drop(task);
        close(&operations).await.unwrap();
    }

    #[tokio::test]
    async fn close_retains_only_first_unobserved_failure() {
        let operations = Operations::default();
        for kind in [ErrorKind::Corruption, ErrorKind::Transport] {
            let task = operations
                .start(OperationOptions::default(), move |_| async move {
                    Err::<(), _>(Error::new(kind, "worker failed"))
                })
                .unwrap();
            operations.tasks.close();
            operations.tasks.wait().await;
            drop(task);
        }
        assert_eq!(
            close(&operations).await.unwrap_err().kind(),
            ErrorKind::Corruption
        );
    }

    #[tokio::test]
    async fn worker_result_preserves_typed_error() {
        let operations = Operations::default();
        let (progress, receiver) = crate::Progress::channel();
        drop(receiver);
        let result: Result<()> = operations
            .run(
                OperationOptions::default().with_progress(progress),
                |_| async { Err(Error::new(ErrorKind::Conflict, "expected conflict")) },
            )
            .await;
        close(&operations).await.unwrap();
        assert_eq!(result.unwrap_err().kind(), ErrorKind::Conflict);
    }
}
