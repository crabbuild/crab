//! Checked Git snapshots and owned pull subprocesses.

use std::io::{self, Read, Write};
use std::path::{Component, Path};
use std::process::{ChildStdin, Command};

use tokio_util::sync::CancellationToken;

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::git::process::{self, GIT_ENV_REMOVALS, MAX_CAPTURE_BYTES, capture_output};

pub(super) async fn pull(
    root: &Path,
    remote: &str,
    branch: Option<&str>,
    show_progress: bool,
    cancel: &CancellationToken,
) -> Result<Vec<String>> {
    check_cancelled(cancel)?;
    let root = root.to_owned();
    let remote = remote.to_owned();
    let branch = branch.map(str::to_owned);
    let cancel = cancel.child_token();
    // Dropping the async caller must also stop its blocking worker. The
    // child token revokes only this pull, not its caller or sibling work.
    let _cancel_on_drop = cancel.clone().drop_guard();
    tokio::task::spawn_blocking(move || {
        pull_in(&root, &remote, branch.as_deref(), show_progress, &cancel)
    })
    .await
    .map_err(|error| CrabError::Io(io::Error::other(error)))?
}

fn pull_in(
    root: &Path,
    remote: &str,
    branch: Option<&str>,
    show_progress: bool,
    cancel: &CancellationToken,
) -> Result<Vec<String>> {
    let before = head(root, cancel)?;
    let mut args = vec!["pull"];
    if show_progress {
        args.push("--progress");
    }
    args.extend(["--", remote]);
    args.extend(branch);
    let output = run(root, &args, show_progress, cancel)?;
    if output.status.success() {
        let after = head(root, cancel)?.ok_or_else(|| invalid("successful pull has no HEAD"))?;
        return changed_paths(root, before.as_deref(), &after, cancel);
    }

    // Git writes merge diagnostics to stdout and fetch progress to stderr.
    // Neither localized text nor an unrelated path containing "conflict"
    // establishes index state; ask Git for the actual unmerged paths.
    let unmerged = checked(
        root,
        &[
            "diff",
            "--cached",
            "--no-ext-diff",
            "--no-textconv",
            "--name-only",
            "--diff-filter=U",
            "-z",
            "--",
        ],
        cancel,
    )?;
    let files = parse_paths(&unmerged)?;
    if !files.is_empty() {
        return Err(CrabError::PullConflict {
            count: files.len(),
            files,
        });
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if let Some(reason) = transport_error(&stderr) {
        return Err(CrabError::PullRemoteUnreachable {
            remote: remote.to_owned(),
            reason: reason.to_owned(),
        });
    }
    Err(command_error("pull", &output))
}

fn head(root: &Path, cancel: &CancellationToken) -> Result<Option<String>> {
    let output = run(
        root,
        &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
        false,
        cancel,
    )?;
    if output.status.success() {
        let oid = single_line(&output.stdout)?;
        if !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(invalid("Git returned an invalid HEAD object ID"));
        }
        return Ok(Some(oid.to_owned()));
    }
    // A failed rev-parse can mean corruption, not just an unborn branch.
    // Only an exact missing symbolic target proves the initial-pull case.
    let symbolic = run(root, &["symbolic-ref", "--quiet", "HEAD"], false, cancel)?;
    if symbolic.status.success() {
        let name = single_line(&symbolic.stdout)?;
        let exists = run(
            root,
            &["show-ref", "--verify", "--quiet", "--", name],
            false,
            cancel,
        )?;
        if exists.status.code() == Some(1) {
            return Ok(None);
        }
        if !exists.status.success() {
            return Err(command_error("show-ref", &exists));
        }
    }
    Err(command_error("rev-parse HEAD", &output))
}

