use std::io::{Read, Write};
use std::time::{Duration, Instant};

use super::*;

pub(super) struct WorkingPointer {
    requested: PathBuf,
    path: PathBuf,
    original: Option<(Bytes, Option<SystemTime>)>,
    index: Option<IndexPointer>,
}

struct IndexPointer {
    root: PathBuf,
    path: String,
    mode: String,
    oid: String,
}

impl IndexPointer {
    async fn entry(
        root: &Path,
        path: &str,
        scope: &ReadScope,
    ) -> Result<(Option<String>, Option<String>)> {
        let entries = command::run(root, &["ls-files", "--stage", "-z", "--", path], scope).await?;
        command::file_entry_at(&entries, path, true)
    }

    async fn check(&self, scope: &ReadScope) -> Result<()> {
        let entry = Self::entry(&self.root, &self.path, scope).await?;
        if !self.matches(&entry) {
            return Err(changed());
        }
        Ok(())
    }

    fn matches(&self, entry: &(Option<String>, Option<String>)) -> bool {
        entry.0.as_deref() == Some(self.mode.as_str())
            && entry.1.as_deref() == Some(self.oid.as_str())
    }

    async fn check_commit(&self, scope: &ReadScope) -> Result<()> {
        let entries = command::run(
            &self.root,
            &["ls-tree", "-z", "HEAD", "--", &self.path],
            scope,
        )
        .await?;
        if !self.matches(&command::file_entry_at(&entries, &self.path, false)?) {
            return Err(AgentError::Other(
                "Commit the selected pointer and mode before dehydrating".into(),
            ));
        }
        Ok(())
    }
}

pub(crate) struct Dehydration {
    index: IndexPointer,
    pointer: FileSource,
    bytes: Bytes,
}

impl Dehydration {
    pub(crate) async fn capture(
        repo: &BrowseSession,
        path: &str,
        scope: ReadScope,
    ) -> Result<Self> {
        GitPath::new(path.as_bytes().to_vec())?;
        let root = repo
            .work_dir
            .as_deref()
            .ok_or_else(|| AgentError::Other("Dehydration requires a working tree".into()))?;
        let (mode, oid) = IndexPointer::entry(root, path, &scope).await?;
        let (Some(mode), Some(oid)) = (
            mode.filter(|mode| matches!(mode.as_str(), "100644" | "100755")),
            oid,
        ) else {
            return Err(AgentError::Other(
                "The selected file has no resolved regular index entry".into(),
            ));
        };
        let index = IndexPointer {
            root: root.into(),
            path: path.into(),
            mode,
            oid,
        };
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
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
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

            let parent = path.parent().ok_or_else(changed)?;
            let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
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
                Ok(self.bytes.len() as u64)
            })();
            publish(temporary, path, true, prepared)
        })
        .await
        .map_err(std::io::Error::other)?
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

fn unchanged_file(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    if !after.is_file()
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
        || before.created().ok() != after.created().ok()
    {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
    }
    #[cfg(not(unix))]
    true
}

impl WorkingPointer {
    pub(super) fn capture(
        requested: PathBuf,
        path: PathBuf,
        bytes: Bytes,
        metadata: std::fs::Metadata,
    ) -> Self {
        Self {
            requested,
            path,
            original: Some((bytes, metadata.modified().ok())),
            index: None,
        }
    }

    fn check(&self) -> Result<Option<std::fs::Permissions>> {
        let Some((original, modified)) = &self.original else {
            let parent = self.requested.parent().ok_or_else(changed)?;
            if std::fs::canonicalize(parent)?.join(self.requested.file_name().ok_or_else(changed)?)
                != self.path
            {
                return Err(changed());
            }
            return match std::fs::symlink_metadata(&self.requested) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.into()),
                Ok(_) => Err(changed()),
            };
        };
        let metadata = std::fs::symlink_metadata(&self.requested)?;
        if !metadata.is_file()
            || metadata.modified().ok() != *modified
            || metadata.len() != original.len() as u64
            || std::fs::canonicalize(&self.requested)? != self.path
        {
            return Err(changed());
        }
        let mut bytes = Vec::with_capacity(original.len() + 1);
        std::fs::File::open(&self.requested)?
            .take(original.len() as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes != *original {
            return Err(changed());
        }
        Ok(Some(metadata.permissions()))
    }
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
                let (mode, oid) = IndexPointer::entry(root, path, &scope).await?;
                let Some(mode @ ("100644" | "100755")) = mode.as_deref() else {
                    return Err(AgentError::Other(
                        "Missing file has no regular index entry".into(),
                    ));
                };
                let oid = oid.ok_or_else(changed)?;
                let mut source = Self::git_blob(repo, root, oid.clone(), scope).await?;
                let Some(Source::Pointer { target, .. }) = Arc::get_mut(&mut source.source) else {
                    return Err(AgentError::Other(
                        "Missing file has no index pointer".into(),
                    ));
                };
                let parent =
                    tokio::fs::canonicalize(requested.parent().ok_or_else(changed)?).await?;
                if !parent.starts_with(tokio::fs::canonicalize(root).await?) {
                    return Err(AgentError::Other(
                        "File resolves outside the repository".into(),
                    ));
                }
                *target = Some(WorkingPointer {
                    path: parent.join(requested.file_name().ok_or_else(changed)?),
                    requested,
                    original: None,
                    index: Some(IndexPointer {
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
            self.scope.check()?;
            target.check()?;
            let parent = target
                .path
                .parent()
                .ok_or_else(|| AgentError::Other("file has no parent directory".into()))?;
            let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
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
                if let Some(index) = &target.index {
                    runtime.block_on(index.check(&self.scope))?;
                }
                self.scope.check()?;
                if let Some(permissions) = target.check()? {
                    temporary.as_file().set_permissions(permissions)?;
                } else if target
                    .index
                    .as_ref()
                    .is_some_and(|index| index.mode == "100755")
                {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let file = temporary.as_file();
                        let mut permissions = file.metadata()?.permissions();
                        permissions.set_mode(permissions.mode() | 0o100);
                        file.set_permissions(permissions)?;
                    }
                }
                Ok::<_, AgentError>(written)
            })();
            publish(temporary, &target.path, target.original.is_some(), prepared)
        })
        .await
        .map_err(std::io::Error::other)?;
        result.map_err(|error| scope.record(error))
    }
}

fn publish<T>(
    temporary: tempfile::NamedTempFile,
    path: &Path,
    replace: bool,
    prepared: Result<T>,
) -> Result<T> {
    // A successful same-filesystem publication wins a late cancellation.
    let (operation, temporary) = match prepared {
        Ok(value) => {
            let result = if replace {
                temporary.persist(path)
            } else {
                temporary.persist_noclobber(path)
            };
            match result {
                Ok(_) => return Ok(value),
                Err(error) => (AgentError::from(error.error), error.file),
            }
        }
        Err(error) => (error, temporary),
    };
    match temporary.close() {
        Ok(()) => Err(operation),
        Err(cleanup) => Err(AgentError::Cleanup {
            operation: Box::new(operation),
            cleanup: Box::new(cleanup.into()),
        }),
    }
}
