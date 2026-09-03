use std::fs::File;
use std::io::{Read as _, Write as _};

use tokio_util::sync::CancellationToken;

use super::*;
use crate::clean::{EntryKind, object_entry_kind};
use crate::private_fs::{FileStat, PinnedRoot, check_cancelled, with_pinned_root};

const OBJECT_FAMILIES: &[&str] = &["chunks", "xorbs", "shards"];
const MAX_CACHE_LRU_ENTRIES: usize = 1_000_000;

impl LocalCache {
    /// Prune eligible private objects to the configured LRU limits.
    pub async fn prune(&self) -> Result<PruneStats> {
        self.prune_with_options(PruneOptions::default()).await
    }

    /// Reclaim whole oldest objects toward `target_bytes`, stopping when enough is freed.
    ///
    /// Busy objects are skipped; totals count actual removals, not inventory sizes.
    pub async fn evict_bytes(&self, target_bytes: u64) -> Result<PruneStats> {
        if target_bytes == 0 {
            return Ok(PruneStats::default());
        }
        let root_path = self.root.clone();
        with_pinned_root(
            &self.root,
            &CancellationToken::new(),
            move |root, cancel| {
                let mut entries = collect_objects(root, cancel)?;
                entries.sort_unstable_by(|a, b| (a.modified, &a.path).cmp(&(b.modified, &b.path)));
                evict_oldest(
                    root,
                    &root_path,
                    &entries,
                    target_bytes,
                    PruneOptions::default(),
                    cancel,
                )
            },
        )
        .await
    }

    /// Prune eligible private objects, optionally previewing the same locked decisions.
    ///
    /// Unknown and busy entries are retained. Unsafe recognized paths fail closed.
    pub async fn prune_with_options(&self, options: PruneOptions) -> Result<PruneStats> {
        let root_path = self.root.clone();
        let large_max = self.chunk_max_bytes;
        let shard_max = self.shard_max_bytes;
        with_pinned_root(
            &self.root,
            &CancellationToken::new(),
            move |root, cancel| {
                let mut entries = collect_objects(root, cancel)?;
                entries.sort_unstable_by(|a, b| {
                    (a.kind == PruneObjectKind::Shard, a.modified, &a.path).cmp(&(
                        b.kind == PruneObjectKind::Shard,
                        b.modified,
                        &b.path,
                    ))
                });
                let boundary =
                    entries.partition_point(|entry| entry.kind != PruneObjectKind::Shard);
                let (large, shards) = entries.split_at(boundary);
                let target = total_bytes(large)?.saturating_sub(large_max);
                let mut stats = evict_oldest(root, &root_path, large, target, options, cancel)?;
                if let Some(max) = shard_max {
                    let target = total_bytes(shards)?.saturating_sub(max);
                    let shard_stats =
                        evict_oldest(root, &root_path, shards, target, options, cancel)?;
                    stats.shards_evicted = shard_stats.shards_evicted;
                    stats.bytes_freed = stats.bytes_freed.saturating_add(shard_stats.bytes_freed);
                    stats.entries.extend(shard_stats.entries);
                }
                Ok(stats)
            },
        )
        .await
    }

    /// Verify private chunks, shards, and xorbs, removing only proven corrupt entries.
    ///
    /// Unknown and busy entries are excluded from checked totals. Operational
    /// failures return errors; they do not authorize deletion. Manifests and
    /// workflow stages have logical keys and are not content-hash verified here.
    pub async fn verify(&self) -> Result<VerifyReport> {
        with_pinned_root(
            &self.root,
            &CancellationToken::new(),
            move |root, cancel| {
                let mut report = VerifyReport::default();
                visit_objects(root, OBJECT_FAMILIES, cancel, &mut |path, _| {
                    let Some(kind) = object_kind(path) else {
                        return Ok(());
                    };
                    let result = root.remove_file_if(path, false, &mut |file| {
                        let valid = verify_file(file, path, kind, cancel)?;
                        check_cancelled(cancel)?;
                        Ok(!valid)
                    });
                    let removed = match result {
                        Ok(removed) => removed.is_some(),
                        Err(CacheError::Io(error)) if unavailable(&error) => return Ok(()),
                        Err(error) => return Err(error),
                    };
                    if removed {
                        report.corrupt += 1;
                    } else {
                        report.valid += 1;
                    }
                    report.total += 1;
                    Ok(())
                })?;
                Ok(report)
            },
        )
        .await
    }

