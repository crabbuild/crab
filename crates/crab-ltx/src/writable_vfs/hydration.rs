//! Bounded asynchronous fetch and owner-thread installation of inherited pages.

use super::*;

/// A bounded fetch plan for missing pages of one sparse activation.
///
/// Fetch outside the SQLite worker, then install its result on the same Db.
/// Dropping a plan or fetched batch leaves hydration progress unchanged.
pub struct HydrationRead {
    app: Arc<App>,
    pages: Vec<u32>,
    next: u32,
}

/// Authenticated inherited pages awaiting installation on their owning Db.
pub struct HydrationBatch {
    app: Arc<App>,
    pages: crate::paged_io::Pages,
    next: u32,
}

impl HydrationRead {
    /// Returns the maximum page payload retained by the fetched batch.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        self.pages.len() * self.app.page_size as usize
    }

    /// Fetches authenticated pages without accessing the SQLite connection.
    ///
    /// The caller bounds the wait and retains payload admission until the
    /// resulting batch is installed or dropped.
    pub async fn fetch(self) -> Result<HydrationBatch> {
        let mut pages = Vec::with_capacity(self.pages.len());
        let mut index = 0;
        while let Some(&first) = self.pages.get(index) {
            let mut count = 1;
            while index + count < self.pages.len()
                && self.pages.get(index + count) == first.checked_add(count as u32).as_ref()
            {
                count += 1;
            }
            let fetched = self.app.io.hydration_pages(first, count as u32).await?;
            if fetched.is_empty() || fetched.len() > count {
                return Err(CrabError::LTXCorrupted);
            }
            for (page, bytes) in fetched {
                if self.pages.get(index) != Some(&page)
                    || bytes.len() != self.app.page_size as usize
                {
                    return Err(CrabError::LTXCorrupted);
                }
                pages.push((page, bytes));
                index += 1;
            }
        }
        Ok(HydrationBatch {
            app: self.app,
            pages,
            next: self.next,
        })
    }
}

impl Registration {
    pub(crate) fn prepare_hydration(&self, limit: u32) -> Result<HydrationRead> {
        if !(1..=64).contains(&limit) {
            return Err(CrabError::InvalidState(
                "hydration batch must contain 1..64 pages",
            ));
        }
        let state = self
            .app
            .state
            .lock()
            .map_err(|_| CrabError::InvalidState("sparse state poisoned"))?;
        let mut pages = Vec::with_capacity(limit as usize);
        let mut next = self.cursor;
        while next <= self.app.count && pages.len() < limit as usize {
            if next <= state.ceiling
                && !state.present[next as usize - 1]
                && next != crate::ltx::lock_pgno(self.app.page_size)
            {
                pages.push(next);
            }
            next = next.checked_add(1).ok_or(CrabError::LTXCorrupted)?;
        }
        Ok(HydrationRead {
            app: self.app.clone(),
            pages,
            next,
        })
    }

    pub(crate) fn install_hydration(
        &mut self,
        connection: &Connection,
        batch: HydrationBatch,
    ) -> Result<Hydration> {
        if !Arc::ptr_eq(&self.app, &batch.app) {
            return Err(CrabError::InvalidState(
                "hydration batch belongs to another activation",
            ));
        }
        let mut file: *mut ffi::sqlite3_file = std::ptr::null_mut();
        // SAFETY: the connection is exclusively borrowed on its owning worker.
        // Check the wrapper and activation before using its base SQLite handle.
        unsafe {
            sqlite(ffi::sqlite3_file_control(
                connection.handle(),
                c"main".as_ptr(),
                ffi::SQLITE_FCNTL_FILE_POINTER,
                (&mut file as *mut *mut ffi::sqlite3_file).cast(),
            ))?;
            if file.is_null() || !std::ptr::eq((*file).pMethods, &METHODS) {
                return Err(CrabError::InvalidState(
                    "sparse SQLite main file unavailable",
                ));
            }
            let file = file.cast::<File>();
            if !std::ptr::eq((*file).app, Arc::as_ptr(&self.app)) {
                return Err(CrabError::InvalidState(
                    "hydration connection belongs to another activation",
                ));
            }
            for (page, bytes) in batch.pages {
                install_page(file, page, &bytes)?;
            }
        }
        self.cursor = self.cursor.max(batch.next);
        self.hydration()
    }
}
