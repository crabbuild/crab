use super::ImportError;
use std::{io, path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};
use tokio_util::sync::CancellationToken;
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MAX_GIT_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_PUSH_ATTEMPTS: usize = 3;
const PUSH_RETRY_DELAY: Duration = Duration::from_secs(2);

pub(super) async fn mirror_size(path: &Path) -> Result<u64, ImportError> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || directory_size(&path))
        .await
        .map_err(|error| ImportError::Io(io::Error::other(error)))?
        .map_err(ImportError::Io)
}

fn directory_size(path: &Path) -> io::Result<u64> {
    let metadata = std::fs::symlink_metadata(path)?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Git mirror contains a symbolic link",
        ));
    }
    if file_type.is_file() {
        return Ok(metadata.len());
    }
    if !file_type.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Git mirror contains a special file",
        ));
    }
    std::fs::read_dir(path)?.try_fold(0_u64, |total, entry| {
        let size = directory_size(&entry?.path())?;
        total
            .checked_add(size)
            .ok_or_else(|| io::Error::other("Git mirror size overflowed"))
    })
}

pub(super) async fn push_mirror(
    mirror: &Path,
    destination: &str,
    internal_key: &str,
    cancellation: &CancellationToken,
) -> Result<(), ImportError> {
    let mut attempt = 0;
    loop {
        let mut push = Command::new("git");
        push.current_dir(mirror)
            .args(["push", "--quiet", "--atomic", "--mirror"])
            .arg(destination);
        configure_git(&mut push, None, Some(internal_key));
        match run_git(push, cancellation, Some(internal_key)).await {
            Ok(_) => return Ok(()),
            Err(error)
                if matches!(&error, ImportError::Git(_)) && attempt + 1 < MAX_PUSH_ATTEMPTS =>
            {
                attempt += 1;
                tracing::warn!(
                    attempt,
                    max_attempts = MAX_PUSH_ATTEMPTS,
                    error = ?error,
                    "Git import publication failed; retrying"
                );
                let delay = PUSH_RETRY_DELAY
                    .checked_mul(attempt as u32)
                    .unwrap_or(PUSH_RETRY_DELAY);
                tokio::select! {
                    () = cancellation.cancelled() => return Err(ImportError::Cancelled),
                    () = tokio::time::sleep(delay) => {}
                }
            }
            Err(error) => return Err(error),
        }
    }
}

pub(super) async fn mirror_refs(
    path: &Path,
    cancellation: &CancellationToken,
) -> Result<Vec<String>, ImportError> {
    let mut refs = Command::new("git");
    refs.current_dir(path)
        .args(["for-each-ref", "--format=%(refname)"]);
    configure_git(&mut refs, None, None);
    let output = run_git(refs, cancellation, None).await?;
    if output.len() > MAX_GIT_OUTPUT_BYTES {
        return Err(ImportError::Git(
            "Git source advertised too much ref metadata".into(),
        ));
    }
    Ok(output
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect())
}

pub(super) async fn default_branch(
    path: &Path,
    cancellation: &CancellationToken,
) -> Result<String, ImportError> {
    let mut symbolic = Command::new("git");
    symbolic
        .current_dir(path)
        .args(["symbolic-ref", "--short", "HEAD"]);
    configure_git(&mut symbolic, None, None);
    match run_git(symbolic, cancellation, None).await {
        Ok(branch) => {
            let branch = branch.trim();
            if valid_branch(branch) {
                return Ok(branch.to_owned());
            }
        }
        Err(ImportError::Git(_)) => {}
        Err(error) => return Err(error),
    }

    let mut refs = Command::new("git");
    refs.current_dir(path)
        .args(["for-each-ref", "--format=%(refname:strip=2)", "refs/heads"]);
    configure_git(&mut refs, None, None);
    let refs = run_git(refs, cancellation, None).await?;
    let mut branches = refs.lines().filter(|branch| valid_branch(branch));
    branches
        .find(|branch| *branch == "main")
        .or_else(|| {
            let mut fallback = refs.lines().filter(|branch| valid_branch(branch));
            fallback.find(|branch| *branch == "master")
        })
        .or_else(|| refs.lines().find(|branch| valid_branch(branch)))
        .map(str::to_owned)
        .ok_or(ImportError::Git("Git source has no branch".into()))
}

