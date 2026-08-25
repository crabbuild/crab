//! Shard-batched resolver for bulk hydration.
//!
//! Groups file paths by the shard that holds their reconstruction
//! metadata, pre-loads each shard once, and resolves all files in that
//! batch before moving to the next shard. For N files scattered across
//! M shards, this reduces shard opens from N to ~M — a 20× speedup
//! for 1 000 files across 50 shards on a cold cache.
//!
//! Builds on the existing shard-bloom pre-filter
//! ([`crab_metadata::bloom_prefilter`]) to skip shards that
//! definitely don't contain a queried file hash.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::cache::CacheKey;
use crate::cache::LocalCache;
use crate::core::error::{CrabError, Result};

const MAX_BATCH_SHARD_BYTES: u64 = 512 * 1024 * 1024;
use crab_cache_store::CachingStore;
use crab_metadata::bloom_prefilter::{self, BloomCheck};
use crab_types::pointer::Pointer;
use crab_xet::hash::MerkleHash;
use crab_xet::shard::MDBFileInfo;
use crab_xet::shard::ShardReader;

type StoreLayout = crab_storage::StoreLayout<crab_storage::Store>;

/// Result of hydrating a single file via the batch resolver.
#[derive(Debug)]
pub struct HydrateResult {
    /// The path that was requested for hydration.
    pub path: PathBuf,
    /// Outcome of the hydration attempt.
    pub outcome: HydrateOutcome,
}

/// Per-file outcome from the batch resolver.
#[derive(Debug)]
pub enum HydrateOutcome {
    /// File was successfully resolved and its reconstruction terms are
    /// available. Contains the shard hash and the file's reconstruction
    /// info from the shard metadata.
    Resolved {
        /// Hash of the shard containing this file's reconstruction terms.
        shard_hash: MerkleHash,
        /// The file's reconstruction segments from the shard.
        file_info: MDBFileInfo,
    },
    /// File was already hydrated on disk — skipped.
    Skipped,
    /// Resolution failed for this file.
    Failed {
        /// Human-readable reason for the failure.
        reason: String,
    },
}

/// Internal grouping: a file awaiting resolution, keyed by its shard.
struct PendingFile {
    /// Index into the original `paths` slice for result ordering.
    index: usize,
    /// Absolute path on disk.
    path: PathBuf,
    /// Parsed pointer for this file.
    pointer: Pointer,
    /// The file's content hash (derived from the pointer).
    file_hash: MerkleHash,
}

