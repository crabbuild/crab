//! Aligned read-ahead windows over a remote copy of the scanned pack.

use std::num::NonZeroUsize;
use std::sync::Arc;

use lru::LruCache;

use super::{BoxError, EntrySource};

/// Serves entry re-reads from recently fetched, window-aligned pack ranges.
///
/// Evicted delta bases are small and cluster near each other, so one ranged
/// read usually covers many later re-reads. Windows are clipped to the pack.
pub struct WindowedSource<R> {
    read: R,
    size: u64,
    window: u64,
    windows: LruCache<u64, Arc<[u8]>>,
}

impl<R> WindowedSource<R>
where
    R: FnMut(u64, u64) -> Result<Vec<u8>, BoxError>,
{
    /// Wrap `read(start, end)` over a pack of `size` bytes with `slots` windows.
    pub fn new(read: R, size: u64, window: u64, slots: usize) -> Self {
        Self {
            read,
            size,
            window: window.max(1),
            windows: LruCache::new(NonZeroUsize::new(slots).unwrap_or(NonZeroUsize::MIN)),
        }
    }

    fn window(&mut self, index: u64) -> Result<Arc<[u8]>, BoxError> {
        if let Some(bytes) = self.windows.get(&index) {
            return Ok(Arc::clone(bytes));
        }
        let start = index * self.window;
        let end = start.saturating_add(self.window).min(self.size);
        let bytes: Arc<[u8]> = Arc::from((self.read)(start, end)?);
        if bytes.len() as u64 != end - start {
            return Err("pack window read returned the wrong length".into());
        }
        self.windows.put(index, Arc::clone(&bytes));
        Ok(bytes)
    }
}

impl<R> EntrySource for WindowedSource<R>
where
    R: FnMut(u64, u64) -> Result<Vec<u8>, BoxError>,
{
    fn read(&mut self, start: u64, end: u64) -> Result<Vec<u8>, BoxError> {
        if start >= end || end > self.size {
            return Err("pack entry range is outside the pack".into());
        }
        let mut bytes = Vec::with_capacity((end - start) as usize);
        let mut position = start;
        while position < end {
            let index = position / self.window;
            let window = self.window(index)?;
            let window_start = index * self.window;
            let from = (position - window_start) as usize;
            let to = ((end - window_start) as usize).min(window.len());
            bytes.extend_from_slice(&window[from..to]);
            position = window_start + to as u64;
        }
        Ok(bytes)
    }
}
