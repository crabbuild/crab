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

use super::representation::{FilePreview, commit_json};

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
    maximum: usize,
    exceeded: bool,
    allocation: Option<std::collections::TryReserveError>,
}

impl BoundedWriter {
    fn new(maximum: usize) -> crab_remote_git::Result<Self> {
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
            maximum,
            exceeded: false,
            allocation: None,
        })
    }
}

impl Write for BoundedWriter {
    fn write(&mut self, mut bytes: &[u8]) -> std::io::Result<usize> {
        let length = bytes.len();
        if length > self.maximum - self.bytes {
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

async fn encode<T: Serialize + Send + 'static>(
    runtime: &Arc<RemoteGitRuntime>,
    cancellation: &CancellationToken,
    value: T,
    temporary_bytes: usize,
    maximum: usize,
) -> crab_remote_git::Result<Body> {
    if cancellation.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let maximum = maximum.min(MAX_BYTES);
    let capacity = maximum.div_ceil(CHUNK_BYTES) * CHUNK_BYTES;
    let memory = runtime
        .reserve_bytes(
            capacity
                .saturating_add(CHUNK_CONTROL_BYTES)
                .saturating_add(temporary_bytes),
        )
        .await?;
    let token = cancellation.clone();
    let encode = move || {
        if token.is_cancelled() {
            return Err(Error::Cancelled);
        }
        // Most replies are small. Allocate output as it is encoded, while
        // retaining the complete admission and emitting no partial success.
        let mut writer = BoundedWriter::new(maximum)?;
        if let Err(source) = serde_json::to_writer(&mut writer, &value) {
            if writer.exceeded {
                return Err(Error::LimitExceeded {
                    limit: "HTTP response bytes",
                    actual: maximum as u64 + 1,
                    maximum: maximum as u64,
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
        if token.is_cancelled() {
            return Err(Error::Cancelled);
        }
        Ok(Body::new(JsonBody {
            chunks: writer.chunks.into_iter(),
            remaining: writer.bytes,
            memory: Arc::new(memory),
        }))
    };
    // A preview that fits one chunk has bounded serialization work and must
    // not queue behind bulk decodes. Larger replies retain CPU admission.
    if maximum <= CHUNK_BYTES {
        encode()
    } else {
        runtime.run_cpu(cancellation, encode).await
    }
}

#[derive(Serialize)]
#[serde(transparent)]
struct RetainedPreview {
    value: FilePreview,
    #[serde(skip)]
    _memory: ReadReservation,
}

pub(super) async fn preview(
    runtime: &Arc<RemoteGitRuntime>,
    cancellation: &CancellationToken,
    value: FilePreview,
    memory: ReadReservation,
) -> crab_remote_git::Result<Body> {
    let maximum = value.maximum_encoded_bytes();
    encode(
        runtime,
        cancellation,
        RetainedPreview {
            value,
            _memory: memory,
        },
        0,
        maximum,
    )
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
    let (temporary, maximum) = blame_encoding_budget(&blame);
    encode(
        runtime,
        cancellation,
        BlameReply { blame, generation },
        temporary,
        maximum,
    )
    .await
}

fn blame_encoding_budget(blame: &crab_remote_git::Blame) -> (usize, usize) {
    // Lossy UTF-8, hex, parent IDs and serde Value nodes exist for at most one
    // range. Admit their conservative capacity before constructing those nodes.
    blame
        .ranges
        .iter()
        .fold((0usize, 256usize), |(temporary, maximum), range| {
            let commit = &range.commit;
            let author = commit.author.name.len();
            let message = commit.message.len();
            let parents = commit.parents.len();
            let range_temporary = author
                .saturating_add(message)
                .saturating_add(parents.saturating_mul(64))
                .saturating_add(1024)
                .saturating_mul(12);
            // Lossy UTF-8 expands at most 3x and JSON escaping at most 6x;
            // messages also emit hex. Fixed range fields fit in 512 bytes,
            // and each parent ID fits in 128 bytes with quotes and separators.
            let range_maximum = author
                .saturating_mul(18)
                .saturating_add(message.saturating_mul(20))
                .saturating_add(parents.saturating_mul(128))
                .saturating_add(512);
            (
                temporary.max(range_temporary),
                maximum.saturating_add(range_maximum).min(MAX_BYTES),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body::Body as _;
    use http_body_util::BodyExt;

    #[test]
    fn small_json_does_not_allocate_the_response_ceiling() {
        let mut writer = BoundedWriter::new(MAX_BYTES).unwrap();
        serde_json::to_writer(&mut writer, &serde_json::json!({"text": "small"})).unwrap();
        assert_eq!(
            writer.chunks.iter().map(Vec::capacity).sum::<usize>(),
            CHUNK_BYTES
        );
    }

    #[tokio::test]
    async fn small_output_keeps_only_one_chunk_of_admission() {
        let runtime = Arc::new(RemoteGitRuntime::default());
        let mut body = encode(&runtime, &CancellationToken::new(), "small", 0, 512)
            .await
            .unwrap();
        let frame = body.frame().await.unwrap().unwrap();
        drop(body);
        let held = runtime.snapshot().await.resident_bytes;
        drop(frame);
        let released = runtime.snapshot().await.resident_bytes;
        runtime.shutdown().await;
        assert!((CHUNK_BYTES..CHUNK_BYTES * 2).contains(&held) && released == 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn small_encoding_does_not_wait_for_large_cpu_work() {
        let runtime = Arc::new(
            RemoteGitRuntime::new(
                crab_remote_git::RuntimeOptions {
                    max_decode_concurrency: 1,
                    ..Default::default()
                },
                Arc::new(crab_remote_git::NoopMetrics),
            )
            .unwrap(),
        );
        let (ready, started) = tokio::sync::oneshot::channel();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let worker_runtime = runtime.clone();
        let worker = tokio::spawn(async move {
            worker_runtime
                .run_cpu(&CancellationToken::new(), move || {
                    ready.send(()).unwrap();
                    let _ = held.recv();
                    Ok(())
                })
                .await
        });
        started.await.unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            encode(&runtime, &CancellationToken::new(), "small", 0, 512),
        )
        .await;
        let large = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            encode(
                &runtime,
                &CancellationToken::new(),
                "a".repeat(CHUNK_BYTES * 2),
                0,
                MAX_BYTES,
            ),
        )
        .await;
        drop(release);
        worker.await.unwrap().unwrap();
        let complete = (matches!(result, Ok(Ok(_))), large.is_err());
        runtime.shutdown().await;
        assert_eq!(complete, (true, true));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cached_small_blame_does_not_wait_for_large_cpu_work() {
        use crab_metadata::capsule_protocol::{Capsule, CapsuleRefEdit, CapsuleTransaction};
        use tower::ServiceExt;

        let history = crate::test_support::git::history_with_blob(b"source line\n");
        let commit = history.oids[0].clone();
        let mut server = crate::test_support::server::fixture_without_cells().await;
        Arc::get_mut(&mut server).unwrap().runtime = Arc::new(
            RemoteGitRuntime::new(
                crab_remote_git::RuntimeOptions {
                    max_decode_concurrency: 1,
                    ..Default::default()
                },
                Arc::new(crab_remote_git::NoopMetrics),
            )
            .unwrap(),
        );
        let repository = server.repositories.values().pop().unwrap();
        let root = crab_write::capsule_protocol::open_root(&repository.layout)
            .await
            .unwrap();
        let transaction = CapsuleTransaction::new(
            root.record().digest(),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                Some(commit.clone()),
                None,
            )],
        )
        .unwrap();
        let capsule = Capsule::build(&transaction, vec![history.pack], Vec::new()).unwrap();
        crab_write::capsule_protocol::publish(&repository.layout, root, &transaction, capsule)
            .await
            .unwrap();
        let app = crate::server::http::router(server.clone());
        let request = || {
            axum::http::Request::builder()
                .uri(format!(
                    "/api/repos/team/repo/blame?rev={commit}&path=asset"
                ))
                .header("host", "localhost")
                .body(Body::empty())
                .unwrap()
        };
        let seeded = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(seeded.status(), axum::http::StatusCode::OK);
        let expected = seeded.into_body().collect().await.unwrap().to_bytes();
        let (ready, started) = tokio::sync::oneshot::channel();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let runtime = server.runtime.clone();
        let worker = tokio::spawn(async move {
            runtime
                .run_cpu(&CancellationToken::new(), move || {
                    ready.send(()).unwrap();
                    let _ = held.recv();
                    Ok(())
                })
                .await
        });
        started.await.unwrap();
        let response = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            app.oneshot(request()),
        )
        .await;
        drop(release);
        worker.await.unwrap().unwrap();
        let identical = match response {
            Ok(Ok(response)) if response.status() == axum::http::StatusCode::OK => {
                response.into_body().collect().await.unwrap().to_bytes() == expected
            }
            _ => false,
        };
        drop(expected);
        crate::test_support::server::close(&server).await;
        assert!(
            identical,
            "cached small blame waited for occupied CPU capacity"
        );
    }

    #[test]
    fn blame_encoding_bound_covers_lossy_text_hex_and_numeric_extremes() {
        use crab_remote_git::{Blame, BlameRange, Commit, GitPath, Signature};

        let id = gix_hash::ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap();
        for repetitions in [0, 1, 2048] {
            let text = Bytes::from([0, b'"', b'\\', b'\n', 0xff].repeat(repetitions));
            let author = Signature {
                name: text.clone(),
                email: Bytes::new(),
                seconds: i64::MIN,
                offset_seconds: i32::MAX,
            };
            let commit = Commit {
                oid: id,
                tree: id,
                parents: vec![id; 16],
                author: author.clone(),
                committer: author,
                encoding: None,
                message: text,
                signature_headers: Vec::new(),
            };
            let blame = Blame {
                commit: id,
                path: GitPath::root(),
                ranges: vec![BlameRange {
                    start: u64::MAX,
                    lines: u64::MAX,
                    commit,
                    source_path: GitPath::root(),
                }],
            };
            let (_, maximum) = blame_encoding_budget(&blame);
            let range = &blame.ranges[0];
            let value = serde_json::json!({
                "ranges": [{"start":range.start,"lines":range.lines,"commit":commit_json(&range.commit)}],
                "commit":blame.commit.to_string(),"generation":u64::MAX,
            });
            assert!(serde_json::to_vec(&value).unwrap().len() <= maximum);
        }
    }

    #[tokio::test]
    async fn chunk_boundaries_preserve_exact_json_and_content_length() {
        let runtime = Arc::new(RemoteGitRuntime::default());
        for length in [0, CHUNK_BYTES - 1, CHUNK_BYTES * 2, MAX_BYTES - 2] {
            let value = "a".repeat(length);
            let expected = serde_json::to_vec(&value).unwrap();
            let body = encode(&runtime, &CancellationToken::new(), value, 0, MAX_BYTES)
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
            MAX_BYTES,
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
            MAX_BYTES,
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
