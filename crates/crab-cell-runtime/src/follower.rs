//! Follower lanes: per-leader record streams that back fleet durability proofs.
use std::collections::{BTreeMap, HashMap, btree_map::Entry};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;

use crate::identity::SessionId;
use crate::{Error, Result};

mod directory;
mod records;

use directory::*;
use records::*;

const RECORD_MAGIC: &[u8; 4] = b"CFR1";
const RECORD_HEADER_BYTES: usize = 52;
const ROTATE_BYTES: u64 = 64 << 20;
const MAX_APPEND_FRAMES: usize = 64;
const MAX_TAIL_PAGE_BYTES: usize = 1 << 20;
const MAX_TAIL_PAGE_FRAMES: usize = 4096;
const MAX_RETIRED_LANES: usize = 1_024;
const FOLLOWER_QUARANTINE: &str = "followers-quarantine";
const INDEX_BYTES_PER_RECORD: u64 = 128;
const MAX_FOLLOWER_INDEX_BYTES: u64 = 256 << 20;

#[cfg(test)]
type ScanCounter = Arc<AtomicUsize>;
#[cfg(not(test))]
#[derive(Clone, Copy)]
struct ScanCounter;

#[cfg(test)]
fn new_scan_counter() -> ScanCounter {
    Arc::new(AtomicUsize::new(0))
}

#[cfg(not(test))]
const fn new_scan_counter() -> ScanCounter {
    ScanCounter
}

#[cfg(test)]
fn clone_scan_counter(counter: &ScanCounter) -> ScanCounter {
    Arc::clone(counter)
}

#[cfg(not(test))]
const fn clone_scan_counter(counter: &ScanCounter) -> ScanCounter {
    *counter
}

#[cfg(test)]
fn count_scan(counter: &ScanCounter) {
    counter.fetch_add(1, Ordering::Relaxed);
}

#[cfg(not(test))]
const fn count_scan(_: &ScanCounter) {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Lane {
    leader: SessionId,
    epoch: u64,
}

type LaneState = Arc<Mutex<Option<LaneMemory>>>;
type LaneMap = Arc<Mutex<HashMap<Lane, LaneState>>>;

/// Durable contiguous range retained by one follower lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FollowerReceipt {
    /// First sequence the lane retained before the append.
    pub base_sequence: u64,
    /// Highest sequence the lane has made durable.
    pub durable_through: u64,
}

/// One bounded page from a sealed follower lane.
pub struct FollowerTailPage {
    /// Frames in sequence order.
    pub frames: Vec<Bytes>,
    /// Sequence to continue from when the page filled its bound.
    pub next_sequence: Option<u64>,
}

/// Exact retired follower lane eligible for authority-checked collection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetiredFollowerLane {
    leader: SessionId,
    epoch: u64,
    covered_through: u64,
    retired_at_ms: i64,
}

impl RetiredFollowerLane {
    /// Returns the leader whose lane was retired.
    #[must_use]
    pub const fn leader(&self) -> SessionId {
        self.leader
    }

    /// Returns the node-log epoch the lane belonged to.
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Returns the highest sequence object storage covered.
    #[must_use]
    pub const fn covered_through(&self) -> u64 {
        self.covered_through
    }

    /// Returns the logical time the lane was retired.
    #[must_use]
    pub const fn retired_at_ms(&self) -> i64 {
        self.retired_at_ms
    }
}

/// Local SSD store for checksum-verified follower fragments.
///
/// The bytes are durability obligations until the leader's contiguous object
/// watermark permits whole-chunk deletion. They are never cache-evicted.
#[derive(Clone)]
pub struct FollowerStore {
    root: PathBuf,
    limits: crab_ltx::Limits,
    lanes: LaneMap,
    disk: crab_ltx::DiskBudget,
    retained: Arc<Mutex<crab_ltx::DiskReservation>>,
    index_used: Arc<Mutex<u64>>,
    quarantined_entries: usize,
    scan_counter: ScanCounter,
}

impl FollowerStore {
    /// Opens the `followers` namespace beneath a durable node data directory.
    pub fn open(
        root: PathBuf,
        limits: crab_ltx::Limits,
        disk: crab_ltx::DiskBudget,
    ) -> Result<Self> {
        let existed = root.exists();
        std::fs::create_dir_all(&root).map_err(crab_ltx::CrabError::from)?;
        if !existed {
            let parent = root
                .parent()
                .ok_or(Error::Node("follower root has no parent"))?;
            sync_directory(parent).map_err(crab_ltx::CrabError::from)?;
        }
        sync_directory(&root).map_err(crab_ltx::CrabError::from)?;
        scrub_followers(&root, limits)?;
        let retained = disk.try_reserve(follower_bytes(&root)?)?;
        let quarantined_entries = quarantine_entry_count(&root)?;
        Ok(Self {
            root,
            limits,
            lanes: Arc::new(Mutex::new(HashMap::new())),
            disk,
            retained: Arc::new(Mutex::new(retained)),
            index_used: Arc::new(Mutex::new(0)),
            quarantined_entries,
            scan_counter: new_scan_counter(),
        })
    }

