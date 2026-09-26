use std::{
    collections::HashMap,
    io::{BufReader, Read as _},
    sync::Arc,
};

use crate::{CHECKSUM_FLAG, CrabError, Result, ltx};

#[cfg(feature = "replica")]
const CHECKSUM_READ_BYTES: usize = 64 * 1024;
/// Buffered page-checksum writes keep a dense copy off the syscall path.
const DENSE_WRITE_BYTES: usize = 64 * 1024;

#[derive(Clone)]
enum ChecksumBase {
    Memory(Arc<[u64]>),
    #[cfg(feature = "replica")]
    File(Arc<FileChecksumBase>),
}

#[cfg(feature = "replica")]
#[derive(Clone)]
struct FileChecksumBase {
    host: crate::Host,
    path: std::path::PathBuf,
    limit: u64,
}

#[cfg(feature = "replica")]
impl FileChecksumBase {
    fn open(&self) -> Result<crate::HostFile> {
        crate::LtxHost {
            facilities: self.host.clone(),
            max_database_bytes: self.limit,
            max_file_bytes: self.limit,
        }
        .open_rw(&self.path)
        .map_err(Into::into)
    }
}

/// Transactional page-checksum view used by WAL capture.
///
/// Clones retain an immutable base and copy only the uncommitted overlay. Cell
/// activations use a local fixed-width file as that base, keeping resident
/// memory proportional to pages changed by the current cut.
#[derive(Clone)]
pub(crate) struct PageChecksums {
    base: ChecksumBase,
    base_count: u32,
    count: u32,
    changes: HashMap<u32, u64>,
    checksum: u64,
}

pub(crate) struct PageChecksumApply<'a> {
    target: &'a mut PageChecksums,
    page_size: u32,
    commit: u32,
    previous_count: u32,
    previous: u32,
    next_required: u32,
    base_file: Option<crate::HostFile>,
}

impl Default for PageChecksums {
    fn default() -> Self {
        Self {
            base: ChecksumBase::Memory(Arc::from([])),
            base_count: 0,
            count: 0,
            changes: HashMap::new(),
            checksum: CHECKSUM_FLAG,
        }
    }
}

impl PageChecksums {
    #[cfg(feature = "replica")]
    pub(crate) fn from_file(
        host: crate::LtxHost,
        path: &std::path::Path,
        page_size: u32,
        count: u32,
        checksum: u64,
    ) -> Result<Self> {
        if !ltx::is_valid_page_size(page_size) || count == 0 || checksum & CHECKSUM_FLAG == 0 {
            return Err(CrabError::LTXCorrupted);
        }
        if host.metadata(path)?.len != u64::from(count) * 8 {
            return Err(CrabError::LTXCorrupted);
        }
        Ok(Self {
            base: ChecksumBase::File(Arc::new(FileChecksumBase {
                host: host.facilities,
                path: path.to_owned(),
                limit: host.max_file_bytes,
            })),
            base_count: count,
            count,
            changes: HashMap::new(),
            checksum,
        })
    }

    #[cfg_attr(any(not(test), all(test, not(feature = "replica"))), expect(dead_code))]
    pub fn apply(
        &mut self,
        page_size: u32,
        commit: u32,
        pages: &[(u32, Vec<u8>)],
        limit: u64,
    ) -> Result<()> {
        self.apply_iter(page_size, commit, pages.iter().cloned().map(Ok), limit)
    }

    pub(crate) fn apply_iter(
        &mut self,
        page_size: u32,
        commit: u32,
        pages: impl Iterator<Item = Result<(u32, Vec<u8>)>>,
        limit: u64,
    ) -> Result<()> {
        let mut apply = self.begin_apply(page_size, commit, limit)?;
        for page in pages {
            let (pgno, data) = page?;
            apply.page(pgno, &data)?;
        }
        apply.finish()
    }

    pub(crate) fn begin_apply(
        &mut self,
        page_size: u32,
        commit: u32,
        limit: u64,
    ) -> Result<PageChecksumApply<'_>> {
        if !ltx::is_valid_page_size(page_size) || commit == 0 {
            return Err(CrabError::LTXCorrupted);
        }
        if u64::from(page_size) * u64::from(commit) > limit {
            return Err(CrabError::Limit(crate::LimitKind::DatabaseBytes));
        }

