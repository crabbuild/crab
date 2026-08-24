//! Standalone LFS transfer agent protocol.
//!
//! Implements the Git LFS custom/standalone transfer agent protocol using
//! JSON lines over stdin/stdout. Handles `init`, `upload`, `download`, and
//! `terminate` events with concurrent transfer support and progress reporting.
//!
//! Retry and resume behaviour:
//! - Transient errors (network, 5xx) are retried up to 3 times with
//!   exponential backoff (1 s base, 4 s cap).
//! - Permanent errors (not found, access denied) surface immediately.
//! - Objects larger than 64 MB upload via a streaming multipart path
//!   (see [`crab_lfs::LfsObjectStore::put_stream`])
//!   that bounds peak memory to a few parts-in-flight regardless of
//!   file size, so a 50 GiB object uploads without OOMing. Downloads
//!   use range-request resume with partial state persisted in
//!   `.git/lfs/tmp/`.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Semaphore};

use crate::core::error::{CrabError, Result};
use crate::storage::retry::{RetryPolicy, retry};
use crab_lfs::LfsObjectStore;

// ---------------------------------------------------------------------------
// Protocol message types
// ---------------------------------------------------------------------------

/// Inbound event from the LFS client (one JSON object per line on stdin).
#[derive(Debug, Deserialize)]
#[serde(tag = "event")]
#[serde(rename_all = "lowercase")]
enum InboundEvent {
    Init {
        operation: String,
        #[serde(default)]
        #[allow(dead_code)]
        remote: String,
        #[serde(default)]
        #[allow(dead_code)]
        concurrent: bool,
        #[serde(default = "default_concurrency")]
        concurrenttransfers: u32,
    },
    Upload {
        oid: String,
        size: u64,
        path: String,
    },
    Download {
        oid: String,
        size: u64,
    },
    Terminate,
}

fn default_concurrency() -> u32 {
    8
}

/// Init response sent back to the LFS client.
#[derive(Debug, Serialize)]
struct InitResponse {}

/// Progress event emitted during a transfer.
#[derive(Debug, Serialize)]
struct ProgressEvent {
    event: &'static str,
    oid: String,
    #[serde(rename = "bytesSoFar")]
    bytes_so_far: u64,
    #[serde(rename = "bytesSinceLast")]
    bytes_since_last: u64,
}

/// Successful completion event.
#[derive(Debug, Serialize)]
struct CompleteEvent {
    event: &'static str,
    oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<TransferError>,
}

/// Error detail within a complete event.
#[derive(Debug, Serialize)]
struct TransferError {
    code: u32,
    message: String,
}

// ---------------------------------------------------------------------------
// Error code mapping
// ---------------------------------------------------------------------------

