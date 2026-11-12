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
    let command = git_command(root, args)?;
    execute(command, scope, capture, None).await
}

fn git_command(root: &Path, args: &[&str]) -> Result<std::process::Command> {
    let mut command = crate::shellout::git_command()?.into_std();
    command
        .arg("--literal-pathspecs")
        .args(args)
        .current_dir(root)
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
    Ok(command)
}

pub(super) fn file_entry(bytes: &[u8], index: bool) -> Result<(Option<String>, Option<String>)> {
    if bytes.is_empty() {
        return Ok((None, None));
    }
    let entries = bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .collect::<Vec<_>>();
    if entries.len() != 1 {
        return Err(AgentError::Other(
            "Git selection requires one resolved file; resolve index conflicts first".into(),
        ));
    }
    let header = entries[0]
        .split(|byte| *byte == b'\t')
        .next()
        .ok_or_else(|| std::io::Error::other("invalid Git entry"))?;
    let fields = std::str::from_utf8(header)
        .map_err(std::io::Error::other)?
        .split(' ')
        .collect::<Vec<_>>();
    if fields.len() != 3 || (index && fields[2] != "0") {
        return Err(AgentError::Other(
            "Git selection requires a resolved entry".into(),
        ));
    }
    Ok((
        Some(fields[0].into()),
        Some(fields[if index { 1 } else { 2 }].into()),
    ))
}

pub(super) fn file_entry_at(
    entries: &[u8],
    path: &str,
    index: bool,
) -> Result<(Option<String>, Option<String>)> {
    let matching = entries
        .split_inclusive(|byte| *byte == 0)
        .filter(|record| {
            record
                .splitn(2, |byte| *byte == b'\t')
                .nth(1)
                .and_then(|name| name.strip_suffix(&[0]))
                == Some(path.as_bytes())
        })
        .flatten()
        .copied()
        .collect::<Vec<_>>();
    file_entry(&matching, index)
}

