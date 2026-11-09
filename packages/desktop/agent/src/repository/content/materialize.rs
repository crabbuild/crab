use std::io::{Read, Write};
use std::time::{Duration, Instant};

use super::*;

pub(super) struct WorkingPointer {
    requested: PathBuf,
    path: PathBuf,
    bytes: Bytes,
    modified: Option<SystemTime>,
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
            bytes,
            modified: metadata.modified().ok(),
        }
    }

    fn check(&self) -> Result<std::fs::Permissions> {
        let metadata = std::fs::symlink_metadata(&self.requested)?;
        if !metadata.is_file()
            || metadata.modified().ok() != self.modified
            || metadata.len() != self.bytes.len() as u64
            || std::fs::canonicalize(&self.requested)? != self.path
        {
            return Err(changed());
        }
        let mut bytes = Vec::with_capacity(self.bytes.len() + 1);
        std::fs::File::open(&self.requested)?
            .take(self.bytes.len() as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes != self.bytes {
            return Err(changed());
        }
        Ok(metadata.permissions())
    }
}

impl FileSource {
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
                self.scope.check()?;
                let permissions = target.check()?;
                temporary.as_file().set_permissions(permissions)?;
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
            if let Err(error) = temporary.persist(&target.path) {
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
