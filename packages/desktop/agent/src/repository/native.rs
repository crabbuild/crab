use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use crab_sdk::operation::ReadProgress;
use crab_sdk::storage::DirectStoreOptions;
use crab_types::pointer_read::{Request, Response};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout},
    sync::{Mutex, OnceCell, mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

use crate::{AgentError, Result, shellout, storage_auth::Connection};

mod process;
use process::Process;

const CLOSE_TIMEOUT: Duration = Duration::from_secs(15);
// Match the SDK's five-minute read owner; an initial LFS range can
// verify the whole payload before returning its selected bytes.
const READ_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Clone)]
pub(super) struct NativeReads(Arc<Runtime>);

pub(super) struct CapturedRead {
    runtime: NativeReads,
    root: PathBuf,
    pointer: Bytes,
    dehydrate_path: Option<String>,
    selected: Option<Connection>,
    options: Option<DirectStoreOptions>,
    size: u64,
    id: String,
    opened: OnceCell<NativeFile>,
    read_progress: ReadProgress,
}

impl CapturedRead {
    pub(super) fn for_dehydration(mut self, path: String) -> Self {
        self.dehydrate_path = Some(path);
        self
    }

    pub(super) async fn open(&self, stop: &CancellationToken) -> Result<&NativeFile> {
        self.opened
            .get_or_try_init(|| async {
                let file = self.runtime.open(self, stop).await?;
                if file.size != self.size {
                    file.close().await?;
                    return Err(AgentError::Other(
                        "native reader opened a different file size".into(),
                    ));
                }
                Ok(file)
            })
            .await
    }

    pub(super) async fn close(&self) -> Result<()> {
        if let Some(file) = self.opened.get() {
            file.close().await?;
        }
        Ok(())
    }
}

struct Runtime {
    workers: Mutex<Workers>,
    stop: CancellationToken,
}

#[derive(Default)]
struct Workers {
    jobs: JoinSet<Result<()>>,
    closed: bool,
    failure: Option<Arc<AgentError>>,
}

impl Default for NativeReads {
    fn default() -> Self {
        Self(Arc::new(Runtime {
            workers: Mutex::new(Workers::default()),
            stop: CancellationToken::new(),
        }))
    }
}

impl NativeReads {
    pub(super) fn read_token(&self) -> CancellationToken {
        self.0.stop.child_token()
    }

    pub(super) fn capture(
        &self,
        root: &Path,
        pointer: Bytes,
        size: u64,
        id: String,
        read_progress: ReadProgress,
    ) -> Result<CapturedRead> {
        // Metadata-only comparisons need no actor. Capture task-local identity
        // now, so later data preparation on a blocking parser cannot select
        // ambient credentials or a different connection.
        let selected = crate::storage_auth::current();
        let options = selected.as_ref().map(Connection::options).transpose()?;
        Ok(CapturedRead {
            runtime: self.clone(),
            root: root.to_owned(),
            pointer,
            dehydrate_path: None,
            selected,
            options,
            size,
            id,
            opened: OnceCell::new(),
            read_progress,
        })
    }