        let previous_count = self.count;
        let mut base_file = {
            #[cfg(feature = "replica")]
            {
                match &self.base {
                    ChecksumBase::File(base) => Some(base.open()?),
                    ChecksumBase::Memory(_) => None,
                }
            }
            #[cfg(not(feature = "replica"))]
            {
                None
            }
        };
        if commit < previous_count {
            #[cfg(feature = "replica")]
            if matches!(self.base, ChecksumBase::File(_)) {
                self.remove_file_suffix(commit, previous_count, base_file.as_mut())?;
            } else {
                for page in commit + 1..=previous_count {
                    let old = self.value(page, base_file.as_mut())?;
                    self.checksum = CHECKSUM_FLAG | (self.checksum ^ old);
                }
            }
            #[cfg(not(feature = "replica"))]
            for page in commit + 1..=previous_count {
                let old = self.value(page, base_file.as_mut())?;
                self.checksum = CHECKSUM_FLAG | (self.checksum ^ old);
            }
            self.changes.retain(|page, _| *page <= commit);
        }

        Ok(PageChecksumApply {
            target: self,
            page_size,
            commit,
            previous_count,
            previous: 0,
            next_required: previous_count.saturating_add(1),
            base_file,
        })
    }

    /// Persists a successful candidate after its LTX cut is sealed.
    pub(crate) fn persist(&mut self) -> Result<()> {
        match self.base.clone() {
            ChecksumBase::Memory(base) => {
                let mut dense = vec![0; self.count as usize];
                let retained = dense.len().min(base.len()).min(self.base_count as usize);
                dense[..retained].copy_from_slice(&base[..retained]);
                for (&page, &checksum) in &self.changes {
                    if page <= self.count {
                        dense[page as usize - 1] = checksum;
                    }
                }
                self.base = ChecksumBase::Memory(Arc::from(dense));
                self.base_count = self.count;
                self.changes.clear();
            }
            #[cfg(feature = "replica")]
            ChecksumBase::File(base) => {
                let mut file = base.open()?;
                let length = u64::from(self.count) * 8;
                if length > u64::from(self.base_count) * 8 {
                    file.set_len(length)?;
                }
                let mut changes = self
                    .changes
                    .iter()
                    .filter(|(page, _)| **page <= self.count)
                    .collect::<Vec<_>>();
                changes.sort_unstable_by_key(|(page, _)| **page);
                let mut output = Vec::with_capacity(changes.len().min(DENSE_WRITE_BYTES / 8) * 8);
                let mut start = 0;
                for (&page, &checksum) in changes {
                    let offset = u64::from(page - 1) * 8;
                    if !output.is_empty()
                        && (offset != start + output.len() as u64
                            || output.len() == DENSE_WRITE_BYTES)
                    {
                        file.write_all_at(start, &output)?;
                        output.clear();
                    }
                    if output.is_empty() {
                        start = offset;
                    }
                    output.extend_from_slice(&checksum.to_be_bytes());
                }
                if !output.is_empty() {
                    file.write_all_at(start, &output)?;
                }
                file.set_len(length)?;
                // This base is active-session scratch. A clean handoff writes
                // and syncs a fresh dense sidecar; a crash cannot reopen this
                // session directory or use its mutable base as authority.
                self.base_count = self.count;
                self.changes.clear();
            }
        }
        Ok(())
    }

    pub fn checksum(&self) -> u64 {
        self.checksum
    }

    /// Returns the database page count this index describes.
    pub(crate) fn count(&self) -> u32 {
        self.count
    }

    /// Verifies the checksum-bearing pages of a clean local database.
    #[cfg(feature = "replica")]
    pub(crate) fn verify_database(
        &self,
        host: &crate::LtxHost,
        path: &std::path::Path,
        page_size: u32,
    ) -> Result<()> {
        let mut database = host.open(path)?;
        let mut base_file = match &self.base {
            ChecksumBase::File(base) => base.open()?,
            ChecksumBase::Memory(_) => {
                return Err(CrabError::InvalidState(
                    "resume requires a file-backed checksum index",
                ));
            }
        };
        let mut fold = CHECKSUM_FLAG;
        let pages_per_read = (CHECKSUM_READ_BYTES / page_size as usize).max(1) as u64;
        let mut first = 1u64;
        while first <= u64::from(self.count) {
            let pages = (u64::from(self.count) - first + 1).min(pages_per_read);
            let checksums = base_file.read_exact_at((first - 1) * 8, pages as usize * 8)?;
            let image = database.read_exact_at(
                (first - 1) * u64::from(page_size),
                pages as usize * page_size as usize,
            )?;
            for (index, (stored, bytes)) in checksums
                .chunks_exact(8)
                .zip(image.chunks_exact(page_size as usize))
                .enumerate()
            {
                let page = (first + index as u64) as u32;
                let expected =
                    u64::from_be_bytes(stored.try_into().map_err(|_| CrabError::LTXCorrupted)?);
                if page == ltx::lock_pgno(page_size) {
                    if expected != 0 {
                        return Err(CrabError::ChecksumMismatch);
                    }
                    continue;
                }
                if expected != ltx::checksum_page(page, bytes) {
                    return Err(CrabError::ChecksumMismatch);
                }
                fold = CHECKSUM_FLAG | (fold ^ expected);
            }
            first += pages;
        }
        if fold != self.checksum {
            return Err(CrabError::ChecksumMismatch);
        }
        Ok(())
    }

    /// Writes the dense per-page checksum list, proving it folds to this index.
    ///
    /// The fold is the same aggregate the capture maintains, so a base file that
    /// no longer matches it is refused instead of copied into a continuation
    /// that a later open would trust.
    pub(crate) fn write_dense(&self, sink: &mut dyn crate::environment::FileIo) -> Result<()> {
        let mut base_file: Option<BufReader<crate::HostFile>> = {
            #[cfg(feature = "replica")]
            {
                match &self.base {
                    ChecksumBase::File(base) => {
                        Some(BufReader::with_capacity(DENSE_WRITE_BYTES, base.open()?))
                    }
                    ChecksumBase::Memory(_) => None,
                }
            }
            #[cfg(not(feature = "replica"))]
            {
                None
            }
        };
        let mut fold = CHECKSUM_FLAG;
        let mut output = Vec::with_capacity(DENSE_WRITE_BYTES);
        for page in 1..=self.count {
            let checksum = if page <= self.base_count
                && let Some(file) = base_file.as_mut()
            {
                // Consume the base even when an overlay replaces this entry,
                // so later pages retain their exact position in the sidecar.
                let mut bytes = [0; 8];
                file.read_exact(&mut bytes)?;
                let checksum = self
                    .changes
                    .get(&page)
                    .copied()
                    .unwrap_or(u64::from_be_bytes(bytes));
                if checksum != 0 && checksum & CHECKSUM_FLAG == 0 {
                    return Err(CrabError::LTXCorrupted);
                }
                checksum
            } else {
                self.value(page, None)?
            };
            fold = CHECKSUM_FLAG | (fold ^ checksum);
            output.extend_from_slice(&checksum.to_be_bytes());
            if output.len() >= DENSE_WRITE_BYTES {
                sink.write_all(&output)?;
                output.clear();
            }
        }
        if !output.is_empty() {
            sink.write_all(&output)?;
        }
        if fold != self.checksum {
            return Err(CrabError::ChecksumMismatch);
        }
        Ok(())
    }

    #[cfg(feature = "replica")]
    fn remove_file_suffix(
        &mut self,
        commit: u32,
        previous_count: u32,
        file: Option<&mut crate::HostFile>,
    ) -> Result<()> {
        let file = file.ok_or(CrabError::InvalidState("checksum file not open"))?;
        let mut page = commit.checked_add(1).ok_or(CrabError::LTXCorrupted)?;
        let file_end = previous_count.min(self.base_count);
        while page <= file_end {
            let count = usize::try_from(file_end - page + 1)
                .map_err(|_| CrabError::LTXCorrupted)?
                .min(CHECKSUM_READ_BYTES / 8);
            let bytes = file.read_exact_at(u64::from(page - 1) * 8, count * 8)?;
            for checksum in bytes.as_chunks::<8>().0 {
                let stored = u64::from_be_bytes(*checksum);
                let old = self.changes.get(&page).copied().unwrap_or(stored);
                if old != 0 && old & CHECKSUM_FLAG == 0 {
                    return Err(CrabError::LTXCorrupted);
                }
                self.checksum = CHECKSUM_FLAG | (self.checksum ^ old);
                page = page.checked_add(1).ok_or(CrabError::LTXCorrupted)?;
            }
        }
        while page <= previous_count {
            let old = self.changes.get(&page).copied().unwrap_or(0);
            if old != 0 && old & CHECKSUM_FLAG == 0 {
                return Err(CrabError::LTXCorrupted);
            }
            self.checksum = CHECKSUM_FLAG | (self.checksum ^ old);
            page = page.checked_add(1).ok_or(CrabError::LTXCorrupted)?;
        }
        Ok(())
    }

    fn value(&self, page: u32, _file: Option<&mut crate::HostFile>) -> Result<u64> {
        if page == 0 || page > self.count {
            return Err(CrabError::LTXCorrupted);
        }
        if let Some(checksum) = self.changes.get(&page) {
            return Ok(*checksum);
        }
        if page > self.base_count {
            return Ok(0);
        }
        let checksum = match &self.base {
            ChecksumBase::Memory(pages) => *pages
                .get(page as usize - 1)
                .ok_or(CrabError::LTXCorrupted)?,
            #[cfg(feature = "replica")]
            ChecksumBase::File(_) => {
                let file = _file.ok_or(CrabError::InvalidState("checksum file not open"))?;
                let bytes = file.read_exact_at(u64::from(page - 1) * 8, 8)?;
                u64::from_be_bytes(bytes.try_into().map_err(|_| CrabError::LTXCorrupted)?)
            }
        };
        if checksum != 0 && checksum & CHECKSUM_FLAG == 0 {
            return Err(CrabError::LTXCorrupted);
        }
        Ok(checksum)
    }
}

