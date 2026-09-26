use std::{
    io,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use futures_util::{StreamExt as _, TryStreamExt as _, stream};

use super::merge::LocatorMerge;
use super::{
    CellReplica, DirectoryEntry, LoadedGraph, PreparedRoot, RootRef, SEGMENT_TRANSFER_CONCURRENCY,
    SegmentDescriptor, directory,
};
use crate::{CellObjectKind, CrabError, Result, SegmentInfo, Txid, environment::FileIo};

mod output;
mod source;

pub(super) mod scratch;
use scratch::ScratchFiles;

use output::*;
use scratch::upload;
pub(super) use scratch::upload_source;
use source::*;

const INDEX_READ_BYTES: u64 = 60 * 8_192;
const FRAME_READ_BYTES: u64 = 1 << 20;

pub(super) async fn prepare(
    replica: &CellReplica,
    base: &RootRef,
    graph: LoadedGraph,
    range: Range<usize>,
    level: u8,
    scratch_directory: &Path,
) -> Result<PreparedRoot> {
    let selected = graph
        .descriptors
        .get(range.clone())
        .filter(|segments| !segments.is_empty())
        .ok_or(CrabError::TxNotAvailable)?;
    if !(1..=9).contains(&level)
        || (level == 9 && (range.start != 0 || range.end != graph.descriptors.len()))
        || (level < 9 && selected.iter().any(|segment| segment.level() >= level))
    {
        return Err(CrabError::InvalidState("invalid compaction level or range"));
    }

    let host = replica.host.clone();
    let directory = scratch_directory.to_owned();
    let runtime = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
    let (cleaned, cleanup) = tokio::sync::oneshot::channel();
    let result = async {
        let files = replica
            .host
            .run(move || {
                let mut scratch = ScratchFiles::new(host, &directory, runtime, cleaned);
                Ok::<_, CrabError>(CompactionFiles {
                    original_indexes: scratch.create("source-indexes")?,
                    original_bodies: scratch.create("source-bodies")?,
                    compacted_ltx: scratch.create("compacted-ltx")?,
                    codec_index: scratch.create("codec-index")?,
                    compacted_index: scratch.create("compacted-index")?,
                    scratch: Arc::new(scratch),
                })
            })
            .await??;
        prepare_root(replica, base, graph, range, level, files).await
    }
    .await;
    // Ordinary completion includes cleanup. Cancellation leaves cleanup owned
    // by the last dispatched file job instead of unlinking its inputs early.
    let _ = cleanup.await;
    result
}

struct CompactionFiles {
    original_indexes: PathBuf,
    original_bodies: PathBuf,
    compacted_ltx: PathBuf,
    codec_index: PathBuf,
    compacted_index: PathBuf,
    scratch: Arc<ScratchFiles>,
}

async fn prepare_root(
    replica: &CellReplica,
    base: &RootRef,
    graph: LoadedGraph,
    range: Range<usize>,
    level: u8,
    files: CompactionFiles,
) -> Result<PreparedRoot> {
    let selected = &graph.descriptors[range.clone()];
    // The authenticated streams have separate scratch files. Both must finish
    // before the merge, but neither depends on the other's transfer.
    let (spooled, body_inputs) = futures_util::future::join(
        spool_indexes(replica, selected, &files.scratch, &files.original_indexes),
        spool_selected_bodies(replica, selected, &files.scratch, &files.original_bodies),
    )
    .await;
    let spooled = spooled?;
    let body_inputs = body_inputs?;
    let artifacts = write_compacted(replica, spooled, &body_inputs, &files).await?;

    let first = selected.first().ok_or(CrabError::TxNotAvailable)?;
    let last = selected.last().ok_or(CrabError::TxNotAvailable)?;
    let info = SegmentInfo {
        min_txid: first.info.min_txid,
        max_txid: last.info.max_txid,
        page_size: first.info.page_size,
        database_pages: last.info.database_pages,
        pre_checksum: first.info.pre_checksum,
        post_checksum: last.info.post_checksum,
        size_bytes: artifacts.ltx.length,
        blake3: artifacts.ltx.digest,
    };
    let descriptor =
        SegmentDescriptor::native(info, artifacts.index.digest, artifacts.index.length)
            .with_level(level);
    descriptor.validate_published(replica.limits)?;

    // These immutable objects are unreachable until the final root is returned,
    // so either upload may finish first without publishing a partial compaction.
    let (body_upload, index_upload) = futures_util::future::join(
        upload(
            replica,
            &files.scratch,
            &files.compacted_ltx,
            artifacts.ltx.length,
            &descriptor.info.blake3,
            CellObjectKind::Ltx,
        ),
        upload(
            replica,
            &files.scratch,
            &files.compacted_index,
            artifacts.index.length,
            &descriptor.index_digest,
            CellObjectKind::Index,
        ),
    )
    .await;
    body_upload?;
    index_upload?;

    let mut descriptors = graph.descriptors.clone();
    descriptors.splice(range.clone(), [descriptor.clone()]);
    replica.validate_chain(&descriptors, base.position)?;

    let compacted_source = files.scratch.open(&files.compacted_index).await?;
    let compacted_input = SpoolInput {
        descriptor,
        start: 0,
        length: artifacts.index.length,
    };
    let entries =
        MergedEntries::open(&replica.host, compacted_source, vec![compacted_input]).await?;
    let endpoint = descriptors.last().ok_or(CrabError::LTXCorrupted)?;
    let page_size = endpoint.info.page_size;
    let database_pages = endpoint.info.database_pages;
    let directory = directory::relocate_and_upload(
        replica,
        &graph,
        &descriptors,
        selected,
        entries.stream(replica.host.clone()),
    )
    .await?;
    replica
        .finish_root(
            Some(base),
            descriptors,
            base.position,
            base.commit_sequence,
            graph.document.schema,
            page_size,
            database_pages,
            directory,
        )
        .await
}

struct CompactedArtifacts {
    ltx: Artifact,
    index: Artifact,
}

struct Artifact {
    digest: [u8; 32],
    length: u64,
}

#[derive(Clone)]
struct SpoolInput {
    descriptor: SegmentDescriptor,
    start: u64,
    length: u64,
}

#[derive(Clone)]
struct BodySpoolInput {
    descriptor: SegmentDescriptor,
    start: u64,
}

struct LocalBodyRange {
    start: u64,
    length: usize,
}

struct MergedEntries {
    source: Box<dyn FileIo>,
    cursors: Vec<SpoolCursor>,
    merge: LocatorMerge,
}

impl MergedEntries {
    async fn open(
        host: &crate::Host,
        mut source: Box<dyn FileIo>,
        inputs: Vec<SpoolInput>,
    ) -> Result<Self> {
        host.run(move || {
            const TOTAL_BUFFER_ENTRIES: usize = 16_384;
            let buffer_entries = TOTAL_BUFFER_ENTRIES
                .checked_div(inputs.len())
                .filter(|entries| *entries > 0)
                .ok_or(CrabError::LTXCorrupted)?
                .min(1_024);
            let mut merge = LocatorMerge::new(
                inputs
                    .iter()
                    .map(|input| input.descriptor.info.database_pages),
            );
            let mut cursors = Vec::with_capacity(inputs.len());
            for input in inputs {
                let cursor = SpoolCursor::new(input, source.as_mut(), buffer_entries)?;
                if let Some(entry) = &cursor.current {
                    merge.push(cursors.len(), entry.page);
                }
                cursors.push(cursor);
            }
            Ok(Self {
                source,
                cursors,
                merge,
            })
        })
        .await?
    }

    fn next_batch(&mut self) -> Result<(Vec<DirectoryEntry>, bool)> {
        let mut batch = Vec::new();
        let mut visited = 0;
        // Stop between page groups, including discarded pages. One group
        // visits at most the admitted descriptor count.
        while visited < 4_096 {
            let next = self.merge.next_group(|index| {
                visited += 1;
                let cursor = self.cursors.get_mut(index).ok_or(CrabError::LTXCorrupted)?;
                let entry = cursor.current.take().ok_or(CrabError::LTXCorrupted)?;
                cursor.advance(self.source.as_mut())?;
                Ok((entry, cursor.current.as_ref().map(|entry| entry.page)))
            });
            match next {
                Some(Ok(Some(entry))) => batch.push(entry),
                Some(Ok(None)) => {}
                Some(Err(error)) => return Err(error),
                None => return Ok((batch, true)),
            }
        }
        Ok((batch, false))
    }

    fn stream(self, host: crate::Host) -> impl futures_util::Stream<Item = Result<DirectoryEntry>> {
        stream::try_unfold(Some(self), move |entries| {
            let host = host.clone();
            async move {
                let Some(mut entries) = entries else {
                    return Ok::<_, CrabError>(None);
                };
                let (entries, batch, finished) = host
                    .run(move || {
                        let (batch, finished) = entries.next_batch()?;
                        Ok::<_, CrabError>((entries, batch, finished))
                    })
                    .await??;
                if finished && batch.is_empty() {
                    return Ok(None);
                }
                Ok(Some((
                    stream::iter(batch.into_iter().map(Ok)),
                    (!finished).then_some(entries),
                )))
            }
        })
        .try_flatten()
    }
}