/// Map a `CrabError` to the standardized LFS transfer error code.
///
/// Codes: 1 = generic, 2 = not found, 3 = exists, 4 = unauthorized,
/// 5 = rate limited.
fn error_code(err: &CrabError) -> u32 {
    match err {
        CrabError::LfsObjectMissing { .. } | CrabError::NotFound { .. } => 2,
        CrabError::RefAlreadyExists { .. } => 3,
        CrabError::Forbidden { .. }
        | CrabError::AuthFailed { .. }
        | CrabError::AuthExpired { .. }
        | CrabError::NoCredentials => 4,
        CrabError::Throttled { .. } => 5,
        _ => 1,
    }
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Objects larger than this threshold use multipart upload and
/// range-request download resume.
const MULTIPART_THRESHOLD: u64 = 64 * 1024 * 1024; // 64 MB

/// Retry policy for individual LFS transfers: 3 attempts with
/// exponential backoff (1 s base, 4 s cap).
const TRANSFER_RETRY_POLICY: RetryPolicy = RetryPolicy {
    max_attempts: 3,
    base: Duration::from_secs(1),
    cap: Duration::from_secs(4),
};

// ---------------------------------------------------------------------------
// Transfer agent entry point
// ---------------------------------------------------------------------------

/// Run the standalone LFS transfer agent protocol loop.
///
/// Reads JSON-line events from `input` (stdin), dispatches uploads and
/// downloads concurrently via tokio tasks bounded by a semaphore, and
/// writes JSON-line responses to `output` (stdout). Returns when a
/// `terminate` event is received or the input stream ends.
///
/// # Errors
///
/// Returns [`CrabError::LfsTransferProtocol`] on malformed input.
/// Individual transfer failures are reported as `complete` events with
/// an `error` object — they do not terminate the agent.
pub async fn run_transfer_agent<R, W>(input: R, output: W, store: LfsObjectStore) -> Result<()>
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    let output = Arc::new(Mutex::new(output));
    let store = Arc::new(store);

    // Default concurrency; overridden by the init event.
    let mut initialized = false;

    // Read lines from stdin synchronously in a blocking task so we don't
    // block the tokio runtime.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<InboundEvent>(64);

    let reader_handle = tokio::task::spawn_blocking(move || {
        for line_result in input.lines() {
            let line = match line_result {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(error = %e, "stdin read error, stopping reader");
                    break;
                }
            };

            if line.trim().is_empty() {
                continue;
            }

            match serde_json::from_str::<InboundEvent>(&line) {
                Ok(event) => {
                    if tx.blocking_send(event).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!(line = %line, error = %e, "ignoring malformed JSON line");
                }
            }
        }
    });

    // Semaphore created after init tells us the concurrency limit.
    let mut semaphore: Option<Arc<Semaphore>> = None;
    let mut join_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    while let Some(event) = rx.recv().await {
        match event {
            InboundEvent::Init {
                operation,
                concurrenttransfers,
                ..
            } => {
                let concurrency = concurrenttransfers.clamp(1, 100);
                initialized = true;
                semaphore = Some(Arc::new(Semaphore::new(concurrency as usize)));

                tracing::debug!(
                    %operation,
                    concurrency,
                    "transfer agent initialized",
                );

                // Respond with empty JSON object.
                write_json_line(&output, &InitResponse {}).await?;
            }

            InboundEvent::Upload { oid, size, path } => {
                if !initialized {
                    return Err(CrabError::LfsTransferProtocol(
                        "upload event before init".into(),
                    ));
                }

                let Some(sem) = semaphore.clone() else {
                    return Err(CrabError::LfsTransferProtocol(
                        "upload event before init".into(),
                    ));
                };
                let store = Arc::clone(&store);
                let output = Arc::clone(&output);

                let handle = tokio::spawn(async move {
                    let _permit = sem.acquire().await;
                    let result = handle_upload(&store, &oid, size, &path, &output).await;
                    if let Err(e) = result {
                        tracing::error!(oid = %oid, error = %e, "upload failed to send response");
                    }
                });
                join_handles.push(handle);
            }

            InboundEvent::Download { oid, size } => {
                if !initialized {
                    return Err(CrabError::LfsTransferProtocol(
                        "download event before init".into(),
                    ));
                }

                let Some(sem) = semaphore.clone() else {
                    return Err(CrabError::LfsTransferProtocol(
                        "download event before init".into(),
                    ));
                };
                let store = Arc::clone(&store);
                let output = Arc::clone(&output);

                let handle = tokio::spawn(async move {
                    let _permit = sem.acquire().await;
                    let result = handle_download(&store, &oid, size, &output).await;
                    if let Err(e) = result {
                        tracing::error!(oid = %oid, error = %e, "download failed to send response");
                    }
                });
                join_handles.push(handle);
            }

            InboundEvent::Terminate => {
                tracing::debug!("received terminate, waiting for in-flight transfers");
                break;
            }
        }
    }

    // Wait for all in-flight transfers to complete.
    for handle in join_handles {
        let _ = handle.await;
    }

    // Drop the reader task.
    reader_handle.abort();
    let _ = reader_handle.await;

    Ok(())
}

// ---------------------------------------------------------------------------
// Upload handler
// ---------------------------------------------------------------------------

/// Handle a single upload: read the local file, upload to the object store,
/// emit progress and complete events.
async fn handle_upload<W: Write + Send>(
    store: &LfsObjectStore,
    oid: &str,
    size: u64,
    path: &str,
    output: &Arc<Mutex<W>>,
) -> Result<()> {
    let result = do_upload(store, oid, size, path, output).await;
    match result {
        Ok(()) => {
            write_json_line(
                output,
                &CompleteEvent {
                    event: "complete",
                    oid: oid.to_owned(),
                    path: None,
                    error: None,
                },
            )
            .await
        }
        Err(e) => {
            let code = error_code(&e);
            write_json_line(
                output,
                &CompleteEvent {
                    event: "complete",
                    oid: oid.to_owned(),
                    path: None,
                    error: Some(TransferError {
                        code,
                        message: e.to_string(),
                    }),
                },
            )
            .await
        }
    }
}

