//! Lane appends, seals, retirements, and chunk rewrites.

use super::*;

pub(in crate::follower) fn append_sync(
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
pub(in crate::follower) fn seal_sync(
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
pub(in crate::follower) fn retire_sync(
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
pub(in crate::follower) fn write_record(
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
pub(in crate::follower) fn parse_record_header(
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
pub(in crate::follower) fn open_append(path: &Path) -> Result<std::fs::File> {
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
pub(in crate::follower) fn rotate_open(
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
pub(in crate::follower) fn relocate_records(
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
pub(in crate::follower) fn prune_covered(
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
pub(in crate::follower) fn rewrite_open_chunk(
    path: &Path,
    records: Vec<StoredRecord>,
) -> Result<()> {
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
pub(in crate::follower) fn parse_chunk_name(name: &str) -> Option<(u64, u64)> {
    let value = name.strip_suffix(".log")?;
    let (first, last) = value.split_once('-')?;
    if first.len() != 20 || last.len() != 20 {
        return None;
    }
    Some((first.parse().ok()?, last.parse().ok()?))
}
