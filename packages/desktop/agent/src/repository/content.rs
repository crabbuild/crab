use std::{process::Stdio, time::SystemTime};

use crab_sdk::operation::ReadOptions;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Mutex;

use super::*;

pub(crate) const MAX_RANGE_BYTES: u32 = 8 * 1024 * 1024;

pub(crate) struct FileSource {
    source: Source,
    pub size: u64,
}

enum Source {
    Local {
        file: Mutex<tokio::fs::File>,
        modified: Option<SystemTime>,
    },
    Git {
        root: PathBuf,
        oid: String,
    },
    Pointer {
        root: PathBuf,
        bytes: Bytes,
        connection: Option<DirectStoreOptions>,
        remote: Option<String>,
    },
    Remote {
        snapshot: Snapshot,
        path: GitPath,
    },
}

impl FileSource {
    pub(crate) async fn capture(
        repo: &BrowseSession,
        path: &str,
        rev: Option<&str>,
    ) -> Result<Self> {
        let validated = GitPath::new(path.as_bytes().to_vec())?;
        let Some(root) = &repo.work_dir else {
            let snapshot = repo.snapshot(rev).await?;
            let metadata = snapshot.file_metadata(validated.clone()).await?;
            return Ok(Self {
                size: metadata.size,
                source: Source::Remote {
                    snapshot,
                    path: validated,
                },
            });
        };
        if let Some(rev) = rev {
            // Resolve the index or revision once; a later checkout cannot redirect reads.
            let object = format!("{rev}:{path}");
            let oid = String::from_utf8(
                git(
                    root,
                    &["rev-parse", "--verify", "--end-of-options", &object],
                )
                .await?,
            )?
            .trim()
            .to_owned();
            let size = String::from_utf8(git(root, &["cat-file", "-s", &oid]).await?)?
                .trim()
                .parse::<u64>()
                .map_err(std::io::Error::other)?;
            if size < crab_git::MAX_LFS_POINTER_SIZE as u64 {
                let bytes = git(root, &["cat-file", "blob", &oid]).await?;
                if let Some(size) = pointer_size(&bytes) {
                    return Self::pointer(root, bytes.into(), size).await;
                }
            }
            return Ok(Self {
                size,
                source: Source::Git {
                    root: root.clone(),
                    oid,
                },
            });
        }
        let absolute = tokio::fs::canonicalize(root.join(path))
            .await
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    AgentError::PathNotFound { path: path.into() }
                } else {
                    error.into()
                }
            })?;
        if !absolute.starts_with(root) {
            return Err(AgentError::Other(
                "file resolves outside the repository".into(),
            ));
        }
        let mut file = tokio::fs::File::open(absolute).await?;
        let metadata = file.metadata().await?;
        if !metadata.is_file() {
            return Err(AgentError::Other("preview source is not a file".into()));
        }
        let size = metadata.len();
        if size < crab_git::MAX_LFS_POINTER_SIZE as u64 {
            let mut bytes = Vec::with_capacity(size as usize);
            (&mut file).take(size + 1).read_to_end(&mut bytes).await?;
            if bytes.len() != size as usize
                || file.metadata().await?.modified().ok() != metadata.modified().ok()
            {
                return Err(changed());
            }
            if let Some(size) = pointer_size(&bytes) {
                return Self::pointer(root, bytes.into(), size).await;
            }
        }
        Ok(Self {
            size,
            source: Source::Local {
                file: Mutex::new(file),
                modified: metadata.modified().ok(),
            },
        })
    }

    async fn pointer(root: &Path, bytes: Bytes, size: u64) -> Result<Self> {
        #[derive(serde::Deserialize)]
        struct Configuration {
            remote_url: Option<String>,
        }
        // Parsing runs outside the request's task-local scope. Capture both its
        // credentials and the native configuration owner before leaving it.
        let selected = crate::storage_auth::current();
        let connection = selected
            .as_ref()
            .map(|connection| connection.options())
            .transpose()?;
        let output = native_pointer(
            root,
            &bytes,
            connection.as_ref(),
            &["read-pointer", "--inspect"],
        )
        .await?;
        let configuration: Configuration = serde_json::from_slice(&output)?;
        if let (Some(selected), Some(remote)) = (&selected, &configuration.remote_url) {
            selected.repository_prefix(remote)?;
        }
        Ok(Self {
            size,
            source: Source::Pointer {
                root: root.to_owned(),
                bytes,
                connection,
                remote: configuration.remote_url,
            },
        })
    }

    pub(crate) async fn read_range(&self, offset: u64, length: u32) -> Result<Bytes> {
        if length > MAX_RANGE_BYTES {
            return Err(AgentError::Other("range length exceeds 8 MiB".into()));
        }
        let start = offset.min(self.size);
        let end = start.saturating_add(u64::from(length)).min(self.size);
        let length = (end - start) as usize;
        if length == 0 {
            return Ok(Bytes::new());
        }
        let bytes = match &self.source {
            Source::Local { file, modified } => {
                let mut file = file.lock().await;
                let before = file.metadata().await?;
                if before.len() != self.size || before.modified().ok() != *modified {
                    return Err(changed());
                }
                file.seek(std::io::SeekFrom::Start(start)).await?;
                let mut bytes = vec![0; length];
                file.read_exact(&mut bytes).await?;
                let after = file.metadata().await?;
                if after.len() != self.size || after.modified().ok() != *modified {
                    return Err(changed());
                }
                bytes.into()
            }
            Source::Git { root, oid } => {
                let mut child = shellout::git_command()
                    .args(["cat-file", "blob", oid])
                    .current_dir(root)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()?;
                let mut stdout = child
                    .stdout
                    .take()
                    .ok_or_else(|| AgentError::Other("Git stdout unavailable".into()))?;
                let read = async {
                    let skipped =
                        tokio::io::copy(&mut (&mut stdout).take(start), &mut tokio::io::sink())
                            .await?;
                    if skipped != start {
                        return Err(AgentError::Other("Git blob ended before range".into()));
                    }
                    let mut bytes = vec![0; length];
                    stdout.read_exact(&mut bytes).await?;
                    Ok::<Bytes, AgentError>(bytes.into())
                }
                .await;
                drop(stdout);
                // Stop decoding the remainder of a large Git blob, then reap it.
                let _ = child.start_kill();
                let _ = child.wait().await?;
                read?
            }
            Source::Pointer {
                root,
                bytes,
                connection,
                remote,
            } => {
                let offset = start.to_string();
                let length = length.to_string();
                let expected = serde_json::to_string(remote)?;
                native_pointer(
                    root,
                    bytes,
                    connection.as_ref(),
                    &[
                        "read-pointer",
                        "--offset",
                        &offset,
                        "--length",
                        &length,
                        "--expected-remote",
                        &expected,
                    ],
                )
                .await?
            }
            Source::Remote { snapshot, path } => {
                let options = ReadOptions::default().with_range(start..end)?;
                let mut stream = snapshot
                    .open_file(path.clone())
                    .with_options(options)
                    .await?;
                let mut bytes = Vec::with_capacity(length);
                let result = async {
                    while let Some(chunk) = stream.next().await? {
                        if chunk.len() > length - bytes.len() {
                            return Err(AgentError::Other(
                                "range stream exceeded requested length".into(),
                            ));
                        }
                        bytes.extend_from_slice(&chunk);
                    }
                    Ok(())
                }
                .await;
                let close = stream.close().await;
                result?;
                close?;
                bytes.into()
            }
        };
        if bytes.len() != length {
            return Err(AgentError::Other(
                "range source returned an incomplete range".into(),
            ));
        }
        Ok(bytes)
    }
}

async fn native_pointer(
    root: &Path,
    bytes: &[u8],
    connection: Option<&DirectStoreOptions>,
    args: &[&str],
) -> Result<Bytes> {
    let mut command = tokio::process::Command::new(shellout::resolve_crab_bin());
    if let Some(connection) = connection {
        connection.configure_command(command.as_std_mut())?;
    }
    let mut child = command
        .args(args)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| AgentError::Other("Crab stdin unavailable".into()))?;
    let write = async move {
        stdin.write_all(bytes).await?;
        drop(stdin);
        Ok::<_, std::io::Error>(())
    };
    let (_, output) = tokio::try_join!(write, child.wait_with_output())?;
    if !output.status.success() {
        return Err(AgentError::Cli {
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(output.stdout.into())
}

fn pointer_size(bytes: &[u8]) -> Option<u64> {
    match crab_git::classify(bytes) {
        crab_git::PointerKind::Crab(pointer) => Some(pointer.size),
        crab_git::PointerKind::Lfs(pointer) => Some(pointer.size),
        crab_git::PointerKind::NotAPointer => None,
    }
}

fn changed() -> AgentError {
    AgentError::Other("file changed while reading; reopen the preview".into())
}

#[cfg(test)]
mod tests;
