//! Owned native stdio processes. Provider protocol policy stays in Electron.

use std::collections::HashMap;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;

use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{Mutex, Semaphore, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::{AgentError, Result, SharedStdout};

const CHUNK_BYTES: usize = 16 * 1024;
const MAX_PROCESSES: usize = 64;
mod group;
pub(crate) mod mutation;
use group::supervise;
#[cfg(test)]
use group::{GRACE, group_exists};

#[derive(Deserialize)]
pub(crate) struct Open {
    process_id: String,
    task_id: Option<uuid::Uuid>,
    executable: String,
    args: Vec<String>,
    cwd: PathBuf,
}

#[derive(Deserialize)]
pub(crate) struct Target {
    process_id: String,
}

#[derive(Deserialize)]
pub(crate) struct Stop {
    process_id: String,
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize)]
pub(crate) struct Write {
    process_id: String,
    bytes_b64: String,
}

#[derive(Deserialize)]
pub(crate) struct Ack {
    process_id: String,
    stream: Stream,
    sequence: u64,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Stream {
    Stdout,
    Stderr,
}

impl Stream {
    fn name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
    fn index(self) -> usize {
        match self {
            Self::Stdout => 0,
            Self::Stderr => 1,
        }
    }
}

#[derive(Clone, Copy)]
struct Exit {
    code: Option<i32>,
    signal: Option<i32>,
}
type Outcome = std::result::Result<Exit, Arc<io::Error>>;

struct Credit {
    available: Semaphore,
    pending: Mutex<Option<u64>>,
}

impl Credit {
    fn new() -> Self {
        Self {
            available: Semaphore::new(1),
            pending: Mutex::new(None),
        }
    }
}

struct Process {
    workspace_lease: Mutex<Option<std::fs::File>>,
    stdin: Mutex<Option<ChildStdin>>,
    stop: CancellationToken,
    force: CancellationToken,
    closed: watch::Receiver<Option<Outcome>>,
    credit: [Arc<Credit>; 2],
}

#[derive(Default)]
struct Registry {
    closing: bool,
    stop_mutations: CancellationToken,
    processes: HashMap<String, Arc<Process>>,
    tasks: tokio::task::JoinSet<()>,
}

#[derive(Clone, Default)]
pub(crate) struct NativeProcesses {
    registry: Arc<Mutex<Registry>>,
}

impl NativeProcesses {
    pub(crate) async fn apply(
        &self,
        repo: PathBuf,
        params: crate::handlers::TaskApplyParams,
    ) -> Result<Value> {
        let (send, receive) = oneshot::channel();
        {
            let mut registry = self.registry.lock().await;
            if registry.closing {
                return Err(AgentError::Other("Native process owner is closing".into()));
            }
            while let Some(result) = registry.tasks.try_join_next() {
                result.map_err(io::Error::other)?;
            }
            // The registry owns the worker past RPC cancellation. On hard
            // sidecar death, the worker observes its lifetime pipe closing.
            let stop = registry.stop_mutations.child_token();
            // Task locals do not follow spawned futures. Preserve this request's
            // selected store until its detached worker configures its children.
            let connection = crate::storage_auth::current();
            registry.tasks.spawn(async move {
                let work = mutation::worker::run(stop, repo, params);
                let _ = send.send(crate::storage_auth::scope(connection, work).await);
            });
        }
        receive.await.map_err(io::Error::other)?
    }

