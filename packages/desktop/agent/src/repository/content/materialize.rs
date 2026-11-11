use std::io::Write;
use std::time::{Duration, Instant};

use super::publication::{IndexFile, WorkingFile, publish, refresh_stat_cache, unchanged_file};
use super::workspace::Workspace;
use super::*;

pub(super) struct MaterializationTarget {
    pub(super) working: WorkingFile,
    pub(super) bytes: Bytes,
    pub(super) index: Option<IndexFile>,
}

impl MaterializationTarget {
    pub(super) fn capture(
        root: PathBuf,
        requested: PathBuf,
        path: PathBuf,
        bytes: Bytes,
        metadata: std::fs::Metadata,
    ) -> Self {
        Self {
            working: WorkingFile::from_bytes(root, requested, path, &bytes, metadata),
            bytes,
            index: None,
        }
    }
}

pub(crate) struct Dehydration {
    index: IndexFile,
    pointer: FileSource,
    bytes: Bytes,
}

impl Dehydration {
    pub(crate) fn version(&self) -> Result<FileVersion> {
        self.pointer.version()
    }

    pub(crate) async fn capture(
        repo: &BrowseSession,
        path: &str,
        scope: ReadScope,
    ) -> Result<Self> {
        let root = repo
            .work_dir
            .as_deref()
            .ok_or_else(|| AgentError::Other("Dehydration requires a working tree".into()))?;
        let index = IndexFile::capture(root, path, &scope).await?;
        index.check_commit(&scope).await?;
        let no_pointer =
            || AgentError::Other("The committed file has no Crab or Git LFS pointer".into());
        let len = command::blob_size(root, &index.oid, &scope).await?;
        if len >= crab_git::MAX_LFS_POINTER_SIZE as u64 {
            return Err(no_pointer());
        }
        let bytes = command::blob(root, &index.oid, 0, len as usize, len, &scope).await?;
        let (size, identity) = pointer_metadata(&bytes).ok_or_else(no_pointer)?;
        let reader = repo
            .readers
            .capture(root, bytes.clone(), size, scope.id.clone())?
            .for_dehydration(path.into());
        let pointer = FileSource {
            source: Arc::new(Source::Pointer {
                reader,
                identity,
                target: None,
            }),
            scope,
            size,
        };
        Ok(Self {
            index,
            pointer,
            bytes,
        })
    }

    pub(crate) fn size(&self) -> u64 {
        self.pointer.size
    }

    pub(crate) async fn release_reader(&self) -> Result<()> {
        self.pointer.release_reader().await
    }