/// Core upload logic separated from error-response plumbing.
///
/// Chooses between two paths based on the declared object size:
///
/// - **Small objects** (≤ [`MULTIPART_THRESHOLD`]): load the full file
///   into memory and hand it to [`LfsObjectStore::put`]. This keeps
///   the simple fast path for the 99% case and shares the
///   Store-level CAS idempotency (PutMode::Create + blake3 content
///   compare). Peak memory ≈ file size, which is bounded by the
///   threshold (currently 64 MiB) so this is always safe.
///
/// - **Large objects** (> [`MULTIPART_THRESHOLD`]): stream the file
///   through [`LfsObjectStore::put_stream`], which reads in bounded
///   chunks, hashes incrementally, and drives the object-store
///   multipart API directly. Peak memory is bounded to
///   `STREAM_PART_SIZE * MAX_IN_FLIGHT_PARTS` (~32 MiB at defaults)
///   regardless of file size, so a 50 GiB LFS object uploads
///   without OOMing.
///
/// Retries apply at the full-operation level. For the streaming
/// path we rely on the object-store's internal per-part retry plus
/// the outer [`retry`] wrapper for transient errors that escape part
/// handling (connection resets between part PUTs, etc.). Because the
/// multipart upload is aborted on error, a retry starts a fresh
/// UploadId — no orphan parts accumulate on S3.
async fn do_upload<W: Write + Send>(
    store: &LfsObjectStore,
    oid: &str,
    size: u64,
    path: &str,
    output: &Arc<Mutex<W>>,
) -> Result<()> {
    let oid_bytes = parse_oid_hex(oid)?;
    let actual_size = tokio::fs::metadata(path)
        .await
        .map_err(CrabError::Io)?
        .len();
    if actual_size != size {
        return Err(CrabError::LfsObjectCorrupt {
            oid: oid.to_owned(),
        });
    }

    // Emit a progress event at the start (0 bytes). Done before any
    // file I/O so the LFS client sees activity immediately even if
    // the file is large and the first read stalls on a cold cache.
    write_json_line(
        output,
        &ProgressEvent {
            event: "progress",
            oid: oid.to_owned(),
            bytes_so_far: 0,
            bytes_since_last: 0,
        },
    )
    .await?;

    if size > MULTIPART_THRESHOLD {
        // Large object: streaming path. `put_stream` takes a file path
        // and handles reading/hashing/multipart itself, bounding peak
        // memory to one part × MAX_IN_FLIGHT_PARTS regardless of the
        // file's size.
        let file_path = std::path::PathBuf::from(path);
        retry(&TRANSFER_RETRY_POLICY, || {
            let fp = file_path.clone();
            async move {
                store
                    .put_stream(&oid_bytes, &fp)
                    .await
                    .map_err(CrabError::from)
            }
        })
        .await?;
    } else {
        // Small object: buffer-and-put path, same as before. Reading
        // and the put happen under the outer retry so transient
        // failures re-read the file (in case the prior attempt
        // consumed the buffer or we got a partial file).
        let file_path = path.to_owned();
        let content = tokio::task::spawn_blocking(move || std::fs::read(&file_path))
            .await
            .map_err(|e| CrabError::Internal(format!("spawn_blocking join error: {e}")))?
            .map_err(CrabError::Io)?;
        let bytes = Bytes::from(content);

        // Upload with retry. The underlying LfsObjectStore.put already
        // verifies SHA-256 integrity and is idempotent, so retries are
        // safe.
        retry(&TRANSFER_RETRY_POLICY, || {
            let b = bytes.clone();
            async { store.put(&oid_bytes, b).await.map_err(CrabError::from) }
        })
        .await?;
    }

    // Emit final progress event.
    write_json_line(
        output,
        &ProgressEvent {
            event: "progress",
            oid: oid.to_owned(),
            bytes_so_far: size,
            bytes_since_last: size,
        },
    )
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Download handler
// ---------------------------------------------------------------------------

/// Handle a single download: fetch from the object store, write to a temp
/// file, emit progress and complete events.
async fn handle_download<W: Write + Send>(
    store: &LfsObjectStore,
    oid: &str,
    size: u64,
    output: &Arc<Mutex<W>>,
) -> Result<()> {
    let result = do_download(store, oid, size, output).await;
    match result {
        Ok(temp_path) => {
            write_json_line(
                output,
                &CompleteEvent {
                    event: "complete",
                    oid: oid.to_owned(),
                    path: Some(temp_path),
                    error: None,
                },
            )
            .await
        }
        Err(e) => {
            let code = error_code(&e);
            write_json_line(
                output,
                &CompleteEvent {
                    event: "complete",
                    oid: oid.to_owned(),
                    path: None,
                    error: Some(TransferError {
                        code,
                        message: e.to_string(),
                    }),
                },
            )
            .await
        }
    }
}

/// Core download logic separated from error-response plumbing.
///
/// For objects larger than [`MULTIPART_THRESHOLD`], the download uses
/// range requests to resume from the last received byte if a partial
/// file exists in `.git/lfs/tmp/`. Transient errors are retried up to
/// 3 times with exponential backoff.
async fn do_download<W: Write + Send>(
    store: &LfsObjectStore,
    oid: &str,
    size: u64,
    output: &Arc<Mutex<W>>,
) -> Result<String> {
    let oid_bytes = parse_oid_hex(oid)?;

    let tmp_dir = lfs_tmp_dir();

    // Create the temp directory if it doesn't exist.
    let tmp_dir_clone = tmp_dir.clone();
    tokio::task::spawn_blocking(move || std::fs::create_dir_all(&tmp_dir_clone))
        .await
        .map_err(|e| CrabError::Internal(format!("spawn_blocking join error: {e}")))?
        .map_err(CrabError::Io)?;

    let partial_path = tmp_dir.join(format!("{oid}.partial"));
    let final_path = tmp_dir.join(oid);

    // Emit a progress event at the start (0 bytes).
    write_json_line(
        output,
        &ProgressEvent {
            event: "progress",
            oid: oid.to_owned(),
            bytes_so_far: 0,
            bytes_since_last: 0,
        },
    )
    .await?;

    // For large objects, attempt range-request resume from partial file.
    let use_resume = size > MULTIPART_THRESHOLD;
    let content = if use_resume {
        download_with_resume(store, &oid_bytes, size, &partial_path, oid, output).await?
    } else {
        // Small object: simple get with retry.
        retry(&TRANSFER_RETRY_POLICY, || async {
            store.get(&oid_bytes).await.map_err(CrabError::from)
        })
        .await?
    };

    let actual_size = content.len() as u64;
    if let Err(error) = crate::lfs::cache::verify_bytes(&oid_bytes, size, &content) {
        if use_resume {
            let corrupt_partial = partial_path.clone();
            let _ =
                tokio::task::spawn_blocking(move || std::fs::remove_file(corrupt_partial)).await;
        }
        return Err(error);
    }

    // Write content to the final temp file.
    let content_for_write = content;
    let final_path_clone = final_path.clone();
    let temp_path = tokio::task::spawn_blocking(move || -> Result<String> {
        std::fs::write(&final_path_clone, &content_for_write).map_err(CrabError::Io)?;
        Ok(final_path_clone.to_string_lossy().into_owned())
    })
    .await
    .map_err(|e| CrabError::Internal(format!("spawn_blocking join error: {e}")))??;

    // Clean up partial file on success.
    if use_resume {
        let pp = partial_path.clone();
        let _ = tokio::task::spawn_blocking(move || std::fs::remove_file(&pp)).await;
    }

    // Emit final progress event.
    let report_size = if size > 0 { size } else { actual_size };
    write_json_line(
        output,
        &ProgressEvent {
            event: "progress",
            oid: oid.to_owned(),
            bytes_so_far: report_size,
            bytes_since_last: report_size,
        },
    )
    .await?;

    Ok(temp_path)
}

/// Download a large object using range requests for resume.
///
/// If a partial file exists at `partial_path`, reads its length and
/// issues a range request for the remaining bytes. The partial file is
/// updated incrementally so that a subsequent retry can resume from
/// where the previous attempt left off.
async fn download_with_resume<W: Write + Send>(
    store: &LfsObjectStore,
    oid: &[u8; 32],
    size: u64,
    partial_path: &Path,
    oid_hex: &str,
    output: &Arc<Mutex<W>>,
) -> Result<Bytes> {
    let obj_path = store.object_path_for(oid);

    // Check for existing partial download.
    let pp = partial_path.to_path_buf();
    let existing_len: u64 = tokio::task::spawn_blocking(move || -> u64 {
        std::fs::metadata(&pp).map(|m| m.len()).unwrap_or(0)
    })
    .await
    .map_err(|e| CrabError::Internal(format!("spawn_blocking join error: {e}")))?;

    if existing_len > 0 && existing_len < size {
        tracing::debug!(
            oid = %oid_hex,
            existing_bytes = existing_len,
            total_size = size,
            "resuming download from partial file",
        );

        // Report progress for already-downloaded bytes.
        write_json_line(
            output,
            &ProgressEvent {
                event: "progress",
                oid: oid_hex.to_owned(),
                bytes_so_far: existing_len,
                bytes_since_last: existing_len,
            },
        )
        .await?;

        // Fetch the remaining range with retry.
        let remaining = retry(&TRANSFER_RETRY_POLICY, || {
            let path = obj_path.clone();
            async move {
                store
                    .store()
                    .range_get(&path, existing_len..size)
                    .await
                    .map_err(CrabError::from)
            }
        })
        .await?;

        // Append to partial file and read the complete content.
        let pp = partial_path.to_path_buf();
        let content = tokio::task::spawn_blocking(move || -> Result<Bytes> {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&pp)
                .map_err(CrabError::Io)?;
            f.write_all(&remaining).map_err(CrabError::Io)?;
            f.flush().map_err(CrabError::Io)?;
            drop(f);
            let data = std::fs::read(&pp).map_err(CrabError::Io)?;
            Ok(Bytes::from(data))
        })
        .await
        .map_err(|e| CrabError::Internal(format!("spawn_blocking join error: {e}")))??;

        return Ok(content);
    }

    // No usable partial file — full download with retry, saving partial
    // state so a future attempt can resume.
    let result = retry(&TRANSFER_RETRY_POLICY, || {
        let path = obj_path.clone();
        async move {
            let (bytes, _etag) = store
                .store()
                .get_with_etag(&path)
                .await
                .map_err(CrabError::from)?;
            Ok(bytes)
        }
    })
    .await;

    match result {
        Ok(content) => {
            // Write partial file so cross-process resume works if the
            // caller crashes after download but before final rename.
            let pp = partial_path.to_path_buf();
            let c = content.clone();
            let _ = tokio::task::spawn_blocking(move || std::fs::write(&pp, &c)).await;
            Ok(content)
        }
        Err(e) => Err(match e {
            CrabError::NotFound { .. } => CrabError::LfsObjectMissing {
                oid: crab_git::lfs_pointer::hex_encode(oid),
            },
            other => other,
        }),
    }
}

