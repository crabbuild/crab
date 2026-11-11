//! Crab Desktop sidecar.
//!
//! A tiny JSON-RPC 2.0 server over stdio. The Electron main process
//! spawns one of these per application launch and funnels every
//! renderer IPC call through it. Reads go through `crab-sdk`;
//! writes shell out to the `crab` CLI and stream progress back as
//! JSON-RPC notifications.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod audit;
mod batch_git_status;
mod cache;
mod cancel;
mod conflicts;
mod credential_store;
mod experiments;
mod file_publications;
mod forge;
#[cfg(unix)]
mod git_active_apply;
mod git_branches;
mod git_commit;
mod git_hunks;
mod git_ref_merge;
mod git_remote;
mod git_worktree;
mod handlers;
mod indexer;
mod managed_service;
#[cfg(unix)]
mod native_process;
mod notebook_strip;
mod ops;
mod preview;
mod repository;
mod rpc;
mod shellout;
mod storage_auth;
mod system_health;
mod task_diff;
mod task_workspace;
mod watcher;
mod workflow;
mod workspace_discovery;
mod worktree;

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::rpc::{Notification, Request, Response, ResponseBody, RpcError};

/// Agent-level error. Everything that reaches the top-level dispatch
/// loop is converted to a JSON-RPC error response via
/// [`RpcError::from`]; the binary itself only exits on stdin EOF.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("invalid repository URL")]
    ParsedUrl(#[from] url::ParseError),
    #[error(transparent)]
    Storage(#[from] crab_storage::StorageError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON decode error: {0}")]
    Decode(#[from] serde_json::Error),

    #[error("SDK error: {0}")]
    Sdk(#[from] crab_sdk::Error),

    #[error(transparent)]
    Cache(#[from] crab_cache::CacheError),

    #[error("file not found: {path}")]
    PathNotFound { path: String },

    #[error("invalid UTF-8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),

    #[error(transparent)]
    Url(#[from] crab_git::UrlError),

    #[error(transparent)]
    Auth(#[from] crab_auth::error::AuthError),

    #[error(transparent)]
    Config(#[from] toml::de::Error),

    #[error(transparent)]
    Workflow(#[from] crab_workflow::WorkflowError),

    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),

    #[error("crab CLI exited with status {status}: {stderr}")]
    Cli { status: i32, stderr: String },

    #[error("{message} ({code})")]
    Command {
        status: i32,
        code: String,
        message: String,
        details: serde_json::Value,
    },

    #[error("native content read failed ({code}): {message}")]
    NativeRead { code: String, message: String },

    #[error(
        "complete file {algorithm} hash differs from its captured pointer: expected {expected}, got {actual}"
    )]
    Integrity {
        algorithm: &'static str,
        expected: String,
        actual: String,
    },

    #[error(transparent)]
    Shared(Arc<AgentError>),

    #[error("{operation}; cleanup failed: {cleanup}")]
    Cleanup {
        #[source]
        operation: Box<AgentError>,
        cleanup: Box<AgentError>,
    },

    #[error("published {written_bytes} bytes; cleanup failed: {cleanup}")]
    PublicationCleanup {
        written_bytes: u64,
        #[source]
        cleanup: Box<AgentError>,
    },

    #[error("{context}: {source}")]
    Preview {
        context: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("unknown method: {0}")]
    UnknownMethod(String),

    #[error("invalid params for {method}: {source}")]
    InvalidParams {
        method: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("repository not open: {0}")]
    RepoNotOpen(String),

    #[error(
        "stale hunk anchor for {path}: expected {expected_old_lines} lines at {expected_old_start}, file disagrees"
    )]
    StaleHunk {
        path: String,
        expected_old_start: u32,
        expected_old_lines: u32,
    },

    #[error("patch apply failed: {stderr}")]
    PatchApply { stderr: String },

    #[error("operation cancelled: {op_id}")]
    OpCancelled { op_id: String },

    #[error("{message}")]
    Managed {
        kind: String,
        message: String,
        remediation: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    #[error("{0}")]
    Other(String),

    #[cfg(unix)]
    #[error("{}", .0.message)]
    ApplyWorker(RpcError),
}

pub type Result<T> = std::result::Result<T, AgentError>;

impl AgentError {
    pub(crate) fn published_bytes(&self) -> Option<u64> {
        match self {
            Self::PublicationCleanup { written_bytes, .. } => Some(*written_bytes),
            Self::Shared(error) => error.published_bytes(),
            Self::Cleanup { operation, .. } => operation.published_bytes(),
            _ => None,
        }
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        match self {
            Self::OpCancelled { .. } => true,
            Self::Shared(error) => error.is_cancelled(),
            Self::Sdk(error) => {
                error.kind() == crab_sdk::ErrorKind::Cancelled && error.cleanup_error().is_none()
            }
            _ => false,
        }
    }
}

/// Writer shared between the request dispatch loop and long-running
/// streaming handlers. `Mutex` keeps NDJSON frames from interleaving
/// when a `hydrate` notification races with a `list` response.
pub type SharedStdout = Arc<Mutex<tokio::io::Stdout>>;

fn main() -> std::io::Result<()> {
    #[cfg(unix)]
    if let Some(result) = git_active_apply::run_if_hook() {
        return result;
    }
    #[cfg(unix)]
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new(native_process::mutation::worker::ARG))
    {
        init_tracing(None);
        return native_process::mutation::worker::serve();
    }
    // Handle --version early, before any other initialization.
    if std::env::args().any(|a| a == "--version") {
        println!("crab-desktop-agent {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    run_sidecar()
}

#[tokio::main]
async fn run_sidecar() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--sqlite-live-query") {
        return preview::query::run_live_worker().await;
    }
    let log_file = path_argument("--log-file")?;
    init_tracing(log_file.as_deref());
    info!(
        version = env!("CARGO_PKG_VERSION"),
        "crab-desktop-agent starting"
    );
    if let Err(error) = preview::query::reclaim_abandoned_snapshot() {
        warn!(%error, "could not reclaim the database query snapshot");
    }

    let stdin = tokio::io::stdin();
    let stdout: SharedStdout = Arc::new(Mutex::new(tokio::io::stdout()));
    let state = handlers::State::new();

    // Spawn the background indexer. It opens its SlateDB instance and
    // waits for commands. The sender is stored in State so handlers
    // (indexer_status, open_local) can communicate with it.
    let indexer_handle = indexer::Indexer::spawn().map_err(std::io::Error::other)?;
    state.set_indexer_tx(indexer_handle.tx_clone()).await;

    let mut requests = tokio::task::JoinSet::new();
    let mut reader = BufReader::new(stdin).lines();
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let read_result = loop {
        let input = tokio::select! {
            input = reader.next_line() => input,
            result = &mut shutdown => break result,
        };
        let line = match input {
            Ok(Some(line)) => line,
            Ok(None) => break Ok(()),
            Err(error) => break Err(error),
        };
        if line.trim().is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                warn!(%error, "malformed request");
                if let Err(error) = write_frame(
                    &stdout,
                    &Response::error(None, RpcError::parse_error(&error)),
                )
                .await
                {
                    break Err(std::io::Error::other(error));
                }
                continue;
            }
        };
        let operation = match request.method.as_str() {
            "rpc/cancel" | "workflow_cancel" => None,
            "workflow_run" => request.params.get("run_id"),
            _ => request.params.get("op_id"),
        }
        .and_then(|id| id.as_str())
        .map(str::to_owned);
        if let Some(id) = &operation {
            // Reserve before spawning: a following cancel frame can otherwise
            // run before the handler subscribes and lose the signal entirely.
            drop(state.cancel_registry.register(id.clone()).await);
        }
        let out = Arc::clone(&stdout);
        let state = state.clone();
        while let Some(result) = requests.try_join_next() {
            if let Err(error) = result {
                error!(%error, "request task failed");
            }
        }
        requests.spawn(async move {
            if let Err(err) = dispatch(request, state, out, operation).await {
                error!(%err, "failed to dispatch request");
            }
        });
    };

    info!("agent shutting down");
    // Stop child owners before joining dispatch; dropping request futures would
    // skip their explicit captured-read and SDK cleanup.
    state.cancel_registry.close().await;
    state.stop_reads().await;
    #[cfg(unix)]
    if let Err(error) = state.native_processes.shutdown().await {
        error!(%error, "native process shutdown failed");
    }
    while let Some(result) = requests.join_next().await {
        if let Err(error) = result {
            error!(%error, "request task failed");
        }
    }
    indexer_handle.shutdown().await;
    if let Err(error) = state.shutdown().await {
        error!(%error, "SDK shutdown failed");
    }
    read_result
}

#[cfg(unix)]
async fn shutdown_signal() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    tokio::select! { _ = term.recv() => {}, _ = hangup.recv() => {}, _ = interrupt.recv() => {} }
    Ok(())
}

