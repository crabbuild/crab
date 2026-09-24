//! Follower directory layout, quarantine, and retired lanes.

use super::*;

pub(super) fn directory_bytes(path: &Path) -> Result<u64> {
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

pub(super) fn follower_bytes(root: &Path) -> Result<u64> {
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

pub(super) fn scrub_followers(root: &Path, limits: crab_ltx::Limits) -> Result<()> {
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

pub(super) fn quarantine_entry_count(root: &Path) -> Result<usize> {
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

pub(super) fn retired_lanes_sync(
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

pub(super) fn remove_retired_sync(
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

pub(super) fn settle_disk_reservation(
    result: Result<FollowerReceipt>,
    resize: Result<()>,
) -> Result<FollowerReceipt> {
    // Reconcile the shared admission even when the filesystem operation
    // failed.  Returning early on `result` would retain the preflight growth
    // forever and eventually make unrelated Cells fail closed for capacity.
    resize?;
    result
}

pub(super) fn lane_directory(root: &Path, lane: Lane) -> PathBuf {
    root.join("followers")
        .join(hex(lane.leader.as_bytes()))
        .join(lane.epoch.to_string())
}

pub(super) fn ensure_lane_directories(root: &Path, lane: Lane) -> Result<()> {
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

pub(super) fn validate_lane(lane: Lane) -> Result<()> {
    if lane.leader.as_bytes().iter().all(|byte| *byte == 0) || lane.epoch == 0 {
        return Err(Error::Node("invalid follower lane"));
    }
    Ok(())
}

pub(super) fn sync_directory(path: &Path) -> std::io::Result<()> {
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