    async fn open(
        &self,
        capture: &CapturedRead,
        capture_stop: &CancellationToken,
    ) -> Result<NativeFile> {
        let id = capture.id.clone();
        let selected = capture.selected.clone();
        let options = capture.options.clone();
        let pointer = capture.pointer.clone();
        let dehydrate_path = capture.dehydrate_path.clone();
        let (send, receive) = mpsc::channel(8);
        let (ready, opened) = oneshot::channel();
        let (finished, completion) = watch::channel(Completion::Running);
        let actor_id = id.clone();
        // A retained reader outlives any one query or view. Opening still
        // belongs to its request; cancellation must drain a late handshake.
        let source_stop = self.read_token();
        let stop = source_stop.clone();
        let root = capture.root.clone();
        let read_progress = capture.read_progress.clone();
        let mut workers = self.0.workers.lock().await;
        if workers.closed {
            return Err(cancelled(&id));
        }
        while let Some(result) = workers.jobs.try_join_next() {
            workers.record(result);
        }
        workers.jobs.spawn(async move {
            let result = actor(
                root,
                Request::Open {
                    pointer_b64: STANDARD.encode(pointer),
                    dehydrate_path,
                },
                selected,
                options,
                receive,
                ready,
                &stop,
                &actor_id,
                read_progress,
            )
            .await;
            let result = result.map_err(Arc::new);
            let _ = finished.send(Completion::Finished(result.clone()));
            result.map_err(AgentError::Shared)
        });
        drop(workers);
        let opened = tokio::select! {
            biased;
            result = opened => result.map_err(|_| AgentError::Other("native reader stopped before opening".into())),
            () = capture_stop.cancelled() => {
                source_stop.cancel();
                completed(completion).await?;
                return Err(cancelled(&id));
            }
        };
        let size = match opened.and_then(|result| result) {
            Ok(size) => size,
            Err(error) => {
                completed(completion).await?;
                return Err(error);
            }
        };
        Ok(NativeFile {
            send,
            completion,
            stop: source_stop,
            id,
            size,
        })
    }

    pub(super) async fn close(&self) -> Result<()> {
        // Keep ownership through child exit, including stderr readers. Closing
        // a browse session cannot abort actors while native work still drains.
        let mut workers = self.0.workers.lock().await;
        workers.closed = true;
        self.0.stop.cancel();
        while let Some(result) = workers.jobs.join_next().await {
            workers.record(result);
        }
        match &workers.failure {
            Some(error) => Err(AgentError::Shared(error.clone())),
            None => Ok(()),
        }
    }

    pub(super) fn cancel(&self) {
        self.0.stop.cancel();
    }
}

impl Workers {
    fn record(&mut self, result: std::result::Result<Result<()>, tokio::task::JoinError>) {
        let result = result.unwrap_or_else(|source| Err(std::io::Error::other(source).into()));
        if let Err(error) = result {
            if self.failure.is_none() {
                self.failure = Some(Arc::new(error));
            } else {
                tracing::error!(%error, "native reader cleanup failed");
            }
        }
    }
}

#[derive(Clone)]
enum Completion {
    Running,
    Finished(std::result::Result<(), Arc<AgentError>>),
}

pub(super) struct NativeFile {
    send: mpsc::Sender<Query>,
    completion: watch::Receiver<Completion>,
    stop: CancellationToken,
    id: String,
    pub size: u64,
}

struct Query {
    request: Request,
    reply: oneshot::Sender<Result<Response>>,
    cancel: CancellationToken,
    read_progress: ReadProgress,
}

impl NativeFile {
    pub(super) async fn read(
        &self,
        offset: u64,
        length: u32,
        cancel: &CancellationToken,
        read_progress: &ReadProgress,
    ) -> Result<Bytes> {
        let response = self
            .query(Request::Range { offset, length }, cancel, read_progress)
            .await?;
        let result = range_response(response, self.size, offset, length);
        self.checked(result).await
    }

    pub(super) async fn chunks(
        &self,
        start: u64,
        limit: u32,
        cancel: &CancellationToken,
        read_progress: &ReadProgress,
    ) -> Result<crab_types::file_layout::ChunkPage> {
        let response = self
            .query(Request::Chunks { start, limit }, cancel, read_progress)
            .await?;
        let result = chunk_response(response, start, limit);
        self.checked(result).await
    }

    async fn checked<T>(&self, result: Result<T>) -> Result<T> {
        if result.is_err() {
            // Invalid framing makes subsequent replies untrustworthy. Drain the
            // source before reporting it; a retained file must not reuse it.
            self.close().await?;
        }
        result
    }

