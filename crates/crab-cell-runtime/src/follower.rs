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

mod records;

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
    pub base_sequence: u64,
    pub durable_through: u64,
}

/// One bounded page from a sealed follower lane.
pub struct FollowerTailPage {
    pub frames: Vec<Bytes>,
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
    #[must_use]
    pub const fn leader(&self) -> SessionId {
        self.leader
    }

    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    #[must_use]
    pub const fn covered_through(&self) -> u64 {
        self.covered_through
    }

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

    #[must_use]
    pub fn retained_bytes(&self) -> u64 {
        match self.retained.lock() {
            Ok(retained) => retained.bytes(),
            Err(poisoned) => poisoned.into_inner().bytes(),
        }
    }

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

fn directory_bytes(path: &Path) -> Result<u64> {
    let mut total = 0_u64;
    let mut pending = vec![path.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                total = total
                    .checked_add(entry.metadata()?.len())
                    .ok_or(Error::Node("follower retained byte count overflow"))?;
            } else {
                return Err(Error::Node("follower storage contains a special file"));
            }
        }
    }
    Ok(total)
}

fn follower_bytes(root: &Path) -> Result<u64> {
    let followers = root.join("followers");
    let retained = if followers.exists() {
        directory_bytes(&followers)?
    } else {
        0
    };
    let quarantine = root.join(FOLLOWER_QUARANTINE);
    let quarantined = if quarantine.exists() {
        directory_bytes(&quarantine)?
    } else {
        0
    };
    retained
        .checked_add(quarantined)
        .ok_or(Error::Node("follower retained byte count overflow"))
}

fn scrub_followers(root: &Path, limits: crab_ltx::Limits) -> Result<()> {
    let followers = root.join("followers");
    if !followers.exists() {
        return Ok(());
    }
    if !std::fs::symlink_metadata(&followers)?.file_type().is_dir() {
        quarantine_entry(root, &followers)?;
        return Ok(());
    }
    let mut leaders = directory_entries(&followers)?;
    for leader_path in leaders.drain(..) {
        let leader = match directory_lane_component(&leader_path, parse_session_directory) {
            Ok(leader) => leader,
            Err(_) => {
                quarantine_entry(root, &leader_path)?;
                continue;
            }
        };
        let mut epochs = directory_entries(&leader_path)?;
        for epoch_path in epochs.drain(..) {
            let epoch = match directory_lane_component(&epoch_path, parse_epoch_directory) {
                Ok(epoch) => epoch,
                Err(_) => {
                    quarantine_entry(root, &epoch_path)?;
                    continue;
                }
            };
            let lane = Lane { leader, epoch };
            let prune_temp = epoch_path.join("open.log.tmp");
            if prune_temp.exists() {
                // A crashed rewrite leaves the old open lane authoritative;
                // discard only the uncommitted temporary before validation.
                if std::fs::symlink_metadata(&prune_temp)?
                    .file_type()
                    .is_file()
                {
                    std::fs::remove_file(&prune_temp)?;
                    sync_directory(&epoch_path)?;
                } else {
                    quarantine_entry(root, &epoch_path)?;
                    continue;
                }
            }
            if validate_stored_lane(root, lane, limits).is_err() {
                quarantine_entry(root, &epoch_path)?;
            }
        }
        if leader_path.exists() && std::fs::read_dir(&leader_path)?.next().is_none() {
            std::fs::remove_dir(&leader_path)?;
            sync_directory(&followers)?;
        }
    }
    Ok(())
}

fn directory_entries(directory: &Path) -> Result<Vec<PathBuf>> {
    let mut entries = std::fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort();
    Ok(entries)
}

fn directory_lane_component<T>(path: &Path, parse: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
    if !std::fs::symlink_metadata(path)?.file_type().is_dir() {
        return Err(Error::Node("follower lane component is not a directory"));
    }
    parse(path)
}

fn validate_stored_lane(root: &Path, lane: Lane, limits: crab_ltx::Limits) -> Result<()> {
    let directory = lane_directory(root, lane);
    for entry in std::fs::read_dir(&directory)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| Error::Node("follower lane entry is not UTF-8"))?;
        let file_type = entry.file_type()?;
        let valid = match name.as_str() {
            "chunks" => file_type.is_dir(),
            "sealed" | "retired" => file_type.is_file(),
            _ => false,
        };
        if !valid {
            return Err(Error::Node("follower lane contains an invalid entry"));
        }
    }
    let records = scan_lane(&directory.join("chunks"), lane, limits)?;
    let durable_through = records.keys().next_back().copied().unwrap_or(0);
    let sealed = directory.join("sealed");
    if sealed.exists()
        && read_watermark(&sealed, "follower seal marker is invalid")? != durable_through
    {
        return Err(Error::Node("follower seal watermark differs"));
    }
    let retired = directory.join("retired");
    if retired.exists()
        && read_watermark(&retired, "follower retire marker is invalid")? < durable_through
    {
        return Err(Error::Node("follower lane has uncovered records"));
    }
    Ok(())
}

fn quarantine_entry(root: &Path, source: &Path) -> Result<()> {
    let quarantine = ensure_child(root, FOLLOWER_QUARANTINE)?;
    let mut index = quarantine_entry_count(root)? as u64;
    let destination = loop {
        index = index
            .checked_add(1)
            .ok_or(Error::Node("follower quarantine index overflow"))?;
        let candidate = quarantine.join(format!("{index:020}.bad"));
        if !candidate.exists() {
            break candidate;
        }
    };
    std::fs::rename(source, destination)?;
    let source_parent = source
        .parent()
        .ok_or(Error::Node("follower quarantine source has no parent"))?;
    sync_directory(source_parent)?;
    sync_directory(&quarantine)?;
    Ok(())
}

