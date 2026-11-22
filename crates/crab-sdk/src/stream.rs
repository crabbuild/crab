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
    pub(crate) progress: Option<crate::Progress>,
    pub(crate) delivered_bytes: u64,
    pub(crate) delivered_items: u64,
    pub(crate) id: crate::OperationId,
    pub(crate) entries: mpsc::Receiver<T>,
    pub(crate) task: Option<OperationTask<End>>,
    #[cfg(all(feature = "files", feature = "service"))]
    pub(crate) retention: Option<RetainedRead>,
}

impl<T> StreamReceiver<T> {
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
        if self.task.is_none() {
            return Ok(None);
        }
        if self
            .task
            .as_ref()
            .is_some_and(OperationTask::caller_is_cancelled)
        {
            return self.finish_cancellation().await;
        }
        #[cfg(all(feature = "files", feature = "service"))]
        let entry = match &self.retention {
            Some(retention) => match receive_retained(&mut self.entries, retention).await {
                Ok(entry) => entry,
                Err(source) => {
                    let error =
                        crate::remote_error::read_error(source.into()).with_operation(self.id);
                    return Err(match self.close().await {
                        Ok(()) => error,
                        Err(cleanup) => error.with_cleanup(cleanup),
                    });
                }
            },
            None => self.entries.recv().await,
        };
        #[cfg(not(all(feature = "files", feature = "service")))]
        let entry = self.entries.recv().await;
        if self
            .task
            .as_ref()
            .is_some_and(OperationTask::caller_is_cancelled)
        {
            return self.finish_cancellation().await;
        }
        if let Some(entry) = entry {
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
async fn receive_retained<T>(
    entries: &mut mpsc::Receiver<T>,
    retention: &RetainedRead,
) -> crab_coordination::Result<Option<T>> {
    let pin = retention.pin()?;
    let entry = tokio::select! {
        biased;
        () = pin.cancellation().cancelled() => None,
        entry = entries.recv() => entry,
    };
    // A queued frame may have been verified before the consumer stalled.
    // Check the claim at delivery and drop the pin before draining on failure.
    pin.ensure_valid()?;
    Ok(entry)
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
        let mut stream = StreamReceiver::<()> {
            progress: None,
            delivered_bytes: 0,
            delivered_items: 0,
            id,
            entries,
            task: Some(task),
            #[cfg(all(feature = "files", feature = "service"))]
            retention: None,
        };

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
        let mut stream = StreamReceiver {
            progress: None,
            delivered_bytes: 0,
            delivered_items: 0,
            id,
            entries,
            task: Some(task),
            retention: Some(retention),
        };

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
}
