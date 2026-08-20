//! Cross-client shard synchronization.
//!
//! `ShardSynchronizer` computes the delta between the remote shard list
//! and locally cached shards, downloads missing shards in parallel with
//! hash verification, and incrementally refreshes the `ChunkIndex` after
//! each install. Race-window handling (listed-but-not-yet-visible shards)
//! is built in: a 404 on a listed shard is logged and skipped, not fatal.
//!
//! Incremental sync: when a cached shard-list generation matches the
//! remote generation, all downloads are skipped and the `ChunkIndex` is
//! populated from the `PersistentChunkIndex` only. When the remote is
//! ahead, only the delta (hashes in remote but not in cached list) is
//! downloaded. Falls back to full sync when the cached generation is
//! missing or corrupt.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::cache::{CacheKey, LocalCache};
use crate::core::error::{CrabError, Result};
use crate::core::metrics::Metrics;
use crate::storage::StoreLayout;
use crab_metadata::bloom_prefilter::{BloomCheck, check_shard_chunk_bloom};
use crab_metadata::chunk_index::ChunkIndex;
use crab_metadata::manifests::ShardList;
use crab_metadata::persistent_chunk_index::PersistentChunkIndex;
use crab_xet::hash::compute_data_hash;
use crab_xet::shard::{MDBMinimalShard, MDBShardFile, new_shard_file_cache};
use crab_xet::xorb::format::{MerkleHash, XorbRef};

static SHARD_GEN_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Statistics from a shard sync operation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncStats {
    /// Shards successfully downloaded and installed.
    pub shards_downloaded: u64,
    /// Shards already present locally (skipped).
    pub shards_skipped: u64,
    /// Shards that failed to download or verify.
    pub shards_failed: u64,
    /// Shards skipped because the bloom filter reported no matches.
    pub shards_bloom_skipped: u64,
}

/// Locally cached shard-list generation for incremental sync.
///
/// Persisted at `~/.cache/crab/repos/{repo_hash}/shard-list-gen.json`.
/// Stores the generation counter and the set of shard hashes that were
/// present at that generation, enabling delta computation on subsequent
/// syncs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedShardListGen {
    /// The shard-list generation counter at the time of caching.
    pub generation: u64,
    /// The shard hashes present in the shard-list at this generation.
    pub shard_hashes: Vec<String>,
}

/// Load a cached shard-list generation from disk.
///
/// Returns `None` if the file is missing or corrupt (triggers full sync
/// fallback).
fn load_cached_generation(path: &Path) -> Option<CachedShardListGen> {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            debug!(path = %path.display(), error = %e, "no cached shard-list generation");
            return None;
        }
    };
    match serde_json::from_slice::<CachedShardListGen>(&data) {
        Ok(cached) => Some(cached),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "corrupt cached shard-list generation, falling back to full sync");
            None
        }
    }
}

/// Persist a shard-list generation to disk.
///
/// Creates parent directories as needed. Errors are logged but not fatal
/// — the next sync will simply do a full download.
fn save_cached_generation(path: &Path, cached: &CachedShardListGen) {
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        warn!(path = %parent.display(), error = %e, "failed to create cache directory for shard-list generation");
        return;
    }
    match serde_json::to_vec(cached) {
        Ok(data) => {
            if let Err(e) = write_cached_generation_atomic(path, &data) {
                warn!(path = %path.display(), error = %e, "failed to persist shard-list generation");
            }
        }
        Err(e) => {
            warn!(error = %e, "failed to serialize shard-list generation");
        }
    }
}

fn write_cached_generation_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = cached_generation_tmp_path(path);
    let write_result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(data)?;
        file.flush()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();

    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

fn cached_generation_tmp_path(path: &Path) -> PathBuf {
    let seq = SHARD_GEN_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    match path.file_name() {
        Some(name) => {
            let mut tmp_name = OsString::from(".");
            tmp_name.push(name);
            tmp_name.push(format!(".{pid}.{seq}.tmp"));
            match path.parent() {
                Some(parent) => parent.join(tmp_name),
                None => PathBuf::from(tmp_name),
            }
        }
        None => path.with_extension(format!("tmp.{pid}.{seq}")),
    }
}

/// Cross-client shard synchronizer.
///
/// Computes the remote-vs-local shard delta, downloads missing shards
/// in parallel, hash-verifies each download, installs into the local
/// cache, and incrementally refreshes the `ChunkIndex`.
///
/// When a cached shard-list generation is available, compares it against
/// the remote generation to download only the delta set. Falls back to
/// full sync when the cached generation is missing or corrupt.
pub struct ShardSynchronizer {
    router: StoreLayout,
    cache: Arc<LocalCache>,
    persistent_index: Option<Arc<PersistentChunkIndex>>,
    metrics: Option<Arc<Metrics>>,
    /// Maximum number of concurrent shard downloads.
    parallelism: usize,
    /// Path to the cached shard-list generation file, if set.
    gen_cache_path: Option<PathBuf>,
    /// Chunk hashes being pushed, used for bloom-first filtering.
    /// When set, shards in the delta set are checked against their bloom
    /// filter before downloading the full shard body.
    query_hashes: Option<Arc<HashSet<MerkleHash>>>,
    /// Install every queried hash instead of only the global sampling subset.
    exact_query_matches: bool,
    /// On-disk shard file handles for shards that exceeded the in-memory
    /// ChunkIndex ceiling. Queried via interpolation search as a fallback
    /// when a chunk hash is not found in the in-memory index.
    shard_files: Vec<Arc<MDBShardFile>>,
    /// Directory for on-disk shard files used by the fallback tier.
    /// Typically `~/.cache/crab/repos/{repo_hash}/shards/`.
    shard_cache_dir: Option<PathBuf>,
}

impl ShardSynchronizer {
    /// Create a new synchronizer.
    #[must_use]
    pub fn new(router: StoreLayout, cache: Arc<LocalCache>, metrics: Option<Arc<Metrics>>) -> Self {
        Self {
            router,
            cache,
            persistent_index: None,
            metrics,
            parallelism: 8,
            gen_cache_path: None,
            query_hashes: None,
            exact_query_matches: false,
            shard_files: Vec::new(),
            shard_cache_dir: None,
        }
    }

    /// Attach a persistent chunk index for write-through caching.
    #[must_use]
    pub fn with_persistent_index(mut self, index: Arc<PersistentChunkIndex>) -> Self {
        self.persistent_index = Some(index);
        self
    }

    /// Set the per-repo cache directory for generation-based incremental
    /// sync. The generation file is stored at
    /// `{cache_dir}/repos/{repo_hash}/shard-list-gen.json`.
    #[must_use]
    pub fn with_repo_cache_dir(mut self, cache_dir: &Path, repo_hash: &str) -> Self {
        self.gen_cache_path = Some(
            cache_dir
                .join("repos")
                .join(repo_hash)
                .join("shard-list-gen.json"),
        );
        self
    }

    /// Override the default download parallelism.
    #[must_use]
    pub fn with_parallelism(mut self, n: usize) -> Self {
        self.parallelism = n.max(1);
        self
    }

    /// Set the chunk hashes being pushed for bloom-first filtering.
    ///
    /// When provided, shards in the delta set are checked against their
    /// bloom filter (via a small suffix Range GET) before downloading the
    /// full shard body. Shards whose bloom reports no matches for any of
    /// these hashes are skipped entirely.
    #[must_use]
    pub fn with_query_hashes(mut self, hashes: HashSet<MerkleHash>) -> Self {
        self.query_hashes = Some(Arc::new(hashes));
        self.exact_query_matches = false;
        self
    }

    /// Restrict shard downloads with blooms and install every queried match.
    #[must_use]
    pub fn with_exact_query_hashes(mut self, hashes: HashSet<MerkleHash>) -> Self {
        self.query_hashes = Some(Arc::new(hashes));
        self.exact_query_matches = true;
        self
    }

    /// Set the on-disk shard cache directory for the fallback tier.
    ///
    /// When the in-memory ChunkIndex exceeds its memory ceiling, newly
    /// downloaded shards are written to this directory and queried via
    /// `MDBShardFile` interpolation search instead of being loaded into
    /// the HashMap.
    #[must_use]
    pub fn with_shard_cache_dir(mut self, dir: PathBuf) -> Self {
        self.shard_cache_dir = Some(dir);
        self
    }

    /// Returns the on-disk `MDBShardFile` handles accumulated during sync.
    ///
    /// These cover shards that exceeded the in-memory ChunkIndex ceiling
    /// and can be queried via `chunk_hash_dedup_query` for fallback lookups.
    pub fn shard_files(&self) -> &[Arc<MDBShardFile>] {
        &self.shard_files
    }

