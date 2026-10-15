use std::fs::File;
use std::iter::Peekable;
use std::sync::Arc;
use std::vec::IntoIter;

use super::{
    Bytes, IndexedCoalescedReadGroup, MerkleHash, Path, PreparedChunkLocator, ReaderPool, Result,
    StagedChunkLocator, StagingAreaReadOnly, StagingError, StagingMetrics, XorbParser,
    decode_prepared_chunk, load_prepared_source, lock_index, pin_segment_read_groups, push_plan,
    read_located_staged_chunks_batch, read_prepared_staged_chunks_batch,
};

enum CoalescedChunkAuthority {
    Segments(Vec<(u64, StagedChunkLocator)>),
    Prepared(Vec<(u64, MerkleHash, PreparedChunkLocator)>),
}

/// One bounded residual-read unit. Prepared units contain exactly one source xorb.
struct CoalescedChunkReadGroup {
    authority: CoalescedChunkAuthority,
}

/// Connection-local residual planner backed by SQLite temporary storage.
///
/// Appends are bounded by the caller's recipe page. Reads remove one bounded
/// segment batch or one complete prepared source xorb at a time.
pub struct CoalescedChunkReadPlan<'a> {
    staging: &'a StagingAreaReadOnly,
    next_sequence: u64,
    prepared: Option<PreparedCursor>,
    pending: Option<tokio::task::JoinHandle<Result<DecodedBatch>>>,
    buffered: Peekable<IntoIter<(u64, MerkleHash, Bytes)>>,
}

impl<'a> CoalescedChunkReadPlan<'a> {
    pub(super) fn new(staging: &'a StagingAreaReadOnly) -> Result<Self> {
        lock_index(&staging.index)?.begin_coalesced_read_plan()?;
        Ok(Self {
            staging,
            next_sequence: 0,
            prepared: None,
            pending: None,
            buffered: Vec::new().into_iter().peekable(),
        })
    }

    /// Append a bounded request page with an opaque caller context per chunk.
    pub fn append(&mut self, requests: &[(MerkleHash, u64, u64)]) -> Result<()> {
        let mut indexed = Vec::with_capacity(requests.len());
        for (hash, size, context) in requests {
            indexed.push((self.next_sequence, <[u8; 32]>::from(*hash), *size, *context));
            self.next_sequence = self.next_sequence.checked_add(1).ok_or_else(|| {
                StagingError::StagingCorrupt("residual request sequence overflow".to_owned())
            })?;
        }
        lock_index(&self.staging.index)?.append_coalesced_read_requests(&indexed)
    }

