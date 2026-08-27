//! Standalone LFS transfer agent protocol.
//!
//! Implements the Git LFS custom/standalone transfer agent protocol using
//! JSON lines over stdin/stdout. Handles `init`, `upload`, `download`, and
//! `terminate` events with bounded concurrent transfers and progress reporting.
//!
//! Transfers stream through bounded object-store paths, retry transient
//! failures using the resolved Git LFS policy, and use unique temporary
//! download paths so concurrent worktrees and agent processes do not collide.

use std::future::Future;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::task::JoinSet;

use crate::core::error::{CrabError, Result};
use crate::lfs::coordinator::{
    DEFAULT_IN_FLIGHT_BYTES, TransferCoordinator, TransferOutcome, TransferPolicy, TransferRequest,
};
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
        remote: String,
        #[serde(default = "default_concurrent")]
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

fn default_concurrent() -> bool {
    true
}

fn effective_concurrency(concurrent: bool, requested: u32) -> u32 {
    if concurrent {
        // Git LFS starts one process per concurrent transfer in this mode.
        // Keeping each process serial prevents the installed default from
        // multiplying the configured concurrency by the process count.
        1
    } else {
        requested.clamp(1, 100)
    }
}

/// Init response sent back to the LFS client.
#[derive(Debug, Serialize)]
struct InitResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<TransferError>,
}

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

fn init_error_response(error: &CrabError) -> InitResponse {
    InitResponse {
        error: Some(TransferError {
            code: 32,
            message: error.to_string(),
        }),
    }
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

/// Transfer retry and backoff settings resolved from Git LFS configuration.
#[derive(Debug, Clone)]
pub struct TransferAgentConfig {
    /// Number of retries after the initial attempt.
    pub max_retries: u32,
    /// Maximum delay between retry attempts, in seconds.
    pub max_retry_delay: u32,
    /// Directory where completed downloads are staged for Git LFS.
    pub temp_dir: PathBuf,
    /// Maximum number of object transfers admitted at once.
    pub concurrent_transfers: u32,
    /// Maximum aggregate transfer bandwidth in bytes per second (zero is unlimited).
    pub max_bandwidth: u64,
    /// Byte budget shared by admitted object transfers.
    pub in_flight_bytes: u64,
}

impl Default for TransferAgentConfig {
    fn default() -> Self {
        Self {
            max_retries: 8,
            max_retry_delay: 10,
            temp_dir: lfs_tmp_dir(),
            concurrent_transfers: 8,
            max_bandwidth: 0,
            in_flight_bytes: DEFAULT_IN_FLIGHT_BYTES,
        }
    }
}

impl TransferAgentConfig {
    fn transfer_policy(&self) -> TransferPolicy {
        TransferPolicy {
            max_concurrency: self.concurrent_transfers.max(1) as usize,
            max_retries: self.max_retries,
            max_retry_delay: self.max_retry_delay,
            skip_download_errors: false,
            max_bandwidth: self.max_bandwidth,
            in_flight_bytes: self.in_flight_bytes.max(1),
        }
    }
}

// ---------------------------------------------------------------------------
// Transfer agent entry point
// ---------------------------------------------------------------------------

/// Run the standalone LFS transfer agent protocol loop.
///
/// Reads JSON-line events from `input` (stdin), resolves the store and policy
/// from the init event, dispatches bounded uploads and downloads, and writes
/// JSON-line responses to `output` (stdout).
///
/// # Errors
///
/// Returns [`CrabError::LfsTransferProtocol`] on malformed input or invalid
/// protocol ordering. Individual transfer failures are reported as `complete`
/// events with an `error` object — they do not terminate the agent.
pub async fn run_transfer_agent<R, W>(input: R, output: W, store: LfsObjectStore) -> Result<()>
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    run_transfer_agent_with_resolver(input, output, move |_, _| async move {
        Ok((store, TransferAgentConfig::default()))
    })
    .await
}

