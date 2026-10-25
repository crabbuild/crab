use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::recipe::{FileRecipe, RECIPE_PAGE_ENTRIES, RecipePage};
use crate::{
    CoalescedChunkReadPlan, Result, StagingAreaReadOnly, StagingError, StagingPushSnapshot,
    lock_index,
};
use crab_xet::hash::MerkleHash;

/// Retains one local recipe without holding staging's process admission lock.
#[must_use = "retain the pin while its unpublished content is in use"]
pub struct StagedFilePin {
    root: PathBuf,
    index: Arc<std::sync::Mutex<crate::index::Index>>,
    id: String,
    marker: std::fs::File,
}

impl StagedFilePin {
    pub(crate) fn from_snapshot(staging: &StagingAreaReadOnly, id: &str) -> Result<Self> {
        let source = crate::inflight_marker_path(&staging.root, id);
        let marker = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&source)?;
        crate::try_flock(&marker, crate::StagingLockKind::Exclusive)?;
        // Recipe capture is already durable. The ready marker's own lock
        // protects its lifetime across PID namespaces, while staging writers
        // and unrelated retirement no longer need to wait for this reader.
        std::fs::rename(&source, crate::read_pin_marker_path(&staging.root, id))?;
        Ok(Self {
            root: staging.root.clone(),
            index: Arc::clone(&staging.index),
            id: id.to_owned(),
            marker,
        })
    }
}

impl Drop for StagedFilePin {
    fn drop(&mut self) {
        // This removes metadata ownership only; physical reclamation remains
        // with staging maintenance. SQLite serializes it with concurrent writers.
        if let Err(error) =
            lock_index(&self.index).and_then(|index| index.remove_push_snapshot(&self.id))
        {
            tracing::warn!(snapshot_id = %self.id, %error, "staged read pin cleanup needs maintenance");
        }
        if let Err(error) = fs4::fs_std::FileExt::unlock(&self.marker) {
            tracing::warn!(snapshot_id = %self.id, %error, "staged read pin unlock failed");
        }
        let path = crate::read_pin_marker_path(&self.root, &self.id);
        if let Err(error) = std::fs::remove_file(path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(snapshot_id = %self.id, %error, "staged read pin marker cleanup failed");
        }
    }
}

/// A retained immutable local recipe, protected from concurrent push retirement.
pub struct StagedFileReader {
    staging: Arc<StagingAreaReadOnly>,
    recipe: FileRecipe,
    _snapshot: StagingPushSnapshot,
    // The residual planner's temporary table belongs to this connection.
    // Concurrent operations on one reader must not overwrite each other's plan.
    reads: tokio::sync::Mutex<()>,
}

impl StagedFileReader {
    /// Transfer this operation's recipe ownership into a lifetime pin without reading bytes.
    pub fn into_pin(self) -> Result<StagedFilePin> {
        self._snapshot.into_read_pin()
    }