/// Resolve a batch of file paths through the shard-batched pipeline.
///
/// Steps:
/// 1. Parse each path's pointer to extract `file_hash`.
/// 2. Resolve `file_hash → shard_hash` via the file-index (with
///    shard-hint fast path when available).
/// 3. Group files by `shard_hash`.
/// 4. For each shard batch, download the shard once and look up all
///    files in that batch.
/// 5. Return per-file results preserving the original input order.
///
/// Files that are already hydrated on disk are skipped. Files whose
/// pointer cannot be parsed or whose shard cannot be resolved are
/// reported as failed without aborting the batch.
pub async fn hydrate_batch(
    store: &CachingStore,
    router: &StoreLayout,
    cache: &Arc<LocalCache>,
    paths: &[PathBuf],
    pointers: &[Pointer],
) -> Result<Vec<HydrateResult>> {
    if paths.len() != pointers.len() {
        return Err(CrabError::Internal(
            "hydrate_batch: paths and pointers must have the same length".to_string(),
        ));
    }

    let file_count = paths.len();
    info!(file_count, "batch resolver: starting");

    let mut results: Vec<Option<HydrateResult>> = (0..file_count).map(|_| None).collect();

    // Step 1: Build pending file list, skipping already-hydrated files.
    let mut pending: Vec<PendingFile> = Vec::with_capacity(file_count);
    for (i, (path, ptr)) in paths.iter().zip(pointers.iter()).enumerate() {
        if is_already_hydrated(path, ptr) {
            debug!(path = %path.display(), "batch: already hydrated, skipping");
            results[i] = Some(HydrateResult {
                path: path.clone(),
                outcome: HydrateOutcome::Skipped,
            });
            continue;
        }

        let file_hash = MerkleHash::from(ptr.file_hash);
        pending.push(PendingFile {
            index: i,
            path: path.clone(),
            pointer: ptr.clone(),
            file_hash,
        });
    }

    if pending.is_empty() {
        info!("batch resolver: all files already hydrated");
        return Ok(results.into_iter().flatten().collect());
    }

    // Step 2: Resolve file_hash → shard_hash for each pending file.
    // Use shard-hint fast path when available, then batch all file-index
    // fallbacks through one read-only MetaDb session.
    let mut shard_groups: HashMap<MerkleHash, Vec<usize>> = HashMap::new();
    let mut file_index_fallbacks: Vec<(usize, MerkleHash)> = Vec::new();

    for (pi, pf) in pending.iter().enumerate() {
        if let Some(shard_hash) = try_shard_hint(store, router, &pf.pointer, &pf.file_hash).await {
            shard_groups.entry(shard_hash).or_default().push(pi);
        } else {
            file_index_fallbacks.push((pi, pf.file_hash));
        }
    }

    if !file_index_fallbacks.is_empty() {
        let fallback_hashes: Vec<MerkleHash> =
            file_index_fallbacks.iter().map(|(_, hash)| *hash).collect();
        match resolve_file_index_batch(store, router, &fallback_hashes).await {
            Ok(hits) => {
                for ((pi, file_hash), hit) in file_index_fallbacks.iter().zip(hits.into_iter()) {
                    let pf = &pending[*pi];
                    if let Some(shard_hash) = hit {
                        shard_groups.entry(shard_hash).or_default().push(*pi);
                    } else {
                        warn!(
                            path = %pf.path.display(),
                            file_hash = %file_hash.hex(),
                            "batch: file-index miss for file"
                        );
                        results[pf.index] = Some(HydrateResult {
                            path: pf.path.clone(),
                            outcome: HydrateOutcome::Failed {
                                reason: format!(
                                    "shard resolution failed: file_index:{}",
                                    file_hash.hex()
                                ),
                            },
                        });
                    }
                }
            }
            Err(e) => {
                for (pi, file_hash) in &file_index_fallbacks {
                    let pf = &pending[*pi];
                    warn!(
                        path = %pf.path.display(),
                        file_hash = %file_hash.hex(),
                        error = %e,
                        "batch: failed to resolve shard for file"
                    );
                    results[pf.index] = Some(HydrateResult {
                        path: pf.path.clone(),
                        outcome: HydrateOutcome::Failed {
                            reason: format!("shard resolution failed: {e}"),
                        },
                    });
                }
            }
        }
    }

    let shard_count = shard_groups.len();
    let resolved_count = shard_groups.values().map(Vec::len).sum::<usize>();
    info!(
        resolved_count,
        shard_count, "batch resolver: grouped files by shard"
    );

    // Step 3: For each shard batch, download the shard once and resolve
    // all files in that batch.
    for (shard_hash, pending_indices) in &shard_groups {
        debug!(
            shard_hash = %shard_hash.hex(),
            file_count = pending_indices.len(),
            "batch: processing shard batch"
        );

        let shard = match get_or_download_shard(store, router, cache, shard_hash).await {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    shard_hash = %shard_hash.hex(),
                    error = %e,
                    "batch: failed to download shard"
                );
                for &pi in pending_indices {
                    let pf = &pending[pi];
                    results[pf.index] = Some(HydrateResult {
                        path: pf.path.clone(),
                        outcome: HydrateOutcome::Failed {
                            reason: format!("shard download failed: {e}"),
                        },
                    });
                }
                continue;
            }
        };

        // Look up each file in this shard.
        for &pi in pending_indices {
            let pf = &pending[pi];
            match shard.get_file_info(&pf.file_hash) {
                Ok(Some(file_info)) => {
                    results[pf.index] = Some(HydrateResult {
                        path: pf.path.clone(),
                        outcome: HydrateOutcome::Resolved {
                            shard_hash: *shard_hash,
                            file_info,
                        },
                    });
                }
                Ok(None) => {
                    warn!(
                        path = %pf.path.display(),
                        file_hash = %pf.file_hash.hex(),
                        shard_hash = %shard_hash.hex(),
                        "batch: file not found in shard despite file-index mapping"
                    );
                    results[pf.index] = Some(HydrateResult {
                        path: pf.path.clone(),
                        outcome: HydrateOutcome::Failed {
                            reason: format!(
                                "file {} not found in shard {} (stale file-index?)",
                                pf.file_hash.hex(),
                                shard_hash.hex(),
                            ),
                        },
                    });
                }
                Err(e) => {
                    results[pf.index] = Some(HydrateResult {
                        path: pf.path.clone(),
                        outcome: HydrateOutcome::Failed {
                            reason: format!("shard lookup error: {e}"),
                        },
                    });
                }
            }
        }
    }

    // Collect xorb hashes across all resolved files for grouped fetching.
    let xorb_set = collect_xorb_hashes(&results);
    if !xorb_set.is_empty() {
        debug!(
            unique_xorbs = xorb_set.len(),
            "batch resolver: unique xorbs across all resolved files"
        );
    }

    // Fill any remaining None slots (shouldn't happen, but defensive).
    let final_results: Vec<HydrateResult> = results
        .into_iter()
        .enumerate()
        .map(|(i, opt)| {
            opt.unwrap_or_else(|| HydrateResult {
                path: paths[i].clone(),
                outcome: HydrateOutcome::Failed {
                    reason: "internal: result slot not populated".to_string(),
                },
            })
        })
        .collect();

    let resolved = final_results
        .iter()
        .filter(|r| matches!(r.outcome, HydrateOutcome::Resolved { .. }))
        .count();
    let skipped = final_results
        .iter()
        .filter(|r| matches!(r.outcome, HydrateOutcome::Skipped))
        .count();
    let failed = final_results
        .iter()
        .filter(|r| matches!(r.outcome, HydrateOutcome::Failed { .. }))
        .count();

    info!(
        resolved,
        skipped, failed, shard_count, "batch resolver: complete"
    );

    Ok(final_results)
}

