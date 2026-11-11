use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::handlers::{TaskApplyParams, task_apply};
use crate::rpc::RpcError;
use crate::{AgentError, Result};

pub(crate) const ARG: &str = "--task-apply-worker";

#[derive(Deserialize, Serialize)]
struct Request {
    repo: PathBuf,
    params: TaskApplyParams,
}

type Receipt = std::result::Result<Value, RpcError>;

pub(in crate::native_process) async fn run(
    stop: CancellationToken,
    repo: PathBuf,
    params: TaskApplyParams,
) -> Result<Value> {
    if stop.is_cancelled() {
        return Err(io::Error::other("Task Apply stopped before starting").into());
    }
    let mut request = serde_json::to_vec(&Request { repo, params })?;
    request.push(b'\n');
    // The worker must survive the RPC parent's death long enough to stop Git.
    // Isolate it from the SSH command group and leave its lifetime to the pipe.
    let mut child = Command::new(std::env::current_exe()?)
        .arg(ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn()?;
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("Apply worker pipe unavailable"))?;
    let keepalive = async move {
        input.write_all(&request).await?;
        stop.cancelled().await;
        Ok::<_, io::Error>(())
    };
    let wait = child.wait_with_output();
    tokio::pin!(wait);
    let output = tokio::select! {
        output = &mut wait => output?,
        input = keepalive => {
            let output = wait.await?;
            input?;
            output
        }
    };
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "Apply worker exited without a receipt ({})",
            output.status
        ))
        .into());
    }
    serde_json::from_slice::<Receipt>(&output.stdout)?.map_err(AgentError::ApplyWorker)
}

#[tokio::main(flavor = "current_thread")]
pub(crate) async fn serve() -> io::Result<()> {
    let mut input = BufReader::new(io::stdin());
    let mut line = String::new();
    input.read_line(&mut line)?;
    if !line.ends_with('\n') {
        return Ok(());
    }
    let request: Request = serde_json::from_str(&line)?;
    let stop = CancellationToken::new();
    let stop_on_pipe = stop.clone();
    // A dedicated thread can wait for EOF without keeping the Tokio runtime
    // alive after a successful receipt. The pipe carries one request only.
    std::thread::Builder::new()
        .name("apply-lifetime".into())
        .spawn(move || {
            let _ = input.read(&mut [0]);
            stop_on_pipe.cancel();
        })?;
    let apply = super::scope(stop.clone(), task_apply(&request.repo, request.params));
    tokio::pin!(apply);
    let result = tokio::select! {
        result = &mut apply => result,
        _ = crate::shutdown_signal() => {
            stop.cancel();
            apply.await
        }
    };
    let receipt = result.map_err(RpcError::from);
    let mut output = io::stdout().lock();
    serde_json::to_writer(&mut output, &receipt)?;
    output.flush()
}