    fn install_chunk_index_shard(
        &self,
        chunk_index: &mut ChunkIndex,
        shard_hash: MerkleHash,
        entries: &[(MerkleHash, XorbRef)],
    ) {
        install_chunk_index_shard(chunk_index, shard_hash, entries, self.metrics.as_deref());
    }

    /// Synchronize shards using the manifest's shard list hash.
    ///
    /// Reads the bulk shard-list object from the manifest pointer's
    /// `shard_index_hash`, then delegates to [`Self::sync`]. Uses the
    /// content hash as a cache key: if the hash hasn't changed since the
    /// last sync, all downloads are skipped (same hash = same content).
    ///
    /// # Errors
    ///
    /// Returns errors from manifest loading or fatal storage failures.
    pub async fn sync_from_manifest(
        &mut self,
        chunk_index: &mut ChunkIndex,
        shard_index_hash: &str,
        generation: u64,
    ) -> Result<SyncStats> {
        // Cache key: if the shard-list hash hasn't changed AND the
        // ChunkIndex already has entries (from the PersistentChunkIndex
        // loaded earlier), skip the download. Without the `!chunk_index.is_empty()`
        // guard, a stale generation cache combined with an empty/missing
        // PersistentChunkIndex causes the sync to skip, leaving the
        // ChunkIndex empty — all chunks are then classified as "New"
        // and re-uploaded, destroying cross-version dedup.
        if let Some(ref gen_path) = self.gen_cache_path
            && let Some(cached) = load_cached_generation(gen_path)
            && cached.generation == generation
            && !shard_index_hash.is_empty()
        {
            if !chunk_index.is_empty() {
                debug!(
                    shard_index_hash,
                    generation,
                    chunk_index_entries = chunk_index.len(),
                    "shard-list hash unchanged and ChunkIndex populated, skipping sync"
                );
                let stats = SyncStats {
                    shards_skipped: cached.shard_hashes.len() as u64,
                    ..SyncStats::default()
                };
                return Ok(stats);
            }
            debug!(
                shard_index_hash,
                generation,
                "shard-list generation matches but ChunkIndex is empty, forcing full sync"
            );
        }

        if shard_index_hash.is_empty() {
            debug!("empty shard_index_hash, nothing to sync");
            return Ok(SyncStats::default());
        }

        let shard_hashes = crate::metadata::manifest::read_bulk_shard_list(
            self.router.store(),
            &self.router,
            shard_index_hash,
        )
        .await?;

        let shard_list = ShardList {
            generation,
            entries: shard_hashes,
        };

        self.sync(chunk_index, &shard_list).await
    }

    /// Synchronize shards from the remote shard list into the local cache
    /// and chunk index.
    ///
    /// When a cached shard-list generation is available and matches the
    /// remote generation, all downloads are skipped and the `ChunkIndex`
    /// is populated from the `PersistentChunkIndex` only. When the remote
    /// is ahead, only the delta (hashes in remote but not in cached list)
    /// is downloaded. Falls back to full sync when the cached generation
    /// is missing or corrupt.
    ///
    /// Returns statistics about the sync operation.
    ///
    /// # Errors
    ///
    /// Returns errors from manifest loading or fatal storage failures.
    /// Individual shard download failures are counted in `shards_failed`
    /// rather than aborting the entire sync.
    pub async fn sync(
        &mut self,
        chunk_index: &mut ChunkIndex,
        shard_list: &ShardList,
    ) -> Result<SyncStats> {
        let mut stats = SyncStats::default();
        let remote_generation = shard_list.generation;
        let remote_shards = &shard_list.entries;

        // Try incremental sync via generation comparison.
        let effective_shards = self.resolve_incremental_delta(
            chunk_index,
            remote_generation,
            remote_shards,
            &mut stats,
        );

        let to_download = self
            .compute_delta(chunk_index, &effective_shards, &mut stats)
            .await;

        if to_download.is_empty() {
            debug!(
                skipped = stats.shards_skipped,
                "shard sync: all shards up to date"
            );
            self.persist_generation(remote_generation, remote_shards);
            return Ok(stats);
        }

        // Apply bloom-first filtering: for shards with a v2 bloom trailer,
        // download only the bloom section and skip the full shard when the
        // bloom reports no matches against the query hashes.
        let to_download = self.bloom_filter_delta(to_download, &mut stats).await;

        if to_download.is_empty() {
            debug!(
                skipped = stats.shards_skipped,
                bloom_skipped = stats.shards_bloom_skipped,
                "shard sync: all shards up to date after bloom filtering"
            );
            self.persist_generation(remote_generation, remote_shards);
            return Ok(stats);
        }

        debug!(
            to_download = to_download.len(),
            skipped = stats.shards_skipped,
            bloom_skipped = stats.shards_bloom_skipped,
            "shard sync: downloading missing shards"
        );

        self.download_and_install(chunk_index, &to_download, &mut stats)
            .await;

        if let Some(m) = &self.metrics {
            m.set_chunk_index_entries(chunk_index.len() as u64);
        }

        debug!(
            downloaded = stats.shards_downloaded,
            skipped = stats.shards_skipped,
            bloom_skipped = stats.shards_bloom_skipped,
            failed = stats.shards_failed,
            "shard sync complete"
        );

        // Persist the new generation after successful sync.
        self.persist_generation(remote_generation, remote_shards);

        Ok(stats)
    }

    /// Resolve the effective set of shards to consider for delta
    /// computation, using the cached generation for incremental sync.
    ///
    /// Returns the subset of remote shards that need to be checked
    /// against the local state. When generations match, returns an empty
    /// vec (all shards are already accounted for). When the remote is
    /// ahead, returns only the delta (remote - cached). Falls back to
    /// the full remote list when no cached generation is available.
    fn resolve_incremental_delta(
        &self,
        chunk_index: &mut ChunkIndex,
        remote_generation: u64,
        remote_shards: &[String],
        stats: &mut SyncStats,
    ) -> Vec<String> {
        let Some(gen_path) = &self.gen_cache_path else {
            return remote_shards.to_vec();
        };

        let Some(cached) = load_cached_generation(gen_path) else {
            debug!("no cached generation, performing full sync");
            return remote_shards.to_vec();
        };

        if cached.generation == remote_generation {
            // Generations match — all shards are already known locally.
            // Mark all remote shards as installed in the chunk index
            // (they should already be in the persistent index from a
            // previous sync).
            //
            // However, if the ChunkIndex is still empty after attempting
            // to load from the persistent index, the persistent index is
            // missing or corrupt. In that case, fall through to full sync
            // so the shards are re-downloaded and entries are installed.
            let mut all_verified = true;
            for shard_hex in remote_shards {
                let Ok(hash) = MerkleHash::from_hex(shard_hex) else {
                    continue;
                };
                if !chunk_index.has_shard(&hash) {
                    // The persistent index should have this shard from
                    // the previous sync. Mark it as installed.
                    if let Some(pi) = &self.persistent_index {
                        if let Ok(true) = pi.has_shard(&hash) {
                            self.install_chunk_index_shard(chunk_index, hash, &[]);
                        } else {
                            all_verified = false;
                        }
                    } else {
                        all_verified = false;
                    }
                }
                stats.shards_skipped += 1;
            }

            if all_verified {
                debug!(
                    generation = remote_generation,
                    shards = remote_shards.len(),
                    "generations match, all shards verified in persistent index"
                );
                return Vec::new();
            }

            // Persistent index is missing entries for some shards.
            // Reset the skipped count and fall through to full sync.
            debug!(
                generation = remote_generation,
                shards = remote_shards.len(),
                "generations match but persistent index incomplete, falling back to full sync"
            );
            stats.shards_skipped = 0;
            return remote_shards.to_vec();
        }

        if remote_generation > cached.generation {
            // Remote is ahead — compute the delta.
            let cached_set: HashSet<&str> =
                cached.shard_hashes.iter().map(String::as_str).collect();
            let delta: Vec<String> = remote_shards
                .iter()
                .filter(|h| !cached_set.contains(h.as_str()))
                .cloned()
                .collect();
            info!(
                cached_gen = cached.generation,
                remote_gen = remote_generation,
                total_remote = remote_shards.len(),
                delta = delta.len(),
                "incremental sync: downloading delta shards"
            );

            // Mark cached shards as skipped — they were synced in a
            // previous session and should be in the persistent index.
            for shard_hex in remote_shards {
                if cached_set.contains(shard_hex.as_str()) {
                    let Ok(hash) = MerkleHash::from_hex(shard_hex) else {
                        continue;
                    };
                    if !chunk_index.has_shard(&hash)
                        && let Some(pi) = &self.persistent_index
                        && let Ok(true) = pi.has_shard(&hash)
                    {
                        self.install_chunk_index_shard(chunk_index, hash, &[]);
                    }
                    stats.shards_skipped += 1;
                }
            }

            return delta;
        }

        // Remote generation is behind cached — shouldn't happen in
        // normal operation (generations are monotonic). Fall back to
        // full sync to be safe.
        warn!(
            cached_gen = cached.generation,
            remote_gen = remote_generation,
            "remote generation behind cached, falling back to full sync"
        );
        remote_shards.to_vec()
    }