#[cfg(not(unix))]
async fn shutdown_signal() -> std::io::Result<()> {
    std::future::pending().await
}

async fn dispatch(
    req: Request,
    state: handlers::State,
    out: SharedStdout,
    operation: Option<String>,
) -> Result<()> {
    debug!(method = %req.method, id = ?req.id, "dispatching");
    let id = req.id.clone();
    let method = req.method.clone();
    let body = match handlers::handle(&state, req, Arc::clone(&out)).await {
        Ok(value) => ResponseBody::ok(value),
        Err(err) => {
            let msg = err.to_string();
            if msg.contains("ForgeAuthRequired") || msg.contains("ForgeNotSupported") {
                debug!(%err, method = %method, "handler error (auth/provider not configured)");
            } else {
                warn!(%err, method = %method, "handler error");
            }
            ResponseBody::err(RpcError::from(err))
        }
    };
    // Release admission before publishing the reply; a peer may reuse an ID
    // immediately after observing completion, while this writer is flushing.
    if let Some(id) = operation {
        state.cancel_registry.unregister(&id).await;
    }
    write_frame(&out, &Response::new(id, body)).await?;
    Ok(())
}

/// Serialize `frame` and write it to stdout as a single NDJSON line.
/// Used by both the dispatch loop and streaming notifications.
pub(crate) async fn write_frame<T: serde::Serialize>(out: &SharedStdout, frame: &T) -> Result<()> {
    let mut line = serde_json::to_vec(frame)?;
    line.push(b'\n');
    let mut guard = out.lock().await;
    guard.write_all(&line).await?;
    guard.flush().await?;
    Ok(())
}