fn changed_paths(
    root: &Path,
    before: Option<&str>,
    after: &str,
    cancel: &CancellationToken,
) -> Result<Vec<String>> {
    if before == Some(after) {
        return Ok(Vec::new());
    }
    let bytes = if let Some(before) = before {
        checked(
            root,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "--name-only",
                "-z",
                before,
                after,
                "--",
            ],
            cancel,
        )?
    } else {
        checked(
            root,
            &["ls-tree", "-r", "--name-only", "-z", after, "--"],
            cancel,
        )?
    };
    parse_paths(&bytes)
}

fn checked(root: &Path, args: &[&str], cancel: &CancellationToken) -> Result<Vec<u8>> {
    let output = run(root, args, false, cancel)?;
    if !output.status.success() {
        return Err(command_error(args[0], &output));
    }
    Ok(output.stdout)
}

fn run(
    root: &Path,
    args: &[&str],
    show_progress: bool,
    cancel: &CancellationToken,
) -> Result<process::Output<Vec<u8>>> {
    let mut command = Command::new("git");
    command.args(args).current_dir(root).env("LC_ALL", "C");
    for key in GIT_ENV_REMOVALS {
        command.env_remove(key);
    }
    process::run_with_stderr(
        command,
        cancel,
        None::<fn(ChildStdin) -> Result<()>>,
        |stdout| Ok(capture_output(stdout, MAX_CAPTURE_BYTES)?),
        |stderr| {
            if show_progress {
                Ok(capture_output(
                    ProgressReader {
                        source: stderr,
                        sink: io::stderr(),
                    },
                    MAX_CAPTURE_BYTES,
                )?)
            } else {
                Ok(capture_output(stderr, MAX_CAPTURE_BYTES)?)
            }
        },
    )
}

struct ProgressReader<R, W> {
    source: R,
    sink: W,
}

impl<R: Read, W: Write> Read for ProgressReader<R, W> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.source.read(buffer)?;
        // Do not hold the terminal lock while waiting for the child pipe.
        self.sink.write_all(&buffer[..count])?;
        Ok(count)
    }
}

fn single_line(bytes: &[u8]) -> Result<&str> {
    let value = std::str::from_utf8(bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    value
        .strip_suffix('\n')
        .filter(|value| !value.is_empty() && !value.contains(['\r', '\n', '\0']))
        .ok_or_else(|| invalid("Git returned a malformed single-line response"))
}

pub(super) fn parse_paths(bytes: &[u8]) -> Result<Vec<String>> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let bytes = bytes
        .strip_suffix(&[0])
        .ok_or_else(|| invalid("Git path inventory is not NUL terminated"))?;
    let mut paths = Vec::new();
    let mut allocation = 0_u64;
    for raw in bytes.split(|byte| *byte == 0) {
        let path = std::str::from_utf8(raw)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if path.is_empty()
            || path
                .split('/')
                .any(|part| matches!(part, "" | "." | ".." | ".git"))
            || Path::new(path)
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(invalid("Git path inventory contains a non-relative path"));
        }
        allocation = allocation.saturating_add((std::mem::size_of::<String>() + path.len()) as u64);
        if allocation > MAX_CAPTURE_BYTES {
            return Err(invalid("Git path inventory exceeds allocation limit"));
        }
        paths.push(path.to_owned());
    }
    Ok(paths)
}

fn transport_error(stderr: &str) -> Option<&str> {
    stderr.lines().map(str::trim).find(|line| {
        let lower = line.to_ascii_lowercase();
        lower.starts_with("fatal: unable to access ")
            || lower.starts_with("fatal: could not read from remote repository")
            || lower.starts_with("ssh: connect to host ")
            || lower.starts_with("ssh: could not resolve hostname ")
    })
}

fn command_error(operation: &str, output: &process::Output<Vec<u8>>) -> CrabError {
    let status = output
        .status
        .code()
        .map_or_else(|| "signal".to_owned(), |code| code.to_string());
    CrabError::Io(io::Error::other(format!(
        "git {operation} failed (exit {status}): {}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    )))
}

fn invalid(message: &'static str) -> CrabError {
    io::Error::new(io::ErrorKind::InvalidData, message).into()
}

#[cfg(test)]
mod tests;
