use std::time::SystemTime;

use crab_sdk::operation::{Cancellation, Options, ReadOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

use super::*;

pub(super) mod command;
pub(crate) mod diff;
mod materialize;
mod publication;
mod restore;
mod version;
mod workspace;
pub(crate) use restore::Restoration;
pub(crate) use version::FileVersion;
mod sqlite;
pub(crate) use materialize::Dehydration;
use materialize::MaterializationTarget;

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

impl Default for ReadScope {
    fn default() -> Self {
        Self::new(tokio_util::sync::CancellationToken::new())
    }
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

    pub(crate) fn cancellation_token(&self) -> &tokio_util::sync::CancellationToken {
        &self.stop
    }

    pub(crate) fn child(&self) -> Self {
        Self::new(self.stop.child_token())
    }

    pub(crate) async fn cancelled(&self) -> AgentError {
        self.stop.cancelled().await;
        self.record(AgentError::OpCancelled {
            op_id: self.id.clone(),
        })
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

    pub(crate) fn options(&self) -> Options {
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
        path: PathBuf,
        modified: Option<SystemTime>,
    },
    Git {
        root: PathBuf,
        oid: String,
    },
    Pointer {
        reader: super::native::CapturedRead,
        identity: Identity,
        target: Option<MaterializationTarget>,
    },
    Remote {
        snapshot: Snapshot,
        path: GitPath,
        kind: ContentKind,
        oid: String,
    },
}

impl FileSource {
    pub(crate) fn version(&self) -> Result<FileVersion> {
        match self.source.as_ref() {
            Source::Pointer { identity, .. } => Ok(FileVersion::pointer(*identity, self.size)),
            _ => Err(AgentError::Other(
                "Transfer source is not a captured pointer".into(),
            )),
        }
    }

    pub(crate) async fn capture(
        repo: &BrowseSession,
        path: &str,
        rev: Option<&str>,
        scope: ReadScope,
    ) -> Result<Self> {
        let source = Self::capture_metadata(repo, path, rev, scope).await?;
        source.prepare_content().await?;
        Ok(source)
    }

    async fn capture_metadata(
        repo: &BrowseSession,
        path: &str,
        rev: Option<&str>,
        scope: ReadScope,
    ) -> Result<Self> {
        scope.check()?;
        let validated = GitPath::new(path.as_bytes().to_vec())?;
        let Some(root) = &repo.work_dir else {
            let snapshot = scope.run(repo.snapshot(rev, scope.options())).await?;
            return Self::snapshot_file(snapshot, validated, scope).await;
        };
        if let Some(rev) = rev {
            // Resolve the index or revision once; a later checkout cannot redirect reads.
            let object = format!("{rev}:{path}");
            let oid = String::from_utf8(
                command::run(
                    root,
                    &["rev-parse", "--verify", "--end-of-options", &object],
                    &scope,
                )
                .await?,
            )?
            .trim()
            .to_owned();
            return Self::git_blob(repo, root, oid, scope).await;
        }
        Self::working_file(repo, root, Path::new(path), scope).await
    }

    async fn working_file(
        repo: &BrowseSession,
        root: &Path,
        path: &Path,
        scope: ReadScope,
    ) -> Result<Self> {
        scope.check()?;
        let absolute = tokio::fs::canonicalize(root.join(path))
            .await
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    AgentError::PathNotFound {
                        path: path.display().to_string(),
                    }
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
        let mut file = tokio::fs::File::open(&absolute).await?;
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
                let bytes = Bytes::from(bytes);
                let requested = root.join(path);
                let target = if tokio::fs::symlink_metadata(&requested).await?.is_file() {
                    Some(MaterializationTarget::capture(
                        root.into(),
                        requested,
                        absolute,
                        bytes.clone(),
                        metadata,
                    ))
                } else {
                    None
                };
                return Self::pointer(repo, root, bytes, size, identity, target, scope);
            }
        }
        Ok(Self {
            scope,
            size,
            source: Arc::new(Source::Local {
                file: Mutex::new(file),
                path: absolute,
                modified: metadata.modified().ok(),
            }),
        })
    }

    async fn snapshot_file(snapshot: Snapshot, path: GitPath, scope: ReadScope) -> Result<Self> {
        let metadata = scope
            .run(async {
                Ok(snapshot
                    .file_metadata(path.clone())
                    .with_options(ReadOptions::default().with_operation(scope.options()))
                    .await?)
            })
            .await?;
        Ok(Self {
            scope,
            size: metadata.size,
            source: Arc::new(Source::Remote {
                snapshot,
                path,
                kind: metadata.kind,
                oid: metadata.id.to_string(),
            }),
        })
    }

    // Blob size probes and later reads both bypass replace refs. A captured
    // physical object must not change when a user edits refs/replace.
    async fn git_blob(
        repo: &BrowseSession,
        root: &Path,
        oid: String,
        scope: ReadScope,
    ) -> Result<Self> {
        let size = command::blob_size(root, &oid, &scope).await?;
        if size < crab_git::MAX_LFS_POINTER_SIZE as u64 {
            let bytes = command::run(
                root,
                &["--no-replace-objects", "cat-file", "blob", &oid],
                &scope,
            )
            .await?;
            if let Some((size, identity)) = pointer_metadata(&bytes) {
                return Self::pointer(repo, root, bytes.into(), size, identity, None, scope);
            }
        }
        Ok(Self {
            scope,
            size,
            source: Arc::new(Source::Git {
                root: root.into(),
                oid,
            }),
        })
    }

    fn pointer(
        repo: &BrowseSession,
        root: &Path,
        bytes: Bytes,
        size: u64,
        identity: Identity,
        target: Option<MaterializationTarget>,
        scope: ReadScope,
    ) -> Result<Self> {
        let source = repo.readers.capture(root, bytes, size, scope.id.clone())?;
        Ok(Self {
            scope,
            size,
            source: Arc::new(Source::Pointer {
                reader: source,
                identity,
                target,
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
        let can_materialize = matches!(
            self.source.as_ref(),
            Source::Pointer {
                target: Some(_),
                ..
            }
        );
        json!({ "kind": kind, "representation": representation, "size": self.size, "can_materialize": can_materialize,
            "content_hash": hash, "git_oid": oid })
    }

    async fn prepare_content(&self) -> Result<()> {
        self.scope.check()?;
        if let Source::Pointer { reader, .. } = self.source.as_ref() {
            reader.open(&self.scope.stop).await?;
        }
        Ok(())
    }

    pub(super) fn query(&self) -> Self {
        Self {
            source: self.source.clone(),
            scope: self.scope.child(),
            size: self.size,
        }
    }

    pub(super) fn materialization(&self, scope: ReadScope) -> Result<Self> {
        if !matches!(
            self.source.as_ref(),
            Source::Pointer {
                target: Some(_),
                ..
            }
        ) {
            return Err(AgentError::Other(
                "only a captured working pointer can be materialized".into(),
            ));
        }
        Ok(Self {
            source: self.source.clone(),
            scope,
            size: self.size,
        })
    }

    pub(crate) fn scope(&self) -> ReadScope {
        self.scope.clone()
    }

    pub(super) fn cancel(&self) {
        self.scope.cancel();
    }

    pub(crate) async fn close(&self) -> Result<()> {
        self.scope.cancel();
        self.release_reader().await
    }

    pub(crate) async fn release_reader(&self) -> Result<()> {
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
                ..
            } => {
                reader
                    .open(&self.scope.stop)
                    .await?
                    .chunks(start, limit, &self.scope.stop)
                    .await?
            }
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

    fn content_options(&self) -> Result<ReadOptions> {
        let mut options = ReadOptions::default().with_operation(self.scope.options());
        if matches!(
            self.source.as_ref(),
            Source::Remote {
                kind: ContentKind::Lfs,
                ..
            }
        ) {
            // A cold LFS range verifies the complete object before delivery.
            // Keep the SDK's finite metadata/retry allowance plus this captured
            // payload; response, decoding, request and deadline limits stay bounded.
            let mut limits = options.read_limits();
            limits.max_fetched_bytes =
                limits
                    .max_fetched_bytes
                    .checked_add(self.size)
                    .ok_or_else(|| {
                        AgentError::Other(
                            "captured LFS size exceeds the finite fetch budget".into(),
                        )
                    })?;
            options = options.with_limits(limits)?;
        }
        Ok(options)
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
            if let Source::Local { file, modified, .. } = self.source.as_ref() {
                let metadata = file.lock().await.metadata().await?;
                if metadata.len() != self.size || metadata.modified().ok() != *modified {
                    return Err(changed());
                }
            }
            return Ok(Bytes::new());
        }
        let bytes = match self.source.as_ref() {
            Source::Local { file, modified, .. } => {
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
                reader
                    .open(&self.scope.stop)
                    .await?
                    .read(start, length as u32, &self.scope.stop)
                    .await?
            }
            Source::Remote { snapshot, path, .. } => {
                let options = self.content_options()?.with_range(start..end)?;
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
                let options = self.content_options()?;
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
        self.write_ranges(&mut output).await?;
        Ok(output.into())
    }

    pub(crate) async fn copy_complete_to_file(&self, file: std::fs::File) -> Result<()> {
        self.scope.check()?;
        if let Source::Git { root, oid } = self.source.as_ref() {
            return command::blob_to_file(root, oid, self.size, file, &self.scope).await;
        }
        let mut output = tokio::fs::File::from_std(file);
        let copied = match self.source.as_ref() {
            Source::Remote { snapshot, path, .. } => match self.content_options() {
                Ok(options) => {
                    self.scope
                        .run(remote_write(
                            snapshot,
                            path,
                            options,
                            self.size,
                            &mut output,
                        ))
                        .await
                }
                Err(error) => Err(error),
            },
            _ => self.write_ranges(&mut output).await,
        };
        // Tokio may still own the last buffered write when capture fails. Join
        // it before the query owner closes and removes the temporary snapshot.
        let flushed = output.flush().await;
        match (copied, flushed) {
            (Err(operation), Err(cleanup)) => Err(AgentError::Cleanup {
                operation: Box::new(operation),
                cleanup: Box::new(cleanup.into()),
            }),
            (Err(error), Ok(())) => Err(self.scope.record(error)),
            (Ok(()), Err(error)) => Err(error.into()),
            (Ok(()), Ok(())) => self.scope.check(),
        }
    }

    async fn write_ranges(&self, output: &mut (impl AsyncWrite + Unpin)) -> Result<()> {
        let mut digest = match self.source.as_ref() {
            Source::Pointer { identity, .. } => Some((*identity, identity.hasher())),
            _ => None,
        };
        let mut offset = 0;
        while offset < self.size {
            let count = (self.size - offset).min(u64::from(MAX_RANGE_BYTES)) as u32;
            let bytes = self.read_range(offset, count).await?;
            if let Some((_, hasher)) = &mut digest {
                hasher.update(&bytes);
            }
            output.write_all(&bytes).await?;
            offset += bytes.len() as u64;
        }
        // Ranges authenticate their individual blocks. Verify the stitched
        // stream against its complete pointer identity before opening it.
        if let Some((identity, hasher)) = digest {
            identity.verify_digest(hasher.finish())?;
        }
        self.read_range(self.size, 0).await?;
        self.scope.check()
    }

    async fn read_git(&self, root: &Path, oid: &str, start: u64, length: usize) -> Result<Bytes> {
        command::blob(root, oid, start, length, self.size, &self.scope).await
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
    remote_write(snapshot, path, options, length as u64, &mut bytes).await?;
    Ok(bytes.into())
}

async fn remote_write(
    snapshot: &Snapshot,
    path: &GitPath,
    options: ReadOptions,
    length: u64,
    output: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    let mut stream = snapshot
        .open_file(path.clone())
        .with_options(options)
        .await?;
    let result = async {
        let mut written = 0;
        while let Some(chunk) = stream.next().await? {
            if chunk.len() as u64 > length - written {
                return Err(AgentError::Other(
                    "content stream exceeded captured length".into(),
                ));
            }
            output.write_all(&chunk).await?;
            written += chunk.len() as u64;
        }
        if written != length {
            return Err(AgentError::Other(
                "content stream ended before captured length".into(),
            ));
        }
        Ok(())
    }
    .await;
    let closed = stream.close().await;
    closed?;
    result
}

#[derive(Clone, Copy)]
enum Identity {
    Blake3([u8; 32]),
    Sha256([u8; 32]),
}

impl Identity {
    fn hasher(self) -> ContentHasher {
        use sha2::Digest as _;
        match self {
            Self::Blake3(_) => ContentHasher::Blake3(Box::new(blake3::Hasher::new())),
            Self::Sha256(_) => ContentHasher::Sha256(sha2::Sha256::new()),
        }
    }

    fn verify_digest(self, actual: [u8; 32]) -> Result<()> {
        let (algorithm, expected) = match self {
            Self::Blake3(expected) => ("BLAKE3", expected),
            Self::Sha256(expected) => ("SHA-256", expected),
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

enum ContentHasher {
    Blake3(Box<blake3::Hasher>),
    Sha256(sha2::Sha256),
}

impl ContentHasher {
    fn update(&mut self, bytes: &[u8]) {
        use sha2::Digest as _;
        match self {
            Self::Blake3(hasher) => {
                hasher.update(bytes);
            }
            Self::Sha256(hasher) => hasher.update(bytes),
        }
    }

    fn finish(self) -> [u8; 32] {
        use sha2::Digest as _;
        match self {
            Self::Blake3(hasher) => *hasher.finalize().as_bytes(),
            Self::Sha256(hasher) => hasher.finalize().into(),
        }
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
