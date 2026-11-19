use std::{
    io::Write,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use axum::body::Body;
use bytes::Bytes;
use crab_remote_git::{Error, ReadReservation, RemoteGitRuntime, SharedBlame};
use http_body::{Frame, SizeHint};
use serde::{
    Serialize,
    ser::{SerializeSeq, SerializeStruct},
};
use tokio_util::sync::CancellationToken;

use super::representation::commit_json;

const MAX_BYTES: usize = 8 * 1024 * 1024;
const CHUNK_BYTES: usize = 64 * 1024;
const MAX_CHUNKS: usize = MAX_BYTES / CHUNK_BYTES;
// Retain the chunk slots and bytes::from_owner's atomic-plus-owner wrappers
// alongside payload admission, including frames held after the body is dropped.
const CHUNK_CONTROL_BYTES: usize = MAX_CHUNKS
    * (size_of::<Vec<u8>>()
        + size_of::<RetainedChunk>()
        + size_of::<std::sync::atomic::AtomicUsize>());

struct BoundedWriter {
    chunks: Vec<Vec<u8>>,
    bytes: usize,
    exceeded: bool,
    allocation: Option<std::collections::TryReserveError>,
}

impl BoundedWriter {
    fn new() -> crab_remote_git::Result<Self> {
        let mut chunks = Vec::new();
        chunks
            .try_reserve_exact(MAX_CHUNKS)
            .map_err(|source| Error::Allocation {
                requested: MAX_CHUNKS * size_of::<Vec<u8>>(),
                source,
            })?;
        Ok(Self {
            chunks,
            bytes: 0,
            exceeded: false,
            allocation: None,
        })
    }
}

impl Write for BoundedWriter {
    fn write(&mut self, mut bytes: &[u8]) -> std::io::Result<usize> {
        let length = bytes.len();
        if length > MAX_BYTES - self.bytes {
            self.exceeded = true;
            return Err(std::io::Error::other("HTTP response byte limit exceeded"));
        }
        while !bytes.is_empty() {
            match self.chunks.last_mut() {
                Some(chunk) if chunk.len() < CHUNK_BYTES => {
                    let count = bytes.len().min(CHUNK_BYTES - chunk.len());
                    chunk.extend_from_slice(&bytes[..count]);
                    bytes = &bytes[count..];
                }
                _ => {
                    let mut chunk = Vec::new();
                    if let Err(source) = chunk.try_reserve_exact(CHUNK_BYTES) {
                        self.allocation = Some(source);
                        return Err(std::io::Error::other("HTTP response allocation failed"));
                    }
                    let count = bytes.len().min(CHUNK_BYTES);
                    chunk.extend_from_slice(&bytes[..count]);
                    bytes = &bytes[count..];
                    self.chunks.push(chunk);
                }
            }
        }
        self.bytes += length;
        Ok(length)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct RetainedChunk {
    bytes: Vec<u8>,
    _memory: Arc<ReadReservation>,
}

impl AsRef<[u8]> for RetainedChunk {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

struct JsonBody {
    chunks: std::vec::IntoIter<Vec<u8>>,
    remaining: usize,
    memory: Arc<ReadReservation>,
}

impl http_body::Body for JsonBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let this = self.get_mut();
        Poll::Ready(this.chunks.next().map(|bytes| {
            this.remaining -= bytes.len();
            Ok(Frame::data(Bytes::from_owner(RetainedChunk {
                bytes,
                _memory: this.memory.clone(),
            })))
        }))
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining as u64)
    }

    fn is_end_stream(&self) -> bool {
        self.remaining == 0
    }
}

pub(super) async fn encode<T: Serialize + Send + 'static>(
    runtime: &Arc<RemoteGitRuntime>,
    cancellation: &CancellationToken,
    value: T,
    temporary_bytes: usize,
) -> crab_remote_git::Result<Body> {
    let memory = runtime
        .reserve_bytes(
            MAX_BYTES
                .saturating_add(CHUNK_CONTROL_BYTES)
                .saturating_add(temporary_bytes),
        )
        .await?;
    runtime
        .run_cpu(cancellation, move || {
            // Most replies are small. Allocate output as it is encoded, while
            // retaining the complete admission and emitting no partial success.
            let mut writer = BoundedWriter::new()?;
            if let Err(source) = serde_json::to_writer(&mut writer, &value) {
                if writer.exceeded {
                    return Err(Error::LimitExceeded {
                        limit: "HTTP response bytes",
                        actual: MAX_BYTES as u64 + 1,
                        maximum: MAX_BYTES as u64,
                    });
                }
                if let Some(source) = writer.allocation {
                    return Err(Error::Allocation {
                        requested: CHUNK_BYTES,
                        source,
                    });
                }
                return Err(Error::Consumer {
                    source: Box::new(source),
                });
            }
            Ok(Body::new(JsonBody {
                chunks: writer.chunks.into_iter(),
                remaining: writer.bytes,
                memory: Arc::new(memory),
            }))
        })
        .await
}

