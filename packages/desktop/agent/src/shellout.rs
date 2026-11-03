//! Streaming wrapper around the `crab` CLI.
//!
//! Every write-side verb (`clone`, `add`, `push`, `hydrate`,
//! `dehydrate`) is routed through here. We spawn a detached child,
//! stream stdout+stderr line-by-line as JSON-RPC notifications, and
//! resolve with the final exit status.
//!
//! Progress lines: the `crab` CLI already emits structured
//! `indicatif` progress. We treat each line as an opaque string — the
//! Electron UI can render it verbatim or parse out percentages with a
//! regex. Keeping the agent dumb here avoids coupling desktop to the
//! exact progress format.

use std::process::Stdio;
use std::sync::Arc;

use serde::Serialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::{debug, warn};

use crate::{AgentError, Result, SharedStdout, notify};

pub async fn run_json(
    cwd: Option<&std::path::Path>,
    args: &[&str],
    env_vars: &[(String, String)],
) -> Result<serde_json::Value> {
    let mut command = Command::new(resolve_crab_bin());
    command
        .args(args)
        .envs(env_vars.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::null())
        .kill_on_drop(true);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command.output().await?;
    if !output.status.success() {
        return Err(AgentError::Cli {
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    let mut envelope: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    envelope
        .get_mut("data")
        .map(serde_json::Value::take)
        .ok_or_else(|| AgentError::Other("Crab returned no structured response data".into()))
}

pub async fn run_structured(
    op: &str,
    job_id: &str,
    cwd: &std::path::Path,
    args: &[String],
    env_vars: &[(String, String)],
    event_method: &str,
    out: SharedStdout,
) -> Result<serde_json::Value> {
    let mut child = Command::new(resolve_crab_bin())
        .args(args)
        .envs(env_vars.iter().map(|(key, value)| (key, value)))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AgentError::Other("Crab stdout unavailable".into()))?;
    let stderr = child.stderr.take();
    let read = async {
        let mut lines = BufReader::new(stdout).lines();
        let mut terminal = None;
        while let Some(line) = lines.next_line().await? {
            let frame: serde_json::Value = serde_json::from_str(&line)?;
            let kind = frame["type"]
                .as_str()
                .ok_or_else(|| AgentError::Other("Crab JSONL frame has no type".into()))?;
            let data = &frame["data"];
            notify(
                &out,
                event_method,
                serde_json::json!({ "job_id": job_id, "event": kind, "schema": frame["schema"], "data": data }),
            )
            .await?;
            if kind == "result" && !data.is_null() {
                terminal = Some(data.clone());
            }
        }
        Ok::<_, AgentError>(terminal)
    };
    let (status, result, stderr) = tokio::try_join!(
        async { child.wait().await.map_err(AgentError::Io) },
        read,
        async {
            Ok::<_, AgentError>(
                pump_stream(op, job_id, Stream::Stderr, stderr, Arc::clone(&out)).await,
            )
        },
    )?;
    let code = status.code().unwrap_or(-1);
    notify(
        &out,
        "progress/complete",
        CompletionEvent {
            op,
            job_id,
            status: code,
        },
    )
    .await?;
    if !status.success() {
        return Err(AgentError::Cli {
            status: code,
            stderr,
        });
    }
    result.ok_or_else(|| AgentError::Other("Crab exited without a terminal result".into()))
}

/// Which stream a progress line came from. Surfaced to the UI so
/// it can colour stderr differently (most crab progress bars go
/// through stderr).
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug, Serialize)]
pub struct ProgressEvent<'a> {
    pub op: &'a str,
    pub job_id: &'a str,
    pub stream: Stream,
    pub line: String,
}

#[derive(Debug, Serialize)]
pub struct CompletionEvent<'a> {
    pub op: &'a str,
    pub job_id: &'a str,
    pub status: i32,
}

/// Run `crab <args>` in `cwd`, streaming every output line as a
/// `progress` notification tagged with `op` + `job_id`. Returns the
/// exit status on completion; a non-zero status is returned as
/// [`AgentError::Cli`] with the full captured stderr.
pub async fn run_streaming(
    op: &str,
    job_id: &str,
    cwd: Option<&std::path::Path>,
    args: &[&str],
    out: SharedStdout,
) -> Result<i32> {
    run_streaming_with_env(op, job_id, cwd, args, &[], out).await
}

/// Run `crab <args>` with additional environment variables injected.
/// Used by the structured add/push handlers to inject credentials from
/// the OS keychain without writing them to disk.
pub async fn run_streaming_with_env(
    op: &str,
    job_id: &str,
    cwd: Option<&std::path::Path>,
    args: &[&str],
    env_vars: &[(&str, &str)],
    out: SharedStdout,
) -> Result<i32> {
    let bin = resolve_crab_bin();
    debug!(op, job_id, bin = %bin.display(), "spawning crab");

    let mut cmd = Command::new(&bin);
    cmd.args(args);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.stdin(Stdio::null());
    cmd.kill_on_drop(true);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    for (key, value) in env_vars {
        cmd.env(key, value);
    }

    let mut child = cmd.spawn().map_err(AgentError::Io)?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let stdout_task = pump_stream(op, job_id, Stream::Stdout, stdout, Arc::clone(&out));
    let stderr_task = pump_stream(op, job_id, Stream::Stderr, stderr, Arc::clone(&out));

    let (status, captured_stderr, _) = tokio::join!(
        async { child.wait().await.map_err(AgentError::Io) },
        stderr_task,
        stdout_task,
    );
    let status = status?;
    let code = status.code().unwrap_or(-1);
    let stderr_text = captured_stderr;

    let _ = notify(
        &out,
        "progress/complete",
        CompletionEvent {
            op,
            job_id,
            status: code,
        },
    )
    .await;

    if !status.success() {
        return Err(AgentError::Cli {
            status: code,
            stderr: stderr_text,
        });
    }
    Ok(code)
}

async fn pump_stream<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    op: &str,
    job_id: &str,
    stream: Stream,
    reader: Option<R>,
    out: SharedStdout,
) -> String {
    let Some(reader) = reader else {
        return String::new();
    };
    let mut captured = String::new();
    let mut lines = BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if matches!(stream, Stream::Stderr) {
                    if !captured.is_empty() {
                        captured.push('\n');
                    }
                    captured.push_str(&line);
                }
                let event = ProgressEvent {
                    op,
                    job_id,
                    stream,
                    line,
                };
                if let Err(e) = notify(&out, "progress/line", event).await {
                    warn!(error = %e, "failed to emit progress line");
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => {
                warn!(error = %e, "stream read error");
                break;
            }
        }
    }
    captured
}

/// Resolve the selected CLI, preferring the runtime shipped beside this agent.
pub fn resolve_crab_bin() -> std::path::PathBuf {
    if let Some(explicit) = std::env::var_os("CRAB_BIN") {
        return explicit.into();
    }
    let name = if cfg!(windows) { "crab.exe" } else { "crab" };
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join(name);
        if sibling.is_file() {
            return sibling;
        }
    }
    name.into()
}
