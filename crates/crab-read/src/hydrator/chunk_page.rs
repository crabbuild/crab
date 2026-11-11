use std::collections::{HashMap, HashSet};

use crab_diff::chunk_sequence::ChunkSpan;
use crab_metadata::file_index_lookup::SharedFileIndexLookup;
use crab_types::pointer::Pointer;
use crab_xet::hash::MerkleHash;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::ShardHydrator;
use crate::term_resolver::{get_or_download_shard, resolve_term_chunks, term_spans};
use crate::{ReadError, Result};

/// A bounded page of ordered chunk occurrences from one captured file recipe.
pub struct FileChunkPage {
    pub chunk_count: u64,
    /// Local recipe inspection does not claim published xorb placement.
    pub xorb_count: Option<u64>,
    pub chunks: Vec<ChunkSpan>,
    pub next: Option<u64>,
}

impl From<FileChunkPage> for crab_types::file_layout::ChunkPage {
    fn from(page: FileChunkPage) -> Self {
        // Xet hash text has its own byte ordering. Encode it here so the SDK
        // and native protocol cannot disagree with catalog/xorb identities.
        Self {
            chunk_count: page.chunk_count,
            xorb_count: page.xorb_count,
            next: page.next,
            chunks: page
                .chunks
                .into_iter()
                .map(|chunk| crab_types::file_layout::Chunk {
                    hash: chunk.chunk_hash.hex(),
                    offset: chunk.offset,
                    length: chunk.len,
                    xorb_hash: chunk.origin.xorb_hash.map(|hash| hash.hex()),
                    xorb_chunk_index: chunk.origin.xorb_chunk_index,
                })
                .collect(),
        }
    }
}

impl ShardHydrator {
    /// Inspect at most 512 chunks through the caller's captured file catalog.
    ///
    /// The cursor counts occurrences, including repeated hashes. This reads
    /// shard/xorb metadata, never reconstructs file payloads, and leaves lookup
    /// close with the caller. Invalid cursors, limits and coverage return errors.
    pub async fn chunk_page(
        &self,
        pointer: &Pointer,
        lookup: Option<&SharedFileIndexLookup>,
        start: u64,
        limit: u32,
        cancel: &CancellationToken,
    ) -> Result<FileChunkPage> {
        let lookup =
            lookup
                .or(self.file_index_lookup.as_ref())
                .ok_or_else(|| ReadError::Configuration {
                    key: "chunk inspection requires a captured file catalog".into(),
                    origin: "file chunk inspection".into(),
                })?;
        crate::read_retention::read(self.retention.as_ref(), cancel, |_, cancel| async move {
            self.chunk_page_inner(pointer, lookup, start, limit, &cancel)
                .await
        })
        .await
    }

    async fn chunk_page_inner(
        &self,
        pointer: &Pointer,
        lookup: &SharedFileIndexLookup,
        start: u64,
        limit: u32,
        cancel: &CancellationToken,
    ) -> Result<FileChunkPage> {
        check_cancelled(cancel)?;
        if !(1..=512).contains(&limit) {
            return Err(invalid());
        }
        let file_hash = MerkleHash::from(pointer.file_hash);
        // Pointer hints do not authorize inspection outside the selected
        // snapshot. Resolve through its captured catalog before loading metadata.
        let shard_hash = lookup
            .lookup(&file_hash)
            .await?
            .ok_or_else(|| ReadError::NotFound {
                path: format!("file_index:{}", file_hash.hex()),
            })?;
        check_cancelled(cancel)?;
        let readers = Mutex::new(HashMap::new());
        let cache = self.store.local_cache();
        let shard =
            get_or_download_shard(cache, &self.store, &self.router, &readers, &shard_hash).await?;
        check_cancelled(cancel)?;
        let file = shard
            .get_file_info(&file_hash)?
            .ok_or_else(|| ReadError::NotFound {
                path: format!("file:{} in shard:{}", file_hash.hex(), shard_hash.hex()),
            })?;
        let corrupt = || ReadError::CorruptObject {
            path: format!("shard:{}", shard_hash.hex()),
            reason: "file chunk counts or byte coverage are invalid".into(),
        };
        let mut chunk_count = 0_u64;
        let mut file_size = 0_u64;
        let mut xorbs = HashSet::new();
        for term in &file.segments {
            check_cancelled(cancel)?;
            let count = term
                .chunk_index_end
                .checked_sub(term.chunk_index_start)
                .filter(|count| *count > 0)
                .ok_or_else(corrupt)?;
            if term.unpacked_segment_bytes == 0 {
                return Err(corrupt());
            }
            chunk_count = chunk_count
                .checked_add(u64::from(count))
                .ok_or_else(corrupt)?;
            file_size = file_size
                .checked_add(u64::from(term.unpacked_segment_bytes))
                .ok_or_else(corrupt)?;
            xorbs.insert(&term.xorb_hash);
        }
        if file_size != pointer.size {
            return Err(corrupt());
        }
        if start > chunk_count {
            return Err(invalid());
        }
        let end = start.saturating_add(u64::from(limit)).min(chunk_count);
        let mut chunks = Vec::with_capacity((end - start) as usize);
        let metadata = Mutex::new(HashMap::new());
        let mut occurrence = 0_u64;
        let mut offset = 0_u64;
        for term in &file.segments {
            check_cancelled(cancel)?;
            let term_end = occurrence + u64::from(term.chunk_index_end - term.chunk_index_start);
            if occurrence < end && term_end > start {
                let entries = resolve_term_chunks(
                    &self.store,
                    &self.router,
                    &metadata,
                    &shard,
                    &shard_hash,
                    term,
                )
                .await?;
                check_cancelled(cancel)?;
                let skip = start.saturating_sub(occurrence) as usize;
                let take = (end.min(term_end) - start.max(occurrence)) as usize;
                chunks.extend(
                    term_spans(term, &entries, offset, &shard_hash)?
                        .skip(skip)
                        .take(take),
                );
            }
            occurrence = term_end;
            offset += u64::from(term.unpacked_segment_bytes);
            if occurrence >= end {
                break;
            }
        }
        check_cancelled(cancel)?;
        Ok(FileChunkPage {
            chunk_count,
            xorb_count: Some(xorbs.len() as u64),
            chunks,
            next: (end < chunk_count).then_some(end),
        })
    }
}

fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(ReadError::Cancelled);
    }
    Ok(())
}

fn invalid() -> ReadError {
    ReadError::Configuration {
        key: "invalid chunk page cursor or limit".into(),
        origin: "file chunk inspection".into(),
    }
}
