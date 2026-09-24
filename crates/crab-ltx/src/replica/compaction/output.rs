//! Compacted LTX output, sidecar index, and digests for one compaction pass.

use super::*;

pub(super) async fn write_compacted(
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
    encoder: crate::codec::Encoder<io::BufWriter<DigestWriter>>,
    sidecar: io::BufWriter<DigestWriter>,
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
        let output = DigestWriter::new(output, limits.max_file_bytes);
        let mut encoder = crate::codec::Encoder::new_block_with_index(
            io::BufWriter::with_capacity(64 << 10, output),
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
            sidecar: io::BufWriter::with_capacity(
                64 << 10,
                DigestWriter::new(sidecar, limits.max_plan_bytes),
            ),
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
            write_sidecar_entry(&mut self.sidecar, &encoded)?;
        }
        Ok(self)
    }

    fn finish(mut self, post_checksum: u64) -> Result<CompactedArtifacts> {
        self.encoder.close(post_checksum)?;
        // Flush both streams before DigestWriter checks the exact stored length
        // and syncs; a partial buffered output must never become publishable.
        let ltx = self
            .encoder
            .into_writer()
            .into_inner()
            .map_err(|error| error.into_error())?
            .finish()?;
        let index = self
            .sidecar
            .into_inner()
            .map_err(|error| error.into_error())?
            .finish()?;
        Ok(CompactedArtifacts { ltx, index })
    }
}

fn write_sidecar_entry(
    writer: &mut impl io::Write,
    page: &crate::codec::EncodedPage,
) -> Result<()> {
    writer.write_all(&page.page.to_be_bytes())?;
    writer.write_all(&page.offset.to_be_bytes())?;
    writer.write_all(&page.size.to_be_bytes())?;
    writer.write_all(&page.frame_hash)?;
    writer.write_all(&page.checksum.to_be_bytes())?;
    Ok(())
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
