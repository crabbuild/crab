use std::collections::{BTreeMap, HashMap, btree_map::Entry};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bytes::Bytes;

use crate::{Error, Result, SessionId};

const RECORD_MAGIC: &[u8; 4] = b"CFR1";
const RECORD_HEADER_BYTES: usize = 52;
const ROTATE_BYTES: u64 = 64 << 20;
const MAX_APPEND_FRAMES: usize = 64;
const MAX_TAIL_PAGE_BYTES: usize = 1 << 20;
const MAX_TAIL_PAGE_FRAMES: usize = 4096;
const MAX_RETIRED_LANES: usize = 1_024;
const FOLLOWER_QUARANTINE: &str = "followers-quarantine";

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
    quarantined_entries: usize,
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
            quarantined_entries,
        })
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
            let result = append_sync(&root, lane, frames, covered_through, limits, &mut state);
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
            let result = seal_sync(&root, lane, limits, &mut state);
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
            let result = retire_sync(&root, lane, covered_through, limits);
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
        tokio::task::spawn_blocking(move || {
            let _guard = lock
                .lock()
                .map_err(|_| Error::Node("follower lane lock poisoned"))?;
            read_tail_sync(&root, lane, first_sequence, limits, usize::MAX, usize::MAX)
                .map(|page| page.frames)
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
        tokio::task::spawn_blocking(move || {
            let _guard = lock
                .lock()
                .map_err(|_| Error::Node("follower lane lock poisoned"))?;
            read_tail_sync(
                &root,
                lane,
                first_sequence,
                limits,
                MAX_TAIL_PAGE_BYTES,
                MAX_TAIL_PAGE_FRAMES,
            )
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
    let receipt = result?;
    resize?;
    Ok(receipt)
}

fn append_sync(
    root: &Path,
    lane: Lane,
    frames: Vec<Bytes>,
    covered_through: u64,
    limits: crab_ltx::Limits,
    state: &mut Option<LaneMemory>,
) -> Result<FollowerReceipt> {
    validate_lane(lane)?;
    let directory = lane_directory(root, lane);
    let chunks = directory.join("chunks");
    ensure_lane_directories(root, lane)?;
    if directory.join("retired").exists() {
        return Err(Error::Node("follower lane is retired"));
    }
    if directory.join("sealed").exists() {
        return Err(Error::Node("follower lane is sealed"));
    }
    if state.is_none() {
        let retained = scan_lane(&chunks, lane, limits)?;
        let open_records = scan_chunk(&chunks.join("open.log"), lane, limits, true)?;
        *state = Some(LaneMemory {
            records: retained
                .into_iter()
                .map(|(sequence, record)| (sequence, record.digest))
                .collect(),
            open_first: open_records.first().map(|record| record.sequence),
            open_last: open_records.last().map(|record| record.sequence),
        });
    }
    let pruned_through = prune_covered(&chunks, covered_through)?;
    let state = state
        .as_mut()
        .ok_or(Error::Node("follower lane state did not initialize"))?;
    if let Some(pruned_through) = pruned_through {
        state
            .records
            .retain(|sequence, _| *sequence > pruned_through);
    }
    let mut durable_through = state
        .records
        .keys()
        .next_back()
        .copied()
        .unwrap_or(covered_through);
    if durable_through < covered_through {
        durable_through = covered_through;
    }

    let open_path = chunks.join("open.log");
    let mut file = open_append(&open_path)?;
    let mut wrote = false;
    for encoded in frames {
        let frame = crab_ltx::inspect_node_frame(encoded.clone(), limits)?;
        let scope = frame.scope();
        if scope.leader_session != *lane.leader.as_bytes() || scope.log_epoch != lane.epoch {
            return Err(Error::Node("follower frame changed lane scope"));
        }
        let sequence = scope.node_sequence;
        let digest = frame.digest();
        if let Some(existing) = state.records.get(&sequence) {
            if *existing != digest {
                return Err(Error::Node("conflicting duplicate follower frame"));
            }
            continue;
        }
        // Object publication can advance while this frame is still queued for
        // shipping. Its authoritative coverage makes a missing prefix safe to
        // skip; the follower must still persist every uncovered suffix frame.
        if sequence <= covered_through {
            continue;
        }
        if sequence != durable_through.saturating_add(1) {
            return Err(Error::Node("follower append has a sequence gap"));
        }
        let record_bytes = RECORD_HEADER_BYTES as u64 + encoded.len() as u64;
        if file.metadata()?.len() > 0
            && file.metadata()?.len().saturating_add(record_bytes) > ROTATE_BYTES
        {
            file.sync_data()?;
            drop(file);
            rotate_open(&chunks, &open_path, state.open_first, state.open_last)?;
            file = open_append(&open_path)?;
            state.open_first = None;
            state.open_last = None;
        }
        write_record(&mut file, sequence, digest, &encoded)?;
        state.open_first.get_or_insert(sequence);
        state.open_last = Some(sequence);
        durable_through = sequence;
        state.records.insert(sequence, digest);
        wrote = true;
    }
    if wrote {
        file.sync_data()?;
    }
    let base_sequence = state
        .records
        .keys()
        .next()
        .copied()
        .unwrap_or_else(|| durable_through.saturating_add(1));
    Ok(FollowerReceipt {
        base_sequence,
        durable_through,
    })
}

fn seal_sync(
    root: &Path,
    lane: Lane,
    limits: crab_ltx::Limits,
    state: &mut Option<LaneMemory>,
) -> Result<FollowerReceipt> {
    validate_lane(lane)?;
    let directory = lane_directory(root, lane);
    let chunks = directory.join("chunks");
    let retired = directory.join("retired");
    if retired.exists() {
        let covered_through = read_watermark(&retired, "follower retire marker is invalid")?;
        return Ok(FollowerReceipt {
            base_sequence: covered_through.saturating_add(1),
            durable_through: covered_through,
        });
    }
    if state.is_none() {
        let retained = scan_lane(&chunks, lane, limits)?;
        *state = Some(LaneMemory {
            records: retained
                .into_iter()
                .map(|(sequence, record)| (sequence, record.digest))
                .collect(),
            open_first: None,
            open_last: None,
        });
    }
    let state = state
        .as_ref()
        .ok_or(Error::Node("follower lane state did not initialize"))?;
    let durable_through = state.records.keys().next_back().copied().unwrap_or(0);
    let base_sequence = state.records.keys().next().copied().unwrap_or(0);
    let marker = directory.join("sealed");
    if marker.exists() {
        let stored = read_watermark(&marker, "follower seal marker is invalid")?;
        if stored != durable_through {
            return Err(Error::Node("follower seal watermark differs"));
        }
    } else {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)?;
        file.write_all(&durable_through.to_le_bytes())?;
        file.sync_all()?;
        sync_directory(&directory)?;
    }
    Ok(FollowerReceipt {
        base_sequence,
        durable_through,
    })
}

fn retire_sync(
    root: &Path,
    lane: Lane,
    covered_through: u64,
    limits: crab_ltx::Limits,
) -> Result<FollowerReceipt> {
    validate_lane(lane)?;
    ensure_lane_directories(root, lane)?;
    let directory = lane_directory(root, lane);
    let chunks = directory.join("chunks");
    let retained = scan_lane(&chunks, lane, limits)?;
    let durable_through = retained.keys().next_back().copied().unwrap_or(0);
    if durable_through > covered_through {
        return Err(Error::Node("follower lane has uncovered records"));
    }
    let marker = directory.join("retired");
    if marker.exists() {
        if read_watermark(&marker, "follower retire marker is invalid")? != covered_through {
            return Err(Error::Node("follower retire watermark differs"));
        }
    } else {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)?;
        file.write_all(&covered_through.to_le_bytes())?;
        file.sync_all()?;
        sync_directory(&directory)?;
    }
    if chunks.exists() {
        std::fs::remove_dir_all(&chunks)?;
    }
    let sealed = directory.join("sealed");
    if sealed.exists() {
        std::fs::remove_file(sealed)?;
    }
    sync_directory(&directory)?;
    Ok(FollowerReceipt {
        base_sequence: covered_through.saturating_add(1),
        durable_through: covered_through,
    })
}

fn read_tail_sync(
    root: &Path,
    lane: Lane,
    first_sequence: u64,
    limits: crab_ltx::Limits,
    max_bytes: usize,
    max_frames: usize,
) -> Result<FollowerTailPage> {
    validate_lane(lane)?;
    let directory = lane_directory(root, lane);
    let marker = directory.join("sealed");
    if !marker.exists() {
        return Err(Error::Node("follower lane is not sealed"));
    }
    let retained = scan_lane(&directory.join("chunks"), lane, limits)?;
    let durable_through = retained.keys().next_back().copied().unwrap_or(0);
    if read_watermark(&marker, "follower seal marker is invalid")? != durable_through {
        return Err(Error::Node("follower seal watermark differs"));
    }
    if retained.is_empty() || first_sequence < *retained.keys().next().unwrap_or(&u64::MAX) {
        return Err(Error::Node("requested follower tail is not retained"));
    }
    if first_sequence > durable_through.saturating_add(1) {
        return Err(Error::Node(
            "requested follower tail is beyond durable data",
        ));
    }
    let mut frames = Vec::new();
    let mut bytes = 0_usize;
    let mut next_sequence = None;
    for (sequence, record) in retained.range(first_sequence..) {
        if frames.len() == max_frames
            || (!frames.is_empty() && bytes.saturating_add(record.length) > max_bytes)
        {
            next_sequence = Some(*sequence);
            break;
        }
        let mut file = std::fs::File::open(&record.path)?;
        file.seek(SeekFrom::Start(record.offset))?;
        let mut encoded = vec![0; record.length];
        file.read_exact(&mut encoded)?;
        // The scan verified framing and LTX; recheck bytes after seeking so
        // disk changes between validation and page materialization fail closed.
        if *blake3::hash(&encoded).as_bytes() != record.digest {
            return Err(Error::Node("stored follower record changed after scan"));
        }
        bytes = bytes.saturating_add(record.length);
        frames.push(Bytes::from(encoded));
    }
    Ok(FollowerTailPage {
        frames,
        next_sequence,
    })
}

#[derive(Clone)]
struct StoredRecord {
    sequence: u64,
    digest: [u8; 32],
    path: Arc<Path>,
    offset: u64,
    length: usize,
}

struct LaneMemory {
    records: BTreeMap<u64, [u8; 32]>,
    open_first: Option<u64>,
    open_last: Option<u64>,
}

fn scan_lane(
    chunks: &Path,
    lane: Lane,
    limits: crab_ltx::Limits,
) -> Result<BTreeMap<u64, StoredRecord>> {
    if !chunks.exists() {
        return Ok(BTreeMap::new());
    }
    let mut paths = std::fs::read_dir(chunks)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.sort();
    let mut records = BTreeMap::new();
    for path in paths {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(Error::Node("follower chunk name is not UTF-8"))?;
        let open = name == "open.log";
        let expected = if open {
            None
        } else {
            Some(parse_chunk_name(name).ok_or(Error::Node("invalid follower chunk name"))?)
        };
        let chunk = scan_chunk(&path, lane, limits, open)?;
        if let Some((first, last)) = expected
            && (chunk.first().map(|record| record.sequence) != Some(first)
                || chunk.last().map(|record| record.sequence) != Some(last))
        {
            return Err(Error::Node("follower chunk name differs from contents"));
        }
        for record in chunk {
            match records.entry(record.sequence) {
                Entry::Vacant(entry) => {
                    entry.insert(record);
                }
                Entry::Occupied(entry) if entry.get().digest == record.digest => {}
                Entry::Occupied(_) => {
                    return Err(Error::Node("conflicting stored follower frame"));
                }
            }
        }
    }
    if !records
        .keys()
        .copied()
        .collect::<Vec<_>>()
        .windows(2)
        .all(|pair| pair[0].checked_add(1) == Some(pair[1]))
    {
        return Err(Error::Node("stored follower lane has a sequence gap"));
    }
    Ok(records)
}

fn read_watermark(path: &Path, invalid: &'static str) -> Result<u64> {
    let bytes = std::fs::read(path)?;
    let watermark = bytes
        .as_slice()
        .try_into()
        .map(u64::from_le_bytes)
        .map_err(|_| Error::Node(invalid))?;
    Ok(watermark)
}

fn scan_chunk(
    path: &Path,
    lane: Lane,
    limits: crab_ltx::Limits,
    truncate_suffix: bool,
) -> Result<Vec<StoredRecord>> {
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .write(truncate_suffix)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut records = Vec::new();
    let mut valid_bytes = 0_u64;
    let path: Arc<Path> = Arc::from(path);
    loop {
        let mut header = [0_u8; RECORD_HEADER_BYTES];
        match file.read_exact(&mut header) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                return finish_scan(file, records, valid_bytes, truncate_suffix);
            }
            Err(error) => return Err(error.into()),
        }
        let parsed = parse_record_header(&header);
        let (sequence, length, digest) = match parsed {
            Ok(value) => value,
            Err(_error) if truncate_suffix => {
                file.set_len(valid_bytes)?;
                file.sync_data()?;
                return Ok(records);
            }
            Err(error) => return Err(error),
        };
        if length > limits.max_capture_bytes.saturating_add(240) || length > usize::MAX as u64 {
            if truncate_suffix {
                file.set_len(valid_bytes)?;
                file.sync_data()?;
                return Ok(records);
            }
            return Err(Error::Node("stored follower frame exceeds limit"));
        }
        let mut encoded = vec![0; length as usize];
        if let Err(error) = file.read_exact(&mut encoded) {
            if truncate_suffix && error.kind() == std::io::ErrorKind::UnexpectedEof {
                file.set_len(valid_bytes)?;
                file.sync_data()?;
                return Ok(records);
            }
            return Err(error.into());
        }
        let encoded = Bytes::from(encoded);
        let frame = match crab_ltx::inspect_node_frame(encoded.clone(), limits) {
            Ok(frame) if frame.digest() == digest => frame,
            Ok(_) | Err(_) if truncate_suffix => {
                file.set_len(valid_bytes)?;
                file.sync_data()?;
                return Ok(records);
            }
            Ok(_) => return Err(Error::Node("stored follower record digest differs")),
            Err(error) => return Err(error.into()),
        };
        let scope = frame.scope();
        if scope.node_sequence != sequence
            || scope.leader_session != *lane.leader.as_bytes()
            || scope.log_epoch != lane.epoch
        {
            if truncate_suffix {
                file.set_len(valid_bytes)?;
                file.sync_data()?;
                return Ok(records);
            }
            return Err(Error::Node("stored follower record scope differs"));
        }
        // Keep only verified locations, not every body in the lane. Restart,
        // seal and paged reads must not allocate the entire retained log.
        records.push(StoredRecord {
            sequence,
            digest,
            path: Arc::clone(&path),
            offset: valid_bytes + RECORD_HEADER_BYTES as u64,
            length: encoded.len(),
        });
        valid_bytes = valid_bytes
            .checked_add(RECORD_HEADER_BYTES as u64 + length)
            .ok_or(Error::Node("follower chunk length overflow"))?;
    }
}

