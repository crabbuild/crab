use std::{process::Stdio, time::SystemTime};

use crab_sdk::operation::{Cancellation, Options, ReadOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::Mutex;

use super::*;

pub(crate) const MAX_RANGE_BYTES: u32 = 8 * 1024 * 1024;

pub(crate) struct FileSource {
    source: Source,
    scope: ReadScope,
    pub size: u64,
}

#[derive(Clone)]
pub(crate) struct ReadScope {
    sdk: Cancellation,
    stop: tokio_util::sync::CancellationToken,
    id: String,
}

impl ReadScope {
    pub(super) fn new(stop: tokio_util::sync::CancellationToken) -> Self {
        Self {
            sdk: Cancellation::default(),
            stop,
            id: uuid::Uuid::new_v4().to_string(),
        }
    }

    pub(crate) fn cancel(&self) {
        self.sdk.cancel();
        self.stop.cancel();
    }

    pub(crate) fn check(&self) -> Result<()> {
        if self.stop.is_cancelled() {
            return Err(AgentError::OpCancelled {
                op_id: self.id.clone(),
            });
        }
        Ok(())
    }

    fn options(&self) -> Options {
        Options::default().with_cancellation(self.sdk.clone())
    }
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
    Pointer(super::native::NativeFile),
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
        scope: ReadScope,
    ) -> Result<Self> {
        scope.check()?;
        let validated = GitPath::new(path.as_bytes().to_vec())?;
        let Some(root) = &repo.work_dir else {
            let snapshot = repo.snapshot(rev).await?;
            let metadata = snapshot
                .file_metadata(validated.clone())
                .with_options(ReadOptions::default().with_operation(scope.options()))
                .await?;
            return Ok(Self {
                scope,
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
                    return Self::pointer(repo, root, bytes.into(), size, scope).await;
                }
            }
            return Ok(Self {
                scope,
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
                return Self::pointer(repo, root, bytes.into(), size, scope).await;
            }
        }
        Ok(Self {
            scope,
            size,
            source: Source::Local {
                file: Mutex::new(file),
                modified: metadata.modified().ok(),
            },
        })
    }

    async fn pointer(
        repo: &BrowseSession,
        root: &Path,
        bytes: Bytes,
        size: u64,
        scope: ReadScope,
    ) -> Result<Self> {
        let source = repo
            .readers
            .open(root, bytes, scope.stop.clone(), scope.id.clone())
            .await?;
        if source.size != size {
            let closed = source.close().await;
            closed?;
            return Err(AgentError::Other(
                "native reader opened a different file size".into(),
            ));
        }
        Ok(Self {
            scope,
            size,
            source: Source::Pointer(source),
        })
    }

    pub(crate) async fn close(&self) -> Result<()> {
        self.scope.cancel();
        if let Source::Pointer(source) = &self.source {
            source.close().await?;
        }
        Ok(())
    }

    pub(crate) fn check_cancelled(&self) -> Result<()> {
        self.scope.check()
    }

    pub(crate) async fn read_range(&self, offset: u64, length: u32) -> Result<Bytes> {
        self.scope.check()?;
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
                    let mut remaining = start;
                    let mut discard = vec![0; (1024 * 1024_u64).min(start) as usize];
                    while remaining > 0 {
                        self.scope.check()?;
                        let take = remaining.min(discard.len() as u64) as usize;
                        stdout.read_exact(&mut discard[..take]).await?;
                        remaining -= take as u64;
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
            Source::Pointer(source) => source.read(start, length as u32).await?,
            Source::Remote { snapshot, path } => {
                let options = ReadOptions::default()
                    .with_operation(self.scope.options())
                    .with_range(start..end)?;
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
        self.scope.check()?;
        if bytes.len() != length {
            return Err(AgentError::Other(
                "range source returned an incomplete range".into(),
            ));
        }
        Ok(bytes)
    }
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
