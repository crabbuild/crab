use std::sync::Arc;

use bytes::Bytes;
use crab_remote_git::{
    ContentClassification, OperationKind, RemoteGitRepository, RemoteGitSnapshot,
};
use tokio::sync::{mpsc, oneshot};

use crate::remote_error::{consumer_error, remote_error};
use crate::runtime::Operations;
use crate::stream::{End, StreamReceiver};
use crate::{Error, ErrorKind, GitPath, ReadOptions, Result};

#[cfg(feature = "content")]
pub(crate) mod crab;
#[cfg(feature = "content")]
pub(crate) mod lfs;
#[cfg(feature = "content")]
pub use crab::ContentCache;

const OUTPUT_CHUNK_BYTES: usize = 64 * 1024;

/// Verified logical file bytes with tracked cleanup and bounded delivery.
///
/// Observe EOF or close explicitly to receive finalization errors. Ordinary
/// Git objects are decoded within the owner's per-object memory bound.
pub struct ContentStream(StreamReceiver<Bytes>);

impl ContentStream {
    /// Return the identity of the operation producing this stream.
    #[must_use]
    pub fn operation_id(&self) -> crate::OperationId {
        self.0.id
    }

    /// Receive the next content chunk; successful EOF includes session close.
    pub async fn next(&mut self) -> Result<Option<Bytes>> {
        let value = self.0.next().await?;
        if let Some(value) = &value {
            self.0.delivered(value.len());
        }
        Ok(value)
    }

    /// Stop delivery and report unobserved finalization failures after cleanup.
    ///
    /// Closing early does not verify unread bytes; observe successful EOF before
    /// claiming full-file integrity.
    pub async fn close(self) -> Result<()> {
        self.0.close().await
    }
}

pub(crate) async fn open(
    operations: &Operations,
    repository: Arc<RemoteGitRepository>,
    snapshot: RemoteGitSnapshot,
    path: GitPath,
    options: ReadOptions,
    #[cfg(feature = "content")] runtime: Arc<crab::ContentRuntime>,
) -> Result<ContentStream> {
    let (send, entries) = mpsc::channel(1);
    let (ready, started) = oneshot::channel();
    let mut task = operations.start(options.operation.clone(), move |cancel| async move {
        let path = crab_remote_git::GitPath::new(path.as_bytes().to_vec()).map_err(remote_error)?;
        let operation = repository
            .operation_with_limits(OperationKind::Content, &cancel, options.content_limits())
            .await
            .map_err(remote_error)?;
        let result = async {
            let blob = snapshot.read_blob(&path, &operation).await?;
            #[cfg(feature = "content")]
            if blob.metadata.classification != ContentClassification::OrdinaryGit {
                let size = blob.metadata.logical_size.ok_or(
                    crab_remote_git::Error::InternalInvariant {
                        invariant: "verified pointer has no logical size",
                    },
                )?;
                let range = options.byte_range(size).map_err(consumer_error)?;
                operation
                    .charge(
                        crab_remote_git::BudgetDimension::ResponseBytes,
                        range.end - range.start,
                    )
                    .await?;
            }
            #[cfg(feature = "content")]
            if blob.metadata.classification == ContentClassification::LfsPointer {
                return lfs::deliver(
                    &runtime.layout,
                    blob.bytes,
                    options,
                    &operation,
                    send,
                    ready,
                )
                .await;
            }
            #[cfg(feature = "content")]
            if blob.metadata.classification == ContentClassification::CrabPointer {
                return runtime
                    .deliver(blob.bytes, options, &operation, send, ready)
                    .await;
            }
            if blob.metadata.classification != ContentClassification::OrdinaryGit {
                return Err(consumer_error(Error::new(
                    ErrorKind::UnsupportedCapability,
                    "pointer content requires hydration support",
                )));
            }
            let range = options
                .byte_range(blob.bytes.len() as u64)
                .map_err(consumer_error)?;
            let _ = ready.send(());
            deliver_bytes(
                blob.bytes.slice(range.start as usize..range.end as usize),
                &send,
                operation.cancellation(),
            )
            .await?;
            Ok(())
        }
        .await;
        match operation.finish(result).await {
            Ok(()) => Ok(End::Complete),
            Err(crab_remote_git::Error::Cancelled) => Ok(End::Cancelled),
            Err(error) => Err(remote_error(error)),
        }
    })?;
    let id = task.id();
    if started.await.is_err() {
        match task.wait().await? {
            End::Cancelled => {
                return Err(
                    Error::new(ErrorKind::Cancelled, "content opening was cancelled")
                        .with_operation(id),
                );
            }
            End::Complete => {
                return Err(
                    Error::new(ErrorKind::Io, "content ended before opening").with_operation(id)
                );
            }
        }
    }
    Ok(ContentStream(StreamReceiver {
        progress: task.progress(),
        delivered_bytes: 0,
        delivered_items: 0,
        id,
        entries,
        task: Some(task),
    }))
}

async fn deliver_bytes(
    bytes: Bytes,
    send: &mpsc::Sender<Bytes>,
    cancellation: &tokio_util::sync::CancellationToken,
) -> crab_remote_git::Result<()> {
    let mut offset = 0;
    while offset < bytes.len() {
        let end = offset.saturating_add(OUTPUT_CHUNK_BYTES).min(bytes.len());
        let chunk = bytes.slice(offset..end);
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(crab_remote_git::Error::Cancelled),
            result = send.send(chunk) => {
                if result.is_err() {
                    return Err(crab_remote_git::Error::Cancelled);
                }
            }
        }
        offset = end;
    }
    Ok(())
}
