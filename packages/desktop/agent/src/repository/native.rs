use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use crab_sdk::storage::DirectStoreOptions;
use crab_types::pointer_read::{Request, Response};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout},
    sync::{Mutex, mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

use crate::{AgentError, Result, shellout, storage_auth::Connection};

const CLOSE_TIMEOUT: Duration = Duration::from_secs(15);
// Match the SDK's five-minute read owner; an initial LFS range can
// verify the whole payload before returning its selected bytes.
const READ_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Clone)]
pub(super) struct NativeReads(Arc<Runtime>);

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

    pub(super) async fn open(
        &self,
        root: &Path,
        pointer: Bytes,
        capture_stop: CancellationToken,
        id: String,
    ) -> Result<NativeFile> {
        // Storage identity is task-local. Capture it before the actor leaves
        // request scope; validate Ready before any native range may access it.
        let selected = crate::storage_auth::current();
        let options = selected.as_ref().map(Connection::options).transpose()?;
        let (send, receive) = mpsc::channel(8);
        let (ready, opened) = oneshot::channel();
        let (finished, completion) = watch::channel(Completion::Running);
        let actor_id = id.clone();
        // A retained reader outlives any one query or view. Opening still
        // belongs to its request; cancellation must drain a late handshake.
        let source_stop = self.read_token();
        let stop = source_stop.clone();
        let root = root.to_owned();
        let mut workers = self.0.workers.lock().await;
        if workers.closed {
            return Err(cancelled(&id));
        }
        while let Some(result) = workers.jobs.try_join_next() {
            workers.record(result);
        }
        workers.jobs.spawn(async move {
            let result = actor(
                root, pointer, selected, options, receive, ready, &stop, &actor_id,
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
}

impl NativeFile {
    pub(super) async fn read(
        &self,
        offset: u64,
        length: u32,
        cancel: &CancellationToken,
    ) -> Result<Bytes> {
        let response = self
            .query(Request::Range { offset, length }, cancel)
            .await?;
        let result = range_response(response, self.size, offset, length);
        self.checked(result).await
    }

    pub(super) async fn chunks(
        &self,
        start: u64,
        limit: u32,
        cancel: &CancellationToken,
    ) -> Result<crab_types::file_layout::ChunkPage> {
        let response = self.query(Request::Chunks { start, limit }, cancel).await?;
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

    async fn query(&self, request: Request, cancel: &CancellationToken) -> Result<Response> {
        if self.stop.is_cancelled() || cancel.is_cancelled() {
            return Err(cancelled(&self.id));
        }
        let (reply, receive) = oneshot::channel();
        let query = Query {
            request,
            reply,
            cancel: cancel.clone(),
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
    pointer: Bytes,
    selected: Option<Connection>,
    options: Option<DirectStoreOptions>,
    receive: mpsc::Receiver<Query>,
    ready: oneshot::Sender<Result<u64>>,
    stop: &CancellationToken,
    id: &str,
) -> Result<()> {
    let mut process = match Process::spawn(&root, options.as_ref()).await {
        Ok(process) => process,
        Err(error) => {
            let _ = ready.send(Err(error));
            return Ok(());
        }
    };
    let setup = process.open(pointer, selected.as_ref(), stop, id).await;
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

struct Process {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Lines<BufReader<ChildStdout>>,
    stderr: JoinHandle<std::io::Result<Vec<u8>>>,
    closed: bool,
}

impl Process {
    async fn spawn(root: &Path, options: Option<&DirectStoreOptions>) -> Result<Self> {
        let mut command = tokio::process::Command::new(shellout::resolve_crab_bin());
        if let Some(options) = options {
            options.configure_command(command.as_std_mut())?;
        }
        let mut child = command
            .arg("read-pointer")
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let (Some(stdin), Some(stdout), Some(mut stderr)) = pipes else {
            child.kill().await?;
            return Err(AgentError::Other("native reader pipes unavailable".into()));
        };
        let stderr = tokio::spawn(async move {
            let mut bytes = Vec::new();
            (&mut stderr)
                .take(64 * 1024)
                .read_to_end(&mut bytes)
                .await?;
            tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await?;
            Ok(bytes)
        });
        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout).lines(),
            stderr,
            closed: false,
        })
    }

    async fn open(
        &mut self,
        pointer: Bytes,
        selected: Option<&Connection>,
        stop: &CancellationToken,
        id: &str,
    ) -> Result<u64> {
        self.send(Request::Open {
            pointer_b64: STANDARD.encode(pointer),
        })
        .await?;
        let response = tokio::select! {
            () = stop.cancelled() => return Err(cancelled(id)),
            result = tokio::time::timeout(READ_TIMEOUT, response(&mut self.stdout)) => result.map_err(|_| AgentError::Other("native reader open timed out".into()))??,
        };
        let Response::Ready { size, remote_url } = response else {
            return Err(unexpected(response));
        };
        if let (Some(selected), Some(remote)) = (selected, remote_url) {
            selected.repository_prefix(&remote)?;
        }
        Ok(size)
    }

    async fn serve(
        &mut self,
        mut receive: mpsc::Receiver<Query>,
        stop: &CancellationToken,
        id: &str,
    ) -> Result<()> {
        loop {
            let request = tokio::select! {
                () = stop.cancelled() => return Ok(()),
                request = receive.recv() => request,
            };
            let Some(Query {
                request,
                mut reply,
                cancel,
            }) = request
            else {
                return Ok(());
            };
            if reply.is_closed() || cancel.is_cancelled() {
                let _ = reply.send(Err(cancelled(id)));
                continue;
            }
            let result = self.query(request, &mut reply, stop, &cancel, id).await;
            match result {
                Err(error)
                    if !matches!(
                        error,
                        AgentError::NativeRead { .. } | AgentError::OpCancelled { .. }
                    ) || stop.is_cancelled()
                        && !matches!(error, AgentError::OpCancelled { .. }) =>
                {
                    let error = Arc::new(error);
                    let _ = reply.send(Err(AgentError::Shared(error.clone())));
                    return Err(AgentError::Shared(error));
                }
                result => {
                    let _ = reply.send(result);
                }
            }
            if stop.is_cancelled() {
                return Ok(());
            }
        }
    }

    async fn query(
        &mut self,
        request: Request,
        reply: &mut oneshot::Sender<Result<Response>>,
        stop: &CancellationToken,
        cancel: &CancellationToken,
        id: &str,
    ) -> Result<Response> {
        self.send(request).await?;
        let mut timed_out = false;
        let next = response(&mut self.stdout);
        tokio::pin!(next);
        let result = tokio::select! {
            biased;
            result = &mut next => result,
            () = stop.cancelled() => {
                self.stdin.take();
                tokio::time::timeout(CLOSE_TIMEOUT, next).await.map_err(|_| AgentError::Other("native reader did not acknowledge close".into()))?
            },
            () = cancel.cancelled() => {
                send(&mut self.stdin, Request::Cancel).await?;
                tokio::time::timeout(CLOSE_TIMEOUT, next).await.map_err(|_| AgentError::Other("native reader did not acknowledge cancellation".into()))?
            },
            () = reply.closed() => {
                send(&mut self.stdin, Request::Cancel).await?;
                tokio::time::timeout(CLOSE_TIMEOUT, next).await.map_err(|_| AgentError::Other("native reader did not acknowledge cancellation".into()))?
            },
            () = tokio::time::sleep(READ_TIMEOUT) => {
                send(&mut self.stdin, Request::Cancel).await?;
                timed_out = true;
                tokio::time::timeout(CLOSE_TIMEOUT, next).await.map_err(|_| AgentError::Other("native reader did not acknowledge cancellation".into()))?
            },
        }?;
        let result = match result {
            reply @ (Response::Data { .. } | Response::Chunks { .. }) => Ok(reply),
            Response::Cancelled => Err(cancelled(id)),
            Response::Closed if stop.is_cancelled() => {
                self.closed = true;
                Err(cancelled(id))
            }
            Response::Failed { code, message } => {
                if stop.is_cancelled() {
                    self.closed = true;
                }
                Err(AgentError::NativeRead { code, message })
            }
            other => Err(unexpected(other)),
        };
        if timed_out && matches!(&result, Ok(_) | Err(AgentError::OpCancelled { .. })) {
            return Err(AgentError::Other(format!(
                "native read exceeded {} seconds",
                READ_TIMEOUT.as_secs()
            )));
        }
        result
    }

    async fn send(&mut self, request: Request) -> Result<()> {
        send(&mut self.stdin, request).await
    }

    async fn finish(mut self) -> Result<()> {
        self.stdin.take();
        let drain = async {
            let mut failure = None;
            while !self.closed {
                match response(&mut self.stdout).await {
                    Ok(Response::Closed) => self.closed = true,
                    Ok(Response::Failed { code, message }) => {
                        failure = Some(AgentError::NativeRead { code, message });
                        break;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                }
            }
            // Do not short-circuit on a protocol/wait error: stderr still owns
            // a task and the child must be reaped before returning a close result.
            let status = self.child.wait().await;
            let stderr = (&mut self.stderr).await;
            let status = status?;
            let stderr = stderr.map_err(std::io::Error::other)??;
            if !status.success() {
                return Err(AgentError::Cli {
                    status: status.code().unwrap_or(-1),
                    stderr: String::from_utf8_lossy(&stderr).into_owned(),
                });
            }
            if let Some(error) = failure {
                return Err(error);
            }
            Ok(())
        };
        match tokio::time::timeout(CLOSE_TIMEOUT, drain).await {
            Ok(result) => result,
            Err(_) => {
                let kill = self.child.kill().await;
                let stderr = self.stderr.await;
                kill?;
                stderr.map_err(std::io::Error::other)??;
                Err(AgentError::Other(
                    "native reader cleanup timed out; process terminated".into(),
                ))
            }
        }
    }
}

async fn send(stdin: &mut Option<ChildStdin>, request: Request) -> Result<()> {
    let stdin = stdin
        .as_mut()
        .ok_or_else(|| AgentError::Other("native reader input is closed".into()))?;
    let mut bytes = serde_json::to_vec(&request)?;
    bytes.push(b'\n');
    stdin.write_all(&bytes).await?;
    stdin.flush().await?;
    Ok(())
}

async fn response(stdout: &mut Lines<BufReader<ChildStdout>>) -> Result<Response> {
    let line = stdout
        .next_line()
        .await?
        .ok_or_else(|| AgentError::Other("native reader closed its response pipe".into()))?;
    if line.len() > 12 * 1024 * 1024 {
        return Err(AgentError::Other(
            "native reader response exceeded its range limit".into(),
        ));
    }
    Ok(serde_json::from_str(&line)?)
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