    /// Open a retained recipe selected by a Git pointer; absence permits an origin read.
    ///
    /// Existing but corrupt staging and size mismatches remain errors. The shared
    /// handle lasts for this operation, so it cannot block writers for a mount's lifetime.
    pub async fn open(root: PathBuf, hash: MerkleHash, size: u64) -> Result<Option<Self>> {
        match tokio::fs::metadata(&root).await {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let staging = Arc::new(StagingAreaReadOnly::open(root).await?);
        Self::capture(staging, hash, size)
    }

    fn capture(
        staging: Arc<StagingAreaReadOnly>,
        hash: MerkleHash,
        size: u64,
    ) -> Result<Option<Self>> {
        if lock_index(&staging.index)?
            .retained_recipe_for_file(&hash.into())?
            .is_none()
        {
            return Ok(None);
        }
        let id = format!("read-{}", crate::new_staging_batch_id().as_str());
        // Retirement uses shared opens too. Fence it before recipe selection,
        // then pin the exact version; a shared flock alone cannot retain bytes.
        let snapshot = StagingPushSnapshot::begin_read(Arc::clone(&staging), id)?;
        let recipe = lock_index(&staging.index)?.retained_recipe_for_file(&hash.into())?;
        let Some(recipe) = recipe else {
            return Ok(None);
        };
        if recipe.file_size() != size {
            return Err(StagingError::StagingCorrupt(format!(
                "recipe for {} has {} bytes, pointer declares {size}",
                hash.hex(),
                recipe.file_size()
            )));
        }
        snapshot.capture(std::slice::from_ref(&recipe))?;
        Ok(Some(Self {
            staging,
            recipe,
            _snapshot: snapshot,
            reads: tokio::sync::Mutex::new(()),
        }))
    }

    /// Read a bounded range, verifying each selected chunk and exact coverage.
    pub async fn read_range(&self, range: Range<u64>, cancel: &CancellationToken) -> Result<Bytes> {
        self.check_range(&range)?;
        if cancel.is_cancelled() {
            return Err(StagingError::Cancelled);
        }
        let _read = self.reads.lock().await;
        let capacity =
            usize::try_from(range.end - range.start).map_err(|_| StagingError::Configuration {
                key: "staged read range exceeds address space".into(),
                origin: "staging read".into(),
            })?;
        let mut output = Vec::new();
        output
            .try_reserve_exact(capacity)
            .map_err(|error| StagingError::Io(std::io::Error::other(error)))?;
        output.resize(capacity, 0);
        let mut plan = self.plan(&range)?;
        let mut copied = 0u64;
        while let Some(batch) = plan.read_next_bounded(8, 1024 * 1024).await? {
            if cancel.is_cancelled() {
                return Err(StagingError::Cancelled);
            }
            for (offset, _, data) in batch {
                let chunk_end = offset.checked_add(data.len() as u64).ok_or_else(|| {
                    StagingError::StagingCorrupt("staged chunk offset overflow".into())
                })?;
                let selected = range.start.max(offset)..range.end.min(chunk_end);
                if selected.start >= selected.end {
                    return Err(StagingError::StagingCorrupt(
                        "staged reader returned an unrelated chunk".into(),
                    ));
                }
                let start = usize::try_from(selected.start - offset)
                    .map_err(|_| StagingError::StagingCorrupt("chunk offset overflow".into()))?;
                let len = usize::try_from(selected.end - selected.start)
                    .map_err(|_| StagingError::StagingCorrupt("chunk size overflow".into()))?;
                let dest = usize::try_from(selected.start - range.start)
                    .map_err(|_| StagingError::StagingCorrupt("output offset overflow".into()))?;
                let source = data.get(start..start + len).ok_or_else(|| {
                    StagingError::StagingCorrupt("staged chunk is shorter than its range".into())
                })?;
                output
                    .get_mut(dest..dest + len)
                    .ok_or_else(|| {
                        StagingError::StagingCorrupt(
                            "staged chunk exceeds the selected range".into(),
                        )
                    })?
                    .copy_from_slice(source);
                copied = copied.checked_add(len as u64).ok_or_else(|| {
                    StagingError::StagingCorrupt("staged range size overflow".into())
                })?;
            }
        }
        if copied != range.end - range.start {
            return Err(StagingError::StagingCorrupt(
                "staged range is incomplete".into(),
            ));
        }
        if range.start == 0 && range.end == self.recipe.file_size() {
            self.verify_hash(*blake3::hash(&output).as_bytes())?;
        }
        Ok(Bytes::from(output))
    }

    /// Stream a complete file to a destination and verify its whole-file hash.
    pub async fn reconstruct_to_path(
        &self,
        dest: &Path,
        cancel: &CancellationToken,
    ) -> Result<u64> {
        if cancel.is_cancelled() {
            return Err(StagingError::Cancelled);
        }
        let _read = self.reads.lock().await;
        let mut file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(dest)
            .await?;
        let mut plan = self.plan(&(0..self.recipe.file_size()))?;
        let mut written = 0u64;
        while let Some(batch) = plan.read_next_bounded(8, 1024 * 1024).await? {
            if cancel.is_cancelled() {
                return Err(StagingError::Cancelled);
            }
            for (offset, _, data) in batch {
                let end = offset.checked_add(data.len() as u64).ok_or_else(|| {
                    StagingError::StagingCorrupt("staged file offset overflow".into())
                })?;
                if end > self.recipe.file_size() {
                    return Err(StagingError::StagingCorrupt(
                        "staged chunk exceeds the file".into(),
                    ));
                }
                file.seek(std::io::SeekFrom::Start(offset)).await?;
                file.write_all(&data).await?;
                written = written.checked_add(data.len() as u64).ok_or_else(|| {
                    StagingError::StagingCorrupt("staged file size overflow".into())
                })?;
            }
        }
        if written != self.recipe.file_size() {
            return Err(StagingError::StagingCorrupt(
                "staged file size mismatch".into(),
            ));
        }
        // Coalescing groups physical sources rather than file order. Verify the
        // completed output in order, without retaining all decoded chunks in RAM.
        file.seek(std::io::SeekFrom::Start(0)).await?;
        let mut buffer = vec![0; 1024 * 1024];
        let mut hasher = blake3::Hasher::new();
        loop {
            if cancel.is_cancelled() {
                return Err(StagingError::Cancelled);
            }
            let n = file.read(&mut buffer).await?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }
        self.verify_hash(*hasher.finalize().as_bytes())?;
        file.sync_all().await?;
        Ok(written)
    }

    fn plan(&self, range: &Range<u64>) -> Result<CoalescedChunkReadPlan<'_>> {
        let mut plan = self.staging.begin_coalesced_chunk_read_plan()?;
        let mut position = range.start;
        while position < range.end {
            let page = self.page_at_offset(position)?;
            let mut requests = Vec::with_capacity(page.chunks.len());
            for chunk in page.chunks {
                let end = chunk
                    .offset
                    .checked_add(chunk.len)
                    .ok_or_else(|| StagingError::StagingCorrupt("recipe offset overflow".into()))?;
                if end <= position {
                    continue;
                }
                if position >= range.end {
                    break;
                }
                if chunk.offset > position {
                    return Err(StagingError::StagingCorrupt(
                        "recipe range has a gap".into(),
                    ));
                }
                requests.push((chunk.chunk_hash, chunk.len, chunk.offset));
                position = end.min(range.end);
            }
            if requests.is_empty() {
                return Err(StagingError::StagingCorrupt(
                    "recipe range made no progress".into(),
                ));
            }
            plan.append(&requests)?;
        }
        Ok(plan)
    }