/// Resolve `file_hash → shard_hash`, trying the shard-hint fast path
/// first when the pointer carries one.
///
/// The shard-hint is an advisory optimization: when present, we check
/// the bloom pre-filter on the hinted shard to confirm the file is
/// likely there before committing to a full shard download. On any
/// failure (hint absent, bloom says absent, fetch error), we fall back
/// to the file-index lookup.
async fn try_shard_hint(
    store: &CachingStore,
    router: &StoreLayout,
    pointer: &Pointer,
    file_hash: &MerkleHash,
) -> Option<MerkleHash> {
    // Try shard-hint fast path.
    if let Some(hint_bytes) = pointer.shard_hint {
        let hint_hash = MerkleHash::from(hint_bytes);
        let shard_path = router.shard_path(&hint_hash);

        if store.has_cache_service() {
            debug!(
                file_hash = %file_hash.hex(),
                hint_hash = %hint_hash.hex(),
                "batch: trusting shard hint because cache service owns immutable shard reads"
            );
            return Some(hint_hash);
        }

        match bloom_prefilter::check_shard_file_bloom(store.origin(), &shard_path, file_hash).await
        {
            Ok(BloomCheck::PossiblyPresent | BloomCheck::NoBloom) => {
                debug!(
                    file_hash = %file_hash.hex(),
                    hint_hash = %hint_hash.hex(),
                    "batch: shard-hint bloom check passed"
                );
                return Some(hint_hash);
            }
            Ok(BloomCheck::DefinitelyAbsent) => {
                debug!(
                    file_hash = %file_hash.hex(),
                    hint_hash = %hint_hash.hex(),
                    "batch: shard-hint bloom says absent, falling back to file-index"
                );
            }
            Err(e) => {
                debug!(
                    file_hash = %file_hash.hex(),
                    hint_hash = %hint_hash.hex(),
                    error = %e,
                    "batch: shard-hint bloom check failed, falling back to file-index"
                );
            }
        }
    }

    None
}

/// Look up `file_hash → shard_hash` values via the per-repo `file_index_db`.
///
/// Uses one read-only MetaDb session for the whole fallback set.
async fn resolve_file_index_batch(
    store: &CachingStore,
    router: &StoreLayout,
    file_hashes: &[MerkleHash],
) -> Result<Vec<Option<MerkleHash>>> {
    let session = crab_metadata::file_index_lookup::FileIndexLookupSession::open(
        Arc::clone(store.origin().inner()),
        router.repo_prefix(),
    )
    .await?;

    let result = session.lookup_batch(file_hashes).await;
    if let Err(close_err) = session.close().await {
        warn!(
            error = %close_err,
            "batch: file-index lookup session close failed after read"
        );
    }
    result.map_err(crate::core::error::CrabError::from)
}

/// Download a shard via the local cache, or return a cached copy.
async fn get_or_download_shard(
    store: &CachingStore,
    router: &StoreLayout,
    cache: &Arc<LocalCache>,
    shard_hash: &MerkleHash,
) -> Result<ShardReader> {
    let key = CacheKey::Shard(*shard_hash);
    let obj_path = router.shard_path(shard_hash);
    let origin = store.origin().clone();
    let hash = *shard_hash;

    let data = cache
        .get_or_fetch_with(&key, || {
            let origin = origin;
            let obj_path = obj_path;
            async move {
                debug!(shard_hash = %hash.hex(), "batch: downloading shard");
                let (data, _) = origin
                    .get_with_etag_bounded(&obj_path, MAX_BATCH_SHARD_BYTES)
                    .await?;
                Ok::<_, CrabError>(data)
            }
        })
        .await?;

    Ok(ShardReader::from_bytes(data, *shard_hash))
}

