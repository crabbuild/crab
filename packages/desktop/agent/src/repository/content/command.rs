use std::{
    io::Read,
    path::Path,
    process::Stdio,
    sync::mpsc,
    time::{Duration, Instant},
};

use crab_git::process::OwnedChild;

use super::*;

const MAX_PATCH_BYTES: u64 = 8 * 1024 * 1024;

pub(in crate::repository) async fn run(
    root: &Path,
    args: &[&str],
    scope: &ReadScope,
) -> Result<Vec<u8>> {
    let output = output(root, args, scope).await?;
    if !output.status.success() {
        return Err(AgentError::Cli {
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(output.stdout)
}

pub(crate) async fn output(
    root: &Path,
    args: &[&str],
    scope: &ReadScope,
) -> Result<std::process::Output> {
    let command = git_command(root, args);
    execute(command, scope, capture, false).await
}

fn git_command(root: &Path, args: &[&str]) -> std::process::Command {
    let mut command = std::process::Command::new("git");
    command
        .arg("--literal-pathspecs")
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // A captured repository owns its physical Git paths. Ambient helper-relative
    // selectors must not redirect reads after changing the working directory.
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_SHALLOW_FILE",
        "GIT_GRAFT_FILE",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
        "GIT_IMPLICIT_WORK_TREE",
        "GIT_CONFIG",
    ] {
        command.env_remove(name);
    }
    command
}

pub(super) async fn blob(
    root: &Path,
    oid: &str,
    start: u64,
    length: usize,
    size: u64,
    scope: &ReadScope,
) -> Result<Bytes> {
    let partial = start + (length as u64) < size;
    let command = git_command(root, &["--no-replace-objects", "cat-file", "blob", oid]);
    let read = move |mut stdout: std::process::ChildStdout| {
        let discarded = std::io::copy(&mut (&mut stdout).take(start), &mut std::io::sink())?;
        if discarded != start {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(std::io::Error::other)?;
        bytes.resize(length, 0);
        stdout.read_exact(&mut bytes)?;
        if !partial && stdout.read(&mut [0])? != 0 {
            return Err(std::io::Error::other("Git blob exceeds captured size"));
        }
        Ok(bytes)
    };
    let output = execute(command, scope, read, partial).await?;
    if !partial && !output.status.success() {
        return Err(AgentError::Cli {
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(output.stdout.into())
}

async fn execute(
    command: std::process::Command,
    scope: &ReadScope,
    read_stdout: impl FnOnce(std::process::ChildStdout) -> std::io::Result<Vec<u8>> + Send + 'static,
    partial: bool,
) -> Result<std::process::Output> {
    scope.check()?;
    let scope = scope.clone();
    // Keep the blocking owner joined through cancellation. Its pipe workers
    // cannot outlive the retained repository/source or a failed comparison.
    tokio::task::spawn_blocking(move || {
        scope.check()?;
        let mut child = OwnedChild::spawn(command)?;
        std::thread::scope(|workers| {
            let result = communicate(workers, &mut child, &scope, read_stdout, partial);
            if result.is_err() {
                child.stop()?;
            }
            result
        })
    })
    .await
    .map_err(std::io::Error::other)?
}

fn communicate<'a>(
    workers: &'a std::thread::Scope<'a, '_>,
    child: &mut OwnedChild,
    scope: &ReadScope,
    read_stdout: impl FnOnce(std::process::ChildStdout) -> std::io::Result<Vec<u8>> + Send + 'a,
    partial: bool,
) -> Result<std::process::Output> {
    let stdout = child
        .take_stdout()
        .ok_or_else(|| std::io::Error::other("Git stdout unavailable"))?;
    let stderr = child
        .take_stderr()
        .ok_or_else(|| std::io::Error::other("Git stderr unavailable"))?;
    let (sender, receiver) = mpsc::channel();
    {
        let sender = sender.clone();
        std::thread::Builder::new().spawn_scoped(workers, move || {
            let _ = sender.send((true, read_stdout(stdout)));
        })?;
    }
    {
        let sender = sender.clone();
        std::thread::Builder::new().spawn_scoped(workers, move || {
            let _ = sender.send((false, capture(stderr)));
        })?;
    }
    let mut stdout: Option<Vec<u8>> = None;
    let mut stderr: Option<Vec<u8>> = None;
    let mut status = None;
    let deadline = Instant::now() + Duration::from_secs(5 * 60);
    loop {
        scope.check()?;
        if Instant::now() >= deadline {
            return Err(AgentError::Other("Git read timed out".into()));
        }
        if status.is_none() && child.has_exited()? {
            status = Some(child.stop()?);
        }
        if let Some(status) = status
            && stdout.is_some()
            && stderr.is_some()
        {
            return Ok(std::process::Output {
                status,
                stdout: stdout
                    .take()
                    .ok_or_else(|| std::io::Error::other("Git stdout unavailable"))?,
                stderr: stderr
                    .take()
                    .ok_or_else(|| std::io::Error::other("Git stderr unavailable"))?,
            });
        }
        match receiver.recv_timeout(Duration::from_millis(10)) {
            Ok((is_stdout, result)) => {
                if is_stdout {
                    stdout = Some(result?);
                    // A selected prefix/range is complete before Git emits EOF.
                    // Stop its tree now; its resulting exit signal is expected.
                    if partial {
                        status = Some(child.stop()?);
                    }
                } else {
                    stderr = Some(result?);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(AgentError::Other(
                    "Git pipe worker stopped before completion".into(),
                ));
            }
        }
    }
}

fn capture(mut pipe: impl Read) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    (&mut pipe)
        .take(MAX_PATCH_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PATCH_BYTES {
        return Err(std::io::Error::other("Git read output exceeds 8 MiB"));
    }
    Ok(bytes)
}
