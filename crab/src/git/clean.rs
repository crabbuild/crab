//! Clean pipeline: streaming chunker + blake3 + staging + index.db + pointer.
//!
//! The clean path runs at `git add` time. It hashes file content via blake3,
//! chunks it via CDC, stages chunks locally, records them in index.db, and
//! emits a pointer blob. The slow path is local-only; the fast path
//! performs bounded remote metadata/object HEADs before it skips staging.
//!
//! The fast path skips staging for files already known to
//! the file-index: when the file size exceeds `fastpath_min_size` and the
//! session-scoped bloom filter reports a hit, a HEAD request to the
//! file-index confirms existence and the referenced shard object exists.
//! On success the pointer is emitted directly with a `shard-hint`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::cache::ShardHintCache;
use crate::core::context::AppContext;
use crate::core::error::{CrabError, Result};
use crate::core::pattern::PatternFilter;
use crate::storage::StoreLayout;
use bytes::{Bytes, BytesMut};
use crab_git::lfs_pointer::{LfsPointer, MAX_LFS_POINTER_SIZE};
use crab_git::pointer_detect::{PointerKind, classify};
use crab_staging::StagingArea;

use crab_types::pointer::Pointer;
use crab_xet::hash::MerkleHash;
use gix_object::Find;

/// Default minimum file size for the fast path (64 MiB).
pub const DEFAULT_FASTPATH_MIN_SIZE: u64 = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Session-scoped bloom filter
// ---------------------------------------------------------------------------

/// Lightweight bloom filter tracking known `file_hash` values from the local
/// file-index. Populated at session start and consulted on each clean to
/// decide whether the fast path is worth attempting.
///
/// Uses a simple bit-vector with k=3 hash functions derived from the
/// blake3 file hash. False positives are expected and tracked via the
/// `clean_fastpath_false_positives` counter.
#[derive(Debug)]
struct FileHashBloom {
    bits: Vec<u64>,
    num_bits: u64,
}

impl FileHashBloom {
    /// Create a bloom filter sized for `expected_items` with ~1% FPR.
    fn new(expected_items: usize) -> Self {
        // ~10 bits per item for ~1% FPR with k=3.
        let num_bits = ((expected_items as u64).max(64)) * 10;
        let words = num_bits.div_ceil(64) as usize;
        Self {
            bits: vec![0u64; words],
            num_bits,
        }
    }

    /// Insert a file hash into the bloom filter.
    fn insert(&mut self, file_hash: &[u8; 32]) {
        for idx in self.hash_indices(file_hash) {
            let word = (idx / 64) as usize;
            let bit = idx % 64;
            if word < self.bits.len() {
                self.bits[word] |= 1 << bit;
            }
        }
    }

    /// Check if a file hash might be in the bloom filter.
    ///
    /// Returns `false` for definite misses, `true` for possible hits
    /// (which may be false positives).
    fn maybe_contains(&self, file_hash: &[u8; 32]) -> bool {
        for idx in self.hash_indices(file_hash) {
            let word = (idx / 64) as usize;
            let bit = idx % 64;
            if word >= self.bits.len() || (self.bits[word] & (1 << bit)) == 0 {
                return false;
            }
        }
        true
    }

    /// Derive k=3 bit indices from the 32-byte hash.
    fn hash_indices(&self, hash: &[u8; 32]) -> [u64; 3] {
        // Use non-overlapping 8-byte slices from the hash as independent
        // hash values, then reduce modulo num_bits.
        let h1 = u64::from_le_bytes(hash[0..8].try_into().unwrap_or([0; 8]));
        let h2 = u64::from_le_bytes(hash[8..16].try_into().unwrap_or([0; 8]));
        let h3 = u64::from_le_bytes(hash[16..24].try_into().unwrap_or([0; 8]));
        [h1 % self.num_bits, h2 % self.num_bits, h3 % self.num_bits]
    }

    fn encode(&self) -> Vec<u8> {
        let mut data = Vec::with_capacity(8 + self.bits.len() * 8);
        data.extend_from_slice(&self.num_bits.to_le_bytes());
        for word in &self.bits {
            data.extend_from_slice(&word.to_le_bytes());
        }
        data
    }

    fn decode(data: &[u8]) -> Option<Self> {
        if data.len() < 8 {
            return None;
        }
        let num_bits = u64::from_le_bytes(data[0..8].try_into().ok()?);
        if num_bits == 0 {
            return None;
        }
        let words_bytes = &data[8..];
        if words_bytes.len() % 8 != 0 {
            return None;
        }
        let bits: Vec<u64> = words_bytes
            .chunks_exact(8)
            .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap_or([0; 8])))
            .collect();
        let expected_words = num_bits.div_ceil(64) as usize;
        if bits.len() != expected_words {
            return None;
        }
        Some(Self { bits, num_bits })
    }
}

// ---------------------------------------------------------------------------
// File-index checker
// ---------------------------------------------------------------------------

/// Result of a file-index lookup for a given file hash.
#[derive(Debug)]
pub struct FileIndexHeadResult {
    /// Whether the file hash exists in the file-index.
    pub exists: bool,
    /// Shard hash hint from the response headers, if available.
    pub shard_hint: Option<[u8; 32]>,
}

/// Trait abstracting the file-index fast-path check for testability.
///
/// The real implementation resolves `file_hash -> shard_hash` through
/// `file_index_db` and verifies the referenced shard object exists.
pub trait FileIndexChecker: Send {
    /// Check whether the file is already reconstructable from remote storage.
    ///
    /// Returns existence + optional shard-hint on success.
    fn head_file_index(&self, file_hash: &[u8; 32]) -> Result<FileIndexHeadResult>;
}

/// No-op checker that always reports files as unknown (disables fast path).
#[derive(Debug, Default)]
struct NoopFileIndexChecker;

impl FileIndexChecker for NoopFileIndexChecker {
    fn head_file_index(&self, _file_hash: &[u8; 32]) -> Result<FileIndexHeadResult> {
        Ok(FileIndexHeadResult {
            exists: false,
            shard_hint: None,
        })
    }
}

/// File-index checker backed by a [`StoreLayout`].
///
/// Routes file-hash existence checks through the per-repo
/// `file_index_db` via
/// [`crab_metadata::file_index_lookup::resolve_file_hash_to_shard`].
/// Uses `Handle::block_on` to bridge the sync [`FileIndexChecker`]
/// trait with the async SlateDB access path — safe because the
/// filter-process loop invokes the checker inside `spawn_blocking`.
pub struct StoreFileIndexChecker {
    router: StoreLayout,
    handle: tokio::runtime::Handle,
    session: std::sync::Mutex<Option<crab_metadata::file_index_lookup::FileIndexLookupSession>>,
}

impl StoreFileIndexChecker {
    /// Create a checker that routes lookups via the given layout.
    ///
    /// `handle` must be a handle to the tokio runtime that owns the store.
    pub fn new(router: StoreLayout, handle: tokio::runtime::Handle) -> Self {
        Self {
            router,
            handle,
            session: std::sync::Mutex::new(None),
        }
    }
}

impl Drop for StoreFileIndexChecker {
    fn drop(&mut self) {
        let session = self
            .session
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(session) = session {
            let handle = self.handle.clone();
            let close = std::thread::Builder::new()
                .name("crab-file-index-close".to_owned())
                .spawn(move || handle.block_on(session.close()));
            match close {
                Ok(thread) => match thread.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(error = %error, "clean file-index reader close failed");
                    }
                    Err(_) => tracing::warn!("clean file-index close worker panicked"),
                },
                Err(error) => {
                    tracing::warn!(error = %error, "failed to start clean file-index close worker");
                }
            }
        }
    }
}

// SAFETY: StoreLayout is Send (Arc-wrapped internals), and Handle is Send+Sync.
unsafe impl Send for StoreFileIndexChecker {}