    pub(crate) async fn run(
        self: Arc<Self>,
        repo: &BrowseSession,
        mut progress: impl FnMut(u64) -> Result<()> + Send + 'static,
    ) -> Result<u64> {
        let scope = self.pointer.scope();
        let requested = self.index.root.join(&self.index.path);
        let before = tokio::fs::symlink_metadata(&requested).await?;
        if !before.is_file() || before.len() != self.size() {
            return Err(AgentError::Other(
                "Working file differs from committed content".into(),
            ));
        }
        let working = FileSource::working_file(
            repo,
            &self.index.root,
            Path::new(&self.index.path),
            scope.clone(),
        )
        .await?;
        if working.size != self.size() {
            return Err(changed());
        }
        let destination = tokio::fs::canonicalize(&self.index.root)
            .await?
            .join(&self.index.path);
        let parent = destination.parent().ok_or_else(changed)?.to_owned();
        let (workspace, mut temporary) =
            Workspace::create(&self.index.root, parent, &scope).await?;
        let runtime = tokio::runtime::Handle::current();
        let result = tokio::task::spawn_blocking(move || {
            let Source::Local { file, path, .. } = working.source.as_ref() else {
                return Err(AgentError::Other(
                    "Select a hydrated working file to dehydrate".into(),
                ));
            };
            if *path != destination {
                return Err(AgentError::Other(
                    "Only regular working paths without symlink components can be dehydrated"
                        .into(),
                ));
            }
            let Source::Pointer { identity, .. } = self.pointer.source.as_ref() else {
                return Err(AgentError::Other(
                    "Dehydration requires a committed pointer".into(),
                ));
            };
            progress(0)?;
            verify_content(&runtime, &working, *identity, &mut progress).map_err(|error| {
                match error {
                    AgentError::Integrity { .. } => AgentError::Preview {
                        context: "Working file differs from committed content".into(),
                        source: Box::new(error),
                    },
                    error => error,
                }
            })?;
            // Keep the reconstruction reader and its staging pin through
            // publication. The only usable copy must not be the working file.
            runtime.block_on(self.pointer.prepare_content())?;
            verify_content(&runtime, &self.pointer, *identity, |bytes| {
                progress(self.size().saturating_add(bytes))
            })?;
            scope.check()?;

            let prepared = (|| {
                temporary.write_all(&self.bytes)?;
                temporary.flush()?;
                temporary.as_file().sync_data()?;
                runtime.block_on(self.index.check(&scope))?;
                runtime.block_on(self.index.check_commit(&scope))?;
                let current = std::fs::symlink_metadata(&requested)?;
                let opened = runtime.block_on(async { file.lock().await.metadata().await })?;
                if !unchanged_file(&before, &opened)
                    || !unchanged_file(&opened, &current)
                    || std::fs::canonicalize(&requested)? != *path
                {
                    return Err(changed());
                }
                temporary.as_file().set_permissions(current.permissions())?;
                scope.check()?;
                let update = crab_git::index_stat::IndexStatUpdate::from_file(
                    PathBuf::from(&self.index.path),
                    temporary.as_file(),
                    &self.bytes,
                )
                .map_err(std::io::Error::other)?;
                Ok((self.bytes.len() as u64, update))
            })();
            let (written, update) = publish(temporary, path, true, prepared)?;
            refresh_stat_cache(&self.index.root, update);
            Ok(written)
        })
        .await
        .map_err(|error| AgentError::Io(std::io::Error::other(error)))
        .and_then(|result| result);
        workspace.finish(result).await
    }
}

fn verify_content(
    runtime: &tokio::runtime::Handle,
    source: &FileSource,
    identity: Identity,
    mut progress: impl FnMut(u64) -> Result<()>,
) -> Result<()> {
    let mut digest = identity.hasher();
    let mut offset = 0;
    let mut reported = Instant::now();
    while offset < source.size {
        let length = (source.size - offset).min(1024 * 1024) as u32;
        let bytes = runtime.block_on(source.range(offset, length))?;
        digest.update(&bytes);
        offset += bytes.len() as u64;
        if reported.elapsed() >= Duration::from_millis(250) {
            progress(offset)?;
            reported = Instant::now();
        }
    }
    identity.verify_digest(digest.finish())?;
    progress(offset)
}

impl FileSource {
    pub(crate) async fn capture_materialization(
        repo: &BrowseSession,
        path: &str,
        scope: ReadScope,
    ) -> Result<Self> {
        GitPath::new(path.as_bytes().to_vec())?;
        let root = repo
            .work_dir
            .as_deref()
            .ok_or_else(|| AgentError::Other("Hydration requires a working tree".into()))?;
        let requested = root.join(path);
        match tokio::fs::symlink_metadata(&requested).await {
            Ok(metadata) if metadata.is_file() => {
                let source = Self::working_file(repo, root, Path::new(path), scope.clone()).await?;
                source.materialization(scope)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Restore only a resolved index pointer and only if the path
                // is still absent at publication. A new file always wins.
                let (mode, oid) = IndexFile::entry(root, path, &scope).await?;
                let Some(mode @ ("100644" | "100755")) = mode.as_deref() else {
                    return Err(AgentError::Other(
                        "Missing file has no regular index entry".into(),
                    ));
                };
                let oid = oid.ok_or_else(changed)?;
                let len = command::blob_size(root, &oid, &scope).await?;
                if len >= crab_git::MAX_LFS_POINTER_SIZE as u64 {
                    return Err(changed());
                }
                let bytes = command::blob(root, &oid, 0, len as usize, len, &scope).await?;
                let mut source = Self::git_blob(repo, root, oid.clone(), scope).await?;
                let Some(Source::Pointer { target, .. }) = Arc::get_mut(&mut source.source) else {
                    return Err(AgentError::Other(
                        "Missing file has no index pointer".into(),
                    ));
                };
                source.scope.check()?;
                *target = Some(MaterializationTarget {
                    working: WorkingFile::missing(root, path)?,
                    bytes,
                    index: Some(IndexFile {
                        root: root.into(),
                        path: path.into(),
                        mode: mode.into(),
                        oid,
                    }),
                });
                Ok(source)
            }
            Err(error) => Err(error.into()),
            Ok(_) => Err(AgentError::Other(
                "Only regular files can be hydrated".into(),
            )),
        }
    }