fn finish_scan(
    mut file: std::fs::File,
    records: Vec<StoredRecord>,
    valid_bytes: u64,
    truncate_suffix: bool,
) -> Result<Vec<StoredRecord>> {
    if file.seek(SeekFrom::End(0))? != valid_bytes {
        if !truncate_suffix {
            return Err(Error::Node("sealed follower chunk has a torn suffix"));
        }
        file.set_len(valid_bytes)?;
        file.sync_data()?;
    }
    Ok(records)
}

fn write_record(
    file: &mut std::fs::File,
    sequence: u64,
    digest: [u8; 32],
    encoded: &[u8],
) -> Result<()> {
    file.write_all(RECORD_MAGIC)?;
    file.write_all(&sequence.to_le_bytes())?;
    file.write_all(&(encoded.len() as u64).to_le_bytes())?;
    file.write_all(&digest)?;
    file.write_all(encoded)?;
    Ok(())
}

fn parse_record_header(header: &[u8; RECORD_HEADER_BYTES]) -> Result<(u64, u64, [u8; 32])> {
    if &header[..4] != RECORD_MAGIC {
        return Err(Error::Node("invalid follower record magic"));
    }
    let sequence = u64::from_le_bytes(
        header[4..12]
            .try_into()
            .map_err(|_| Error::Node("invalid follower record sequence"))?,
    );
    let length = u64::from_le_bytes(
        header[12..20]
            .try_into()
            .map_err(|_| Error::Node("invalid follower record length"))?,
    );
    let digest = header[20..]
        .try_into()
        .map_err(|_| Error::Node("invalid follower record digest"))?;
    if sequence == 0 || length == 0 {
        return Err(Error::Node("invalid follower record header"));
    }
    Ok((sequence, length, digest))
}