    pub(crate) async fn open(&self, p: Open, out: SharedStdout) -> Result<Value> {
        uuid::Uuid::parse_str(&p.process_id).map_err(|source| AgentError::Managed {
            kind: "native_process_identity".into(),
            message: "Native process identity must be a UUID".into(),
            remediation: "Open a new native connection".into(),
            source: Some(Box::new(source)),
        })?;
        if !p.cwd.is_absolute() || !tokio::fs::metadata(&p.cwd).await?.is_dir() {
            return Err(AgentError::Other(
                "Native process requires an absolute working directory".into(),
            ));
        }
        let workspace_lease = crate::task_workspace::writer_lease(&p.cwd, p.task_id).await?;
        let mut registry = self.registry.lock().await;
        while let Some(result) = registry.tasks.try_join_next() {
            result.map_err(io::Error::other)?;
        }
        if registry.closing
            || registry.processes.len() >= MAX_PROCESSES
            || registry.processes.contains_key(&p.process_id)
        {
            return Err(AgentError::Other(
                "Native process owner is closing, full, or already registered".into(),
            ));
        }
        let mut command = Command::new(&p.executable);
        command
            .args(&p.args)
            .current_dir(&p.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command.process_group(0);

        let mut child = command.spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("Native process PID unavailable"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("Native stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("Native stdout unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("Native stderr unavailable"))?;
        let (closed, receipt) = watch::channel(None);
        let process = Arc::new(Process {
            workspace_lease: Mutex::new(workspace_lease),
            stdin: Mutex::new(Some(stdin)),
            stop: CancellationToken::new(),
            force: CancellationToken::new(),
            closed: receipt,
            credit: [Arc::new(Credit::new()), Arc::new(Credit::new())],
        });
        registry
            .processes
            .insert(p.process_id.clone(), Arc::clone(&process));
        // No await separates spawn from its registered supervisor. Aborting the
        // RPC cannot abandon a child before shutdown can find its physical owner.
        registry.tasks.spawn(async move {
            let pipes: [(Stream, Box<dyn AsyncRead + Send + Unpin>); 2] = [
                (Stream::Stdout, Box::new(stdout)),
                (Stream::Stderr, Box::new(stderr)),
            ];
            let [stdout, stderr] = pipes.map(|(stream, input)| {
                let process = Arc::clone(&process);
                let out = Arc::clone(&out);
                let id = p.process_id.clone();
                tokio::spawn(async move {
                    let credit = Arc::clone(&process.credit[stream.index()]);
                    let stop = process.stop.clone();
                    let result = pump(input, credit, out, id, stream, stop).await;
                    if result.is_err() {
                        process.stop.cancel();
                    }
                    result
                })
            });
            let result = supervise(&mut child, pid, &process.stop, &process.force)
                .await
                .map(|status| Exit {
                    code: status.code(),
                    signal: status.signal(),
                });
            if result.is_err() {
                process.stop.cancel();
            }
            let (stdout, stderr) = tokio::join!(stdout, stderr);
            let streams = stdout
                .map_err(io::Error::other)
                .and_then(|result| result)
                .and(stderr.map_err(io::Error::other).and_then(|result| result));
            let result = result
                .and_then(|exit| streams.map(|()| exit))
                .map_err(Arc::new);
            let frame = match &result {
                Ok(exit) => {
                    json!({ "process_id": p.process_id, "code": exit.code, "signal": exit.signal })
                }
                Err(error) => json!({ "process_id": p.process_id, "error": error.to_string() }),
            };
            // Cleanup may start as soon as another profile sees the receipt.
            // Release only confirmed writers before publication; uncertainty
            // retains the lease so a failed stream cannot authorize removal.
            if result.is_ok() {
                process.workspace_lease.lock().await.take();
            }
            closed.send_replace(Some(result));
            if let Err(error) = crate::write_frame(
                &out,
                &crate::rpc::Notification::new("native_process/closed", frame),
            )
            .await
            {
                tracing::warn!(%error, "native process close receipt could not be delivered");
            }
        });
        Ok(json!({ "opened": true, "pid": pid }))
    }

    async fn process(&self, id: &str) -> Result<Arc<Process>> {
        self.registry
            .lock()
            .await
            .processes
            .get(id)
            .cloned()
            .ok_or_else(|| AgentError::Other("Native process owner not found".into()))
    }

    pub(crate) async fn write(&self, p: Write) -> Result<Value> {
        if p.bytes_b64.len() > CHUNK_BYTES * 4 / 3 + 4 {
            return Err(AgentError::Other(
                "Native stdin chunk exceeds transport limit".into(),
            ));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(p.bytes_b64)
            .map_err(|source| io::Error::new(io::ErrorKind::InvalidInput, source))?;
        if bytes.len() > CHUNK_BYTES {
            return Err(AgentError::Other(
                "Native stdin chunk exceeds transport limit".into(),
            ));
        }
        let process = self.process(&p.process_id).await?;
        let mut stdin = process.stdin.lock().await;
        let stdin = stdin
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "Native stdin is closed"))?;
        stdin.write_all(&bytes).await?;
        Ok(json!({ "written": bytes.len() }))
    }

    pub(crate) async fn close_input(&self, p: Target) -> Result<Value> {
        let process = self.process(&p.process_id).await?;
        process.stdin.lock().await.take();
        Ok(json!({ "closed": true }))
    }

    pub(crate) async fn ack(&self, p: Ack) -> Result<Value> {
        let process = self.process(&p.process_id).await?;
        let credit = &process.credit[p.stream.index()];
        let mut pending = credit.pending.lock().await;
        if *pending != Some(p.sequence) {
            return Err(AgentError::Other(
                "Native output acknowledgement does not match its frame".into(),
            ));
        }
        *pending = None;
        credit.available.add_permits(1);
        Ok(json!({ "accepted": true }))
    }

    pub(crate) async fn stop(&self, p: Stop) -> Result<Value> {
        let process = self.process(&p.process_id).await?;
        if p.force {
            process.force.cancel();
        }
        process.stop.cancel();
        let exit = wait_closed(process.closed.clone()).await?;
        Ok(json!({ "closed": true, "code": exit.code, "signal": exit.signal }))
    }

    pub(crate) async fn release(&self, p: Target) -> Result<Value> {
        let mut registry = self.registry.lock().await;
        let process = registry
            .processes
            .get(&p.process_id)
            .ok_or_else(|| AgentError::Other("Native process owner not found".into()))?;
        if !matches!(process.closed.borrow().as_ref(), Some(Ok(_))) {
            return Err(AgentError::Other(
                "Native process closure is not confirmed".into(),
            ));
        }
        registry.processes.remove(&p.process_id);
        Ok(json!({ "released": true }))
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        let (processes, mut tasks) = {
            let mut registry = self.registry.lock().await;
            registry.closing = true;
            registry.stop_mutations.cancel();
            let processes = registry.processes.values().cloned().collect::<Vec<_>>();
            let tasks = std::mem::take(&mut registry.tasks);
            (processes, tasks)
        };
        for process in &processes {
            process.stop.cancel();
        }
        let mut result = Ok(());
        for process in processes {
            if let Err(error) = wait_closed(process.closed.clone()).await {
                result = Err(error);
            }
        }
        // Physical receipts can precede their outgoing close frame. Keep every
        // supervisor owned until that frame settles before the runtime exits.
        while let Some(task) = tasks.join_next().await {
            if let Err(error) = task {
                result = Err(io::Error::other(error).into());
            }
        }
        result
    }
}

async fn wait_closed(mut receipt: watch::Receiver<Option<Outcome>>) -> Result<Exit> {
    loop {
        if let Some(outcome) = receipt.borrow().clone() {
            return outcome.map_err(|source| io::Error::new(source.kind(), source).into());
        }
        receipt.changed().await.map_err(io::Error::other)?;
    }
}

async fn pump(
    mut input: impl AsyncRead + Unpin,
    credit: Arc<Credit>,
    out: SharedStdout,
    process_id: String,
    stream: Stream,
    stop: CancellationToken,
) -> io::Result<()> {
    let mut bytes = vec![0; CHUNK_BYTES];
    let mut sequence = 0_u64;
    loop {
        let permit = tokio::select! { biased; _ = stop.cancelled() => return Ok(()), permit = credit.available.acquire() => permit.map_err(io::Error::other)? };
        let size = tokio::select! { biased; _ = stop.cancelled() => return Ok(()), size = input.read(&mut bytes) => size? };
        if size == 0 {
            return Ok(());
        }
        permit.forget();
        sequence += 1;
        *credit.pending.lock().await = Some(sequence);
        let bytes_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes[..size]);
        let frame = crate::rpc::Notification::new(
            "native_process/output",
            json!({ "process_id": process_id, "stream": stream.name(), "sequence": sequence, "bytes_b64": bytes_b64 }),
        );
        // Never cancel a frame halfway through the shared NDJSON writer.
        crate::write_frame(&out, &frame)
            .await
            .map_err(io::Error::other)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output() -> SharedStdout {
        Arc::new(Mutex::new(tokio::io::stdout()))
    }

    async fn open(
        service: &NativeProcesses,
        root: &std::path::Path,
        command: &str,
    ) -> (String, u32) {
        let id = uuid::Uuid::new_v4().to_string();
        let value = service
            .open(
                Open {
                    task_id: None,
                    process_id: id.clone(),
                    executable: "/bin/sh".into(),
                    args: vec!["-c".into(), command.into()],
                    cwd: root.to_path_buf(),
                },
                output(),
            )
            .await
            .unwrap();
        (id, value["pid"].as_u64().unwrap() as u32)
    }

    async fn until(mut ready: impl AsyncFnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while !ready().await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stdin_acknowledges_real_bytes_and_release_requires_physical_close() {
        let root = tempfile::tempdir().unwrap();
        let service = NativeProcesses::default();
        let (id, pid) = open(&service, root.path(), "cat > received.bin").await;
        let bytes = [0, 255, b'\n', 128, b'x'];
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        service
            .write(Write {
                process_id: id.clone(),
                bytes_b64: encoded,
            })
            .await
            .unwrap();
        assert!(
            service
                .release(Target {
                    process_id: id.clone()
                })
                .await
                .is_err()
        );
        service
            .close_input(Target {
                process_id: id.clone(),
            })
            .await
            .unwrap();
        let process = service.process(&id).await.unwrap();
        let exit =
            tokio::time::timeout(Duration::from_secs(15), wait_closed(process.closed.clone()))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(
            (
                exit.code,
                group_exists(pid).await.unwrap(),
                tokio::fs::read(root.path().join("received.bin"))
                    .await
                    .unwrap()
            ),
            (Some(0), false, bytes.to_vec())
        );
        service.release(Target { process_id: id }).await.unwrap();
        service.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn output_credit_bounds_both_streams_until_exact_acknowledgement() {
        let root = tempfile::tempdir().unwrap();
        let service = NativeProcesses::default();
        let (id, _) = open(&service, root.path(), "printf stdout; printf stderr >&2").await;
        let process = service.process(&id).await.unwrap();
        until(async || {
            process.credit[0].pending.lock().await.is_some()
                && process.credit[1].pending.lock().await.is_some()
        })
        .await;
        assert!(process.closed.borrow().is_none());
        assert!(
            service
                .ack(Ack {
                    process_id: id.clone(),
                    stream: Stream::Stdout,
                    sequence: 2
                })
                .await
                .is_err()
        );
        for stream in [Stream::Stdout, Stream::Stderr] {
            service
                .ack(Ack {
                    process_id: id.clone(),
                    stream,
                    sequence: 1,
                })
                .await
                .unwrap();
            assert!(
                service
                    .ack(Ack {
                        process_id: id.clone(),
                        stream,
                        sequence: 1
                    })
                    .await
                    .is_err()
            );
        }
        let exit =
            tokio::time::timeout(Duration::from_secs(15), wait_closed(process.closed.clone()))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(exit.code, Some(0));
        service.release(Target { process_id: id }).await.unwrap();
        service.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_retires_descendants_and_rejects_new_spawns() {
        let root = tempfile::tempdir().unwrap();
        let service = NativeProcesses::default();
        let (_, pid) = open(
            &service,
            root.path(),
            "sleep 120 & echo $! > descendant.pid; wait",
        )
        .await;
        until(async || {
            tokio::fs::try_exists(root.path().join("descendant.pid"))
                .await
                .unwrap()
        })
        .await;
        let descendant: u32 = tokio::fs::read_to_string(root.path().join("descendant.pid"))
            .await
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        service.shutdown().await.unwrap();
        assert!(!group_exists(pid).await.unwrap());
        // Signal zero against a PID distinguishes a still-running descendant
        // from merely seeing the leader's closed stdio.
        // SAFETY: signal zero only queries the descendant created by this fixture.
        assert_eq!(unsafe { libc::kill(descendant as i32, 0) }, -1);
        assert!(
            service
                .open(
                    Open {
                        task_id: None,
                        process_id: uuid::Uuid::new_v4().to_string(),
                        executable: "/bin/sh".into(),
                        args: vec![],
                        cwd: root.path().to_path_buf()
                    },
                    output()
                )
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stop_escalates_a_term_resistant_group_before_acknowledging_closure() {
        let root = tempfile::tempdir().unwrap();
        let service = NativeProcesses::default();
        let (id, pid) = open(
            &service,
            root.path(),
            "trap '' TERM; sleep 120 & echo ready > ready; wait",
        )
        .await;
        until(async || {
            tokio::fs::try_exists(root.path().join("ready"))
                .await
                .unwrap()
        })
        .await;
        let stopped = tokio::time::timeout(
            Duration::from_secs(15),
            service.stop(Stop {
                process_id: id.clone(),
                force: false,
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            (stopped["signal"].as_i64(), group_exists(pid).await.unwrap()),
            (Some(i64::from(libc::SIGKILL)), false)
        );
        service.release(Target { process_id: id }).await.unwrap();
        service.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn force_stop_does_not_wait_for_the_term_grace_period() {
        let root = tempfile::tempdir().unwrap();
        let service = NativeProcesses::default();
        let (id, _) = open(
            &service,
            root.path(),
            "trap '' TERM; echo ready > ready; sleep 120",
        )
        .await;
        until(async || {
            tokio::fs::try_exists(root.path().join("ready"))
                .await
                .unwrap()
        })
        .await;
        let started = tokio::time::Instant::now();
        let stopped = service
            .stop(Stop {
                process_id: id.clone(),
                force: true,
            })
            .await
            .unwrap();
        assert_eq!(
            (stopped["signal"].as_i64(), started.elapsed() < GRACE),
            (Some(i64::from(libc::SIGKILL)), true)
        );
        service.release(Target { process_id: id }).await.unwrap();
        service.shutdown().await.unwrap();
    }
}
