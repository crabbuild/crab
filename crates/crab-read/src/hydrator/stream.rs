use std::{
    io::{self, Write},
    pin::Pin,
};

use bytes::Bytes;
use futures_util::Stream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::ShardHydrator;
use crate::{ReadError, Result};

const OUTPUT_CHUNK_BYTES: usize = 64 * 1024;
const OUTPUT_CHANNEL_DEPTH: usize = 1;

/// Bounded logical-byte stream produced by Xet reconstruction.
pub type ReconstructionStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send + 'static>>;

pub(super) fn reconstruct_range(
    hydrator: ShardHydrator,
    pointer: crab_types::pointer::Pointer,
    range: std::ops::Range<u64>,
) -> Result<ReconstructionStream> {
    if range.start > range.end || range.end > pointer.size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reconstruction range is outside the declared file size",
        )
        .into());
    }
    if range.is_empty() {
        return Ok(Box::pin(futures_util::stream::empty()));
    }
    let runtime = tokio::runtime::Handle::try_current().map_err(|source| {
        io::Error::other(format!(
            "reconstruction stream requires a Tokio runtime: {source}"
        ))
    })?;
    let cancellation = CancellationToken::new();
    let (send, receive) = mpsc::channel(OUTPUT_CHANNEL_DEPTH);
    let writer = ChannelWriter {
        send: send.clone(),
        cancellation: cancellation.clone(),
        runtime,
    };
    let operation_cancel = cancellation.clone();
    tokio::spawn(async move {
        let outcome = hydrator
            .reconstruct_range_to_writer_with_cancel(
                &pointer,
                range,
                writer,
                None,
                &operation_cancel,
            )
            .await;
        if let Err(error) = outcome
            && !operation_cancel.is_cancelled()
        {
            let _ = send.send(Err(error)).await;
        }
    });
    let cancel_on_drop = cancellation.drop_guard();
    Ok(Box::pin(futures_util::stream::unfold(
        (receive, cancel_on_drop),
        |(mut receive, cancel_on_drop)| async move {
            receive
                .recv()
                .await
                .map(|item| (item, (receive, cancel_on_drop)))
        },
    )))
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
