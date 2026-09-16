use std::{collections::HashMap, sync::Arc};

use crate::{CHECKSUM_FLAG, CrabError, Result, ltx};

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
    pub(crate) fn from_checksums(
        page_size: u32,
        count: u32,
        pages: impl Iterator<Item = (u32, u64)>,
    ) -> Result<Self> {
        let mut dense = vec![0; count as usize];
        for (page, checksum) in pages {
            if page == 0 || page > count || checksum & CHECKSUM_FLAG == 0 {
                return Err(CrabError::LTXCorrupted);
            }
            dense[page as usize - 1] = checksum;
        }
        Self::from_dense(page_size, dense)
    }

    #[cfg(feature = "replica")]
    pub(crate) fn from_dense(page_size: u32, pages: Vec<u64>) -> Result<Self> {
        if !ltx::is_valid_page_size(page_size)
            || pages.is_empty()
            || pages.iter().enumerate().any(|(index, checksum)| {
                let page = index as u32 + 1;
                (*checksum == 0) != (page == ltx::lock_pgno(page_size))
                    || (*checksum != 0 && *checksum & CHECKSUM_FLAG == 0)
            })
        {
            return Err(CrabError::LTXCorrupted);
        }
        let checksum = pages
            .iter()
            .fold(CHECKSUM_FLAG, |sum, page| CHECKSUM_FLAG | (sum ^ page));
        let count = u32::try_from(pages.len()).map_err(|_| CrabError::LTXCorrupted)?;
        Ok(Self {
            base: ChecksumBase::Memory(Arc::from(pages)),
            base_count: count,
            count,
            changes: HashMap::new(),
            checksum,
        })
    }

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
        if !ltx::is_valid_page_size(page_size) || commit == 0 {
            return Err(CrabError::LTXCorrupted);
        }
        if u64::from(page_size) * u64::from(commit) > limit {
            return Err(CrabError::Limit("database bytes"));
        }

        let previous_count = self.count;
        #[cfg(feature = "replica")]
        let mut base_file = match &self.base {
            ChecksumBase::File(base) => Some(base.open()?),
            ChecksumBase::Memory(_) => None,
        };
        #[cfg(not(feature = "replica"))]
        let mut base_file = None;
        if commit < previous_count {
            for page in commit + 1..=previous_count {
                let old = self.value(page, base_file.as_mut())?;
                self.checksum = CHECKSUM_FLAG | (self.checksum ^ old);
            }
            self.changes.retain(|page, _| *page <= commit);
        }

        let lock = ltx::lock_pgno(page_size);
        let mut previous = 0;
        let mut next_required = previous_count.saturating_add(1);
        for page in pages {
            let (pgno, data) = page?;
            if pgno <= previous || pgno > commit || pgno == lock || data.len() != page_size as usize
            {
                return Err(CrabError::LTXCorrupted);
            }
            while next_required == lock {
                next_required = next_required
                    .checked_add(1)
                    .ok_or(CrabError::LTXCorrupted)?;
            }
            if pgno > previous_count {
                if pgno != next_required {
                    return Err(CrabError::LTXCorrupted);
                }
                next_required = next_required
                    .checked_add(1)
                    .ok_or(CrabError::LTXCorrupted)?;
            }
            let old = if pgno <= previous_count {
                self.value(pgno, base_file.as_mut())?
            } else {
                0
            };
            let checksum = ltx::checksum_page(pgno, &data);
            self.checksum = CHECKSUM_FLAG | (self.checksum ^ old ^ checksum);
            self.changes.insert(pgno, checksum);
            previous = pgno;
        }
        while next_required == lock {
            next_required = next_required
                .checked_add(1)
                .ok_or(CrabError::LTXCorrupted)?;
        }
        if commit > previous_count && next_required <= commit {
            return Err(CrabError::LTXCorrupted);
        }
        self.count = commit;
        Ok(())
    }

    /// Persists a successful candidate after its LTX cut is durably sealed.
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
                for (&page, &checksum) in &self.changes {
                    if page <= self.count {
                        file.write_all_at(u64::from(page - 1) * 8, &checksum.to_be_bytes())?;
                    }
                }
                file.set_len(length)?;
                file.sync_all()?;
                self.base_count = self.count;
                self.changes.clear();
            }
        }
        Ok(())
    }

    pub fn checksum(&self) -> u64 {
        self.checksum
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
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 24);

        pages.push(page(4, 7));
        pages.push(page(5, 8));
        index
            .apply(4096, 5, &[pages[3].clone(), pages[4].clone()], 1 << 20)
            .unwrap();
        assert_eq!(index.checksum(), checksum(&pages));
        index.persist().unwrap();
        assert_eq!(std::fs::metadata(path).unwrap().len(), 40);
    }
}
