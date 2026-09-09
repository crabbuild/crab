use bytes::Bytes;
use crab_remote_git::OperationContext;
use crab_storage::{Store, StoreLayout};
use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot};

use crate::remote_error::consumer_error;
use crate::{Error, ErrorKind, ReadOptions};

pub(crate) async fn deliver(
    layout: &StoreLayout<Store>,
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
    let store = crab_lfs::LfsObjectStore::new(
        layout
            .store()
            .clone()
            .with_read_admission(operation.read_admission()),
        layout.repo_prefix(),
    );
    let cancellation = operation.cancellation();
    let session = crab_lfs::LfsReadSession::default();
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
                Some(Ok(bytes)) => super::deliver_bytes(bytes, &send, cancellation).await?,
                Some(Err(error)) => return Err(lfs_error(error)),
                None => return Ok(()),
            }
        }
    }.await;
    // Cancellation may drop a hashing join future. Drain its admitted job
    // before the SDK worker can finalize the Git session or report completion.
    session.close().await;
    result
}

fn lfs_error(source: crab_lfs::LfsError) -> crab_remote_git::Error {
    let kind = crate::remote_error::lfs_kind(&source);
    if kind == ErrorKind::Cancelled {
        crab_remote_git::Error::Cancelled
    } else {
        consumer_error(Error::with_source(kind, "LFS content read failed", source))
    }
}
