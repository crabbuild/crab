use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
        .ok_or(CrabError::Limit(crate::LimitKind::ScratchDiskBytes))
}

/// A fully verified, explicit snapshot-plus-deltas plan ending at an exact position.
///
/// Construction reads only named files and owns their exact verified database
/// image, preventing later path replacement from changing the plan. A remote
/// manifest's authenticity, repository identity, epoch, and object selection
/// remain the caller's job.
pub struct VerifiedPlan {
    pub(crate) infos: Vec<SegmentInfo>,
    materialized: MaterializedPlan,
    limits: Limits,
}

pub(crate) struct MaterializedPlan {
    pub(crate) image: Vec<u8>,
    pub(crate) checksums: PageChecksums,
    pub(crate) page_size: u32,
    pub(crate) database_pages: u32,
    pub(crate) position: Position,
    timestamp: i64,
}

#[derive(Default)]
struct MaterializationState {
    image: Vec<u8>,
    page: Vec<u8>,
    checksums: PageChecksums,
    position: Position,
    page_size: u32,
    timestamp: i64,
}

impl MaterializationState {
    fn apply(&mut self, bytes: &[u8], info: &SegmentInfo, limits: Limits) -> Result<()> {
        if bytes.len() as u64 > limits.max_file_bytes {
            return Err(CrabError::Limit(crate::LimitKind::LtxBytes));
        }
        if bytes.len() as u64 != info.size_bytes {
            return Err(CrabError::ChecksumMismatch);
        }
        let mut decoder = crate::codec::Decoder::new(bytes);
        decoder.decode_header()?;
        let header = decoder.header;
        validate_header(&header, limits)?;
        if header.min_txid.0
            != self
                .position
                .txid
                .checked_add(1)
                .ok_or(CrabError::TxNotAvailable)?
        {
            return Err(CrabError::TxNotAvailable);
        }
        if header.pre_apply_checksum != self.position.checksum {
            return Err(CrabError::ChecksumMismatch);
        }
        if self.page_size != 0 && self.page_size != header.page_size {
            return Err(CrabError::LTXCorrupted);
        }
        self.page_size = header.page_size;
        self.timestamp = header.timestamp;
        let image_len = usize::try_from(header.commit)
            .ok()
            .and_then(|pages| pages.checked_mul(header.page_size as usize))
            .ok_or(CrabError::Limit(crate::LimitKind::DatabaseBytes))?;
        self.image.resize(image_len, 0);
        let mut checksums = self.checksums.begin_apply(
            header.page_size,
            header.commit,
            limits.max_database_bytes,
        )?;
        self.page.resize(header.page_size as usize, 0);
        while let Some(page) = decoder.decode_page(&mut self.page)? {
            checksums.page(page.pgno, &self.page)?;
            let offset = (page.pgno as usize - 1)
                .checked_mul(header.page_size as usize)
                .ok_or(CrabError::Limit(crate::LimitKind::DatabaseBytes))?;
            let end = offset
                .checked_add(header.page_size as usize)
                .ok_or(CrabError::Limit(crate::LimitKind::DatabaseBytes))?;
            self.image[offset..end].copy_from_slice(&self.page);
        }
        checksums.finish()?;
        decoder.close()?;
        let (size, digest) = decoder.artifact()?;
        let file = ltx::DecodedFile {
            header,
            trailer: decoder.trailer,
        };
        if self.checksums.checksum() != file.trailer.post_apply_checksum {
            return Err(CrabError::ChecksumMismatch);
        }
        if size != bytes.len() as u64 || SegmentInfo::from_inspected(&file, size, digest) != *info {
            return Err(CrabError::ChecksumMismatch);
        }
        self.position = Position {
            txid: header.max_txid.0,
            checksum: file.trailer.post_apply_checksum,
        };
        Ok(())
    }

