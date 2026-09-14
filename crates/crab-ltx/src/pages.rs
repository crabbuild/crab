use crate::{CHECKSUM_FLAG, CrabError, Result, ltx};

#[derive(Clone, Default)]
pub(crate) struct PageChecksums {
    // Every real LTX page checksum has its high bit set; zero means missing.
    pages: Vec<u64>,
}

impl PageChecksums {
    #[cfg(feature = "replica")]
    pub(crate) fn from_checksums(
        page_size: u32,
        count: u32,
        pages: impl Iterator<Item = (u32, u64)>,
    ) -> Result<Self> {
        let mut result = Self {
            pages: vec![0; count as usize],
        };
        for (page, checksum) in pages {
            if page == 0 || page > count || checksum & CHECKSUM_FLAG == 0 {
                return Err(CrabError::LTXCorrupted);
            }
            result.pages[page as usize - 1] = checksum;
        }
        if result
            .pages
            .iter()
            .enumerate()
            .any(|(i, c)| *c == 0 && i as u32 + 1 != ltx::lock_pgno(page_size))
        {
            return Err(CrabError::LTXCorrupted);
        }
        Ok(result)
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
        Ok(Self { pages })
    }
    pub fn apply(
        &mut self,
        page_size: u32,
        commit: u32,
        pages: &[(u32, Vec<u8>)],
        limit: u64,
    ) -> Result<()> {
        if !ltx::is_valid_page_size(page_size) || commit == 0 {
            return Err(CrabError::LTXCorrupted);
        }
        if u64::from(page_size) * u64::from(commit) > limit {
            return Err(CrabError::Limit("database bytes"));
        }
        self.pages.resize(commit as usize, 0);
        let lock = ltx::lock_pgno(page_size);
        let mut previous = 0;
        for (pgno, data) in pages {
            if *pgno <= previous
                || *pgno > commit
                || *pgno == lock
                || data.len() != page_size as usize
            {
                return Err(CrabError::LTXCorrupted);
            }
            self.pages[*pgno as usize - 1] = ltx::checksum_page(*pgno, data);
            previous = *pgno;
        }
        if self
            .pages
            .iter()
            .enumerate()
            .any(|(i, c)| i as u32 + 1 != lock && *c == 0)
        {
            return Err(CrabError::LTXCorrupted);
        }
        Ok(())
    }

    pub fn checksum(&self) -> u64 {
        self.pages
            .iter()
            .fold(CHECKSUM_FLAG, |sum, page| CHECKSUM_FLAG | (sum ^ page))
    }
}