/// Collect the set of unique xorb hashes referenced by all resolved files.
///
/// This enables callers to group xorb fetches across files, minimizing
/// redundant downloads when multiple files share xorb data.
fn collect_xorb_hashes(results: &[Option<HydrateResult>]) -> Vec<MerkleHash> {
    let mut seen = std::collections::HashSet::new();
    for result in results.iter().flatten() {
        if let HydrateOutcome::Resolved { file_info, .. } = &result.outcome {
            for seg in &file_info.segments {
                seen.insert(seg.xorb_hash);
            }
        }
    }
    seen.into_iter().collect()
}

/// Check whether a file is already hydrated on disk.
///
/// Uses the same size-only heuristic as `cmd/hydrate.rs`: if the file
/// exists, its size matches the pointer's declared size, and it doesn't
/// parse as a pointer, it's considered hydrated.
fn is_already_hydrated(path: &Path, ptr: &Pointer) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if meta.len() != ptr.size {
        return false;
    }
    matches!(
        crate::engine::pointer::is_working_tree_pointer(path),
        Ok(false)
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    fn sample_hash(seed: u8) -> [u8; 32] {
        let mut h = [0u8; 32];
        for (i, byte) in h.iter_mut().enumerate() {
            *byte = seed.wrapping_add(i as u8);
        }
        h
    }

    fn sample_pointer(seed: u8, size: u64) -> Pointer {
        Pointer {
            file_hash: sample_hash(seed),
            size,
            shard_hint: None,
        }
    }

    fn _sample_pointer_with_hint(seed: u8, size: u64, hint_seed: u8) -> Pointer {
        Pointer {
            file_hash: sample_hash(seed),
            size,
            shard_hint: Some(sample_hash(hint_seed)),
        }
    }

    // --- is_already_hydrated tests ---

    #[test]
    fn not_hydrated_when_file_missing() {
        let ptr = sample_pointer(1, 4096);
        assert!(!is_already_hydrated(
            Path::new("/nonexistent/file.bin"),
            &ptr
        ));
    }

    #[test]
    fn not_hydrated_when_size_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        std::fs::write(&path, vec![0xAB; 9999]).unwrap();
        let ptr = sample_pointer(1, 4096);
        assert!(!is_already_hydrated(&path, &ptr));
    }

    #[test]
    fn not_hydrated_when_file_is_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let ptr = sample_pointer(1, 4096);
        let path = dir.path().join("ptr.bin");
        std::fs::write(&path, ptr.serialize()).unwrap();
        assert!(!is_already_hydrated(&path, &ptr));
    }

    #[test]
    fn hydrated_when_size_matches_and_not_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let ptr = sample_pointer(1, 512);
        let path = dir.path().join("data.bin");
        std::fs::write(&path, vec![0xCD; 512]).unwrap();
        assert!(is_already_hydrated(&path, &ptr));
    }

    // --- collect_xorb_hashes tests ---

    #[test]
    fn collect_xorb_hashes_empty_for_no_resolved() {
        let results: Vec<Option<HydrateResult>> = vec![
            Some(HydrateResult {
                path: PathBuf::from("a.bin"),
                outcome: HydrateOutcome::Skipped,
            }),
            Some(HydrateResult {
                path: PathBuf::from("b.bin"),
                outcome: HydrateOutcome::Failed {
                    reason: "test".to_string(),
                },
            }),
        ];
        let xorbs = collect_xorb_hashes(&results);
        assert!(xorbs.is_empty());
    }

    #[test]
    fn collect_xorb_hashes_deduplicates() {
        use crab_xet::shard::FileDataSequenceEntry;

        let xorb_hash = MerkleHash::from([1u64, 2, 3, 4]);
        let file_info_a = MDBFileInfo {
            metadata: Default::default(),
            segments: vec![FileDataSequenceEntry {
                xorb_hash,
                chunk_index_start: 0,
                chunk_index_end: 5,
                ..Default::default()
            }],
            verification: vec![],
            metadata_ext: None,
        };
        let file_info_b = MDBFileInfo {
            metadata: Default::default(),
            segments: vec![FileDataSequenceEntry {
                xorb_hash, // same xorb
                chunk_index_start: 5,
                chunk_index_end: 10,
                ..Default::default()
            }],
            verification: vec![],
            metadata_ext: None,
        };

        let results: Vec<Option<HydrateResult>> = vec![
            Some(HydrateResult {
                path: PathBuf::from("a.bin"),
                outcome: HydrateOutcome::Resolved {
                    shard_hash: MerkleHash::default(),
                    file_info: file_info_a,
                },
            }),
            Some(HydrateResult {
                path: PathBuf::from("b.bin"),
                outcome: HydrateOutcome::Resolved {
                    shard_hash: MerkleHash::default(),
                    file_info: file_info_b,
                },
            }),
        ];

        let xorbs = collect_xorb_hashes(&results);
        // Both files reference the same xorb — should be deduplicated.
        assert_eq!(xorbs.len(), 1);
        assert_eq!(xorbs[0], xorb_hash);
    }
}