/// Send a JSON-RPC notification (no `id`). Used by streaming handlers
/// to push progress events to the main process without blocking on a
/// reply.
pub(crate) async fn notify<T: serde::Serialize>(
    out: &SharedStdout,
    method: &str,
    params: T,
) -> Result<()> {
    let note = Notification::new(method, params);
    write_frame(out, &note).await
}

/// Emit a `progress/partial` notification carrying one incremental
/// chunk of a streaming read. Handlers that opt in (list, repo_summary)
/// call this for each item as it becomes available. The renderer
/// accumulates chunks keyed by `(op_id, kind)` so the UI can render
/// partial results before the full response arrives.
pub(crate) async fn emit_partial(
    out: &SharedStdout,
    op_id: &str,
    kind: &str,
    chunk: serde_json::Value,
) -> Result<()> {
    notify(
        out,
        "progress/partial",
        serde_json::json!({
            "op_id": op_id,
            "kind": kind,
            "chunk": chunk,
        }),
    )
    .await
}

fn path_argument(flag: &str) -> std::io::Result<Option<String>> {
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        if arg == flag {
            return iter
                .next()
                .filter(|value| !value.is_empty() && !value.starts_with("--"))
                .map(Some)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("{flag} requires a path"),
                    )
                });
        }
    }
    Ok(None)
}

fn init_tracing(log_file: Option<&str>) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    // Logs go to stderr. stdout is reserved for NDJSON RPC frames.
    let filter = tracing_subscriber::EnvFilter::try_from_env("CRAB_DESKTOP_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    match log_file {
        Some(file_path) => {
            // Set up a rolling file appender: 10 MB × 5 files.
            // tracing-appender's RollingFileAppender uses daily rotation
            // by default. We use a non-rotating appender pointed at the
            // specified path — the Electron main process manages the
            // directory. For size-based rotation we use the rolling
            // writer with a daily rotation as a reasonable approximation
            // (tracing-appender doesn't natively support size-based
            // rotation, so we rely on the log directory being managed
            // externally or use daily as a proxy).
            let parent = std::path::Path::new(file_path)
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."));
            let file_name = std::path::Path::new(file_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("agent.log");

            let file_appender = tracing_appender::rolling::daily(parent, file_name);
            let file_layer = tracing_subscriber::fmt::layer()
                .with_writer(file_appender)
                .with_ansi(false);

            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(stderr_layer)
                .with(file_layer)
                .try_init();
        }
        None => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .try_init();
        }
    }
}