    /// Inspect recognized object payloads without opening or repairing cache state.
    ///
    /// Missing roots remain missing. Unknown paths, range files, and SQLite
    /// state are outside these family totals; unsafe recognized paths error.
    pub async fn stats(&self) -> Result<CacheStats> {
        with_pinned_root(&self.root, &CancellationToken::new(), |root, cancel| {
            let mut stats = CacheStats::default();
            visit_objects(
                root,
                &["chunks", "shards", "xorbs", "stages", "manifests"],
                cancel,
                &mut |path, metadata| {
                    let (bytes, count) = match family(path) {
                        Some("chunks") => (&mut stats.chunk_bytes, &mut stats.chunk_count),
                        Some("shards") => (&mut stats.shard_bytes, &mut stats.shard_count),
                        Some("xorbs") => (&mut stats.xorb_bytes, &mut stats.xorb_count),
                        Some("stages") => (&mut stats.stage_bytes, &mut stats.stage_count),
                        Some("manifests") => {
                            if path.extension().is_some_and(|ext| ext == "json") {
                                stats.manifest_count += 1;
                            }
                            return Ok(());
                        }
                        _ => return Ok(()),
                    };
                    *bytes = bytes
                        .checked_add(metadata.size)
                        .ok_or_else(|| CacheError::Internal("cache byte total overflow".into()))?;
                    *count += 1;
                    Ok(())
                },
            )?;
            Ok(stats)
        })
        .await
    }
}

struct ObjectEntry {
    path: PathBuf,
    size: u64,
    modified: u64,
    kind: PruneObjectKind,
}

fn collect_objects(root: &PinnedRoot, cancel: &CancellationToken) -> Result<Vec<ObjectEntry>> {
    let mut entries = Vec::new();
    visit_objects(root, OBJECT_FAMILIES, cancel, &mut |path, stat| {
        let Some(kind) = object_kind(path) else {
            return Ok(());
        };
        if entries.len() >= MAX_CACHE_LRU_ENTRIES {
            return Err(CacheError::Internal(format!(
                "cache contains more than {MAX_CACHE_LRU_ENTRIES} objects; refusing an unbounded LRU scan"
            )));
        }
        entries.push(ObjectEntry {
            path: path.to_owned(),
            size: stat.size,
            modified: stat.modified_ns,
            kind,
        });
        Ok(())
    })?;
    Ok(entries)
}

fn visit_objects(
    root: &PinnedRoot,
    families: &[&str],
    cancel: &CancellationToken,
    visitor: &mut dyn FnMut(&Path, FileStat) -> Result<()>,
) -> Result<()> {
    let kind = |path: &Path| {
        let Some(parts) = path
            .iter()
            .map(|name| name.to_str())
            .collect::<Option<Vec<_>>>()
        else {
            return EntryKind::Retain;
        };
        object_entry_kind(&parts)
    };
    root.visit_selected_files(
        &|path| {
            check_cancelled(cancel)?;
            Ok(
                family(path).is_some_and(|family| families.contains(&family))
                    && !matches!(kind(path), EntryKind::Retain),
            )
        },
        &mut |path, metadata| {
            if matches!(kind(path), EntryKind::Payload) {
                visitor(path, metadata)?;
            }
            Ok(())
        },
    )
}

fn family(path: &Path) -> Option<&str> {
    path.iter().next().and_then(|name| name.to_str())
}

fn object_kind(path: &Path) -> Option<PruneObjectKind> {
    match family(path) {
        Some("chunks") => Some(PruneObjectKind::Chunk),
        Some("shards") => Some(PruneObjectKind::Shard),
        Some("xorbs") => Some(PruneObjectKind::Xorb),
        _ => None,
    }
}