    pub(crate) async fn materialize(
        self: Arc<Self>,
        mut progress: impl FnMut(u64) -> Result<()> + Send + 'static,
    ) -> Result<u64> {
        let runtime = tokio::runtime::Handle::current();
        let scope = self.scope();
        let Source::Pointer {
            target: Some(target),
            ..
        } = self.source.as_ref()
        else {
            return Err(AgentError::Other(
                "only a captured working pointer can be materialized".into(),
            ));
        };
        let (workspace, mut temporary) = Workspace::create(
            &target.working.root,
            target.working.temporary_parent()?,
            &scope,
        )
        .await?;
        // The blocking owner retains the tempfile, hasher and range source.
        // Cancellation joins it before acknowledging cleanup or releasing pins.
        let result = tokio::task::spawn_blocking(move || {
            let Source::Pointer {
                identity,
                target: Some(target),
                ..
            } = self.source.as_ref()
            else {
                return Err(AgentError::Other(
                    "only a captured working pointer can be materialized".into(),
                ));
            };
            let working = &target.working;
            self.scope.check()?;
            working.check_metadata()?;
            let prepared = (|| {
                let mut hasher = identity.hasher();
                let mut written = 0;
                let mut reported = Instant::now();
                progress(0)?;
                while written < self.size {
                    let length = (self.size - written).min(1024 * 1024) as u32;
                    let bytes = runtime.block_on(self.read_range(written, length))?;
                    temporary.write_all(&bytes)?;
                    hasher.update(&bytes);
                    written += bytes.len() as u64;
                    if reported.elapsed() >= Duration::from_millis(250) {
                        progress(written)?;
                        reported = Instant::now();
                    }
                }
                identity.verify_digest(hasher.finish())?;
                temporary.flush()?;
                temporary.as_file().sync_data()?;
                progress(written)?;
                let permissions = working.check(&self.scope)?;
                if let Some(index) = &target.index {
                    index.apply_mode(temporary.as_file(), permissions)?;
                } else if let Some(permissions) = permissions {
                    temporary.as_file().set_permissions(permissions)?;
                }
                let relative = working
                    .requested
                    .strip_prefix(&working.root)
                    .map_err(std::io::Error::other)?;
                let update = crab_git::index_stat::IndexStatUpdate::from_file(
                    relative.into(),
                    temporary.as_file(),
                    &target.bytes,
                )
                .map_err(std::io::Error::other)?;
                // Index identity is checked after the potentially large raw
                // working-file hash, immediately before publication admission.
                if let Some(index) = &target.index {
                    runtime.block_on(index.check(&self.scope))?;
                }
                self.scope.check()?;
                Ok::<_, AgentError>((written, update))
            })();
            let (written, update) = working.publish(temporary, &self.scope, prepared)?;
            refresh_stat_cache(&working.root, update);
            Ok(written)
        })
        .await
        .map_err(|error| AgentError::Io(std::io::Error::other(error)))
        .and_then(|result| result);
        workspace
            .finish(result)
            .await
            .map_err(|error| scope.record(error))
    }
}

#[cfg(test)]
mod tests;
