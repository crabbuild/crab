use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    io::{self, Write as _},
    ops::Range,
    path::Path,
};

use super::{
    CellReplica, DirectoryEntry, LoadedGraph, PreparedRoot, RootRef, SegmentDescriptor, directory,
};
use crate::{CellObjectKind, CrabError, Result, SegmentInfo, Txid, environment::FileIo};

pub(super) mod scratch;
use scratch::ScratchFiles;
pub(super) use scratch::{upload, upload_source};

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
    let (spooled, body_inputs) = futures_util::future::try_join(
        spool_indexes(replica, &graph.descriptors, &original_indexes),
        spool_selected_bodies(replica, selected, &original_bodies),
    )
    .await?;
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
    futures_util::future::try_join(
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
    .await?;

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

async fn spool_selected_bodies(
    replica: &CellReplica,
    descriptors: &[SegmentDescriptor],
    destination: &Path,
) -> Result<Vec<BodySpoolInput>> {
    let mut file = replica.host.filesystem.open_rw(destination)?;
    let mut spooled = Vec::with_capacity(descriptors.len());
    let mut destination_offset = 0_u64;
    for descriptor in descriptors {
        let start = descriptor.offset();
        let end = start
            .checked_add(descriptor.info.size_bytes)
            .ok_or(CrabError::LTXCorrupted)?;
        let path = replica.layout.incarnation_object_path(
            &replica.cell,
            &replica.incarnation,
            &descriptor.object_digest(),
            descriptor.object_kind(),
        );
        let mut offset = start;
        let mut hasher = blake3::Hasher::new();
        while offset < end {
            let next = offset
                .checked_add((end - offset).min(FRAME_READ_BYTES))
                .ok_or(CrabError::LTXCorrupted)?;
            let _permit = replica.host.io_permit().await?;
            let bytes = replica
                .layout
                .store()
                .range_get(&path, offset..next)
                .await?;
            drop(_permit);
            if bytes.len() as u64 != next - offset {
                return Err(CrabError::ChecksumMismatch);
            }
            hasher.update(&bytes);
            file = replica
                .host
                .run(move || {
                    file.write_all(&bytes)?;
                    Ok::<_, CrabError>(file)
                })
                .await??;
            offset = next;
        }
        if *hasher.finalize().as_bytes() != descriptor.info.blake3 {
            return Err(CrabError::ChecksumMismatch);
        }
        spooled.push(BodySpoolInput {
            descriptor: descriptor.clone(),
            start: destination_offset,
        });
        destination_offset = destination_offset
            .checked_add(descriptor.info.size_bytes)
            .ok_or(CrabError::Limit("compaction body spool"))?;
    }
    replica
        .host
        .run(move || {
            if file.file_len()? != destination_offset {
                return Err(CrabError::LTXCorrupted);
            }
            file.sync_all()?;
            Ok::<_, CrabError>(())
        })
        .await??;
    Ok(spooled)
}

async fn spool_indexes(
    replica: &CellReplica,
    descriptors: &[SegmentDescriptor],
    destination: &Path,
) -> Result<Vec<SpoolInput>> {
    let mut file = replica.host.filesystem.open_rw(destination)?;
    let mut inputs = Vec::with_capacity(descriptors.len());
    let mut destination_offset = 0_u64;
    for descriptor in descriptors {
        if descriptor.index_length == 0
            || descriptor.index_length % crate::paged::ENTRY_BYTES as u64 != 0
        {
            return Err(CrabError::LTXCorrupted);
        }
        let path = replica.layout.incarnation_object_path(
            &replica.cell,
            &replica.incarnation,
            &descriptor.index_digest,
            CellObjectKind::Index,
        );
        let mut source_offset = 0_u64;
        let mut hasher = blake3::Hasher::new();
        let mut validator = crate::paged::IndexValidator::new(&descriptor.info);
        while source_offset < descriptor.index_length {
            let length = (descriptor.index_length - source_offset).min(INDEX_READ_BYTES);
            let _permit = replica.host.io_permit().await?;
            let bytes = replica
                .layout
                .store()
                .range_get(&path, source_offset..source_offset + length)
                .await?;
            drop(_permit);
            if bytes.len() as u64 != length {
                return Err(CrabError::ChecksumMismatch);
            }
            hasher.update(&bytes);
            let returned = replica
                .host
                .run(move || {
                    for entry in bytes.as_chunks::<{ crate::paged::ENTRY_BYTES }>().0 {
                        validator.validate(crate::paged::decode_index_entry(entry)?)?;
                    }
                    file.write_all(&bytes)?;
                    Ok::<_, CrabError>((file, validator))
                })
                .await??;
            file = returned.0;
            validator = returned.1;
            source_offset += length;
        }
        if *hasher.finalize().as_bytes() != descriptor.index_digest {
            return Err(CrabError::ChecksumMismatch);
        }
        inputs.push(SpoolInput {
            descriptor: descriptor.clone(),
            source: 0,
            start: destination_offset,
            length: descriptor.index_length,
        });
        destination_offset = destination_offset
            .checked_add(descriptor.index_length)
            .ok_or(CrabError::Limit("compaction index spool"))?;
    }
    replica
        .host
        .run(move || {
            if file.file_len()? != destination_offset {
                return Err(CrabError::LTXCorrupted);
            }
            file.sync_all()?;
            Ok::<_, CrabError>(())
        })
        .await??;
    Ok(inputs)
}

async fn write_compacted(
    replica: &CellReplica,
    inputs: &[SpoolInput],
    spool_path: &Path,
    body_path: &Path,
    body_inputs: &[BodySpoolInput],
    ltx_path: &Path,
    codec_index_path: &Path,
    index_path: &Path,
) -> Result<CompactedArtifacts> {
    let source = replica.host.filesystem.open(spool_path)?;
    let mut body_source = replica.host.filesystem.open(body_path)?;
    let mut entries = MergedEntries::new(vec![source], inputs.to_vec())?;
    let output_file = replica.host.filesystem.open_rw(ltx_path)?;
    let codec_index_file = replica.host.filesystem.open_rw(codec_index_path)?;
    let sidecar_file = replica.host.filesystem.open_rw(index_path)?;
    let first = inputs.first().ok_or(CrabError::TxNotAvailable)?;
    let last = inputs.last().ok_or(CrabError::TxNotAvailable)?;
    let mut state = OutputState::new(
        output_file,
        codec_index_file,
        sidecar_file,
        replica.limits,
        &first.descriptor.info,
        &last.descriptor.info,
    )?;
    let mut next = entries.next().transpose()?;
    while let Some(first_entry) = next.take() {
        let mut batch = vec![first_entry];
        while let Some(entry) = entries.next().transpose()? {
            let previous = batch.last().ok_or(CrabError::LTXCorrupted)?;
            let encoded = batch.iter().try_fold(0_u64, |total, entry| {
                total
                    .checked_add(u64::from(entry.length))
                    .ok_or(CrabError::LTXCorrupted)
            })?;
            if entry.object != previous.object
                || entry.offset != previous.offset + u64::from(previous.length)
                || encoded + u64::from(entry.length) > FRAME_READ_BYTES
                || batch.len() as u64 * u64::from(first.descriptor.info.page_size)
                    >= FRAME_READ_BYTES
            {
                next = Some(entry);
                break;
            }
            batch.push(entry);
        }
        let range = body_range(&batch, body_inputs)?;
        let page_size = first.descriptor.info.page_size;
        let returned = replica
            .host
            .run(move || {
                let frames = body_source.read_exact_at(range.start, range.length)?;
                let pages = decode_pages(&batch, &frames, page_size)?;
                Ok::<_, CrabError>((body_source, pages))
            })
            .await??;
        body_source = returned.0;
        let pages = returned.1;
        state = replica.host.run(move || state.encode(pages)).await??;
    }
    let post_checksum = last.descriptor.info.post_checksum;
    replica
        .host
        .run(move || state.finish(post_checksum))
        .await?
}

fn body_range(entries: &[DirectoryEntry], inputs: &[BodySpoolInput]) -> Result<LocalBodyRange> {
    let first = entries.first().ok_or(CrabError::LTXCorrupted)?;
    let last = entries.last().ok_or(CrabError::LTXCorrupted)?;
    let end = last
        .offset
        .checked_add(u64::from(last.length))
        .ok_or(CrabError::LTXCorrupted)?;
    if entries.iter().any(|entry| entry.object != first.object) {
        return Err(CrabError::LTXCorrupted);
    }
    let input = inputs
        .iter()
        .find(|input| {
            input.descriptor.object_digest() == first.object
                && input.descriptor.offset() <= first.offset
                && input
                    .descriptor
                    .offset()
                    .checked_add(input.descriptor.info.size_bytes)
                    .is_some_and(|input_end| end <= input_end)
        })
        .ok_or(CrabError::LTXCorrupted)?;
    let start = input
        .start
        .checked_add(first.offset - input.descriptor.offset())
        .ok_or(CrabError::LTXCorrupted)?;
    let length = usize::try_from(end - first.offset).map_err(|_| CrabError::LTXCorrupted)?;
    Ok(LocalBodyRange { start, length })
}

fn decode_pages(
    entries: &[DirectoryEntry],
    frames: &[u8],
    page_size: u32,
) -> Result<Vec<(u32, Vec<u8>)>> {
    let first = entries.first().ok_or(CrabError::LTXCorrupted)?;
    let mut pages = Vec::with_capacity(entries.len());
    for entry in entries {
        let start =
            usize::try_from(entry.offset - first.offset).map_err(|_| CrabError::LTXCorrupted)?;
        let frame = frames
            .get(start..start + entry.length as usize)
            .ok_or(CrabError::LTXCorrupted)?;
        if *blake3::hash(frame).as_bytes() != entry.frame_hash {
            return Err(CrabError::ChecksumMismatch);
        }
        let bytes = crate::paged::decode_frame(frame, page_size, entry.page)?;
        if crate::ltx::checksum_page(entry.page, &bytes) != entry.checksum {
            return Err(CrabError::ChecksumMismatch);
        }
        pages.push((entry.page, bytes));
    }
    Ok(pages)
}

struct OutputState {
    encoder: crate::codec::Encoder<DigestWriter>,
    sidecar: DigestWriter,
}

impl OutputState {
    fn new(
        output: Box<dyn FileIo>,
        codec_index: Box<dyn FileIo>,
        sidecar: Box<dyn FileIo>,
        limits: crate::Limits,
        first: &SegmentInfo,
        last: &SegmentInfo,
    ) -> Result<Self> {
        if first.page_size != last.page_size {
            return Err(CrabError::LTXCorrupted);
        }
        let mut encoder = crate::codec::Encoder::new_block_with_index(
            DigestWriter::new(output, limits.max_file_bytes),
            Some(codec_index),
        );
        encoder.encode_header(crate::ltx::Header {
            version: crate::ltx::VERSION,
            flags: 0,
            page_size: first.page_size,
            commit: last.database_pages,
            min_txid: Txid(first.min_txid),
            max_txid: Txid(last.max_txid),
            timestamp: 0,
            pre_apply_checksum: first.pre_checksum,
            ..crate::ltx::Header::default()
        })?;
        Ok(Self {
            encoder,
            sidecar: DigestWriter::new(sidecar, limits.max_plan_bytes),
        })
    }

    fn encode(mut self, pages: Vec<(u32, Vec<u8>)>) -> Result<Self> {
        for (page, bytes) in pages {
            let encoded = self.encoder.encode_page(
                crate::ltx::PageHeader {
                    pgno: page,
                    flags: 0,
                },
                &bytes,
            )?;
            self.sidecar.write_index(&encoded)?;
        }
        Ok(self)
    }

    fn finish(mut self, post_checksum: u64) -> Result<CompactedArtifacts> {
        self.encoder.close(post_checksum)?;
        Ok(CompactedArtifacts {
            ltx: self.encoder.into_writer().finish()?,
            index: self.sidecar.finish()?,
        })
    }
}

struct DigestWriter {
    file: Box<dyn FileIo>,
    hasher: blake3::Hasher,
    length: u64,
    limit: u64,
}

impl DigestWriter {
    fn new(file: Box<dyn FileIo>, limit: u64) -> Self {
        Self {
            file,
            hasher: blake3::Hasher::new(),
            length: 0,
            limit,
        }
    }

    fn write_index(&mut self, page: &crate::codec::EncodedPage) -> Result<()> {
        self.write_all(&page.page.to_be_bytes())?;
        self.write_all(&page.offset.to_be_bytes())?;
        self.write_all(&page.size.to_be_bytes())?;
        self.write_all(&page.frame_hash)?;
        self.write_all(&page.checksum.to_be_bytes())?;
        Ok(())
    }

    fn finish(mut self) -> Result<Artifact> {
        if self.file.file_len()? != self.length {
            return Err(CrabError::LTXCorrupted);
        }
        self.file.sync_all()?;
        Ok(Artifact {
            digest: *self.hasher.finalize().as_bytes(),
            length: self.length,
        })
    }
}

impl io::Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self
            .length
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::other("compaction output length overflow"))?;
        if next > self.limit {
            return Err(io::Error::other("compaction output limit exceeded"));
        }
        self.file.write_all(bytes)?;
        self.hasher.update(bytes);
        self.length = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
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

