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
    hydrator: ShardHydrator,
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
    hydrator: ShardHydrator,
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
    let (send, receive) = mpsc::channel(OUTPUT_CHANNEL_DEPTH);
    let writer = ChannelWriter {
        send: send.clone(),
        cancellation: cancellation.clone(),
        runtime,
    };
    let operation_cancel = cancellation.clone();
    let work = Box::pin(async move {
        let outcome = hydrator
            .reconstruct_range_to_writer_with_cancel(
                &pointer,
                range,
                writer,
                lookup.as_ref(),
                &operation_cancel,
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
    let cancel_on_drop = cancellation.drop_guard();
    let stream = Box::pin(futures_util::stream::unfold(
        (receive, cancel_on_drop),
        |(mut receive, cancel_on_drop)| async move {
            receive
                .recv()
                .await
                .map(|item| (item, (receive, cancel_on_drop)))
        },
    ));
    Ok((stream, work))
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