impl PageChecksumApply<'_> {
    pub(crate) fn page(&mut self, pgno: u32, data: &[u8]) -> Result<()> {
        let lock = ltx::lock_pgno(self.page_size);
        if pgno <= self.previous
            || pgno > self.commit
            || pgno == lock
            || data.len() != self.page_size as usize
        {
            return Err(CrabError::LTXCorrupted);
        }
        while self.next_required == lock {
            self.next_required = self
                .next_required
                .checked_add(1)
                .ok_or(CrabError::LTXCorrupted)?;
        }
        if pgno > self.previous_count {
            if pgno != self.next_required {
                return Err(CrabError::LTXCorrupted);
            }
            self.next_required = self
                .next_required
                .checked_add(1)
                .ok_or(CrabError::LTXCorrupted)?;
        }
        let old = if pgno <= self.previous_count {
            self.target.value(pgno, self.base_file.as_mut())?
        } else {
            0
        };
        let checksum = ltx::checksum_page(pgno, data);
        self.target.checksum = CHECKSUM_FLAG | (self.target.checksum ^ old ^ checksum);
        self.target.changes.insert(pgno, checksum);
        self.previous = pgno;
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<()> {
        let lock = ltx::lock_pgno(self.page_size);
        while self.next_required == lock {
            self.next_required = self
                .next_required
                .checked_add(1)
                .ok_or(CrabError::LTXCorrupted)?;
        }
        if self.commit > self.previous_count && self.next_required <= self.commit {
            return Err(CrabError::LTXCorrupted);
        }
        self.target.count = self.commit;
        Ok(())
    }
}

