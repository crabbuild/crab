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
    execute(command, scope, capture, false, None).await
}

fn git_command(root: &Path, args: &[&str]) -> Result<std::process::Command> {
    let mut command = std::process::Command::new("git");
    // Captured reads must not refresh index stats during an explicit mutation.
    command
        .arg("--literal-pathspecs")
        .args(args)
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
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
    if let Some(connection) = crate::storage_auth::current() {
        connection.options()?.configure_command(&mut command)?;
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
    let partial = start + (length as u64) < size;
    let command = git_command(root, &["--no-replace-objects", "cat-file", "blob", oid])?;
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
    let output = execute(command, scope, read, partial, None).await?;
    if !partial && !output.status.success() {
        return Err(AgentError::Cli {
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(output.stdout.into())
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

pub(super) async fn blob_to_file(
    root: &Path,
    oid: &str,
    size: u64,
    mut file: std::fs::File,
    scope: &ReadScope,
) -> Result<()> {
    let command = git_command(root, &["--no-replace-objects", "cat-file", "blob", oid])?;
    let read = move |mut stdout: std::process::ChildStdout| {
        let copied = std::io::copy(&mut (&mut stdout).take(size), &mut file)?;
        if copied != size || stdout.read(&mut [0])? != 0 {
            return Err(std::io::Error::other("Git blob differs from captured size"));
        }
        Ok(Vec::new())
    };
    let output = execute(command, scope, read, false, None).await?;
    if !output.status.success() {
        return Err(AgentError::Cli {
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(())
}

pub(super) async fn verify_blob(
    root: &Path,
    oid: &str,
    size: u64,
    scope: &ReadScope,
) -> Result<()> {
    use std::io::{BufRead, Seek, Write};
    let mut input = tempfile::tempfile()?;
    writeln!(input, "{oid}")?;
    input.rewind()?;
    let mut command = git_command(root, &["--no-replace-objects", "cat-file", "--batch"])?;
    command.stdin(Stdio::from(input));
    let oid = oid.to_owned();
    let cancelled = scope.clone();
    let read = move |stdout| {
        let mut reader = std::io::BufReader::new(stdout);
        let mut header = Vec::new();
        (&mut reader).take(128).read_until(b'\n', &mut header)?;
        if header != format!("{oid} blob {size}\n").as_bytes() {
            return Err(std::io::Error::other(
                "Git blob differs from the captured index source",
            ));
        }
        crab_git::batch::visit_small_blobs(
            std::io::Read::chain(std::io::Cursor::new(header), reader),
            [oid.as_str()],
            0,
            &|| cancelled.check().is_err(),
            |_, _| Ok(()),
        )?;
        Ok(Vec::new())
    };
    let output = execute(command, scope, read, false, None).await?;
    if !output.status.success() {
        return Err(AgentError::Cli {
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(())
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

pub(super) async fn checkout_index(
    index: &super::publication::IndexFile,
    source_index: &Path,
    directory: &Path,
    scope: &ReadScope,
    mut progress: impl FnMut(u64) -> Result<()> + Send + 'static,
) -> Result<PathBuf> {
    tokio::fs::copy(source_index, directory.join("index")).await?;
    scope.check()?;
    // The physical index path comes from native Git, including its Windows
    // spelling. Keep private checkout files inside that metadata directory.
    let private_index = directory.join("index");
    let mut command = git_command(
        &index.root,
        &["ls-files", "--stage", "-z", "--", &index.path],
    )?;
    command.env("GIT_INDEX_FILE", &private_index);
    let selected = execute(command, scope, capture, false, None).await?;
    if !selected.status.success() {
        return Err(AgentError::Cli {
            status: selected.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&selected.stderr).into_owned(),
        });
    }
    let entry = file_entry_at(&selected.stdout, &index.path, true)?;
    if entry.0.as_deref() != Some(index.mode.as_str())
        || entry.1.as_deref() != Some(index.oid.as_str())
    {
        return Err(changed());
    }
    let prefix = format!("--prefix={}/output/", directory.display());
    let mut command = git_command(
        &index.root,
        &[
            "--no-replace-objects",
            "checkout-index",
            "--ignore-skip-worktree-bits",
            &prefix,
            "--",
            &index.path,
        ],
    )?;
    command.env("GIT_INDEX_FILE", private_index);
    let output_path = directory.join("output").join(&index.path);
    let observed = output_path.clone();
    let observer = move || match std::fs::symlink_metadata(&observed) {
        Ok(metadata) if metadata.is_file() => progress(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => Err(changed()),
    };
    let output = execute(command, scope, capture, false, Some(Box::new(observer))).await?;
    if !output.status.success() {
        return Err(AgentError::Cli {
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(output_path)
}

async fn execute(
    command: std::process::Command,
    scope: &ReadScope,
    read_stdout: impl FnOnce(std::process::ChildStdout) -> std::io::Result<Vec<u8>> + Send + 'static,
    partial: bool,
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
            let result = communicate(
                workers,
                &mut child,
                &scope,
                read_stdout,
                partial,
                &mut progress,
            );
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