struct SpoolCursor {
    input: SpoolInput,
    offset: u64,
    current: Option<DirectoryEntry>,
}

impl SpoolCursor {
    fn new(input: SpoolInput, sources: &mut [Box<dyn FileIo>]) -> Result<Self> {
        let mut cursor = Self {
            offset: input.start,
            input,
            current: None,
        };
        cursor.advance(sources)?;
        Ok(cursor)
    }

    fn advance(&mut self, sources: &mut [Box<dyn FileIo>]) -> Result<()> {
        let end = self
            .input
            .start
            .checked_add(self.input.length)
            .ok_or(CrabError::LTXCorrupted)?;
        if self.offset == end {
            self.current = None;
            return Ok(());
        }
        if self.offset > end || end - self.offset < crate::paged::ENTRY_BYTES as u64 {
            return Err(CrabError::LTXCorrupted);
        }
        let source = sources
            .get_mut(self.input.source)
            .ok_or(CrabError::LTXCorrupted)?;
        let bytes = source.read_exact_at(self.offset, crate::paged::ENTRY_BYTES)?;
        let entry = crate::paged::decode_index_entry(&bytes)?;
        let descriptor = &self.input.descriptor;
        self.current = Some(DirectoryEntry {
            page: entry.page,
            object: descriptor.object_digest(),
            offset: descriptor
                .offset()
                .checked_add(entry.offset)
                .ok_or(CrabError::LTXCorrupted)?,
            length: u32::try_from(entry.size).map_err(|_| CrabError::LTXCorrupted)?,
            frame_hash: entry.hash,
            checksum: entry.checksum,
        });
        self.offset += crate::paged::ENTRY_BYTES as u64;
        Ok(())
    }
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