#[cfg(all(test, feature = "replica"))]
mod tests {
    use super::*;

    fn page(number: u32, byte: u8) -> (u32, Vec<u8>) {
        (number, vec![byte; 4096])
    }

    fn checksum(pages: &[(u32, Vec<u8>)]) -> u64 {
        pages.iter().fold(CHECKSUM_FLAG, |sum, (number, bytes)| {
            CHECKSUM_FLAG | (sum ^ ltx::checksum_page(*number, bytes))
        })
    }

    #[test]
    fn file_backed_overlay_matches_full_scan_across_update_truncate_and_regrowth() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("checksums");
        let mut pages = vec![page(1, 1), page(2, 2), page(3, 3), page(4, 4)];
        let initial_checksum = checksum(&pages);
        let bytes: Vec<u8> = pages
            .iter()
            .flat_map(|(number, bytes)| ltx::checksum_page(*number, bytes).to_be_bytes())
            .collect();
        std::fs::write(&path, bytes).unwrap();
        let host = crate::LtxHost {
            facilities: crate::Host::default(),
            max_database_bytes: 1 << 20,
            max_file_bytes: 1 << 20,
        };
        let mut index = PageChecksums::from_file(host, &path, 4096, 4, initial_checksum).unwrap();
        assert!(matches!(index.base, ChecksumBase::File(_)));
        assert!(index.changes.is_empty());