    /// Read one segment batch or complete prepared source.
    ///
    /// This preserves the complete-source API shipped in v1.2.4. New callers
    /// should use `read_next_bounded`; removing this method requires a breaking release.
    pub async fn read_next(
        &self,
        max_segment_chunks: usize,
        max_segment_bytes: u64,
    ) -> Result<Option<Vec<(u64, MerkleHash, Bytes)>>> {
        // The complete-source method is the v1.2.4 public contract. Mixing it
        // with a partially consumed bounded source would silently skip chunks.
        if self.prepared.is_some() || self.pending.is_some() || self.buffered.len() > 0 {
            return Err(StagingError::StagingCorrupt(
                "bounded read is not drained".to_owned(),
            ));
        }
        let group = lock_index(&self.staging.index)?
            .take_coalesced_read_group(max_segment_chunks, max_segment_bytes)?
            .map(|group| match group {
                IndexedCoalescedReadGroup::Segments(chunks) => CoalescedChunkReadGroup {
                    authority: CoalescedChunkAuthority::Segments(
                        chunks
                            .into_iter()
                            .map(|(context, chunk)| (context, StagedChunkLocator::from(chunk)))
                            .collect(),
                    ),
                },
                IndexedCoalescedReadGroup::Prepared(chunks) => CoalescedChunkReadGroup {
                    authority: CoalescedChunkAuthority::Prepared(
                        chunks
                            .into_iter()
                            .map(|(context, hash, locator)| {
                                (context, MerkleHash::from(hash), locator)
                            })
                            .collect(),
                    ),
                },
            });
        match group {
            Some(group) => read_coalesced_chunk_group(
                &self.staging.root,
                &self.staging.readers,
                self.staging.metrics.as_deref(),
                group,
            )
            .await
            .map(Some),
            None => Ok(None),
        }
    }
    /// Read a bounded decoded batch while retaining at most one serialized source.
    ///
    /// One oversized chunk may exceed the byte target to ensure progress; the
    /// xorb format bounds that chunk. Finish appending requests before reading
    /// to coalesce every occurrence of one source into one verified open.
    /// Retain selected recipe leases or a push snapshot until reading completes.
    /// Cancelling a pending call keeps its worker result for the next call.
    pub async fn read_next_bounded(
        &mut self,
        max_chunks: usize,
        max_bytes: u64,
    ) -> Result<Option<Vec<(u64, MerkleHash, Bytes)>>> {
        if self.buffered.len() == 0 {
            if self.pending.is_none() {
                let task = if let Some(cursor) = self.prepared.take() {
                    tokio::task::spawn_blocking(move || cursor.batch(max_chunks, max_bytes))
                } else {
                    let index = lock_index(&self.staging.index)?;
                    let Some(group) = index.take_coalesced_read_group(max_chunks, max_bytes)?
                    else {
                        return Ok(None);
                    };
                    let metrics = self.staging.metrics.clone();
                    match group {
                        IndexedCoalescedReadGroup::Segments(chunks) => {
                            let locators = chunks
                                .iter()
                                .map(|(_, chunk)| StagedChunkLocator::from(*chunk))
                                .collect::<Vec<_>>();
                            let pinned = pin_segment_read_groups(&self.staging.readers, &locators)?;
                            tokio::spawn(async move {
                                let payloads = read_located_staged_chunks_batch(
                                    pinned,
                                    metrics.as_deref(),
                                    &locators,
                                )
                                .await?;
                                let chunks = chunks
                                    .into_iter()
                                    .zip(payloads)
                                    .map(|((context, chunk), (_, data))| {
                                        (context, MerkleHash::from(chunk.chunk_hash), data)
                                    })
                                    .collect();
                                Ok(DecodedBatch { next: None, chunks })
                            })
                        }
                        IndexedCoalescedReadGroup::Prepared(chunks) => {
                            let (_, _, first) = chunks.first().ok_or_else(|| {
                                StagingError::StagingCorrupt("empty prepared read group".to_owned())
                            })?;
                            let identity = (first.xorb_hash, first.payload_hash, first.xorb_bytes);
                            // Pin before releasing locator ownership or queuing the
                            // worker; retirement cannot invalidate this descriptor.
                            let file = File::open(push_plan::prepared_xorb_path(
                                &self.staging.root,
                                &MerkleHash::from(identity.0),
                            ))?;
                            tokio::task::spawn_blocking(move || {
                                let activity = PreparedReadActivity::new(metrics);
                                let parser = load_prepared_source(file, identity)?;
                                if let Some(metrics) = activity.0.as_deref() {
                                    metrics.add_staging_bytes_read(identity.2);
                                    metrics.add_prepared_source_xorb_bytes_read(identity.2);
                                }
                                PreparedCursor {
                                    parser,
                                    chunks: chunks
                                        .into_iter()
                                        .map(|(context, hash, locator)| {
                                            (context, MerkleHash::from(hash), locator)
                                        })
                                        .collect::<Vec<_>>()
                                        .into_iter()
                                        .peekable(),
                                }
                                .batch(max_chunks, max_bytes)
                            })
                        }
                    }
                };
                // Await the stored handle by reference. Cancelling this read keeps
                // its owned worker and result available to the next call.
                self.pending = Some(task);
            }
            let pending = self.pending.as_mut().ok_or_else(|| {
                StagingError::Internal("bounded read worker disappeared".to_owned())
            })?;
            let result = pending.await;
            self.pending = None;
            let batch = result.map_err(std::io::Error::other)??;
            self.prepared = batch.next;
            self.buffered = batch.chunks.into_iter().peekable();
        }
        // A cancelled call may have queued a larger batch. Bound the returned
        // prefix using this call's limits and retain the remaining owned bytes.
        let mut out = Vec::new();
        let mut bytes = 0_u64;
        while let Some(chunk) = self.buffered.next_if(|(_, _, data)| {
            out.is_empty()
                || (out.len() < max_chunks.max(1)
                    && bytes.saturating_add(data.len() as u64) <= max_bytes.max(1))
        }) {
            bytes = bytes.saturating_add(chunk.2.len() as u64);
            out.push(chunk);
        }
        Ok(Some(out))
    }
}