    fn check_range(&self, range: &Range<u64>) -> Result<()> {
        if range.start > range.end || range.end > self.recipe.file_size() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "range outside staged file",
            )
            .into());
        }
        Ok(())
    }

    // Recipe pages have a primary key by ordinal, not by byte offset. Binary
    // search keeps random reads bounded without scanning all preceding terms.
    fn page_at_offset(&self, offset: u64) -> Result<RecipePage> {
        let mut low = 0;
        let mut high = self.recipe.page_count();
        while low < high {
            let middle = low + (high - low) / 2;
            let page = self
                .staging
                .recipe_page(&self.recipe, middle * RECIPE_PAGE_ENTRIES as u64)?;
            if offset < page.start_offset {
                high = middle;
            } else if offset >= page.next_offset() {
                low = middle + 1;
            } else {
                return Ok(page);
            }
        }
        Err(StagingError::StagingCorrupt(
            "recipe does not cover requested offset".into(),
        ))
    }

    fn verify_hash(&self, actual: [u8; 32]) -> Result<()> {
        let actual = MerkleHash::from(actual);
        if actual != self.recipe.file_hash() {
            return Err(StagingError::HashMismatch {
                requested: self.recipe.file_hash().hex(),
                actual: actual.hex(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "staging reader test setup and assertions"
)]
mod tests;