fn open_append(path: &Path) -> Result<std::fs::File> {
    let existed = path.exists();
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(path)?;
    if !existed {
        let parent = path
            .parent()
            .ok_or(Error::Node("follower chunk has no parent"))?;
        sync_directory(parent)?;
    }
    Ok(file)
}

fn rotate_open(
    chunks: &Path,
    open_path: &Path,
    first: Option<u64>,
    last: Option<u64>,
) -> Result<()> {
    let (Some(first), Some(last)) = (first, last) else {
        return Err(Error::Node("cannot rotate an empty follower chunk"));
    };
    let destination = chunks.join(format!("{first:020}-{last:020}.log"));
    std::fs::rename(open_path, destination)?;
    sync_directory(chunks)?;
    Ok(())
}

fn prune_covered(chunks: &Path, covered_through: u64) -> Result<Option<u64>> {
    if !chunks.exists() {
        return Ok(None);
    }
    let mut removed = false;
    let mut pruned_through = None;
    for entry in std::fs::read_dir(chunks)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(Error::Node("follower chunk name is not UTF-8"));
        };
        if name == "open.log" {
            continue;
        }
        let Some((_, last)) = parse_chunk_name(name) else {
            return Err(Error::Node("invalid follower chunk name"));
        };
        if last <= covered_through {
            std::fs::remove_file(entry.path())?;
            removed = true;
            pruned_through = Some(pruned_through.map_or(last, |current: u64| current.max(last)));
        }
    }
    if removed {
        sync_directory(chunks)?;
    }
    Ok(pruned_through)
}

fn parse_chunk_name(name: &str) -> Option<(u64, u64)> {
    let value = name.strip_suffix(".log")?;
    let (first, last) = value.split_once('-')?;
    if first.len() != 20 || last.len() != 20 {
        return None;
    }
    Some((first.parse().ok()?, last.parse().ok()?))
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
