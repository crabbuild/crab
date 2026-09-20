use std::io::Cursor;
use std::path::Path;

use crate::{
    CrabError, Limits, LocalSegment, Position, Result, SegmentInfo, ltx, pages::PageChecksums,
};

#[cfg(feature = "replica")]
pub(crate) fn full_job_scratch_bytes(page_size: u32, database_pages: u32) -> Result<u64> {
    const HEADROOM: u64 = 64 << 20;
    u64::from(page_size)
        .checked_mul(u64::from(database_pages))
        .and_then(|bytes| bytes.checked_mul(2))
        .and_then(|bytes| bytes.checked_add(HEADROOM))
        .ok_or(CrabError::Limit("scratch disk bytes"))
}

/// A fully verified, explicit snapshot-plus-deltas plan ending at an exact position.
///
/// Construction reads only named files and owns their bytes, preventing later
/// path replacement from changing the plan. A remote manifest's authenticity,
/// repository identity, epoch, and object selection remain the caller's job.
pub struct VerifiedPlan {
    pub(crate) inputs: Vec<Vec<u8>>,
    pub(crate) infos: Vec<SegmentInfo>,
    image_digest: [u8; 32],
    position: Position,
    limits: Limits,
}

impl VerifiedPlan {
    /// Verifies expected sizes/digests, headers, page ordering, checksums, and continuity.
    ///
    /// The first file must be a full snapshot. Subsequent files must start at
    /// the previous maximum TXID plus one; gaps, overlaps and implicit latest
    /// selection are rejected. Every resulting database state is checksummed.
    pub fn new(segments: &[LocalSegment], target: Position, limits: Limits) -> Result<Self> {
        crate::Host::default().verify(segments, target, limits)
    }

    pub(crate) fn with_host(
        segments: &[LocalSegment],
        target: Position,
        limits: Limits,
        host: &crate::Host,
    ) -> Result<Self> {
        let limits = limits.validate()?;
        if segments.is_empty() {
            return Err(CrabError::TxNotAvailable);
        }
        if segments.len() > limits.max_segments {
            return Err(CrabError::Limit("plan segments"));
        }
        let mut inputs = Vec::new();
        let mut infos = Vec::with_capacity(segments.len());
        let mut total = 0u64;
        for segment in segments {
            total = total
                .checked_add(segment.info().size_bytes)
                .ok_or(CrabError::Limit("plan bytes"))?;
            if total > limits.max_plan_bytes || segment.info().size_bytes > limits.max_file_bytes {
                return Err(CrabError::Limit("plan bytes"));
            }
            let bytes = host.read(segment.path(), segment.info().size_bytes)?;
            verify_segment(&bytes, segment.info(), limits)?;
            infos.push(segment.info().clone());
            inputs.push(bytes);
        }
        let mut plan = Self {
            inputs,
            infos,
            image_digest: [0; 32],
            position: target,
            limits,
        };
        let image = plan.image()?;
        plan.image_digest = *blake3::hash(&image).as_bytes();
        Ok(plan)
    }

    #[must_use]
    pub fn position(&self) -> Position {
        self.position
    }

    pub(crate) fn image(&self) -> Result<Vec<u8>> {
        let mut image = Vec::new();
        let mut checksums = PageChecksums::default();
        let mut position = Position::default();
        let mut page_size = 0;
        for bytes in &self.inputs {
            let (file, pages) = ltx::decode_file_with_pages(bytes)?;
            let header = file.header;
            validate_header(&header, self.limits)?;
            if header.min_txid.0
                != position
                    .txid
                    .checked_add(1)
                    .ok_or(CrabError::TxNotAvailable)?
            {
                return Err(CrabError::TxNotAvailable);
            }
            if header.pre_apply_checksum != position.checksum {
                return Err(CrabError::ChecksumMismatch);
            }
            if page_size != 0 && page_size != header.page_size {
                return Err(CrabError::LTXCorrupted);
            }
            page_size = header.page_size;
            checksums.apply(
                page_size,
                header.commit,
                &pages,
                self.limits.max_database_bytes,
            )?;
            if checksums.checksum() != file.trailer.post_apply_checksum {
                return Err(CrabError::ChecksumMismatch);
            }
            image.resize(header.commit as usize * page_size as usize, 0);
            for (pgno, data) in pages {
                let offset = (pgno as usize - 1) * page_size as usize;
                image[offset..offset + page_size as usize].copy_from_slice(&data);
            }
            position = Position {
                txid: header.max_txid.0,
                checksum: file.trailer.post_apply_checksum,
            };
        }
        if position != self.position {
            return Err(CrabError::ChecksumMismatch);
        }
        Ok(image)
    }
}

fn validate_header(header: &ltx::Header, limits: Limits) -> Result<()> {
    header.validate()?;
    if header.no_checksum() {
        return Err(CrabError::LTXCorrupted);
    }
    if u64::from(header.commit) * u64::from(header.page_size) > limits.max_database_bytes {
        return Err(CrabError::Limit("database bytes"));
    }
    Ok(())
}

