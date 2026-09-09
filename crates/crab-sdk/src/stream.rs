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
        if self
            .task
            .as_ref()
            .is_some_and(OperationTask::caller_is_cancelled)
        {
            return self.finish_cancellation().await;
        }
        // The worker owns the sender and observes the same caller token, so a
        // blocked receive wakes when cancellation finishes producer cleanup.
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
        self.task = None;
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
        self.entries.close();
        let Some(task) = self.task.as_mut() else {
            return Err(self.cancellation_error());
        };
        let result = task.wait().await;
        self.task = None;
        result.map(|_| ())?;
        Err(self.cancellation_error())
    }

    fn cancellation_error(&self) -> Error {
        Error::new(ErrorKind::Cancelled, "stream was cancelled").with_operation(self.id)
    }

    /// Stop traversal and wait for session close without verifying unread entries.
    pub async fn close(mut self) -> Result<()> {
        self.entries.close();
        if let Some(task) = self.task.as_mut() {
            task.cancel();
            task.wait().await?;
        }
        self.task = None;
        Ok(())
    }
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
}