fn quarantine_entry_count(root: &Path) -> Result<usize> {
    let quarantine = root.join(FOLLOWER_QUARANTINE);
    if !quarantine.exists() {
        return Ok(0);
    }
    Ok(
        std::fs::read_dir(quarantine)?.try_fold(0_usize, |count, entry| {
            entry?;
            count
                .checked_add(1)
                .ok_or_else(|| std::io::Error::other("follower quarantine entry count overflow"))
        })?,
    )
}

fn retired_lanes_sync(
    root: &Path,
    retired_before_ms: i64,
    limit: usize,
) -> Result<Vec<RetiredFollowerLane>> {
    let followers = root.join("followers");
    if !followers.exists() {
        return Ok(Vec::new());
    }
    let mut leaders = std::fs::read_dir(&followers)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    leaders.sort();
    let mut retired = Vec::new();
    for leader_path in leaders {
        let leader = parse_session_directory(&leader_path)?;
        let mut epochs = std::fs::read_dir(&leader_path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        epochs.sort();
        for epoch_path in epochs {
            let epoch = parse_epoch_directory(&epoch_path)?;
            let marker = epoch_path.join("retired");
            if !marker.exists() {
                continue;
            }
            let retired_at_ms = modified_at_ms(&marker)?;
            if retired_at_ms > retired_before_ms {
                continue;
            }
            retired.push(RetiredFollowerLane {
                leader,
                epoch,
                covered_through: read_watermark(&marker, "follower retire marker is invalid")?,
                retired_at_ms,
            });
            if retired.len() == limit {
                return Ok(retired);
            }
        }
    }
    Ok(retired)
}

fn remove_retired_sync(
    root: &Path,
    lane: Lane,
    candidate: RetiredFollowerLane,
    retired_before_ms: i64,
) -> Result<bool> {
    validate_lane(lane)?;
    let directory = lane_directory(root, lane);
    let marker = directory.join("retired");
    if !marker.exists() {
        return Ok(false);
    }
    if read_watermark(&marker, "follower retire marker is invalid")? != candidate.covered_through
        || modified_at_ms(&marker)? != candidate.retired_at_ms
        || candidate.retired_at_ms > retired_before_ms
    {
        return Err(Error::Node("retired follower marker changed"));
    }
    std::fs::remove_dir_all(&directory)?;
    let leader = directory
        .parent()
        .ok_or(Error::Node("follower leader directory is missing"))?;
    sync_directory(leader)?;
    if std::fs::read_dir(leader)?.next().is_none() {
        std::fs::remove_dir(leader)?;
        if let Some(followers) = leader.parent() {
            sync_directory(followers)?;
        }
    }
    Ok(true)
}

fn modified_at_ms(path: &Path) -> Result<i64> {
    let duration = std::fs::metadata(path)?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| Error::Node("follower marker time predates Unix epoch"))?;
    i64::try_from(duration.as_millis()).map_err(|_| Error::Node("follower marker time exceeds i64"))
}

fn parse_session_directory(path: &Path) -> Result<SessionId> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(Error::Node("follower leader directory is not UTF-8"))?;
    if name.len() != 32 {
        return Err(Error::Node("follower leader directory is invalid"));
    }
    let mut bytes = [0_u8; 16];
    for (index, pair) in name.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let text = std::str::from_utf8(pair)
            .map_err(|_| Error::Node("follower leader directory is invalid"))?;
        bytes[index] = u8::from_str_radix(text, 16)
            .map_err(|_| Error::Node("follower leader directory is invalid"))?;
    }
    let session = SessionId::from_bytes(bytes);
    validate_lane(Lane {
        leader: session,
        epoch: 1,
    })?;
    Ok(session)
}

fn parse_epoch_directory(path: &Path) -> Result<u64> {
    let epoch = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.parse::<u64>().ok())
        .filter(|epoch| *epoch != 0)
        .ok_or(Error::Node("follower epoch directory is invalid"))?;
    Ok(epoch)
}

fn settle_disk_reservation(
    result: Result<FollowerReceipt>,
    resize: Result<()>,
) -> Result<FollowerReceipt> {
    // Reconcile the shared admission even when the filesystem operation
    // failed.  Returning early on `result` would retain the preflight growth
    // forever and eventually make unrelated Cells fail closed for capacity.
    resize?;
    result
}

fn lane_directory(root: &Path, lane: Lane) -> PathBuf {
    root.join("followers")
        .join(hex(lane.leader.as_bytes()))
        .join(lane.epoch.to_string())
}

fn ensure_lane_directories(root: &Path, lane: Lane) -> Result<()> {
    let followers = ensure_child(root, "followers")?;
    let leader = ensure_child(&followers, &hex(lane.leader.as_bytes()))?;
    let epoch = ensure_child(&leader, &lane.epoch.to_string())?;
    ensure_child(&epoch, "chunks")?;
    Ok(())
}

fn ensure_child(parent: &Path, name: &str) -> Result<PathBuf> {
    let child = parent.join(name);
    if !child.exists() {
        std::fs::create_dir(&child)?;
        sync_directory(parent)?;
    }
    Ok(child)
}

fn validate_lane(lane: Lane) -> Result<()> {
    if lane.leader.as_bytes().iter().all(|byte| *byte == 0) || lane.epoch == 0 {
        return Err(Error::Node("invalid follower lane"));
    }
    Ok(())
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

fn hex(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(TABLE[(byte >> 4) as usize] as char);
        encoded.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    encoded
}

#[cfg(test)]
mod tests;
