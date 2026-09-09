use tokio::sync::watch;

use crate::OperationId;

/// An observed stage or cumulative delivery count, never a terminal result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProgressUpdate {
    Started,
    Cancelling,
    /// Bytes and items returned to the stream consumer, including buffered items.
    Delivered {
        bytes: u64,
        items: u64,
    },
}

/// A bounded progress observation associated with its producing operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProgressEvent {
    pub operation_id: OperationId,
    pub update: ProgressUpdate,
}

/// A shared single-slot progress destination that coalesces unread updates.
#[derive(Clone, Debug)]
pub struct Progress(watch::Sender<Option<ProgressEvent>>);

impl Progress {
    /// Create a bounded progress destination and its asynchronous consumer.
    ///
    /// Sharing a destination across operations coalesces across those operations;
    /// use separate channels when each operation needs its own latest update.
    #[must_use]
    pub fn channel() -> (Self, ProgressReceiver) {
        let (send, receive) = watch::channel(None);
        (Self(send), ProgressReceiver(receive))
    }

    pub(crate) fn emit(&self, operation_id: OperationId, update: ProgressUpdate) {
        self.0.send_replace(Some(ProgressEvent {
            operation_id,
            update,
        }));
    }
}

/// A progress consumer retaining at most the most recent unread observation.
///
/// Completion and failures must be observed through the operation or stream;
/// this channel closes only after all destination handles have been dropped.
#[derive(Debug)]
pub struct ProgressReceiver(watch::Receiver<Option<ProgressEvent>>);

impl ProgressReceiver {
    /// Wait for the latest unread update, or None after all senders are dropped.
    ///
    /// Dropping this wait does not consume an update or cancel an operation.
    pub async fn next(&mut self) -> Option<ProgressEvent> {
        self.0.changed().await.ok()?;
        *self.0.borrow_and_update()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn slow_consumer_gets_latest_update_after_sender_drop() {
        let (progress, mut receive) = Progress::channel();
        let id = OperationId::allocate().unwrap();
        for items in 1..=10000 {
            progress.emit(
                id,
                ProgressUpdate::Delivered {
                    bytes: items * 2,
                    items,
                },
            );
        }
        drop(progress);
        assert_eq!(
            receive.next().await,
            Some(ProgressEvent {
                operation_id: id,
                update: ProgressUpdate::Delivered {
                    bytes: 20000,
                    items: 10000
                },
            })
        );
        assert_eq!(receive.next().await, None);
    }
}
