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
                let entries =
                    command::run(root, &["ls-files", "--stage", "-z", "--", path], &scope).await?;
                let (mode, oid) = command::file_entry_at(&entries, path, true)?;
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
                    let entries = runtime.block_on(command::run(
                        &index.root,
                        &["ls-files", "--stage", "-z", "--", &index.path],
                        &self.scope,
                    ))?;
                    let (mode, oid) = command::file_entry_at(&entries, &index.path, true)?;
                    if mode.as_deref() != Some(index.mode.as_str())
                        || oid.as_deref() != Some(index.oid.as_str())
                    {
                        return Err(changed());
                    }
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
            let written = match prepared {
                Ok(written) => written,
                Err(error) => {
                    let error = self.scope.record(error);
                    if let Err(cleanup) = temporary.close() {
                        tracing::error!(%cleanup, "materialization temporary cleanup failed");
                        return Err(cleanup.into());
                    }
                    return Err(error);
                }
            };
            // Keep cleanup ownership until the same-filesystem atomic rename.
            // A cancellation after this commit point must report success.
            let published = if target.original.is_some() {
                temporary.persist(&target.path)
            } else {
                temporary.persist_noclobber(&target.path)
            };
            if let Err(error) = published {
                let failure = self.scope.record(error.error.into());
                if let Err(cleanup) = error.file.close() {
                    tracing::error!(%cleanup, "materialization temporary cleanup failed");
                    return Err(cleanup.into());
                }
                return Err(failure);
            }
            Ok(written)
        })
        .await
        .map_err(std::io::Error::other)?;
        result.map_err(|error| scope.record(error))
    }
}
