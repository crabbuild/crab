use std::{
    future::Future,
    io::{self, Write},
    pin::Pin,
};

use bytes::Bytes;
use crab_metadata::file_index_lookup::SharedFileIndexLookup;
use futures_util::Stream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::ShardHydrator;
use crate::{ReadError, Result};

const OUTPUT_CHUNK_BYTES: usize = 64 * 1024;
const OUTPUT_CHANNEL_DEPTH: usize = 1;

/// Bounded logical-byte stream produced by Xet reconstruction.
pub type ReconstructionStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send + 'static>>;

/// Worker that must be driven until reconstruction and lookup cleanup finish.
pub type ReconstructionWork = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

pub(super) fn reconstruct_range(
    hydrator: &ShardHydrator,
    pointer: crab_types::pointer::Pointer,
    range: std::ops::Range<u64>,
) -> Result<ReconstructionStream> {
    if range.start == range.end && range.end <= pointer.size {
        return Ok(Box::pin(futures_util::stream::empty()));
    }
    let (stream, work) = prepare(hydrator, pointer, range, None, CancellationToken::new())?;
    tokio::spawn(work);
    Ok(stream)
}

pub(super) fn prepare(
    hydrator: &ShardHydrator,
    pointer: crab_types::pointer::Pointer,
    range: std::ops::Range<u64>,
    lookup: Option<SharedFileIndexLookup>,
    cancellation: CancellationToken,
) -> Result<(ReconstructionStream, ReconstructionWork)> {
    if range.start > range.end || range.end > pointer.size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reconstruction range is outside the declared file size",
        )
        .into());
    }
    let runtime = tokio::runtime::Handle::try_current().map_err(|source| {
        io::Error::other(format!(
            "reconstruction stream requires a Tokio runtime: {source}"
        ))
    })?;
    let cancellation = cancellation.child_token();
    let (receive, work) = start_range(
        hydrator.clone(),
        pointer,
        range,
        lookup,
        cancellation.clone(),
        runtime,
    );
    Ok((stream_output(hydrator, receive, cancellation), work))
}

fn start_range(
    hydrator: ShardHydrator,
    pointer: crab_types::pointer::Pointer,
    range: std::ops::Range<u64>,
    lookup: Option<SharedFileIndexLookup>,
    cancellation: CancellationToken,
    runtime: tokio::runtime::Handle,
) -> (mpsc::Receiver<Result<Bytes>>, ReconstructionWork) {
    let (send, receive) = mpsc::channel(OUTPUT_CHANNEL_DEPTH);
    let work = Box::pin(async move {
        let outcome = hydrator
            .reconstruct_range_writer(
                &pointer,
                range,
                |cancel| {
                    // A full queue must not hold the writer pin after claim loss.
                    Ok(ChannelWriter {
                        send: send.clone(),
                        cancellation: cancel.clone(),
                        runtime,
                    })
                },
                lookup.as_ref(),
                &cancellation,
            )
            .await;
        let closed = match lookup {
            Some(lookup) => lookup.close().await,
            None => Ok(()),
        };
        let outcome = match (outcome, closed) {
            (outcome, Ok(())) => outcome,
            (Ok(_), Err(error)) => Err(error.into()),
            (Err(operation), Err(source)) => Err(ReadError::CapturedLookupClose {
                operation: Box::new(operation),
                source,
            }),
        };
        if let Err(error) = outcome {
            let _ = send.send(Err(error)).await;
        }
    });
    (receive, work)
}

fn stream_output(
    hydrator: &ShardHydrator,
    receive: mpsc::Receiver<Result<Bytes>>,
    cancellation: CancellationToken,
) -> ReconstructionStream {
    let store = hydrator.router.store();
    let authority = (hydrator.retention.is_some() || store.has_bound_read_authority())
        .then(|| (hydrator.retention.clone(), store.clone()));
    let cancel_on_drop = cancellation.clone().drop_guard();
    Box::pin(futures_util::stream::try_unfold(
        (receive, authority, cancellation, cancel_on_drop),
        |(mut receive, authority, cancellation, cancel_on_drop)| async move {
            let bytes = receive.recv().await.transpose()?;
            // A verified chunk can sit past the grant or claim. At EOF a caller
            // may still hold the final chunk pending whole-file verification.
            if let Some((retention, store)) = &authority {
                crate::read_retention::check_read_authority(
                    retention.as_ref(),
                    store,
                    &cancellation,
                )
                .await?;
            }
            Ok(bytes.map(|bytes| (bytes, (receive, authority, cancellation, cancel_on_drop))))
        },
    ))
}

