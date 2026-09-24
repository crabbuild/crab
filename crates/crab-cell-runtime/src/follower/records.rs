//! Follower lane record framing, appends, tails, and scans.
//!
//! A lane is a directory of chunk files; every helper here encodes or
//! decodes those records, reconciles the admission that reserved their
//! bytes, and never lets a torn or mismatched tail authorize a receipt.

use super::*;

pub(super) fn append_sync(
    root: &Path,
    lane: Lane,
    frames: Vec<Bytes>,
    covered_through: u64,
    limits: crab_ltx::Limits,
    index_used: &Arc<Mutex<u64>>,
    state: &mut Option<LaneMemory>,
    scan_counter: &ScanCounter,
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
        let retained = scan_lane_counted(&chunks, lane, limits, scan_counter)?;
        let open_records = scan_chunk(&chunks.join("open.log"), lane, limits, true)?;
        *state = Some(lane_memory(retained, &open_records, index_used)?);
    }
    let pruned_through = prune_covered(&chunks, lane, covered_through, limits)?;
    let state = state
        .as_mut()
        .ok_or(Error::Node("follower lane state did not initialize"))?;
    if pruned_through.is_some() {
        let records = scan_lane_counted(&chunks, lane, limits, scan_counter)?;
        let index_bytes = u64::try_from(records.len())
            .map_err(|_| Error::Capacity("follower lane index"))?
            .checked_mul(INDEX_BYTES_PER_RECORD)
            .ok_or(Error::Capacity("follower lane index"))?;
        state.index.resize_to(index_bytes)?;
        state.records = records;
        let open_records = scan_chunk(&chunks.join("open.log"), lane, limits, true)?;
        state.open_first = open_records.first().map(|record| record.sequence);
        state.open_last = open_records.last().map(|record| record.sequence);
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
    let mut pending: Vec<StoredRecord> = Vec::new();
    let mut pending_digests = HashMap::new();
    let mut open_first = state.open_first;
    let mut open_last = state.open_last;
    let frame_count =
        u64::try_from(frames.len()).map_err(|_| Error::Capacity("follower lane index"))?;
    state.index.grow(
        frame_count
            .checked_mul(INDEX_BYTES_PER_RECORD)
            .ok_or(Error::Capacity("follower lane index"))?,
    )?;
    for encoded in frames {
        let frame = crab_ltx::inspect_node_frame(encoded.clone(), limits)?;
        let scope = frame.scope();
        if scope.leader_session != *lane.leader.as_bytes() || scope.log_epoch != lane.epoch {
            return Err(Error::Node("follower frame changed lane scope"));
        }
        let sequence = scope.node_sequence;
        let digest = frame.digest();
        if let Some(existing) = state.records.get(&sequence) {
            if existing.digest != digest {
                return Err(Error::Node("conflicting duplicate follower frame"));
            }
            continue;
        }
        if let Some(existing) = pending_digests.get(&sequence)
            && *existing != digest
        {
            return Err(Error::Node("conflicting duplicate follower frame"));
        }
        if pending_digests.contains_key(&sequence) {
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
            let destination = rotate_open(&chunks, &open_path, open_first, open_last)?;
            relocate_records(
                &mut state.records,
                open_first.ok_or(Error::Node("follower open range is missing"))?,
                open_last.ok_or(Error::Node("follower open range is missing"))?,
                &destination,
            );
            let destination: Arc<Path> = Arc::from(destination.as_path());
            for record in &mut pending {
                record.path = Arc::clone(&destination);
            }
            file = open_append(&open_path)?;
            open_first = None;
        }
        let offset = file.metadata()?.len();
        write_record(&mut file, sequence, digest, &encoded)?;
        open_first.get_or_insert(sequence);
        open_last = Some(sequence);
        durable_through = sequence;
        pending_digests.insert(sequence, digest);
        pending.push(StoredRecord {
            sequence,
            digest,
            path: Arc::from(open_path.as_path()),
            offset: offset
                .checked_add(RECORD_HEADER_BYTES as u64)
                .ok_or(Error::Node("follower record offset overflow"))?,
            length: encoded.len(),
        });
    }
    if !pending.is_empty() {
        file.sync_data()?;
        state
            .records
            .extend(pending.into_iter().map(|record| (record.sequence, record)));
        state.open_first = open_first;
        state.open_last = open_last;
    }
    let index_bytes = u64::try_from(state.records.len())
        .map_err(|_| Error::Capacity("follower lane index"))?
        .checked_mul(INDEX_BYTES_PER_RECORD)
        .ok_or(Error::Capacity("follower lane index"))?;
    state.index.shrink_to(index_bytes);
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

pub(super) fn seal_sync(
    root: &Path,
    lane: Lane,
    limits: crab_ltx::Limits,
    index_used: &Arc<Mutex<u64>>,
    state: &mut Option<LaneMemory>,
    scan_counter: &ScanCounter,
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
        let retained = scan_lane_counted(&chunks, lane, limits, scan_counter)?;
        *state = Some(lane_memory(retained, &[], index_used)?);
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

pub(super) fn retire_sync(
    root: &Path,
    lane: Lane,
    covered_through: u64,
    limits: crab_ltx::Limits,
    scan_counter: &ScanCounter,
) -> Result<FollowerReceipt> {
    validate_lane(lane)?;
    ensure_lane_directories(root, lane)?;
    let directory = lane_directory(root, lane);
    let chunks = directory.join("chunks");
    let retained = scan_lane_counted(&chunks, lane, limits, scan_counter)?;
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

#[expect(
    clippy::too_many_arguments,
    reason = "keeps tail bounds, cache ownership, and scan instrumentation explicit"
)]
pub(super) fn read_tail_sync(
    root: &Path,
    lane: Lane,
    first_sequence: u64,
    limits: crab_ltx::Limits,
    index_used: &Arc<Mutex<u64>>,
    state: &mut Option<LaneMemory>,
    max_bytes: usize,
    max_frames: usize,
    scan_counter: &ScanCounter,
) -> Result<FollowerTailPage> {
    validate_lane(lane)?;
    let directory = lane_directory(root, lane);
    let marker = directory.join("sealed");
    if !marker.exists() {
        return Err(Error::Node("follower lane is not sealed"));
    }
    if state.is_none() {
        let retained = scan_lane_counted(&directory.join("chunks"), lane, limits, scan_counter)?;
        let open_records = scan_chunk(&directory.join("chunks/open.log"), lane, limits, true)?;
        let scan_only = retained.clone();
        match lane_memory(retained, &open_records, index_used) {
            Ok(memory) => *state = Some(memory),
            Err(Error::Capacity(_)) => {
                return read_tail_records(
                    root,
                    lane,
                    first_sequence,
                    limits,
                    &scan_only,
                    max_bytes,
                    max_frames,
                );
            }
            Err(error) => return Err(error),
        }
    }
    let state = state
        .as_ref()
        .ok_or(Error::Node("follower lane state did not initialize"))?;
    read_tail_records(
        root,
        lane,
        first_sequence,
        limits,
        &state.records,
        max_bytes,
        max_frames,
    )
}

pub(super) fn read_tail_records(
    root: &Path,
    lane: Lane,
    first_sequence: u64,
    limits: crab_ltx::Limits,
    records: &BTreeMap<u64, StoredRecord>,
    max_bytes: usize,
    max_frames: usize,
) -> Result<FollowerTailPage> {
    let directory = lane_directory(root, lane);
    let marker = directory.join("sealed");
    let durable_through = records.keys().next_back().copied().unwrap_or(0);
    if read_watermark(&marker, "follower seal marker is invalid")? != durable_through {
        return Err(Error::Node("follower seal watermark differs"));
    }
    if records.is_empty() || first_sequence < *records.keys().next().unwrap_or(&u64::MAX) {
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
    for (sequence, record) in records.range(first_sequence..) {
        if frames.len() == max_frames
            || (!frames.is_empty() && bytes.saturating_add(record.length) > max_bytes)
        {
            next_sequence = Some(*sequence);
            break;
        }
        let encoded = read_indexed_record(root, lane, record, limits)?;
        bytes = bytes.saturating_add(record.length);
        frames.push(encoded);
    }
    Ok(FollowerTailPage {
        frames,
        next_sequence,
    })
}

pub(super) fn read_indexed_record(
    root: &Path,
    lane: Lane,
    record: &StoredRecord,
    limits: crab_ltx::Limits,
) -> Result<Bytes> {
    let chunks = lane_directory(root, lane).join("chunks");
    if record.path.parent() != Some(chunks.as_path())
        || !std::fs::symlink_metadata(&record.path)?
            .file_type()
            .is_file()
    {
        return Err(Error::Node("indexed follower record path is invalid"));
    }
    let header_offset = record
        .offset
        .checked_sub(RECORD_HEADER_BYTES as u64)
        .ok_or(Error::Node("indexed follower record offset is invalid"))?;
    let mut file = std::fs::File::open(&record.path)?;
    file.seek(SeekFrom::Start(header_offset))?;
    let mut header = [0_u8; RECORD_HEADER_BYTES];
    file.read_exact(&mut header)?;
    let (sequence, length, digest) = parse_record_header(&header)?;
    if sequence != record.sequence || length != record.length as u64 || digest != record.digest {
        return Err(Error::Node("indexed follower record header changed"));
    }
    let mut encoded = vec![0; record.length];
    file.read_exact(&mut encoded)?;
    if *blake3::hash(&encoded).as_bytes() != record.digest {
        return Err(Error::Node("stored follower record changed after index"));
    }
    let frame = crab_ltx::inspect_node_frame(Bytes::from(encoded.clone()), limits)?;
    let scope = frame.scope();
    if scope.node_sequence != record.sequence
        || scope.leader_session != *lane.leader.as_bytes()
        || scope.log_epoch != lane.epoch
        || frame.digest() != record.digest
    {
        return Err(Error::Node("indexed follower record scope changed"));
    }
    Ok(Bytes::from(encoded))
}

#[derive(Clone)]
pub(super) struct StoredRecord {
    sequence: u64,
    digest: [u8; 32],
    path: Arc<Path>,
    offset: u64,
    length: usize,
}

pub(super) struct IndexReservation {
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

pub(super) struct LaneMemory {
    records: BTreeMap<u64, StoredRecord>,
    open_first: Option<u64>,
    open_last: Option<u64>,
    index: IndexReservation,
}

pub(super) fn lane_memory(
    records: BTreeMap<u64, StoredRecord>,
    open_records: &[StoredRecord],
    index_used: &Arc<Mutex<u64>>,
) -> Result<LaneMemory> {
    let record_count =
        u64::try_from(records.len()).map_err(|_| Error::Capacity("follower lane index"))?;
    let bytes = record_count
        .checked_mul(INDEX_BYTES_PER_RECORD)
        .ok_or(Error::Capacity("follower lane index"))?;
    Ok(LaneMemory {
        open_first: open_records.first().map(|record| record.sequence),
        open_last: open_records.last().map(|record| record.sequence),
        index: IndexReservation::new(index_used, bytes)?,
        records,
    })
}

pub(super) fn scan_lane(
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

pub(super) fn scan_lane_counted(
    chunks: &Path,
    lane: Lane,
    limits: crab_ltx::Limits,
    counter: &ScanCounter,
) -> Result<BTreeMap<u64, StoredRecord>> {
    count_scan(counter);
    scan_lane(chunks, lane, limits)
}

pub(super) fn read_watermark(path: &Path, invalid: &'static str) -> Result<u64> {
    let bytes = std::fs::read(path)?;
    let watermark = bytes
        .as_slice()
        .try_into()
        .map(u64::from_le_bytes)
        .map_err(|_| Error::Node(invalid))?;
    Ok(watermark)
}

pub(super) fn scan_chunk(
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

pub(super) fn finish_scan(
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

pub(super) fn write_record(
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

pub(super) fn parse_record_header(
    header: &[u8; RECORD_HEADER_BYTES],
) -> Result<(u64, u64, [u8; 32])> {
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

pub(super) fn open_append(path: &Path) -> Result<std::fs::File> {
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

pub(super) fn rotate_open(
    chunks: &Path,
    open_path: &Path,
    first: Option<u64>,
    last: Option<u64>,
) -> Result<PathBuf> {
    let (Some(first), Some(last)) = (first, last) else {
        return Err(Error::Node("cannot rotate an empty follower chunk"));
    };
    let destination = chunks.join(format!("{first:020}-{last:020}.log"));
    std::fs::rename(open_path, &destination)?;
    sync_directory(chunks)?;
    Ok(destination)
}

pub(super) fn relocate_records(
    records: &mut BTreeMap<u64, StoredRecord>,
    first: u64,
    last: u64,
    destination: &Path,
) {
    let path: Arc<Path> = Arc::from(destination);
    for record in records.range_mut(first..=last).map(|(_, record)| record) {
        record.path = Arc::clone(&path);
    }
}

pub(super) fn prune_covered(
    chunks: &Path,
    lane: Lane,
    covered_through: u64,
    limits: crab_ltx::Limits,
) -> Result<Option<u64>> {
    if !chunks.exists() {
        return Ok(None);
    }
    let mut removed = false;
    let mut open_removed = false;
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
    let open_path = chunks.join("open.log");
    if open_path.exists() {
        let records = scan_chunk(&open_path, lane, limits, true)?;
        let mut retained = Vec::with_capacity(records.len());
        for record in records {
            if record.sequence <= covered_through {
                removed = true;
                open_removed = true;
                pruned_through = Some(
                    pruned_through
                        .map_or(record.sequence, |current: u64| current.max(record.sequence)),
                );
            } else {
                retained.push(record);
            }
        }
        if open_removed {
            rewrite_open_chunk(&open_path, retained)?;
        }
    }
    if removed {
        sync_directory(chunks)?;
    }
    Ok(pruned_through)
}

pub(super) fn rewrite_open_chunk(path: &Path, records: Vec<StoredRecord>) -> Result<()> {
    let parent = path
        .parent()
        .ok_or(Error::Node("follower open chunk has no parent"))?;
    let temporary = parent.join("open.log.tmp");
    if temporary.exists() {
        std::fs::remove_file(&temporary)?;
    }
    if records.is_empty() {
        std::fs::remove_file(path)?;
        sync_directory(parent)?;
        return Ok(());
    }

    // Covered prefixes are rewritten through a synced temporary and atomically
    // renamed so a restart sees either the old contiguous lane or the new one.
    let result = (|| {
        let mut source = std::fs::File::open(path)?;
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        for record in records {
            source.seek(SeekFrom::Start(record.offset))?;
            let mut encoded = vec![0; record.length];
            source.read_exact(&mut encoded)?;
            if *blake3::hash(&encoded).as_bytes() != record.digest {
                return Err(Error::Node("stored follower record changed during prune"));
            }
            write_record(&mut output, record.sequence, record.digest, &encoded)?;
        }
        output.sync_data()?;
        drop(output);
        std::fs::rename(&temporary, path)?;
        sync_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

pub(super) fn parse_chunk_name(name: &str) -> Option<(u64, u64)> {
    let value = name.strip_suffix(".log")?;
    let (first, last) = value.split_once('-')?;
    if first.len() != 20 || last.len() != 20 {
        return None;
    }
    Some((first.parse().ok()?, last.parse().ok()?))
}