impl Drop for CoalescedChunkReadPlan<'_> {
    fn drop(&mut self) {
        if let Ok(index) = lock_index(&self.staging.index) {
            let _ = index.clear_coalesced_read_plan();
        }
    }
}

struct DecodedBatch {
    next: Option<PreparedCursor>,
    chunks: Vec<(u64, MerkleHash, Bytes)>,
}

// The worker owns activity accounting, so dropping an awaiting future cannot
// leave a reader permanently active after its pinned source is released.
struct PreparedReadActivity(Option<Arc<dyn StagingMetrics>>);

impl PreparedReadActivity {
    fn new(metrics: Option<Arc<dyn StagingMetrics>>) -> Self {
        if let Some(metrics) = metrics.as_deref() {
            metrics.inc_prepared_source_xorb_open();
            metrics.prepared_source_reader_started();
        }
        Self(metrics)
    }
}

impl Drop for PreparedReadActivity {
    fn drop(&mut self) {
        if let Some(metrics) = self.0.as_deref() {
            metrics.prepared_source_reader_finished();
        }
    }
}

struct PreparedCursor {
    parser: XorbParser,
    chunks: Peekable<IntoIter<(u64, MerkleHash, PreparedChunkLocator)>>,
}

impl PreparedCursor {
    fn batch(mut self, max_chunks: usize, max_bytes: u64) -> Result<DecodedBatch> {
        let mut out = Vec::new();
        let mut bytes = 0_u64;
        while let Some((context, hash, locator)) = self.chunks.next_if(|(_, _, locator)| {
            out.is_empty()
                || (out.len() < max_chunks.max(1)
                    && bytes.saturating_add(u64::from(locator.size)) <= max_bytes.max(1))
        }) {
            let data =
                decode_prepared_chunk(&self.parser, hash, locator.chunk_index, locator.size)?;
            bytes = bytes.saturating_add(u64::from(locator.size));
            out.push((context, hash, data));
        }
        let next = self.chunks.peek().is_some().then_some(self);
        Ok(DecodedBatch { next, chunks: out })
    }
}

async fn read_coalesced_chunk_group(
    root: &Path,
    readers: &ReaderPool,
    metrics: Option<&dyn StagingMetrics>,
    group: CoalescedChunkReadGroup,
) -> Result<Vec<(u64, MerkleHash, Bytes)>> {
    match group.authority {
        CoalescedChunkAuthority::Segments(chunks) => {
            let locators = chunks
                .iter()
                .map(|(_, locator)| *locator)
                .collect::<Vec<_>>();
            let payloads = read_located_staged_chunks_batch(
                pin_segment_read_groups(readers, &locators)?,
                metrics,
                &locators,
            )
            .await?;
            Ok(chunks
                .into_iter()
                .zip(payloads)
                .map(|((position, locator), (_, payload))| (position, locator.hash, payload))
                .collect())
        }
        CoalescedChunkAuthority::Prepared(chunks) => {
            let contexts = chunks
                .iter()
                .map(|(context, _, _)| *context)
                .collect::<Vec<_>>();
            let indexed = chunks
                .into_iter()
                .enumerate()
                .map(|(position, (_, hash, locator))| (position, hash, locator))
                .collect::<Vec<_>>();
            Ok(read_prepared_staged_chunks_batch(root, metrics, &indexed)
                .await?
                .into_iter()
                .map(|(position, payload)| {
                    indexed
                        .get(position)
                        .and_then(|(_, hash, _)| {
                            contexts
                                .get(position)
                                .map(|context| (*context, *hash, payload))
                        })
                        .ok_or_else(|| {
                            StagingError::Internal(
                                "prepared read returned an unknown position".to_owned(),
                            )
                        })
                })
                .collect::<Result<Vec<_>>>()?)
        }
    }
}