pub(super) async fn blob(
    root: &Path,
    oid: &str,
    start: u64,
    length: usize,
    size: u64,
    scope: &ReadScope,
) -> Result<Bytes> {
    if start > size || length as u64 > size - start {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Git range exceeds captured size",
        )
        .into());
    }
    let mut selected = SelectedBytes {
        skip: start,
        remaining: length,
        bytes: Vec::new(),
    };
    selected
        .bytes
        .try_reserve_exact(length)
        .map_err(std::io::Error::other)?;
    let command = blob_batch_command(root, oid)?;
    let captured_oid = oid.to_owned();
    let cancellation = scope.clone();
    let read = move |stdout: std::process::ChildStdout| {
        // A prefix cannot authenticate a Git OID. Verify the complete stream
        // before exposing selected bytes, retaining only the requested range.
        crab_git::batch::copy_blob(stdout, &captured_oid, size, &mut selected, &|| {
            cancellation.check().is_err()
        })?;
        Ok(selected.bytes)
    };
    let output = execute(command, scope, read, None).await?;
    if !output.status.success() {
        return Err(AgentError::Cli {
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(output.stdout.into())
}

struct SelectedBytes {
    skip: u64,
    remaining: usize,
    bytes: Vec<u8>,
}

impl std::io::Write for SelectedBytes {
    fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
        let skip = self.skip.min(input.len() as u64) as usize;
        self.skip -= skip as u64;
        let count = self.remaining.min(input.len() - skip);
        self.bytes.extend_from_slice(&input[skip..skip + count]);
        self.remaining -= count;
        Ok(input.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) async fn blob_size(root: &Path, oid: &str, scope: &ReadScope) -> Result<u64> {
    String::from_utf8(
        run(
            root,
            &["--no-replace-objects", "cat-file", "-s", oid],
            scope,
        )
        .await?,
    )?
    .trim()
    .parse()
    .map_err(|error| std::io::Error::other(error).into())
}

pub(super) async fn copy_blob(
    root: &Path,
    oid: &str,
    size: u64,
    mut output: impl std::io::Write + Send + 'static,
    scope: &ReadScope,
) -> Result<()> {
    let command = blob_batch_command(root, oid)?;
    let oid = oid.to_owned();
    let cancellation = scope.clone();
    let read = move |stdout: std::process::ChildStdout| {
        crab_git::batch::copy_blob(stdout, &oid, size, &mut output, &|| {
            cancellation.check().is_err()
        })?;
        Ok(Vec::new())
    };
    let output = execute(command, scope, read, None).await?;
    if !output.status.success() {
        return Err(AgentError::Cli {
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(())
}

fn blob_batch_command(root: &Path, oid: &str) -> Result<std::process::Command> {
    use std::io::{Seek, Write};
    let mut input = tempfile::tempfile()?;
    writeln!(input, "{oid}")?;
    input.rewind()?;
    let mut command = git_command(root, &["--no-replace-objects", "cat-file", "--batch"])?;
    command.stdin(Stdio::from(input));
    Ok(command)
}

pub(super) async fn index_path(root: &Path, scope: &ReadScope) -> Result<PathBuf> {
    let source = run(
        root,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        scope,
    )
    .await?;
    let source = source.strip_suffix(b"\n").unwrap_or(&source);
    let source = source.strip_suffix(b"\r").unwrap_or(source);
    Ok(PathBuf::from(
        std::str::from_utf8(source).map_err(std::io::Error::other)?,
    ))
}

pub(super) async fn converted_blob_to_file(
    index: &super::publication::IndexFile,
    private_index: &Path,
    mut file: std::fs::File,
    scope: &ReadScope,
    mut progress: impl FnMut(u64) -> Result<()> + Send + 'static,
) -> Result<()> {
    // Git can read a Windows verbatim index path as an empty index. Scratch is
    // inside this worktree; keep native ownership paths canonical and give Git
    // the owned index relative to its working directory.
    let root = tokio::fs::canonicalize(&index.root).await?;
    let private_index = private_index
        .strip_prefix(root)
        .map_err(std::io::Error::other)?;
    let mut command = git_command(
        &index.root,
        &["ls-files", "--stage", "-z", "--", &index.path],
    )?;
    command.env("GIT_INDEX_FILE", private_index);
    let selected = execute(command, scope, capture, None).await?;
    if !selected.status.success() {
        return Err(AgentError::Cli {
            status: selected.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&selected.stderr).into_owned(),
        });
    }
    let entry = file_entry_at(&selected.stdout, &index.path, true)?;
    if entry.0.as_deref() != Some(&index.mode) || entry.1.as_deref() != Some(&index.oid) {
        return Err(changed());
    }
    let path = format!(":{}", index.path);
    // The index expression makes Git load index-backed attributes when working
    // attributes are absent. A raw OID with --path skips that on supported Git.
    let mut command = git_command(
        &index.root,
        &["--no-replace-objects", "cat-file", "--filters", &path],
    )?;
    command.env("GIT_INDEX_FILE", private_index);
    let observed = file.try_clone()?;
    let observer = move || progress(observed.metadata()?.len());
    let read = move |mut stdout: std::process::ChildStdout| {
        std::io::copy(&mut stdout, &mut file)?;
        Ok(Vec::new())
    };
    let output = execute(command, scope, read, Some(Box::new(observer))).await?;
    if !output.status.success() {
        return Err(AgentError::Cli {
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(())
}

async fn execute(
    command: std::process::Command,
    scope: &ReadScope,
    read_stdout: impl FnOnce(std::process::ChildStdout) -> std::io::Result<Vec<u8>> + Send + 'static,
    mut progress: Option<Box<dyn FnMut() -> Result<()> + Send>>,
) -> Result<std::process::Output> {
    scope.check()?;
    let scope = scope.clone();
    // Keep the blocking owner joined through cancellation. Its pipe workers
    // cannot outlive the retained repository/source or a failed comparison.
    tokio::task::spawn_blocking(move || {
        scope.check()?;
        let mut child = OwnedChild::spawn(command)?;
        std::thread::scope(|workers| {
            let result = communicate(workers, &mut child, &scope, read_stdout, &mut progress);
            if result.is_err() {
                child.stop()?;
            }
            // The verifier can observe cancellation before the pipe owner does.
            // Preserve typed cancellation after stopping its child; actual
            // checksum and other I/O failures must retain their own identity.
            if matches!(&result, Err(AgentError::Io(error)) if error.kind() == std::io::ErrorKind::Interrupted)
                && scope.cancelled_read()
            {
                scope.check()?;
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
    progress: &mut Option<Box<dyn FnMut() -> Result<()> + Send>>,
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
    let mut reported = Instant::now();
    loop {
        scope.check()?;
        if reported.elapsed() >= Duration::from_millis(250) {
            if let Some(progress) = progress {
                progress()?;
            }
            reported = Instant::now();
        }
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
            if let Some(progress) = progress {
                progress()?;
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn verifier_interruption_preserves_typed_cancellation() -> Result<()> {
        let scope = ReadScope::default();
        let reader = scope.clone();
        let stop = scope.clone();
        let command = git_command(Path::new(env!("CARGO_MANIFEST_DIR")), &["--version"])?;
        let read = move |_stdout| {
            // The observer runs after the owner's cancellation poll. Hold the
            // reader until that signal so interruption can arrive on its pipe
            // channel before the owner polls again.
            while !reader.cancellation_token().is_cancelled() {
                std::thread::sleep(Duration::from_millis(1));
            }
            reader
                .check()
                .map_err(|_| std::io::Error::from(std::io::ErrorKind::Interrupted))?;
            Ok(Vec::new())
        };
        let observer = move || {
            stop.cancel();
            Ok(())
        };
        let result = execute(command, &scope, read, Some(Box::new(observer))).await;
        let error = result
            .err()
            .ok_or_else(|| std::io::Error::other("expected cancellation"))?;
        assert!(error.is_cancelled(), "{error}");
        Ok(())
    }
}