    fn finish(self, target: Position) -> Result<MaterializedPlan> {
        if self.position != target || self.page_size == 0 {
            return Err(CrabError::ChecksumMismatch);
        }
        let database_pages = u32::try_from(self.image.len() / self.page_size as usize)
            .map_err(|_| CrabError::Limit(crate::LimitKind::DatabasePages))?;
        Ok(MaterializedPlan {
            image: self.image,
            checksums: self.checksums,
            page_size: self.page_size,
            database_pages,
            position: self.position,
            timestamp: self.timestamp,
        })
    }
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
            return Err(CrabError::Limit(crate::LimitKind::PlanSegments));
        }
        let mut infos = Vec::with_capacity(segments.len());
        let mut materialization = MaterializationState::default();
        let mut total = 0u64;
        for segment in segments {
            total = total
                .checked_add(segment.info().size_bytes)
                .ok_or(CrabError::Limit(crate::LimitKind::PlanBytes))?;
            if total > limits.max_plan_bytes || segment.info().size_bytes > limits.max_file_bytes {
                return Err(CrabError::Limit(crate::LimitKind::PlanBytes));
            }
            let bytes = host.read(segment.path(), segment.info().size_bytes)?;
            materialization.apply(&bytes, segment.info(), limits)?;
            infos.push(segment.info().clone());
        }
        let materialized = materialization.finish(target)?;
        Ok(Self {
            infos,
            materialized,
            limits,
        })
    }

    #[must_use]
    pub fn position(&self) -> Position {
        self.materialized.position
    }

    pub(crate) fn materialize(&self) -> &MaterializedPlan {
        &self.materialized
    }
}

fn validate_header(header: &ltx::Header, limits: Limits) -> Result<()> {
    header.validate()?;
    if header.no_checksum() {
        return Err(CrabError::LTXCorrupted);
    }
    if u64::from(header.commit) * u64::from(header.page_size) > limits.max_database_bytes {
        return Err(CrabError::Limit(crate::LimitKind::DatabaseBytes));
    }
    Ok(())
}

