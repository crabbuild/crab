use std::io::{Cursor, Write};
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
    inputs: Vec<Vec<u8>>,
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
            let bytes = crate::host::read_bounded(segment.path(), segment.info().size_bytes)?;
            if bytes.len() as u64 != segment.info().size_bytes
                || *blake3::hash(&bytes).as_bytes() != segment.info().blake3
            {
                return Err(CrabError::ChecksumMismatch);
            }
            let header = ltx::Header::parse(&bytes)?;
            validate_header(&header, limits)?;
            let file = ltx::decode_file(&bytes)?;
            if SegmentInfo::from_decoded(&bytes, &file) != *segment.info() {
                return Err(CrabError::ChecksumMismatch);
            }
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

    #[must_use]
    pub fn position(&self) -> Position {
        self.position
    }

    fn image(&self) -> Result<Vec<u8>> {
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

/// Restores exactly the verified target into a new, atomically installed SQLite file.
///
/// Never overwrites a destination; no WAL, local database, or bucket listing is
/// consulted. Caller must prevent concurrent use of the destination and sidecars.
pub fn restore_exact(plan: &VerifiedLocalPlan, destination: &Path) -> Result<Position> {
    reject_sidecars(destination)?;
    persist_new(destination, &plan.image()?)?;
    Ok(plan.position)
}

/// Compacts exactly the verified snapshot chain into a new self-contained LTX snapshot.
///
/// Output is checked against the original target before installation. Input
/// deletion, remote publication, retention, and partial-range compaction are not performed.
pub fn compact_exact(plan: &VerifiedLocalPlan, destination: &Path) -> Result<LocalSegment> {
    let readers = plan
        .inputs
        .iter()
        .map(|bytes| Cursor::new(bytes.as_slice()))
        .collect();
    let mut compactor = crate::compactor::Compactor::new(Vec::new(), readers);
    compactor.compact()?;
    let bytes = compactor.into_writer();
    if bytes.len() as u64 > plan.limits.max_file_bytes {
        return Err(CrabError::Limit("compacted file bytes"));
    }
    let file = ltx::decode_file(&bytes)?;
    let info = SegmentInfo::from_decoded(&bytes, &file);
    let compacted = VerifiedLocalPlan {
        inputs: vec![bytes],
        position: plan.position,
        limits: plan.limits,
    };
    if compacted.image()? != plan.image()? {
        return Err(CrabError::ChecksumMismatch);
    }
    persist_new(destination, &compacted.inputs[0])?;
    Ok(LocalSegment::new(destination.to_owned(), info))
}

fn reject_sidecars(path: &Path) -> Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_owned();
        sidecar.push(suffix);
        match std::fs::symlink_metadata(&sidecar) {
            Ok(_) => {
                return Err(CrabError::InvalidState(
                    "restore destination has SQLite sidecars",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(crate) fn persist_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path)
        .map_err(|error| CrabError::Io(error.error))?;
    crate::host::sync_parent(path)?;
    Ok(())
}
