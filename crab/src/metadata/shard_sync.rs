//! Cross-client shard synchronization.
//!
//! `ShardSynchronizer` computes the delta between the remote shard list
//! and locally cached shards, downloads missing shards in parallel with
//! hash verification, and incrementally refreshes the `ChunkIndex` after
//! each install. Race-window handling (listed-but-not-yet-visible shards)
//! is built in: a 404 on a listed shard is logged and skipped, not fatal.
//!
//! Candidate shards come only from an authenticated Capsule pointer catalog.
//! Installed-shard receipts and verified local bytes avoid repeat downloads;
//! repository root generation cannot key independently mutable ref catalogs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use tracing::{debug, info, warn};

use crate::cache::{CacheKey, LocalCache};
use crate::core::error::{CrabError, Result};
use crate::core::metrics::Metrics;
use crate::storage::StoreLayout;
use crab_metadata::bloom_prefilter::{BloomCheck, check_shard_chunk_bloom};
use crab_metadata::chunk_index::ChunkIndex;
use crab_metadata::persistent_chunk_index::PersistentChunkIndex;
use crab_xet::hash::compute_data_hash;
use crab_xet::shard::{MDBMinimalShard, MDBShardFile, new_shard_file_cache};
use crab_xet::shard_parse::MAX_SHARD_SIZE_BYTES;
use crab_xet::xorb::format::{MerkleHash, XorbRef};

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

/// Cross-client shard synchronizer.
///
/// Computes the remote-vs-local shard delta, downloads missing shards
/// in parallel, hash-verifies each download, installs into the local
/// cache, and incrementally refreshes the `ChunkIndex`.
///
pub struct ShardSynchronizer {
    router: StoreLayout,
    cache: Arc<LocalCache>,
    persistent_index: Option<Arc<PersistentChunkIndex>>,
    metrics: Option<Arc<Metrics>>,
    /// Maximum number of concurrent shard downloads.
    parallelism: usize,
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

    /// Synchronize shards from the remote shard list into the local cache
    /// and chunk index.
    ///
    /// Installed-shard receipts and verified local cache entries determine
    /// which authenticated candidates still need downloading.
    ///
    /// Returns statistics about the sync operation.
    ///
    /// # Errors
    ///
    /// Individual download and local-cache failures are counted in the result.
    /// Individual shard download failures are counted in `shards_failed`
    /// rather than aborting the entire sync.
    pub async fn sync(
        &mut self,
        chunk_index: &mut ChunkIndex,
        remote_shards: &[String],
    ) -> Result<SyncStats> {
        let mut stats = SyncStats::default();
        let to_download = self
            .compute_delta(chunk_index, remote_shards, &mut stats)
            .await;

        if to_download.is_empty() {
            debug!(
                skipped = stats.shards_skipped,
                "shard sync: all shards up to date"
            );

            return Ok(stats);
        }

        // Apply bloom-first filtering: for shards with a v1 bloom trailer,
        // download only the bloom section and skip the full shard when the
        // bloom reports no matches against the query hashes.
        let to_download = self.bloom_filter_delta(to_download, &mut stats).await;

        if to_download.is_empty() {
            debug!(
                skipped = stats.shards_skipped,
                bloom_skipped = stats.shards_bloom_skipped,
                "shard sync: all shards up to date after bloom filtering"
            );

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

        Ok(stats)
    }

    /// Apply bloom-first filtering to the delta set.
    ///
    /// For each shard in the download list, reads a small suffix via Range
    /// GET to check for a v1 bloom trailer. If a bloom exists, downloads
    /// only the bloom section and queries it with the push's chunk hashes.
    /// Shards whose bloom reports no matches are skipped entirely.
    ///
    /// Falls back to full download when:
    /// - No query hashes were provided (bloom filtering disabled)
    /// - The shard has no v1 bloom trailer
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
                        // The persistent receipt covers the complete shard.
                        // Its entries stay on disk; delta discovery only needs
                        // the receipt, not an in-memory copy of every chunk.
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

        let mut cursor = std::io::Cursor::new(strip_bloom_trailer(data));
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
    let (body, _etag) = store
        .get_with_etag_bounded(shard_path, MAX_SHARD_SIZE_BYTES as u64)
        .await
        .map_err(|e| {
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
/// small for a v1 trailer. Returns `Err` on I/O or parse failures (the
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
    let v1_data = strip_bloom_trailer(data);
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

/// Return the xet shard body without Crab's optional canonical v1 bloom trailer.
fn strip_bloom_trailer(data: &[u8]) -> &[u8] {
    crab_xet::shard_parse::strip_bloom_trailer(data)
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
/// [`PersistentChunkIndex::has_shard`], downloads the missing
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
/// Repository snapshot failures are returned to the caller. Clone/fetch may
/// report them as a post-fetch cache-warming warning only after the canonical
/// repository snapshot has already been admitted. Individual shard and local
/// cache failures remain optimisation misses. Canonical Capsule read and
/// publication checks remain responsible for content and placement integrity.
pub async fn run_post_fetch_shard_sync(
    router: StoreLayout,
    repo_hash: &str,
    cache_dir: &Path,
    metrics: Option<Arc<Metrics>>,
    emit_progress: bool,
) -> Result<SyncStats> {
    let shard_hashes = load_published_shards(&router).await?;

    if shard_hashes.is_empty() {
        debug!("post-fetch shard sync: repository has no shards, nothing to sync");
        return Ok(SyncStats::default());
    }

    // Admit the shared private root before opening the derived index so a
    // cold sync cannot consume local state beneath an unsafe cache root.
    // Reject unsafe roots without changing them or opening the derived index.
    if let Err(error) = crab_cache::ensure_private_cache_directory(cache_dir) {
        warn!(error = %error, "post-fetch shard sync: private cache unavailable, skipping");
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
            // publication still verifies placement when warming is unavailable.
            warn!(
                error = %e,
                path = %index_path.display(),
                "post-fetch shard sync: failed to open persistent chunk index, skipping"
            );
            return Ok(SyncStats::default());
        }
    };

    // This index is discarded after warming. Persistent receipts already
    // suppress repeat downloads, so copying the bucket's unrelated chunks
    // here only makes fetch memory and latency grow with the entire cache.
    let mut chunk_index = ChunkIndex::new();

    let local_cache = Arc::new(LocalCache::new(cache_dir.to_path_buf()));
    let shard_cache_dir = cache_dir.join("repos").join(repo_hash).join("shards");

    let mut synchronizer = ShardSynchronizer::new(router, local_cache, metrics)
        .with_persistent_index(Arc::clone(&persistent))
        .with_shard_cache_dir(shard_cache_dir);

    let stats = synchronizer.sync(&mut chunk_index, &shard_hashes).await?;

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

async fn load_published_shards(router: &StoreLayout) -> Result<Vec<String>> {
    let layout = crab_storage::StoreLayout::with_global_prefix(
        router.store().as_storage().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    let root = crab_metadata::capsule_protocol::load_root(&layout).await?;
    let inspection =
        crab_metadata::capsule_protocol::inspect_pointer_catalog_from_root(&layout, &root).await?;
    inspection.into_shard_hashes().await.map_err(Into::into)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#[path = "shard_sync_tests.rs"]
mod tests;
