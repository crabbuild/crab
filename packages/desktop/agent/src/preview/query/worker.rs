use std::{
    io,
    path::{Path, PathBuf},
    process::Stdio,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
};

use super::{SqliteJob, execute_sqlite};
use crate::{AgentError, Result, repository::ReadScope};

const MAX_REQUEST_BYTES: u64 = 512 * 1024;
const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Request<P, J> {
    path: P,
    job: J,
    size: u64,
}

type Response = std::result::Result<Value, String>;

async fn read_frame<T: DeserializeOwned>(input: impl AsyncRead + Unpin, max: u64) -> io::Result<T> {
    let mut bytes = Vec::new();
    BufReader::new(input.take(max))
        .read_until(b'\n', &mut bytes)
        .await?;
    if bytes.last() != Some(&b'\n') {
        return Err(io::Error::other("Incomplete SQLite worker frame"));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

pub(super) async fn execute(
    path: &Path,
    job: &SqliteJob,
    size: u64,
    scope: &ReadScope,
) -> Result<Value> {
    let mut request = serde_json::to_vec(&Request { path, job, size })?;
    request.push(b'\n');
    scope.check()?;
    // On POSIX a raw file close in the sidecar could release this database's
    // SQLite locks. Only the child opens the live database through SQLite.
    let mut child = Command::new(std::env::current_exe()?)
        .arg("--sqlite-live-query")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("Missing SQLite worker stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("Missing SQLite worker stdout"))?;
    let reply = async {
        stdin.write_all(&request).await?;
        read_frame::<Response>(stdout, MAX_RESPONSE_BYTES).await
    };
    let response = tokio::select! {
        result = reply => result.map_err(AgentError::from),
        error = scope.cancelled() => Err(error),
    };
    // EOF also tells the worker to release its transaction if the sidecar dies.
    drop(stdin);
    match response {
        Ok(response) => {
            let status = child.wait().await?;
            if !status.success() {
                return Err(io::Error::other(format!("SQLite worker exited with {status}")).into());
            }
            response.map_err(AgentError::Other)
        }
        Err(error) => super::with_cleanup(Err(error), child.kill().await.map_err(AgentError::from)),
    }
}

pub(crate) async fn run() -> io::Result<()> {
    let mut stdin = BufReader::new(tokio::io::stdin());
    let Request { path, job, size } =
        read_frame::<Request<PathBuf, SqliteJob>>(&mut stdin, MAX_REQUEST_BYTES).await?;
    let scope = ReadScope::default();
    let reader = scope.clone();
    let mut work =
        tokio::task::spawn_blocking(move || execute_sqlite(&path, job, size, true, &reader));
    let result = tokio::select! {
        result = &mut work => result.map_err(io::Error::other)?,
        _ = stdin.fill_buf() => {
            scope.cancel();
            let _ = work.await.map_err(io::Error::other)?;
            return Ok(());
        }
    };
    let mut response = serde_json::to_vec(&result.map_err(|error| error.to_string()))?;
    response.push(b'\n');
    let mut stdout = tokio::io::stdout();
    stdout.write_all(&response).await?;
    stdout.flush().await
}