#[cfg_attr(not(feature = "replica"), expect(dead_code))]
pub(crate) fn verify_segment(bytes: &[u8], info: &SegmentInfo, limits: Limits) -> Result<()> {
    if bytes.len() as u64 > limits.max_file_bytes {
        return Err(CrabError::Limit(crate::LimitKind::LtxBytes));
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

pub(crate) fn compact_to_file(
    host: &crate::Host,
    plan: &VerifiedPlan,
    destination: &Path,
) -> Result<LocalSegment> {
    let first = plan.infos.first().ok_or(CrabError::TxNotAvailable)?;
    let last = plan.infos.last().ok_or(CrabError::TxNotAvailable)?;
    if plan.infos.len() > plan.limits.max_segments {
        return Err(CrabError::Limit(crate::LimitKind::CompactionInputs));
    }
    let materialized = plan.materialize();
    if materialized.checksums.checksum() != materialized.position.checksum {
        return Err(CrabError::ChecksumMismatch);
    }
    let (mut scratch, file) = CompactionScratch::create(host, destination)?;
    let writer = BoundedFileWriter {
        file,
        limit: plan.limits.max_file_bytes,
        written: 0,
        digest: blake3::Hasher::new(),
    };
    let mut encoder = crate::codec::Encoder::new_block(BufWriter::with_capacity(1 << 20, writer));
    encoder.encode_header(ltx::Header {
        version: ltx::VERSION,
        page_size: materialized.page_size,
        commit: materialized.database_pages,
        min_txid: crate::Txid(first.min_txid),
        max_txid: crate::Txid(last.max_txid),
        timestamp: materialized.timestamp,
        pre_apply_checksum: first.pre_checksum,
        ..ltx::Header::default()
    })?;
    let page_size = materialized.page_size as usize;
    let lock_page = ltx::lock_pgno(materialized.page_size);
    for page in 1..=materialized.database_pages {
        if page == lock_page {
            continue;
        }
        let start = (page as usize - 1)
            .checked_mul(page_size)
            .ok_or(CrabError::Limit(crate::LimitKind::DatabaseBytes))?;
        let end = start
            .checked_add(page_size)
            .ok_or(CrabError::Limit(crate::LimitKind::DatabaseBytes))?;
        let data = materialized
            .image
            .get(start..end)
            .ok_or(CrabError::LTXCorrupted)?;
        encoder.encode_page(
            ltx::PageHeader {
                pgno: page,
                flags: 0,
            },
            data,
        )?;
    }
    encoder.close(materialized.position.checksum)?;
    let encoded = ltx::DecodedFile {
        header: encoder.header,
        trailer: encoder.trailer,
    };
    let writer = encoder.into_writer();
    let writer = writer.into_inner().map_err(|error| error.into_error())?;
    let (mut file, size, digest) = writer.finish();
    file.sync_all()?;
    drop(file);

    let ltx_host = crate::LtxHost {
        facilities: host.clone(),
        max_database_bytes: plan.limits.max_database_bytes,
        max_file_bytes: plan.limits.max_file_bytes,
    };
    let output = BufReader::with_capacity(1 << 20, ltx_host.open(&scratch.path)?);
    let (stored_size, stored_digest) = digest_reader(output)?;
    if stored_size != size || stored_digest != digest {
        return Err(CrabError::ChecksumMismatch);
    }
    let info = SegmentInfo::from_inspected(&encoded, size, digest);
    if info.min_txid != first.min_txid
        || info.max_txid != last.max_txid
        || info.pre_checksum != first.pre_checksum
        || info.position() != materialized.position
        || info.database_pages != last.database_pages
        || info.page_size != first.page_size
    {
        return Err(CrabError::ChecksumMismatch);
    }
    host.filesystem
        .persist_file_new(&scratch.path, destination)?;
    scratch.installed = true;
    Ok(LocalSegment::new(destination.to_owned(), info))
}

struct BoundedFileWriter {
    file: Box<dyn crate::environment::FileIo>,
    limit: u64,
    written: u64,
    digest: blake3::Hasher,
}

impl Write for BoundedFileWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let end = self
            .written
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| std::io::Error::other("compacted file byte limit exceeded"))?;
        if end > self.limit {
            return Err(std::io::Error::other("compacted file byte limit exceeded"));
        }
        self.file.write_all(bytes)?;
        self.digest.update(bytes);
        self.written = end;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl BoundedFileWriter {
    fn finish(self) -> (Box<dyn crate::environment::FileIo>, u64, [u8; 32]) {
        (self.file, self.written, *self.digest.finalize().as_bytes())
    }
}

fn digest_reader(mut reader: impl Read) -> Result<(u64, [u8; 32])> {
    let mut digest = blake3::Hasher::new();
    let mut size = 0u64;
    let mut buffer = [0; 64 << 10];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .ok_or(CrabError::Limit(crate::LimitKind::LtxBytes))?;
        digest.update(&buffer[..read]);
    }
    Ok((size, *digest.finalize().as_bytes()))
}

struct CompactionScratch {
    filesystem: Arc<dyn crate::environment::FileSystem>,
    path: PathBuf,
    installed: bool,
}

impl CompactionScratch {
    fn create(
        host: &crate::Host,
        destination: &Path,
    ) -> Result<(Self, Box<dyn crate::environment::FileIo>)> {
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let filename = destination
            .file_name()
            .ok_or(CrabError::InvalidState("missing compaction filename"))?;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        for _ in 0..16 {
            let mut scratch_name = filename.to_owned();
            scratch_name.push(format!(
                ".tmp-crab-ltx-compaction-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let path = parent.join(scratch_name);
            match host.filesystem.create(&path) {
                Ok(file) => {
                    return Ok((
                        Self {
                            filesystem: host.filesystem.clone(),
                            path,
                            installed: false,
                        },
                        file,
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "compaction scratch namespace exhausted",
        )
        .into())
    }
}

impl Drop for CompactionScratch {
    fn drop(&mut self) {
        if !self.installed {
            let _ = self.filesystem.remove_file(&self.path);
        }
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
