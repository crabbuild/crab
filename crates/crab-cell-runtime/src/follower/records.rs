//! Follower lane record framing, appends, tails, and scans.
//!
//! A lane is a directory of chunk files; every helper here encodes or
//! decodes those records, reconciles the admission that reserved their
//! bytes, and never lets a torn or mismatched tail authorize a receipt.

use super::*;

mod append;
mod scan;

pub(super) use append::*;
pub(super) use scan::*;

#[derive(Clone)]
pub(in crate::follower) struct StoredRecord {
    sequence: u64,
    digest: [u8; 32],
    path: Arc<Path>,
    offset: u64,
    length: usize,
}
pub(in crate::follower) struct IndexReservation {
    used: Arc<Mutex<u64>>,
    bytes: u64,
}
impl IndexReservation {
    fn new(used: &Arc<Mutex<u64>>, bytes: u64) -> Result<Self> {
        let mut current = used
            .lock()
            .map_err(|_| Error::Node("follower index reservation lock poisoned"))?;
        let next = current
            .checked_add(bytes)
            .ok_or(Error::Capacity("follower lane index"))?;
        if next > MAX_FOLLOWER_INDEX_BYTES {
            return Err(Error::Capacity("follower lane index"));
        }
        *current = next;
        Ok(Self {
            used: Arc::clone(used),
            bytes,
        })
    }

    fn grow(&mut self, additional: u64) -> Result<()> {
        if additional == 0 {
            return Ok(());
        }
        let mut current = self
            .used
            .lock()
            .map_err(|_| Error::Node("follower index reservation lock poisoned"))?;
        let next = current
            .checked_add(additional)
            .ok_or(Error::Capacity("follower lane index"))?;
        if next > MAX_FOLLOWER_INDEX_BYTES {
            return Err(Error::Capacity("follower lane index"));
        }
        *current = next;
        self.bytes = self
            .bytes
            .checked_add(additional)
            .ok_or(Error::Capacity("follower lane index"))?;
        Ok(())
    }

    fn shrink_to(&mut self, bytes: u64) {
        if bytes >= self.bytes {
            return;
        }
        let released = self.bytes - bytes;
        if let Ok(mut current) = self.used.lock() {
            *current = current.saturating_sub(released);
        }
        self.bytes = bytes;
    }

    fn resize_to(&mut self, bytes: u64) -> Result<()> {
        if bytes > self.bytes {
            self.grow(bytes - self.bytes)
        } else {
            self.shrink_to(bytes);
            Ok(())
        }
    }
}
impl Drop for IndexReservation {
    fn drop(&mut self) {
        if let Ok(mut current) = self.used.lock() {
            *current = current.saturating_sub(self.bytes);
        }
    }
}
pub(in crate::follower) struct LaneMemory {
    records: BTreeMap<u64, StoredRecord>,
    open_first: Option<u64>,
    open_last: Option<u64>,
    index: IndexReservation,
}
