#[cfg(all(feature = "files", feature = "service"))]
use crab_coordination::read_retention_runtime::RetainedRead;

use crate::runtime::OperationTask;
use crate::{Error, ErrorKind, Result};
use tokio::sync::mpsc;

pub(crate) enum End {
    Complete,
    Cancelled,
}

pub(crate) struct StreamReceiver<T> {
    progress: Option<crate::Progress>,
    delivered_bytes: u64,
    delivered_items: u64,
    pub(crate) id: crate::OperationId,
    entries: mpsc::Receiver<T>,
    pending: Option<T>,
    task: Option<OperationTask<End>>,
    read_authority: Option<crab_storage::Store>,
    #[cfg(all(feature = "files", feature = "service"))]
    retention: Option<RetainedRead>,
}

impl<T> StreamReceiver<T> {
    pub(crate) fn new(
        entries: mpsc::Receiver<T>,
        task: OperationTask<End>,
        read_authority: Option<crab_storage::Store>,
        #[cfg(all(feature = "files", feature = "service"))] retention: Option<RetainedRead>,
    ) -> Self {
        Self {
            progress: task.progress(),
            delivered_bytes: 0,
            delivered_items: 0,
            id: task.id(),
            entries,
            pending: None,
            task: Some(task),
            read_authority,
            #[cfg(all(feature = "files", feature = "service"))]
            retention,
        }
    }

    pub(crate) fn delivered(&mut self, bytes: usize) {
        if let Some(progress) = &self.progress {
            self.delivered_bytes = self.delivered_bytes.saturating_add(bytes as u64);
            self.delivered_items = self.delivered_items.saturating_add(1);
            progress.emit(
                self.id,
                crate::ProgressUpdate::Delivered {
                    bytes: self.delivered_bytes,
                    items: self.delivered_items,
                },
            );
        }
    }

    /// Receive the next entry; EOF is returned only after operation finalization.
    pub async fn next(&mut self) -> Result<Option<T>> {
        let Some(task) = self.task.as_ref() else {
            return Ok(None);
        };
        if task.caller_is_cancelled() {
            return self.finish_cancellation().await;
        }
        let receive = async {
            if self.pending.is_none() {
                self.pending = self.entries.recv().await;
            }
            if let (Some(_), Some(store)) = (&self.pending, &self.read_authority) {
                // Keep the frame if the caller drops this wait during refresh.
                tokio::select! {
                    biased;
                    error = task.delivery_interruption() => return Err(error),
                    result = store.check_read_authority() => {
                        result.map_err(|source| crate::remote_error::read_error(source.into()))?;
                    }
                }
            }
            Ok(())
        };
        #[cfg(all(feature = "files", feature = "service"))]
        let result = match &self.retention {
            Some(retention) => receive_retained(receive, retention).await,
            None => receive.await,
        };
        #[cfg(not(all(feature = "files", feature = "service")))]
        let result = receive.await;
        if let Err(error) = result {
            let error = error.with_operation(self.id);
            return Err(match self.close().await {
                Ok(()) => error,
                Err(cleanup) => error.with_cleanup(cleanup),
            });
        }
        if self
            .task
            .as_ref()
            .is_some_and(OperationTask::caller_is_cancelled)
        {
            return self.finish_cancellation().await;
        }
        if let Some(entry) = self.pending.take() {
            return Ok(Some(entry));
        }
        let Some(task) = self.task.as_mut() else {
            return Ok(None);
        };
        let result = task.wait().await;
        let cancelled = task.caller_is_cancelled();
        self.clear();
        if cancelled {
            result?;
            return Err(self.cancellation_error());
        }
        match result? {
            End::Complete => Ok(None),
            End::Cancelled => Err(self.cancellation_error()),
        }
    }

    async fn finish_cancellation(&mut self) -> Result<Option<T>> {
        self.close().await?;
        Err(self.cancellation_error())
    }

    fn cancellation_error(&self) -> Error {
        Error::new(ErrorKind::Cancelled, "stream was cancelled").with_operation(self.id)
    }

    fn clear(&mut self) {
        self.task = None;
        self.pending = None;
        self.read_authority = None;
        #[cfg(all(feature = "files", feature = "service"))]
        {
            self.retention = None;
        }
    }