/// Returns the `.git/lfs/tmp/` directory path for partial transfer state.
///
fn lfs_tmp_dir() -> PathBuf {
    let git_dir = crate::git::discover::discover_common_git_dir().unwrap_or_else(|_| {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".git")
    });

    git_dir.join("lfs").join("tmp")
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Write a single JSON line to the output, followed by a newline.
async fn write_json_line<W: Write + Send, T: Serialize>(
    output: &Arc<Mutex<W>>,
    value: &T,
) -> Result<()> {
    let json = serde_json::to_string(value)
        .map_err(|e| CrabError::LfsTransferProtocol(format!("failed to serialize JSON: {e}")))?;

    let mut out = output.lock().await;
    out.write_all(json.as_bytes()).map_err(CrabError::Io)?;
    out.write_all(b"\n").map_err(CrabError::Io)?;
    out.flush().map_err(CrabError::Io)?;
    Ok(())
}

/// Parse a 64-character hex OID string into 32 bytes.
fn parse_oid_hex(hex: &str) -> Result<[u8; 32]> {
    if hex.len() != 64 {
        return Err(CrabError::LfsTransferProtocol(format!(
            "invalid OID length: expected 64 hex chars, got {}",
            hex.len(),
        )));
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = hex_nibble(hex.as_bytes()[i * 2]).map_err(|()| {
            CrabError::LfsTransferProtocol(format!("invalid hex char in OID: {hex:?}"))
        })?;
        let lo = hex_nibble(hex.as_bytes()[i * 2 + 1]).map_err(|()| {
            CrabError::LfsTransferProtocol(format!("invalid hex char in OID: {hex:?}"))
        })?;
        *byte = (hi << 4) | lo;
    }
    Ok(out)
}