    /// Apply bloom-first filtering to the delta set.
    ///
    /// For each shard in the download list, reads a small suffix via Range
    /// GET to check for a v2 bloom trailer. If a bloom exists, downloads
    /// only the bloom section and queries it with the push's chunk hashes.
    /// Shards whose bloom reports no matches are skipped entirely.
    ///
    /// Falls back to full download when:
    /// - No query hashes were provided (bloom filtering disabled)
    /// - The shard has no v2 bloom trailer
    /// - The bloom download or parse fails
    /// - The bloom reports possible matches
    async fn bloom_filter_delta(
        &self,
        candidates: Vec<(String, MerkleHash)>,
        stats: &mut SyncStats,
    ) -> Vec<(String, MerkleHash)> {
        let query = match &self.query_hashes {
            Some(q) if !q.is_empty() => Arc::clone(q),
            _ => return candidates,
        };

        let mut remaining = Vec::with_capacity(candidates.len());

        for batch in candidates.chunks(self.parallelism) {
            let mut handles: Vec<(String, MerkleHash, _)> = Vec::with_capacity(batch.len());

            for (shard_hex, hash) in batch {
                let store = self.router.store().clone();
                let shard_path = self.router.shard_path(hash);
                let hash = *hash;
                let hex = shard_hex.clone();
                let q = Arc::clone(&query);
                // Capture (hex, hash) outside the spawn so that if the
                // task panics, we still know which shard it was for and
                // can fall back to a full download. The previous version
                // moved the hex into the spawn, losing the shard on
                // panic and silently reducing dedup effectiveness. See
                // finding CR1-F17 (task 1.9).
                let task_hex = hex.clone();
                let handle =
                    tokio::spawn(async move { check_bloom_skip(&store, &shard_path, &q).await });
                handles.push((task_hex, hash, handle));
            }

            for (hex, hash, handle) in handles {
                match handle.await {
                    Ok(Ok(true)) => {
                        debug!(shard = %hex, "bloom reports no matches, skipping shard");
                        stats.shards_bloom_skipped += 1;
                    }
                    Ok(Ok(false)) => {
                        remaining.push((hex, hash));
                    }
                    Ok(Err(e)) => {
                        debug!(shard = %hex, error = %e, "bloom check failed, downloading full shard");
                        remaining.push((hex, hash));
                    }
                    Err(e) => {
                        warn!(
                            shard = %hex,
                            error = %e,
                            "bloom check task panicked; falling back to full download"
                        );
                        remaining.push((hex, hash));
                    }
                }
            }
        }

        remaining
    }

    /// Persist the current shard-list generation to disk.
    fn persist_generation(&self, generation: u64, shard_hashes: &[String]) {
        let Some(gen_path) = &self.gen_cache_path else {
            return;
        };
        let cached = CachedShardListGen {
            generation,
            shard_hashes: shard_hashes.to_vec(),
        };
        save_cached_generation(gen_path, &cached);
    }

    /// Compute which remote shards are missing locally.
    async fn compute_delta(
        &mut self,
        chunk_index: &mut ChunkIndex,
        remote_shards: &[String],
        stats: &mut SyncStats,
    ) -> Vec<(String, MerkleHash)> {
        let mut to_download = Vec::new();
        for shard_hex in remote_shards {
            let Ok(hash) = MerkleHash::from_hex(shard_hex) else {
                warn!(shard = %shard_hex, "invalid shard hash in shard list, skipping");
                stats.shards_failed += 1;
                continue;
            };

            if chunk_index.has_shard(&hash) {
                stats.shards_skipped += 1;
                continue;
            }

            // Check persistent index — if the shard is already installed
            // there, mark it as installed in the in-memory index and skip
            // the download entirely.
            if let Some(pi) = &self.persistent_index {
                match pi.has_shard(&hash) {
                    Ok(true) => {
                        // Mark the shard as installed in the in-memory index
                        // without loading entries (they were loaded in bulk
                        // at startup via load_all).
                        self.install_chunk_index_shard(chunk_index, hash, &[]);
                        stats.shards_skipped += 1;
                        continue;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        debug!(shard = %shard_hex, error = %e, "persistent index check failed, will download");
                    }
                }
            }

            // Check local cache.
            if self.cache.contains(&CacheKey::Shard(hash)).await {
                if self.install_from_cache(&hash, chunk_index).await.is_ok() {
                    stats.shards_skipped += 1;
                    continue;
                }
                debug!(shard = %shard_hex, "cache install failed, will re-download");
            }

            to_download.push((shard_hex.clone(), hash));
        }
        to_download
    }

    /// Download shards in parallel batches and install them.
    ///
    /// When bloom filtering is active (`query_hashes` is set), only
    /// global-dedup-eligible chunk entries are installed into the
    /// ChunkIndex. This avoids inflating the index with chunks that
    /// cannot participate in cross-repo dedup.
    async fn download_and_install(
        &mut self,
        chunk_index: &mut ChunkIndex,
        to_download: &[(String, MerkleHash)],
        stats: &mut SyncStats,
    ) {
        let query_scoped = self.query_hashes.is_some();

        for batch in to_download.chunks(self.parallelism) {
            let mut handles = Vec::with_capacity(batch.len());

            for (_shard_hex, hash) in batch {
                let store = self.router.store().clone();
                let shard_path = self.router.shard_path(hash);
                let hash = *hash;
                handles.push(tokio::spawn(async move {
                    download_one_shard(&store, &shard_path, hash).await
                }));
            }

            for handle in handles {
                match handle.await {
                    Ok(Ok((hash, body))) => {
                        if let Err(e) = self.cache.put(&CacheKey::Shard(hash), &body).await {
                            warn!(shard = %hash.hex(), error = %e, "failed to cache shard");
                        }
                        self.install_shard_data(&hash, &body, chunk_index, query_scoped);
                        stats.shards_downloaded += 1;
                    }
                    Ok(Err(CrabError::NotFound { path })) => {
                        warn!(shard = %path, "shard listed but not visible (race window), skipping");
                        stats.shards_failed += 1;
                    }
                    Ok(Err(e)) => {
                        warn!(error = %e, "shard download failed");
                        stats.shards_failed += 1;
                    }
                    Err(e) => {
                        warn!(error = %e, "shard download task panicked");
                        stats.shards_failed += 1;
                    }
                }
            }
        }
    }

    /// Install a shard from the local cache into the chunk index.
    async fn install_from_cache(
        &mut self,
        hash: &MerkleHash,
        chunk_index: &mut ChunkIndex,
    ) -> Result<()> {
        let key = CacheKey::Shard(*hash);
        let data = self
            .cache
            .get_or_fetch_with(&key, || async {
                Err(CrabError::NotFound { path: hash.hex() })
            })
            .await?;
        self.install_shard_data(hash, &data, chunk_index, false);
        Ok(())
    }

    /// Parse shard data and install chunk entries into the index.
    ///
    /// Uses streaming xorb-info extraction to avoid building the full
    /// `MDBShardInfo` intermediate structure. The file-info section is
    /// skipped entirely — it is only needed for hydrate, not for dedup
    /// classification during push.
    ///
    /// Query-scoped installs never mark a shard complete. Cross-repo mode
    /// retains only globally eligible entries; exact mode retains every
    /// requested hash for same-repository edit dedupe.
    ///
    /// When the in-memory ChunkIndex exceeds its memory ceiling and a
    /// `shard_cache_dir` is configured, the shard is written to disk
    /// and an `MDBShardFile` handle is created for on-disk interpolation
    /// search instead of loading entries into the HashMap.
    fn install_shard_data(
        &mut self,
        hash: &MerkleHash,
        data: &Bytes,
        chunk_index: &mut ChunkIndex,
        query_scoped: bool,
    ) {
        // When the in-memory index is over its ceiling and we have a
        // shard cache directory, spill to on-disk MDBShardFile handles.
        if chunk_index.over_ceiling()
            && let Some(dir) = self.shard_cache_dir.clone()
        {
            match self.spill_shard_to_disk(hash, data, &dir, chunk_index) {
                Ok(()) => return,
                Err(e) => {
                    warn!(
                        shard = %hash.hex(),
                        error = %e,
                        "failed to spill shard to disk, falling back to in-memory install"
                    );
                    // Fall through to normal in-memory install.
                }
            }
        }

        let entries = match (&self.query_hashes, self.exact_query_matches) {
            (Some(query), true) => extract_exact_query_entries(data, query),
            (Some(_), false) => extract_dedup_eligible_entries(data),
            (None, _) => extract_chunk_entries_streaming(data),
        };

        // Query-scoped sync may install only a dedup-eligible subset.
        // Do not mark that shard complete, or a later full sync will
        // skip the missing non-query entries.
        if query_scoped {
            for (chunk_hash, xorb_ref) in &entries {
                chunk_index.insert(*chunk_hash, *xorb_ref);
            }
            if let Some(m) = &self.metrics {
                m.set_chunk_index_entries(chunk_index.len() as u64);
            }
        } else {
            self.install_chunk_index_shard(chunk_index, *hash, &entries);
        }

        if let Some(pi) = &self.persistent_index {
            let result = if query_scoped {
                pi.insert_batch(&entries)
            } else {
                pi.install_shard(*hash, &entries)
            };
            if let Err(e) = result {
                warn!(shard = %hash.hex(), error = %e, "failed to persist shard in chunk index");
            }
        }
    }

