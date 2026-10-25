use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use crab_staging::{StagedFilePin, StagedFileReader};
use crab_types::pointer::Pointer;
use crab_xet::hash::MerkleHash;
use tokio_util::sync::CancellationToken;

use crate::{ReadError, Result, ShardHydrator};

/// Verified range bytes and the reconstruction source used for diagnostics.
pub struct PointerRead {
    pub bytes: Bytes,
    /// Whether reconstruction used the configured store runtime; its caches may satisfy the read.
    pub from_store: bool,
}

/// Reads published content from its captured view and unpublished content from staging.
pub struct PointerReader {
    remote: Option<Arc<ShardHydrator>>,
    staging_root: Option<PathBuf>,
}

enum PointerSource {
    Staged(StagedFileReader),
    Store(Pointer),
}

impl PointerReader {
    /// Bind a repository's staging namespace and its optional captured remote view.
    #[must_use]
    pub fn new(remote: Option<Arc<ShardHydrator>>, staging_root: Option<PathBuf>) -> Self {
        Self {
            remote,
            staging_root,
        }
    }

    /// Whether this reader can reconstruct through a configured store runtime.
    #[must_use]
    pub fn has_remote(&self) -> bool {
        self.remote.is_some()
    }

    /// Retain unpublished local content for a descriptor without blocking staging writers.
    pub async fn pin_file(&self, pointer: &Pointer) -> Result<Option<StagedFilePin>> {
        let Some(root) = &self.staging_root else {
            return Ok(None);
        };
        // Absent staging owns no local bytes, so published opens need no catalog
        // lookup here. Other lookup errors follow canonical source selection.
        if matches!(tokio::fs::try_exists(root).await, Ok(false)) {
            return Ok(None);
        }
        match self.source(pointer).await? {
            PointerSource::Staged(local) => local.into_pin().map(Some).map_err(Into::into),
            PointerSource::Store(_) => Ok(None),
        }
    }

    /// Read a clamped range, verifying local chunks or canonical remote reconstruction.
    pub async fn read_range(
        &self,
        pointer: &Pointer,
        range: Range<u64>,
        cancel: &CancellationToken,
    ) -> Result<PointerRead> {
        if cancel.is_cancelled() {
            return Err(ReadError::Cancelled);
        }
        if range.start > range.end {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid pointer read range",
            )
            .into());
        }
        let range = range.start.min(pointer.size)..range.end.min(pointer.size);
        if range.is_empty() {
            return Ok(PointerRead {
                bytes: Bytes::new(),
                from_store: false,
            });
        }
        match self.source(pointer).await? {
            PointerSource::Staged(local) => Ok(PointerRead {
                bytes: local.read_range(range, cancel).await?,
                from_store: false,
            }),
            PointerSource::Store(pointer) => Ok(PointerRead {
                bytes: self
                    .remote(&pointer)?
                    .reconstruct_range_with_cancel(&pointer, range, cancel)
                    .await?
                    .into(),
                from_store: true,
            }),
        }
    }

    /// Reconstruct a whole pointer, retaining local ownership through verified completion.
    pub async fn reconstruct_to_path(
        &self,
        pointer: &Pointer,
        dest: &Path,
        cancel: &CancellationToken,
    ) -> Result<u64> {
        if cancel.is_cancelled() {
            return Err(ReadError::Cancelled);
        }
        match self.source(pointer).await? {
            PointerSource::Staged(local) => local
                .reconstruct_to_path(dest, cancel)
                .await
                .map_err(Into::into),
            PointerSource::Store(pointer) => {
                self.remote(&pointer)?
                    .reconstruct_to_path_with_cancel(&pointer, dest, cancel)
                    .await
            }
        }
    }

    async fn source(&self, pointer: &Pointer) -> Result<PointerSource> {
        let Some(root) = &self.staging_root else {
            return Ok(PointerSource::Store(pointer.clone()));
        };
        if let Some(remote) = &self.remote
            && let Some(shard) = remote.captured_shard(pointer).await?
        {
            // Captured publication already owns these bytes. An unrelated local
            // staging writer cannot shadow it. Carry the captured shard into
            // reconstruction rather than looking up the same catalog binding twice.
            let mut pointer = pointer.clone();
            pointer.shard_hint = Some(shard.into());
            return Ok(PointerSource::Store(pointer));
        }
        // Missing local ownership selects origin. A corrupt selected recipe is
        // an error; widening the remote catalog cannot repair unpublished bytes.
        let local = StagedFileReader::open(
            root.clone(),
            MerkleHash::from(pointer.file_hash),
            pointer.size,
        )
        .await?;
        Ok(match local {
            Some(local) => PointerSource::Staged(local),
            None => PointerSource::Store(pointer.clone()),
        })
    }

    fn remote(&self, pointer: &Pointer) -> Result<&ShardHydrator> {
        self.remote.as_deref().ok_or_else(|| ReadError::NotFound {
            path: format!(
                "pointer content {}",
                MerkleHash::from(pointer.file_hash).hex()
            ),
        })
    }
}
