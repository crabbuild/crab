use std::{process::Stdio, time::SystemTime};

use crab_sdk::operation::{Cancellation, Options, ReadOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::Mutex;

use super::*;

pub(crate) const MAX_RANGE_BYTES: u32 = 8 * 1024 * 1024;
pub(crate) const MAX_COMPLETE_BYTES: usize = 200 * 1024 * 1024;

pub(crate) struct FileSource {
    source: Arc<Source>,
    scope: ReadScope,
    pub size: u64,
}

#[derive(Clone)]
pub(crate) struct ReadScope {
    sdk: Cancellation,
    stop: tokio_util::sync::CancellationToken,
    id: String,
    failure: Arc<std::sync::OnceLock<Arc<AgentError>>>,
    cancelled_read: Arc<std::sync::atomic::AtomicBool>,
}

impl ReadScope {
    pub(super) fn new(stop: tokio_util::sync::CancellationToken) -> Self {
        Self {
            sdk: Cancellation::default(),
            stop,
            id: uuid::Uuid::new_v4().to_string(),
            failure: Arc::new(std::sync::OnceLock::new()),
            cancelled_read: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    pub(crate) fn cancel(&self) {
        self.sdk.cancel();
        self.stop.cancel();
    }

    pub(crate) fn check(&self) -> Result<()> {
        if self.stop.is_cancelled() {
            return Err(self.record(AgentError::OpCancelled {
                op_id: self.id.clone(),
            }));
        }
        Ok(())
    }

    async fn run<T>(&self, work: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        tokio::pin!(work);
        tokio::select! {
            biased;
            result = &mut work => result,
            () = self.stop.cancelled() => {
                self.sdk.cancel();
                work.await
            }
        }
    }

    fn options(&self) -> Options {
        Options::default().with_cancellation(self.sdk.clone())
    }

    fn record(&self, error: AgentError) -> AgentError {
        if error.is_cancelled() {
            self.cancelled_read
                .store(true, std::sync::atomic::Ordering::Release);
            return error;
        }
        let error = match error {
            AgentError::Shared(error) => error,
            error => Arc::new(error),
        };
        let _ = self.failure.set(error.clone());
        AgentError::Shared(error)
    }

    pub(crate) fn failure(&self) -> Option<AgentError> {
        self.failure.get().cloned().map(AgentError::Shared)
    }

    pub(crate) fn cancelled_read(&self) -> bool {
        self.cancelled_read
            .load(std::sync::atomic::Ordering::Acquire)
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
    Pointer {
        reader: super::native::NativeFile,
        identity: Identity,
    },
    Remote {
        snapshot: Snapshot,
        path: GitPath,
        kind: ContentKind,
        oid: String,
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
            let snapshot = scope.run(repo.snapshot(rev, scope.options())).await?;
            let metadata = scope
                .run(async {
                    Ok(snapshot
                        .file_metadata(validated.clone())
                        .with_options(ReadOptions::default().with_operation(scope.options()))
                        .await?)
                })
                .await?;
            return Ok(Self {
                scope,
                size: metadata.size,
                source: Arc::new(Source::Remote {
                    snapshot,
                    path: validated,
                    kind: metadata.kind,
                    oid: metadata.id.to_string(),
                }),
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
                if let Some((size, identity)) = pointer_metadata(&bytes) {
                    return Self::pointer(repo, root, bytes.into(), size, identity, scope).await;
                }
            }
            return Ok(Self {
                scope,
                size,
                source: Arc::new(Source::Git {
                    root: root.clone(),
                    oid,
                }),
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
        // Both sides need the native canonical form, including Windows verbatim
        // paths and repositories opened through a filesystem symlink.
        let boundary = tokio::fs::canonicalize(root).await?;
        if !absolute.starts_with(boundary) {
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
            if let Some((size, identity)) = pointer_metadata(&bytes) {
                return Self::pointer(repo, root, bytes.into(), size, identity, scope).await;
            }
        }
        Ok(Self {
            scope,
            size,
            source: Arc::new(Source::Local {
                file: Mutex::new(file),
                modified: metadata.modified().ok(),
            }),
        })
    }

    async fn pointer(
        repo: &BrowseSession,
        root: &Path,
        bytes: Bytes,
        size: u64,
        identity: Identity,
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
            source: Arc::new(Source::Pointer {
                reader: source,
                identity,
            }),
        })
    }

    pub(crate) fn describe(&self) -> Value {
        let (kind, representation, hash, oid) = match self.source.as_ref() {
            Source::Local { .. } => ("git", "working_file", None, None),
            Source::Git { oid, .. } => ("git", "git_blob", None, Some(oid.as_str())),
            Source::Pointer { identity, .. } => {
                let (kind, hash) = match identity {
                    Identity::Blake3(hash) => ("crab", hash),
                    Identity::Sha256(hash) => ("lfs", hash),
                };
                (
                    kind,
                    "pointer",
                    Some(crab_git::lfs_pointer::hex_encode(hash)),
                    None,
                )
            }
            Source::Remote { kind, oid, .. } => {
                let kind = match kind {
                    ContentKind::Crab => "crab",
                    ContentKind::Lfs => "lfs",
                    _ => "git",
                };
                (kind, "remote_content", None, Some(oid.as_str()))
            }
        };
        json!({ "kind": kind, "representation": representation, "size": self.size,
            "content_hash": hash, "git_oid": oid })
    }

    pub(super) fn query(&self) -> Self {
        Self {
            source: self.source.clone(),
            scope: ReadScope::new(self.scope.stop.child_token()),
            size: self.size,
        }
    }

    pub(crate) fn scope(&self) -> ReadScope {
        self.scope.clone()
    }

    pub(super) fn cancel(&self) {
        self.scope.cancel();
    }

    pub(crate) async fn close(&self) -> Result<()> {
        self.scope.cancel();
        if let Source::Pointer { reader, .. } = self.source.as_ref() {
            reader.close().await?;
        }
        Ok(())
    }

    pub(crate) fn check_cancelled(&self) -> Result<()> {
        self.scope.check()
    }

    pub(crate) async fn chunk_page(
        &self,
        start: u64,
        limit: u32,
    ) -> Result<crab_types::file_layout::ChunkPage> {
        self.scope.check()?;
        let page = match self.source.as_ref() {
            Source::Pointer {
                reader,
                identity: Identity::Blake3(_),
            } => reader.chunks(start, limit, &self.scope.stop).await?,
            Source::Remote { snapshot, path, .. } => {
                self.scope
                    .run(async {
                        Ok(snapshot
                            .file_chunks(path.clone(), start, limit)
                            .with_options(
                                ReadOptions::default().with_operation(self.scope.options()),
                            )
                            .await?)
                    })
                    .await?
            }
            _ => return Err(AgentError::Other("file has no Crab chunk recipe".into())),
        };
        self.scope.check()?;
        Ok(page)
    }

    pub(crate) async fn read_range(&self, offset: u64, length: u32) -> Result<Bytes> {
        // Some format dependencies stringify I/O sources. Keep the first
        // captured read failure so they cannot turn cancellation or corrupt
        // content into an unrelated format error or a successful preview.
        self.range(offset, length)
            .await
            .map_err(|error| self.scope.record(error))
    }

    async fn range(&self, offset: u64, length: u32) -> Result<Bytes> {
        self.scope.check()?;
        if length > MAX_RANGE_BYTES {
            return Err(AgentError::Other("range length exceeds 8 MiB".into()));
        }
        let start = offset.min(self.size);
        let end = start.saturating_add(u64::from(length)).min(self.size);
        let length = (end - start) as usize;
        if length == 0 {
            if let Source::Local { file, modified } = self.source.as_ref() {
                let metadata = file.lock().await.metadata().await?;
                if metadata.len() != self.size || metadata.modified().ok() != *modified {
                    return Err(changed());
                }
            }
            return Ok(Bytes::new());
        }
        let bytes = match self.source.as_ref() {
            Source::Local { file, modified } => {
                let mut file = file.lock().await;
                let before = file.metadata().await?;
                if before.len() != self.size || before.modified().ok() != *modified {
                    return Err(changed());
                }
                file.seek(std::io::SeekFrom::Start(start)).await?;
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(length)
                    .map_err(std::io::Error::other)?;
                bytes.resize(length, 0);
                file.read_exact(&mut bytes).await?;
                let after = file.metadata().await?;
                if after.len() != self.size || after.modified().ok() != *modified {
                    return Err(changed());
                }
                bytes.into()
            }
            Source::Git { root, oid } => self.read_git(root, oid, start, length).await?,
            Source::Pointer { reader, .. } => {
                reader.read(start, length as u32, &self.scope.stop).await?
            }
            Source::Remote { snapshot, path, .. } => {
                let options = ReadOptions::default()
                    .with_operation(self.scope.options())
                    .with_range(start..end)?;
                self.scope
                    .run(remote_bytes(snapshot, path, options, length))
                    .await?
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
    pub(crate) async fn read_complete(&self, maximum: usize) -> Result<Bytes> {
        self.scope.check()?;
        let maximum = maximum.min(MAX_COMPLETE_BYTES);
        if self.size > maximum as u64 {
            return Err(AgentError::Other(format!(
                "complete read exceeds {maximum} bytes"
            )));
        }
        let length = self.size as usize;
        match self.source.as_ref() {
            Source::Remote { snapshot, path, .. } => {
                // Let the SDK observe verified whole-file EOF instead of
                // assembling this response from independently requested ranges.
                let options = ReadOptions::default().with_operation(self.scope.options());
                return self
                    .scope
                    .run(remote_bytes(snapshot, path, options, length))
                    .await;
            }
            Source::Git { root, oid } => return self.read_git(root, oid, 0, length).await,
            _ => {}
        }
        let mut output = Vec::new();
        output
            .try_reserve_exact(length)
            .map_err(std::io::Error::other)?;
        if length == 0 {
            self.read_range(0, 0).await?;
        }
        while output.len() < length {
            let count = (length - output.len()).min(MAX_RANGE_BYTES as usize) as u32;
            output.extend_from_slice(&self.read_range(output.len() as u64, count).await?);
        }
        if let Source::Pointer { identity, .. } = self.source.as_ref() {
            let identity = *identity;
            // Native ranges authenticate their selected chunks/blocks. Binding
            // the assembled response to its whole-file identity is a separate
            // check, including empty pointers and ranges stitched across calls.
            let output = tokio::task::spawn_blocking(move || {
                identity.verify(&output)?;
                Ok::<_, AgentError>(Bytes::from(output))
            })
            .await
            .map_err(std::io::Error::other)??;
            self.scope.check()?;
            return Ok(output);
        }
        self.scope.check()?;
        Ok(output.into())
    }

    async fn read_git(&self, root: &Path, oid: &str, start: u64, length: usize) -> Result<Bytes> {
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
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(length)
                .map_err(std::io::Error::other)?;
            bytes.resize(length, 0);
            for chunk in bytes.chunks_mut(1024 * 1024) {
                self.scope.check()?;
                stdout.read_exact(chunk).await?;
            }
            if start + length as u64 == self.size {
                let mut extra = [0];
                if stdout.read(&mut extra).await? != 0 {
                    return Err(AgentError::Other("Git blob exceeds captured size".into()));
                }
            }
            self.scope.check()?;
            Ok::<Bytes, AgentError>(bytes.into())
        }
        .await;
        drop(stdout);
        let complete = read.is_ok() && start + length as u64 == self.size;
        let killed = if !complete {
            child.start_kill()
        } else {
            Ok(())
        };
        let waited = child.wait_with_output().await;
        let bytes = read?;
        killed?;
        let output = waited?;
        if complete && !output.status.success() {
            return Err(AgentError::Cli {
                status: output.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(bytes)
    }
}

async fn remote_bytes(
    snapshot: &Snapshot,
    path: &GitPath,
    options: ReadOptions,
    length: usize,
) -> Result<Bytes> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(std::io::Error::other)?;
    let mut stream = snapshot
        .open_file(path.clone())
        .with_options(options)
        .await?;
    let result = async {
        while let Some(chunk) = stream.next().await? {
            if chunk.len() > length - bytes.len() {
                return Err(AgentError::Other(
                    "content stream exceeded captured length".into(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.len() != length {
            return Err(AgentError::Other(
                "content stream ended before captured length".into(),
            ));
        }
        Ok(())
    }
    .await;
    let closed = stream.close().await;
    closed?;
    result?;
    Ok(bytes.into())
}

#[derive(Clone, Copy)]
enum Identity {
    Blake3([u8; 32]),
    Sha256([u8; 32]),
}

impl Identity {
    fn verify(self, bytes: &[u8]) -> Result<()> {
        use sha2::Digest as _;
        let (algorithm, expected, actual) = match self {
            Self::Blake3(expected) => ("BLAKE3", expected, *blake3::hash(bytes).as_bytes()),
            Self::Sha256(expected) => ("SHA-256", expected, sha2::Sha256::digest(bytes).into()),
        };
        if expected != actual {
            return Err(AgentError::Integrity {
                algorithm,
                expected: crab_git::lfs_pointer::hex_encode(&expected),
                actual: crab_git::lfs_pointer::hex_encode(&actual),
            });
        }
        Ok(())
    }
}

fn pointer_metadata(bytes: &[u8]) -> Option<(u64, Identity)> {
    match crab_git::classify(bytes) {
        crab_git::PointerKind::Crab(pointer) => {
            Some((pointer.size, Identity::Blake3(pointer.file_hash)))
        }
        crab_git::PointerKind::Lfs(pointer) => Some((pointer.size, Identity::Sha256(pointer.oid))),
        crab_git::PointerKind::NotAPointer => None,
    }
}

fn changed() -> AgentError {
    AgentError::Other("file changed while reading; reopen the preview".into())
}

#[cfg(test)]
mod tests;