    #[cfg(test)]
    pub(crate) fn scan_count(&self) -> usize {
        self.scan_counter.load(Ordering::Relaxed)
    }

    /// Returns the bytes the store currently retains.
    #[must_use]
    pub fn retained_bytes(&self) -> u64 {
        match self.retained.lock() {
            Ok(retained) => retained.bytes(),
            Err(poisoned) => poisoned.into_inner().bytes(),
        }
    }

    /// Returns the bytes the store may still write.
    #[must_use]
    pub fn available_bytes(&self) -> u64 {
        self.disk.available()
    }

    /// Reports diagnostic entries isolated by this or an earlier startup scrub.
    #[must_use]
    pub const fn quarantined_entries(&self) -> usize {
        self.quarantined_entries
    }

    /// Appends one ordered batch and acknowledges only after `sync_data`.
    pub async fn append(
        &self,
        leader: SessionId,
        epoch: u64,
        frames: Vec<Bytes>,
        covered_through: u64,
    ) -> Result<FollowerReceipt> {
        let encoded_bytes = frames
            .iter()
            .try_fold(0_u64, |total, frame| total.checked_add(frame.len() as u64));
        if frames.is_empty()
            || frames.len() > MAX_APPEND_FRAMES
            || encoded_bytes
                .is_none_or(|bytes| bytes > self.limits.max_capture_bytes.saturating_add(64 * 240))
        {
            return Err(Error::Node("invalid follower append batch"));
        }
        let lane = Lane { leader, epoch };
        let lock = self.lane_lock(lane)?;
        let root = self.root.clone();
        let limits = self.limits;
        let retained = Arc::clone(&self.retained);
        let index_used = Arc::clone(&self.index_used);
        let scan_counter = clone_scan_counter(&self.scan_counter);
        let growth = encoded_bytes
            .and_then(|bytes| bytes.checked_add((frames.len() * RECORD_HEADER_BYTES) as u64))
            .ok_or(Error::Node("follower append byte count overflow"))?;
        tokio::task::spawn_blocking(move || {
            let retained = retained
                .lock()
                .map_err(|_| Error::Node("follower disk reservation lock poisoned"))?;
            retained.try_grow(growth)?;
            let mut state = lock
                .lock()
                .map_err(|_| Error::Node("follower lane lock poisoned"))?;
            let result = append_sync(
                &root,
                lane,
                frames,
                covered_through,
                limits,
                &index_used,
                &mut state,
                &scan_counter,
            );
            let resize =
                follower_bytes(&root).and_then(|bytes| retained.resize(bytes).map_err(Error::from));
            if result.is_err() {
                *state = None;
            }
            settle_disk_reservation(result, resize)
        })
        .await
        .map_err(Error::FollowerWorkerJoin)?
    }

    /// Seals a lane against future appends and returns its retained range.
    pub async fn seal(&self, leader: SessionId, epoch: u64) -> Result<FollowerReceipt> {
        let lane = Lane { leader, epoch };
        let lock = self.lane_lock(lane)?;
        let root = self.root.clone();
        let limits = self.limits;
        let retained = Arc::clone(&self.retained);
        let index_used = Arc::clone(&self.index_used);
        let scan_counter = clone_scan_counter(&self.scan_counter);
        tokio::task::spawn_blocking(move || {
            let retained = retained
                .lock()
                .map_err(|_| Error::Node("follower disk reservation lock poisoned"))?;
            let mut state = lock
                .lock()
                .map_err(|_| Error::Node("follower lane lock poisoned"))?;
            let directory = lane_directory(&root, lane);
            if !directory.join("sealed").exists() && !directory.join("retired").exists() {
                retained.try_grow(8)?;
            }
            let result = seal_sync(&root, lane, limits, &index_used, &mut state, &scan_counter);
            let resize =
                follower_bytes(&root).and_then(|bytes| retained.resize(bytes).map_err(Error::from));
            if result.is_err() {
                *state = None;
            }
            settle_disk_reservation(result, resize)
        })
        .await
        .map_err(Error::FollowerWorkerJoin)?
    }

    /// Retires one fully object-covered lane and keeps a durable append fence.
    pub async fn retire(
        &self,
        leader: SessionId,
        epoch: u64,
        covered_through: u64,
    ) -> Result<FollowerReceipt> {
        let lane = Lane { leader, epoch };
        let lock = self.lane_lock(lane)?;
        let root = self.root.clone();
        let limits = self.limits;
        let retained = Arc::clone(&self.retained);
        let scan_counter = clone_scan_counter(&self.scan_counter);
        tokio::task::spawn_blocking(move || {
            let retained = retained
                .lock()
                .map_err(|_| Error::Node("follower disk reservation lock poisoned"))?;
            let mut state = lock
                .lock()
                .map_err(|_| Error::Node("follower lane lock poisoned"))?;
            if !lane_directory(&root, lane).join("retired").exists() {
                retained.try_grow(8)?;
            }
            let result = retire_sync(&root, lane, covered_through, limits, &scan_counter);
            let resize =
                follower_bytes(&root).and_then(|bytes| retained.resize(bytes).map_err(Error::from));
            *state = None;
            settle_disk_reservation(result, resize)
        })
        .await
        .map_err(Error::FollowerWorkerJoin)?
    }

