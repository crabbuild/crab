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

/// Local SSD store for checksum-verified follower fragments.
///
/// The bytes are durability obligations until the leader's contiguous object
/// watermark permits whole-chunk deletion. They are never cache-evicted.
#[derive(Clone)]
pub struct FollowerStore {
    root: PathBuf,
    limits: crab_ltx::Limits,
    lanes: LaneMap,
}

impl FollowerStore {
    /// Opens a follower root. The caller must place it on durable local SSD.
    pub fn open(root: PathBuf, limits: crab_ltx::Limits) -> Result<Self> {
        let existed = root.exists();
        std::fs::create_dir_all(&root).map_err(crab_ltx::CrabError::from)?;
        if !existed {
            let parent = root
                .parent()
                .ok_or(Error::Node("follower root has no parent"))?;
            sync_directory(parent).map_err(crab_ltx::CrabError::from)?;
        }
        sync_directory(&root).map_err(crab_ltx::CrabError::from)?;
        Ok(Self {
            root,
            limits,
            lanes: Arc::new(Mutex::new(HashMap::new())),
        })
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
        tokio::task::spawn_blocking(move || {
            let mut state = lock
                .lock()
                .map_err(|_| Error::Node("follower lane lock poisoned"))?;
            let result = append_sync(&root, lane, frames, covered_through, limits, &mut state);
            if result.is_err() {
                *state = None;
            }
            result
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
        tokio::task::spawn_blocking(move || {
            let mut state = lock
                .lock()
                .map_err(|_| Error::Node("follower lane lock poisoned"))?;
            let result = seal_sync(&root, lane, limits, &mut state);
            if result.is_err() {
                *state = None;
            }
            result
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
            read_tail_sync(&root, lane, first_sequence, limits)
        })
        .await
        .map_err(Error::FollowerWorkerJoin)?
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
        let stored = read_sealed_marker(&marker)?;
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

fn read_tail_sync(
    root: &Path,
    lane: Lane,
    first_sequence: u64,
    limits: crab_ltx::Limits,
) -> Result<Vec<Bytes>> {
    validate_lane(lane)?;
    let directory = lane_directory(root, lane);
    let marker = directory.join("sealed");
    if !marker.exists() {
        return Err(Error::Node("follower lane is not sealed"));
    }
    let retained = scan_lane(&directory.join("chunks"), lane, limits)?;
    let durable_through = retained.keys().next_back().copied().unwrap_or(0);
    if read_sealed_marker(&marker)? != durable_through {
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
    Ok(retained
        .range(first_sequence..)
        .map(|(_, record)| record.encoded.clone())
        .collect())
}

#[derive(Clone)]
struct StoredRecord {
    sequence: u64,
    digest: [u8; 32],
    encoded: Bytes,
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

fn read_sealed_marker(path: &Path) -> Result<u64> {
    let bytes = std::fs::read(path)?;
    let watermark = bytes
        .as_slice()
        .try_into()
        .map(u64::from_le_bytes)
        .map_err(|_| Error::Node("follower seal marker is invalid"))?;
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
        valid_bytes = valid_bytes
            .checked_add(RECORD_HEADER_BYTES as u64 + length)
            .ok_or(Error::Node("follower chunk length overflow"))?;
        records.push(StoredRecord {
            sequence,
            digest,
            encoded,
        });
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
