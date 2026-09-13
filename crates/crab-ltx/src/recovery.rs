use std::io::Cursor;
use std::path::Path;

use crate::{
    CrabError, Limits, LocalSegment, Position, Result, SegmentInfo, ltx, pages::PageChecksums,
};

/// A fully verified, explicit snapshot-plus-deltas plan ending at an exact position.
///
/// Construction reads only named files and owns their bytes, preventing later
/// path replacement from changing the plan. A remote manifest's authenticity,
/// repository identity, epoch, and object selection remain the caller's job.
pub struct VerifiedLocalPlan {
    pub(crate) inputs: Vec<Vec<u8>>,
    position: Position,
    limits: Limits,
}

impl VerifiedLocalPlan {
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
            inputs.push(bytes);
        }
        let plan = Self {
            inputs,
            position: target,
            limits,
        };
        plan.image()?;
        Ok(plan)
    }

    #[cfg(feature = "replica")]
    pub(crate) fn from_bytes(
        inputs: Vec<Vec<u8>>,
        infos: &[SegmentInfo],
        target: Position,
        limits: Limits,
    ) -> Result<Self> {
        let limits = limits.validate()?;
        if inputs.is_empty() || inputs.len() != infos.len() || inputs.len() > limits.max_segments {
            return Err(CrabError::Limit("plan segments"));
        }
        let mut total = 0u64;
        for (bytes, info) in inputs.iter().zip(infos) {
            total = total
                .checked_add(bytes.len() as u64)
                .ok_or(CrabError::Limit("plan bytes"))?;
            if total > limits.max_plan_bytes {
                return Err(CrabError::Limit("plan bytes"));
            }
            verify_segment(bytes, info, limits)?;
        }
        let plan = Self {
            inputs,
            position: target,
            limits,
        };
        plan.image()?;
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
pub fn restore_exact(plan: &VerifiedLocalPlan, destination: &Path) -> Result<Position> {
    crate::Host::default().restore(plan, destination)
}

/// Compacts exactly the verified snapshot chain into a new self-contained LTX snapshot.
///
/// Output is checked against the original target before installation. Input
/// deletion, remote publication, retention, and partial-range compaction are not performed.
pub fn compact_exact(plan: &VerifiedLocalPlan, destination: &Path) -> Result<LocalSegment> {
    crate::Host::default().compact(plan, destination)
}

pub(crate) fn compact_bytes(plan: &VerifiedLocalPlan) -> Result<(Vec<u8>, SegmentInfo)> {
    let infos = plan
        .inputs
        .iter()
        .map(|bytes| ltx::decode_file(bytes).map(|file| SegmentInfo::from_decoded(bytes, &file)))
        .collect::<Result<Vec<_>>>()?;
    compact_inputs(&plan.inputs, &infos, plan.limits)
}

pub(crate) fn continuation(plan: &VerifiedLocalPlan) -> Result<(PageChecksums, u32, u32)> {
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

// Callers prove predecessor/intermediate database states using either an owned
// verified plan or authenticated indexes. This proves the merge's page bytes
// independently, without materializing unrelated prefix/suffix database images.
pub(crate) fn compact_inputs(
    inputs: &[Vec<u8>],
    infos: &[SegmentInfo],
    limits: Limits,
) -> Result<(Vec<u8>, SegmentInfo)> {
    let first = infos.first().ok_or(CrabError::TxNotAvailable)?;
    let last = infos.last().ok_or(CrabError::TxNotAvailable)?;
    if inputs.len() != infos.len() || inputs.len() > limits.max_segments {
        return Err(CrabError::Limit("compaction inputs"));
    }
    let mut expected = std::collections::BTreeMap::new();
    let mut previous = None;
    let mut total = 0u64;
    for (bytes, info) in inputs.iter().zip(infos) {
        total = total
            .checked_add(bytes.len() as u64)
            .ok_or(CrabError::Limit("plan bytes"))?;
        if total > limits.max_plan_bytes {
            return Err(CrabError::Limit("plan bytes"));
        }
        verify_segment(bytes, info, limits)?;
        if info.page_size != first.page_size
            || previous.is_some_and(|position: Position| {
                position.txid.checked_add(1) != Some(info.min_txid)
                    || position.checksum != info.pre_checksum
            })
        {
            return Err(CrabError::LTXCorrupted);
        }
        expected.retain(|page, _| *page <= info.database_pages);
        expected.extend(ltx::decode_file_with_pages(bytes)?.1);
        previous = Some(info.position());
    }
    let readers = inputs
        .iter()
        .map(|bytes| Cursor::new(bytes.as_slice()))
        .collect();
    let writer = BoundedWriter {
        bytes: Vec::new(),
        limit: limits.max_file_bytes,
    };
    let mut compactor = crate::compactor::Compactor::new(writer, readers);
    compactor.compact()?;
    let bytes = compactor.into_writer().bytes;
    let (file, pages) = ltx::decode_file_with_pages(&bytes)?;
    let info = SegmentInfo::from_decoded(&bytes, &file);
    if info.min_txid != first.min_txid
        || info.pre_checksum != first.pre_checksum
        || info.position() != last.position()
        || info.database_pages != last.database_pages
        || info.page_size != first.page_size
        || !pages
            .iter()
            .map(|(page, data)| (*page, data.as_slice()))
            .eq(expected.iter().map(|(page, data)| (*page, data.as_slice())))
    {
        return Err(CrabError::ChecksumMismatch);
    }
    Ok((bytes, info))
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