    async fn query(
        &self,
        request: Request,
        cancel: &CancellationToken,
        read_progress: &ReadProgress,
    ) -> Result<Response> {
        if self.stop.is_cancelled() || cancel.is_cancelled() {
            return Err(cancelled(&self.id));
        }
        let (reply, receive) = oneshot::channel();
        let query = Query {
            request,
            reply,
            cancel: cancel.clone(),
            read_progress: read_progress.clone(),
        };
        tokio::select! {
            biased;
            () = self.stop.cancelled() => return Err(cancelled(&self.id)),
            () = cancel.cancelled() => return Err(cancelled(&self.id)),
            sent = self.send.send(query) => sent.map_err(|_| cancelled(&self.id))?,
        }
        // Admission owns the query. Keep its reply even after cancellation so
        // the actor can acknowledge drained native work before this call ends.
        receive
            .await
            .map_err(|_| AgentError::Other("native reader stopped without a query result".into()))?
    }

    pub(super) async fn close(&self) -> Result<()> {
        self.stop.cancel();
        completed(self.completion.clone()).await
    }
}

fn range_response(response: Response, size: u64, offset: u64, length: u32) -> Result<Bytes> {
    let Response::Data {
        offset: actual,
        bytes_b64,
    } = response
    else {
        return Err(unexpected(response));
    };
    let start = offset.min(size);
    let expected = u64::from(length).min(size - start);
    let bytes = STANDARD.decode(bytes_b64).map_err(std::io::Error::other)?;
    if actual != start || bytes.len() as u64 != expected {
        return Err(AgentError::Other(
            "native reader returned a mismatched range".into(),
        ));
    }
    Ok(Bytes::from(bytes))
}

fn chunk_response(
    response: Response,
    start: u64,
    limit: u32,
) -> Result<crab_types::file_layout::ChunkPage> {
    let Response::Chunks { page } = response else {
        return Err(unexpected(response));
    };
    if start > page.chunk_count
        || page.chunks.len() as u64 != (page.chunk_count - start).min(u64::from(limit))
    {
        return Err(AgentError::Other(
            "native reader returned a mismatched chunk page".into(),
        ));
    }
    let end = start + page.chunks.len() as u64;
    if page.next != (end < page.chunk_count).then_some(end) {
        return Err(AgentError::Other(
            "native reader returned a mismatched chunk cursor".into(),
        ));
    }
    Ok(page)
}

async fn completed(mut completion: watch::Receiver<Completion>) -> Result<()> {
    loop {
        let state = completion.borrow().clone();
        if let Completion::Finished(result) = state {
            return result.map_err(AgentError::Shared);
        }
        completion.changed().await.map_err(|_| {
            AgentError::Other("native reader stopped without a close acknowledgement".into())
        })?;
    }
}

async fn actor(
    root: PathBuf,
    request: Request,
    selected: Option<Connection>,
    options: Option<DirectStoreOptions>,
    receive: mpsc::Receiver<Query>,
    ready: oneshot::Sender<Result<u64>>,
    stop: &CancellationToken,
    id: &str,
    read_progress: ReadProgress,
) -> Result<()> {
    let mut process = match Process::spawn(&root, options.as_ref(), read_progress).await {
        Ok(process) => process,
        Err(error) => {
            let _ = ready.send(Err(error));
            return Ok(());
        }
    };
    let setup = process.open(request, selected.as_ref(), stop, id).await;
    let size = match setup {
        Ok(size) => size,
        Err(error) => {
            let close = process.finish().await;
            return match close {
                Err(error @ AgentError::Cli { .. }) => {
                    let _ = ready.send(Err(error));
                    Ok(())
                }
                close => {
                    let _ = ready.send(Err(error));
                    close
                }
            };
        }
    };
    let result = if ready.send(Ok(size)).is_ok() {
        process.serve(receive, stop, id).await
    } else {
        Ok(())
    };
    // Every exit from the protocol, including malformed replies and dropped
    // request futures, rejoins the same native close and process-reaping path.
    let close = process.finish().await;
    result.and(close)
}

fn unexpected(response: Response) -> AgentError {
    match response {
        Response::Failed { code, message } => AgentError::NativeRead { code, message },
        _ => AgentError::Other("native reader returned an unexpected acknowledgement".into()),
    }
}

fn cancelled(id: &str) -> AgentError {
    AgentError::OpCancelled {
        op_id: id.to_owned(),
    }
}