    /// Write shard bytes to disk and create an `MDBShardFile` handle for
    /// on-disk interpolation search.
    ///
    /// The shard is written to `{shard_cache_dir}/{shard_filename}` via
    /// `MDBShardFile::write_out_from_reader`, which hashes the content
    /// and names the file by its hash. The resulting handle is added to
    /// `self.shard_files` for later querying.
    ///
    /// The shard is still marked as installed in the ChunkIndex (with no
    /// entries) so that future syncs don't re-download it.
    fn spill_shard_to_disk(
        &mut self,
        hash: &MerkleHash,
        data: &Bytes,
        dir: &Path,
        chunk_index: &mut ChunkIndex,
    ) -> Result<()> {
        if let Err(e) = std::fs::create_dir_all(dir) {
            return Err(CrabError::Internal(format!(
                "failed to create shard cache dir {}: {e}",
                dir.display()
            )));
        }

        let mut cursor = std::io::Cursor::new(strip_v2_trailer(data));
        let shard_file_cache = new_shard_file_cache();
        let shard_file = MDBShardFile::write_out_from_reader(dir, &mut cursor, &shard_file_cache)
            .map_err(|e| {
            CrabError::Internal(format!("failed to write shard {} to disk: {e}", hash.hex()))
        })?;

        debug!(
            shard = %hash.hex(),
            path = %shard_file.path.display(),
            "spilled shard to disk (ChunkIndex over ceiling)"
        );

        // Mark the shard as installed (with no entries) so it won't be
        // re-downloaded, but the actual lookups go through the on-disk handle.
        self.install_chunk_index_shard(chunk_index, *hash, &[]);
        self.shard_files.push(shard_file);

        // Write-through to persistent index when available.
        if let Some(pi) = &self.persistent_index {
            let entries = extract_chunk_entries_streaming(data);
            if let Err(e) = pi.install_shard(*hash, &entries) {
                warn!(shard = %hash.hex(), error = %e, "failed to persist spilled shard in chunk index");
            }
        }

        Ok(())
    }
}

/// Download and hash-verify a single shard from the object store.
async fn download_one_shard(
    store: &crate::storage::store::Store,
    shard_path: &object_store::path::Path,
    hash: MerkleHash,
) -> Result<(MerkleHash, Bytes)> {
    let (body, _etag) = store.get_with_etag(shard_path).await.map_err(|e| {
        if matches!(e, CrabError::NotFound { .. }) {
            CrabError::NotFound { path: hash.hex() }
        } else {
            e
        }
    })?;

    let actual = compute_data_hash(&body);
    if actual == hash {
        Ok((hash, body))
    } else {
        Err(CrabError::CorruptObject {
            path: hash.hex(),
            reason: format!("expected {}, got {}", hash.hex(), actual.hex()),
        })
    }
}

/// Check whether a shard's bloom filter indicates no matches for any of
/// the query hashes.
///
/// Returns `Ok(true)` when the bloom definitively reports no matches
/// (the shard can be skipped). Returns `Ok(false)` when the bloom
/// reports possible matches, the shard has no bloom, or the shard is too
/// small for a v2 trailer. Returns `Err` on I/O or parse failures (the
/// caller should fall back to a full download).
///
/// Thin wrapper over the shared pre-filter in
/// [`crab_metadata::bloom_prefilter`] — see that module for the
/// trailer format, the 4 KiB Range-GET budget, and soundness guarantees.
async fn check_bloom_skip(
    store: &crate::storage::store::Store,
    shard_path: &object_store::path::Path,
    query_hashes: &HashSet<MerkleHash>,
) -> Result<bool> {
    match check_shard_chunk_bloom(store.as_storage(), shard_path, query_hashes).await? {
        BloomCheck::DefinitelyAbsent => Ok(true),
        BloomCheck::PossiblyPresent | BloomCheck::NoBloom => Ok(false),
    }
}

/// Extract chunk→xorb mappings from raw shard bytes via streaming parse.
///
/// Thin shim over [`crab_xet::shard_parse::extract_chunk_entries_streaming`]
/// so the same implementation is shared with the rebuild command.
fn extract_chunk_entries_streaming(
    data: &Bytes,
) -> Vec<(MerkleHash, crab_xet::xorb::format::XorbRef)> {
    crab_xet::shard_parse::extract_chunk_entries_streaming(data)
}

fn extract_exact_query_entries(
    data: &Bytes,
    query_hashes: &HashSet<MerkleHash>,
) -> Vec<(MerkleHash, crab_xet::xorb::format::XorbRef)> {
    extract_chunk_entries_streaming(data)
        .into_iter()
        .filter(|(chunk_hash, _)| query_hashes.contains(chunk_hash))
        .collect()
}

/// Extract only global-dedup-eligible chunk→xorb mappings from raw shard
/// bytes.
///
/// Parses the shard into an `MDBMinimalShard` (with both file-info and
/// xorb-info) and calls `global_dedup_eligible_chunks()` to identify
/// which chunks qualify for cross-repo dedup. Only those chunks' XorbRef
/// entries are returned.
///
/// Falls back to the full `extract_chunk_entries_streaming` if the
/// `MDBMinimalShard` parse fails.
fn extract_dedup_eligible_entries(
    data: &Bytes,
) -> Vec<(MerkleHash, crab_xet::xorb::format::XorbRef)> {
    use std::collections::HashSet;
    let v1_data = strip_v2_trailer(data);
    let mut cursor = std::io::Cursor::new(v1_data);

    let shard = match MDBMinimalShard::from_reader(&mut cursor, true, true) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "failed to parse shard for dedup-eligible extraction, falling back to full extraction");
            return extract_chunk_entries_streaming(data);
        }
    };

    let eligible: HashSet<MerkleHash> = shard.global_dedup_eligible_chunks().into_iter().collect();

    let mut entries = Vec::with_capacity(eligible.len());
    for xorb_idx in 0..shard.num_xorb() {
        let Some(xorb_view) = shard.xorb(xorb_idx) else {
            break;
        };
        let xorb_hash = xorb_view.xorb_hash();
        for idx in 0..xorb_view.num_entries() {
            let chunk = xorb_view.chunk(idx);
            if eligible.contains(&chunk.chunk_hash) {
                entries.push((
                    chunk.chunk_hash,
                    crab_xet::xorb::format::XorbRef {
                        xorb_hash,
                        chunk_index: idx as u32,
                        uncompressed_size: chunk.unpacked_segment_bytes,
                    },
                ));
            }
        }
    }

    debug!(
        eligible = entries.len(),
        total_xorbs = shard.num_xorb(),
        "extracted dedup-eligible chunk entries from shard"
    );

    entries
}

/// Strip the v2 bloom trailer from shard bytes, returning the v1 portion.
///
/// For v1 shards (no bloom trailer), returns the full slice unchanged.
fn strip_v2_trailer(data: &[u8]) -> &[u8] {
    crab_xet::shard_parse::strip_v2_trailer(data)
}

fn install_chunk_index_shard(
    chunk_index: &mut ChunkIndex,
    shard_hash: MerkleHash,
    entries: &[(MerkleHash, XorbRef)],
    metrics: Option<&Metrics>,
) {
    let already_installed = chunk_index.has_shard(&shard_hash);
    chunk_index.install_shard(shard_hash, entries);
    if !already_installed && let Some(metrics) = metrics {
        metrics.inc_chunk_index_shards_installed();
        metrics.set_chunk_index_entries(chunk_index.len() as u64);
    }
}