    /// Reads a sealed, verified tail in node-sequence order.
    pub async fn read_tail(
        &self,
        leader: SessionId,
        epoch: u64,
        first_sequence: u64,
    ) -> Result<Vec<Bytes>> {
        let lane = Lane { leader, epoch };
        let lock = self.lane_lock(lane)?;
        let root = self.root.clone();
        let limits = self.limits;
        let index_used = Arc::clone(&self.index_used);
        let scan_counter = clone_scan_counter(&self.scan_counter);
        tokio::task::spawn_blocking(move || {
            let mut state = lock
                .lock()
                .map_err(|_| Error::Node("follower lane lock poisoned"))?;
            let result = read_tail_sync(
                &root,
                lane,
                first_sequence,
                limits,
                &index_used,
                &mut state,
                usize::MAX,
                usize::MAX,
                &scan_counter,
            )
            .map(|page| page.frames);
            if result.is_err() {
                *state = None;
            }
            result
        })
        .await
        .map_err(Error::FollowerWorkerJoin)?
    }

    /// Reads one network-sized page from a sealed, verified tail.
    ///
    /// A single frame may exceed the page target and is returned alone because
    /// node frames are the independently checksummed transport unit.
    pub async fn read_tail_page(
        &self,
        leader: SessionId,
        epoch: u64,
        first_sequence: u64,
    ) -> Result<FollowerTailPage> {
        let lane = Lane { leader, epoch };
        let lock = self.lane_lock(lane)?;
        let root = self.root.clone();
        let limits = self.limits;
        let index_used = Arc::clone(&self.index_used);
        let scan_counter = clone_scan_counter(&self.scan_counter);
        tokio::task::spawn_blocking(move || {
            let mut state = lock
                .lock()
                .map_err(|_| Error::Node("follower lane lock poisoned"))?;
            let result = read_tail_sync(
                &root,
                lane,
                first_sequence,
                limits,
                &index_used,
                &mut state,
                MAX_TAIL_PAGE_BYTES,
                MAX_TAIL_PAGE_FRAMES,
                &scan_counter,
            );
            if result.is_err() {
                *state = None;
            }
            result
        })
        .await
        .map_err(Error::FollowerWorkerJoin)?
    }

    /// Lists bounded retired lanes whose marker predates the caller's grace cutoff.
    pub async fn retired_lanes(
        &self,
        retired_before_ms: i64,
        limit: usize,
    ) -> Result<Vec<RetiredFollowerLane>> {
        if retired_before_ms < 0 || !(1..=MAX_RETIRED_LANES).contains(&limit) {
            return Err(Error::Node("retired follower scan bound is invalid"));
        }
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || retired_lanes_sync(&root, retired_before_ms, limit))
            .await
            .map_err(Error::FollowerWorkerJoin)?
    }

    /// Deletes one exact, grace-aged retired lane after external authority proof.
    pub async fn remove_retired(
        &self,
        candidate: RetiredFollowerLane,
        retired_before_ms: i64,
    ) -> Result<bool> {
        if candidate.retired_at_ms > retired_before_ms {
            return Err(Error::Node("retired follower grace period has not elapsed"));
        }
        let lane = Lane {
            leader: candidate.leader,
            epoch: candidate.epoch,
        };
        let lock = self.lane_lock(lane)?;
        let cleanup_lock = Arc::clone(&lock);
        let root = self.root.clone();
        let retained = Arc::clone(&self.retained);
        let removed = tokio::task::spawn_blocking(move || {
            let retained = retained
                .lock()
                .map_err(|_| Error::Node("follower disk reservation lock poisoned"))?;
            let _lane = lock
                .lock()
                .map_err(|_| Error::Node("follower lane lock poisoned"))?;
            let removed = remove_retired_sync(&root, lane, candidate, retired_before_ms)?;
            retained.resize(follower_bytes(&root)?)?;
            Ok::<bool, Error>(removed)
        })
        .await
        .map_err(Error::FollowerWorkerJoin)??;
        if removed {
            let mut lanes = self
                .lanes
                .lock()
                .map_err(|_| Error::Node("follower store lock poisoned"))?;
            if lanes
                .get(&lane)
                .is_some_and(|current| Arc::ptr_eq(current, &cleanup_lock))
                && Arc::strong_count(&cleanup_lock) == 2
            {
                lanes.remove(&lane);
            }
        }
        Ok(removed)
    }

    fn lane_lock(&self, lane: Lane) -> Result<Arc<Mutex<Option<LaneMemory>>>> {
        let mut lanes = self
            .lanes
            .lock()
            .map_err(|_| Error::Node("follower store lock poisoned"))?;
        Ok(lanes
            .entry(lane)
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone())
    }
}

#[cfg(test)]
mod tests;