/// Convert a single ASCII hex character to its 4-bit value.
fn hex_nibble(b: u8) -> std::result::Result<u8, ()> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(()),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use std::io::{BufReader, Cursor};
    use std::sync::Arc;

    use sha2::{Digest, Sha256};

    use super::*;
    use crab_git::lfs_pointer::hex_encode;
    use crab_storage::{RetryPolicy, Store};

    fn test_store() -> LfsObjectStore {
        let inner: Arc<dyn object_store::ObjectStore> =
            Arc::new(object_store::memory::InMemory::new());
        let policy = RetryPolicy {
            max_attempts: 2,
            base: std::time::Duration::from_millis(1),
            cap: std::time::Duration::from_millis(5),
        };
        let store = Store::with_retry(inner, policy);
        LfsObjectStore::new(store, "repo")
    }

    fn sha256_oid(data: &[u8]) -> [u8; 32] {
        let hash = Sha256::digest(data);
        let mut oid = [0u8; 32];
        oid.copy_from_slice(&hash);
        oid
    }

    /// Collect all JSON lines written to the output buffer.
    fn parse_output_lines(output: &[u8]) -> Vec<serde_json::Value> {
        let text = String::from_utf8_lossy(output);
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn init_responds_with_empty_json() {
        let store = test_store();
        let input = concat!(
            r#"{"event":"init","operation":"upload","remote":"origin","concurrent":true,"concurrenttransfers":4}"#,
            "\n",
            r#"{"event":"terminate"}"#,
            "\n",
        );
        let reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));

        let shared_output = SharedOutput::new();
        let shared_clone = shared_output.clone();

        run_transfer_agent(reader, shared_output, store)
            .await
            .unwrap();

        let bytes = shared_clone.into_bytes();
        let lines = parse_output_lines(&bytes);
        assert!(!lines.is_empty(), "expected at least one output line");
        assert_eq!(lines[0], serde_json::json!({}));
    }

    #[tokio::test]
    async fn upload_and_download_round_trip() {
        let inner: Arc<dyn object_store::ObjectStore> =
            Arc::new(object_store::memory::InMemory::new());
        let policy = RetryPolicy {
            max_attempts: 2,
            base: std::time::Duration::from_millis(1),
            cap: std::time::Duration::from_millis(5),
        };
        let data = b"hello transfer agent";
        let oid = sha256_oid(data);
        let oid_hex = hex_encode(&oid);

        // Write the test file to a temp location.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), data).unwrap();
        let tmp_path = tmp.path().to_string_lossy().to_string();

        // Phase 1: upload.
        let upload_input = format!(
            r#"{{"event":"init","operation":"upload","remote":"origin","concurrent":true,"concurrenttransfers":1}}"#,
        ) + "\n"
            + &format!(
                r#"{{"event":"upload","oid":"{oid_hex}","size":{},"path":"{tmp_path}"}}"#,
                data.len(),
            )
            + "\n"
            + r#"{"event":"terminate"}"#
            + "\n";

        let reader = BufReader::new(Cursor::new(upload_input.into_bytes()));
        let shared_output = SharedOutput::new();
        let shared_clone = shared_output.clone();

        let upload_store = LfsObjectStore::new(
            Store::with_retry(Arc::clone(&inner), policy.clone()),
            "repo",
        );
        run_transfer_agent(reader, shared_output, upload_store)
            .await
            .unwrap();

        let bytes = shared_clone.into_bytes();
        let lines = parse_output_lines(&bytes);
        let upload_complete: Vec<_> = lines
            .iter()
            .filter(|v| v.get("event").and_then(|e| e.as_str()) == Some("complete"))
            .collect();
        assert!(!upload_complete.is_empty(), "expected upload complete");
        assert!(
            upload_complete[0].get("error").is_none(),
            "upload should succeed: {:?}",
            upload_complete[0],
        );

        // Phase 2: download (separate agent run, same backing store).
        let download_input = format!(
            r#"{{"event":"init","operation":"download","remote":"origin","concurrent":true,"concurrenttransfers":1}}"#,
        ) + "\n"
            + &format!(
                r#"{{"event":"download","oid":"{oid_hex}","size":{}}}"#,
                data.len(),
            )
            + "\n"
            + r#"{"event":"terminate"}"#
            + "\n";

        let reader = BufReader::new(Cursor::new(download_input.into_bytes()));
        let shared_output = SharedOutput::new();
        let shared_clone = shared_output.clone();

        let download_store =
            LfsObjectStore::new(Store::with_retry(Arc::clone(&inner), policy), "repo");
        run_transfer_agent(reader, shared_output, download_store)
            .await
            .unwrap();

        let bytes = shared_clone.into_bytes();
        let lines = parse_output_lines(&bytes);
        let download_complete: Vec<_> = lines
            .iter()
            .filter(|v| v.get("event").and_then(|e| e.as_str()) == Some("complete"))
            .collect();
        assert!(!download_complete.is_empty(), "expected download complete");
        assert!(
            download_complete[0].get("error").is_none(),
            "download should succeed: {:?}",
            download_complete[0],
        );
        let download_path = download_complete[0]
            .get("path")
            .and_then(|p| p.as_str())
            .expect("download complete should have a path");

        // Verify the downloaded file content.
        let downloaded = std::fs::read(download_path).unwrap();
        assert_eq!(downloaded, data);

        // Cleanup.
        let _ = std::fs::remove_file(download_path);
    }

    #[tokio::test]
    async fn error_response_for_missing_object() {
        let store = test_store();
        let oid_hex = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

        let input = format!(
            r#"{{"event":"init","operation":"download","remote":"origin","concurrent":true,"concurrenttransfers":2}}"#,
        ) + "\n"
            + &format!(r#"{{"event":"download","oid":"{oid_hex}","size":100}}"#,)
            + "\n"
            + r#"{"event":"terminate"}"#
            + "\n";

        let reader = BufReader::new(Cursor::new(input.into_bytes()));
        let shared_output = SharedOutput::new();
        let shared_clone = shared_output.clone();

        run_transfer_agent(reader, shared_output, store)
            .await
            .unwrap();

        let bytes = shared_clone.into_bytes();
        let lines = parse_output_lines(&bytes);

        let complete_events: Vec<_> = lines
            .iter()
            .filter(|v| v.get("event").and_then(|e| e.as_str()) == Some("complete"))
            .collect();

        assert!(!complete_events.is_empty(), "expected a complete event");
        let complete = complete_events[0];
        let error = complete.get("error").expect("expected error in complete");
        assert_eq!(error.get("code").and_then(|c| c.as_u64()), Some(2));
    }

    #[tokio::test]
    async fn progress_events_emitted_for_upload() {
        let store = test_store();
        let data = b"progress test data";
        let oid = sha256_oid(data);
        let oid_hex = hex_encode(&oid);

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), data).unwrap();
        let tmp_path = tmp.path().to_string_lossy().to_string();

        let input = format!(
            r#"{{"event":"init","operation":"upload","remote":"origin","concurrent":true,"concurrenttransfers":1}}"#,
        ) + "\n"
            + &format!(
                r#"{{"event":"upload","oid":"{oid_hex}","size":{},"path":"{tmp_path}"}}"#,
                data.len(),
            )
            + "\n"
            + r#"{"event":"terminate"}"#
            + "\n";

        let reader = BufReader::new(Cursor::new(input.into_bytes()));
        let shared_output = SharedOutput::new();
        let shared_clone = shared_output.clone();

        run_transfer_agent(reader, shared_output, store)
            .await
            .unwrap();

        let bytes = shared_clone.into_bytes();
        let lines = parse_output_lines(&bytes);

        let progress_events: Vec<_> = lines
            .iter()
            .filter(|v| v.get("event").and_then(|e| e.as_str()) == Some("progress"))
            .collect();

        // Should have at least 2 progress events (start + final).
        assert!(
            progress_events.len() >= 2,
            "expected at least 2 progress events, got {}: {progress_events:?}",
            progress_events.len(),
        );

        // Final progress should have bytesSoFar == size.
        let final_progress = progress_events.last().unwrap();
        assert_eq!(
            final_progress.get("bytesSoFar").and_then(|v| v.as_u64()),
            Some(data.len() as u64),
        );
    }

    /// End-to-end proof: an upload declared above the multipart
    /// threshold routes through `put_stream`, completes successfully,
    /// and the downloaded bytes match. This is the regression test
    /// for the "large LFS object loaded entirely into memory" gap —
    /// with the streaming path in place the agent should handle any
    /// file size bounded only by disk, not RAM.
    #[tokio::test]
    async fn large_upload_takes_streaming_path_and_succeeds() {
        // Build a file just over `MULTIPART_THRESHOLD` so the agent
        // chooses the streaming branch. The memory backend in the
        // test store happily holds the bytes — we're asserting
        // correctness of the streaming path, not production-sized
        // throughput.
        let inner: Arc<dyn object_store::ObjectStore> =
            Arc::new(object_store::memory::InMemory::new());
        let policy = RetryPolicy {
            max_attempts: 2,
            base: std::time::Duration::from_millis(1),
            cap: std::time::Duration::from_millis(5),
        };
        let size = (MULTIPART_THRESHOLD as usize) + 1024;
        let data = vec![0xA5u8; size];
        let oid = sha256_oid(&data);
        let oid_hex = hex_encode(&oid);

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &data).unwrap();
        let tmp_path = tmp.path().to_string_lossy().to_string();

        // Upload the large file.
        let upload_input = format!(
            r#"{{"event":"init","operation":"upload","remote":"origin","concurrent":true,"concurrenttransfers":1}}"#,
        ) + "\n"
            + &format!(
                r#"{{"event":"upload","oid":"{oid_hex}","size":{size},"path":"{tmp_path}"}}"#,
            )
            + "\n"
            + r#"{"event":"terminate"}"#
            + "\n";

        let reader = BufReader::new(Cursor::new(upload_input.into_bytes()));
        let shared_output = SharedOutput::new();
        let shared_clone = shared_output.clone();

        let upload_store = LfsObjectStore::new(
            Store::with_retry(Arc::clone(&inner), policy.clone()),
            "repo",
        );
        run_transfer_agent(reader, shared_output, upload_store)
            .await
            .unwrap();

        let bytes = shared_clone.into_bytes();
        let lines = parse_output_lines(&bytes);
        let complete_events: Vec<_> = lines
            .iter()
            .filter(|v| v.get("event").and_then(|e| e.as_str()) == Some("complete"))
            .collect();
        assert_eq!(complete_events.len(), 1, "expected one complete event");
        assert!(
            complete_events[0].get("error").is_none(),
            "streaming upload should succeed: {:?}",
            complete_events[0],
        );

        // And the bytes are actually on the remote, intact.
        let verify_store =
            LfsObjectStore::new(Store::with_retry(Arc::clone(&inner), policy), "repo");
        assert!(verify_store.exists(&oid).await.unwrap());
        let downloaded = verify_store.get(&oid).await.unwrap();
        assert_eq!(downloaded.len(), size);
        assert_eq!(Sha256::digest(&downloaded).as_slice(), oid.as_slice());
    }

    // --- SharedOutput helper for capturing Write output in tests ---

    /// A thread-safe `Write` implementation that captures all written bytes.
    #[derive(Clone)]
    struct SharedOutput {
        inner: Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl SharedOutput {
        fn new() -> Self {
            Self {
                inner: Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }

        fn into_bytes(self) -> Vec<u8> {
            let guard = self.inner.lock().unwrap();
            guard.clone()
        }
    }

    impl Write for SharedOutput {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut inner = self.inner.lock().unwrap();
            inner.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // --- Transfer agent protocol compliance tests ---

    #[tokio::test]
    async fn upload_before_init_returns_protocol_error() {
        let store = test_store();
        let data = b"test";
        let oid = sha256_oid(data);
        let oid_hex = hex_encode(&oid);

        // Upload event before init — should return LfsTransferProtocol error.
        let input =
            format!(r#"{{"event":"upload","oid":"{oid_hex}","size":4,"path":"/tmp/test"}}"#,)
                + "\n";

        let reader = BufReader::new(Cursor::new(input.into_bytes()));
        let shared_output = SharedOutput::new();

        let err = run_transfer_agent(reader, shared_output, store)
            .await
            .expect_err("upload before init must error");
        match err {
            CrabError::LfsTransferProtocol(msg) => {
                assert!(
                    msg.contains("before init"),
                    "expected 'before init' message, got: {msg}"
                );
            }
            other => panic!("expected LfsTransferProtocol, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn concurrent_uploads_complete_independently() {
        let store = test_store();

        // Create two test files.
        let data1 = b"first file content";
        let data2 = b"second file content";
        let oid1 = sha256_oid(data1);
        let oid2 = sha256_oid(data2);
        let oid1_hex = hex_encode(&oid1);
        let oid2_hex = hex_encode(&oid2);

        let tmp1 = tempfile::NamedTempFile::new().unwrap();
        let tmp2 = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp1.path(), data1).unwrap();
        std::fs::write(tmp2.path(), data2).unwrap();
        let path1 = tmp1.path().to_string_lossy().to_string();
        let path2 = tmp2.path().to_string_lossy().to_string();

        let input = format!(
            r#"{{"event":"init","operation":"upload","remote":"origin","concurrenttransfers":4}}"#,
        ) + "\n"
            + &format!(
                r#"{{"event":"upload","oid":"{oid1_hex}","size":{},"path":"{path1}"}}"#,
                data1.len()
            )
            + "\n"
            + &format!(
                r#"{{"event":"upload","oid":"{oid2_hex}","size":{},"path":"{path2}"}}"#,
                data2.len()
            )
            + "\n"
            + r#"{"event":"terminate"}"#
            + "\n";

        let reader = BufReader::new(Cursor::new(input.into_bytes()));
        let shared_output = SharedOutput::new();
        let shared_clone = shared_output.clone();

        run_transfer_agent(reader, shared_output, store)
            .await
            .unwrap();

        let bytes = shared_clone.into_bytes();
        let lines = parse_output_lines(&bytes);

        let complete_events: Vec<_> = lines
            .iter()
            .filter(|v| v.get("event").and_then(|e| e.as_str()) == Some("complete"))
            .collect();

        assert_eq!(complete_events.len(), 2, "expected 2 complete events");
        for event in &complete_events {
            assert!(event.get("error").is_none(), "expected no error: {event:?}");
        }
    }

    #[tokio::test]
    async fn malformed_json_line_is_skipped() {
        let store = test_store();
        let data = b"valid content";
        let oid = sha256_oid(data);
        let oid_hex = hex_encode(&oid);

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), data).unwrap();
        let path = tmp.path().to_string_lossy().to_string();

        // Malformed line between init and a valid upload — should be skipped.
        let input = format!(
            r#"{{"event":"init","operation":"upload","remote":"origin","concurrenttransfers":2}}"#,
        ) + "\n"
            + "{broken json!!!\n"
            + &format!(
                r#"{{"event":"upload","oid":"{oid_hex}","size":{},"path":"{path}"}}"#,
                data.len()
            )
            + "\n"
            + r#"{"event":"terminate"}"#
            + "\n";

        let reader = BufReader::new(Cursor::new(input.into_bytes()));
        let shared_output = SharedOutput::new();
        let shared_clone = shared_output.clone();

        run_transfer_agent(reader, shared_output, store)
            .await
            .unwrap();

        let bytes = shared_clone.into_bytes();
        let lines = parse_output_lines(&bytes);

        let complete_events: Vec<_> = lines
            .iter()
            .filter(|v| v.get("event").and_then(|e| e.as_str()) == Some("complete"))
            .collect();
        assert!(
            !complete_events.is_empty(),
            "expected at least one complete event despite malformed line"
        );
        assert!(
            complete_events[0].get("error").is_none(),
            "upload should succeed despite malformed line"
        );
    }

    #[tokio::test]
    async fn invalid_oid_triggers_error_complete() {
        let store = test_store();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"content").unwrap();
        let path = tmp.path().to_string_lossy().to_string();

        // OID with wrong length (not 64 chars).
        let bad_oid = "tooshort";

        let input = format!(
            r#"{{"event":"init","operation":"upload","remote":"origin","concurrenttransfers":1}}"#,
        ) + "\n"
            + &format!(r#"{{"event":"upload","oid":"{bad_oid}","size":7,"path":"{path}"}}"#)
            + "\n"
            + r#"{"event":"terminate"}"#
            + "\n";

        let reader = BufReader::new(Cursor::new(input.into_bytes()));
        let shared_output = SharedOutput::new();
        let shared_clone = shared_output.clone();

        run_transfer_agent(reader, shared_output, store)
            .await
            .unwrap();

        let bytes = shared_clone.into_bytes();
        let lines = parse_output_lines(&bytes);

        let complete_events: Vec<_> = lines
            .iter()
            .filter(|v| v.get("event").and_then(|e| e.as_str()) == Some("complete"))
            .collect();
        assert!(
            !complete_events.is_empty(),
            "expected complete event for invalid OID"
        );
        let error = complete_events[0]
            .get("error")
            .expect("expected error for invalid OID");
        assert_eq!(
            error.get("code").and_then(|c| c.as_u64()),
            Some(1),
            "expected error code 1 for invalid OID"
        );
    }

    #[tokio::test]
    async fn non_hex_oid_triggers_error_complete() {
        let store = test_store();
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"content").unwrap();
        let path = tmp.path().to_string_lossy().to_string();

        // 64 chars but contains non-hex characters.
        let bad_oid = "gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg";

        let input = format!(
            r#"{{"event":"init","operation":"upload","remote":"origin","concurrenttransfers":1}}"#,
        ) + "\n"
            + &format!(r#"{{"event":"upload","oid":"{bad_oid}","size":7,"path":"{path}"}}"#)
            + "\n"
            + r#"{"event":"terminate"}"#
            + "\n";

        let reader = BufReader::new(Cursor::new(input.into_bytes()));
        let shared_output = SharedOutput::new();
        let shared_clone = shared_output.clone();

        run_transfer_agent(reader, shared_output, store)
            .await
            .unwrap();

        let bytes = shared_clone.into_bytes();
        let lines = parse_output_lines(&bytes);

        let complete_events: Vec<_> = lines
            .iter()
            .filter(|v| v.get("event").and_then(|e| e.as_str()) == Some("complete"))
            .collect();
        assert!(
            !complete_events.is_empty(),
            "expected complete event for non-hex OID"
        );
        let error = complete_events[0]
            .get("error")
            .expect("expected error for non-hex OID");
        assert_eq!(
            error.get("code").and_then(|c| c.as_u64()),
            Some(1),
            "expected error code 1 for non-hex OID"
        );
    }
}