pub(crate) fn verify_segment(bytes: &[u8], info: &SegmentInfo, limits: Limits) -> Result<()> {
    if bytes.len() as u64 > limits.max_file_bytes {
        return Err(CrabError::Limit("LTX bytes"));
    }
    if bytes.len() as u64 != info.size_bytes || *blake3::hash(bytes).as_bytes() != info.blake3 {
        return Err(CrabError::ChecksumMismatch);
    }
    validate_header(&ltx::Header::parse(bytes)?, limits)?;
    if SegmentInfo::from_decoded(bytes, &ltx::decode_file(bytes)?) != *info {
        return Err(CrabError::ChecksumMismatch);
    }
    Ok(())
}

/// Restores exactly the verified target into a new, atomically installed SQLite file.
///
/// Never overwrites a destination; no WAL, local database, or bucket listing is
/// consulted. Caller must prevent concurrent use of the destination and sidecars.
pub fn restore_exact(plan: &VerifiedPlan, destination: &Path) -> Result<Position> {
    crate::Host::default().restore(plan, destination)
}

/// Compacts exactly the verified snapshot chain into a new self-contained LTX snapshot.
///
/// Output is checked against the original target before installation. Input
/// deletion, remote publication, retention, and partial-range compaction are not performed.
pub fn compact_exact(plan: &VerifiedPlan, destination: &Path) -> Result<LocalSegment> {
    crate::Host::default().compact(plan, destination)
}

pub(crate) fn compact_bytes(plan: &VerifiedPlan) -> Result<(Vec<u8>, SegmentInfo)> {
    compact_verified(plan)
}

fn compact_verified(plan: &VerifiedPlan) -> Result<(Vec<u8>, SegmentInfo)> {
    // The plan owns bytes that were already fully verified. Re-decoding every
    // input here only repeats work; Compactor validates frames and recomputes
    // the merged snapshot checksum while it selects the newest page.
    let first = plan.infos.first().ok_or(CrabError::TxNotAvailable)?;
    let last = plan.infos.last().ok_or(CrabError::TxNotAvailable)?;
    if plan.inputs.len() != plan.infos.len() || plan.inputs.len() > plan.limits.max_segments {
        return Err(CrabError::Limit("compaction inputs"));
    }
    let readers = plan
        .inputs
        .iter()
        .map(|bytes| Cursor::new(bytes.as_slice()))
        .collect();
    let writer = BoundedWriter {
        bytes: Vec::new(),
        limit: plan.limits.max_file_bytes,
    };
    let mut compactor = crate::compactor::Compactor::new(writer, readers);
    compactor.compact()?;
    let bytes = compactor.into_writer().bytes;
    let (file, pages) = ltx::decode_file_with_pages(&bytes)?;
    let info = SegmentInfo::from_decoded(&bytes, &file);
    let mut image_digest = blake3::Hasher::new();
    let zero_page = vec![0; file.header.page_size as usize];
    let mut page_number = 1;
    for (pgno, data) in pages {
        while page_number < pgno {
            image_digest.update(&zero_page);
            page_number = page_number.checked_add(1).ok_or(CrabError::LTXCorrupted)?;
        }
        image_digest.update(&data);
        page_number = pgno.checked_add(1).ok_or(CrabError::LTXCorrupted)?;
    }
    while page_number <= file.header.commit {
        image_digest.update(&zero_page);
        page_number = page_number.checked_add(1).ok_or(CrabError::LTXCorrupted)?;
    }
    if info.min_txid != first.min_txid
        || info.max_txid != last.max_txid
        || info.pre_checksum != first.pre_checksum
        || info.position() != plan.position
        || info.database_pages != last.database_pages
        || info.page_size != first.page_size
        || *image_digest.finalize().as_bytes() != plan.image_digest
    {
        return Err(CrabError::ChecksumMismatch);
    }
    Ok((bytes, info))
}

pub(crate) fn continuation(plan: &VerifiedPlan) -> Result<(PageChecksums, u32, u32)> {
    let image = plan.image()?;
    let header = ltx::Header::parse(&plan.inputs[0])?;
    let page_size = header.page_size;
    let count = (image.len() / page_size as usize) as u32;
    let pages: Vec<_> = image
        .chunks_exact(page_size as usize)
        .enumerate()
        .filter(|(i, _)| *i as u32 + 1 != ltx::lock_pgno(page_size))
        .map(|(i, data)| (i as u32 + 1, data.to_vec()))
        .collect();
    let mut checksums = PageChecksums::default();
    checksums.apply(page_size, count, &pages, plan.limits.max_database_bytes)?;
    Ok((checksums, page_size, count))
}

struct BoundedWriter {
    bytes: Vec<u8>,
    limit: u64,
}

impl std::io::Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if (self.bytes.len() as u64).saturating_add(bytes.len() as u64) > self.limit {
            return Err(std::io::Error::other("compacted file byte limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn reject_sidecars(path: &Path, host: &crate::Host) -> Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_owned();
        sidecar.push(suffix);
        if host.filesystem.exists(Path::new(&sidecar))? {
            return Err(CrabError::InvalidState(
                "restore destination has SQLite sidecars",
            ));
        }
    }
    Ok(())
}
