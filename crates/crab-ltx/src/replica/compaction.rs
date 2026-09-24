use std::{cmp::Reverse, collections::BinaryHeap, io, ops::Range, path::Path};

use futures_util::{StreamExt as _, stream};

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
pub(super) use scratch::{upload, upload_source};
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

    let mut scratch = ScratchFiles::new(&replica.host, scratch_directory);
    let original_indexes = scratch.create("source-indexes")?;
    let original_bodies = scratch.create("source-bodies")?;
    let compacted_ltx = scratch.create("compacted-ltx")?;
    let codec_index = scratch.create("codec-index")?;
    let compacted_index = scratch.create("compacted-index")?;
    // The authenticated streams have separate scratch files. Both must finish
    // before the merge, but neither depends on the other's transfer.
    let (spooled, body_inputs) = futures_util::future::join(
        spool_indexes(replica, &graph.descriptors, &original_indexes),
        spool_selected_bodies(replica, selected, &original_bodies),
    )
    .await;
    let spooled = spooled?;
    let body_inputs = body_inputs?;
    let selected_inputs = spooled[range.clone()].to_vec();
    let artifacts = write_compacted(
        replica,
        &selected_inputs,
        &original_indexes,
        &original_bodies,
        &body_inputs,
        &compacted_ltx,
        &codec_index,
        &compacted_index,
    )
    .await?;

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
            &compacted_ltx,
            &descriptor.info.blake3,
            CellObjectKind::Ltx,
        ),
        upload(
            replica,
            &compacted_index,
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

    let compacted_source = replica.host.filesystem.open(&compacted_index)?;
    let original_source = replica.host.filesystem.open(&original_indexes)?;
    let mut final_inputs = Vec::with_capacity(descriptors.len());
    final_inputs.extend(spooled[..range.start].iter().cloned().map(|mut input| {
        input.source = 1;
        input
    }));
    final_inputs.push(SpoolInput {
        descriptor,
        source: 0,
        start: 0,
        length: artifacts.index.length,
    });
    final_inputs.extend(spooled[range.end..].iter().cloned().map(|mut input| {
        input.source = 1;
        input
    }));
    let entries = MergedEntries::new(vec![compacted_source, original_source], final_inputs)?;
    let endpoint = descriptors.last().ok_or(CrabError::LTXCorrupted)?;
    let page_size = endpoint.info.page_size;
    let database_pages = endpoint.info.database_pages;
    let directory =
        directory::build_initial_and_upload(entries, page_size, database_pages, replica).await?;
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
    source: usize,
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
    sources: Vec<Box<dyn FileIo>>,
    cursors: Vec<SpoolCursor>,
    heap: BinaryHeap<Reverse<(u32, usize)>>,
    valid_through: Vec<u32>,
    failed: bool,
}

impl MergedEntries {
    fn new(mut sources: Vec<Box<dyn FileIo>>, inputs: Vec<SpoolInput>) -> Result<Self> {
        if inputs.is_empty() {
            return Err(CrabError::LTXCorrupted);
        }
        let mut valid_through = vec![0; inputs.len()];
        let mut suffix_min = u32::MAX;
        for (index, input) in inputs.iter().enumerate().rev() {
            suffix_min = suffix_min.min(input.descriptor.info.database_pages);
            valid_through[index] = suffix_min;
        }
        let mut cursors = Vec::with_capacity(inputs.len());
        let mut heap = BinaryHeap::new();
        for input in inputs {
            let cursor = SpoolCursor::new(input, &mut sources)?;
            let index = cursors.len();
            if let Some(entry) = &cursor.current {
                heap.push(Reverse((entry.page, index)));
            }
            cursors.push(cursor);
        }
        Ok(Self {
            sources,
            cursors,
            heap,
            valid_through,
            failed: false,
        })
    }

    fn take_current(&mut self, index: usize) -> Result<DirectoryEntry> {
        let cursor = self.cursors.get_mut(index).ok_or(CrabError::LTXCorrupted)?;
        let entry = cursor.current.take().ok_or(CrabError::LTXCorrupted)?;
        cursor.advance(&mut self.sources)?;
        if let Some(next) = &cursor.current {
            self.heap.push(Reverse((next.page, index)));
        }
        Ok(entry)
    }
}

impl Iterator for MergedEntries {
    type Item = Result<DirectoryEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        loop {
            let Reverse((page, first_index)) = self.heap.pop()?;
            let mut selected = None;
            let mut index = first_index;
            loop {
                let entry = match self.take_current(index) {
                    Ok(entry) => entry,
                    Err(error) => {
                        self.failed = true;
                        self.heap.clear();
                        return Some(Err(error));
                    }
                };
                if page <= self.valid_through[index]
                    && selected
                        .as_ref()
                        .is_none_or(|(selected_index, _)| index > *selected_index)
                {
                    selected = Some((index, entry));
                }
                let Some(Reverse((next_page, next_index))) = self.heap.peek().copied() else {
                    break;
                };
                if next_page != page {
                    break;
                }
                self.heap.pop();
                index = next_index;
            }
            if let Some((_, entry)) = selected {
                return Some(Ok(entry));
            }
        }
    }
}
