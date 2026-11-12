use super::materialize::MaterializationTarget;
use super::publication::{IndexFile, WorkingFile};
use super::workspace::Workspace;
use super::*;

pub(crate) struct Restoration {
    source: RestoreSource,
}

enum RestoreSource {
    Pointer(Arc<FileSource>),
    Git(Arc<GitRestoration>),
}

struct GitRestoration {
    index: IndexFile,
    working: WorkingFile,
    size: u64,
    scope: ReadScope,
}

impl Restoration {
    pub(crate) fn version(&self) -> Result<FileVersion> {
        match &self.source {
            RestoreSource::Pointer(source) => source.version(),
            RestoreSource::Git(source) => Ok(FileVersion::git(
                &source.index.oid,
                source.size,
                &source.index.mode,
            )),
        }
    }

    pub(crate) async fn capture(
        repo: &BrowseSession,
        path: &str,
        scope: ReadScope,
    ) -> Result<Self> {
        let root = repo
            .work_dir
            .as_deref()
            .ok_or_else(|| AgentError::Other("Restore requires a working tree".into()))?;
        let index = IndexFile::capture(root, path, &scope).await?;
        let working = WorkingFile::capture(root, path, &scope).await?;
        let size = command::blob_size(root, &index.oid, &scope).await?;
        if size < crab_git::MAX_LFS_POINTER_SIZE as u64 {
            let bytes = command::blob(root, &index.oid, 0, size as usize, size, &scope).await?;
            if let Some((size, identity)) = pointer_metadata(&bytes) {
                let target = MaterializationTarget {
                    working,
                    bytes: bytes.clone(),
                    index: Some(index),
                };
                let source =
                    FileSource::pointer(repo, root, bytes, size, identity, Some(target), scope)?;
                return Ok(Self {
                    source: RestoreSource::Pointer(Arc::new(source)),
                });
            }
        }
        Ok(Self {
            source: RestoreSource::Git(Arc::new(GitRestoration {
                index,
                working,
                size,
                scope,
            })),
        })
    }

    pub(crate) fn size(&self) -> u64 {
        match &self.source {
            RestoreSource::Pointer(source) => source.size,
            RestoreSource::Git(source) => source.size,
        }
    }

    pub(crate) async fn release_reader(&self) -> Result<()> {
        match &self.source {
            RestoreSource::Pointer(source) => source.release_reader().await,
            RestoreSource::Git(_) => Ok(()),
        }
    }

    pub(crate) async fn run(
        self: Arc<Self>,
        progress: impl FnMut(u64) -> Result<()> + Send + 'static,
    ) -> Result<u64> {
        match &self.source {
            RestoreSource::Pointer(source) => source.clone().materialize(progress).await,
            RestoreSource::Git(source) => source.clone().run(progress).await,
        }
    }
}

impl GitRestoration {
    async fn run(
        self: Arc<Self>,
        progress: impl FnMut(u64) -> Result<()> + Send + 'static,
    ) -> Result<u64> {
        let Self {
            index,
            working,
            size,
            scope,
        } = self.as_ref();
        scope.check()?;
        working.check_metadata()?;
        let source_index = command::git_path(&index.root, "index", scope).await?;
        let (workspace, temporary) =
            Workspace::create(&index.root, working.temporary_parent()?, scope).await?;
        let (workspace, private_index) = workspace.additional_temporary().await?;
        let result = async {
            let mut input = tokio::fs::File::open(source_index).await?;
            let mut output = tokio::fs::File::from_std(private_index.as_file().try_clone()?);
            tokio::io::copy(&mut input, &mut output).await?;
            output.flush().await?;
            command::conversion::copy(
                index,
                private_index.path(),
                *size,
                temporary.as_file().try_clone()?,
                scope,
                progress,
            )
            .await?;
            let runtime = tokio::runtime::Handle::current();
            let captured = self.clone();
            tokio::task::spawn_blocking(move || {
                let Self {
                    index,
                    working,
                    scope,
                    ..
                } = captured.as_ref();
                let prepared = (|| {
                    temporary.as_file().sync_all()?;
                    let written = temporary.as_file().metadata()?.len();
                    let permissions = working.check(scope)?;
                    index.apply_mode(temporary.as_file(), permissions)?;
                    runtime.block_on(index.check(scope))?;
                    scope.check()?;
                    Ok::<_, AgentError>(written)
                })();
                working.publish(temporary, scope, prepared)
            })
            .await
            .map_err(std::io::Error::other)?
        }
        .await;
        workspace.finish(result).await
    }
}

#[cfg(test)]
mod tests;
