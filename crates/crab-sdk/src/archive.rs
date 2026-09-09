use std::sync::Arc;

use bytes::Bytes;
use crab_remote_git::{OperationKind, OperationLimits, RemoteGitRepository, RemoteGitSnapshot};
use tokio::sync::{mpsc, oneshot};

use crate::remote_error::{consumer_error, remote_error};
use crate::runtime::Operations;
use crate::stream::{End, StreamReceiver};
use crate::{Error, ErrorKind, ReadOptions, Result, TreeEntry};

/// Representation selected for archive file contents.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum ContentMode {
    #[default]
    Git,
    Hydrated,
}

/// One bounded archive frame, ordered Entry, zero or more Data, then EndEntry.
///
/// EndEntry follows successful content verification; archive EOF also includes
/// operation finalization. Directories and submodules have no Data frames.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ArchiveEvent {
    Entry { entry: TreeEntry, size: Option<u64> },
    Data(Bytes),
    EndEntry,
}

/// Bounded archive frames sharing one operation budget and cleanup lifetime.
pub struct ArchiveStream(StreamReceiver<ArchiveEvent>);

impl ArchiveStream {
    /// Return the identity of the operation producing this stream.
    #[must_use]
    pub fn operation_id(&self) -> crate::OperationId {
        self.0.id
    }

    /// Receive the next frame, reporting finalization errors before EOF.
    pub async fn next(&mut self) -> Result<Option<ArchiveEvent>> {
        let value = self.0.next().await?;
        if let Some(value) = &value {
            self.0.delivered(match value {
                ArchiveEvent::Data(bytes) => bytes.len(),
                _ => 0,
            });
        }
        Ok(value)
    }

    /// Stop traversal and drain cleanup without verifying unread content.
    ///
    /// Returns finalization failures not already observed through `next`.
    pub async fn close(self) -> Result<()> {
        self.0.close().await
    }
}

pub(crate) async fn open(
    operations: &Operations,
    repository: Arc<RemoteGitRepository>,
    snapshot: RemoteGitSnapshot,
    options: ReadOptions,
    limits: OperationLimits,
    mode: ContentMode,
    #[cfg(feature = "content")] runtime: Arc<crate::content::crab::ContentRuntime>,
) -> Result<ArchiveStream> {
    let (send, entries) = mpsc::channel(1);
    let (ready, started) = oneshot::channel();
    let mut task = operations.start(options.operation.clone(), move |cancel| async move {
        let operation = repository
            .operation_with_limits(OperationKind::Archive, &cancel, limits)
            .await
            .map_err(remote_error)?;
        let cancellation = operation.cancellation().clone();
        let mut reader = snapshot.archive_reader(operation).map_err(remote_error)?;
        if mode == ContentMode::Hydrated {
            reader = reader.with_logical_content_sizes();
        }
        let _ = ready.send(());
        let result = async {
            while let Some(entry) = reader.next().await? {
                let size = entry.metadata.as_ref().and_then(|metadata| {
                    if mode == ContentMode::Hydrated {
                        metadata.logical_size
                    } else {
                        Some(metadata.git_size)
                    }
                });
                let value = TreeEntry::from_owner(crab_remote_git::TreeEntry {
                    path: entry.path,
                    oid: entry.oid,
                    mode: entry.mode,
                    kind: entry.kind,
                    size,
                })
                .map_err(consumer_error)?;
                emit(
                    &send,
                    ArchiveEvent::Entry { entry: value, size },
                    &cancellation,
                )
                .await?;
                if let Some(bytes) = entry.bytes {
                    #[cfg(feature = "content")]
                    let hydrated = if mode == ContentMode::Hydrated {
                        let operation = reader.operation().ok_or(
                            crab_remote_git::Error::InternalInvariant {
                                invariant: "archive entry lost its operation",
                            },
                        )?;
                        hydrate(
                            &send,
                            entry.metadata.as_ref(),
                            bytes.clone(),
                            options.clone(),
                            operation,
                            &runtime,
                        )
                        .await?
                    } else {
                        false
                    };
                    #[cfg(not(feature = "content"))]
                    let hydrated = false;
                    if !hydrated {
                        for chunk in bytes.chunks(64 * 1024) {
                            emit(
                                &send,
                                ArchiveEvent::Data(Bytes::copy_from_slice(chunk)),
                                &cancellation,
                            )
                            .await?;
                        }
                    }
                }
                emit(&send, ArchiveEvent::EndEntry, &cancellation).await?;
            }
            Ok(End::Complete)
        }
        .await;
        match reader.finish(result).await {
            Ok(end) => Ok(end),
            Err(crab_remote_git::Error::Cancelled) => Ok(End::Cancelled),
            Err(error) => Err(remote_error(error)),
        }
    })?;
    let id = task.id();
    if started.await.is_err() {
        task.wait().await?;
        return Err(Error::new(ErrorKind::Io, "archive ended before opening").with_operation(id));
    }
    Ok(ArchiveStream(StreamReceiver {
        progress: task.progress(),
        delivered_bytes: 0,
        delivered_items: 0,
        id,
        entries,
        task: Some(task),
    }))
}

async fn emit(
    send: &mpsc::Sender<ArchiveEvent>,
    event: ArchiveEvent,
    cancellation: &tokio_util::sync::CancellationToken,
) -> crab_remote_git::Result<()> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(crab_remote_git::Error::Cancelled),
        result = send.send(event) => result.map_err(|_| crab_remote_git::Error::Cancelled),
    }
}

#[cfg(feature = "content")]
async fn hydrate(
    send: &mpsc::Sender<ArchiveEvent>,
    metadata: Option<&crab_remote_git::BlobMetadata>,
    bytes: Bytes,
    options: ReadOptions,
    operation: &crab_remote_git::OperationContext,
    runtime: &crate::content::crab::ContentRuntime,
) -> crab_remote_git::Result<bool> {
    use crab_remote_git::ContentClassification;
    let metadata = metadata.ok_or(crab_remote_git::Error::InternalInvariant {
        invariant: "archive blob has no verified metadata",
    })?;
    if metadata.classification == ContentClassification::OrdinaryGit {
        return Ok(false);
    }
    let (chunks, mut receive) = mpsc::channel(1);
    let (ready, _started) = oneshot::channel();
    let reconstruct = async {
        match metadata.classification {
            ContentClassification::CrabPointer => {
                runtime
                    .deliver(bytes, options, operation, chunks, ready)
                    .await
            }
            ContentClassification::LfsPointer => {
                crate::content::lfs::deliver(
                    &runtime.layout,
                    bytes,
                    options,
                    operation,
                    chunks,
                    ready,
                )
                .await
            }
            ContentClassification::OrdinaryGit => Ok(()),
        }
    };
    let forward = async move {
        while let Some(bytes) = receive.recv().await {
            emit(send, ArchiveEvent::Data(bytes), operation.cancellation()).await?;
        }
        Ok::<_, crab_remote_git::Error>(())
    };
    // A disconnected consumer drops the chunk receiver, unblocking reconstruction.
    // Join both futures so parser/hash cleanup completes before archive finalization.
    let (reconstructed, forwarded) = tokio::join!(reconstruct, forward);
    reconstructed?;
    forwarded?;
    Ok(true)
}