/// Verify the byte count and optional whole-file hash before releasing the final chunk.
///
/// Consumers can stop at the declared content length without polling stream EOF.
pub fn verify_reconstruction_stream(
    stream: ReconstructionStream,
    expected_size: u64,
    expected_hash: [u8; 32],
    verify_hash: bool,
) -> ReconstructionStream {
    use futures_util::StreamExt as _;

    let expected_hash_hex = blake3::Hash::from_bytes(expected_hash).to_hex().to_string();
    Box::pin(futures_util::stream::try_unfold(
        (
            stream,
            verify_hash.then(blake3::Hasher::new),
            0_u64,
            None::<Bytes>,
            false,
        ),
        move |(mut stream, mut hasher, mut received, mut pending, finished)| {
            let expected_hash_hex = expected_hash_hex.clone();
            async move {
                if finished {
                    return Ok(None);
                }
                loop {
                    match stream.next().await {
                        Some(Ok(chunk)) if chunk.is_empty() => {}
                        Some(Ok(chunk)) => {
                            received = received
                                .checked_add(chunk.len() as u64)
                                .filter(|size| *size <= expected_size)
                                .ok_or_else(|| ReadError::CorruptObject {
                                    path: expected_hash_hex.clone(),
                                    reason: format!(
                                        "reconstruction exceeded declared size {expected_size}"
                                    ),
                                })?;
                            if let Some(hasher) = &mut hasher {
                                hasher.update(&chunk);
                            }
                            if let Some(previous) = pending.replace(chunk) {
                                return Ok(Some((
                                    previous,
                                    (stream, hasher, received, pending, false),
                                )));
                            }
                        }
                        Some(Err(error)) => return Err(error),
                        None => {
                            if received != expected_size {
                                return Err(ReadError::CorruptObject {
                                    path: expected_hash_hex.clone(),
                                    reason: format!(
                                        "reconstruction size mismatch: expected {expected_size}, got {received}"
                                    ),
                                });
                            }
                            if let Some(hasher) = &hasher {
                                let actual = hasher.finalize();
                                if actual.as_bytes() != &expected_hash {
                                    return Err(ReadError::HashMismatch {
                                        requested: expected_hash_hex,
                                        actual: actual.to_hex().to_string(),
                                    });
                                }
                            }
                            if let Some(last) = pending.take() {
                                return Ok(Some((last, (stream, hasher, received, pending, true))));
                            }
                            return Ok(None);
                        }
                    }
                }
            }
        },
    ))
}

struct ChannelWriter {
    send: mpsc::Sender<Result<Bytes>>,
    cancellation: CancellationToken,
    runtime: tokio::runtime::Handle,
}

impl Write for ChannelWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let count = bytes.len().min(OUTPUT_CHUNK_BYTES);
        let chunk = Bytes::copy_from_slice(&bytes[..count]);
        self.runtime.block_on(async {
            tokio::select! {
                biased;
                () = self.cancellation.cancelled() => Err(cancelled_write()),
                result = self.send.send(Ok(chunk)) => result.map_err(|_| cancelled_write()),
            }
        })?;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn cancelled_write() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, ReadError::Cancelled)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use crab_coordination::read_retention::{ReadRetentionLease, RetainedReadRoot};
    use crab_coordination::read_retention_runtime::ReadRetentionSession;
    use futures_util::StreamExt as _;

    use super::*;
    use crate::hydrator::tests::{reconstruction_fixture, reconstruction_fixture_with_origin};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closed_retention_drains_a_backpressured_range_and_denies_queued_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let (hydrator, pointer, _) =
            reconstruction_fixture(&directory.path().join("cache"), false).await;
        let session = ReadRetentionSession::default();
        let root = RetainedReadRoot::new(blake3::hash(b"capture").to_hex().to_string(), 7).unwrap();
        let lease = ReadRetentionLease::create(
            Arc::new(object_store::memory::InMemory::new()),
            "repo/v2/readers/owner/claim".into(),
            root,
        )
        .await
        .unwrap();
        let hydrator =
            hydrator.with_read_retention(session.capture().unwrap().retain(lease).unwrap());
        assert!(pointer.size > OUTPUT_CHUNK_BYTES as u64);
        let range = 0..pointer.size;
        let cancellation = CancellationToken::new();
        let (receive, work) = start_range(
            hydrator.clone(),
            pointer,
            range,
            None,
            cancellation.clone(),
            tokio::runtime::Handle::current(),
        );
        let worker = tokio::spawn(work);
        wait_for_queued_bytes(&receive).await;
        let mut stream = stream_output(&hydrator, receive, cancellation);

        tokio::time::timeout(Duration::from_secs(5), session.shutdown())
            .await
            .expect("retention shutdown must drain without polling the stream")
            .unwrap();
        assert!(matches!(
            stream.next().await,
            Some(Err(ReadError::Retention(_)))
        ));
        assert!(stream.next().await.is_none());
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn rejected_grant_denies_queued_range_bytes_and_verification_eof() {
        #[derive(Default)]
        struct ReadGrant(AtomicBool);

        #[async_trait::async_trait]
        impl crab_storage::ReadAuthorization for ReadGrant {
            async fn check(&self) -> crab_storage::Result<()> {
                if self.0.load(Ordering::SeqCst) {
                    return Err(crab_storage::StorageError::Forbidden {
                        path: "closed test grant".into(),
                    });
                }
                Ok(())
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let grant = Arc::new(ReadGrant::default());
        let origin = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()))
            .with_target_identity([1; 32])
            .with_read_authorization(grant.clone(), "runtime-test")
            .unwrap();
        let (hydrator, pointer, _) =
            reconstruction_fixture_with_origin(&directory.path().join("cache"), false, origin)
                .await;
        for read_before_denial in [false, true] {
            grant.0.store(false, Ordering::SeqCst);
            let cancellation = CancellationToken::new();
            let (receive, work) = start_range(
                hydrator.clone(),
                pointer.clone(),
                0..512,
                None,
                cancellation.clone(),
                tokio::runtime::Handle::current(),
            );
            let worker = tokio::spawn(work);
            wait_for_queued_bytes(&receive).await;
            let mut stream = stream_output(&hydrator, receive, cancellation);
            if read_before_denial {
                assert_eq!(stream.next().await.unwrap().unwrap().len(), 512);
            }

            grant.0.store(true, Ordering::SeqCst);
            let error = stream.next().await.unwrap().unwrap_err();
            assert!(matches!(
                error,
                ReadError::Storage(crab_storage::StorageError::ReadRejected { source })
                    if matches!(
                        source.downcast_ref::<crab_storage::StorageError>(),
                        Some(crab_storage::StorageError::Forbidden { path }) if path == "closed test grant"
                    )
            ));
            assert!(stream.next().await.is_none());
            worker.await.unwrap();
        }
    }

    async fn wait_for_queued_bytes(receive: &mpsc::Receiver<Result<Bytes>>) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while receive.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}