fn object_hash(path: &Path) -> Result<MerkleHash> {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| MerkleHash::from_hex(name).ok())
        .ok_or_else(|| CacheError::UnsafeRoot {
            path: path.display().to_string(),
            reason: "object has no content-addressed filename".into(),
        })
}

fn total_bytes(entries: &[ObjectEntry]) -> Result<u64> {
    entries.iter().try_fold(0u64, |total, entry| {
        total
            .checked_add(entry.size)
            .ok_or_else(|| CacheError::Internal("cache byte total overflow".into()))
    })
}

fn unavailable(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::WouldBlock
    )
}

fn evict_oldest(
    root: &PinnedRoot,
    root_path: &Path,
    entries: &[ObjectEntry],
    target: u64,
    options: PruneOptions,
    cancel: &CancellationToken,
) -> Result<PruneStats> {
    let mut stats = PruneStats::default();
    for entry in entries {
        check_cancelled(cancel)?;
        if stats.bytes_freed >= target {
            break;
        }
        let removed = root.remove_file_if(&entry.path, options.dry_run, &mut |_| {
            check_cancelled(cancel)?;
            Ok(true)
        });
        let bytes = match removed {
            Ok(Some(bytes)) => bytes,
            Ok(None) => continue,
            Err(CacheError::Io(error)) if unavailable(&error) => continue,
            Err(error) => return Err(error),
        };
        match entry.kind {
            PruneObjectKind::Chunk => stats.chunks_evicted += 1,
            PruneObjectKind::Shard => stats.shards_evicted += 1,
            PruneObjectKind::Xorb => stats.xorbs_evicted += 1,
        }
        stats.bytes_freed = stats.bytes_freed.saturating_add(bytes);
        if options.record_entries {
            stats.entries.push(PrunedCacheObject {
                kind: entry.kind,
                path: root_path.join(&entry.path),
                bytes,
            });
        }
    }
    Ok(stats)
}

fn verify_file(
    file: &mut File,
    path: &Path,
    kind: PruneObjectKind,
    cancel: &CancellationToken,
) -> Result<bool> {
    let expected = object_hash(path)?;
    let bytes = file.metadata()?.len();
    if kind == PruneObjectKind::Xorb {
        return match super::xorb_file::verify(file, path, bytes, &expected, cancel) {
            Ok(()) => Ok(true),
            Err(CacheError::CorruptObject { .. } | CacheError::HashMismatch { .. }) => Ok(false),
            Err(CacheError::Xet {
                source: crab_xet::error::XetError::Decompress { .. },
            }) => Ok(false),
            Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                Ok(false)
            }
            Err(error) => Err(error),
        };
    }
    let limit = if kind == PruneObjectKind::Chunk {
        MAX_CACHE_CHUNK_BYTES
    } else {
        MAX_CACHE_SHARD_BYTES
    };
    if bytes > limit {
        return Ok(false);
    }
    let mut hashed = crab_xet::hash::HashedWrite::new(std::io::sink());
    let mut buffer = vec![0; 64 * 1024];
    let mut remaining = bytes;
    loop {
        check_cancelled(cancel)?;
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let Some(rest) = remaining.checked_sub(read as u64) else {
            return Ok(false);
        };
        remaining = rest;
        hashed.write_all(&buffer[..read])?;
    }
    Ok(remaining == 0 && file.metadata()?.len() == bytes && hashed.hash() == expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operational_read_failures_are_not_corruption() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let hash = compute_data_hash(b"data").hex();
        let path = temp.path().join(hash);
        std::fs::write(&path, vec![0; 128])?;
        for kind in [
            PruneObjectKind::Chunk,
            PruneObjectKind::Shard,
            PruneObjectKind::Xorb,
        ] {
            let mut file = File::options().write(true).open(&path)?;
            let result = verify_file(&mut file, &path, kind, &CancellationToken::new());
            assert!(matches!(result, Err(CacheError::Io(_))));
            assert!(path.exists());
        }
        Ok(())
    }
}