        pages[1] = page(2, 9);
        pages.truncate(3);
        index.apply(4096, 3, &[pages[1].clone()], 1 << 20).unwrap();
        assert_eq!(index.changes.len(), 1);
        assert_eq!(index.checksum(), checksum(&pages));
        index.persist().unwrap();
        assert!(index.changes.is_empty());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            pages
                .iter()
                .flat_map(|(number, bytes)| ltx::checksum_page(*number, bytes).to_be_bytes())
                .collect::<Vec<_>>()
        );

        pages[0] = page(1, 6);
        pages.push(page(4, 7));
        pages.push(page(5, 8));
        index
            .apply(
                4096,
                5,
                &[pages[0].clone(), pages[3].clone(), pages[4].clone()],
                1 << 20,
            )
            .unwrap();
        assert_eq!(index.checksum(), checksum(&pages));
        index.persist().unwrap();
        assert_eq!(
            std::fs::read(path).unwrap(),
            pages
                .iter()
                .flat_map(|(number, bytes)| ltx::checksum_page(*number, bytes).to_be_bytes())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn file_backed_truncation_reduces_multiple_checksum_chunks_with_overlay_updates() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("checksums");
        let page_size = 512;
        let count = 20_000;
        let pages = (1..=count)
            .map(|page| (page, vec![page as u8; page_size as usize]))
            .collect::<Vec<_>>();
        let initial_checksum = checksum(&pages);
        let bytes = pages
            .iter()
            .flat_map(|(number, bytes)| ltx::checksum_page(*number, bytes).to_be_bytes())
            .collect::<Vec<_>>();
        std::fs::write(&path, bytes).unwrap();
        let facilities = crate::Host::default();
        let host = crate::LtxHost {
            facilities: facilities.clone(),
            max_database_bytes: 32 << 20,
            max_file_bytes: 32 << 20,
        };
        let mut index =
            PageChecksums::from_file(host, &path, page_size, count, initial_checksum).unwrap();
        index
            .apply(
                page_size,
                count,
                &[
                    (101, vec![91; page_size as usize]),
                    (9_000, vec![92; page_size as usize]),
                    (19_999, vec![93; page_size as usize]),
                ],
                32 << 20,
            )
            .unwrap();

        let dense_path = directory.path().join("dense");
        let mut dense = facilities.filesystem.create(&dense_path).unwrap();
        index.write_dense(dense.as_mut()).unwrap();
        drop(dense);
        let expected = pages
            .iter()
            .map(|(number, bytes)| {
                let replacement = match number {
                    101 => Some(vec![91; page_size as usize]),
                    9_000 => Some(vec![92; page_size as usize]),
                    19_999 => Some(vec![93; page_size as usize]),
                    _ => None,
                };
                ltx::checksum_page(*number, replacement.as_ref().unwrap_or(bytes))
            })
            .flat_map(u64::to_be_bytes)
            .collect::<Vec<_>>();
        assert_eq!(std::fs::read(dense_path).unwrap(), expected);

        index.apply(page_size, 100, &[], 32 << 20).unwrap();

        assert_eq!(index.checksum(), checksum(&pages[..100]));
    }
}