    /// Stop traversal and wait for session close without verifying unread entries.
    pub async fn close(&mut self) -> Result<()> {
        self.entries.close();
        let result = if let Some(task) = self.task.as_mut() {
            task.cancel();
            task.wait().await.map(|_| ())
        } else {
            Ok(())
        };
        self.clear();
        while self.entries.try_recv().is_ok() {}
        result
    }
}

#[cfg(all(feature = "files", feature = "service"))]
async fn receive_retained(
    receive: impl Future<Output = Result<()>>,
    retention: &RetainedRead,
) -> Result<()> {
    let retention_error = |source: crab_coordination::CoordinationError| {
        crate::remote_error::read_error(source.into())
    };
    let pin = retention.pin().map_err(retention_error)?;
    let result = tokio::select! {
        biased;
        () = pin.cancellation().cancelled() => Ok(()),
        result = receive => result,
    };
    // A queued frame may have been verified before the consumer stalled.
    // Check the claim at delivery and drop the pin before draining on failure.
    pin.ensure_valid().map_err(retention_error)?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cancellation, OperationOptions};

    #[tokio::test]
    async fn caller_cancellation_prevents_completed_stream_eof() {
        let operations = crate::runtime::Operations::default();
        let caller = Cancellation::default();
        let worker_caller = caller.clone();
        let (send, entries) = mpsc::channel(1);
        let task = operations
            .start(
                OperationOptions::default().with_cancellation(caller),
                move |_| async move {
                    worker_caller.cancel();
                    drop(send);
                    Ok(End::Complete)
                },
            )
            .unwrap();
        let id = task.id();
        let mut stream = StreamReceiver::<()>::new(
            entries,
            task,
            None,
            #[cfg(all(feature = "files", feature = "service"))]
            None,
        );

        let error = stream.next().await.unwrap_err();
        assert_eq!(
            (error.kind(), error.operation_id()),
            (ErrorKind::Cancelled, Some(id))
        );
        stream.close().await.unwrap();
        operations.drain().await;
        operations.take_failure().unwrap();
    }

    #[cfg(all(feature = "files", feature = "service"))]
    #[tokio::test(start_paused = true)]
    async fn failed_renewal_discards_queued_projected_data_and_drains_producer() {
        use std::sync::Arc;

        use crab_coordination::{
            CoordinationError,
            read_retention::{
                READ_RETENTION_TTL, ReadRetentionLease, RetainedReadRoot, retained_read_root,
            },
            read_retention_runtime::ReadRetentionSession,
        };
        use object_store::{ObjectStoreExt as _, path::Path};
        use tokio::sync::oneshot;

        let session = ReadRetentionSession::default();
        let store: Arc<dyn object_store::ObjectStore> =
            Arc::new(object_store::memory::InMemory::new());
        let path = Path::from("repo/v2/readers/fixture/claim");
        let root = RetainedReadRoot::new("a".repeat(64), 1).unwrap();
        let lease = ReadRetentionLease::create(store.clone(), path.clone(), root)
            .await
            .unwrap();
        let retention = session.capture().unwrap().retain(lease).unwrap();
        let pin = retention.pin().unwrap();

        let (send, entries) = mpsc::channel(1);
        send.send("queued while authorized").await.unwrap();
        let operations = crate::runtime::Operations::default();
        let (finished, mut drained) = oneshot::channel();
        let task = operations
            .start(OperationOptions::default(), move |cancel| async move {
                cancel.cancelled().await;
                drop(send);
                finished.send(()).unwrap();
                Ok(End::Cancelled)
            })
            .unwrap();
        let id = task.id();
        let mut stream = StreamReceiver::new(entries, task, None, Some(retention));

        // Retire the native claim and let renewal race its last acknowledged version.
        let expired = store.head(&path).await.unwrap().last_modified.timestamp()
            + READ_RETENTION_TTL.as_secs() as i64;
        retained_read_root(&store, &path, expired).await.unwrap();
        pin.cancellation().cancelled().await;
        drop(pin);

        let error = stream.next().await.unwrap_err();
        assert_eq!(error.operation_id(), Some(id));
        assert!(matches!(
            std::error::Error::source(&error)
                .and_then(std::error::Error::source)
                .and_then(|source| source.downcast_ref::<CoordinationError>()),
            Some(CoordinationError::ReadRetentionFailed(_))
        ));
        assert_eq!(drained.try_recv(), Ok(()));
        assert_eq!(stream.next().await.unwrap(), None);
        assert!(stream.entries.is_empty());
        assert!(session.shutdown().await.is_err());
    }

    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use tokio::sync::Notify;

    struct DeliveryGrant {
        allowed: AtomicBool,
        pending: AtomicBool,
        checking: Notify,
    }

    #[async_trait::async_trait]
    impl crab_storage::ReadAuthorization for DeliveryGrant {
        async fn check(&self) -> crab_storage::Result<()> {
            if self.pending.load(Ordering::Relaxed) {
                self.checking.notify_one();
                std::future::pending::<()>().await;
            }
            if !self.allowed.load(Ordering::Relaxed) {
                return Err(crab_storage::StorageError::Forbidden {
                    path: "fixture".into(),
                });
            }
            Ok(())
        }
    }

    async fn buffered_read(
        operations: &crate::runtime::Operations,
        grant: Arc<DeliveryGrant>,
        options: OperationOptions,
    ) -> StreamReceiver<&'static str> {
        let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()))
            .with_target_identity([1; 32])
            .with_read_authorization(grant, "repo")
            .unwrap();
        let worker_store = store.clone();
        let (send, entries) = mpsc::channel(1);
        let (queued, ready) = tokio::sync::oneshot::channel();
        let task = operations
            .start(options, move |cancel| async move {
                worker_store
                    .check_read_authority()
                    .await
                    .map_err(|source| crate::remote_error::read_error(source.into()))?;
                send.send("queued while authorized").await.unwrap();
                queued.send(()).unwrap();
                cancel.cancelled().await;
                Ok(End::Cancelled)
            })
            .unwrap();
        ready.await.unwrap();
        StreamReceiver::new(
            entries,
            task,
            Some(store),
            #[cfg(all(feature = "files", feature = "service"))]
            None,
        )
    }

    impl Default for DeliveryGrant {
        fn default() -> Self {
            Self {
                allowed: AtomicBool::new(true),
                pending: AtomicBool::new(false),
                checking: Notify::new(),
            }
        }
    }

    #[tokio::test]
    async fn rejected_grant_discards_buffered_data() {
        let operations = crate::runtime::Operations::default();
        let grant = Arc::new(DeliveryGrant::default());
        let mut stream =
            buffered_read(&operations, grant.clone(), OperationOptions::default()).await;
        grant.allowed.store(false, Ordering::Relaxed);

        assert_eq!(
            stream.next().await.unwrap_err().kind(),
            ErrorKind::Authorization
        );
        assert_eq!(stream.next().await.unwrap(), None);
        assert!(stream.pending.is_none());
        assert!(stream.entries.is_empty());
        operations.drain().await;
        operations.take_failure().unwrap();
    }

    #[tokio::test]
    async fn dropping_a_pending_grant_check_preserves_the_frame() {
        let operations = crate::runtime::Operations::default();
        let grant = Arc::new(DeliveryGrant::default());
        let mut stream =
            buffered_read(&operations, grant.clone(), OperationOptions::default()).await;
        grant.pending.store(true, Ordering::Relaxed);
        let mut next = Box::pin(stream.next());
        tokio::select! {
            () = grant.checking.notified() => {}
            result = &mut next => panic!("unexpected delivery result: {result:?}"),
        }
        drop(next);
        grant.pending.store(false, Ordering::Relaxed);

        assert_eq!(
            stream.next().await.unwrap(),
            Some("queued while authorized")
        );
        stream.close().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_grant_check_respects_the_stream_deadline() {
        let operations = crate::runtime::Operations::default();
        let grant = Arc::new(DeliveryGrant::default());
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        let options = OperationOptions::default().with_deadline(deadline.into_std());
        let mut stream = buffered_read(&operations, grant.clone(), options).await;
        grant.pending.store(true, Ordering::Relaxed);

        assert_eq!(stream.next().await.unwrap_err().kind(), ErrorKind::Timeout);
        assert_eq!(stream.next().await.unwrap(), None);
    }
}
