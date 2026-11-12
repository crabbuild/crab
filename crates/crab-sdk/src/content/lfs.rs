use bytes::Bytes;
use crab_cache::{LocalCache, local_cache::LfsCacheFile};
use crab_remote_git::OperationContext;
use futures_util::StreamExt;
use std::ops::Range;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

use crate::remote_error::consumer_error;
use crate::{Error, ErrorKind, ReadOptions};

pub(crate) async fn deliver(
    runtime: &super::crab::ContentRuntime,
    bytes: Bytes,
    options: ReadOptions,
    operation: &OperationContext,
    send: mpsc::Sender<Bytes>,
    ready: oneshot::Sender<()>,
) -> crab_remote_git::Result<()> {
    let pointer = crab_git::LfsPointer::parse(&bytes).map_err(|source| {
        consumer_error(Error::with_source(
            ErrorKind::Corruption,
            "invalid LFS pointer",
            source,
        ))
    })?;
    if !pointer.extensions.is_empty() {
        return Err(consumer_error(Error::new(
            ErrorKind::UnsupportedCapability,
            "LFS extension processing is unavailable for remote reads",
        )));
    }
    let range = options.byte_range(pointer.size).map_err(consumer_error)?;
    let cancellation = operation.cancellation();
    let cache = runtime.local_cache().await.map_err(consumer_error)?;
    if let Some(cache) = cache
        && let Some((source, session, first)) =
            open_cached(cache, &pointer, &range, cancellation).await?
    {
        let _ = ready.send(());
        let result = deliver_cached(
            &source,
            &session,
            &pointer,
            range,
            first,
            &send,
            cancellation,
        )
        .await;
        // The proof cache retains a descriptor. Release it before finishing the
        // cache claim, or Windows cleanup cannot reclaim the selected payload.
        session.close().await;
        return match result {
            Ok(()) => source
                .finish::<crab_lfs::LfsError>(Ok(()))
                .await
                .map_err(lfs_error),
            Err(CachedError::Read(_)) if cancellation.is_cancelled() => {
                Err(crab_remote_git::Error::Cancelled)
            }
            Err(CachedError::Read(error)) => source.finish(Err(error)).await.map_err(lfs_error),
            Err(CachedError::Delivery(error)) => Err(error),
        };
    }
    let mut fill = if range == (0..pointer.size) {
        match cache {
            Some(cache) => match cache.begin_lfs_fill(&pointer.oid, pointer.size).await {
                Ok(fill) => fill,
                Err(error) => {
                    tracing::warn!(%error, "LFS cache retention unavailable");
                    None
                }
            },
            None => None,
        }
    } else {
        None
    };
    let store = crab_lfs::LfsObjectStore::new(
        runtime
            .layout
            .store()
            .clone()
            .with_read_admission(operation.read_admission()),
        runtime.layout.repo_prefix(),
    );
    let session = crab_lfs::LfsReadSession::new(runtime.lfs_proofs.clone());
    let result = async {
        let (_, _, mut stream) = tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                return Err(crab_remote_git::Error::Cancelled);
            }
            result = store.get_stream_with_session(&pointer.oid, pointer.size, Some(range), &session) => {
                result.map_err(lfs_error)?
            }
        };
        let _ = ready.send(());
        loop {
            let entry = tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    return Err(crab_remote_git::Error::Cancelled);
                }
                entry = stream.next() => entry,
            };
            match entry {
                Some(Ok(bytes)) => {
                    if let Some(writer) = fill.as_mut()
                        && let Err(error) = writer.write(&bytes).await
                    {
                        tracing::warn!(%error, "LFS cache fill failed");
                        fill = None;
                    }
                    super::deliver_bytes(bytes, &send, cancellation).await?;
                }
                Some(Err(error)) => return Err(lfs_error(error)),
                None => {
                    if cancellation.is_cancelled() {
                        return Err(crab_remote_git::Error::Cancelled);
                    }
                    if let Some(fill) = fill.take()
                        && let Err(error) = fill.commit().await
                    {
                        tracing::warn!(%error, "LFS cache publication failed");
                    }
                    return Ok(());
                }
            }
        }
    }.await;
    // Cancellation may drop a hashing join future. Drain its admitted job
    // before the SDK worker can finalize the Git session or report completion.
    session.close().await;
    result
}

async fn open_cached(
    cache: &Arc<LocalCache>,
    pointer: &crab_git::LfsPointer,
    range: &Range<u64>,
    cancel: &tokio_util::sync::CancellationToken,
) -> crab_remote_git::Result<Option<(LfsCacheFile, crab_lfs::LfsReadSession, Bytes)>> {
    let source = match cache.open_lfs_file(&pointer.oid, pointer.size).await {
        Ok(Some(source)) => source,
        Ok(None) => return Ok(None),
        Err(error) => {
            tracing::warn!(%error, "LFS cache read unavailable");
            return Ok(None);
        }
    };
    let session = crab_lfs::LfsReadSession::default();
    let first_range = range.start..range.end.min(range.start.saturating_add(64 * 1024));
    match read_cached(&source, &session, pointer, first_range, cancel).await {
        Ok(first) => Ok(Some((source, session, first))),
        Err(error) => {
            session.close().await;
            if cancel.is_cancelled() {
                return Err(crab_remote_git::Error::Cancelled);
            }
            // No bytes were delivered; optional corrupt cache can select origin.
            // After delivery starts, a selected-source failure remains visible.
            let _ = source.finish(Err(error)).await;
            Ok(None)
        }
    }
}

enum CachedError {
    Read(crab_lfs::LfsError),
    Delivery(crab_remote_git::Error),
}

async fn deliver_cached(
    source: &LfsCacheFile,
    session: &crab_lfs::LfsReadSession,
    pointer: &crab_git::LfsPointer,
    range: Range<u64>,
    first: Bytes,
    send: &mpsc::Sender<Bytes>,
    cancel: &tokio_util::sync::CancellationToken,
) -> std::result::Result<(), CachedError> {
    let mut offset = range.start + first.len() as u64;
    super::deliver_bytes(first, send, cancel)
        .await
        .map_err(CachedError::Delivery)?;
    while offset < range.end {
        let end = range.end.min(offset.saturating_add(64 * 1024));
        let bytes = read_cached(source, session, pointer, offset..end, cancel)
            .await
            .map_err(CachedError::Read)?;
        super::deliver_bytes(bytes, send, cancel)
            .await
            .map_err(CachedError::Delivery)?;
        offset = end;
    }
    Ok(())
}

async fn read_cached(
    source: &LfsCacheFile,
    session: &crab_lfs::LfsReadSession,
    pointer: &crab_git::LfsPointer,
    range: Range<u64>,
    cancel: &tokio_util::sync::CancellationToken,
) -> crab_lfs::Result<Bytes> {
    let file = source.reader()?;
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "LFS cache read cancelled").into()),
        result = session.read_opened_range(source.key(), file, &pointer.oid, pointer.size, range, cancel) => result,
    }
}

fn lfs_error(source: crab_lfs::LfsError) -> crab_remote_git::Error {
    let kind = crate::remote_error::lfs_kind(&source);
    if kind == ErrorKind::Cancelled {
        crab_remote_git::Error::Cancelled
    } else {
        consumer_error(Error::with_source(kind, "LFS content read failed", source))
    }
}
