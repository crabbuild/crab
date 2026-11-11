use std::io::{Read, Write};

use super::materialize::MaterializationTarget;
use super::publication::{CheckoutWorkspace, IndexFile, WorkingFile, unchanged_file};
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
    workspace: Mutex<Option<CheckoutWorkspace>>,
}

impl Restoration {
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
            // Authenticate the exact pointer bytes retained by the SDK reader,
            // rather than trusting a syntactically valid but corrupt Git blob.
            let header = format!("{} blob {size}\n", index.oid);
            let body = Read::chain(
                std::io::Cursor::new(header),
                std::io::Cursor::new(bytes.as_ref()),
            );
            let input = Read::chain(body, std::io::Cursor::new(b"\n"));
            crab_git::batch::visit_small_blobs(
                input,
                [index.oid.as_str()],
                0,
                &|| scope.check().is_err(),
                |_, _| Ok(()),
            )?;
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
                workspace: Mutex::new(None),
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
            RestoreSource::Git(source) => {
                let directory = source.workspace.lock().await.take();
                if let Some(directory) = directory {
                    tokio::task::spawn_blocking(move || directory.close())
                        .await
                        .map_err(std::io::Error::other)??;
                }
                Ok(())
            }
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
            workspace,
        } = self.as_ref();
        scope.check()?;
        working.check_metadata()?;
        let source_index = command::index_path(&index.root, scope).await?;
        let directory_path = {
            let mut owner = workspace.lock().await;
            if owner.is_some() {
                return Err(AgentError::Other(
                    "Restore already owns a checkout workspace".into(),
                ));
            }
            let parent = source_index.parent().ok_or_else(changed)?;
            let directory = CheckoutWorkspace::new(parent)?;
            let path = directory.path().to_owned();
            *owner = Some(directory);
            path
        };
        // Cleanup belongs to release_reader, after the job records publication.
        // A cleanup failure must not erase a file that was actually restored.
        async {
            command::verify_blob(&index.root, &index.oid, *size, scope).await?;
            let path =
                command::checkout_index(index, &source_index, &directory_path, scope, progress)
                    .await?;
            // Native Git owns checkout conversions. Verify its immutable input
            // again before admitting converted bytes to the working tree.
            command::verify_blob(&index.root, &index.oid, *size, scope).await?;
            let runtime = tokio::runtime::Handle::current();
            let captured = self.clone();
            tokio::task::spawn_blocking(move || {
                let Self {
                    index,
                    working,
                    scope,
                    ..
                } = captured.as_ref();
                let metadata = std::fs::symlink_metadata(&path)?;
                if !metadata.is_file() {
                    return Err(changed());
                }
                let mut file = std::fs::File::open(&path)?;
                let mut temporary = tempfile::NamedTempFile::new_in(working.temporary_parent()?)?;
                let prepared = (|| {
                    // The Git directory can be on another volume. Copy through
                    // a bounded buffer into the publication filesystem; retain
                    // the private source until both publication and cleanup end.
                    let mut buffer = [0u8; 64 * 1024];
                    let mut written = 0u64;
                    loop {
                        scope.check()?;
                        let count = file.read(&mut buffer)?;
                        if count == 0 {
                            break;
                        }
                        written = written.checked_add(count as u64).ok_or_else(changed)?;
                        if written > metadata.len() {
                            return Err(changed());
                        }
                        temporary.write_all(&buffer[..count])?;
                    }
                    if written != metadata.len()
                        || !unchanged_file(&metadata, &file.metadata()?)
                        || !unchanged_file(&metadata, &std::fs::symlink_metadata(&path)?)
                    {
                        return Err(changed());
                    }
                    temporary.flush()?;
                    temporary.as_file().sync_data()?;
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
        .await
    }
}

#[cfg(test)]
mod tests;
