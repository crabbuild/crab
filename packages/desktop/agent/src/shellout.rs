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

use std::io;
use std::process::{ExitStatus, Output, Stdio};
use std::sync::Arc;

use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tracing::{debug, warn};

use crate::{AgentError, Result, SharedStdout, notify};

pub(crate) fn git_command() -> Result<Command> {
    let mut command = Command::new("git");
    // The agent owns stdin for NDJSON. Git must never inherit that pipe;
    // commands that consume data supply their own stdin explicitly.
    command.stdin(Stdio::null());
    // Inspection must not take optional index-refresh locks while the user
    // stages or commits. Git's mandatory mutation locks remain enabled.
    command.env("GIT_OPTIONAL_LOCKS", "0");
    // Status and diff can invoke clean filters too. Every Git child needs the
    // selected repository's credentials, without changing the process environment.
    crate::storage_auth::configure_command(&mut command)?;
    Ok(command)
}

pub(crate) async fn output(command: &mut Command) -> Result<Output> {
    #[cfg(unix)]
    if crate::native_process::mutation::active() {
        let (status, stdout, stderr) = piped(command, async |_, mut stdout| {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes).await?;
            Ok(bytes)
        })
        .await?;
        return Ok(Output {
            status,
            stdout: stdout?,
            stderr,
        });
    }
    Ok(command.output().await?)
}

pub(crate) async fn piped<T>(
    command: &mut Command,
    exchange: impl AsyncFnOnce(Option<ChildStdin>, ChildStdout) -> Result<T>,
) -> Result<(ExitStatus, Result<T>, Vec<u8>)> {
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    let (mut child, mut owner) = crate::native_process::mutation::CommandScope::spawn(command)?;
    #[cfg(not(unix))]
    let mut child = command.spawn()?;
    let input = child.stdin.take();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("Command stdout unavailable"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("Command stderr unavailable"))?;
    let (status, (result, bytes)) = {
        let wait = async {
            #[cfg(unix)]
            if let Some(owner) = &owner {
                return owner.wait(&mut child).await;
            }
            child.wait().await
        };
        let drain = async {
            let errors = async {
                let mut bytes = Vec::new();
                stderr.read_to_end(&mut bytes).await?;
                Ok::<_, io::Error>(bytes)
            };
            let (result, bytes) = tokio::join!(exchange(input, stdout), errors);
            Ok::<_, io::Error>((result, bytes?))
        };
        tokio::pin!(wait, drain);
        tokio::select! {
            result = &mut drain => (wait.await?, result?),
            status = &mut wait => (status?, tokio::time::timeout(std::time::Duration::from_secs(5), drain).await.map_err(io::Error::other)??),
        }
    };
    #[cfg(unix)]
    if let Some(owner) = &mut owner {
        owner.confirm_closed();
    }
    Ok((status, result, bytes))
}

pub fn command() -> Result<Command> {
    let mut command = Command::new(resolve_crab_bin());
    crate::storage_auth::configure_command(&mut command)?;
    Ok(command)
}

pub async fn run_json(
    cwd: Option<&std::path::Path>,
    args: &[&str],
    env_vars: &[(String, String)],
) -> Result<serde_json::Value> {
    let mut command = self::command()?;
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
    event_method: &str,
    out: SharedStdout,
    cancel: Option<tokio_util::sync::WaitForCancellationFutureOwned>,
) -> Result<serde_json::Value> {
    let mut command = command()?;
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = crab_git::process::OwnedChild::spawn(command.into_std())?;
    let stdout = child
        .take_stdout()
        .ok_or_else(|| AgentError::Other("Crab stdout unavailable".into()))?;
    let stdout = tokio::process::ChildStdout::from_std(stdout)?;
    let stderr = child
        .take_stderr()
        .map(tokio::process::ChildStderr::from_std)
        .transpose()?;
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
    let wait = async {
        let cancellation = async {
            match cancel {
                Some(cancel) => cancel.await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(cancellation);
        let mut cancelled = false;
        loop {
            // Retain the process identity until tree cleanup and reaping.
            // SIGTERM lets the workflow supervisor stop its own child groups;
            // Windows uses the shared owner's job-object termination instead.
            if child.has_exited()? {
                return Ok::<_, AgentError>((child.stop()?, cancelled));
            }
            tokio::select! {
                _ = &mut cancellation, if !cancelled => { child.request_shutdown()?; cancelled = true; }
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
            }
        }
    };
    let ((status, cancelled), result, stderr) = tokio::try_join!(wait, read, async {
        Ok::<_, AgentError>(pump_stream(op, job_id, Stream::Stderr, stderr, Arc::clone(&out)).await)
    },)?;
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
    if cancelled {
        return Err(AgentError::OpCancelled {
            op_id: job_id.into(),
        });
    }
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
/// Request-scoped provider credentials are applied by `command`; callers may
/// add operation-specific variables without modifying the agent environment.
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

    let mut cmd = command()?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[tokio::test]
    async fn inspection_keeps_index_bytes_unchanged_after_a_metadata_change() {
        async fn git(root: &std::path::Path, args: &[&str]) -> Vec<u8> {
            let output = git_command()
                .expect("command")
                .args(args)
                .current_dir(root)
                .output()
                .await
                .expect("Git runs");
            assert!(output.status.success(), "{:?}", output);
            output.stdout
        }
        let directory = tempfile::tempdir().expect("repository");
        let root = directory.path();
        git(root, &["init", "--quiet"]).await;
        git(root, &["config", "user.name", "Fixture"]).await;
        git(root, &["config", "user.email", "test@example.invalid"]).await;
        std::fs::write(root.join("file.txt"), "original\n").expect("file");
        git(root, &["add", "file.txt"]).await;
        git(
            root,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        )
        .await;
        let before = std::fs::read(root.join(".git/index")).expect("index");
        // Same bytes with a different inode require a stat-cache refresh.
        std::fs::rename(root.join("file.txt"), root.join("previous.txt")).expect("replace");
        std::fs::write(root.join("file.txt"), "original\n").expect("replacement");
        let status = git(root, &["status", "--porcelain", "--", "file.txt"]).await;
        assert_eq!(
            (
                status,
                std::fs::read(root.join(".git/index")).expect("index")
            ),
            (Vec::new(), before)
        );
    }

    #[tokio::test]
    async fn git_commands_carry_scoped_storage_without_leaking_to_siblings() {
        let connection: crate::storage_auth::Connection = serde_json::from_value(serde_json::json!({
            "bucket": "fixture-bucket", "region": "us-east-1", "endpoint": "http://127.0.0.1:9",
            "credentials": { "provider": "s3", "access_key_id": "fixture-access", "secret_access_key": "fixture-secret", "session_token": null }
        })).expect("fixture connection");
        crate::storage_auth::scope(Some(connection), async {
            let command = git_command().expect("scoped command");
            let overrides = command
                .as_std()
                .get_envs()
                .collect::<std::collections::HashMap<_, _>>();
            assert_eq!(
                (
                    overrides.get(OsStr::new("AWS_ACCESS_KEY_ID")),
                    overrides.get(OsStr::new("AWS_ENDPOINT_URL")),
                    overrides.get(OsStr::new("AWS_SESSION_TOKEN"))
                ),
                (
                    Some(&Some(OsStr::new("fixture-access"))),
                    Some(&Some(OsStr::new("http://127.0.0.1:9"))),
                    Some(&None)
                ),
            );
        })
        .await;
        let command = git_command().expect("unscoped command");
        assert!(
            !command
                .as_std()
                .get_envs()
                .any(|(name, _)| name == "AWS_ACCESS_KEY_ID")
        );
    }
}