struct BlameReply {
    blame: SharedBlame,
    generation: u64,
}

impl Serialize for BlameReply {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        struct Ranges<'a>(&'a SharedBlame);
        impl Serialize for Ranges<'_> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let mut sequence = serializer.serialize_seq(Some(self.0.ranges.len()))?;
                for range in &self.0.ranges {
                    // Present one range at a time. Repeated origins must not
                    // multiply an entire intermediate JSON tree in memory.
                    sequence.serialize_element(&serde_json::json!({
                        "start": range.start, "lines": range.lines, "commit": commit_json(&range.commit),
                    }))?;
                }
                sequence.end()
            }
        }
        let mut value = serializer.serialize_struct("Blame", 3)?;
        value.serialize_field("ranges", &Ranges(&self.blame))?;
        value.serialize_field("commit", &self.blame.commit.to_string())?;
        value.serialize_field("generation", &self.generation)?;
        value.end()
    }
}

pub(super) async fn blame(
    runtime: &Arc<RemoteGitRuntime>,
    cancellation: &CancellationToken,
    blame: SharedBlame,
    generation: u64,
) -> crab_remote_git::Result<Body> {
    // Lossy UTF-8, hex, parent IDs and serde Value nodes exist for at most one
    // range. Admit their conservative capacity before constructing those nodes.
    let temporary = blame
        .ranges
        .iter()
        .map(|range| {
            let commit = &range.commit;
            commit
                .author
                .name
                .len()
                .saturating_add(commit.message.len())
                .saturating_add(commit.parents.len().saturating_mul(64))
                .saturating_add(1024)
                .saturating_mul(12)
        })
        .max()
        .unwrap_or(0);
    encode(
        runtime,
        cancellation,
        BlameReply { blame, generation },
        temporary,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body::Body as _;
    use http_body_util::BodyExt;

    #[test]
    fn small_json_does_not_allocate_the_response_ceiling() {
        let mut writer = BoundedWriter::new().unwrap();
        serde_json::to_writer(&mut writer, &serde_json::json!({"text": "small"})).unwrap();
        assert_eq!(
            writer.chunks.iter().map(Vec::capacity).sum::<usize>(),
            CHUNK_BYTES
        );
    }

    #[tokio::test]
    async fn chunk_boundaries_preserve_exact_json_and_content_length() {
        let runtime = Arc::new(RemoteGitRuntime::default());
        for length in [0, CHUNK_BYTES - 1, CHUNK_BYTES * 2, MAX_BYTES - 2] {
            let value = "a".repeat(length);
            let expected = serde_json::to_vec(&value).unwrap();
            let body = encode(&runtime, &CancellationToken::new(), value, 0)
                .await
                .unwrap();
            assert_eq!(body.size_hint().exact(), Some(expected.len() as u64));
            assert_eq!(body.collect().await.unwrap().to_bytes(), expected);
        }
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn oversized_encoding_errors_before_publishing_a_body() {
        let runtime = Arc::new(RemoteGitRuntime::default());
        let result = encode(
            &runtime,
            &CancellationToken::new(),
            "a".repeat(MAX_BYTES - 1),
            0,
        )
        .await;
        assert!(
            matches!(result, Err(Error::LimitExceeded { maximum, .. }) if maximum == MAX_BYTES as u64)
        );
        assert_eq!(runtime.snapshot().await.resident_bytes, 0);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn transport_slice_retains_output_admission_after_body_drop() {
        let runtime = Arc::new(RemoteGitRuntime::default());
        let mut body = encode(
            &runtime,
            &CancellationToken::new(),
            "a".repeat(CHUNK_BYTES * 2),
            0,
        )
        .await
        .unwrap();
        let bytes = body.frame().await.unwrap().unwrap().into_data().unwrap();
        let held = bytes.slice(1..2);
        drop(bytes);
        drop(body);
        assert!(runtime.snapshot().await.resident_bytes >= MAX_BYTES);
        drop(held);
        assert_eq!(runtime.snapshot().await.resident_bytes, 0);
        runtime.shutdown().await;
    }
}