pub(super) fn configure_git(
    command: &mut Command,
    source_token: Option<&str>,
    internal_key: Option<&str>,
) {
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_git_config())
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "credential.helper")
        .env("GIT_CONFIG_VALUE_0", "")
        .env("GIT_CONFIG_KEY_1", "http.followRedirects")
        .env("GIT_CONFIG_VALUE_1", "false");
    if let Some(token) = source_token {
        command
            .env("GIT_CONFIG_COUNT", "3")
            .env("GIT_CONFIG_KEY_2", "http.extraHeader")
            .env(
                "GIT_CONFIG_VALUE_2",
                format!("Authorization: Bearer {token}"),
            );
    }
    if let Some(key) = internal_key {
        command
            .env("GIT_CONFIG_COUNT", "3")
            .env("GIT_CONFIG_KEY_2", "http.extraHeader")
            .env(
                "GIT_CONFIG_VALUE_2",
                format!("X-Crab-Internal-Import: {key}"),
            );
    }
}

fn empty_git_config() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

pub(super) async fn run_git(
    mut command: Command,
    cancellation: &CancellationToken,
    secret: Option<&str>,
) -> Result<String, ImportError> {
    command
        .kill_on_drop(true)
        .env_remove("GIT_TRACE")
        .env_remove("GIT_TRACE_CURL")
        .env_remove("GIT_CURL_VERBOSE")
        .env_remove("GIT_TRACE_PACKET")
        .env("LC_ALL", "C")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(ImportError::Io)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ImportError::Io(io::Error::other("git stdout was not piped for import")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ImportError::Io(io::Error::other("git stderr was not piped for import")))?;
    let (status, stdout, stderr) = tokio::select! {
        result = tokio::time::timeout(GIT_COMMAND_TIMEOUT, async {
            let (stdout, stderr) = tokio::try_join!(
                read_git_output(stdout),
                read_git_output(stderr),
            )?;
            let status = child.wait().await.map_err(GitOutputError::Io)?;
            Ok::<_, GitOutputError>((status, stdout, stderr))
        }) => {
            match result {
                Ok(Ok(output)) => output,
                Ok(Err(GitOutputError::Limit)) => {
                    return Err(ImportError::Git(
                        "Git command output exceeded the import limit".into(),
                    ));
                }
                Ok(Err(GitOutputError::Io(error))) => return Err(ImportError::Io(error)),
                Err(_) => return Err(ImportError::Timeout),
            }
        }
        () = cancellation.cancelled() => return Err(ImportError::Cancelled),
    };
    if !status.success() {
        let mut detail = String::from_utf8_lossy(&stderr).trim().to_owned();
        if let Some(secret) = secret.filter(|secret| !secret.is_empty()) {
            detail = detail.replace(secret, "[redacted]");
        }
        if detail.is_empty() {
            detail = "git exited unsuccessfully".into();
        }
        detail.truncate(512);
        return Err(ImportError::Git(detail));
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

#[derive(Debug)]
enum GitOutputError {
    Io(io::Error),
    Limit,
}

async fn read_git_output<R>(mut reader: R) -> Result<Vec<u8>, GitOutputError>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer).await.map_err(GitOutputError::Io)?;
        if read == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(read) > MAX_GIT_OUTPUT_BYTES {
            return Err(GitOutputError::Limit);
        }
        output.extend_from_slice(&buffer[..read]);
    }
}

fn valid_branch(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with("refs/")
        && crab_git::validate_push_refname(&format!("refs/heads/{value}")).is_ok()
}