/// Run the transfer agent with a store resolver selected by the Git LFS init event.
pub async fn run_transfer_agent_with_resolver<R, W, F, Fut>(
    input: R,
    output: W,
    resolver: F,
) -> Result<()>
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
    F: FnOnce(String, Option<String>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(LfsObjectStore, TransferAgentConfig)>> + Send,
{
    let output = Arc::new(Mutex::new(output));

    // Default concurrency; overridden by the init event.
    let mut initialized = false;
    let mut store: Option<Arc<LfsObjectStore>> = None;
    let mut config: Option<Arc<TransferAgentConfig>> = None;
    let mut resolver = Some(resolver);

    // Read lines from stdin synchronously in a blocking task so we don't
    // block the tokio runtime.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<std::result::Result<InboundEvent, String>>(64);

    let reader_handle = tokio::task::spawn_blocking(move || {
        for line_result in input.lines() {
            let line = match line_result {
                Ok(l) => l,
                Err(e) => {
                    let _ = tx.blocking_send(Err(format!("stdin read error: {e}")));
                    break;
                }
            };

            if line.trim().is_empty() {
                continue;
            }

            match serde_json::from_str::<InboundEvent>(&line) {
                Ok(event) => {
                    let terminate = matches!(&event, InboundEvent::Terminate);
                    if tx.blocking_send(Ok(event)).is_err() || terminate {
                        // Do not attempt another blocking read after the
                        // protocol's terminal event. This is what lets a
                        // real stdin pipe remain open without leaking a
                        // blocked reader thread during shutdown.
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.blocking_send(Err(format!("malformed transfer event: {e}")));
                    break;
                }
            }
        }
    });

    let mut coordinator: Option<Arc<TransferCoordinator>> = None;
    let mut join_set = JoinSet::new();
    let mut loop_error = None;

    while let Some(event) = rx.recv().await {
        while let Some(join_result) = join_set.try_join_next() {
            if let Err(error) = join_result {
                tracing::error!(error = %error, "transfer task failed");
            }
        }

        let event = match event {
            Ok(event) => event,
            Err(error) => {
                loop_error = Some(CrabError::LfsTransferProtocol(error));
                break;
            }
        };
        match event {
            InboundEvent::Init {
                operation,
                remote,
                concurrent,
                concurrenttransfers,
                ..
            } => {
                if initialized {
                    let error = CrabError::LfsTransferProtocol("duplicate init event".into());
                    loop_error = Some(
                        match write_json_line(&output, &init_error_response(&error)).await {
                            Ok(()) => error,
                            Err(send_error) => send_error,
                        },
                    );
                    break;
                }
                if !matches!(operation.as_str(), "upload" | "download") {
                    let error = CrabError::LfsTransferProtocol(format!(
                        "unsupported transfer operation: {operation}"
                    ));
                    loop_error = Some(
                        match write_json_line(&output, &init_error_response(&error)).await {
                            Ok(()) => error,
                            Err(send_error) => send_error,
                        },
                    );
                    break;
                }
                let Some(resolver) = resolver.take() else {
                    let error = CrabError::LfsTransferProtocol(
                        "transfer store resolver already consumed".into(),
                    );
                    loop_error = Some(
                        match write_json_line(&output, &init_error_response(&error)).await {
                            Ok(()) => error,
                            Err(send_error) => send_error,
                        },
                    );
                    break;
                };
                let remote = (!remote.trim().is_empty()).then_some(remote);
                let (resolved_store, resolved_config) =
                    match resolver(operation.clone(), remote).await {
                        Ok(result) => result,
                        Err(error) => {
                            loop_error = Some(
                                match write_json_line(&output, &init_error_response(&error)).await {
                                    Ok(()) => error,
                                    Err(send_error) => send_error,
                                },
                            );
                            break;
                        }
                    };
                let concurrency = effective_concurrency(concurrent, concurrenttransfers);
                let mut resolved_config = resolved_config;
                resolved_config.concurrent_transfers = concurrency;
                initialized = true;
                store = Some(Arc::new(resolved_store));
                let policy = resolved_config.transfer_policy();
                config = Some(Arc::new(resolved_config));
                coordinator = Some(Arc::new(TransferCoordinator::new(policy)));

                tracing::debug!(
                    %operation,
                    concurrency,
                    "transfer agent initialized",
                );

                // Respond with empty JSON object.
                write_json_line(&output, &InitResponse { error: None }).await?;
            }

            InboundEvent::Upload { oid, size, path } => {
                if !initialized {
                    loop_error = Some(CrabError::LfsTransferProtocol(
                        "upload event before init".into(),
                    ));
                    break;
                }

                let Some(coordinator) = coordinator.clone() else {
                    loop_error = Some(CrabError::LfsTransferProtocol(
                        "upload event before init".into(),
                    ));
                    break;
                };
                let Some(store) = store.clone() else {
                    loop_error = Some(CrabError::LfsTransferProtocol(
                        "upload event before init".into(),
                    ));
                    break;
                };
                let request = match parse_oid_hex(&oid) {
                    Ok(oid_bytes) => TransferRequest {
                        oid: oid_bytes,
                        size,
                    },
                    Err(error) => {
                        let output = Arc::clone(&output);
                        join_set.spawn(async move {
                            if let Err(send_error) = write_json_line(
                                &output,
                                &CompleteEvent {
                                    event: "complete",
                                    oid,
                                    path: None,
                                    error: Some(TransferError {
                                        code: error_code(&error),
                                        message: error.to_string(),
                                    }),
                                },
                            )
                            .await
                            {
                                tracing::error!(error = %send_error, "upload failed to send response");
                            }
                        });
                        continue;
                    }
                };
                let permit = match coordinator.admit(request).await {
                    Ok(permit) => permit,
                    Err(error) => {
                        loop_error = Some(error);
                        break;
                    }
                };
                let output = Arc::clone(&output);

                join_set.spawn(async move {
                    let result = handle_upload(
                        &coordinator,
                        request,
                        permit,
                        &store,
                        &oid,
                        size,
                        &path,
                        &output,
                    )
                    .await;
                    if let Err(e) = result {
                        tracing::error!(oid = %oid, error = %e, "upload failed to send response");
                    }
                });
            }

            InboundEvent::Download { oid, size } => {
                if !initialized {
                    loop_error = Some(CrabError::LfsTransferProtocol(
                        "download event before init".into(),
                    ));
                    break;
                }

                let Some(coordinator) = coordinator.clone() else {
                    loop_error = Some(CrabError::LfsTransferProtocol(
                        "download event before init".into(),
                    ));
                    break;
                };
                let Some(store) = store.clone() else {
                    loop_error = Some(CrabError::LfsTransferProtocol(
                        "download event before init".into(),
                    ));
                    break;
                };
                let Some(config) = config.clone() else {
                    loop_error = Some(CrabError::LfsTransferProtocol(
                        "download event before init".into(),
                    ));
                    break;
                };
                let request = match parse_oid_hex(&oid) {
                    Ok(oid_bytes) => TransferRequest {
                        oid: oid_bytes,
                        size,
                    },
                    Err(error) => {
                        let output = Arc::clone(&output);
                        join_set.spawn(async move {
                            if let Err(send_error) = write_json_line(
                                &output,
                                &CompleteEvent {
                                    event: "complete",
                                    oid,
                                    path: None,
                                    error: Some(TransferError {
                                        code: error_code(&error),
                                        message: error.to_string(),
                                    }),
                                },
                            )
                            .await
                            {
                                tracing::error!(error = %send_error, "download failed to send response");
                            }
                        });
                        continue;
                    }
                };
                let permit = match coordinator.admit(request).await {
                    Ok(permit) => permit,
                    Err(error) => {
                        loop_error = Some(error);
                        break;
                    }
                };
                let output = Arc::clone(&output);

                join_set.spawn(async move {
                    let result = handle_download(
                        &coordinator,
                        request,
                        permit,
                        &store,
                        &oid,
                        size,
                        &output,
                        &config.temp_dir,
                    )
                    .await;
                    if let Err(e) = result {
                        tracing::error!(oid = %oid, error = %e, "download failed to send response");
                    }
                });
            }

            InboundEvent::Terminate => {
                if !initialized {
                    loop_error = Some(CrabError::LfsTransferProtocol(
                        "terminate event before init".into(),
                    ));
                    break;
                }
                tracing::debug!("received terminate, waiting for in-flight transfers");
                break;
            }
        }
    }

    // Wait for all in-flight transfers to complete.
    while let Some(result) = join_set.join_next().await {
        if let Err(error) = result {
            tracing::error!(error = %error, "transfer task failed");
        }
    }

    // Drop the reader task.
    reader_handle.abort();
    let _ = reader_handle.await;

    match loop_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Upload handler
// ---------------------------------------------------------------------------

/// Handle one upload after admission by the canonical coordinator.
async fn handle_upload<W: Write + Send>(
    coordinator: &TransferCoordinator,
    request: TransferRequest,
    permit: crate::lfs::coordinator::TransferPermit,
    store: &Arc<LfsObjectStore>,
    oid: &str,
    size: u64,
    path: &str,
    output: &Arc<Mutex<W>>,
) -> Result<()> {
    let first_progress = Arc::new(AtomicBool::new(true));
    let store = Arc::clone(store);
    let oid = oid.to_owned();
    let path = PathBuf::from(path);
    let output_for_transfer = Arc::clone(output);
    let first_progress_for_transfer = Arc::clone(&first_progress);
    let response_oid = oid.clone();
    let result = coordinator
        .run_admitted(request, permit, move |cancel| {
            let store = Arc::clone(&store);
            let oid = oid.clone();
            let path = path.clone();
            let output = Arc::clone(&output_for_transfer);
            let emit_initial_progress = first_progress_for_transfer.swap(false, Ordering::AcqRel);
            async move {
                if cancel.is_cancelled() {
                    return Err(CrabError::Cancelled);
                }
                do_upload(&store, &oid, size, &path, &output, emit_initial_progress)
                    .await
                    .map(|()| TransferOutcome::Transferred)
            }
        })
        .await;
    let response = match result {
        Ok(_) => CompleteEvent {
            event: "complete",
            oid: response_oid,
            path: None,
            error: None,
        },
        Err(error) => CompleteEvent {
            event: "complete",
            oid: response_oid,
            path: None,
            error: Some(TransferError {
                code: error_code(&error),
                message: error.to_string(),
            }),
        },
    };
    write_json_line(output, &response).await
}

/// Core upload logic separated from error-response plumbing.
///
/// Streams and verifies the local file for every object size. Peak memory is
/// bounded by the object-store multipart pipeline instead of file size.
///
async fn do_upload<W: Write + Send>(
    store: &LfsObjectStore,
    oid: &str,
    size: u64,
    path: &std::path::Path,
    output: &Arc<Mutex<W>>,
    emit_initial_progress: bool,
) -> Result<()> {
    let actual_size = tokio::fs::metadata(path)
        .await
        .map_err(CrabError::Io)?
        .len();
    if actual_size != size {
        return Err(CrabError::LfsObjectCorrupt {
            oid: oid.to_owned(),
        });
    }

    if emit_initial_progress {
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
    }

    let oid_bytes = parse_oid_hex(oid)?;
    store
        .put_stream_with_size(&oid_bytes, Some(size), path)
        .await
        .map_err(CrabError::from)?;

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

/// Handle one download after admission by the canonical coordinator.
async fn handle_download<W: Write + Send>(
    coordinator: &TransferCoordinator,
    request: TransferRequest,
    permit: crate::lfs::coordinator::TransferPermit,
    store: &Arc<LfsObjectStore>,
    oid: &str,
    size: u64,
    output: &Arc<Mutex<W>>,
    temp_dir: &std::path::Path,
) -> Result<()> {
    let first_progress = Arc::new(AtomicBool::new(true));
    let store = Arc::clone(store);
    let oid = oid.to_owned();
    let temp_dir = temp_dir.to_owned();
    let output_for_transfer = Arc::clone(output);
    let first_progress_for_transfer = Arc::clone(&first_progress);
    let downloaded_path = Arc::new(Mutex::new(None));
    let downloaded_path_for_transfer = Arc::clone(&downloaded_path);
    let response_oid = oid.clone();
    let result = coordinator
        .run_admitted(request, permit, move |cancel| {
            let store = Arc::clone(&store);
            let oid = oid.clone();
            let temp_dir = temp_dir.clone();
            let output = Arc::clone(&output_for_transfer);
            let downloaded_path = Arc::clone(&downloaded_path_for_transfer);
            let emit_initial_progress = first_progress_for_transfer.swap(false, Ordering::AcqRel);
            async move {
                if cancel.is_cancelled() {
                    return Err(CrabError::Cancelled);
                }
                let path = do_download(
                    &store,
                    &oid,
                    size,
                    &output,
                    &temp_dir,
                    emit_initial_progress,
                )
                .await?;
                *downloaded_path.lock().await = Some(path);
                Ok(TransferOutcome::Transferred)
            }
        })
        .await;
    let response = match result {
        Ok(_) => match downloaded_path.lock().await.take() {
            Some(temp_path) => CompleteEvent {
                event: "complete",
                oid: response_oid,
                path: Some(temp_path),
                error: None,
            },
            None => CompleteEvent {
                event: "complete",
                oid: response_oid,
                path: None,
                error: Some(TransferError {
                    code: 1,
                    message: "download completed without a staged path".to_owned(),
                }),
            },
        },
        Err(error) => CompleteEvent {
            event: "complete",
            oid: response_oid,
            path: None,
            error: Some(TransferError {
                code: error_code(&error),
                message: error.to_string(),
            }),
        },
    };
    let staged_path = response.path.clone();
    match write_json_line(output, &response).await {
        Ok(()) => Ok(()),
        Err(error) => {
            if let Some(path) = staged_path {
                let _ = tokio::fs::remove_file(path).await;
            }
            Err(error)
        }
    }
}

/// Core download logic separated from error-response plumbing.
///
/// The object store streams directly into a unique temporary file and hashes
/// the bytes before returning the path to Git LFS. A failed attempt is removed
/// before the coordinator retries it, so no partial path reaches the client.
async fn do_download<W: Write + Send>(
    store: &LfsObjectStore,
    oid: &str,
    size: u64,
    output: &Arc<Mutex<W>>,
    temp_dir: &std::path::Path,
    emit_initial_progress: bool,
) -> Result<String> {
    let oid_bytes = parse_oid_hex(oid)?;

    tokio::fs::create_dir_all(temp_dir)
        .await
        .map_err(CrabError::Io)?;
    let temp = tempfile::Builder::new()
        .prefix("crab-lfs-transfer-")
        .tempfile_in(temp_dir)
        .map_err(CrabError::Io)?;
    let temp_path = temp
        .into_temp_path()
        .keep()
        .map_err(|error| CrabError::Io(error.error))?;

    if emit_initial_progress {
        if let Err(error) = write_json_line(
            output,
            &ProgressEvent {
                event: "progress",
                oid: oid.to_owned(),
                bytes_so_far: 0,
                bytes_since_last: 0,
            },
        )
        .await
        {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(error);
        }
    }

    if let Err(error) = store
        .download_to_file(&oid_bytes, size, &temp_path)
        .await
        .map_err(CrabError::from)
    {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(error);
    }

    // Emit final progress event.
    if let Err(error) = write_json_line(
        output,
        &ProgressEvent {
            event: "progress",
            oid: oid.to_owned(),
            bytes_so_far: size,
            bytes_since_last: size,
        },
    )
    .await
    {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(error);
    }

    Ok(temp_path.to_string_lossy().into_owned())
}

/// Returns the `.git/lfs/tmp/` directory path for completed transfer files.
///
fn lfs_tmp_dir() -> PathBuf {
    if let Ok(cwd) = std::env::current_dir()
        && let Ok(lfs_dir) = crate::lfs::config::LfsConfig::resolve_storage_dir(&cwd)
    {
        return lfs_dir.join("tmp");
    }

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

    #[test]
    fn custom_transfer_concurrency_has_one_owner() {
        assert_eq!(effective_concurrency(true, 8), 1);
        assert_eq!(effective_concurrency(false, 0), 1);
        assert_eq!(effective_concurrency(false, 8), 8);
        assert_eq!(effective_concurrency(false, 101), 100);
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

        let reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
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

        let reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
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
        let mut previous = 0;
        for event in progress_events {
            let current = event
                .get("bytesSoFar")
                .and_then(|value| value.as_u64())
                .expect("progress must contain bytesSoFar");
            assert!(current >= previous, "progress must be monotonic");
            previous = current;
        }
    }

    /// End-to-end proof that a multi-part upload routes through the bounded
    /// streaming path and the downloaded bytes match.
    #[tokio::test]
    async fn large_upload_takes_streaming_path_and_succeeds() {
        // The memory backend happily holds the bytes; this asserts
        // correctness of the streaming path, not production throughput.
        let inner: Arc<dyn object_store::ObjectStore> =
            Arc::new(object_store::memory::InMemory::new());
        let policy = RetryPolicy {
            max_attempts: 2,
            base: std::time::Duration::from_millis(1),
            cap: std::time::Duration::from_millis(5),
        };
        let size = (8 * 1024 * 1024) + 1024;
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
    async fn malformed_json_line_is_fatal() {
        let store = test_store();
        let input = concat!(
            r#"{"event":"init","operation":"upload","remote":"origin","concurrenttransfers":2}"#,
            "\n",
            "{broken json!!!\n",
        );

        let reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
        let shared_output = SharedOutput::new();

        let error = run_transfer_agent(reader, shared_output, store)
            .await
            .expect_err("malformed input must terminate the protocol");
        assert!(
            matches!(error, CrabError::LfsTransferProtocol(message) if message.contains("malformed transfer event"))
        );
    }

    #[tokio::test]
    async fn init_resolver_receives_operation_and_remote() {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let resolver_seen = Arc::clone(&seen);
        let input = concat!(
            r#"{"event":"init","operation":"download","remote":"archive","concurrent":false}"#,
            "\n",
            r#"{"event":"terminate"}"#,
            "\n",
        );

        let reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
        let output = SharedOutput::new();
        run_transfer_agent_with_resolver(reader, output, move |operation, remote| {
            let resolver_seen = Arc::clone(&resolver_seen);
            async move {
                *resolver_seen.lock().unwrap() = Some((operation, remote));
                Ok((test_store(), TransferAgentConfig::default()))
            }
        })
        .await
        .unwrap();

        assert_eq!(
            seen.lock().unwrap().as_ref(),
            Some(&("download".to_owned(), Some("archive".to_owned())))
        );
    }

    #[tokio::test]
    async fn init_resolver_failure_is_reported_to_git_lfs() {
        let input = concat!(
            r#"{"event":"init","operation":"download","remote":"origin"}"#,
            "\n",
        );
        let reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
        let output = SharedOutput::new();
        let output_clone = output.clone();

        let error = run_transfer_agent_with_resolver(reader, output, |_operation, _remote| async {
            Err::<(LfsObjectStore, TransferAgentConfig), _>(CrabError::Configuration {
                key: "lfs.remote".to_owned(),
                origin: "credentials unavailable".to_owned(),
            })
        })
        .await
        .expect_err("resolver failure must terminate the protocol");

        assert!(matches!(error, CrabError::Configuration { .. }));
        assert_eq!(
            parse_output_lines(&output_clone.into_bytes()),
            vec![serde_json::json!({
                "error": {
                    "code": 32,
                    "message": "configuration error [CRAB-E0050] in credentials unavailable: lfs.remote"
                }
            })]
        );
    }

    #[tokio::test]
    async fn duplicate_init_is_fatal() {
        let input = concat!(
            r#"{"event":"init","operation":"download"}"#,
            "\n",
            r#"{"event":"init","operation":"download"}"#,
            "\n",
        );
        let reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
        let error = run_transfer_agent(reader, SharedOutput::new(), test_store())
            .await
            .expect_err("duplicate init must terminate the protocol");
        assert!(
            matches!(error, CrabError::LfsTransferProtocol(message) if message.contains("duplicate init"))
        );
    }

    #[tokio::test]
    async fn terminate_before_init_is_fatal() {
        let input = r#"{"event":"terminate"}
"#;
        let reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
        let error = run_transfer_agent(reader, SharedOutput::new(), test_store())
            .await
            .expect_err("terminate before init must terminate the protocol");
        assert!(
            matches!(error, CrabError::LfsTransferProtocol(message) if message.contains("before init"))
        );
    }

    #[tokio::test]
    async fn unknown_event_is_a_fatal_protocol_error() {
        let input = concat!(
            r#"{"event":"init","operation":"upload"}"#,
            "\n",
            r#"{"event":"not-a-transfer-event"}"#,
            "\n",
        );
        let reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
        let error = run_transfer_agent(reader, SharedOutput::new(), test_store())
            .await
            .expect_err("unknown event must terminate the protocol");
        assert!(matches!(
            error,
            CrabError::LfsTransferProtocol(message) if message.contains("malformed transfer event")
        ));
    }

    #[tokio::test]
    async fn declared_size_mismatch_is_reported_for_upload() {
        let store = test_store();
        let data = b"seven bytes";
        let oid = sha256_oid(data);
        let oid_hex = hex_encode(&oid);
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), data).unwrap();
        let input = format!(
            r#"{{"event":"init","operation":"upload"}}
{{"event":"upload","oid":"{oid_hex}","size":{},"path":"{}"}}
{{"event":"terminate"}}
"#,
            data.len() + 1,
            file.path().display()
        );
        let reader = BufReader::new(Cursor::new(input.into_bytes()));
        let output = SharedOutput::new();
        let output_copy = output.clone();
        run_transfer_agent(reader, output, store).await.unwrap();
        let completes: Vec<_> = parse_output_lines(&output_copy.into_bytes())
            .into_iter()
            .filter(|value| value.get("event").and_then(|event| event.as_str()) == Some("complete"))
            .collect();
        assert_eq!(completes.len(), 1);
        assert!(completes[0].get("error").is_some());
    }

    #[tokio::test]
    async fn missing_upload_path_is_reported_without_panicking() {
        let data = b"content";
        let oid_hex = hex_encode(&sha256_oid(data));
        let input = format!(
            r#"{{"event":"init","operation":"upload"}}
{{"event":"upload","oid":"{oid_hex}","size":{},"path":"/path/that/does/not/exist"}}
{{"event":"terminate"}}
"#,
            data.len()
        );
        let reader = BufReader::new(Cursor::new(input.into_bytes()));
        let output = SharedOutput::new();
        let output_copy = output.clone();
        run_transfer_agent(reader, output, test_store())
            .await
            .unwrap();
        let completes: Vec<_> = parse_output_lines(&output_copy.into_bytes())
            .into_iter()
            .filter(|value| value.get("event").and_then(|event| event.as_str()) == Some("complete"))
            .collect();
        assert_eq!(completes.len(), 1);
        assert!(completes[0].get("error").is_some());
    }

    #[tokio::test]
    async fn duplicate_oid_requests_each_receive_one_completion() {
        let data = b"duplicate request";
        let oid_hex = hex_encode(&sha256_oid(data));
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), data).unwrap();
        let input = format!(
            r#"{{"event":"init","operation":"upload","concurrenttransfers":1}}
{{"event":"upload","oid":"{oid_hex}","size":{},"path":"{}"}}
{{"event":"upload","oid":"{oid_hex}","size":{},"path":"{}"}}
{{"event":"terminate"}}
"#,
            data.len(),
            file.path().display(),
            data.len(),
            file.path().display()
        );
        let reader = BufReader::new(Cursor::new(input.into_bytes()));
        let output = SharedOutput::new();
        let output_copy = output.clone();
        run_transfer_agent(reader, output, test_store())
            .await
            .unwrap();
        let completes: Vec<_> = parse_output_lines(&output_copy.into_bytes())
            .into_iter()
            .filter(|value| value.get("event").and_then(|event| event.as_str()) == Some("complete"))
            .collect();
        assert_eq!(completes.len(), 2);
        assert!(completes.iter().all(|value| value.get("error").is_none()));
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