/// Run a post-fetch shard sync to warm the local chunk-index cache.
///
/// Invoked by clone, pull, and fetch after packs are on disk: reads
/// the current repository snapshot, computes the shard delta against
/// locally installed shards via
/// [`PersistentChunkIndex::installed_shards`], downloads the missing
/// shards in parallel, and installs them into both local cache tiers.
///
/// Progress is reported to stderr when `emit_progress` is true —
/// clone/pull/fetch CLI entry points set it; the remote-helper path
/// keeps it silent because stderr carries the git plumbing protocol.
///
/// `repo_prefix` is the bucket-relative path (e.g. `org/models`); the
/// function uses `repo_hash` for repo-scoped shard state and the store's
/// bucket identity for the globally shared chunk-index cache.
///
/// Failures are intentionally non-fatal: a missing manifest, a 404 on
/// an individual shard, or a corrupt cache entry all produce a
/// `warn!` and let the caller proceed. The local cache is an
/// optimisation; correctness still comes from lazy-on-miss lookups
/// against the remote `chunk_index_db` on the next push.
pub async fn run_post_fetch_shard_sync(
    router: StoreLayout,
    repo_hash: &str,
    cache_dir: &Path,
    metrics: Option<Arc<Metrics>>,
    emit_progress: bool,
) -> Result<SyncStats> {
    let snapshot =
        crate::metadata::manifest::read_repository_snapshot(router.store(), &router).await?;

    if snapshot.journal.shards.is_empty() {
        debug!("post-fetch shard sync: repository has no shards, nothing to sync");
        return Ok(SyncStats::default());
    }

    // Shard placements are bucket-global, so clone/fetch and push must
    // warm the same cache across every repository in that bucket.
    let index_path =
        crate::cache::chunk_index_cache_path(cache_dir, &router.store().bucket_identity());
    let persistent = match PersistentChunkIndex::open_shared(&index_path) {
        Ok(pi) => pi,
        Err(e) => {
            // The cache is an optimisation — if it can't be opened, skip
            // the sync entirely rather than aborting the fetch. Next
            // push will lazy-fill against chunk_index_db on miss.
            warn!(
                error = %e,
                path = %index_path.display(),
                "post-fetch shard sync: failed to open persistent chunk index, skipping"
            );
            return Ok(SyncStats::default());
        }
    };

    // Warm the in-memory ChunkIndex from the persistent tier so the
    // delta computation can skip shards already on disk.
    let mut chunk_index = ChunkIndex::new();
    match persistent.load_all() {
        Ok(entries) => {
            for (chunk_hash, xorb_ref) in &entries {
                chunk_index.insert(*chunk_hash, *xorb_ref);
            }
            if let Ok(installed) = persistent.installed_shards() {
                for shard_hash in &installed {
                    install_chunk_index_shard(
                        &mut chunk_index,
                        *shard_hash,
                        &[],
                        metrics.as_deref(),
                    );
                }
                debug!(
                    shards = installed.len(),
                    entries = entries.len(),
                    "post-fetch shard sync: loaded persistent chunk index"
                );
            }
        }
        Err(e) => {
            warn!(
                error = %e,
                "post-fetch shard sync: failed to load persistent chunk index, starting empty"
            );
        }
    }

    let local_cache = Arc::new(LocalCache::new(cache_dir.to_path_buf()));
    let shard_cache_dir = cache_dir.join("repos").join(repo_hash).join("shards");

    let mut synchronizer = ShardSynchronizer::new(router, local_cache, metrics)
        .with_persistent_index(Arc::clone(&persistent))
        .with_shard_cache_dir(shard_cache_dir);

    // The compacted manifest generation does not change until journal
    // compaction. Do not cache an active journal shard set under that stale
    // generation or a later fetch could incorrectly skip newly added shards.
    if snapshot.journal.transactions.is_empty() {
        synchronizer = synchronizer.with_repo_cache_dir(cache_dir, repo_hash);
    }

    let stats = synchronizer
        .sync(
            &mut chunk_index,
            &ShardList {
                generation: snapshot.manifest.generation,
                entries: snapshot.journal.shards,
            },
        )
        .await?;

    info!(
        downloaded = stats.shards_downloaded,
        skipped = stats.shards_skipped,
        failed = stats.shards_failed,
        bloom_skipped = stats.shards_bloom_skipped,
        "post-fetch shard sync complete"
    );

    if emit_progress && stats.shards_downloaded > 0 {
        // One concise line on stderr — no TTY progress bar, no JSONL.
        // The CLI paths that invoke this (clone / fetch) own the
        // structured output surface and can augment this if needed.
        eprintln!(
            "Syncing chunk index: {} shard(s) downloaded",
            stats.shards_downloaded
        );
    }

    Ok(stats)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::metadata::manifest::{
        Manifest, RefJournalEdit, RefJournalTransaction, commit_ref_journal_transaction,
        create_manifest, read_ref_journal_head,
    };
    use crate::storage::StoreLayout;
    use crate::storage::store::Store;
    use crab_metadata::manifests::ShardList;
    use object_store::memory::InMemory;
    use tempfile::TempDir;

    fn setup() -> (StoreLayout, Arc<LocalCache>, TempDir) {
        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store, "repo".to_string());
        let dir = TempDir::new().unwrap();
        let cache = Arc::new(LocalCache::new(dir.path().to_path_buf()));
        (router, cache, dir)
    }

    /// Build a ShardList from a slice of hex strings and a generation.
    fn make_shard_list(generation: u64, entries: &[String]) -> ShardList {
        ShardList {
            generation,
            entries: entries.to_vec(),
        }
    }

    #[tokio::test]
    async fn sync_with_empty_shard_list() {
        let (router, cache, _dir) = setup();
        let mut sync = ShardSynchronizer::new(router, cache, None);
        let mut idx = ChunkIndex::new();
        let shard_list = make_shard_list(0, &[]);
        let stats = sync.sync(&mut idx, &shard_list).await.unwrap();
        assert_eq!(stats.shards_downloaded, 0);
        assert_eq!(stats.shards_skipped, 0);
        assert_eq!(stats.shards_failed, 0);
    }

    #[tokio::test]
    async fn sync_skips_already_installed_shards() {
        let (router, cache, _dir) = setup();
        let mut sync = ShardSynchronizer::new(router, cache, None);
        let mut idx = ChunkIndex::new();

        let hash = MerkleHash::from([1u64, 1, 1, 1]);
        idx.install_shard(hash, &[]);

        let shard_list = make_shard_list(1, &[hash.hex()]);
        let stats = sync.sync(&mut idx, &shard_list).await.unwrap();
        assert_eq!(stats.shards_skipped, 1);
        assert_eq!(stats.shards_downloaded, 0);
    }

    #[tokio::test]
    async fn sync_handles_invalid_hash_gracefully() {
        let (router, cache, _dir) = setup();
        let mut sync = ShardSynchronizer::new(router, cache, None);
        let mut idx = ChunkIndex::new();

        let shard_list = make_shard_list(1, &["not-a-valid-hex".to_string()]);
        let stats = sync.sync(&mut idx, &shard_list).await.unwrap();
        assert_eq!(stats.shards_failed, 1);
    }

    #[tokio::test]
    async fn sync_handles_missing_shard_race_window() {
        let (router, cache, _dir) = setup();
        let mut sync = ShardSynchronizer::new(router, cache, None);
        let mut idx = ChunkIndex::new();

        // Shard listed but not uploaded — 404 (race window).
        let hash = MerkleHash::from([42u64, 42, 42, 42]);
        let shard_list = make_shard_list(1, &[hash.hex()]);
        let stats = sync.sync(&mut idx, &shard_list).await.unwrap();
        assert_eq!(stats.shards_failed, 1);
        assert_eq!(stats.shards_downloaded, 0);
    }

    #[tokio::test]
    async fn sync_downloads_and_installs_shard() {
        let (router, cache, _dir) = setup();

        let shard_data = b"fake shard data for testing";
        let hash = compute_data_hash(shard_data);
        let hex = hash.hex();
        // Upload to the global shard path that StoreLayout produces.
        let obj_path = router.shard_path(&hash);
        router
            .store()
            .put(&obj_path, Bytes::from(shard_data.as_slice()))
            .await
            .unwrap();

        let mut sync = ShardSynchronizer::new(router, cache.clone(), None);
        let mut idx = ChunkIndex::new();

        let shard_list = make_shard_list(1, &[hex.clone()]);
        let stats = sync.sync(&mut idx, &shard_list).await.unwrap();
        assert_eq!(stats.shards_downloaded, 1);
        assert!(idx.has_shard(&hash));
        assert!(cache.contains(&CacheKey::Shard(hash)).await);
    }

    #[tokio::test]
    async fn post_fetch_sync_does_not_hide_journal_shards_behind_manifest_generation_cache() {
        let (router, _cache, dir) = setup();
        let manifest = Manifest::default_for_repo("refs/heads/main");
        create_manifest(router.store(), &router, &manifest)
            .await
            .unwrap();

        let shard_data = b"journal shard";
        let shard_hash = compute_data_hash(shard_data);
        router
            .store()
            .put(
                &router.shard_path(&shard_hash),
                Bytes::from_static(shard_data),
            )
            .await
            .unwrap();

        let ref_name = "refs/heads/main";
        let head = read_ref_journal_head(router.store(), &router, ref_name)
            .await
            .unwrap();
        let transaction = RefJournalTransaction::new(
            BTreeMap::from([(ref_name.to_owned(), head.visible_transaction.clone())]),
            vec![RefJournalEdit {
                ref_name: ref_name.to_owned(),
                old_oid: None,
                new_oid: Some("a".repeat(40)),
                peeled_oid: None,
            }],
            None,
            Vec::new(),
            vec![shard_hash.hex()],
        )
        .unwrap();
        commit_ref_journal_transaction(router.store(), &router, &transaction, &[head])
            .await
            .unwrap();

        let generation_path = dir
            .path()
            .join("repos")
            .join("repo-hash")
            .join("shard-list-gen.json");
        save_cached_generation(
            &generation_path,
            &CachedShardListGen {
                generation: manifest.generation,
                shard_hashes: Vec::new(),
            },
        );

        let stats = run_post_fetch_shard_sync(router, "repo-hash", dir.path(), None, false)
            .await
            .unwrap();

        assert_eq!(stats.shards_downloaded, 1);
    }

    #[tokio::test]
    async fn sync_detects_corrupt_download() {
        let (router, cache, _dir) = setup();

        let hash = MerkleHash::from([99u64, 99, 99, 99]);
        let obj_path = router.shard_path(&hash);
        router
            .store()
            .put(&obj_path, Bytes::from_static(b"wrong content"))
            .await
            .unwrap();

        let mut sync = ShardSynchronizer::new(router, cache, None);
        let mut idx = ChunkIndex::new();

        let shard_list = make_shard_list(1, &[hash.hex()]);
        let stats = sync.sync(&mut idx, &shard_list).await.unwrap();
        assert_eq!(stats.shards_failed, 1);
        assert_eq!(stats.shards_downloaded, 0);
    }

    #[tokio::test]
    async fn sync_emits_metrics() {
        let (router, cache, _dir) = setup();
        let metrics = Arc::new(Metrics::new());

        let shard_data = b"metrics test shard";
        let hash = compute_data_hash(shard_data);
        let hex = hash.hex();
        let obj_path = router.shard_path(&hash);
        router
            .store()
            .put(&obj_path, Bytes::from(shard_data.as_slice()))
            .await
            .unwrap();

        let mut sync = ShardSynchronizer::new(router, cache, Some(metrics.clone()));
        let mut idx = ChunkIndex::new();

        let shard_list = make_shard_list(1, &[hex]);
        sync.sync(&mut idx, &shard_list).await.unwrap();

        let snap = metrics.snapshot();
        assert_eq!(snap.chunk_index_shards_installed, 1);
    }

    // --- Incremental sync tests ---

    #[tokio::test]
    async fn incremental_sync_skips_all_when_generations_match() {
        let (router, cache, dir) = setup();
        let cache_dir = dir.path();

        let hash_a = MerkleHash::from([1u64, 2, 3, 4]);
        let hash_b = MerkleHash::from([5u64, 6, 7, 8]);
        let entries = vec![hash_a.hex(), hash_b.hex()];

        // Pre-populate the cached generation file.
        let gen_path = cache_dir
            .join("repos")
            .join("testhash")
            .join("shard-list-gen.json");
        let cached = CachedShardListGen {
            generation: 5,
            shard_hashes: entries.clone(),
        };
        save_cached_generation(&gen_path, &cached);

        // Set up a PersistentChunkIndex with both shards marked as
        // installed. Without this, the sync correctly falls through to
        // full download because it can't verify the shards are locally
        // available.
        let pi_path = cache_dir
            .join("repos")
            .join("testhash")
            .join("chunk-index.sqlite");
        let pi = PersistentChunkIndex::open_or_create(&pi_path).unwrap();
        // Install both shards with dummy entries so has_shard returns true.
        let dummy_hash = MerkleHash::from([99u64, 99, 99, 99]);
        let dummy_ref = crab_xet::xorb::format::XorbRef {
            xorb_hash: dummy_hash,
            chunk_index: 0,
            uncompressed_size: 1024,
        };
        pi.install_shard(hash_a, &[(dummy_hash, dummy_ref)])
            .unwrap();
        pi.install_shard(hash_b, &[(dummy_hash, dummy_ref)])
            .unwrap();

        let mut sync = ShardSynchronizer::new(router, cache, None)
            .with_repo_cache_dir(cache_dir, "testhash")
            .with_persistent_index(Arc::new(pi));
        let mut idx = ChunkIndex::new();

        // Remote generation matches cached — should skip all because
        // the persistent index confirms both shards are installed.
        let shard_list = make_shard_list(5, &entries);
        let stats = sync.sync(&mut idx, &shard_list).await.unwrap();
        assert_eq!(stats.shards_downloaded, 0);
        assert_eq!(stats.shards_skipped, 2);
        assert_eq!(stats.shards_failed, 0);
    }

    #[tokio::test]
    async fn incremental_sync_downloads_only_delta_when_remote_ahead() {
        let (router, cache, dir) = setup();
        let cache_dir = dir.path();

        let hash_a = MerkleHash::from([1u64, 2, 3, 4]);
        let hash_b = MerkleHash::from([5u64, 6, 7, 8]);

        // Cached generation has only hash_a at gen 3.
        let gen_path = cache_dir
            .join("repos")
            .join("testhash")
            .join("shard-list-gen.json");
        let cached = CachedShardListGen {
            generation: 3,
            shard_hashes: vec![hash_a.hex()],
        };
        save_cached_generation(&gen_path, &cached);

        // Remote has both hash_a and hash_b at gen 5.
        // hash_b is new — it should be the only one in the delta.
        // hash_b won't be found in the store (404), so it'll be counted
        // as failed, but the point is hash_a is skipped.
        let mut sync =
            ShardSynchronizer::new(router, cache, None).with_repo_cache_dir(cache_dir, "testhash");
        let mut idx = ChunkIndex::new();

        let shard_list = make_shard_list(5, &[hash_a.hex(), hash_b.hex()]);
        let stats = sync.sync(&mut idx, &shard_list).await.unwrap();
        // hash_a skipped via incremental delta, hash_b attempted download (404 = failed)
        assert_eq!(stats.shards_skipped, 1);
        assert_eq!(stats.shards_failed, 1);
        assert_eq!(stats.shards_downloaded, 0);
    }

    #[tokio::test]
    async fn incremental_sync_falls_back_on_missing_generation() {
        let (router, cache, dir) = setup();
        let cache_dir = dir.path();

        // No cached generation file — should fall back to full sync.
        let hash = MerkleHash::from([42u64, 42, 42, 42]);
        let mut sync =
            ShardSynchronizer::new(router, cache, None).with_repo_cache_dir(cache_dir, "testhash");
        let mut idx = ChunkIndex::new();

        let shard_list = make_shard_list(1, &[hash.hex()]);
        let stats = sync.sync(&mut idx, &shard_list).await.unwrap();
        // Full sync attempted — hash not in store, so failed.
        assert_eq!(stats.shards_failed, 1);
    }

    #[tokio::test]
    async fn incremental_sync_falls_back_on_corrupt_generation() {
        let (router, cache, dir) = setup();
        let cache_dir = dir.path();

        // Write corrupt data to the generation file.
        let gen_path = cache_dir
            .join("repos")
            .join("testhash")
            .join("shard-list-gen.json");
        std::fs::create_dir_all(gen_path.parent().unwrap()).unwrap();
        std::fs::write(&gen_path, b"not valid json").unwrap();

        let hash = MerkleHash::from([42u64, 42, 42, 42]);
        let mut sync =
            ShardSynchronizer::new(router, cache, None).with_repo_cache_dir(cache_dir, "testhash");
        let mut idx = ChunkIndex::new();

        let shard_list = make_shard_list(1, &[hash.hex()]);
        let stats = sync.sync(&mut idx, &shard_list).await.unwrap();
        // Full sync attempted — hash not in store, so failed.
        assert_eq!(stats.shards_failed, 1);
    }

    #[tokio::test]
    async fn incremental_sync_persists_generation_after_sync() {
        let (router, cache, dir) = setup();
        let cache_dir = dir.path();

        let shard_data = b"persist gen test";
        let hash = compute_data_hash(shard_data);
        let obj_path = router.shard_path(&hash);
        router
            .store()
            .put(&obj_path, Bytes::from(shard_data.as_slice()))
            .await
            .unwrap();

        let mut sync =
            ShardSynchronizer::new(router, cache, None).with_repo_cache_dir(cache_dir, "testhash");
        let mut idx = ChunkIndex::new();

        let shard_list = make_shard_list(7, &[hash.hex()]);
        sync.sync(&mut idx, &shard_list).await.unwrap();

        // Verify the generation was persisted.
        let gen_path = cache_dir
            .join("repos")
            .join("testhash")
            .join("shard-list-gen.json");
        let saved = load_cached_generation(&gen_path).unwrap();
        assert_eq!(saved.generation, 7);
        assert_eq!(saved.shard_hashes, vec![hash.hex()]);
    }

    #[test]
    fn cached_generation_save_overwrites_without_tempfile_leftover() {
        let dir = TempDir::new().unwrap();
        let gen_path = dir.path().join("repos/testhash/shard-list-gen.json");
        let first = CachedShardListGen {
            generation: 1,
            shard_hashes: vec!["a".to_owned()],
        };
        let second = CachedShardListGen {
            generation: 2,
            shard_hashes: vec!["b".to_owned(), "c".to_owned()],
        };

        save_cached_generation(&gen_path, &first);
        save_cached_generation(&gen_path, &second);

        let saved = load_cached_generation(&gen_path).unwrap();
        assert_eq!(saved.generation, 2);
        assert_eq!(saved.shard_hashes, second.shard_hashes);

        let files: Vec<_> = std::fs::read_dir(gen_path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(files, vec![std::ffi::OsString::from("shard-list-gen.json")]);
    }

    #[tokio::test]
    async fn incremental_sync_without_repo_cache_dir_does_full_sync() {
        let (router, cache, _dir) = setup();

        // No with_repo_cache_dir call — should do full sync (no generation caching).
        let hash = MerkleHash::from([42u64, 42, 42, 42]);
        let mut sync = ShardSynchronizer::new(router, cache, None);
        let mut idx = ChunkIndex::new();

        let shard_list = make_shard_list(1, &[hash.hex()]);
        let stats = sync.sync(&mut idx, &shard_list).await.unwrap();
        // Full sync — hash not in store, so failed.
        assert_eq!(stats.shards_failed, 1);
    }

    // --- Bloom-first filtering tests ---

    /// Build a v2 shard with a bloom filter from the given chunk hashes.
    /// Returns `(shard_bytes, shard_hash)`.
    fn build_v2_shard(chunk_hashes: &[MerkleHash]) -> (Vec<u8>, MerkleHash) {
        use crab_xet::shard::ShardWriter;

        let w = ShardWriter::new();
        w.finalize_with_bloom(&[], chunk_hashes).unwrap()
    }

    /// Build a v1 shard (no bloom). Returns `(shard_bytes, shard_hash)`.
    fn build_v1_shard() -> (Vec<u8>, MerkleHash) {
        use crab_xet::shard::ShardWriter;

        let w = ShardWriter::new();
        w.finalize().unwrap()
    }

    fn build_shard_with_one_chunk() -> (
        Vec<u8>,
        MerkleHash,
        MerkleHash,
        crab_xet::xorb::format::XorbRef,
    ) {
        use crab_xet::shard::ShardWriter;
        use crab_xet::shard::{MDBXorbInfo, XorbChunkSequenceEntry, XorbChunkSequenceHeader};

        let xorb_hash = MerkleHash::from([7000, 7000, 7000, 7000]);
        let chunk_hash = MerkleHash::from([7001, 7001, 7001, 7001]);
        let chunk_size = 1024_u32;
        let xorb = Arc::new(MDBXorbInfo {
            metadata: XorbChunkSequenceHeader::new(xorb_hash, 1, chunk_size as usize),
            chunks: vec![XorbChunkSequenceEntry::new(chunk_hash, chunk_size, 0)],
        });
        let mut writer = ShardWriter::new();
        writer.add_xorb(xorb).unwrap();
        let (bytes, shard_hash) = writer.finalize().unwrap();
        (
            bytes,
            shard_hash,
            chunk_hash,
            crab_xet::xorb::format::XorbRef {
                xorb_hash,
                chunk_index: 0,
                uncompressed_size: chunk_size,
            },
        )
    }

    #[tokio::test]
    async fn bloom_filter_skips_shard_when_no_chunk_matches() {
        let (router, cache, _dir) = setup();

        // Build a v2 shard with 500 chunk hashes to create a large bloom
        // filter where false positives for unrelated hashes are negligible.
        let shard_chunks: Vec<MerkleHash> = (10_000..10_500)
            .map(|i: u64| {
                MerkleHash::from([
                    i,
                    i.wrapping_mul(31),
                    i.wrapping_mul(97),
                    i.wrapping_mul(127),
                ])
            })
            .collect();
        let (shard_bytes, shard_hash) = build_v2_shard(&shard_chunks);

        let obj_path = router.shard_path(&shard_hash);
        router
            .store()
            .put(&obj_path, Bytes::from(shard_bytes))
            .await
            .unwrap();

        // Single query hash from a completely different range.
        let query: HashSet<MerkleHash> = [MerkleHash::from([
            99_999u64,
            99_999u64.wrapping_mul(31),
            99_999u64.wrapping_mul(97),
            99_999u64.wrapping_mul(127),
        ])]
        .into_iter()
        .collect();

        let mut sync = ShardSynchronizer::new(router, cache, None).with_query_hashes(query);
        let mut idx = ChunkIndex::new();

        let shard_list = make_shard_list(1, &[shard_hash.hex()]);
        let stats = sync.sync(&mut idx, &shard_list).await.unwrap();

        assert_eq!(stats.shards_bloom_skipped, 1);
        assert_eq!(stats.shards_downloaded, 0);
        assert!(!idx.has_shard(&shard_hash));
    }

    #[tokio::test]
    async fn bloom_filter_downloads_shard_when_chunk_may_match() {
        let (router, cache, _dir) = setup();

        // Build a v2 shard whose bloom contains 100 chunk hashes.
        let shard_chunks: Vec<MerkleHash> = (1000..1100)
            .map(|i: u64| {
                MerkleHash::from([
                    i,
                    i.wrapping_mul(31),
                    i.wrapping_mul(97),
                    i.wrapping_mul(127),
                ])
            })
            .collect();
        let (shard_bytes, shard_hash) = build_v2_shard(&shard_chunks);

        let obj_path = router.shard_path(&shard_hash);
        router
            .store()
            .put(&obj_path, Bytes::from(shard_bytes))
            .await
            .unwrap();

        // Query includes hash 1050 which IS in the shard's bloom.
        let query: HashSet<MerkleHash> = [MerkleHash::from([
            1050u64,
            1050u64.wrapping_mul(31),
            1050u64.wrapping_mul(97),
            1050u64.wrapping_mul(127),
        ])]
        .into_iter()
        .collect();

        let mut sync = ShardSynchronizer::new(router, cache, None).with_query_hashes(query);
        let mut idx = ChunkIndex::new();

        let shard_list = make_shard_list(1, &[shard_hash.hex()]);
        let stats = sync.sync(&mut idx, &shard_list).await.unwrap();

        assert_eq!(stats.shards_bloom_skipped, 0);
        assert_eq!(stats.shards_downloaded, 1);
        assert!(!idx.has_shard(&shard_hash));
    }

    #[tokio::test]
    async fn bloom_filter_falls_back_for_v1_shard() {
        let (router, cache, _dir) = setup();

        // Build a v1 shard (no bloom).
        let (shard_bytes, shard_hash) = build_v1_shard();

        let obj_path = router.shard_path(&shard_hash);
        router
            .store()
            .put(&obj_path, Bytes::from(shard_bytes))
            .await
            .unwrap();

        // Even with query hashes set, v1 shards should be downloaded normally.
        let query: HashSet<MerkleHash> =
            (200..210).map(|i| MerkleHash::from([i, i, i, i])).collect();

        let mut sync = ShardSynchronizer::new(router, cache, None).with_query_hashes(query);
        let mut idx = ChunkIndex::new();

        let shard_list = make_shard_list(1, &[shard_hash.hex()]);
        let stats = sync.sync(&mut idx, &shard_list).await.unwrap();

        assert_eq!(stats.shards_bloom_skipped, 0);
        assert_eq!(stats.shards_downloaded, 1);
        assert!(!idx.has_shard(&shard_hash));
    }

    #[tokio::test]
    async fn query_scoped_sync_does_not_mark_persistent_shard_complete() {
        let (router, cache, dir) = setup();

        let shard_chunks: Vec<MerkleHash> = (3000..3100)
            .map(|i: u64| {
                MerkleHash::from([
                    i,
                    i.wrapping_mul(31),
                    i.wrapping_mul(97),
                    i.wrapping_mul(127),
                ])
            })
            .collect();
        let (shard_bytes, shard_hash) = build_v2_shard(&shard_chunks);

        let obj_path = router.shard_path(&shard_hash);
        router
            .store()
            .put(&obj_path, Bytes::from(shard_bytes.clone()))
            .await
            .unwrap();

        let persistent_path = dir
            .path()
            .join("repos")
            .join("testhash")
            .join("chunk-index.sqlite");
        let persistent = Arc::new(PersistentChunkIndex::open_or_create(&persistent_path).unwrap());
        let query: HashSet<MerkleHash> = [shard_chunks[0]].into_iter().collect();
        let mut sync = ShardSynchronizer::new(router, cache, None)
            .with_query_hashes(query)
            .with_persistent_index(Arc::clone(&persistent));
        let mut idx = ChunkIndex::new();

        let shard_list = make_shard_list(1, &[shard_hash.hex()]);
        let stats = sync.sync(&mut idx, &shard_list).await.unwrap();

        assert_eq!(stats.shards_downloaded, 1);
        assert!(!idx.has_shard(&shard_hash));
        assert!(!persistent.has_shard(&shard_hash).unwrap());

        sync.install_shard_data(&shard_hash, &Bytes::from(shard_bytes), &mut idx, false);

        assert!(idx.has_shard(&shard_hash));
        assert!(persistent.has_shard(&shard_hash).unwrap());
    }

    #[tokio::test]
    async fn exact_query_sync_installs_every_requested_match_only() {
        let (router, cache, _dir) = setup();
        let (shard_bytes, shard_hash, requested, expected_ref) = build_shard_with_one_chunk();
        router
            .store()
            .put(&router.shard_path(&shard_hash), Bytes::from(shard_bytes))
            .await
            .unwrap();
        let mut sync = ShardSynchronizer::new(router, cache, None)
            .with_exact_query_hashes([requested].into_iter().collect());
        let mut idx = ChunkIndex::new();

        let stats = sync
            .sync(&mut idx, &make_shard_list(1, &[shard_hash.hex()]))
            .await
            .unwrap();

        assert_eq!(stats.shards_downloaded, 1);
        assert_eq!(idx.get(&requested), Some(&expected_ref));
        assert_eq!(idx.len(), 1);
        assert!(!idx.has_shard(&shard_hash));
    }

    #[tokio::test]
    async fn full_shard_install_repairs_marker_only_persistent_index() {
        let (router, cache, dir) = setup();
        let (shard_bytes, shard_hash, chunk_hash, xorb_ref) = build_shard_with_one_chunk();
        let persistent_path = dir
            .path()
            .join("repos")
            .join("testhash")
            .join("chunk-index.sqlite");
        let persistent = Arc::new(PersistentChunkIndex::open_or_create(&persistent_path).unwrap());

        persistent.install_shard(shard_hash, &[]).unwrap();
        assert!(persistent.has_shard(&shard_hash).unwrap());
        assert!(persistent.get(&chunk_hash).unwrap().is_none());

        let mut sync = ShardSynchronizer::new(router, cache, None)
            .with_persistent_index(Arc::clone(&persistent));
        let mut idx = ChunkIndex::new();

        sync.install_shard_data(&shard_hash, &Bytes::from(shard_bytes), &mut idx, false);

        assert_eq!(persistent.get(&chunk_hash).unwrap(), Some(xorb_ref));
        assert_eq!(idx.get(&chunk_hash).copied(), Some(xorb_ref));
        assert!(idx.has_shard(&shard_hash));
    }

    #[tokio::test]
    async fn bloom_filter_disabled_without_query_hashes() {
        let (router, cache, _dir) = setup();

        // Build a v2 shard with bloom.
        let shard_chunks: Vec<MerkleHash> = (1000..1100)
            .map(|i: u64| {
                MerkleHash::from([
                    i,
                    i.wrapping_mul(31),
                    i.wrapping_mul(97),
                    i.wrapping_mul(127),
                ])
            })
            .collect();
        let (shard_bytes, shard_hash) = build_v2_shard(&shard_chunks);

        let obj_path = router.shard_path(&shard_hash);
        router
            .store()
            .put(&obj_path, Bytes::from(shard_bytes))
            .await
            .unwrap();

        // No with_query_hashes — bloom filtering should be skipped entirely.
        let mut sync = ShardSynchronizer::new(router, cache, None);
        let mut idx = ChunkIndex::new();

        let shard_list = make_shard_list(1, &[shard_hash.hex()]);
        let stats = sync.sync(&mut idx, &shard_list).await.unwrap();

        assert_eq!(stats.shards_bloom_skipped, 0);
        assert_eq!(stats.shards_downloaded, 1);
        assert!(idx.has_shard(&shard_hash));
    }

    // --- On-disk shard fallback tests ---

    #[tokio::test]
    async fn shard_spills_to_disk_when_chunk_index_over_ceiling() {
        let (router, cache, dir) = setup();
        let shard_cache_dir = dir.path().join("repos").join("testhash").join("shards");

        // Build a valid v1 shard and upload it.
        let (shard_bytes, shard_hash) = build_v1_shard();
        let obj_path = router.shard_path(&shard_hash);
        router
            .store()
            .put(&obj_path, Bytes::from(shard_bytes))
            .await
            .unwrap();

        // Use a ChunkIndex with a very low ceiling and pre-populate it
        // with entries so it's already over the ceiling before sync.
        let mut idx = ChunkIndex::with_ceiling(40);
        let dummy_hash = MerkleHash::from([1u64, 1, 1, 1]);
        let dummy_ref = crab_xet::xorb::format::XorbRef {
            xorb_hash: MerkleHash::from([2u64, 2, 2, 2]),
            chunk_index: 0,
            uncompressed_size: 100,
        };
        idx.insert(dummy_hash, dummy_ref);
        idx.insert(MerkleHash::from([3u64, 3, 3, 3]), dummy_ref);
        assert!(idx.over_ceiling());

        let mut sync = ShardSynchronizer::new(router, cache, None)
            .with_shard_cache_dir(shard_cache_dir.clone());

        let shard_list = make_shard_list(1, &[shard_hash.hex()]);
        let stats = sync.sync(&mut idx, &shard_list).await.unwrap();

        assert_eq!(stats.shards_downloaded, 1);
        // The shard is marked as installed (so it won't be re-downloaded).
        assert!(idx.has_shard(&shard_hash));
        // An MDBShardFile handle was created for on-disk querying.
        assert_eq!(sync.shard_files().len(), 1);
        // The shard cache directory was created.
        assert!(shard_cache_dir.exists());
    }

    #[tokio::test]
    async fn shard_loads_into_memory_when_under_ceiling() {
        let (router, cache, dir) = setup();
        let shard_cache_dir = dir.path().join("repos").join("testhash").join("shards");

        // Build a valid v1 shard and upload it.
        let (shard_bytes, shard_hash) = build_v1_shard();
        let obj_path = router.shard_path(&shard_hash);
        router
            .store()
            .put(&obj_path, Bytes::from(shard_bytes))
            .await
            .unwrap();

        // Default ceiling (1 GiB) — should load into memory, not disk.
        let mut idx = ChunkIndex::new();

        let mut sync =
            ShardSynchronizer::new(router, cache, None).with_shard_cache_dir(shard_cache_dir);

        let shard_list = make_shard_list(1, &[shard_hash.hex()]);
        let stats = sync.sync(&mut idx, &shard_list).await.unwrap();

        assert_eq!(stats.shards_downloaded, 1);
        assert!(idx.has_shard(&shard_hash));
        // No on-disk fallback handles created.
        assert_eq!(sync.shard_files().len(), 0);
    }

    #[tokio::test]
    async fn shard_falls_back_to_memory_without_shard_cache_dir() {
        let (router, cache, _dir) = setup();

        // Build a valid v1 shard and upload it.
        let (shard_bytes, shard_hash) = build_v1_shard();
        let obj_path = router.shard_path(&shard_hash);
        router
            .store()
            .put(&obj_path, Bytes::from(shard_bytes))
            .await
            .unwrap();

        // Ceiling of 0 but no shard_cache_dir — should fall back to
        // in-memory install since there's nowhere to spill.
        let mut idx = ChunkIndex::with_ceiling(0);

        let mut sync = ShardSynchronizer::new(router, cache, None);

        let shard_list = make_shard_list(1, &[shard_hash.hex()]);
        let stats = sync.sync(&mut idx, &shard_list).await.unwrap();

        assert_eq!(stats.shards_downloaded, 1);
        assert!(idx.has_shard(&shard_hash));
        assert_eq!(sync.shard_files().len(), 0);
    }
}