impl FileIndexChecker for StoreFileIndexChecker {
    fn head_file_index(&self, file_hash: &[u8; 32]) -> Result<FileIndexHeadResult> {
        // Convert the raw 32-byte file hash to the MerkleHash the helper
        // expects. The codec on the metadb side matches this layout.
        let merkle = MerkleHash::from(*file_hash);
        let mut session = self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if session.is_none() {
            *session = Some(self.handle.block_on(
                crab_metadata::file_index_lookup::FileIndexLookupSession::open(
                    Arc::clone(self.router.store().inner()),
                    self.router.repo_prefix(),
                ),
            )?);
        }
        let shard_hash = self.handle.block_on(
            session
                .as_ref()
                .ok_or_else(|| CrabError::Internal("file-index session missing".to_owned()))?
                .lookup(&merkle),
        )?;

        Ok(match shard_hash {
            Some(hash) => {
                let shard_path = self.router.shard_path(&hash);
                match self.handle.block_on(self.router.store().head(&shard_path)) {
                    Ok(_) => FileIndexHeadResult {
                        exists: true,
                        shard_hint: Some(hash.into()),
                    },
                    Err(CrabError::NotFound { .. }) => {
                        tracing::warn!(
                            file_hash = %merkle.hex(),
                            shard_hash = %hash.hex(),
                            "clean fast path: file-index hit points at missing shard"
                        );
                        FileIndexHeadResult {
                            exists: false,
                            shard_hint: None,
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
            None => FileIndexHeadResult {
                exists: false,
                shard_hint: None,
            },
        })
    }
}

fn clean_xorb_builder(ctx: &AppContext) -> crab_staging::ingest::StreamStageXorbBuilder {
    let config = crate::git::push::PushConfig::from_config(ctx.config());
    crab_staging::ingest::StreamStageXorbBuilder::new(1, move || {
        config.configure_builder(crab_xet::xorb::builder::XorbBuilder::with_policy(
            config.compression_policy(),
        ))
    })
}

/// Product adapter retaining Git publication policy outside shared ingestion.
struct CleanStaging {
    staging: Arc<StagingArea>,
    handle: tokio::runtime::Handle,
    #[cfg(test)]
    _temporary: Option<tempfile::TempDir>,
}

#[cfg(test)]
fn test_staging() -> CleanStaging {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    std::thread::spawn(|| {
        let runtime = RUNTIME.get_or_init(|| tokio::runtime::Runtime::new().unwrap());
        let temporary = tempfile::tempdir().unwrap();
        let staging = Arc::new(
            runtime
                .block_on(StagingArea::open(temporary.path().join("staging")))
                .unwrap(),
        );
        CleanStaging {
            staging,
            handle: runtime.handle().clone(),
            _temporary: Some(temporary),
        }
    })
    .join()
    .unwrap()
}

// ---------------------------------------------------------------------------
// CleanSession
// ---------------------------------------------------------------------------

/// Per-session state for the clean pipeline.
///
/// Maintains the bloom filter, shared staging resources, and metrics across multiple
/// clean operations within a single filter-process invocation. Supports
/// both crab-native (Blake3 + CDC) and LFS (SHA-256) clean paths,
/// selected per-file based on `.gitattributes` `filter=` settings.
pub struct CleanSession {
    ctx: AppContext,
    bloom: FileHashBloom,
    /// File hashes confirmed via HEAD, with their shard hint if the file-index has one.
    confirmed_hashes: HashMap<[u8; 32], Option<[u8; 32]>>,
    file_index_checker: Box<dyn FileIndexChecker>,
    staging: Option<CleanStaging>,
    xorb_builder: crab_staging::ingest::StreamStageXorbBuilder,
    fastpath_min_size: u64,
    /// Compiled include/exclude filter for auto-hydrate. `Some` when
    /// `hydrate.auto = true` and patterns compiled successfully.
    hydrate_filter: Option<PatternFilter>,
    /// Git LFS fetch include/exclude filters used by smudge paths.
    lfs_fetch_filter: Option<crate::lfs::fetch_filter::FetchPathFilter>,
    /// Preserve Git LFS's explicit skip-download-errors behavior for smudge.
    lfs_skip_download_errors: bool,
    /// Resolved local LFS storage for this filter-process session.
    lfs_storage_dir: Option<PathBuf>,
    /// Repository root directory, used to locate `.gitattributes`.
    /// `None` when the repo root could not be determined.
    repo_root: Option<PathBuf>,
    /// Cached LFS-tracked patterns from `.gitattributes`, parsed once at
    /// session start. Used by the legacy classifier and kept in both
    /// modes so the diagnostics in `set_repo_root` can report a count.
    lfs_patterns: Vec<String>,
    /// Consolidated attributes reader built by
    /// [`crate::core::attrs::AttrsReader`]. When `Some`, drives
    /// `is_lfs_tracked` in place of walking `lfs_patterns` with the
    /// hand-rolled glob matcher. Gated behind `gix-pathmatch` so the
    /// legacy classifier stays reachable until the flag flips
    /// default-on.
    ///
    /// Lazily initialized via `OnceLock`: the underlying tree walk +
    /// `gix_attributes` parse is only paid when `is_lfs_tracked` is
    /// actually called, which in current code paths is never (the
    /// filter-process dispatch uses `resolve_filter_for` →
    /// `FilterAttrCache` instead). Without this deferral the
    /// filter-process startup in `crab init` walks the entire working
    /// tree a third time for no benefit.
    #[cfg(feature = "gix-pathmatch")]
    lfs_attrs: std::sync::OnceLock<Option<Arc<crate::core::attrs::AttrsReader>>>,
    /// Persistent `file_hash → shard_hash` mapping used to populate the
    /// `shard-hint` field in emitted pointers. Loaded lazily via
    /// [`load_shard_hints_from_cache`](Self::load_shard_hints_from_cache);
    /// an empty cache (default) simply emits pointers without hints.
    shard_hints: ShardHintCache,
    /// Cached `.gitattributes` filter rules for LFS/XET dispatch.
    /// Built once at `set_repo_root` time and shared across all files
    /// in the session. Resolves `filter=lfs` vs `filter=crab` per file
    /// path with git's "last match wins" semantics.
    filter_attr_cache: Option<crate::git::filter_attr_cache::FilterAttrCache>,
    /// When `Some`, the backing staging area is unavailable for writes
    /// and the crab-native clean path must fail instead of producing
    /// a pointer that points at chunks we never staged. `None` means
    /// writable admission has not been rejected; novel content still requires
    /// an attached staging owner.
    ///
    /// The inner `Option<u32>` carries the holder PID when known, so
    /// the surfaced `CRAB-E0081` error tells the user which process
    /// to resolve.
    #[expect(
        clippy::option_option,
        reason = "outer None means writable staging; inner None means blocked by an unknown holder"
    )]
    staging_unavailable: Option<Option<u32>>,
}

impl CleanSession {
    /// Create a new session with default configuration.
    pub fn new(ctx: AppContext) -> Self {
        let fastpath_min_size = ctx.perf().effective_fastpath_min_size();

        let hydrate_filter = Self::compile_hydrate_filter(&ctx);
        let xorb_builder = clean_xorb_builder(&ctx);

        Self {
            ctx,
            bloom: FileHashBloom::new(1024),
            confirmed_hashes: HashMap::new(),
            file_index_checker: Box::new(NoopFileIndexChecker),
            #[cfg(test)]
            staging: Some(test_staging()),
            #[cfg(not(test))]
            staging: None,
            xorb_builder,
            fastpath_min_size,
            hydrate_filter,
            lfs_fetch_filter: None,
            lfs_skip_download_errors: false,
            lfs_storage_dir: None,
            repo_root: None,
            lfs_patterns: Vec::new(),
            #[cfg(feature = "gix-pathmatch")]
            lfs_attrs: std::sync::OnceLock::new(),
            filter_attr_cache: None,
            shard_hints: ShardHintCache::new(),
            staging_unavailable: None,
        }
    }

    /// The application context for this session.
    #[must_use]
    pub fn ctx(&self) -> &AppContext {
        &self.ctx
    }

    #[cfg(test)]
    fn with_deps(
        ctx: AppContext,
        checker: Box<dyn FileIndexChecker>,
        staging: Option<CleanStaging>,
        bloom_items: &[[u8; 32]],
        fastpath_min_size: u64,
    ) -> Self {
        let mut session = Self::new(ctx);
        session.file_index_checker = checker;
        session.staging = staging.or_else(|| Some(test_staging()));
        session.seed_bloom(bloom_items);
        session.fastpath_min_size = fastpath_min_size;
        session
    }

    /// Mark the session as having no writable staging area because
    /// another process holds the exclusive lock. The crab-native
    /// clean path will refuse with
    /// [`CrabError::StagingLocked`] instead of emitting a pointer
    /// without a writable owner, which would produce a valid-looking
    /// pointer backed by no chunks — irrecoverably broken on push.
    pub fn set_staging_locked(&mut self, holder_pid: Option<u32>) {
        self.staging_unavailable = Some(holder_pid);
    }

    /// Mark the session as having no staging area at all (directory
    /// missing). Same effect on the clean path as
    /// [`set_staging_locked`](Self::set_staging_locked): refuse to
    /// emit crab-native pointers. Unit tests that exercise clean
    /// without a real staging area should not call this.
    pub fn set_staging_unavailable(&mut self) {
        self.staging_unavailable = Some(None);
    }

    /// Attach the lazily acquired writer before processing clean content.
    pub fn set_staging(&mut self, staging: Arc<StagingArea>, handle: tokio::runtime::Handle) {
        self.staging = Some(CleanStaging {
            staging,
            handle,
            #[cfg(test)]
            _temporary: None,
        });
        self.staging_unavailable = None;
    }

    /// Install the remote file-index checker used by the clean fast path.
    pub fn set_file_index_checker(&mut self, checker: Box<dyn FileIndexChecker>) {
        self.file_index_checker = checker;
    }

    /// If the session knows staging isn't backing writes, return the
    /// appropriate `StagingLocked` error so callers can bail before
    /// running CDC. Returns `Ok(())` otherwise.
    fn check_staging_available(&self) -> Result<()> {
        match self.staging_unavailable {
            None => Ok(()),
            Some(holder_pid) => Err(CrabError::StagingLocked { holder_pid }),
        }
    }

    /// Seed the bloom filter with known file hashes from the local file-index.
    pub fn seed_bloom(&mut self, file_hashes: &[[u8; 32]]) {
        for hash in file_hashes {
            self.bloom.insert(hash);
        }
    }

    /// Load advisory bloom state through the private cache owner.
    pub fn load_bloom_from_cache(&mut self) {
        let cache = crate::cache::LocalCache::with_limits(
            crate::cache::default_cache_root(),
            self.ctx.config().cache.max_bytes,
            None,
        );
        match cache.read_clean_bloom() {
            Ok(Some(data)) => {
                if let Some(bloom) = FileHashBloom::decode(&data) {
                    self.bloom = bloom;
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::debug!(%error, "clean bloom unavailable; using session bloom");
            }
        }
    }

    /// Persist advisory bloom state within the configured private-cache budget.
    pub fn save_bloom_to_cache(&self) {
        let cache = crate::cache::LocalCache::with_limits(
            crate::cache::default_cache_root(),
            self.ctx.config().cache.max_bytes,
            None,
        );
        let data = self.bloom.encode();
        if let Err(error) = cache.put_clean_bloom(&data) {
            tracing::debug!(%error, "failed to persist bloom filter (non-fatal)");
        }
    }

    /// Load persisted hints for one resolved global-content storage scope.
    ///
    /// Any unavailable or corrupt index degrades to an empty cache. Emitted
    /// pointers then omit `shard-hint` and hydrate uses the file-index path.
    pub fn load_shard_hints_from_cache(
        &mut self,
        scope: &crate::cache::shard_hints::ShardHintScope,
    ) {
        let root = crate::cache::default_cache_root();
        let path = crate::cache::shard_hints::database_path(&root);
        match ShardHintCache::load_sync(&root, scope) {
            Ok(cache) => {
                tracing::debug!(
                    path = %path.display(),
                    entries = cache.len(),
                    "loaded shard-hints cache for clean session"
                );
                self.shard_hints = cache;
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to load shard-hints cache, proceeding without hints"
                );
            }
        }
    }

    /// Compile the persistent hydrate include/exclude patterns into a
    /// [`PatternFilter`] when `hydrate.auto` is enabled. Returns `None`
    /// when auto-hydrate is off or when pattern compilation fails (with a
    /// warning logged so the session can fall back gracefully).
    fn compile_hydrate_filter(ctx: &AppContext) -> Option<PatternFilter> {
        let hydrate = &ctx.config().hydrate;
        if !hydrate.auto {
            return None;
        }
        if hydrate.include.is_empty() {
            tracing::debug!("hydrate.auto enabled but no include patterns configured");
            return None;
        }
        match crate::core::pattern::build_filter(&hydrate.include, &hydrate.exclude) {
            Ok(filter) => {
                tracing::debug!(
                    include_count = hydrate.include.len(),
                    exclude_count = hydrate.exclude.len(),
                    "compiled auto-hydrate pattern filter"
                );
                Some(filter)
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "failed to compile auto-hydrate patterns, falling back to lazy for all files"
                );
                None
            }
        }
    }

    /// Check whether a file should be eagerly smudged despite lazy mode.
    ///
    /// Returns `true` when `hydrate.auto` is enabled and the file path
    /// matches the persistent include/exclude patterns.
    pub fn should_auto_hydrate(&self, pathname: &str) -> bool {
        match self.hydrate_filter {
            Some(ref filter) => filter.matches(pathname),
            None => false,
        }
    }

    /// Set the repository root directory for `.gitattributes` lookup.
    ///
    /// Walks the working tree **once** to collect both LFS patterns
    /// (legacy `is_lfs_tracked` fallback) and the
    /// [`FilterAttrCache`](crate::git::filter_attr_cache::FilterAttrCache)
    /// entries, avoiding the redundant second tree walk that the
    /// previous two-call approach incurred.
    pub fn set_repo_root(&mut self, root: PathBuf) {
        let (entries, root_mtime) = crate::git::filter_attr_cache::collect_all_entries(&root);
        let lfs_config = crate::lfs::config::LfsConfig::resolve(&root);
        self.lfs_skip_download_errors = lfs_config
            .as_ref()
            .is_ok_and(|config| config.skip_download_errors);
        self.lfs_storage_dir = crate::git::worktree::WorktreeContext::resolve_from_path(&root)
            .ok()
            .map(|worktree| match lfs_config.as_ref() {
                Ok(config) => config.storage_dir(&worktree.common_git_dir),
                Err(error) => {
                    tracing::debug!(
                        root = %root.display(),
                        error = %error,
                        "invalid LFS config; using the default local cache path"
                    );
                    crate::lfs::config::LfsConfig::default().storage_dir(&worktree.common_git_dir)
                }
            });
        self.lfs_fetch_filter = match lfs_config
            .and_then(|config| crate::lfs::fetch_filter::FetchPathFilter::from_config(&config))
        {
            Ok(filter) => filter,
            Err(e) => {
                tracing::warn!(
                    root = %root.display(),
                    error = %e,
                    "failed to compile LFS fetch filters; process smudge will not apply path filters"
                );
                None
            }
        };

        // Extract LFS-tracked patterns for the legacy `is_lfs_tracked`
        // fallback (used when `gix-pathmatch` is disabled or the
        // AttrsReader fails to open). Produces the same `Vec<String>`
        // that `parse_lfs_patterns` would have returned.
        self.lfs_patterns = entries
            .iter()
            .filter(|e| e.filter == Some(crate::git::filter_attr_cache::FilterKind::Lfs))
            .map(|e| e.pattern.clone())
            .collect();

        self.filter_attr_cache =
            Some(crate::git::filter_attr_cache::FilterAttrCache::from_entries(entries, root_mtime));

        // AttrsReader is lazily initialized by `is_lfs_tracked` on first
        // call — we don't pay the tree-walk + gix_attributes parse cost
        // unless the legacy LFS classifier path is actually exercised.
        // The filter-process dispatch uses `resolve_filter_for` →
        // `FilterAttrCache` instead, so the lazy init keeps `crab init`
        // fast on large working trees.

        self.repo_root = Some(root);
    }

    /// Return the repository root bound to this filter session, when known.
    #[must_use]
    pub fn repo_root(&self) -> Option<&Path> {
        self.repo_root.as_deref()
    }

    fn preserve_committed_recipe_before_publication(
        &self,
        pathname: &str,
        new_file_hash: &[u8; 32],
    ) -> Result<()> {
        let Some(root) = self.repo_root.as_ref() else {
            return Ok(());
        };
        let Some(staging) = &self.staging else {
            return Ok(());
        };
        let path = Path::new(pathname).to_path_buf();
        let pointers =
            crate::git::worktree::committed_pointers_for_paths(root, std::slice::from_ref(&path))?;
        if let Some(pointers) = pointers.get(&path) {
            for pointer in pointers {
                if pointer.file_hash != *new_file_hash {
                    staging.staging.preserve_published_history_recipe(
                        &MerkleHash::from(pointer.file_hash),
                        pointer.size,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Return the resolved local LFS storage directory for this session.
    #[must_use]
    pub fn lfs_storage_dir(&self) -> Option<&Path> {
        self.lfs_storage_dir.as_deref()
    }

    /// Check whether an LFS-tracked path should be smudged under LFS fetch filters.
    #[must_use]
    pub fn should_lfs_smudge(&self, pathname: &str) -> bool {
        self.lfs_fetch_filter
            .as_ref()
            .is_none_or(|filter| filter.allows(pathname))
    }

    /// Check whether an explicit Git LFS setting permits pointer fallback.
    #[must_use]
    pub fn should_skip_lfs_download_errors(&self) -> bool {
        self.lfs_skip_download_errors
    }

    /// Check whether a file is LFS-tracked via `filter=lfs` in `.gitattributes`.
    ///
    /// Resolution order (first match wins):
    /// 1. [`crate::core::attrs::AttrsReader`] (when `gix-pathmatch` is enabled)
    /// 2. [`FilterAttrCache`](crate::git::filter_attr_cache::FilterAttrCache)
    ///    (same entries, O(n) scan with "last match wins" semantics)
    /// 3. Legacy `lfs_patterns` slice (hand-rolled glob match)
    ///
    /// Returns `false` when the repo root is unknown or `.gitattributes` is
    /// missing/unreadable.
    #[allow(dead_code)]
    fn is_lfs_tracked(&self, pathname: &str) -> bool {
        #[cfg(feature = "gix-pathmatch")]
        {
            // Lazily build the AttrsReader on first call. The underlying
            // tree walk + gix_attributes parse is expensive and never
            // needed by the filter-process clean dispatch (which uses
            // FilterAttrCache instead). Deferring keeps `crab init` fast
            // on large working trees.
            let reader_opt = self.lfs_attrs.get_or_init(|| {
                let root = self.repo_root.as_ref()?;
                match crate::core::attrs::AttrsReader::open(root) {
                    Ok(r) => {
                        tracing::debug!(
                            root = %root.display(),
                            "lazily initialized AttrsReader for is_lfs_tracked"
                        );
                        Some(Arc::new(r))
                    }
                    Err(err) => {
                        tracing::warn!(
                            root = %root.display(),
                            error = %err,
                            "failed to lazily open consolidated attributes reader; using legacy LFS patterns"
                        );
                        None
                    }
                }
            });
            if let Some(reader) = reader_opt.as_ref() {
                return reader.has_filter(pathname, "lfs");
            }
        }
        // Prefer the FilterAttrCache — it uses the same entries as
        // resolve_filter_for and has proper "last match wins" semantics.
        if let Some(ref cache) = self.filter_attr_cache {
            return cache.resolve_filter(pathname)
                == Some(crate::git::filter_attr_cache::FilterKind::Lfs);
        }
        self.lfs_patterns
            .iter()
            .any(|pattern| glob_matches(pattern, pathname))
    }

    /// Resolve the filter handler for a file path using the
    /// [`FilterAttrCache`](crate::git::filter_attr_cache::FilterAttrCache).
    ///
    /// Returns the filter kind from the last matching `.gitattributes` line.
    /// Returns `None` when no filter attribute matches or the cache hasn't
    /// been initialized (missing repo root).
    pub fn resolve_filter_for(
        &self,
        pathname: &str,
    ) -> Option<crate::git::filter_attr_cache::FilterKind> {
        let cache = self.filter_attr_cache.as_ref()?;
        let resolved = cache.resolve_filter(pathname);
        match resolved {
            Some(filter) => {
                tracing::debug!(
                    path = %pathname,
                    filter = ?filter,
                    "resolved filter for path"
                );
            }
            None => {
                tracing::trace!(path = %pathname, "no filter attribute matches, falling back to blob classification");
            }
        }
        resolved
    }

    /// Reset transient state after a failed operation.
    ///
    /// Called by the session-isolation wrapper to ensure a
    /// panic or error in one clean doesn't leave stale buffered data that
    /// could affect the next operation.
    pub fn reset_transient_state(&mut self) {
        // The chunk buffer is per-operation and created fresh each time,
        // so there's nothing to reset there. The bloom filter and
        // confirmed_hashes are session-scoped and remain valid.
        // This method exists as the hook point for any future transient
        // state that needs clearing.
        tracing::debug!("transient session state reset after operation failure");
    }

    /// Clean a file: hash, chunk, stage, and return the pointer bytes.
    ///
    /// Dispatches to the LFS path (SHA-256 + LFS pointer) when the file
    /// has `filter=lfs` in `.gitattributes` and an LFS object store is
    /// configured. Otherwise uses the crab path (Blake3 + CDC).
    ///
    /// Errors abort unpublished preparation; process termination leaves its
    /// journal for writable-reopen recovery before payload reclamation.
    ///
    /// This is a thin wrapper that re-frames `content` as pkt-line data
    /// packets and delegates to [`clean_stream`](Self::clean_stream) so
    /// the buffered and streaming entry points share a single pipeline
    /// implementation. Output pointer bytes are byte-identical to the
    /// pre-wrapper behavior because CDC boundaries and blake3 hashing
    /// are both independent of input-feed granularity.
    pub fn clean_file(&mut self, pathname: &str, content: Vec<u8>) -> Result<Vec<u8>> {
        let framed = frame_as_pktlines(&content);
        let mut reader = crate::git::filter_process::PktLineReader::from_slice(&framed);
        self.clean_stream(pathname, &mut reader)
    }

    /// LFS clean path for callers that already materialized the input.
    fn lfs_clean_path(&self, pathname: &str, content: Vec<u8>) -> Result<Vec<u8>> {
        let lfs_dir = self.lfs_dir()?;
        let mut writer = crate::lfs::cache::ObjectWriter::new(&lfs_dir)?;
        std::io::Write::write_all(&mut writer, &content).map_err(CrabError::Io)?;
        let extensions = crate::lfs::extension::configured_extensions_sorted()?;
        let cleaned = crate::lfs::extension::clean_staged_with_extensions(
            writer.finish()?,
            &lfs_dir,
            pathname,
            &extensions,
        )?;
        self.finish_lfs_clean(pathname, cleaned.staged, cleaned.pointer_extensions)
    }

    fn finish_lfs_clean(
        &self,
        pathname: &str,
        staged: crate::lfs::cache::StagedObject,
        extensions: Vec<crab_git::lfs_pointer::LfsExtension>,
    ) -> Result<Vec<u8>> {
        let oid = *staged.oid();
        let size = staged.size();
        let lfs_dir = self.lfs_dir()?;
        staged.install(&lfs_dir)?;
        let oid_hex = crab_git::lfs_pointer::hex_encode(&oid);
        tracing::debug!(
            oid = %oid_hex,
            path = %pathname,
            size,
            "lfs clean: object verified and installed locally"
        );

        Ok(LfsPointer {
            oid,
            size,
            extensions,
        }
        .serialize())
    }

    fn lfs_dir(&self) -> Result<PathBuf> {
        let root = self
            .repo_root
            .as_ref()
            .ok_or_else(|| CrabError::Configuration {
                key: "lfs cache".to_owned(),
                origin: "repository root is unavailable".to_owned(),
            })?;
        let ctx = crate::git::worktree::WorktreeContext::resolve_from_path(root)?;
        if let Some(path) = self.lfs_storage_dir() {
            return Ok(path.to_path_buf());
        }
        let config = crate::lfs::config::LfsConfig::resolve(&ctx.current_worktree_root)?;
        Ok(config.storage_dir(&ctx.common_git_dir))
    }
    fn crab_clean_path(&mut self, pathname: &str, content: Vec<u8>) -> Result<Vec<u8>> {
        let mut content = Bytes::from(content);
        self.crab_clean_content(pathname, || {
            if content.is_empty() {
                return Ok(None);
            }
            let length = content.len().min(crab_staging::ingest::READ_BUF_SIZE);
            Ok(Some(content.split_to(length)))
        })
    }

    /// Clean one packet-framed request using bounded shared ingestion.
    ///
    /// Pointer and LFS dispatch is resolved before Crab content enters staging.
    pub fn clean_stream<R: std::io::Read>(
        &mut self,
        pathname: &str,
        reader: &mut crate::git::filter_process::PktLineReader<R>,
    ) -> Result<Vec<u8>> {
        // Resolve the filter handler for this path from .gitattributes.
        // This runs BEFORE blob classification so explicit user rules
        // always take precedence over pointer format detection.
        let resolved_filter = self.resolve_filter_for(pathname);

        // Buffer only the bounded pointer probe. A pointer may span packets,
        // and a pointer-shaped first packet may be followed by real content;
        // classification before either condition is known can bypass cleaning
        // or grow an unbounded passthrough Vec.
        let Some(mut prefix) = reader.read_packet()?.map(<[u8]>::to_vec) else {
            // Empty stream: mirror `clean_file(vec![])`.
            match resolved_filter {
                Some(crate::git::filter_attr_cache::FilterKind::Lfs) => {
                    return self.lfs_clean_path(pathname, Vec::new());
                }
                _ => {
                    return self.crab_clean_path(pathname, Vec::new());
                }
            }
        };

        let mut reached_flush = false;
        while prefix.len() < MAX_LFS_POINTER_SIZE {
            let Some(packet) = reader.read_packet()? else {
                reached_flush = true;
                break;
            };
            prefix.extend_from_slice(packet);
        }

        // Pointer passthrough with filter-awareness: if the complete content
        // is a pointer and it matches the resolved filter, pass it through.
        // If the stream exceeded the pointer cutoff or the format disagrees
        // with the filter rule, re-process it with the selected pipeline.
        let ptr_kind = if reached_flush {
            classify(&prefix)
        } else {
            PointerKind::NotAPointer
        };
        let should_passthrough = match (resolved_filter, &ptr_kind) {
            (Some(crate::git::filter_attr_cache::FilterKind::Lfs), PointerKind::Lfs(_)) => {
                !lfs_extensions_configured_for_clean()
            }
            (Some(crate::git::filter_attr_cache::FilterKind::Crab), PointerKind::Crab(_))
            | (None, PointerKind::Lfs(_) | PointerKind::Crab(_)) => true,
            // Filter disagrees with pointer → warn and re-process
            (Some(filter), kind) => {
                tracing::warn!(
                    path = %pathname,
                    resolved = ?filter,
                    blob = ?kind,
                    "pointer format mismatch: .gitattributes filter disagrees with blob content; re-processing"
                );
                false
            }
            _ => false,
        };

        if should_passthrough {
            tracing::debug!(
                path = %pathname,
                size = prefix.len(),
                "clean_stream: content is already a matching pointer, passing through"
            );
            return Ok(prefix);
        }

        // Explicit attributes are the routing contract. Unmatched paths
        // remain on Crab/XET because the filter does not yet have complete
        // file size and version-history evidence for an automatic decision.
        match resolved_filter {
            Some(crate::git::filter_attr_cache::FilterKind::Lfs) => {
                let extensions = crate::lfs::extension::configured_extensions_sorted()?;
                let lfs_dir = self.lfs_dir()?;
                let mut writer = crate::lfs::cache::ObjectWriter::new(&lfs_dir)?;
                std::io::Write::write_all(&mut writer, &prefix).map_err(CrabError::Io)?;
                if !reached_flush {
                    while let Some(pkt) = reader.read_packet()? {
                        std::io::Write::write_all(&mut writer, pkt).map_err(CrabError::Io)?;
                    }
                }
                let cleaned = crate::lfs::extension::clean_staged_with_extensions(
                    writer.finish()?,
                    &lfs_dir,
                    pathname,
                    &extensions,
                )?;
                self.finish_lfs_clean(pathname, cleaned.staged, cleaned.pointer_extensions)
            }
            _ => {
                // Crab/XET path: stream into the hasher and chunker.
                // This is the default when no filter matches (backward compat).
                self.crab_clean_stream(pathname, prefix, reader, reached_flush)
            }
        }
    }

    // Decode packets into bounded blocks. Only the request flush ends content;
    // packet boundaries never finalize the shared content-defined chunker.
    fn crab_clean_stream<R: std::io::Read>(
        &mut self,
        pathname: &str,
        first: Vec<u8>,
        reader: &mut crate::git::filter_process::PktLineReader<R>,
        reached_flush: bool,
    ) -> Result<Vec<u8>> {
        let mut prefix = Some(Bytes::from(first));
        let mut ended = reached_flush;
        self.crab_clean_content(pathname, || {
            if let Some(prefix) = prefix.take() {
                return Ok(Some(prefix));
            }
            if ended {
                return Ok(None);
            }
            let mut bytes = BytesMut::with_capacity(crab_staging::ingest::READ_BUF_SIZE);
            while bytes.len() < crab_staging::ingest::READ_BUF_SIZE {
                match reader.read_packet()? {
                    Some(packet) => bytes.extend_from_slice(packet),
                    None => {
                        ended = true;
                        break;
                    }
                }
            }
            Ok((!bytes.is_empty()).then(|| bytes.freeze()))
        })
    }

    fn crab_clean_content(
        &mut self,
        pathname: &str,
        mut read: impl FnMut() -> Result<Option<Bytes>>,
    ) -> Result<Vec<u8>> {
        if self.staging_unavailable.is_some() || self.staging.is_none() {
            let mut hasher = blake3::Hasher::new();
            let mut size = 0u64;
            while let Some(bytes) = read()? {
                hasher.update(&bytes);
                size = size.checked_add(bytes.len() as u64).ok_or_else(|| {
                    CrabError::StagingCorrupt("clean input size overflow".to_owned())
                })?;
            }
            let hash = *hasher.finalize().as_bytes();
            if let Some(pointer) = self.try_index_pointer_match(pathname, &hash) {
                return Ok(pointer);
            }
            if let Some(pointer) = self.try_fast_path(size, &hash)? {
                return Ok(pointer);
            }
            self.check_staging_available()?;
            return Err(CrabError::StagingLocked { holder_pid: None });
        }
        let backend = self
            .staging
            .as_ref()
            .ok_or_else(|| CrabError::StagingLocked { holder_pid: None })?;
        let staging = Arc::clone(&backend.staging);
        let handle = backend.handle.clone();
        let prepared = staging.local_recipe_for_path(Path::new(pathname))?;
        let progress = crab_staging::ingest::StreamStageProgress {
            xorb_builder: Some(self.xorb_builder.clone()),
            ..Default::default()
        };
        let cancel = self.ctx.cancel_token();
        let mut session = handle.block_on(crab_staging::ingest::IngestSession::begin(
            &staging,
            Path::new(pathname),
            progress,
            &cancel,
            prepared,
        ))?;
        let input = (|| -> Result<()> {
            while let Some(bytes) = read()? {
                handle.block_on(session.feed(bytes))?;
            }
            Ok(())
        })();
        if let Err(error) = input {
            handle.block_on(session.abort())?;
            return Err(error);
        }
        let completed = handle.block_on(session.finish_input())?;
        let hash = completed.file_hash();
        let size = completed.size();
        let fast_path = self
            .try_index_pointer_match(pathname, &hash)
            .map(|pointer| Ok(Some(pointer)))
            .unwrap_or_else(|| self.try_fast_path(size, &hash));
        match fast_path {
            Ok(Some(pointer)) => {
                handle.block_on(completed.abort())?;
                return Ok(pointer);
            }
            Err(error) => {
                handle.block_on(completed.abort())?;
                return Err(error);
            }
            Ok(None) => {}
        }
        if let Err(error) = self.preserve_committed_recipe_before_publication(pathname, &hash) {
            handle.block_on(completed.abort())?;
            return Err(error);
        }
        let staged = handle.block_on(completed.seal())?;
        // Git provides no later index-commit acknowledgment. Retain before
        // pointer emission, including when the output pipe subsequently fails.
        staging.mark_batch_published(&staged.batch_id)?;
        tracing::debug!(
            path = pathname,
            size,
            chunks = staged.chunks,
            chunking_ms = staged.timings.chunking_duration_ms,
            compression_ms = staged.timings.compression_duration_ms,
            payload_write_ms = staged.timings.payload_write_duration_ms,
            "clean content durably staged"
        );
        Ok(self.build_pointer(hash, size, None).serialize())
    }

    /// Construct a [`Pointer`], attaching a `shard-hint` when one is
    /// available. An explicit `shard_hint` (e.g. from a file-index HEAD
    /// response on the fast path) takes precedence; otherwise the local
    /// shard-hints cache is consulted. Missing entries are the common
    /// case on first push and simply produce a hint-less pointer.
    fn build_pointer(
        &self,
        file_hash: [u8; 32],
        file_size: u64,
        shard_hint: Option<[u8; 32]>,
    ) -> Pointer {
        let pointer = self.shard_hints.pointer_for(file_hash, file_size);
        match shard_hint {
            Some(hint) => pointer.with_shard_hint(hint),
            None => pointer,
        }
    }

    /// Attempt to resolve `pathname` to an existing pointer blob in
    /// the git index whose file-hash matches `computed_hash`. A match
    /// proves the working-tree content is a hydrated version of that
    /// pointer and lets the clean filter emit the index's pointer
    /// verbatim — no staging, no network.
    ///
    /// Returns `None` when:
    ///   - the repo root is unknown,
    ///   - the git index is missing or unreadable,
    ///   - the path has no entry,
    ///   - the blob isn't a parseable crab pointer, or
    ///   - the pointer's file_hash doesn't match.
    ///
    /// All failures degrade to the next fallback — the index lookup is
    /// strictly an optimization for the hydrated-file flow.
    fn try_index_pointer_match(&self, pathname: &str, computed_hash: &[u8; 32]) -> Option<Vec<u8>> {
        let root = self.repo_root.as_ref()?;
        let ctx = crate::git::worktree::WorktreeContext::resolve_from_path(root).ok()?;
        let index_path = ctx.index_path();
        if !index_path.is_file() {
            return None;
        }

        let index = gix_index::File::at(
            index_path,
            gix_hash::Kind::Sha1,
            true,
            gix_index::decode::Options::default(),
        )
        .ok()?;

        // Find the entry matching this pathname. The index stores
        // paths as raw bytes; comparing against the pkt-line pathname
        // (UTF-8 forward-slashed) matches git's normal lookup.
        let entry = index
            .entries()
            .iter()
            .find(|e| e.path(&index) == pathname.as_bytes())?;

        let blob_id = entry.id;

        // Read the blob from the object DB. Pointers are tiny (<1 KiB)
        // so this is cheap.
        let odb = gix_odb::at(ctx.objects_dir()).ok()?;
        let mut buf = Vec::new();
        let data = odb.try_find(&blob_id, &mut buf).ok()??;
        if data.kind != gix_object::Kind::Blob {
            return None;
        }
        let blob_bytes = data.data.to_vec();

        // Parse as a crab pointer. Non-pointer blobs mean the
        // working-tree content shouldn't be cleaned to a pointer
        // anyway — fall through to the CDC path.
        let pointer = Pointer::parse(&blob_bytes).ok()?;
        if &pointer.file_hash != computed_hash {
            // Content hash doesn't match the pointer — user has
            // actually modified the file. Fall through so the
            // filter produces the new pointer (requiring staging).
            return None;
        }

        tracing::debug!(
            path = %pathname,
            file_hash = %hex_encode(computed_hash),
            "clean: content matches git-index pointer, emitting pointer verbatim"
        );
        Some(blob_bytes)
    }

    /// Attempt the fast path: skip staging if the file is already known.
    ///
    /// Eligible when:
    /// - `file_size >= fastpath_min_size`
    /// - Bloom filter reports a possible hit
    /// - HEAD to file-index confirms existence (200)
    ///
    /// On bloom hit + HEAD miss, increments the false-positive counter.
    fn try_fast_path(&mut self, file_size: u64, file_hash: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        // When staging is unavailable, the normal gates (size
        // threshold + bloom filter) are too strict: the bloom never
        // got seeded on fresh clones (it's populated by pushes), so
        // any hydrated file would fall through to the slow path and
        // fail with `StagingLocked`. In that state, issue an
        // unconditional HEAD against the file-index — paying one
        // extra HEAP per clean is cheap compared to failing the
        // user's `git status` / `git diff`.
        let staging_is_unavailable = self.staging_unavailable.is_some();

        // Gate: file must be large enough, unless we have to take
        // the fast path anyway.
        if !staging_is_unavailable && file_size < self.fastpath_min_size {
            return Ok(None);
        }

        // Gate: bloom filter must report a possible hit, unless we
        // have to take the fast path anyway.
        if !staging_is_unavailable && !self.bloom.maybe_contains(file_hash) {
            return Ok(None);
        }

        // Already confirmed in this session? Skip the HEAD.
        if let Some(shard_hint) = self.confirmed_hashes.get(file_hash).copied() {
            self.ctx.metrics().inc_clean_fastpath_taken();
            let pointer = self.build_pointer(*file_hash, file_size, shard_hint);
            return Ok(Some(pointer.serialize()));
        }

        // HEAD the file-index.
        let head_result = self.file_index_checker.head_file_index(file_hash)?;

        if head_result.exists {
            self.confirmed_hashes
                .insert(*file_hash, head_result.shard_hint);
            self.ctx.metrics().inc_clean_fastpath_taken();

            // Remember this hash so subsequent clean sessions also
            // fast-path it (seeds the persisted bloom filter).
            self.bloom.insert(file_hash);

            let pointer = self.build_pointer(*file_hash, file_size, head_result.shard_hint);

            tracing::debug!(
                file_hash = %hex_encode(file_hash),
                shard_hint = ?pointer.shard_hint.map(|h| hex_encode(&h)),
                staging_unavailable = staging_is_unavailable,
                "clean fast path: file already known"
            );

            return Ok(Some(pointer.serialize()));
        }

        // Bloom false positive: file not actually in the index.
        self.ctx.metrics().inc_clean_fastpath_false_positives();
        tracing::debug!(
            file_hash = %hex_encode(file_hash),
            "clean fast path: bloom false positive"
        );

        Ok(None)
    }
}

fn lfs_extensions_configured_for_clean() -> bool {
    crate::lfs::extension::configured_extensions()
        .map(|extensions| !extensions.is_empty())
        .unwrap_or(false)
}

/// Check whether a pathname matches an LFS track pattern in `.gitattributes`.
///
/// Reads `.gitattributes` from `root` and checks if any line with
/// `filter=lfs` has a glob pattern matching `pathname`. Returns `false`
/// when `.gitattributes` is missing or unreadable.
#[cfg(test)]
fn is_lfs_tracked_in(root: &std::path::Path, pathname: &str) -> bool {
    let ga_path = root.join(".gitattributes");
    let content = match std::fs::read_to_string(&ga_path) {
        Ok(s) => s,
        Err(_) => return false,
    };

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !trimmed.contains("filter=lfs") {
            continue;
        }
        // Extract the glob pattern (first whitespace-delimited token).
        if let Some(pattern) = trimmed.split_whitespace().next() {
            if glob_matches(pattern, pathname) {
                return true;
            }
        }
    }

    false
}

/// Simple glob matching for `.gitattributes` patterns.
///
/// Supports `*` (matches any sequence except `/`), `**` (matches any
/// sequence including `/`), and `?` (matches any single character except `/`).
/// Literal characters are compared case-sensitively.
#[allow(dead_code)]
fn glob_matches(pattern: &str, path: &str) -> bool {
    // Use the `glob` crate's pattern matching if available, otherwise
    // fall back to a simple implementation that handles the common cases.
    glob_match_impl(pattern.as_bytes(), path.as_bytes())
}

/// Recursive glob matcher operating on byte slices.
#[allow(dead_code)]
fn glob_match_impl(pattern: &[u8], text: &[u8]) -> bool {
    let mut pi = 0;
    let mut ti = 0;
    let mut star_pi = usize::MAX;
    let mut star_ti = 0;

    while ti < text.len() {
        if pi < pattern.len() && pattern[pi] == b'*' {
            // Check for `**` (matches path separators too).
            if pi + 1 < pattern.len() && pattern[pi + 1] == b'*' {
                // `**` — try matching the rest of the pattern at every
                // position in the remaining text.
                let rest = &pattern[pi + 2..];
                // Skip optional `/` after `**`.
                let rest = if rest.first() == Some(&b'/') {
                    &rest[1..]
                } else {
                    rest
                };
                // Try matching from every position.
                for i in ti..=text.len() {
                    if glob_match_impl(rest, &text[i..]) {
                        return true;
                    }
                }
                return false;
            }
            // Single `*` — matches anything except `/`.
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if pi < pattern.len()
            && (pattern[pi] == b'?' && text[ti] != b'/' || pattern[pi] == text[ti])
        {
            pi += 1;
            ti += 1;
        } else if star_pi != usize::MAX {
            // Backtrack to the last `*`.
            pi = star_pi + 1;
            star_ti += 1;
            // `*` must not match `/`.
            if text[star_ti - 1] == b'/' {
                return false;
            }
            ti = star_ti;
        } else {
            return false;
        }
    }

    // Consume trailing `*` patterns.
    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }

    pi == pattern.len()
}

/// Encode a 32-byte hash as lowercase hex.
fn hex_encode(bytes: &[u8; 32]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Wrap a raw byte slice as one or more pkt-line data packets followed by
/// a flush packet, suitable for feeding into
/// [`PktLineReader::from_slice`](crate::git::filter_process::PktLineReader::from_slice).
///
/// Chunks larger than the pkt-line body cap (65 516 B, per the git
/// protocol) are split across multiple packets. An empty input produces
/// just a flush packet, mirroring what git itself sends for empty files.
fn frame_as_pktlines(content: &[u8]) -> Vec<u8> {
    // The git protocol caps a pkt-line at 65 520 bytes total including the
    // 4-byte length header, so the body fits in 65 516 bytes.
    const PKT_LINE_MAX_BODY: usize = 65516;

    // Pre-size: 4-byte header per packet + 4-byte trailing flush.
    let header_overhead = content.len().div_ceil(PKT_LINE_MAX_BODY.max(1)) * 4 + 4;
    let mut out = Vec::with_capacity(content.len() + header_overhead);

    for chunk in content.chunks(PKT_LINE_MAX_BODY) {
        let len = chunk.len() + 4;
        // `len` <= 65 520, which fits in four hex digits.
        out.extend_from_slice(format!("{len:04x}").as_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(b"0000");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::context::AppContext;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn git_repo_tempdir() -> Option<tempfile::TempDir> {
        let dir = tempfile::tempdir().ok()?;
        let status = std::process::Command::new("git")
            .args(["init", "-q", dir.path().to_str()?])
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        Some(dir)
    }

    // --- Bloom filter tests ---

    #[test]
    fn bloom_insert_and_query() {
        let mut bloom = FileHashBloom::new(100);
        let hash = [0xABu8; 32];
        assert!(!bloom.maybe_contains(&hash));
        bloom.insert(&hash);
        assert!(bloom.maybe_contains(&hash));
    }

    #[test]
    fn bloom_definite_miss() {
        let bloom = FileHashBloom::new(100);
        let hash = [0x42u8; 32];
        assert!(!bloom.maybe_contains(&hash));
    }

    #[test]
    fn bloom_roundtrip_retains_known_hashes() {
        let mut bloom = FileHashBloom::new(100);
        bloom.insert(&[0xAB; 32]);
        let decoded = FileHashBloom::decode(&bloom.encode()).unwrap();
        assert!(decoded.maybe_contains(&[0xAB; 32]));
    }

    #[test]
    fn bloom_decode_rejects_malformed_geometry() {
        for data in [vec![], vec![1], u64::MAX.to_le_bytes().to_vec(), vec![1; 9]] {
            assert!(FileHashBloom::decode(&data).is_none());
        }
    }

    #[test]
    fn bloom_decode_rejects_zero_bit_cache() {
        assert!(
            FileHashBloom::decode(&0u64.to_le_bytes()).is_none(),
            "corrupt bloom cache must degrade to an empty bloom, not panic on modulo by zero"
        );
    }

    // --- Mock file-index checker ---

    struct MockFileIndexChecker {
        known_hashes: HashSet<[u8; 32]>,
        shard_hint: Option<[u8; 32]>,
    }

    impl FileIndexChecker for MockFileIndexChecker {
        fn head_file_index(&self, file_hash: &[u8; 32]) -> Result<FileIndexHeadResult> {
            Ok(FileIndexHeadResult {
                exists: self.known_hashes.contains(file_hash),
                shard_hint: if self.known_hashes.contains(file_hash) {
                    self.shard_hint
                } else {
                    None
                },
            })
        }
    }

    struct CountingFileIndexChecker {
        known_hashes: HashSet<[u8; 32]>,
        shard_hint: Option<[u8; 32]>,
        head_calls: Arc<AtomicUsize>,
    }

    impl FileIndexChecker for CountingFileIndexChecker {
        fn head_file_index(&self, file_hash: &[u8; 32]) -> Result<FileIndexHeadResult> {
            self.head_calls.fetch_add(1, Ordering::Relaxed);
            let exists = self.known_hashes.contains(file_hash);
            Ok(FileIndexHeadResult {
                exists,
                shard_hint: exists.then_some(self.shard_hint).flatten(),
            })
        }
    }

    // --- StoreFileIndexChecker tests ---

    fn seed_file_index(
        rt: &tokio::runtime::Runtime,
        store: Arc<dyn object_store::ObjectStore>,
        repo_prefix: &str,
        file_hash: &MerkleHash,
        shard_hash: &MerkleHash,
    ) {
        rt.block_on(async {
            let storage = crab_storage::Store::new(Arc::clone(&store));
            let router = crab_storage::StoreLayout::new(storage.clone(), repo_prefix.to_owned());
            crab_metadata::layout_descriptor::ensure_canonical_layout(&storage, &router)
                .await
                .expect("initialize canonical layout");
            let (shard_index_hash, _, shard_write) = crab_metadata::manifests::append_shard_index(
                crab_metadata::segmented::SegmentIndex::default(),
                1,
                &[shard_hash.hex()],
            )
            .expect("build shard index");
            crab_metadata::manifest_store::upload_segmented_bulk(
                &storage,
                &router,
                &crab_metadata::manifests::BulkData {
                    shard_index: shard_write,
                    pack_index: crab_metadata::segmented::SegmentWrite::default(),
                },
            )
            .await
            .expect("upload shard index");
            let mut manifest =
                crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main");
            manifest.generation = 1;
            manifest.shard_index_hash = shard_index_hash.clone();
            manifest.seal_git_validation();
            crab_metadata::manifest_store::create_manifest(&storage, &router, &manifest)
                .await
                .expect("create manifest");

            let metadb = crate::metadata::MetaDb::new(
                store,
                repo_prefix.to_owned(),
                crate::metadata::MetaDbConfig::for_repo(repo_prefix),
            );
            let file_store = metadb.file_index().await.expect("file index store");
            let mut txn = metadb.new_transaction();
            file_store.save_committed_batch(
                &mut txn,
                &[(
                    *file_hash,
                    crab_metadata::value_codec::CommittedFileRecord {
                        recipe_hash: [0xC7; 32],
                        shard_hash: *shard_hash,
                        committed_generation: 1,
                        shard_index_hash: MerkleHash::from_hex(&shard_index_hash)
                            .expect("valid shard-index hash"),
                    },
                )],
            );
            metadb.commit(txn).await.expect("seed file-index");
            metadb.close_all().await.expect("close metadb");
        });
    }

    #[test]
    fn store_file_index_checker_requires_file_index_and_shard_body() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let repo_prefix = "clean-fast-path-existing";
        let inner: Arc<dyn object_store::ObjectStore> =
            Arc::new(object_store::memory::InMemory::new());
        let store = crate::storage::store::Store::new(Arc::clone(&inner));
        let router = StoreLayout::new(store.clone(), repo_prefix.to_owned());
        let file_hash = MerkleHash::from([31_u64, 0, 0, 0]);
        let shard_hash = MerkleHash::from([32_u64, 0, 0, 0]);

        seed_file_index(
            &rt,
            Arc::clone(&inner),
            repo_prefix,
            &file_hash,
            &shard_hash,
        );
        rt.block_on(async {
            store
                .put(
                    &router.shard_path(&shard_hash),
                    Bytes::from_static(b"shard body"),
                )
                .await
                .expect("seed shard body");
        });

        let checker = StoreFileIndexChecker::new(router, rt.handle().clone());
        let file_hash_raw: [u8; 32] = file_hash.into();
        let result = checker
            .head_file_index(&file_hash_raw)
            .expect("file-index check");

        assert!(result.exists);
        assert_eq!(result.shard_hint, Some(shard_hash.into()));
    }

    #[test]
    fn store_file_index_checker_rejects_stale_file_index_missing_shard_body() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let repo_prefix = "clean-fast-path-stale";
        let inner: Arc<dyn object_store::ObjectStore> =
            Arc::new(object_store::memory::InMemory::new());
        let store = crate::storage::store::Store::new(Arc::clone(&inner));
        let router = StoreLayout::new(store, repo_prefix.to_owned());
        let file_hash = MerkleHash::from([33_u64, 0, 0, 0]);
        let shard_hash = MerkleHash::from([34_u64, 0, 0, 0]);

        seed_file_index(
            &rt,
            Arc::clone(&inner),
            repo_prefix,
            &file_hash,
            &shard_hash,
        );

        let checker = StoreFileIndexChecker::new(router, rt.handle().clone());
        let file_hash_raw: [u8; 32] = file_hash.into();
        let result = checker
            .head_file_index(&file_hash_raw)
            .expect("file-index check");

        assert!(!result.exists);
        assert!(result.shard_hint.is_none());
    }

    // --- Clean pipeline tests ---

    #[test]
    fn clean_produces_valid_pointer() {
        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        let content = b"hello world";
        let pointer_bytes = session.clean_file("test.bin", content.to_vec()).unwrap();

        let pointer = Pointer::parse(&pointer_bytes).unwrap();
        let expected_hash: [u8; 32] = *blake3::hash(content).as_bytes();
        assert_eq!(pointer.file_hash, expected_hash);
        assert_eq!(pointer.size, content.len() as u64);
    }

    #[test]
    fn clean_empty_file() {
        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        let content = b"";
        let pointer_bytes = session.clean_file("empty.bin", content.to_vec()).unwrap();

        let pointer = Pointer::parse(&pointer_bytes).unwrap();
        assert_eq!(pointer.size, 0);
    }

    #[test]
    fn fast_path_taken_when_file_known() {
        let content = vec![0xAA; 128 * 1024 * 1024]; // 128 MiB
        let file_hash: [u8; 32] = *blake3::hash(&content).as_bytes();
        let shard_hint = [0xBB; 32];

        let mut known = HashSet::new();
        known.insert(file_hash);

        let checker = MockFileIndexChecker {
            known_hashes: known,
            shard_hint: Some(shard_hint),
        };

        let ctx = AppContext::default();
        let mut session = CleanSession::with_deps(
            ctx.clone(),
            Box::new(checker),
            None,
            &[file_hash],
            64 * 1024 * 1024,
        );

        let pointer_bytes = session.clean_file("big.bin", content.clone()).unwrap();
        let pointer = Pointer::parse(&pointer_bytes).unwrap();

        assert_eq!(pointer.file_hash, file_hash);
        assert_eq!(pointer.shard_hint, Some(shard_hint));
        assert_eq!(ctx.metrics().snapshot().clean_fastpath_taken, 1);
        assert_eq!(ctx.metrics().snapshot().clean_fastpath_false_positives, 0);
    }

    #[test]
    fn fast_path_bloom_false_positive_counted() {
        let content = vec![0xCC; 128 * 1024 * 1024]; // 128 MiB
        let file_hash: [u8; 32] = *blake3::hash(&content).as_bytes();

        // Bloom seeded with this hash, but checker says it doesn't exist.
        let checker = MockFileIndexChecker {
            known_hashes: HashSet::new(),
            shard_hint: None,
        };

        let ctx = AppContext::default();
        let mut session = CleanSession::with_deps(
            ctx.clone(),
            Box::new(checker),
            None,
            &[file_hash],
            64 * 1024 * 1024,
        );

        let _pointer_bytes = session.clean_file("big2.bin", content).unwrap();

        assert_eq!(ctx.metrics().snapshot().clean_fastpath_taken, 0);
        assert_eq!(ctx.metrics().snapshot().clean_fastpath_false_positives, 1);
    }

    #[test]
    fn fast_path_skipped_for_small_files() {
        let content = b"small file";
        let file_hash: [u8; 32] = *blake3::hash(content).as_bytes();

        let mut known = HashSet::new();
        known.insert(file_hash);

        let checker = MockFileIndexChecker {
            known_hashes: known,
            shard_hint: None,
        };

        let ctx = AppContext::default();
        let mut session = CleanSession::with_deps(
            ctx.clone(),
            Box::new(checker),
            None,
            &[file_hash],
            64 * 1024 * 1024,
        );

        let _pointer_bytes = session.clean_file("small.bin", content.to_vec()).unwrap();

        // Fast path not taken because file is too small.
        assert_eq!(ctx.metrics().snapshot().clean_fastpath_taken, 0);
    }

    #[test]
    fn slow_path_pointer_carries_cached_shard_hint() {
        // When the shard-hint cache has an entry for the computed file
        // hash, the emitted pointer (slow path — no HEAD confirmation)
        // should include the hint so hydrate can skip the file-index GET.
        let content = b"hello world";
        let file_hash: [u8; 32] = *blake3::hash(content).as_bytes();
        let shard_hint: [u8; 32] = [0xCD; 32];

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        session
            .shard_hints
            .insert(MerkleHash::from(file_hash), MerkleHash::from(shard_hint));

        let pointer_bytes = session.clean_file("test.bin", content.to_vec()).unwrap();
        let pointer = Pointer::parse(&pointer_bytes).unwrap();

        assert_eq!(pointer.file_hash, file_hash);
        assert_eq!(pointer.shard_hint, Some(shard_hint));
    }

    #[test]
    fn slow_path_pointer_has_no_hint_when_cache_miss() {
        // With an empty shard-hint cache, the emitted pointer should have
        // no hint — this is the common case on first push.
        let content = b"hello world";

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);

        let pointer_bytes = session.clean_file("test.bin", content.to_vec()).unwrap();
        let pointer = Pointer::parse(&pointer_bytes).unwrap();

        assert!(pointer.shard_hint.is_none());
    }

    #[test]
    fn crab_clean_refuses_when_staging_is_locked() {
        // Regression guard for the silent-pointer-without-chunks bug:
        // when staging is unavailable, the clean path must return
        // `StagingLocked` instead of producing a valid-looking pointer
        // backed by no staging owner. Emitting such a pointer advances
        // git's index past content that will never reach object storage,
        // which breaks hydration on every other clone of the repo.
        let content = b"large file content we would have chunked";

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        session.set_staging_locked(Some(12345));

        let result = session.clean_file("big.bin", content.to_vec());

        match result {
            Err(CrabError::StagingLocked { holder_pid }) => {
                assert_eq!(holder_pid, Some(12345));
            }
            other => panic!("expected StagingLocked, got {other:?}"),
        }
    }

    #[test]
    fn crab_clean_refuses_when_staging_is_unavailable() {
        // Same failure mode as the locked variant, but without a known
        // holder PID (e.g. staging directory simply doesn't exist).
        let content = b"another would-be chunked payload";

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        session.set_staging_unavailable();

        let result = session.clean_file("big.bin", content.to_vec());

        match result {
            Err(CrabError::StagingLocked { holder_pid }) => {
                assert!(holder_pid.is_none());
            }
            other => panic!("expected StagingLocked, got {other:?}"),
        }
    }

    #[test]
    fn session_isolation_after_reset() {
        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);

        // Simulate a failure + reset.
        session.reset_transient_state();

        // Next clean should still work.
        let content = b"after reset";
        let pointer_bytes = session.clean_file("ok.bin", content.to_vec()).unwrap();
        let pointer = Pointer::parse(&pointer_bytes).unwrap();
        assert_eq!(pointer.size, content.len() as u64);
    }

    #[cfg(unix)]
    #[test]
    fn clean_uses_supplied_content_when_path_cache_differs() {
        use crate::cache::{HydratedPointerCache, hydrated_pointer};

        let Some(dir) = git_repo_tempdir() else {
            eprintln!("SKIP: git unavailable or fixture setup failed");
            return;
        };
        let root = dir.path();
        let relative = "content.bin";
        let hydrated = b"hydrated bytes";
        let supplied = b"different bytes";
        let full_path = root.join(relative);
        std::fs::write(&full_path, hydrated).unwrap();
        let hydrated_pointer = Pointer {
            file_hash: *blake3::hash(hydrated).as_bytes(),
            size: hydrated.len() as u64,
            shard_hint: None,
        }
        .serialize();
        let entry = hydrated_pointer::entry_for_path(&full_path, &hydrated_pointer).unwrap();
        let cache_path = hydrated_pointer::cache_path_for_worktree_root(root).unwrap();
        HydratedPointerCache::update_on_disk(&cache_path, [(relative.to_owned(), entry)]).unwrap();

        let mut session = CleanSession::new(AppContext::default());
        session.set_repo_root(root.to_path_buf());
        let cleaned = session.clean_file(relative, supplied.to_vec()).unwrap();
        let pointer = Pointer::parse(&cleaned).unwrap();

        assert_eq!(pointer.file_hash, *blake3::hash(supplied).as_bytes());
    }

    #[test]
    fn set_repo_root_uses_linked_worktree_attributes() {
        use crate::git::filter_attr_cache::FilterKind;
        use std::process::Command;

        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let linked = tmp.path().join("linked");
        if !Command::new("git")
            .args(["init", "-q", repo.to_str().unwrap()])
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("SKIP: git unavailable or fixture setup failed");
            return;
        }
        let _ = Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&repo)
            .status();
        let _ = Command::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(&repo)
            .status();
        std::fs::write(repo.join("README.md"), "initial\n").unwrap();
        let _ = Command::new("git")
            .args(["add", "README.md"])
            .current_dir(&repo)
            .status();
        if !Command::new("git")
            .args(["commit", "-q", "-m", "initial"])
            .current_dir(&repo)
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("SKIP: git commit unavailable or fixture setup failed");
            return;
        }
        if !Command::new("git")
            .args([
                "worktree",
                "add",
                "-q",
                "--detach",
                linked.to_str().unwrap(),
                "HEAD",
            ])
            .current_dir(&repo)
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("SKIP: git worktree fixture setup failed");
            return;
        }

        std::fs::write(
            repo.join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();
        std::fs::write(
            linked.join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        session.set_repo_root(linked);
        assert_eq!(
            session.resolve_filter_for("model.bin"),
            Some(FilterKind::Lfs)
        );
    }

    #[test]
    fn linked_worktree_index_pointer_match_uses_private_index_and_common_objects() {
        use std::process::Command;

        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let linked = tmp.path().join("linked");
        if !Command::new("git")
            .args(["init", "-q", repo.to_str().unwrap()])
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("SKIP: git unavailable or fixture setup failed");
            return;
        }
        let _ = Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&repo)
            .status();
        let _ = Command::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(&repo)
            .status();

        let hydrated = b"linked hydrated content for index fast path".to_vec();
        let pointer = Pointer {
            file_hash: *blake3::hash(&hydrated).as_bytes(),
            size: hydrated.len() as u64,
            shard_hint: None,
        };
        let pointer_bytes = pointer.serialize();
        let rel = "big.bin";
        std::fs::write(repo.join(rel), &pointer_bytes).unwrap();
        let _ = Command::new("git")
            .args(["add", rel])
            .current_dir(&repo)
            .status();
        if !Command::new("git")
            .args(["commit", "-q", "-m", "add pointer"])
            .current_dir(&repo)
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("SKIP: git commit unavailable or fixture setup failed");
            return;
        }
        if !Command::new("git")
            .args([
                "worktree",
                "add",
                "-q",
                "--detach",
                linked.to_str().unwrap(),
                "HEAD",
            ])
            .current_dir(&repo)
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("SKIP: git worktree fixture setup failed");
            return;
        }

        std::fs::write(linked.join(rel), &hydrated).unwrap();

        let mut session = CleanSession::new(AppContext::default());
        session.set_repo_root(linked);
        session.set_staging_locked(Some(12345));

        let cleaned = session
            .clean_file(rel, hydrated)
            .expect("clean should use linked worktree index fast path");
        assert_eq!(cleaned, pointer_bytes);
    }

    #[test]
    fn lfs_clean_in_linked_worktree_caches_in_common_git_dir() {
        use std::process::Command;

        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let linked = tmp.path().join("linked");
        if !Command::new("git")
            .args(["init", "-q", repo.to_str().unwrap()])
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("SKIP: git unavailable or fixture setup failed");
            return;
        }
        let _ = Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&repo)
            .status();
        let _ = Command::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(&repo)
            .status();
        std::fs::write(repo.join("README.md"), "initial\n").unwrap();
        let _ = Command::new("git")
            .args(["add", "README.md"])
            .current_dir(&repo)
            .status();
        if !Command::new("git")
            .args(["commit", "-q", "-m", "initial"])
            .current_dir(&repo)
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("SKIP: git commit unavailable or fixture setup failed");
            return;
        }
        if !Command::new("git")
            .args([
                "worktree",
                "add",
                "-q",
                "--detach",
                linked.to_str().unwrap(),
                "HEAD",
            ])
            .current_dir(&repo)
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("SKIP: git worktree fixture setup failed");
            return;
        }

        std::fs::write(
            linked.join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();
        let content = b"linked lfs content";
        let mut session = CleanSession::new(AppContext::default());
        session.set_repo_root(linked.clone());

        let pointer_bytes = session.clean_file("model.bin", content.to_vec()).unwrap();
        let pointer = crab_git::lfs_pointer::LfsPointer::parse(&pointer_bytes).unwrap();
        let oid_hex = crab_git::lfs_pointer::hex_encode(&pointer.oid);
        let ctx = crate::git::worktree::WorktreeContext::resolve_from_path(&linked).unwrap();
        let cached_path = ctx
            .lfs_objects_dir()
            .join(&oid_hex[..2])
            .join(&oid_hex[2..4])
            .join(&oid_hex);

        assert_eq!(std::fs::read(cached_path).unwrap(), content);
    }

    #[test]
    fn git_index_pointer_match_short_circuits_clean_without_staging() {
        // Regression guard for the hydrated-file UX: when the working
        // tree has a hydrated file and git's index has its committed
        // pointer blob, the clean filter must reconstruct the pointer
        // from the index (not the staging area). Exercises the
        // `try_index_pointer_match` path: no bloom hit, no hydrated
        // cache, no staging — just the git index + blake3 hash check.
        use std::process::Command;

        // Create a real git repo so gix_index + gix_odb work against it.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status();
        let Ok(s) = status else {
            // Git not available in test env — skip rather than fail.
            return;
        };
        if !s.success() {
            return;
        }
        let _ = Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(root)
            .status();
        let _ = Command::new("git")
            .args(["config", "user.name", "test"])
            .current_dir(root)
            .status();

        // Commit a crab pointer blob at a known path. The pointer's
        // file_hash must match the blake3 of the hydrated content we
        // later "clean" from.
        let hydrated = b"the hydrated content that matches the pointer".to_vec();
        let file_hash: [u8; 32] = *blake3::hash(&hydrated).as_bytes();
        let pointer = Pointer {
            file_hash,
            size: hydrated.len() as u64,
            shard_hint: None,
        };
        let pointer_bytes = pointer.serialize();

        let rel = "big.bin";
        std::fs::write(root.join(rel), &pointer_bytes).unwrap();
        let _ = Command::new("git")
            .args(["add", rel])
            .current_dir(root)
            .status();
        let _ = Command::new("git")
            .args(["commit", "-q", "-m", "add pointer"])
            .current_dir(root)
            .status();

        // Now replace the working-tree file with hydrated content
        // (simulating `crab hydrate`).
        std::fs::write(root.join(rel), &hydrated).unwrap();

        // Clean with staging locked — must still succeed via the
        // index-pointer-match path.
        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        session.set_repo_root(root.to_path_buf());
        session.set_staging_locked(Some(12345));

        let cleaned = session
            .clean_file(rel, hydrated)
            .expect("clean must succeed via git-index pointer match");

        // The cleaned bytes must parse as a pointer with the same
        // file_hash, proving we reproduced the committed pointer.
        let reparsed = Pointer::parse(&cleaned).expect("cleaned output is a valid pointer");
        assert_eq!(reparsed.file_hash, file_hash);
    }

    #[test]
    fn fast_path_second_call_uses_confirmed_cache() {
        let content = b"duplicate content already present remotely".to_vec();
        let file_hash: [u8; 32] = *blake3::hash(&content).as_bytes();
        let shard_hint = [0xEE; 32];

        let mut known = HashSet::new();
        known.insert(file_hash);

        let head_calls = Arc::new(AtomicUsize::new(0));
        let checker = CountingFileIndexChecker {
            known_hashes: known,
            shard_hint: Some(shard_hint),
            head_calls: Arc::clone(&head_calls),
        };

        let ctx = AppContext::default();
        let mut session =
            CleanSession::with_deps(ctx.clone(), Box::new(checker), None, &[file_hash], 0);

        session.clean_file("big.bin", content.clone()).unwrap();
        let pointer_bytes = session.clean_file("copy.bin", content).unwrap();
        let pointer = Pointer::parse(&pointer_bytes).unwrap();

        assert_eq!(pointer.file_hash, file_hash);
        assert_eq!(pointer.shard_hint, Some(shard_hint));
        assert_eq!(head_calls.load(Ordering::Relaxed), 1);
        assert_eq!(ctx.metrics().snapshot().clean_fastpath_taken, 2);
    }

    #[test]
    fn should_auto_hydrate_returns_false_when_auto_disabled() {
        let ctx = AppContext::default();
        let session = CleanSession::new(ctx);
        assert!(!session.should_auto_hydrate("models/big.safetensors"));
    }

    #[test]
    fn should_auto_hydrate_returns_true_when_path_matches_patterns() {
        use crate::core::config::{Config, HydrateConfig};
        use tokio_util::sync::CancellationToken;

        let config = Config {
            hydrate: HydrateConfig {
                include: vec!["*.safetensors".to_owned()],
                exclude: Vec::new(),
                auto: true,
                ..HydrateConfig::default()
            },
            ..Config::default()
        };
        let ctx = AppContext::new(config, CancellationToken::new());
        let session = CleanSession::new(ctx);
        assert!(session.should_auto_hydrate("models/big.safetensors"));
        assert!(!session.should_auto_hydrate("models/big.bin"));
    }

    // --- Glob matching tests ---

    #[test]
    fn glob_matches_extension_wildcard() {
        assert!(glob_matches("*.bin", "model.bin"));
        assert!(glob_matches("*.bin", "a.bin"));
        assert!(!glob_matches("*.bin", "model.txt"));
        // Single `*` does not cross directory boundaries.
        assert!(!glob_matches("*.bin", "dir/model.bin"));
    }

    #[test]
    fn glob_matches_double_star() {
        assert!(glob_matches("**/*.bin", "model.bin"));
        assert!(glob_matches("**/*.bin", "dir/model.bin"));
        assert!(glob_matches("**/*.bin", "a/b/c/model.bin"));
        assert!(!glob_matches("**/*.bin", "model.txt"));
    }

    #[test]
    fn glob_matches_exact_filename() {
        assert!(glob_matches("model.bin", "model.bin"));
        assert!(!glob_matches("model.bin", "other.bin"));
    }

    #[test]
    fn glob_matches_question_mark() {
        assert!(glob_matches("?.bin", "a.bin"));
        assert!(!glob_matches("?.bin", "ab.bin"));
        // `?` does not match `/`.
        assert!(!glob_matches("?.bin", "/.bin"));
    }

    // --- .gitattributes LFS tracking tests ---

    #[test]
    fn is_lfs_tracked_matches_extension_pattern() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();

        assert!(is_lfs_tracked_in(dir.path(), "model.bin"));
        assert!(!is_lfs_tracked_in(dir.path(), "readme.txt"));
    }

    #[test]
    fn is_lfs_tracked_ignores_crab_filter() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();

        assert!(!is_lfs_tracked_in(dir.path(), "model.bin"));
    }

    #[test]
    fn is_lfs_tracked_handles_mixed_filters() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n\
             *.safetensors filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();

        assert!(is_lfs_tracked_in(dir.path(), "model.bin"));
        assert!(!is_lfs_tracked_in(dir.path(), "weights.safetensors"));
    }

    #[test]
    fn is_lfs_tracked_returns_false_when_no_gitattributes() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_lfs_tracked_in(dir.path(), "model.bin"));
    }

    #[test]
    fn is_lfs_tracked_ignores_comments() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "# *.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();

        assert!(!is_lfs_tracked_in(dir.path(), "model.bin"));
    }

    #[cfg(feature = "gix-pathmatch")]
    #[test]
    fn set_repo_root_defers_attrs_reader_until_legacy_lfs_check() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        session.set_repo_root(dir.path().to_path_buf());

        assert!(
            session.lfs_attrs.get().is_none(),
            "set_repo_root must not initialize AttrsReader"
        );
        assert_eq!(
            session.resolve_filter_for("model.bin"),
            Some(crate::git::filter_attr_cache::FilterKind::Lfs)
        );
        assert!(
            session.lfs_attrs.get().is_none(),
            "filter-process cache lookup must not initialize AttrsReader"
        );

        assert!(session.is_lfs_tracked("model.bin"));
        assert!(
            session.lfs_attrs.get().is_some(),
            "legacy LFS classifier is the first AttrsReader user"
        );
    }

    // --- LFS clean path tests ---

    #[test]
    fn lfs_clean_produces_valid_lfs_pointer() {
        let Some(dir) = git_repo_tempdir() else {
            eprintln!("SKIP: git unavailable or fixture setup failed");
            return;
        };
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        session.set_repo_root(dir.path().to_path_buf());
        // No remote LFS store — content is cached locally in .git/lfs/objects/
        // and an LFS pointer is still produced.
        let content = b"hello lfs world";
        let pointer_bytes = session.clean_file("model.bin", content.to_vec()).unwrap();
        // Should produce a valid LFS pointer even without a remote store.
        let lfs_pointer = crab_git::lfs_pointer::LfsPointer::parse(&pointer_bytes).unwrap();
        assert_eq!(lfs_pointer.size, content.len() as u64);

        // Verify the content was cached locally.
        let oid_hex = crab_git::lfs_pointer::hex_encode(&lfs_pointer.oid);
        let worktree_ctx =
            crate::git::worktree::WorktreeContext::resolve_from_path(dir.path()).unwrap();
        let cached_path = worktree_ctx
            .lfs_objects_dir()
            .join(&oid_hex[..2])
            .join(&oid_hex[2..4])
            .join(&oid_hex);
        assert!(
            cached_path.is_file(),
            "LFS content should be cached locally"
        );
        assert_eq!(std::fs::read(&cached_path).unwrap(), content);
    }

    #[test]
    fn lfs_clean_without_lfs_pattern_uses_crab_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.safetensors filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        session.set_repo_root(dir.path().to_path_buf());

        let content = b"crab content";
        let pointer_bytes = session
            .clean_file("weights.safetensors", content.to_vec())
            .unwrap();
        // Should produce a crab pointer (not LFS).
        let pointer = Pointer::parse(&pointer_bytes).unwrap();
        let expected_hash: [u8; 32] = *blake3::hash(content).as_bytes();
        assert_eq!(pointer.file_hash, expected_hash);
    }

    #[test]
    fn unmatched_path_uses_crab_path() {
        let dir = tempfile::tempdir().unwrap();

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        session.set_repo_root(dir.path().to_path_buf());

        let content = b"small unmatched candidate";
        let pointer_bytes = session.clean_file("model.bin", content.to_vec()).unwrap();
        let pointer = Pointer::parse(&pointer_bytes).unwrap();
        let expected_hash: [u8; 32] = *blake3::hash(content).as_bytes();
        assert_eq!(pointer.file_hash, expected_hash);
    }

    #[test]
    fn lfs_clean_empty_file_produces_empty_pointer() {
        use crab_git::lfs_pointer::LfsPointer;

        let Some(dir) = git_repo_tempdir() else {
            eprintln!("SKIP: git unavailable or fixture setup failed");
            return;
        };
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        session.set_repo_root(dir.path().to_path_buf());

        let pointer_bytes = session.clean_file("empty.bin", Vec::new()).unwrap();

        // Empty content produces a zero-size LFS pointer (serialized as empty bytes).
        let pointer = LfsPointer::parse(&pointer_bytes).unwrap();
        assert_eq!(pointer.size, 0);
    }

    // --- clean_stream tests ---

    /// Encode a single pkt-line data packet: `{len:04x}{body}`.
    ///
    /// Body must be at most 65 516 bytes (pkt-line max body size).
    fn encode_pktline(body: &[u8]) -> Vec<u8> {
        let len = body.len() + 4;
        assert!(len <= 0xffff, "pkt-line body too large for test helper");
        let mut out = format!("{len:04x}").into_bytes();
        out.extend_from_slice(body);
        out
    }

    /// Encode a pkt-line data packet followed by a flush.
    fn encode_pktline_then_flush(body: &[u8]) -> Vec<u8> {
        let mut out = encode_pktline(body);
        out.extend_from_slice(b"0000");
        out
    }

    #[test]
    fn clean_stream_matches_clean_file_for_same_content() {
        use crate::git::filter_process::PktLineReader;

        let content = b"hello streaming world".to_vec();

        let ctx = AppContext::default();
        let mut session_a = CleanSession::new(ctx.clone());
        let via_file = session_a.clean_file("test.bin", content.clone()).unwrap();

        let framed = frame_as_pktlines(&content);
        let mut session_b = CleanSession::new(ctx);
        let mut reader = PktLineReader::from_slice(&framed);
        let via_stream = session_b.clean_stream("test.bin", &mut reader).unwrap();

        assert_eq!(via_file, via_stream);
    }

    #[test]
    fn clean_stream_pointer_passthrough() {
        use crate::git::filter_process::PktLineReader;

        // Build a real crab pointer payload; classify() must recognize it.
        let file_hash = [0xA5u8; 32];
        let pointer = Pointer {
            file_hash,
            size: 123,
            shard_hint: None,
        };
        let pointer_bytes = pointer.serialize();

        let framed = encode_pktline_then_flush(&pointer_bytes);

        let ctx = AppContext::default();
        let mut session = CleanSession::new(ctx);
        let mut reader = PktLineReader::from_slice(&framed);
        let out = session.clean_stream("test.bin", &mut reader).unwrap();

        assert_eq!(out, pointer_bytes);
    }

    #[test]
    fn clean_stream_recognizes_pointer_split_across_packets() {
        use crate::git::filter_process::PktLineReader;

        let pointer_bytes = Pointer {
            file_hash: [0xA5u8; 32],
            size: 123,
            shard_hint: None,
        }
        .serialize();
        let split = pointer_bytes.len() / 2;
        let mut framed = encode_pktline(&pointer_bytes[..split]);
        framed.extend_from_slice(&encode_pktline(&pointer_bytes[split..]));
        framed.extend_from_slice(b"0000");

        let mut session = CleanSession::new(AppContext::default());
        let mut reader = PktLineReader::from_slice(&framed);
        let out = session.clean_stream("test.bin", &mut reader).unwrap();

        assert_eq!(out, pointer_bytes);
    }

    #[test]
    fn clean_stream_pointer_prefix_does_not_bypass_cleaning() {
        use crate::git::filter_process::PktLineReader;

        let pointer_bytes = Pointer {
            file_hash: [0xA5u8; 32],
            size: 123,
            shard_hint: None,
        }
        .serialize();
        let tail = vec![0x5a; 2 * 1024];
        let mut content = pointer_bytes.clone();
        content.extend_from_slice(&tail);

        let mut expected_session = CleanSession::new(AppContext::default());
        let expected = expected_session.clean_file("test.bin", content).unwrap();

        let mut framed = encode_pktline(&pointer_bytes);
        framed.extend_from_slice(&encode_pktline(&tail));
        framed.extend_from_slice(b"0000");
        let mut session = CleanSession::new(AppContext::default());
        let mut reader = PktLineReader::from_slice(&framed);
        let out = session.clean_stream("test.bin", &mut reader).unwrap();

        assert_eq!(out, expected);
    }

    #[test]
    fn clean_stream_empty_content() {
        use crate::git::filter_process::PktLineReader;

        let flush_only = b"0000".to_vec();

        let ctx = AppContext::default();
        let mut session_a = CleanSession::new(ctx.clone());
        let via_file = session_a.clean_file("empty.bin", Vec::new()).unwrap();

        let mut session_b = CleanSession::new(ctx);
        let mut reader = PktLineReader::from_slice(&flush_only);
        let via_stream = session_b.clean_stream("empty.bin", &mut reader).unwrap();

        let pointer = Pointer::parse(&via_stream).unwrap();
        assert_eq!(pointer.size, 0);
        assert_eq!(via_file, via_stream);
    }

    #[test]
    fn clean_fast_path_discards_real_provisional_staging_bytes() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let staging = Arc::new(
            rt.block_on(crab_staging::StagingArea::open(tmp.path().join("staging")))
                .expect("open staging"),
        );

        let mut state = 0xC0FF_EE42_D15C_A11Du64;
        let mut content = Vec::with_capacity(9 * 1024 * 1024);
        for _ in 0..content.capacity() {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            content.push((state >> 33) as u8);
        }

        let file_hash: [u8; 32] = *blake3::hash(&content).as_bytes();
        let checker = MockFileIndexChecker {
            known_hashes: HashSet::from([file_hash]),
            shard_hint: None,
        };
        let stager = Some(CleanStaging {
            staging: Arc::clone(&staging),
            handle: rt.handle().clone(),
            _temporary: None,
        });
        let mut session = CleanSession::with_deps(
            AppContext::default(),
            Box::new(checker),
            stager,
            &[file_hash],
            0,
        );

        let pointer_bytes = session.clean_file("known-large.bin", content).unwrap();

        let pointer = Pointer::parse(&pointer_bytes).unwrap();
        assert_eq!(pointer.file_hash, file_hash);

        let stats = staging.stats().expect("staging stats");
        assert_eq!(stats.file_count, 0);
        assert_eq!(stats.current_segment_bytes, 0);
        assert_eq!(stats.total_staged_bytes, 0);
        assert_eq!(stats.live_bytes, 0);
        assert_eq!(stats.chunk_count, 0);
    }

    #[test]
    fn clean_slow_path_persists_recipe_and_reuses_identical_payloads() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let staging = Arc::new(
            rt.block_on(crab_staging::StagingArea::open(tmp.path().join("staging")))
                .expect("open staging"),
        );
        let content = b"shared clean recipe payload".repeat(64 * 1024);
        let expected_hash = *blake3::hash(&content).as_bytes();
        let stager = Some(CleanStaging {
            staging: Arc::clone(&staging),
            handle: rt.handle().clone(),
            _temporary: None,
        });
        let mut session = CleanSession::with_deps(
            AppContext::default(),
            Box::new(NoopFileIndexChecker),
            stager,
            &[],
            u64::MAX,
        );

        let first = session.clean_file("first.bin", content.clone()).unwrap();
        let after_first = staging.stats().unwrap();
        let second = session.clean_file("second.bin", content).unwrap();
        let after_second = staging.stats().unwrap();

        assert_eq!(Pointer::parse(&first).unwrap().file_hash, expected_hash);
        assert_eq!(first, second);
        assert_eq!(after_first.file_count, 1);
        assert_eq!(after_second.file_count, 1);
        assert_eq!(after_second.chunk_count, after_first.chunk_count);
        assert_eq!(
            after_second.current_segment_bytes, after_first.current_segment_bytes,
            "an exact recipe hit must not rewrite identical chunk payloads"
        );
    }

    #[test]
    fn clean_stream_promotes_unpublished_prepared_recipe_without_segment_rewrite() {
        use crate::git::filter_process::PktLineReader;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("model.bin");
        let content = (0..9 * 1024 * 1024)
            .map(|index| (index as u64).wrapping_mul(2_654_435_761).to_le_bytes()[3])
            .collect::<Vec<_>>();
        std::fs::write(&path, &content).unwrap();
        let staging = Arc::new(
            rt.block_on(crab_staging::StagingArea::open(
                tmp.path().join(".crab/staging"),
            ))
            .expect("open staging"),
        );
        let prepared = rt
            .block_on(crab_staging::stream::stage_file_streaming(
                &path,
                tmp.path(),
                &staging,
                crab_staging::ingest::StreamStageProgress {
                    xorb_builder: Some(crab_staging::ingest::StreamStageXorbBuilder::new(
                        1,
                        crab_xet::xorb::builder::XorbBuilder::new,
                    )),
                    ..crab_staging::ingest::StreamStageProgress::default()
                },
                &tokio_util::sync::CancellationToken::new(),
            ))
            .expect("prepare file");
        assert_eq!(staging.stats().unwrap().current_segment_bytes, 0);
        assert_eq!(
            staging
                .unpublished_local_recipe_for_path(Path::new("model.bin"))
                .unwrap(),
            Some(prepared.recipe.clone())
        );

        let stager = Some(CleanStaging {
            staging: Arc::clone(&staging),
            handle: rt.handle().clone(),
            _temporary: None,
        });
        let mut session = CleanSession::with_deps(
            AppContext::default(),
            Box::new(NoopFileIndexChecker),
            stager,
            &[],
            u64::MAX,
        );
        let framed = frame_as_pktlines(&content);
        let mut reader = PktLineReader::from_slice(&framed);

        let pointer = session.clean_stream("model.bin", &mut reader).unwrap();

        assert_eq!(
            Pointer::parse(&pointer).unwrap().file_hash,
            prepared.file_hash
        );
        assert_eq!(
            staging.stats().unwrap().current_segment_bytes,
            0,
            "promotion must reuse the prepared authority without provisional segment writes"
        );
    }
}
