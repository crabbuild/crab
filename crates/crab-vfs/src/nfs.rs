//! NFSv3 adapter for Crab's virtual filesystem pipeline.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nfs3_server::vfs::{
    DirEntryPlus, FileHandleU64, NextResult, NfsFileSystem, NfsReadFileSystem, ReadDirPlusIterator,
    VFSCapabilities,
};
use nfs3_types::nfs3::{
    self as nfs, fattr3, filename3, ftype3, nfspath3, nfsstat3, nfstime3, sattr3, specdata3,
    stable_how,
};
use tracing::{debug, warn};

use crate::core::error::CrabError;
use crate::engine::{VfsEngine, VfsReadMetricsSnapshot};
use crate::hydration::HydrationReadStatsSnapshot;
use crate::read_lease_pool::{ReadLeasePin, ReadLeasePool, ReadLeasePoolSnapshot};
use crate::resolver::{FuseResolver, ResolvedNode};
use crate::snapshot::NodeType;

const ROOT_ID: u64 = 1;
const GITFILE_ID: u64 = 2;
const FIRST_DYNAMIC_ID: u64 = 3;
const NFS_READ_LEASE_POOL_MAX_ENTRIES: usize = 1024;
const NFS_READ_LEASE_POOL_MAX_BYTES: usize = 8 * 1024 * 1024;
const NFS_DIRECTORY_PAGE_CACHE_MAX_ENTRIES: usize = 256;
const NFS_DIRECTORY_PAGE_CACHE_MAX_BYTES: usize = 16 * 1024 * 1024;
const NFS_DIRECTORY_PAGE_ENTRIES: usize = 256;
const NFS_DIRECTORY_PAGE_INVALIDATION_MAX_ENTRIES: usize = 4096;
const NFS_LARGE_READDIRPLUS_ENTRY_THRESHOLD: usize = 1024;
const NFS_TRANSFER_MAX_BYTES: u32 = 1024 * 1024;
const NFS_MAX_FILE_SIZE_BYTES: u64 = 128 * 1024 * 1024 * 1024;
const NFS_MAX_COMPONENT_BYTES: usize = 255;
const NFS_MAX_PATH_BYTES: usize = 1024;

/// NFSv3 filesystem backed by Crab's resolver, hydration service, and overlay.
pub struct CrabNfsFs {
    resolver: Arc<FuseResolver>,
    engine: Arc<VfsEngine>,
    ids: Arc<NfsIdTable>,
    read_leases: Arc<ReadLeasePool>,
    directory_pages: Arc<NfsDirectoryPageCache>,
    write_journal: Arc<NfsWriteJournal>,
    protocol_stats: Arc<NfsProtocolStats>,
    gitfile_content: Vec<u8>,
    uid: u32,
    gid: u32,
    read_only: bool,
}

impl CrabNfsFs {
    /// Create an NFS adapter for an already-built mount pipeline.
    #[must_use]
    pub fn new(
        resolver: Arc<FuseResolver>,
        engine: Arc<VfsEngine>,
        git_dir: &str,
        read_only: bool,
        exclusive_verifiers_path: Option<PathBuf>,
    ) -> Self {
        let (uid, gid) = current_ids();
        Self {
            resolver,
            engine,
            ids: Arc::new(NfsIdTable::new(exclusive_verifiers_path)),
            read_leases: ReadLeasePool::new(
                NFS_READ_LEASE_POOL_MAX_ENTRIES,
                NFS_READ_LEASE_POOL_MAX_BYTES,
            ),
            directory_pages: NfsDirectoryPageCache::new(
                NFS_DIRECTORY_PAGE_CACHE_MAX_ENTRIES,
                NFS_DIRECTORY_PAGE_CACHE_MAX_BYTES,
            ),
            write_journal: Arc::new(NfsWriteJournal::new()),
            protocol_stats: Arc::new(NfsProtocolStats::new()),
            gitfile_content: format!("gitdir: {git_dir}\n").into_bytes(),
            uid,
            gid,
            read_only,
        }
    }

    /// Return the write journal used to drain unstable NFS writes.
    #[must_use]
    pub fn write_journal(&self) -> Arc<NfsWriteJournal> {
        Arc::clone(&self.write_journal)
    }

    pub fn read_lease_pool(&self) -> Arc<ReadLeasePool> {
        Arc::clone(&self.read_leases)
    }

    pub fn directory_page_cache(&self) -> Arc<NfsDirectoryPageCache> {
        Arc::clone(&self.directory_pages)
    }

    pub fn protocol_stats(&self) -> Arc<NfsProtocolStats> {
        Arc::clone(&self.protocol_stats)
    }

    fn invalidate_parent_directory_page(&self, path: &str) {
        self.directory_pages.invalidate_path(parent_path(path));
    }

    fn lookup_path(&self, parent_id: u64, name: &str) -> Result<(u64, String), nfsstat3> {
        let parent_path = self.directory_path_for_child(parent_id)?;
        if name == "." {
            return Ok((parent_id, parent_path));
        }
        if name == ".." {
            let ancestor = parent_path
                .rsplit_once('/')
                .map_or("", |(parent, _)| parent);
            let id = self.ids.id_for_path(ancestor, NodeType::Dir)?;
            return Ok((id, ancestor.to_owned()));
        }
        if parent_id == ROOT_ID && name == ".git" {
            return Ok((GITFILE_ID, ".git".to_owned()));
        }

        let child_path = join_path(&parent_path, name);
        let (_mode, _size, node_type, _mtime) =
            self.resolver.getattr(&child_path).map_err(to_nfs)?;
        let id = self.ids.id_for_path(&child_path, node_type)?;
        Ok((id, child_path))
    }

    fn directory_path_for_child(&self, id: u64) -> Result<String, nfsstat3> {
        if id == GITFILE_ID {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }
        let path = self.ids.path(id)?;
        if id == ROOT_ID {
            return Ok(path);
        }
        let (_mode, _size, node_type, _mtime) = self.resolver.getattr(&path).map_err(to_nfs)?;
        if node_type != NodeType::Dir {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }
        Ok(path)
    }

    fn path_for_data_handle(&self, id: u64) -> Result<String, nfsstat3> {
        if id == ROOT_ID || id == GITFILE_ID {
            return self.ids.path(id);
        }
        self.ids.path(id)
    }

    fn attr_for_path(&self, id: u64, path: &str) -> Result<fattr3, nfsstat3> {
        match id {
            ROOT_ID => Ok(self.root_attr()),
            GITFILE_ID => Ok(self.gitfile_attr()),
            _ => {
                let (mode, mut size, node_type, mtime) =
                    self.resolver.getattr(path).map_err(to_nfs)?;
                if self.path_has_unknown_blob_size(path)? {
                    size = self.engine.exact_file_size(path).map_err(to_nfs)?;
                }
                Ok(make_nfs_attr(
                    self.uid, self.gid, id, mode, size, node_type, mtime,
                ))
            }
        }
    }

    fn path_has_unknown_blob_size(&self, path: &str) -> Result<bool, nfsstat3> {
        let node = self.resolver.resolve_path(path).map_err(to_nfs)?;
        Ok(matches!(
            node,
            ResolvedNode::Base(ref base)
                if base.node_type == NodeType::File
                    && base.size == 0
                    && base.object_oid.is_some()
        ))
    }

    fn root_attr(&self) -> fattr3 {
        make_nfs_attr(
            self.uid,
            self.gid,
            ROOT_ID,
            0o040_755,
            4096,
            NodeType::Dir,
            self.resolver.commit_time(),
        )
    }

    fn gitfile_attr(&self) -> fattr3 {
        make_nfs_attr(
            self.uid,
            self.gid,
            GITFILE_ID,
            0o100_444,
            self.gitfile_content.len() as u64,
            NodeType::File,
            self.resolver.commit_time(),
        )
    }

    async fn setattr_path(&self, path: &str, attr: sattr3) -> Result<(), nfsstat3> {
        if self.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        if !self.apply_setattr_path(path, attr).await? {
            return Ok(());
        }
        self.mark_write_journal(path, NfsWriteStability::FileSync);
        self.sync_journal_path(path)
    }

    async fn apply_setattr_path(&self, path: &str, attr: sattr3) -> Result<bool, nfsstat3> {
        ensure_setattr_supported(&attr)?;
        // Crab mounts are noatime. macOS includes an atime update in the
        // SETATTR that follows an exclusive CREATE, so rejecting it makes a
        // successful O_EXCL create surface as EIO to the application.
        let mut changed = false;
        if let nfs::set_mode3::Some(mode) = attr.mode {
            self.engine
                .set_mode(path, mode & 0o7777)
                .await
                .map_err(to_nfs)?;
            changed = true;
        }
        if let nfs::set_size3::Some(size) = attr.size {
            self.engine.truncate(path, size).await.map_err(to_nfs)?;
            changed = true;
        }
        match attr.mtime {
            nfs::set_mtime::DONT_CHANGE => {}
            nfs::set_mtime::SET_TO_SERVER_TIME => {
                self.engine
                    .set_mtime(path, now_nanos())
                    .await
                    .map_err(to_nfs)?;
                changed = true;
            }
            nfs::set_mtime::SET_TO_CLIENT_TIME(time) => {
                self.engine
                    .set_mtime(path, nfstime_to_nanos(time))
                    .await
                    .map_err(to_nfs)?;
                changed = true;
            }
        }
        Ok(changed)
    }

    fn mark_write_journal(&self, path: &str, stability: NfsWriteStability) {
        self.write_journal
            .mark_write(path, stability, self.engine.overlay_view_version());
    }

    fn sync_journal_path(&self, path: &str) -> Result<(), nfsstat3> {
        self.write_journal
            .sync_path(&self.engine, path)
            .map_err(to_nfs)
    }

    fn sync_created_path(&self, path: &str) -> Result<(), nfsstat3> {
        self.mark_write_journal(path, NfsWriteStability::FileSync);
        self.sync_journal_path(path)
    }

    fn read_lease_pin(&self, id: u64, path: &str) -> Result<ReadLeasePin, nfsstat3> {
        if let Some(pin) = self.read_leases.pin(id) {
            return Ok(pin);
        }
        self.open_read_lease_pin(id, path)
    }

    fn open_read_lease_pin(&self, id: u64, path: &str) -> Result<ReadLeasePin, nfsstat3> {
        let lease = self.engine.open_read(path).map_err(to_nfs)?;
        Ok(self.read_leases.insert_and_pin(id, lease))
    }

    async fn read_from_pin(
        &self,
        pin: &ReadLeasePin,
        offset: u64,
        count: u32,
    ) -> crate::core::error::Result<(Vec<u8>, bool)> {
        let data = self.engine.read_at(pin.lease(), offset, count).await?;
        let eof = read_reached_eof(offset, data.len(), count, pin.lease().known_size());
        Ok((data.to_vec(), eof))
    }

    async fn read_regular_file(
        &self,
        id: u64,
        path: &str,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let pin = self.read_lease_pin(id, path)?;
        match self.read_from_pin(&pin, offset, count).await {
            Ok(read) => Ok(read),
            Err(error) if VfsEngine::is_stale_read_lease_error(&error) => {
                self.read_leases.record_stale_retry();
                self.read_leases.evict(id);
                drop(pin);
                let retry_pin = self.open_read_lease_pin(id, path)?;
                self.read_from_pin(&retry_pin, offset, count)
                    .await
                    .map_err(to_nfs)
            }
            Err(error) => Err(to_nfs(error)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NfsRuntimeSnapshot {
    pub read_leases: ReadLeasePoolSnapshot,
    pub directory_pages: NfsDirectoryPageCacheSnapshot,
    pub write_journal: NfsWriteJournalSnapshot,
    pub protocol: NfsProtocolStatsSnapshot,
    pub vfs: VfsReadMetricsSnapshot,
    pub hydration: HydrationReadStatsSnapshot,
}

/// Counts protocol-level NFS pressure at the adapter boundary.
pub struct NfsProtocolStats {
    read_rpcs: AtomicU64,
    read_requested_bytes: AtomicU64,
    read_returned_bytes: AtomicU64,
    read_size_le_4k: AtomicU64,
    read_size_le_64k: AtomicU64,
    read_size_le_1m: AtomicU64,
    read_size_gt_1m: AtomicU64,
    readdirplus_rpcs: AtomicU64,
    readdirplus_entries: AtomicU64,
    readdirplus_materialized_entries: AtomicU64,
    readdirplus_returned_candidates: AtomicU64,
    readdirplus_attr_resolutions: AtomicU64,
    readdirplus_prefetch_paths: AtomicU64,
    readdirplus_cookie_resumes: AtomicU64,
    readdirplus_cookie_misses: AtomicU64,
    readdirplus_skipped_entries: AtomicU64,
    readdirplus_large_dirs: AtomicU64,
    readdirplus_prefetch_errors: AtomicU64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NfsProtocolStatsSnapshot {
    pub read_rpcs: u64,
    pub read_requested_bytes: u64,
    pub read_returned_bytes: u64,
    pub read_size_le_4k: u64,
    pub read_size_le_64k: u64,
    pub read_size_le_1m: u64,
    pub read_size_gt_1m: u64,
    pub readdirplus_rpcs: u64,
    pub readdirplus_entries: u64,
    pub readdirplus_materialized_entries: u64,
    pub readdirplus_returned_candidates: u64,
    pub readdirplus_attr_resolutions: u64,
    pub readdirplus_prefetch_paths: u64,
    pub readdirplus_cookie_resumes: u64,
    pub readdirplus_cookie_misses: u64,
    pub readdirplus_skipped_entries: u64,
    pub readdirplus_large_dirs: u64,
    pub readdirplus_prefetch_errors: u64,
}

pub struct NfsDirectoryPageCache {
    state: Mutex<NfsDirectoryPageCacheState>,
    max_entries: usize,
    max_estimated_bytes: usize,
}

struct NfsDirectoryPageCacheState {
    entries: HashMap<NfsDirectoryPageCacheKey, NfsDirectoryPageCacheEntry>,
    estimated_bytes: usize,
    access_clock: u64,
    version_clock: u64,
    reset_version: u64,
    path_versions: HashMap<String, u64>,
    subtree_versions: HashMap<String, u64>,
    hits: u64,
    misses: u64,
    evictions: u64,
    stale_evictions: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct NfsDirectoryPageCacheKey {
    path: String,
    generation: i64,
    directory_version: u64,
    after_name: Option<String>,
    include_virtual_git: bool,
}

struct NfsDirectoryPageCacheEntry {
    candidates: Arc<Vec<NfsDirectoryCandidate>>,
    last_access: u64,
    estimated_bytes: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NfsDirectoryPageCacheSnapshot {
    pub entries: usize,
    pub max_entries: usize,
    pub estimated_bytes: usize,
    pub max_estimated_bytes: usize,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub stale_evictions: u64,
}

impl NfsDirectoryPageCache {
    pub fn new(max_entries: usize, max_estimated_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(NfsDirectoryPageCacheState {
                entries: HashMap::new(),
                estimated_bytes: 0,
                access_clock: 0,
                version_clock: 0,
                reset_version: 0,
                path_versions: HashMap::new(),
                subtree_versions: HashMap::new(),
                hits: 0,
                misses: 0,
                evictions: 0,
                stale_evictions: 0,
            }),
            max_entries: max_entries.max(1),
            max_estimated_bytes: max_estimated_bytes.max(1),
        })
    }

    #[cfg(test)]
    fn key(&self, path: &str, generation: i64) -> NfsDirectoryPageCacheKey {
        self.page_key(path, generation, None, false)
    }

    fn page_key(
        &self,
        path: &str,
        generation: i64,
        after_name: Option<&str>,
        include_virtual_git: bool,
    ) -> NfsDirectoryPageCacheKey {
        let state = self.lock_state();
        NfsDirectoryPageCacheKey {
            path: path.to_owned(),
            generation,
            directory_version: state.directory_version_for_path(path),
            after_name: after_name.map(str::to_owned),
            include_virtual_git,
        }
    }

    fn get(&self, key: &NfsDirectoryPageCacheKey) -> Option<Arc<Vec<NfsDirectoryCandidate>>> {
        let mut state = self.lock_state();
        let access = state.next_access();
        let hit = if let Some(entry) = state.entries.get_mut(key) {
            entry.last_access = access;
            Some(Arc::clone(&entry.candidates))
        } else {
            None
        };
        if let Some(candidates) = hit {
            state.hits = state.hits.saturating_add(1);
            return Some(candidates);
        };
        state.misses = state.misses.saturating_add(1);
        state.evict_stale_for_path(key);
        None
    }

    fn insert(&self, key: NfsDirectoryPageCacheKey, candidates: Arc<Vec<NfsDirectoryCandidate>>) {
        let estimated_bytes = directory_page_estimated_bytes(&key, &candidates);
        let mut state = self.lock_state();
        let last_access = state.next_access();
        if let Some(previous) = state.entries.insert(
            key,
            NfsDirectoryPageCacheEntry {
                candidates,
                last_access,
                estimated_bytes,
            },
        ) {
            state.estimated_bytes = state
                .estimated_bytes
                .saturating_sub(previous.estimated_bytes);
        }
        state.estimated_bytes = state.estimated_bytes.saturating_add(estimated_bytes);
        state.shrink(self.max_entries, self.max_estimated_bytes);
    }

    fn invalidate_path(&self, path: &str) {
        let mut state = self.lock_state();
        let version = state.next_version();
        state.path_versions.insert(path.to_owned(), version);
        state.remove_matching(|key| key.path == path);
        state.compact_invalidations(NFS_DIRECTORY_PAGE_INVALIDATION_MAX_ENTRIES);
    }

    fn invalidate_subtree(&self, path: &str) {
        let mut state = self.lock_state();
        let version = state.next_version();
        state.subtree_versions.insert(path.to_owned(), version);
        state.remove_matching(|key| path_is_at_or_under(&key.path, path));
        state.compact_invalidations(NFS_DIRECTORY_PAGE_INVALIDATION_MAX_ENTRIES);
    }

    fn invalidate_rename(&self, from_path: &str, to_path: &str) {
        let mut state = self.lock_state();
        let version = state.next_version();
        state.subtree_versions.insert(from_path.to_owned(), version);
        state.subtree_versions.insert(to_path.to_owned(), version);
        state.remove_matching(|key| {
            path_is_at_or_under(&key.path, from_path) || path_is_at_or_under(&key.path, to_path)
        });
        state.compact_invalidations(NFS_DIRECTORY_PAGE_INVALIDATION_MAX_ENTRIES);
    }

    pub fn invalidate_all(&self) {
        let mut state = self.lock_state();
        state.invalidate_all();
    }

    pub fn snapshot(&self) -> NfsDirectoryPageCacheSnapshot {
        let state = self.lock_state();
        NfsDirectoryPageCacheSnapshot {
            entries: state.entries.len(),
            max_entries: self.max_entries,
            estimated_bytes: state.estimated_bytes,
            max_estimated_bytes: self.max_estimated_bytes,
            hits: state.hits,
            misses: state.misses,
            evictions: state.evictions,
            stale_evictions: state.stale_evictions,
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, NfsDirectoryPageCacheState> {
        self.state.lock().unwrap_or_else(|error| {
            warn!("NFS directory page cache mutex was poisoned; recovering");
            error.into_inner()
        })
    }
}

impl NfsDirectoryPageCacheState {
    fn next_access(&mut self) -> u64 {
        self.access_clock = self.access_clock.saturating_add(1);
        self.access_clock
    }

    fn next_version(&mut self) -> u64 {
        self.version_clock = self.version_clock.saturating_add(1);
        self.version_clock
    }

    fn directory_version_for_path(&self, path: &str) -> u64 {
        let mut version = self.reset_version;
        if let Some(path_version) = self.path_versions.get(path) {
            version = version.max(*path_version);
        }
        for ancestor in path_and_ancestors(path) {
            if let Some(subtree_version) = self.subtree_versions.get(ancestor) {
                version = version.max(*subtree_version);
            }
        }
        version
    }

    fn evict_stale_for_path(&mut self, key: &NfsDirectoryPageCacheKey) {
        let stale = self
            .entries
            .keys()
            .filter(|candidate| {
                candidate.path == key.path
                    && (candidate.generation != key.generation
                        || candidate.directory_version != key.directory_version)
            })
            .cloned()
            .collect::<Vec<_>>();
        for stale_key in stale {
            self.remove_stale(&stale_key);
        }
    }

    fn shrink(&mut self, max_entries: usize, max_estimated_bytes: usize) {
        while self.entries.len() > max_entries || self.estimated_bytes > max_estimated_bytes {
            if self.entries.len() == 1 && self.estimated_bytes > max_estimated_bytes {
                return;
            }
            let Some(evict_key) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_access)
                .map(|(key, _)| key.clone())
            else {
                return;
            };
            if let Some(entry) = self.entries.remove(&evict_key) {
                self.estimated_bytes = self.estimated_bytes.saturating_sub(entry.estimated_bytes);
                self.evictions = self.evictions.saturating_add(1);
            }
        }
    }

    fn remove_matching<F>(&mut self, mut matches: F)
    where
        F: FnMut(&NfsDirectoryPageCacheKey) -> bool,
    {
        let stale = self
            .entries
            .keys()
            .filter(|key| matches(key))
            .cloned()
            .collect::<Vec<_>>();
        for stale_key in stale {
            self.remove_stale(&stale_key);
        }
    }

    fn remove_stale(&mut self, key: &NfsDirectoryPageCacheKey) {
        if let Some(entry) = self.entries.remove(key) {
            self.estimated_bytes = self.estimated_bytes.saturating_sub(entry.estimated_bytes);
            self.stale_evictions = self.stale_evictions.saturating_add(1);
        }
    }

    fn compact_invalidations(&mut self, max_entries: usize) {
        if self.path_versions.len() + self.subtree_versions.len() <= max_entries {
            return;
        }
        self.invalidate_all();
    }

    fn invalidate_all(&mut self) {
        self.next_version();
        self.reset_version = self.version_clock;
        if !self.entries.is_empty() {
            self.stale_evictions = self
                .stale_evictions
                .saturating_add(self.entries.len() as u64);
            self.entries.clear();
            self.estimated_bytes = 0;
        }
        self.path_versions.clear();
        self.subtree_versions.clear();
    }
}

fn directory_page_estimated_bytes(
    key: &NfsDirectoryPageCacheKey,
    candidates: &[NfsDirectoryCandidate],
) -> usize {
    key.path.len()
        + std::mem::size_of::<NfsDirectoryPageCacheKey>()
        + candidates
            .iter()
            .map(directory_candidate_estimated_bytes)
            .sum::<usize>()
}

fn directory_candidate_estimated_bytes(candidate: &NfsDirectoryCandidate) -> usize {
    std::mem::size_of::<NfsDirectoryCandidate>()
        + candidate.name.len()
        + candidate.path.as_ref().map_or(0, String::len)
        + candidate
            .attr
            .as_ref()
            .map_or(0, |_| std::mem::size_of::<fattr3>())
}

impl NfsProtocolStats {
    pub fn new() -> Self {
        Self {
            read_rpcs: AtomicU64::new(0),
            read_requested_bytes: AtomicU64::new(0),
            read_returned_bytes: AtomicU64::new(0),
            read_size_le_4k: AtomicU64::new(0),
            read_size_le_64k: AtomicU64::new(0),
            read_size_le_1m: AtomicU64::new(0),
            read_size_gt_1m: AtomicU64::new(0),
            readdirplus_rpcs: AtomicU64::new(0),
            readdirplus_entries: AtomicU64::new(0),
            readdirplus_materialized_entries: AtomicU64::new(0),
            readdirplus_returned_candidates: AtomicU64::new(0),
            readdirplus_attr_resolutions: AtomicU64::new(0),
            readdirplus_prefetch_paths: AtomicU64::new(0),
            readdirplus_cookie_resumes: AtomicU64::new(0),
            readdirplus_cookie_misses: AtomicU64::new(0),
            readdirplus_skipped_entries: AtomicU64::new(0),
            readdirplus_large_dirs: AtomicU64::new(0),
            readdirplus_prefetch_errors: AtomicU64::new(0),
        }
    }

    fn record_read_request(&self, requested_bytes: u32) {
        self.read_rpcs.fetch_add(1, Ordering::Relaxed);
        self.read_requested_bytes
            .fetch_add(u64::from(requested_bytes), Ordering::Relaxed);
        match requested_bytes {
            0..=4096 => &self.read_size_le_4k,
            4097..=65_536 => &self.read_size_le_64k,
            65_537..=1_048_576 => &self.read_size_le_1m,
            _ => &self.read_size_gt_1m,
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    fn record_read_response(&self, returned_bytes: usize) {
        self.read_returned_bytes
            .fetch_add(returned_bytes as u64, Ordering::Relaxed);
    }

    fn record_readdirplus_request(&self, cookie: u64, cookie_miss: bool) {
        self.readdirplus_rpcs.fetch_add(1, Ordering::Relaxed);
        if cookie != 0 {
            self.readdirplus_cookie_resumes
                .fetch_add(1, Ordering::Relaxed);
        }
        if cookie_miss {
            self.readdirplus_cookie_misses
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record_readdirplus_page(
        &self,
        materialized_entries: usize,
        prefetch_paths: usize,
        prefetch_error: bool,
    ) {
        self.readdirplus_materialized_entries
            .fetch_add(materialized_entries as u64, Ordering::Relaxed);
        self.readdirplus_prefetch_paths
            .fetch_add(prefetch_paths as u64, Ordering::Relaxed);
        if prefetch_error {
            self.readdirplus_prefetch_errors
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record_readdirplus_entry(&self, attr_resolved: bool) {
        self.readdirplus_entries.fetch_add(1, Ordering::Relaxed);
        self.readdirplus_returned_candidates
            .fetch_add(1, Ordering::Relaxed);
        if attr_resolved {
            self.readdirplus_attr_resolutions
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record_large_readdirplus(&self) {
        self.readdirplus_large_dirs.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> NfsProtocolStatsSnapshot {
        NfsProtocolStatsSnapshot {
            read_rpcs: self.read_rpcs.load(Ordering::Relaxed),
            read_requested_bytes: self.read_requested_bytes.load(Ordering::Relaxed),
            read_returned_bytes: self.read_returned_bytes.load(Ordering::Relaxed),
            read_size_le_4k: self.read_size_le_4k.load(Ordering::Relaxed),
            read_size_le_64k: self.read_size_le_64k.load(Ordering::Relaxed),
            read_size_le_1m: self.read_size_le_1m.load(Ordering::Relaxed),
            read_size_gt_1m: self.read_size_gt_1m.load(Ordering::Relaxed),
            readdirplus_rpcs: self.readdirplus_rpcs.load(Ordering::Relaxed),
            readdirplus_entries: self.readdirplus_entries.load(Ordering::Relaxed),
            readdirplus_materialized_entries: self
                .readdirplus_materialized_entries
                .load(Ordering::Relaxed),
            readdirplus_returned_candidates: self
                .readdirplus_returned_candidates
                .load(Ordering::Relaxed),
            readdirplus_attr_resolutions: self.readdirplus_attr_resolutions.load(Ordering::Relaxed),
            readdirplus_prefetch_paths: self.readdirplus_prefetch_paths.load(Ordering::Relaxed),
            readdirplus_cookie_resumes: self.readdirplus_cookie_resumes.load(Ordering::Relaxed),
            readdirplus_cookie_misses: self.readdirplus_cookie_misses.load(Ordering::Relaxed),
            readdirplus_skipped_entries: self.readdirplus_skipped_entries.load(Ordering::Relaxed),
            readdirplus_large_dirs: self.readdirplus_large_dirs.load(Ordering::Relaxed),
            readdirplus_prefetch_errors: self.readdirplus_prefetch_errors.load(Ordering::Relaxed),
        }
    }
}

impl NfsReadFileSystem for CrabNfsFs {
    type Handle = FileHandleU64;

    fn root_dir(&self) -> Self::Handle {
        FileHandleU64::new(ROOT_ID)
    }

    async fn lookup(
        &self,
        dirid: &Self::Handle,
        filename: &filename3<'_>,
    ) -> Result<Self::Handle, nfsstat3> {
        let name = nfs_name(filename)?;
        let (id, _path) = self.lookup_path(dirid.as_u64(), name)?;
        Ok(FileHandleU64::new(id))
    }

    async fn getattr(&self, id: &Self::Handle) -> Result<fattr3, nfsstat3> {
        let id = id.as_u64();
        let path = self.ids.path(id)?;
        self.attr_for_path(id, &path)
    }

    async fn fsinfo(&self, root_fileid: &Self::Handle) -> Result<nfs::FSINFO3resok, nfsstat3> {
        let obj_attributes = self
            .getattr(root_fileid)
            .await
            .map_or(nfs::post_op_attr::None, nfs::post_op_attr::Some);
        Ok(nfs::FSINFO3resok {
            obj_attributes,
            rtmax: NFS_TRANSFER_MAX_BYTES,
            rtpref: NFS_TRANSFER_MAX_BYTES,
            rtmult: NFS_TRANSFER_MAX_BYTES,
            wtmax: NFS_TRANSFER_MAX_BYTES,
            wtpref: NFS_TRANSFER_MAX_BYTES,
            wtmult: NFS_TRANSFER_MAX_BYTES,
            dtpref: NFS_TRANSFER_MAX_BYTES,
            maxfilesize: NFS_MAX_FILE_SIZE_BYTES,
            time_delta: nfstime3 {
                seconds: 1,
                nseconds: 0,
            },
            properties: nfs::FSF3_SYMLINK | nfs::FSF3_HOMOGENEOUS,
        })
    }

    async fn read(
        &self,
        id: &Self::Handle,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        self.protocol_stats.record_read_request(count);
        let id = id.as_u64();
        if id == GITFILE_ID {
            let start = usize::try_from(offset)
                .ok()
                .map_or(self.gitfile_content.len(), |offset| {
                    offset.min(self.gitfile_content.len())
                });
            let count = usize::try_from(count).unwrap_or(usize::MAX);
            let end = start.saturating_add(count).min(self.gitfile_content.len());
            let data = self.gitfile_content[start..end].to_vec();
            let eof = end >= self.gitfile_content.len();
            self.protocol_stats.record_read_response(data.len());
            return Ok((data, eof));
        }

        let path = self.path_for_data_handle(id)?;
        let node = self.resolver.resolve_path(&path).map_err(to_nfs)?;
        ensure_regular_file_for_read(node.node_type())?;
        let read = self.read_regular_file(id, &path, offset, count).await?;
        self.protocol_stats.record_read_response(read.0.len());
        Ok(read)
    }

    async fn readdirplus(
        &self,
        dirid: &Self::Handle,
        cookie: u64,
    ) -> Result<impl ReadDirPlusIterator<Self::Handle>, nfsstat3> {
        let dirid = dirid.as_u64();
        let path = self.directory_path_for_child(dirid)?;
        let (after_name, include_virtual_git, cookie_miss) =
            readdirplus_cursor(&self.ids, dirid, &path, cookie);
        self.protocol_stats
            .record_readdirplus_request(cookie, cookie_miss);
        let loader = NfsDirectoryPageLoader {
            resolver: Arc::clone(&self.resolver),
            engine: Arc::clone(&self.engine),
            ids: Arc::clone(&self.ids),
            directory_pages: Arc::clone(&self.directory_pages),
            protocol_stats: Arc::clone(&self.protocol_stats),
            gitfile_attr: self.gitfile_attr(),
            uid: self.uid,
            gid: self.gid,
        };
        let candidates =
            loader.load_candidates(dirid, &path, after_name.as_deref(), include_virtual_git)?;
        let materialized_entries = candidates.len();
        Ok(CrabNfsDirIter {
            loader,
            dirid,
            path,
            candidates,
            index: 0,
            after_name,
            materialized_entries,
            large_directory_recorded: false,
        })
    }

    async fn readlink(&self, id: &Self::Handle) -> Result<nfspath3<'_>, nfsstat3> {
        let id = id.as_u64();
        if id == GITFILE_ID {
            return Err(nfsstat3::NFS3ERR_INVAL);
        }
        let path = self.ids.path(id)?;
        match self.resolver.resolve_path(&path).map_err(to_nfs)? {
            ResolvedNode::Base(base) if base.node_type == NodeType::Symlink => {
                let oid = base.object_oid.ok_or(nfsstat3::NFS3ERR_NOENT)?;
                let target = self.engine.read_symlink_target(&oid).map_err(to_nfs)?;
                Ok(target.into_bytes().into())
            }
            ResolvedNode::Overlay(entry) if entry.node_type == NodeType::Symlink => {
                let Some(overlay) = self.engine.overlay().as_ref() else {
                    return Err(nfsstat3::NFS3ERR_NOENT);
                };
                let backing = overlay
                    .get_backing_path(&path)
                    .ok_or(nfsstat3::NFS3ERR_NOENT)?;
                let target =
                    std::fs::read(backing).map_err(|error| to_nfs(CrabError::Io(error)))?;
                Ok(target.into())
            }
            _ => Err(nfsstat3::NFS3ERR_INVAL),
        }
    }
}

impl NfsFileSystem for CrabNfsFs {
    fn capabilities(&self) -> VFSCapabilities {
        if self.read_only {
            VFSCapabilities::ReadOnly
        } else {
            VFSCapabilities::ReadWrite
        }
    }

    async fn setattr(&self, id: &Self::Handle, attr: sattr3) -> Result<fattr3, nfsstat3> {
        let id = id.as_u64();
        if id == ROOT_ID || id == GITFILE_ID {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let path = self.ids.path(id)?;
        validate_setattr_target(self.resolver.getattr(&path).map_err(to_nfs)?.2, &attr)?;
        self.setattr_path(&path, attr).await?;
        self.invalidate_parent_directory_page(&path);
        self.read_leases.evict(id);
        self.attr_for_path(id, &path)
    }

    async fn write(
        &self,
        id: &Self::Handle,
        offset: u64,
        data: &[u8],
        stable: stable_how,
    ) -> Result<(fattr3, stable_how), nfsstat3> {
        if self.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let id = id.as_u64();
        if id == ROOT_ID {
            return Err(nfsstat3::NFS3ERR_INVAL);
        }
        if id == GITFILE_ID {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let path = self.ids.path(id)?;
        let (_mode, _size, node_type, _mtime) = self.resolver.getattr(&path).map_err(to_nfs)?;
        ensure_regular_file_for_write(node_type)?;
        self.engine
            .write(&path, offset, data)
            .await
            .map_err(to_nfs)?;
        self.invalidate_parent_directory_page(&path);
        self.read_leases.evict(id);
        self.mark_write_journal(&path, NfsWriteStability::from(stable));
        let committed = match stable {
            stable_how::UNSTABLE => stable_how::UNSTABLE,
            stable_how::DATA_SYNC | stable_how::FILE_SYNC => {
                self.sync_journal_path(&path)?;
                stable_how::FILE_SYNC
            }
        };
        let attr = self.attr_for_path(id, &path)?;
        Ok((attr, committed))
    }

    async fn create(
        &self,
        dirid: &Self::Handle,
        filename: &filename3<'_>,
        attr: sattr3,
    ) -> Result<(Self::Handle, fattr3), nfsstat3> {
        if self.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let name = nfs_child_name(filename)?;
        ensure_mutable_child(dirid.as_u64(), name)?;
        let parent = self.directory_path_for_child(dirid.as_u64())?;
        let path = join_path(&parent, name);
        ensure_path_absent(self.resolver.resolve_path(&path))?;
        ensure_setattr_supported(&attr)?;
        let mode = match attr.mode {
            nfs::set_mode3::Some(mode) => mode & 0o7777,
            nfs::set_mode3::None => 0o644,
        };
        self.engine.create(&path, mode).await.map_err(to_nfs)?;
        self.directory_pages.invalidate_path(&parent);
        self.apply_setattr_path(&path, attr).await?;
        self.sync_created_path(&path)?;
        let id = self.ids.id_for_path(&path, NodeType::File)?;
        let attr = self.attr_for_path(id, &path)?;
        Ok((FileHandleU64::new(id), attr))
    }

    async fn create_exclusive(
        &self,
        dirid: &Self::Handle,
        filename: &filename3<'_>,
        createverf: nfs::createverf3,
    ) -> Result<Self::Handle, nfsstat3> {
        if self.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let name = nfs_child_name(filename)?;
        ensure_mutable_child(dirid.as_u64(), name)?;
        let parent = self.directory_path_for_child(dirid.as_u64())?;
        let path = join_path(&parent, name);

        match self.resolver.getattr(&path) {
            Ok((_mode, _size, NodeType::File, _mtime))
                if self.ids.exclusive_verifier(&path)? == Some(createverf) =>
            {
                let id = self.ids.id_for_path(&path, NodeType::File)?;
                return Ok(FileHandleU64::new(id));
            }
            Ok((_mode, _size, _node_type, _mtime)) => return Err(nfsstat3::NFS3ERR_EXIST),
            Err(CrabError::NotFound { .. }) => {}
            Err(error) => return Err(to_nfs(error)),
        }

        self.ids.record_exclusive_create(&path, createverf)?;
        if let Err(error) = self.engine.create(&path, 0o644).await {
            let _ = self.ids.remove_exclusive_verifier(&path);
            return Err(to_nfs(error));
        }
        self.directory_pages.invalidate_path(&parent);
        self.sync_created_path(&path)?;
        let id = self.ids.id_for_path(&path, NodeType::File)?;
        Ok(FileHandleU64::new(id))
    }

    async fn mkdir(
        &self,
        dirid: &Self::Handle,
        dirname: &filename3<'_>,
    ) -> Result<(Self::Handle, fattr3), nfsstat3> {
        if self.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let name = nfs_child_name(dirname)?;
        ensure_mutable_child(dirid.as_u64(), name)?;
        let parent = self.directory_path_for_child(dirid.as_u64())?;
        let path = join_path(&parent, name);
        ensure_path_absent(self.resolver.resolve_path(&path))?;
        self.engine.mkdir(&path, 0o755).await.map_err(to_nfs)?;
        self.directory_pages.invalidate_path(&parent);
        self.engine.checkpoint_overlay().map_err(to_nfs)?;
        let id = self.ids.id_for_path(&path, NodeType::Dir)?;
        let attr = self.attr_for_path(id, &path)?;
        Ok((FileHandleU64::new(id), attr))
    }

    async fn remove(&self, dirid: &Self::Handle, filename: &filename3<'_>) -> Result<(), nfsstat3> {
        if self.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let name = nfs_child_name(filename)?;
        ensure_mutable_child(dirid.as_u64(), name)?;
        let (_id, path) = self.lookup_path(dirid.as_u64(), name)?;
        let (_mode, _size, node_type, _mtime) = self.resolver.getattr(&path).map_err(to_nfs)?;
        let result = if node_type == NodeType::Dir {
            self.engine.rmdir(&path).await
        } else {
            self.engine.unlink(&path).await
        };
        result.map_err(to_nfs)?;
        self.invalidate_parent_directory_page(&path);
        self.directory_pages.invalidate_subtree(&path);
        self.engine.checkpoint_overlay().map_err(to_nfs)?;
        let removed_ids = self.ids.remove_path(&path)?;
        self.read_leases.evict_many(removed_ids);
        self.write_journal.remove_subtree(&path);
        Ok(())
    }

    async fn rename<'a>(
        &self,
        from_dirid: &Self::Handle,
        from_filename: &filename3<'a>,
        to_dirid: &Self::Handle,
        to_filename: &filename3<'a>,
    ) -> Result<(), nfsstat3> {
        if self.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let from_name = nfs_child_name(from_filename)?;
        let to_name = nfs_child_name(to_filename)?;
        ensure_mutable_child(from_dirid.as_u64(), from_name)?;
        ensure_mutable_child(to_dirid.as_u64(), to_name)?;
        let (_from_id, from_path) = self.lookup_path(from_dirid.as_u64(), from_name)?;
        let to_parent = self.directory_path_for_child(to_dirid.as_u64())?;
        let to_path = join_path(&to_parent, to_name);
        self.engine
            .rename(&from_path, &to_path)
            .await
            .map_err(to_nfs)?;
        self.invalidate_parent_directory_page(&from_path);
        self.invalidate_parent_directory_page(&to_path);
        self.directory_pages.invalidate_rename(&from_path, &to_path);
        self.engine.checkpoint_overlay().map_err(to_nfs)?;
        let renamed_ids = self.ids.rename_path(&from_path, &to_path)?;
        self.read_leases
            .evict_many(renamed_ids.moved.into_iter().chain(renamed_ids.replaced));
        self.write_journal.rename_subtree(&from_path, &to_path);
        Ok(())
    }

    async fn symlink<'a>(
        &self,
        dirid: &Self::Handle,
        linkname: &filename3<'a>,
        symlink: &nfspath3<'a>,
        attr: &sattr3,
    ) -> Result<(Self::Handle, fattr3), nfsstat3> {
        if self.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let name = nfs_child_name(linkname)?;
        ensure_mutable_child(dirid.as_u64(), name)?;
        let target = nfs_symlink_target(symlink)?;
        let parent = self.directory_path_for_child(dirid.as_u64())?;
        let path = join_path(&parent, name);
        ensure_path_absent(self.resolver.resolve_path(&path))?;
        ensure_setattr_supported(attr)?;
        validate_setattr_target(NodeType::Symlink, attr)?;
        let mode = match attr.mode {
            nfs::set_mode3::Some(mode) => mode & 0o7777,
            nfs::set_mode3::None => 0o777,
        };
        self.engine
            .create_symlink(&path, target, mode)
            .await
            .map_err(to_nfs)?;
        self.directory_pages.invalidate_path(&parent);
        self.setattr_path(&path, attr.clone()).await?;
        let id = self.ids.id_for_path(&path, NodeType::Symlink)?;
        let attr = self.attr_for_path(id, &path)?;
        Ok((FileHandleU64::new(id), attr))
    }

    async fn commit(&self, id: &Self::Handle, _offset: u64, _count: u32) -> Result<(), nfsstat3> {
        let id = id.as_u64();
        if id == ROOT_ID {
            return Err(nfsstat3::NFS3ERR_INVAL);
        }
        if id == GITFILE_ID {
            return Ok(());
        }
        let path = self.ids.path(id)?;
        let (_mode, _size, node_type, _mtime) = self.resolver.getattr(&path).map_err(to_nfs)?;
        ensure_regular_file_for_write(node_type)?;
        self.sync_journal_path(&path)
    }
}

/// Tracks NFS writes that have not yet reached stable local overlay storage.
pub struct NfsWriteJournal {
    entries: Mutex<HashMap<String, NfsWriteJournalEntry>>,
    sync_stats: Mutex<NfsWriteJournalSyncStats>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NfsWriteJournalEntry {
    path: String,
    overlay_version: u64,
    last_write_stability: NfsWriteStability,
    dirty_since: SystemTime,
    last_sync_error: Option<nfsstat3>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct NfsWriteJournalSyncStats {
    attempts: u64,
    successes: u64,
    failures: u64,
    total_latency_ms: u64,
    last_latency_ms: Option<u64>,
    max_latency_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NfsWriteStability {
    Unstable,
    DataSync,
    FileSync,
}

impl From<stable_how> for NfsWriteStability {
    fn from(stability: stable_how) -> Self {
        match stability {
            stable_how::UNSTABLE => Self::Unstable,
            stable_how::DATA_SYNC => Self::DataSync,
            stable_how::FILE_SYNC => Self::FileSync,
        }
    }
}

impl NfsWriteJournal {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            sync_stats: Mutex::new(NfsWriteJournalSyncStats::default()),
        }
    }

    fn mark_write(&self, path: &str, stability: NfsWriteStability, overlay_version: u64) {
        match self.entries.lock() {
            Ok(mut entries) => {
                entries.insert(
                    path.to_owned(),
                    NfsWriteJournalEntry {
                        path: path.to_owned(),
                        overlay_version,
                        last_write_stability: stability,
                        dirty_since: SystemTime::now(),
                        last_sync_error: None,
                    },
                );
            }
            Err(error) => {
                warn!(error = %error, path, "failed to mark NFS write journal entry");
            }
        }
    }

    fn mark_synced(&self, path: &str) {
        match self.entries.lock() {
            Ok(mut entries) => {
                entries.remove(path);
            }
            Err(error) => {
                warn!(error = %error, path, "failed to clear NFS write journal entry");
            }
        }
    }

    fn record_sync_error(&self, path: &str, status: nfsstat3) {
        match self.entries.lock() {
            Ok(mut entries) => {
                if let Some(entry) = entries.get_mut(path) {
                    entry.last_sync_error = Some(status);
                }
            }
            Err(error) => {
                warn!(error = %error, path, "failed to record NFS write journal sync error");
            }
        }
    }

    fn record_sync_result(&self, latency_ms: u64, success: bool) {
        match self.sync_stats.lock() {
            Ok(mut stats) => {
                stats.attempts = stats.attempts.saturating_add(1);
                if success {
                    stats.successes = stats.successes.saturating_add(1);
                } else {
                    stats.failures = stats.failures.saturating_add(1);
                }
                stats.total_latency_ms = stats.total_latency_ms.saturating_add(latency_ms);
                stats.last_latency_ms = Some(latency_ms);
                stats.max_latency_ms = Some(
                    stats
                        .max_latency_ms
                        .map_or(latency_ms, |current| current.max(latency_ms)),
                );
            }
            Err(error) => {
                warn!(error = %error, "failed to record NFS write journal sync latency");
            }
        }
    }

    fn sync_stats_snapshot(&self) -> NfsWriteJournalSyncStats {
        match self.sync_stats.lock() {
            Ok(stats) => *stats,
            Err(error) => {
                warn!(error = %error, "failed to snapshot NFS write journal sync latency");
                NfsWriteJournalSyncStats::default()
            }
        }
    }

    fn sync_path(&self, engine: &VfsEngine, path: &str) -> crate::core::error::Result<()> {
        let started = Instant::now();
        let result = engine.sync_overlay_path(path);
        let latency_ms = duration_millis(started.elapsed());
        self.record_sync_result(latency_ms, result.is_ok());
        match result {
            Ok(()) => {
                self.mark_synced(path);
                Ok(())
            }
            Err(error) => {
                let status = nfs_status_for_error(&error);
                self.record_sync_error(path, status);
                Err(error)
            }
        }
    }

    fn remove_subtree(&self, path: &str) {
        match self.entries.lock() {
            Ok(mut entries) => {
                remove_journal_subtree(&mut entries, path);
            }
            Err(error) => {
                warn!(error = %error, path, "failed to remove NFS write journal subtree");
            }
        }
    }

    fn rename_subtree(&self, old_path: &str, new_path: &str) {
        match self.entries.lock() {
            Ok(mut entries) => {
                rename_journal_subtree(&mut entries, old_path, new_path);
            }
            Err(error) => {
                warn!(
                    error = %error,
                    old_path,
                    new_path,
                    "failed to rename NFS write journal subtree"
                );
            }
        }
    }

    /// Sync and clear every path with deferred unstable writes.
    pub fn sync_all(&self, engine: &VfsEngine) -> crate::core::error::Result<()> {
        let mut failures = Vec::new();
        for entry in self.pending()? {
            match self.sync_path(engine, &entry.path) {
                Ok(()) => {}
                Err(error) => {
                    failures.push((entry.path, error.to_string()));
                }
            }
        }
        if failures.is_empty() {
            return Ok(());
        }
        let (path, error) = &failures[0];
        Err(CrabError::Internal(format!(
            "NFS write journal sync failed for {} path(s); first failure at {path}: {error}",
            failures.len()
        )))
    }

    fn pending(&self) -> crate::core::error::Result<Vec<NfsWriteJournalEntry>> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| CrabError::Internal("NFS write journal mutex was poisoned".into()))?;
        let mut pending = entries.values().cloned().collect::<Vec<_>>();
        pending.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(pending)
    }

    pub fn snapshot(&self) -> NfsWriteJournalSnapshot {
        let sync_stats = self.sync_stats_snapshot();
        match self.pending() {
            Ok(pending) => NfsWriteJournalSnapshot::from_pending(pending, sync_stats),
            Err(error) => {
                warn!(error = %error, "failed to snapshot NFS write journal");
                NfsWriteJournalSnapshot {
                    pending_paths: 0,
                    oldest_dirty_age_secs: None,
                    paths_with_sync_errors: 0,
                    sync_attempts: sync_stats.attempts,
                    sync_successes: sync_stats.successes,
                    sync_failures: sync_stats.failures,
                    total_sync_latency_ms: sync_stats.total_latency_ms,
                    last_sync_latency_ms: sync_stats.last_latency_ms,
                    max_sync_latency_ms: sync_stats.max_latency_ms,
                    entries: Vec::new(),
                    poisoned: true,
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NfsWriteJournalSnapshot {
    pub pending_paths: usize,
    pub oldest_dirty_age_secs: Option<u64>,
    pub paths_with_sync_errors: usize,
    pub sync_attempts: u64,
    pub sync_successes: u64,
    pub sync_failures: u64,
    pub total_sync_latency_ms: u64,
    pub last_sync_latency_ms: Option<u64>,
    pub max_sync_latency_ms: Option<u64>,
    pub entries: Vec<NfsWriteJournalPathSnapshot>,
    pub poisoned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NfsWriteJournalPathSnapshot {
    pub path: String,
    pub overlay_version: u64,
    pub last_write_stability: NfsWriteStability,
    pub dirty_age_secs: Option<u64>,
    pub last_sync_error: Option<nfsstat3>,
}

impl NfsWriteJournalSnapshot {
    fn from_pending(
        pending: Vec<NfsWriteJournalEntry>,
        sync_stats: NfsWriteJournalSyncStats,
    ) -> Self {
        let entries = pending
            .into_iter()
            .map(|entry| NfsWriteJournalPathSnapshot {
                path: entry.path,
                overlay_version: entry.overlay_version,
                last_write_stability: entry.last_write_stability,
                dirty_age_secs: dirty_age_secs(entry.dirty_since),
                last_sync_error: entry.last_sync_error,
            })
            .collect::<Vec<_>>();
        let oldest_dirty_age_secs = entries
            .iter()
            .filter_map(|entry| entry.dirty_age_secs)
            .max();
        let paths_with_sync_errors = entries
            .iter()
            .filter(|entry| entry.last_sync_error.is_some())
            .count();
        Self {
            pending_paths: entries.len(),
            oldest_dirty_age_secs,
            paths_with_sync_errors,
            sync_attempts: sync_stats.attempts,
            sync_successes: sync_stats.successes,
            sync_failures: sync_stats.failures,
            total_sync_latency_ms: sync_stats.total_latency_ms,
            last_sync_latency_ms: sync_stats.last_latency_ms,
            max_sync_latency_ms: sync_stats.max_latency_ms,
            entries,
            poisoned: false,
        }
    }
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn dirty_age_secs(dirty_since: SystemTime) -> Option<u64> {
    SystemTime::now()
        .duration_since(dirty_since)
        .ok()
        .map(|duration| duration.as_secs())
}

fn remove_journal_subtree(entries: &mut HashMap<String, NfsWriteJournalEntry>, path: &str) {
    let prefix = format!("{path}/");
    entries.retain(|entry_path, _| entry_path != path && !entry_path.starts_with(&prefix));
}

fn rename_journal_subtree(
    entries: &mut HashMap<String, NfsWriteJournalEntry>,
    old_path: &str,
    new_path: &str,
) {
    let old_prefix = format!("{old_path}/");
    let new_prefix = format!("{new_path}/");
    let moved = entries
        .iter()
        .filter_map(|(entry_path, entry)| {
            if entry_path == old_path {
                Some((entry_path.clone(), new_path.to_owned(), entry.clone()))
            } else {
                entry_path.strip_prefix(&old_prefix).map(|suffix| {
                    (
                        entry_path.clone(),
                        format!("{new_path}/{suffix}"),
                        entry.clone(),
                    )
                })
            }
        })
        .collect::<Vec<_>>();

    entries.retain(|entry_path, _| {
        entry_path != old_path
            && !entry_path.starts_with(&old_prefix)
            && entry_path != new_path
            && !entry_path.starts_with(&new_prefix)
    });
    for (_old_entry_path, moved_path, mut entry) in moved {
        entry.path = moved_path.clone();
        entries.insert(moved_path, entry);
    }
}

struct NfsIdTable {
    state: RwLock<NfsIdState>,
    exclusive_verifiers_path: Option<PathBuf>,
}

#[derive(Clone)]
struct NfsIdState {
    by_id: HashMap<u64, NfsNodeRef>,
    by_path: HashMap<String, u64>,
    exclusive_verifiers: HashMap<String, nfs::createverf3>,
    next_id: u64,
}

#[derive(Clone)]
struct NfsNodeRef {
    path: String,
    node_type: NodeType,
}

struct NfsRenameIds {
    moved: Vec<u64>,
    replaced: Vec<u64>,
}

impl NfsIdTable {
    fn new(exclusive_verifiers_path: Option<PathBuf>) -> Self {
        let mut by_id = HashMap::new();
        let mut by_path = HashMap::new();
        by_id.insert(
            ROOT_ID,
            NfsNodeRef {
                path: String::new(),
                node_type: NodeType::Dir,
            },
        );
        by_path.insert(String::new(), ROOT_ID);
        by_id.insert(
            GITFILE_ID,
            NfsNodeRef {
                path: ".git".to_owned(),
                node_type: NodeType::File,
            },
        );
        by_path.insert(".git".to_owned(), GITFILE_ID);
        let exclusive_verifiers = exclusive_verifiers_path
            .as_deref()
            .map_or_else(HashMap::new, load_exclusive_verifiers);
        Self {
            state: RwLock::new(NfsIdState {
                by_id,
                by_path,
                exclusive_verifiers,
                next_id: FIRST_DYNAMIC_ID,
            }),
            exclusive_verifiers_path,
        }
    }

    fn path(&self, id: u64) -> Result<String, nfsstat3> {
        let state = self.state.read().map_err(|_| nfsstat3::NFS3ERR_IO)?;
        state
            .by_id
            .get(&id)
            .map(|node| node.path.clone())
            .ok_or(nfsstat3::NFS3ERR_STALE)
    }

    fn id_for_path(&self, path: &str, node_type: NodeType) -> Result<u64, nfsstat3> {
        let mut state = self.state.write().map_err(|_| nfsstat3::NFS3ERR_IO)?;
        if let Some(id) = state.by_path.get(path).copied() {
            if let Some(node) = state.by_id.get_mut(&id) {
                node.node_type = node_type;
            }
            return Ok(id);
        }
        let id = state.next_id;
        state.next_id = state.next_id.checked_add(1).ok_or(nfsstat3::NFS3ERR_IO)?;
        state.by_path.insert(path.to_owned(), id);
        state.by_id.insert(
            id,
            NfsNodeRef {
                path: path.to_owned(),
                node_type,
            },
        );
        Ok(id)
    }

    fn remove_path(&self, path: &str) -> Result<Vec<u64>, nfsstat3> {
        let mut state = self.state.write().map_err(|_| nfsstat3::NFS3ERR_IO)?;
        let mut next_state = state.clone();
        let removed = Self::remove_path_locked(&mut next_state, path);
        self.persist_exclusive_verifiers(&next_state)?;
        *state = next_state;
        Ok(removed)
    }

    fn record_exclusive_create(
        &self,
        path: &str,
        verifier: nfs::createverf3,
    ) -> Result<(), nfsstat3> {
        let mut state = self.state.write().map_err(|_| nfsstat3::NFS3ERR_IO)?;
        let mut next_state = state.clone();
        next_state
            .exclusive_verifiers
            .insert(path.to_owned(), verifier);
        self.persist_exclusive_verifiers(&next_state)?;
        *state = next_state;
        Ok(())
    }

    fn remove_exclusive_verifier(&self, path: &str) -> Result<(), nfsstat3> {
        let mut state = self.state.write().map_err(|_| nfsstat3::NFS3ERR_IO)?;
        let mut next_state = state.clone();
        next_state.exclusive_verifiers.remove(path);
        self.persist_exclusive_verifiers(&next_state)?;
        *state = next_state;
        Ok(())
    }

    fn exclusive_verifier(&self, path: &str) -> Result<Option<nfs::createverf3>, nfsstat3> {
        let state = self.state.read().map_err(|_| nfsstat3::NFS3ERR_IO)?;
        Ok(state.exclusive_verifiers.get(path).copied())
    }

    fn remove_path_locked(state: &mut NfsIdState, path: &str) -> Vec<u64> {
        let prefix = format!("{path}/");
        let mut ids = state
            .by_id
            .iter()
            .filter_map(|(id, node)| {
                if node.path == path || node.path.starts_with(&prefix) {
                    Some(*id)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        ids.sort_unstable();
        for id in &ids {
            if *id == ROOT_ID || *id == GITFILE_ID {
                continue;
            }
            if let Some(node) = state.by_id.remove(id) {
                state.by_path.remove(&node.path);
            }
        }
        state.exclusive_verifiers.retain(|verifier_path, _| {
            verifier_path != path && !verifier_path.starts_with(&prefix)
        });
        ids.into_iter()
            .filter(|id| *id != ROOT_ID && *id != GITFILE_ID)
            .collect()
    }

    fn rename_path(&self, old_path: &str, new_path: &str) -> Result<NfsRenameIds, nfsstat3> {
        let mut state = self.state.write().map_err(|_| nfsstat3::NFS3ERR_IO)?;
        let mut next_state = state.clone();
        let replaced = Self::remove_path_locked(&mut next_state, new_path);

        let old_prefix = format!("{old_path}/");
        let updates = next_state
            .by_id
            .iter()
            .filter_map(|(id, node)| {
                if node.path == old_path {
                    Some((*id, new_path.to_owned()))
                } else {
                    node.path
                        .strip_prefix(&old_prefix)
                        .map(|suffix| (*id, format!("{new_path}/{suffix}")))
                }
            })
            .collect::<Vec<_>>();
        let moved = updates.iter().map(|(id, _)| *id).collect::<Vec<_>>();
        let verifier_updates = next_state
            .exclusive_verifiers
            .iter()
            .filter_map(|(verifier_path, verifier)| {
                if verifier_path == old_path {
                    Some((verifier_path.clone(), new_path.to_owned(), *verifier))
                } else {
                    verifier_path.strip_prefix(&old_prefix).map(|suffix| {
                        (
                            verifier_path.clone(),
                            format!("{new_path}/{suffix}"),
                            *verifier,
                        )
                    })
                }
            })
            .collect::<Vec<_>>();

        for (id, moved_path) in updates {
            let Some(old_path) = next_state
                .by_id
                .get_mut(&id)
                .map(|node| std::mem::replace(&mut node.path, moved_path.clone()))
            else {
                continue;
            };
            next_state.by_path.remove(&old_path);
            next_state.by_path.insert(moved_path, id);
        }
        for (old_verifier_path, new_verifier_path, verifier) in verifier_updates {
            next_state.exclusive_verifiers.remove(&old_verifier_path);
            next_state
                .exclusive_verifiers
                .insert(new_verifier_path, verifier);
        }
        self.persist_exclusive_verifiers(&next_state)?;
        *state = next_state;
        Ok(NfsRenameIds { moved, replaced })
    }

    fn persist_exclusive_verifiers(&self, state: &NfsIdState) -> Result<(), nfsstat3> {
        let Some(path) = &self.exclusive_verifiers_path else {
            return Ok(());
        };
        persist_exclusive_verifiers(path, &state.exclusive_verifiers)
    }
}

fn load_exclusive_verifiers(path: &Path) -> HashMap<String, nfs::createverf3> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(error) => {
            warn!(
                path = %path.display(),
                error = %error,
                "failed to read NFS exclusive create verifier store"
            );
            return HashMap::new();
        }
    };

    let encoded = match serde_json::from_str::<BTreeMap<String, String>>(&content) {
        Ok(encoded) => encoded,
        Err(error) => {
            warn!(
                path = %path.display(),
                error = %error,
                "failed to parse NFS exclusive create verifier store"
            );
            return HashMap::new();
        }
    };

    encoded
        .into_iter()
        .filter_map(|(path, verifier)| {
            verifier_from_hex(&verifier).map(|verifier| (path, verifier))
        })
        .collect()
}

fn persist_exclusive_verifiers(
    path: &Path,
    verifiers: &HashMap<String, nfs::createverf3>,
) -> Result<(), nfsstat3> {
    let Some(parent) = path.parent() else {
        return Err(nfsstat3::NFS3ERR_IO);
    };
    std::fs::create_dir_all(parent).map_err(|error| {
        warn!(
            path = %parent.display(),
            error = %error,
            "failed to create NFS exclusive create verifier directory"
        );
        nfsstat3::NFS3ERR_IO
    })?;

    let encoded = verifiers
        .iter()
        .map(|(path, verifier)| (path.clone(), verifier_to_hex(*verifier)))
        .collect::<BTreeMap<_, _>>();
    let json = serde_json::to_vec_pretty(&encoded).map_err(|error| {
        warn!(error = %error, "failed to encode NFS exclusive create verifiers");
        nfsstat3::NFS3ERR_IO
    })?;

    let tmp_path = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = std::fs::File::create(&tmp_path).map_err(|error| {
        warn!(
            path = %tmp_path.display(),
            error = %error,
            "failed to create NFS exclusive create verifier temp file"
        );
        nfsstat3::NFS3ERR_IO
    })?;
    file.write_all(&json).map_err(|error| {
        warn!(
            path = %tmp_path.display(),
            error = %error,
            "failed to write NFS exclusive create verifier temp file"
        );
        nfsstat3::NFS3ERR_IO
    })?;
    file.sync_all().map_err(|error| {
        warn!(
            path = %tmp_path.display(),
            error = %error,
            "failed to sync NFS exclusive create verifier temp file"
        );
        nfsstat3::NFS3ERR_IO
    })?;
    drop(file);

    std::fs::rename(&tmp_path, path).map_err(|error| {
        warn!(
            from = %tmp_path.display(),
            to = %path.display(),
            error = %error,
            "failed to replace NFS exclusive create verifier store"
        );
        nfsstat3::NFS3ERR_IO
    })?;
    sync_verifier_parent_dir(parent).map_err(|error| {
        warn!(
            path = %parent.display(),
            error = %error,
            "failed to sync NFS exclusive create verifier directory"
        );
        nfsstat3::NFS3ERR_IO
    })
}

#[cfg(unix)]
fn sync_verifier_parent_dir(parent: &Path) -> std::io::Result<()> {
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_verifier_parent_dir(_parent: &Path) -> std::io::Result<()> {
    Ok(())
}

fn verifier_to_hex(verifier: nfs::createverf3) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(verifier.0.len() * 2);
    for byte in verifier.0 {
        encoded.push(HEX[usize::from(byte >> 4)] as char);
        encoded.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

fn verifier_from_hex(encoded: &str) -> Option<nfs::createverf3> {
    let bytes = encoded.as_bytes();
    if bytes.len() != 16 {
        return None;
    }
    let mut verifier = [0u8; 8];
    for index in 0..8 {
        let high = decode_hex_nibble(bytes[index * 2])?;
        let low = decode_hex_nibble(bytes[index * 2 + 1])?;
        verifier[index] = (high << 4) | low;
    }
    Some(nfs::createverf3(verifier))
}

fn decode_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[derive(Clone)]
struct NfsDirectoryCandidate {
    id: u64,
    name: String,
    path: Option<String>,
    node_type: NodeType,
    attr: Option<fattr3>,
}

struct CrabNfsDirIter {
    loader: NfsDirectoryPageLoader,
    dirid: u64,
    path: String,
    candidates: Arc<Vec<NfsDirectoryCandidate>>,
    index: usize,
    after_name: Option<String>,
    materialized_entries: usize,
    large_directory_recorded: bool,
}

struct NfsDirectoryPageLoader {
    resolver: Arc<FuseResolver>,
    engine: Arc<VfsEngine>,
    ids: Arc<NfsIdTable>,
    directory_pages: Arc<NfsDirectoryPageCache>,
    protocol_stats: Arc<NfsProtocolStats>,
    gitfile_attr: fattr3,
    uid: u32,
    gid: u32,
}

impl NfsDirectoryPageLoader {
    fn load_candidates(
        &self,
        dirid: u64,
        path: &str,
        after_name: Option<&str>,
        include_virtual_git: bool,
    ) -> Result<Arc<Vec<NfsDirectoryCandidate>>, nfsstat3> {
        let page_key = self.directory_pages.page_key(
            path,
            self.resolver.generation(),
            after_name,
            include_virtual_git,
        );
        let candidates = if let Some(candidates) = self.directory_pages.get(&page_key) {
            candidates
        } else {
            let mut candidates = Vec::with_capacity(NFS_DIRECTORY_PAGE_ENTRIES);
            if dirid == ROOT_ID && include_virtual_git {
                candidates.push(NfsDirectoryCandidate {
                    id: GITFILE_ID,
                    name: ".git".to_owned(),
                    path: None,
                    node_type: NodeType::File,
                    attr: Some(self.gitfile_attr.clone()),
                });
            }
            let remaining = NFS_DIRECTORY_PAGE_ENTRIES.saturating_sub(candidates.len());
            for entry in self
                .resolver
                .readdir_page(path, after_name, remaining)
                .map_err(to_nfs)?
            {
                let child_path = join_path(path, &entry.name);
                let id = self.ids.id_for_path(&child_path, entry.node_type)?;
                candidates.push(NfsDirectoryCandidate {
                    id,
                    name: entry.name,
                    path: Some(child_path),
                    node_type: entry.node_type,
                    attr: None,
                });
            }
            let candidates = Arc::new(candidates);
            self.directory_pages
                .insert(page_key, Arc::clone(&candidates));
            candidates
        };

        let prefetch_paths = candidates
            .iter()
            .filter_map(|candidate| {
                file_prefetch_path(candidate.path.as_deref(), candidate.node_type)
            })
            .collect::<Vec<_>>();
        let prefetch_error = if prefetch_paths.is_empty() {
            false
        } else if let Err(error) = self.engine.prefetch_dir(&prefetch_paths) {
            debug!(error = %error, "NFS directory page prefetch failed");
            true
        } else {
            false
        };
        self.protocol_stats.record_readdirplus_page(
            candidates.len(),
            prefetch_paths.len(),
            prefetch_error,
        );
        Ok(candidates)
    }

    fn attr_for_candidate(
        &self,
        candidate: &NfsDirectoryCandidate,
    ) -> Result<Option<fattr3>, nfsstat3> {
        if let Some(attr) = candidate.attr.clone() {
            return Ok(Some(attr));
        }
        let path = candidate.path.as_deref().ok_or(nfsstat3::NFS3ERR_IO)?;
        let node = self.resolver.resolve_path(path).map_err(to_nfs)?;
        if matches!(
            node,
            ResolvedNode::Base(ref base)
                if base.node_type == NodeType::File
                    && base.size == 0
                    && base.object_oid.is_some()
        ) {
            return Ok(None);
        }
        let (mode, size, node_type, mtime) = self.resolver.getattr(path).map_err(to_nfs)?;
        Ok(Some(make_nfs_attr(
            self.uid,
            self.gid,
            candidate.id,
            mode,
            size,
            node_type,
            mtime,
        )))
    }
}

impl ReadDirPlusIterator<FileHandleU64> for CrabNfsDirIter {
    async fn next(&mut self) -> NextResult<DirEntryPlus<FileHandleU64>> {
        loop {
            if let Some(candidate) = self.candidates.get(self.index) {
                self.index += 1;
                let attr = match self.loader.attr_for_candidate(candidate) {
                    Ok(attr) => attr,
                    Err(error) => return NextResult::Err(error),
                };
                if candidate.path.is_some() {
                    self.after_name = Some(candidate.name.clone());
                }
                self.loader
                    .protocol_stats
                    .record_readdirplus_entry(candidate.attr.is_none());
                return NextResult::Ok(DirEntryPlus {
                    fileid: candidate.id,
                    name: candidate.name.clone().into_bytes().into(),
                    cookie: candidate.id,
                    name_attributes: attr,
                    name_handle: Some(FileHandleU64::new(candidate.id)),
                });
            }

            if self.candidates.len() < NFS_DIRECTORY_PAGE_ENTRIES {
                return NextResult::Eof;
            }
            let next = match self.loader.load_candidates(
                self.dirid,
                &self.path,
                self.after_name.as_deref(),
                false,
            ) {
                Ok(next) => next,
                Err(error) => return NextResult::Err(error),
            };
            if next.is_empty() {
                return NextResult::Eof;
            }
            self.materialized_entries = self.materialized_entries.saturating_add(next.len());
            if !self.large_directory_recorded
                && self.materialized_entries >= NFS_LARGE_READDIRPLUS_ENTRY_THRESHOLD
            {
                self.loader.protocol_stats.record_large_readdirplus();
                self.large_directory_recorded = true;
            }
            self.candidates = next;
            self.index = 0;
        }
    }
}

fn readdirplus_cursor(
    ids: &NfsIdTable,
    dirid: u64,
    directory_path: &str,
    cookie: u64,
) -> (Option<String>, bool, bool) {
    if cookie == 0 {
        return (None, dirid == ROOT_ID, false);
    }
    if dirid == ROOT_ID && cookie == GITFILE_ID {
        return (None, false, false);
    }
    let Ok(path) = ids.path(cookie) else {
        return (None, dirid == ROOT_ID, true);
    };
    let (parent, name) = path
        .rsplit_once('/')
        .map_or(("", path.as_str()), |parts| parts);
    if parent != directory_path || name.is_empty() {
        return (None, dirid == ROOT_ID, true);
    }
    (Some(name.to_owned()), false, false)
}

fn make_nfs_attr(
    uid: u32,
    gid: u32,
    id: u64,
    mode: u32,
    size: u64,
    node_type: NodeType,
    mtime: i64,
) -> fattr3 {
    let time = unix_to_nfstime(mtime);
    fattr3 {
        type_: node_type_to_nfs(node_type),
        mode: mode & 0o7777,
        nlink: if node_type == NodeType::Dir { 2 } else { 1 },
        uid,
        gid,
        size,
        used: size.div_ceil(512) * 512,
        rdev: specdata3::default(),
        fsid: 0,
        fileid: id,
        atime: time,
        mtime: time,
        ctime: time,
    }
}

fn nfs_name<'a>(filename: &'a filename3<'_>) -> Result<&'a str, nfsstat3> {
    let bytes = filename.as_ref();
    if bytes.len() > NFS_MAX_COMPONENT_BYTES {
        return Err(nfsstat3::NFS3ERR_NAMETOOLONG);
    }
    if bytes.contains(&0) {
        return Err(nfsstat3::NFS3ERR_INVAL);
    }
    let name = std::str::from_utf8(bytes).map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
    if name.is_empty() || name.contains('/') {
        return Err(nfsstat3::NFS3ERR_INVAL);
    }
    Ok(name)
}

fn nfs_child_name<'a>(filename: &'a filename3<'_>) -> Result<&'a str, nfsstat3> {
    let name = nfs_name(filename)?;
    if name == "." || name == ".." {
        return Err(nfsstat3::NFS3ERR_INVAL);
    }
    Ok(name)
}

fn nfs_symlink_target<'a>(symlink: &'a nfspath3<'_>) -> Result<&'a str, nfsstat3> {
    let bytes = symlink.as_ref();
    if bytes.len() > NFS_MAX_PATH_BYTES {
        return Err(nfsstat3::NFS3ERR_NAMETOOLONG);
    }
    if bytes.is_empty() || bytes.contains(&0) {
        return Err(nfsstat3::NFS3ERR_INVAL);
    }
    std::str::from_utf8(bytes).map_err(|_| nfsstat3::NFS3ERR_INVAL)
}

fn ensure_mutable_child(parent_id: u64, name: &str) -> Result<(), nfsstat3> {
    if parent_id == ROOT_ID && name == ".git" {
        return Err(nfsstat3::NFS3ERR_ROFS);
    }
    Ok(())
}

fn ensure_path_absent(
    result: std::result::Result<ResolvedNode, CrabError>,
) -> Result<(), nfsstat3> {
    match result {
        Ok(_) => Err(nfsstat3::NFS3ERR_EXIST),
        Err(CrabError::NotFound { .. }) => Ok(()),
        Err(error) => Err(to_nfs(error)),
    }
}

fn ensure_regular_file_for_read(node_type: NodeType) -> Result<(), nfsstat3> {
    match node_type {
        NodeType::File => Ok(()),
        NodeType::Dir => Err(nfsstat3::NFS3ERR_ISDIR),
        NodeType::Symlink => Err(nfsstat3::NFS3ERR_INVAL),
    }
}

fn ensure_regular_file_for_write(node_type: NodeType) -> Result<(), nfsstat3> {
    match node_type {
        NodeType::File => Ok(()),
        NodeType::Dir | NodeType::Symlink => Err(nfsstat3::NFS3ERR_INVAL),
    }
}

fn validate_setattr_target(node_type: NodeType, attr: &sattr3) -> Result<(), nfsstat3> {
    if attr.size.is_some() && node_type != NodeType::File {
        return Err(nfsstat3::NFS3ERR_ISDIR);
    }
    Ok(())
}

fn ensure_setattr_supported(attr: &sattr3) -> Result<(), nfsstat3> {
    if attr.uid.is_some() || attr.gid.is_some() {
        return Err(nfsstat3::NFS3ERR_ROFS);
    }
    Ok(())
}

fn join_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

fn parent_path(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

fn path_is_at_or_under(path: &str, subtree: &str) -> bool {
    if subtree.is_empty() || path == subtree {
        return true;
    }
    path.strip_prefix(subtree)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

fn path_and_ancestors(path: &str) -> Vec<&str> {
    let mut ancestors = vec![path];
    let mut current = path;
    while let Some((parent, _name)) = current.rsplit_once('/') {
        ancestors.push(parent);
        current = parent;
    }
    if !path.is_empty() {
        ancestors.push("");
    }
    ancestors
}

fn file_prefetch_path(path: Option<&str>, node_type: NodeType) -> Option<String> {
    if node_type != NodeType::File {
        return None;
    }
    path.map(str::to_owned)
}

fn read_reached_eof(
    offset: u64,
    returned_len: usize,
    requested_count: u32,
    known_size: Option<u64>,
) -> bool {
    if returned_len < requested_count as usize {
        return true;
    }
    let Some(size) = known_size else {
        return false;
    };
    let returned_len = u64::try_from(returned_len).unwrap_or(u64::MAX);
    offset.saturating_add(returned_len) >= size
}

fn node_type_to_nfs(node_type: NodeType) -> ftype3 {
    match node_type {
        NodeType::File => ftype3::NF3REG,
        NodeType::Dir => ftype3::NF3DIR,
        NodeType::Symlink => ftype3::NF3LNK,
    }
}

fn unix_to_nfstime(secs: i64) -> nfstime3 {
    nfstime3 {
        seconds: u32::try_from(secs.max(0)).unwrap_or(u32::MAX),
        nseconds: 0,
    }
}

fn nfstime_to_nanos(time: nfstime3) -> i64 {
    i64::from(time.seconds)
        .saturating_mul(1_000_000_000)
        .saturating_add(i64::from(time.nseconds))
}

fn now_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX)
        })
}

fn to_nfs(error: CrabError) -> nfsstat3 {
    let status = nfs_status_for_error(&error);
    if should_log_nfs_error(&error) {
        warn!(error = %error, "NFS adapter operation failed");
    }
    status
}

fn should_log_nfs_error(error: &CrabError) -> bool {
    !matches!(
        error,
        CrabError::NotFound { .. } | CrabError::Forbidden { .. } | CrabError::Io(_)
    )
}

fn nfs_status_for_error(error: &CrabError) -> nfsstat3 {
    match error {
        CrabError::NotFound { .. } => nfsstat3::NFS3ERR_NOENT,
        CrabError::Forbidden { path } if path.starts_with("directory not empty:") => {
            nfsstat3::NFS3ERR_NOTEMPTY
        }
        CrabError::Forbidden { .. } => nfsstat3::NFS3ERR_ROFS,
        CrabError::Io(error) => io_error_to_nfs(error),
        _ => nfsstat3::NFS3ERR_IO,
    }
}

fn io_error_to_nfs(error: &std::io::Error) -> nfsstat3 {
    #[cfg(unix)]
    if let Some(errno) = error.raw_os_error() {
        match errno {
            libc::ENOENT => return nfsstat3::NFS3ERR_NOENT,
            libc::EIO => return nfsstat3::NFS3ERR_IO,
            libc::EACCES | libc::EPERM => return nfsstat3::NFS3ERR_ACCES,
            libc::EEXIST => return nfsstat3::NFS3ERR_EXIST,
            libc::ENOTDIR => return nfsstat3::NFS3ERR_NOTDIR,
            libc::EISDIR => return nfsstat3::NFS3ERR_ISDIR,
            libc::EINVAL => return nfsstat3::NFS3ERR_INVAL,
            libc::EROFS => return nfsstat3::NFS3ERR_ROFS,
            libc::ENOTEMPTY => return nfsstat3::NFS3ERR_NOTEMPTY,
            libc::ENOSPC => return nfsstat3::NFS3ERR_NOSPC,
            libc::EBADF => return nfsstat3::NFS3ERR_STALE,
            _ => {}
        }
    }

    match error.kind() {
        std::io::ErrorKind::NotFound => nfsstat3::NFS3ERR_NOENT,
        std::io::ErrorKind::PermissionDenied => nfsstat3::NFS3ERR_ACCES,
        std::io::ErrorKind::AlreadyExists => nfsstat3::NFS3ERR_EXIST,
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData => {
            nfsstat3::NFS3ERR_INVAL
        }
        _ => nfsstat3::NFS3ERR_IO,
    }
}

fn current_ids() -> (u32, u32) {
    #[cfg(unix)]
    {
        // SAFETY: getuid/getgid are side-effect-free libc calls.
        (unsafe { libc::getuid() }, unsafe { libc::getgid() })
    }
    #[cfg(not(unix))]
    {
        (0, 0)
    }
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used)]

    use super::*;
    use crate::ChunkCache;
    use crate::data_plane::{NoopFileIndexResolver, NoopShardLoader, NoopXorbFetcher};
    use crate::engine::{BaseRenameEntry, OverlayWriter};
    use crate::hydration::HydrationService;
    use crate::overlay::OverlayStore;
    use crate::resolver::{OverlayEntry, OverlayLookup};
    use crate::snapshot::{BaseNode, SnapshotStore};
    use crate::verified_set::VerifiedSet;
    use tokio_util::sync::CancellationToken;

    struct JournalSyncFixture {
        fs: CrabNfsFs,
        engine: Arc<VfsEngine>,
        overlay: Arc<SyncOnlyOverlay>,
        _root: tempfile::TempDir,
        _cache: tempfile::TempDir,
    }

    struct NfsReadFixture {
        fs: CrabNfsFs,
        engine: Arc<VfsEngine>,
        _root: tempfile::TempDir,
        _cache: tempfile::TempDir,
    }

    struct SyncOnlyOverlay {
        fail_path: Option<String>,
        synced_paths: Mutex<Vec<String>>,
        checkpoints: AtomicU64,
    }

    impl SyncOnlyOverlay {
        fn new(fail_path: Option<&str>) -> Self {
            Self {
                fail_path: fail_path.map(str::to_owned),
                synced_paths: Mutex::new(Vec::new()),
                checkpoints: AtomicU64::new(0),
            }
        }

        fn synced_paths(&self) -> Vec<String> {
            self.synced_paths
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        fn checkpoints(&self) -> u64 {
            self.checkpoints.load(Ordering::SeqCst)
        }
    }

    impl OverlayWriter for SyncOnlyOverlay {
        fn get(&self, _path: &str) -> Option<OverlayEntry> {
            None
        }

        fn get_backing_path(&self, _path: &str) -> Option<PathBuf> {
            None
        }

        fn create_file(&self, path: &str, _mode: u32) -> crate::core::error::Result<OverlayEntry> {
            Err(CrabError::Internal(format!(
                "test overlay cannot create {path}"
            )))
        }

        fn write_file(
            &self,
            path: &str,
            _offset: u64,
            _data: &[u8],
        ) -> crate::core::error::Result<usize> {
            Err(CrabError::Internal(format!(
                "test overlay cannot write {path}"
            )))
        }

        fn promote(
            &self,
            path: &str,
            _mode: u32,
            _content: &[u8],
            _source_oid: Option<&str>,
        ) -> crate::core::error::Result<OverlayEntry> {
            Err(CrabError::Internal(format!(
                "test overlay cannot promote {path}"
            )))
        }

        fn remove(&self, path: &str) -> crate::core::error::Result<()> {
            Err(CrabError::Internal(format!(
                "test overlay cannot remove {path}"
            )))
        }

        fn rename(&self, old_path: &str, new_path: &str) -> crate::core::error::Result<()> {
            Err(CrabError::Internal(format!(
                "test overlay cannot rename {old_path} to {new_path}"
            )))
        }

        fn rename_base_subtree(
            &self,
            _entries: &[BaseRenameEntry],
        ) -> crate::core::error::Result<()> {
            Err(CrabError::Internal(
                "test overlay cannot rename base subtree".into(),
            ))
        }

        fn mkdir(&self, path: &str, _mode: u32) -> crate::core::error::Result<()> {
            Err(CrabError::Internal(format!(
                "test overlay cannot mkdir {path}"
            )))
        }

        fn rmdir(&self, path: &str) -> crate::core::error::Result<()> {
            Err(CrabError::Internal(format!(
                "test overlay cannot rmdir {path}"
            )))
        }

        fn set_mtime(&self, path: &str, _mtime_ns: i64) -> crate::core::error::Result<()> {
            Err(CrabError::Internal(format!(
                "test overlay cannot set mtime for {path}"
            )))
        }

        fn set_mode(&self, path: &str, _mode: u32) -> crate::core::error::Result<()> {
            Err(CrabError::Internal(format!(
                "test overlay cannot set mode for {path}"
            )))
        }

        fn update_size_and_mtime(
            &self,
            path: &str,
            _size: u64,
            _mtime_ns: i64,
        ) -> crate::core::error::Result<()> {
            Err(CrabError::Internal(format!(
                "test overlay cannot update size and mtime for {path}"
            )))
        }

        fn promote_from_file(
            &self,
            path: &str,
            _mode: u32,
            _size: u64,
            _source_oid: Option<&str>,
        ) -> crate::core::error::Result<OverlayEntry> {
            Err(CrabError::Internal(format!(
                "test overlay cannot promote from file {path}"
            )))
        }

        fn backing_path_for(&self, path: &str) -> PathBuf {
            PathBuf::from(path)
        }

        fn backing_tmp_path_for(&self, path: &str) -> PathBuf {
            PathBuf::from(format!("{path}.tmp"))
        }

        fn create_symlink(
            &self,
            path: &str,
            _target: &str,
            _mode: u32,
        ) -> crate::core::error::Result<OverlayEntry> {
            Err(CrabError::Internal(format!(
                "test overlay cannot symlink {path}"
            )))
        }

        fn sync_path(&self, path: &str) -> crate::core::error::Result<()> {
            self.synced_paths
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(path.to_owned());
            if self.fail_path.as_deref() == Some(path) {
                return Err(CrabError::Internal(format!("sync failed for {path}")));
            }
            Ok(())
        }

        fn checkpoint(&self) -> crate::core::error::Result<()> {
            self.checkpoints.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn test_hydration(cache: &tempfile::TempDir) -> Arc<HydrationService> {
        HydrationService::new(
            Arc::new(ChunkCache::open(cache.path().join("chunks"), Some(1024 * 1024)).unwrap()),
            Arc::new(VerifiedSet::new(16)),
            Arc::new(NoopFileIndexResolver),
            Arc::new(NoopShardLoader),
            Arc::new(NoopXorbFetcher),
            None,
            None,
            Some(1),
            CancellationToken::new(),
        )
    }

    fn journal_sync_fixture(fail_path: Option<&str>) -> JournalSyncFixture {
        let root = tempfile::tempdir().unwrap();
        let snapshot =
            Arc::new(SnapshotStore::open_or_create(&root.path().join("snapshot.sqlite")).unwrap());
        let resolver = Arc::new(FuseResolver::new(Arc::clone(&snapshot), None, 0, 0));
        let cache = tempfile::tempdir().unwrap();
        let hydration = test_hydration(&cache);
        let overlay = Arc::new(SyncOnlyOverlay::new(fail_path));
        let overlay_writer: Arc<dyn OverlayWriter> = overlay.clone();
        let engine = Arc::new(VfsEngine::new(
            Arc::clone(&resolver),
            Some(overlay_writer),
            hydration,
            None,
            Some(snapshot),
        ));
        let fs = CrabNfsFs::new(resolver, Arc::clone(&engine), ".git", false, None);

        JournalSyncFixture {
            fs,
            engine,
            overlay,
            _root: root,
            _cache: cache,
        }
    }

    fn nfs_read_fixture(nodes: Vec<BaseNode>) -> NfsReadFixture {
        let root = tempfile::tempdir().unwrap();
        let snapshot =
            Arc::new(SnapshotStore::open_or_create(&root.path().join("snapshot.sqlite")).unwrap());
        snapshot
            .publish_generation("abc123", "refs/heads/main", &nodes)
            .unwrap();
        let overlay_store = Arc::new(
            OverlayStore::open(&root.path().join("overlay.db"), &root.path().join("upper"))
                .unwrap(),
        );
        let overlay_lookup: Arc<dyn OverlayLookup> = overlay_store.clone();
        let overlay_writer: Arc<dyn OverlayWriter> = overlay_store;
        let resolver = Arc::new(FuseResolver::new(
            Arc::clone(&snapshot),
            Some(overlay_lookup),
            1,
            0,
        ));
        let cache = tempfile::tempdir().unwrap();
        let engine = Arc::new(VfsEngine::new(
            Arc::clone(&resolver),
            Some(overlay_writer),
            test_hydration(&cache),
            None,
            Some(snapshot),
        ));
        let fs = CrabNfsFs::new(resolver, Arc::clone(&engine), ".git", false, None);

        NfsReadFixture {
            fs,
            engine,
            _root: root,
            _cache: cache,
        }
    }

    fn base_file(path: &str) -> BaseNode {
        BaseNode {
            path: path.to_owned(),
            node_type: NodeType::File,
            mode: 0o100644,
            object_oid: None,
            pointer: None,
            size: 0,
        }
    }

    fn base_dir(path: &str) -> BaseNode {
        BaseNode {
            path: path.to_owned(),
            node_type: NodeType::Dir,
            mode: 0o040755,
            object_oid: None,
            pointer: None,
            size: 0,
        }
    }

    #[test]
    fn id_table_renames_subtree_without_changing_ids() {
        let table = NfsIdTable::new(None);
        let dir = table.id_for_path("a", NodeType::Dir).unwrap();
        let file = table.id_for_path("a/file", NodeType::File).unwrap();

        table.rename_path("a", "b").unwrap();

        assert_eq!(table.path(dir).unwrap(), "b");
        assert_eq!(table.path(file).unwrap(), "b/file");
        assert_eq!(table.id_for_path("b/file", NodeType::File).unwrap(), file);
    }

    #[test]
    fn id_table_rename_drops_replaced_subtree_ids() {
        let table = NfsIdTable::new(None);
        let moved_file = table.id_for_path("src/file", NodeType::File).unwrap();
        let replaced = table.id_for_path("dst", NodeType::Dir).unwrap();
        let replaced_child = table.id_for_path("dst/stale", NodeType::File).unwrap();

        let renamed = table.rename_path("src", "dst").unwrap();

        assert_eq!(table.path(moved_file).unwrap(), "dst/file");
        assert_eq!(table.path(replaced), Err(nfsstat3::NFS3ERR_STALE));
        assert_eq!(table.path(replaced_child), Err(nfsstat3::NFS3ERR_STALE));
        assert_eq!(renamed.moved, vec![moved_file]);
        assert_eq!(renamed.replaced, vec![replaced, replaced_child]);
        assert_eq!(
            table.id_for_path("dst/file", NodeType::File).unwrap(),
            moved_file
        );
    }

    #[test]
    fn id_table_remove_reports_removed_subtree_ids() {
        let table = NfsIdTable::new(None);
        let dir = table.id_for_path("dir", NodeType::Dir).unwrap();
        let file = table.id_for_path("dir/file", NodeType::File).unwrap();
        let other = table.id_for_path("other", NodeType::File).unwrap();

        let removed = table.remove_path("dir").unwrap();

        assert_eq!(removed, vec![dir, file]);
        assert_eq!(table.path(dir), Err(nfsstat3::NFS3ERR_STALE));
        assert_eq!(table.path(file), Err(nfsstat3::NFS3ERR_STALE));
        assert_eq!(table.path(other).unwrap(), "other");
    }

    #[test]
    fn id_table_moves_and_removes_exclusive_verifiers() {
        let table = NfsIdTable::new(None);
        let verifier = nfs::createverf3([1, 2, 3, 4, 5, 6, 7, 8]);

        table.record_exclusive_create("src/file", verifier).unwrap();
        table.rename_path("src", "dst").unwrap();

        assert_eq!(table.exclusive_verifier("src/file").unwrap(), None);
        assert_eq!(
            table.exclusive_verifier("dst/file").unwrap(),
            Some(verifier)
        );

        table.remove_path("dst").unwrap();

        assert_eq!(table.exclusive_verifier("dst/file").unwrap(), None);
    }

    #[test]
    fn id_table_persists_exclusive_create_verifiers() {
        let dir = tempfile::tempdir().unwrap();
        let verifier_path = dir.path().join("nfs-exclusive-verifiers.json");
        let verifier = nfs::createverf3([1, 2, 3, 4, 5, 6, 7, 8]);

        {
            let table = NfsIdTable::new(Some(verifier_path.clone()));
            table.record_exclusive_create("src/file", verifier).unwrap();
            table.rename_path("src", "dst").unwrap();
        }

        let reopened = NfsIdTable::new(Some(verifier_path));

        assert_eq!(reopened.exclusive_verifier("src/file").unwrap(), None);
        assert_eq!(
            reopened.exclusive_verifier("dst/file").unwrap(),
            Some(verifier)
        );
    }

    #[test]
    fn id_table_record_exclusive_create_preserves_state_when_persist_fails() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("store");
        let verifier_path = store_dir.join("nfs-exclusive-verifiers.json");
        let first = nfs::createverf3([1, 2, 3, 4, 5, 6, 7, 8]);
        let second = nfs::createverf3([8, 7, 6, 5, 4, 3, 2, 1]);
        let table = NfsIdTable::new(Some(verifier_path));
        table.record_exclusive_create("existing", first).unwrap();

        std::fs::remove_dir_all(&store_dir).unwrap();
        std::fs::write(&store_dir, b"not a directory").unwrap();

        assert_eq!(
            table.record_exclusive_create("new", second),
            Err(nfsstat3::NFS3ERR_IO)
        );
        assert_eq!(
            table.record_exclusive_create("existing", second),
            Err(nfsstat3::NFS3ERR_IO)
        );
        assert_eq!(table.exclusive_verifier("new").unwrap(), None);
        assert_eq!(table.exclusive_verifier("existing").unwrap(), Some(first));
    }

    #[test]
    fn id_table_remove_exclusive_verifier_preserves_state_when_persist_fails() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("store");
        let verifier_path = store_dir.join("nfs-exclusive-verifiers.json");
        let verifier = nfs::createverf3([1, 2, 3, 4, 5, 6, 7, 8]);
        let table = NfsIdTable::new(Some(verifier_path));
        table.record_exclusive_create("pending", verifier).unwrap();

        std::fs::remove_dir_all(&store_dir).unwrap();
        std::fs::write(&store_dir, b"not a directory").unwrap();

        assert_eq!(
            table.remove_exclusive_verifier("pending"),
            Err(nfsstat3::NFS3ERR_IO)
        );
        assert_eq!(table.exclusive_verifier("pending").unwrap(), Some(verifier));
    }

    #[test]
    fn id_table_remove_preserves_state_when_verifier_persist_fails() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("store");
        let verifier_path = store_dir.join("nfs-exclusive-verifiers.json");
        let verifier = nfs::createverf3([1, 2, 3, 4, 5, 6, 7, 8]);
        let table = NfsIdTable::new(Some(verifier_path));
        let dir_id = table.id_for_path("dir", NodeType::Dir).unwrap();
        let file_id = table.id_for_path("dir/file", NodeType::File).unwrap();
        table.record_exclusive_create("dir/file", verifier).unwrap();

        std::fs::remove_dir_all(&store_dir).unwrap();
        std::fs::write(&store_dir, b"not a directory").unwrap();

        assert_eq!(table.remove_path("dir"), Err(nfsstat3::NFS3ERR_IO));
        assert_eq!(table.path(dir_id).unwrap(), "dir");
        assert_eq!(table.path(file_id).unwrap(), "dir/file");
        assert_eq!(
            table.exclusive_verifier("dir/file").unwrap(),
            Some(verifier)
        );
    }

    #[test]
    fn id_table_rename_preserves_state_when_verifier_persist_fails() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = dir.path().join("store");
        let verifier_path = store_dir.join("nfs-exclusive-verifiers.json");
        let verifier = nfs::createverf3([1, 2, 3, 4, 5, 6, 7, 8]);
        let table = NfsIdTable::new(Some(verifier_path));
        let src_id = table.id_for_path("src", NodeType::Dir).unwrap();
        let file_id = table.id_for_path("src/file", NodeType::File).unwrap();
        let dst_id = table.id_for_path("dst/stale", NodeType::File).unwrap();
        table.record_exclusive_create("src/file", verifier).unwrap();

        std::fs::remove_dir_all(&store_dir).unwrap();
        std::fs::write(&store_dir, b"not a directory").unwrap();

        assert!(matches!(
            table.rename_path("src", "dst"),
            Err(nfsstat3::NFS3ERR_IO)
        ));
        assert_eq!(table.path(src_id).unwrap(), "src");
        assert_eq!(table.path(file_id).unwrap(), "src/file");
        assert_eq!(table.path(dst_id).unwrap(), "dst/stale");
        assert_eq!(
            table.exclusive_verifier("src/file").unwrap(),
            Some(verifier)
        );
        assert_eq!(table.exclusive_verifier("dst/file").unwrap(), None);
    }

    #[test]
    fn exclusive_verifier_hex_round_trips() {
        let verifier = nfs::createverf3([0, 1, 2, 15, 16, 127, 128, 255]);
        let encoded = verifier_to_hex(verifier);

        assert_eq!(encoded, "0001020f107f80ff");
        assert_eq!(verifier_from_hex(&encoded), Some(verifier));
        assert_eq!(verifier_from_hex("not-hex"), None);
    }

    #[test]
    fn nfs_name_rejects_slashes_and_non_utf8() {
        assert!(nfs_name(&filename3::from(b"file".as_slice())).is_ok());
        assert_eq!(
            nfs_name(&filename3::from(b"a/b".as_slice())),
            Err(nfsstat3::NFS3ERR_INVAL)
        );
        assert_eq!(
            nfs_name(&filename3::from(vec![0xff])),
            Err(nfsstat3::NFS3ERR_INVAL)
        );
    }

    #[test]
    fn nfs_name_rejects_nul_and_oversized_components() {
        let max_name = filename3::from(vec![b'a'; NFS_MAX_COMPONENT_BYTES]);
        let too_long = filename3::from(vec![b'a'; NFS_MAX_COMPONENT_BYTES + 1]);

        assert_eq!(nfs_name(&max_name).unwrap().len(), NFS_MAX_COMPONENT_BYTES);
        assert_eq!(
            nfs_name(&filename3::from(b"a\0b".as_slice())),
            Err(nfsstat3::NFS3ERR_INVAL)
        );
        assert_eq!(nfs_name(&too_long), Err(nfsstat3::NFS3ERR_NAMETOOLONG));
    }

    #[test]
    fn nfs_child_name_rejects_dot_entries() {
        assert!(nfs_child_name(&filename3::from(b"file".as_slice())).is_ok());
        assert_eq!(
            nfs_child_name(&filename3::from(b".".as_slice())),
            Err(nfsstat3::NFS3ERR_INVAL)
        );
        assert_eq!(
            nfs_child_name(&filename3::from(b"..".as_slice())),
            Err(nfsstat3::NFS3ERR_INVAL)
        );
    }

    #[test]
    fn nfs_symlink_target_rejects_empty_nul_non_utf8_and_oversized_paths() {
        let max_target = nfspath3::from(vec![b'a'; NFS_MAX_PATH_BYTES]);
        let too_long = nfspath3::from(vec![b'a'; NFS_MAX_PATH_BYTES + 1]);

        assert_eq!(
            nfs_symlink_target(&nfspath3::from(b"target.bin".as_slice())).unwrap(),
            "target.bin"
        );
        assert_eq!(
            nfs_symlink_target(&max_target).unwrap().len(),
            NFS_MAX_PATH_BYTES
        );
        assert_eq!(
            nfs_symlink_target(&nfspath3::from(b"".as_slice())),
            Err(nfsstat3::NFS3ERR_INVAL)
        );
        assert_eq!(
            nfs_symlink_target(&nfspath3::from(b"a\0b".as_slice())),
            Err(nfsstat3::NFS3ERR_INVAL)
        );
        assert_eq!(
            nfs_symlink_target(&nfspath3::from(vec![0xff])),
            Err(nfsstat3::NFS3ERR_INVAL)
        );
        assert_eq!(
            nfs_symlink_target(&too_long),
            Err(nfsstat3::NFS3ERR_NAMETOOLONG)
        );
    }

    #[test]
    fn synthetic_gitfile_is_not_a_mutable_child() {
        assert_eq!(
            ensure_mutable_child(ROOT_ID, ".git"),
            Err(nfsstat3::NFS3ERR_ROFS)
        );
        assert_eq!(ensure_mutable_child(ROOT_ID, "file"), Ok(()));
        assert_eq!(ensure_mutable_child(FIRST_DYNAMIC_ID, ".git"), Ok(()));
    }

    #[test]
    fn path_absence_check_propagates_resolver_errors() {
        assert_eq!(
            ensure_path_absent(Err(CrabError::NotFound { path: "x".into() })),
            Ok(())
        );
        assert_eq!(
            ensure_path_absent(Err(CrabError::Internal("boom".into()))),
            Err(nfsstat3::NFS3ERR_IO)
        );
    }

    #[test]
    fn nfs_read_rejects_non_regular_nodes() {
        assert_eq!(ensure_regular_file_for_read(NodeType::File), Ok(()));
        assert_eq!(
            ensure_regular_file_for_read(NodeType::Dir),
            Err(nfsstat3::NFS3ERR_ISDIR)
        );
        assert_eq!(
            ensure_regular_file_for_read(NodeType::Symlink),
            Err(nfsstat3::NFS3ERR_INVAL)
        );
    }

    #[test]
    fn nfs_write_rejects_non_regular_nodes_with_protocol_inval() {
        assert_eq!(ensure_regular_file_for_write(NodeType::File), Ok(()));
        assert_eq!(
            ensure_regular_file_for_write(NodeType::Dir),
            Err(nfsstat3::NFS3ERR_INVAL)
        );
        assert_eq!(
            ensure_regular_file_for_write(NodeType::Symlink),
            Err(nfsstat3::NFS3ERR_INVAL)
        );
    }

    #[test]
    fn nfs_setattr_size_requires_regular_file() {
        let mut attr = sattr3::default();
        attr.size = nfs::set_size3::Some(10);

        assert_eq!(validate_setattr_target(NodeType::File, &attr), Ok(()));
        assert_eq!(
            validate_setattr_target(NodeType::Dir, &attr),
            Err(nfsstat3::NFS3ERR_ISDIR)
        );
        assert_eq!(
            validate_setattr_target(NodeType::Symlink, &attr),
            Err(nfsstat3::NFS3ERR_ISDIR)
        );
    }

    #[test]
    fn nfs_setattr_metadata_allows_non_regular_nodes() {
        let mut attr = sattr3::default();
        attr.mode = nfs::set_mode3::Some(0o755);

        assert_eq!(validate_setattr_target(NodeType::Dir, &attr), Ok(()));
        assert_eq!(validate_setattr_target(NodeType::Symlink, &attr), Ok(()));
    }

    #[test]
    fn nfs_setattr_accepts_atime_changes_as_noatime() {
        let mut attr = sattr3::default();
        assert_eq!(ensure_setattr_supported(&attr), Ok(()));

        attr.atime = nfs::set_atime::SET_TO_SERVER_TIME;
        assert_eq!(ensure_setattr_supported(&attr), Ok(()));

        attr.atime = nfs::set_atime::SET_TO_CLIENT_TIME(nfstime3 {
            seconds: 1,
            nseconds: 0,
        });
        assert_eq!(ensure_setattr_supported(&attr), Ok(()));
    }

    #[test]
    fn nfs_prefetch_paths_include_regular_files_only() {
        assert_eq!(
            file_prefetch_path(Some("model.bin"), NodeType::File),
            Some("model.bin".to_owned())
        );
        assert_eq!(
            file_prefetch_path(Some("models/weights.bin"), NodeType::File),
            Some("models/weights.bin".to_owned())
        );
        assert_eq!(file_prefetch_path(None, NodeType::File), None);
        assert_eq!(file_prefetch_path(Some("models/dir"), NodeType::Dir), None);
        assert_eq!(
            file_prefetch_path(Some("models/link"), NodeType::Symlink),
            None
        );
    }

    #[test]
    fn nfs_read_eof_detects_exact_boundary_for_known_size() {
        assert!(read_reached_eof(0, 10, 10, Some(10)));
        assert!(read_reached_eof(5, 5, 5, Some(10)));
        assert!(!read_reached_eof(0, 10, 10, Some(11)));
    }

    #[test]
    fn nfs_read_eof_uses_short_read_for_unknown_size() {
        assert!(read_reached_eof(0, 3, 4, None));
        assert!(!read_reached_eof(0, 4, 4, None));
    }

    #[test]
    fn readdirplus_cursor_resumes_from_cookie_path() {
        let ids = NfsIdTable::new(None);
        let file_id = ids.id_for_path("models/b.bin", NodeType::File).unwrap();

        assert_eq!(
            readdirplus_cursor(&ids, ROOT_ID, "", 0),
            (None, true, false)
        );
        assert_eq!(
            readdirplus_cursor(&ids, ROOT_ID, "", GITFILE_ID),
            (None, false, false)
        );
        assert_eq!(
            readdirplus_cursor(&ids, ROOT_ID, "", file_id),
            (None, true, true)
        );
        assert_eq!(
            readdirplus_cursor(&ids, 99, "models", file_id),
            (Some("b.bin".to_owned()), false, false)
        );
    }

    #[test]
    fn nfs_protocol_stats_snapshot_reports_read_and_directory_pressure() {
        let stats = NfsProtocolStats::new();

        stats.record_read_request(1024);
        stats.record_read_response(512);
        stats.record_read_request(65_536);
        stats.record_read_response(65_536);
        stats.record_read_request(1_048_576);
        stats.record_read_response(1_048_576);
        stats.record_read_request(1_048_577);
        stats.record_read_response(1_048_577);
        stats.record_readdirplus_request(1, false);
        stats.record_readdirplus_page(5, 1, true);
        stats.record_readdirplus_entry(true);
        stats.record_readdirplus_entry(true);
        stats.record_readdirplus_request(1, true);
        stats.record_readdirplus_page(NFS_LARGE_READDIRPLUS_ENTRY_THRESHOLD, 2, false);
        for _ in 0..4 {
            stats.record_readdirplus_entry(true);
        }
        stats.record_large_readdirplus();

        let snapshot = stats.snapshot();

        assert_eq!(snapshot.read_rpcs, 4);
        assert_eq!(snapshot.read_requested_bytes, 2_163_713);
        assert_eq!(snapshot.read_returned_bytes, 2_163_201);
        assert_eq!(snapshot.read_size_le_4k, 1);
        assert_eq!(snapshot.read_size_le_64k, 1);
        assert_eq!(snapshot.read_size_le_1m, 1);
        assert_eq!(snapshot.read_size_gt_1m, 1);
        assert_eq!(snapshot.readdirplus_rpcs, 2);
        assert_eq!(snapshot.readdirplus_entries, 6);
        assert_eq!(
            snapshot.readdirplus_materialized_entries,
            5 + NFS_LARGE_READDIRPLUS_ENTRY_THRESHOLD as u64
        );
        assert_eq!(snapshot.readdirplus_returned_candidates, 6);
        assert_eq!(snapshot.readdirplus_attr_resolutions, 6);
        assert_eq!(snapshot.readdirplus_prefetch_paths, 3);
        assert_eq!(snapshot.readdirplus_cookie_resumes, 2);
        assert_eq!(snapshot.readdirplus_cookie_misses, 1);
        assert_eq!(snapshot.readdirplus_skipped_entries, 0);
        assert_eq!(snapshot.readdirplus_large_dirs, 1);
        assert_eq!(snapshot.readdirplus_prefetch_errors, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfs_fsinfo_reports_crab_metadata_precision() {
        let fixture = nfs_read_fixture(Vec::new());

        let fsinfo =
            <CrabNfsFs as NfsReadFileSystem>::fsinfo(&fixture.fs, &FileHandleU64::new(ROOT_ID))
                .await
                .unwrap();

        assert!(matches!(fsinfo.obj_attributes, nfs::post_op_attr::Some(_)));
        assert_eq!(fsinfo.rtmax, NFS_TRANSFER_MAX_BYTES);
        assert_eq!(fsinfo.wtmax, NFS_TRANSFER_MAX_BYTES);
        assert_eq!(fsinfo.maxfilesize, NFS_MAX_FILE_SIZE_BYTES);
        assert_eq!(
            fsinfo.time_delta,
            nfstime3 {
                seconds: 1,
                nseconds: 0
            }
        );
        assert_eq!(fsinfo.properties & nfs::FSF3_SYMLINK, nfs::FSF3_SYMLINK);
        assert_eq!(
            fsinfo.properties & nfs::FSF3_HOMOGENEOUS,
            nfs::FSF3_HOMOGENEOUS
        );
        assert_eq!(fsinfo.properties & nfs::FSF3_CANSETTIME, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfs_readdirplus_reports_synthetic_gitfile_without_prefetch() {
        let fixture = nfs_read_fixture(Vec::new());
        let root = FileHandleU64::new(ROOT_ID);

        let mut iter = <CrabNfsFs as NfsReadFileSystem>::readdirplus(&fixture.fs, &root, 0)
            .await
            .unwrap();
        let entry = match iter.next().await {
            NextResult::Ok(entry) => entry,
            NextResult::Eof => panic!("root directory did not include synthetic .git entry"),
            NextResult::Err(error) => panic!("root readdirplus failed: {error:?}"),
        };

        assert_eq!(entry.fileid, GITFILE_ID);
        assert_eq!(entry.name.as_ref(), b".git");
        assert_eq!(entry.name_attributes.unwrap().type_, ftype3::NF3REG);
        assert!(matches!(iter.next().await, NextResult::Eof));
        let stats = fixture.fs.protocol_stats.snapshot();
        assert_eq!(stats.readdirplus_materialized_entries, 1);
        assert_eq!(stats.readdirplus_returned_candidates, 1);
        assert_eq!(stats.readdirplus_prefetch_paths, 0);
        assert_eq!(stats.readdirplus_prefetch_errors, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfs_readdirplus_pages_large_directories_without_truncation() {
        let nodes = (0..600)
            .map(|index| base_file(&format!("file-{index:04}.bin")))
            .collect();
        let fixture = nfs_read_fixture(nodes);
        let root = FileHandleU64::new(ROOT_ID);
        let mut iter = <CrabNfsFs as NfsReadFileSystem>::readdirplus(&fixture.fs, &root, 0)
            .await
            .unwrap();
        let mut names = Vec::new();

        loop {
            match iter.next().await {
                NextResult::Ok(entry) => {
                    names.push(String::from_utf8(entry.name.as_ref().to_vec()).unwrap());
                }
                NextResult::Eof => break,
                NextResult::Err(error) => panic!("root readdirplus failed: {error:?}"),
            }
        }

        assert_eq!(names.len(), 601);
        assert_eq!(names.first().map(String::as_str), Some(".git"));
        assert_eq!(names.last().map(String::as_str), Some("file-0599.bin"));
        let stats = fixture.fs.protocol_stats.snapshot();
        assert_eq!(stats.readdirplus_materialized_entries, 601);
        assert_eq!(stats.readdirplus_returned_candidates, 601);
        assert_eq!(fixture.fs.directory_pages.snapshot().entries, 3);
    }

    #[tokio::test]
    async fn nfs_readdirplus_omits_placeholder_size_for_unknown_git_blob() {
        let fixture = nfs_read_fixture(vec![BaseNode {
            path: "unknown.txt".to_owned(),
            node_type: NodeType::File,
            mode: 0o100644,
            object_oid: Some("0123456789012345678901234567890123456789".to_owned()),
            pointer: None,
            size: 0,
        }]);
        let root = FileHandleU64::new(ROOT_ID);
        let mut iter = <CrabNfsFs as NfsReadFileSystem>::readdirplus(&fixture.fs, &root, 0)
            .await
            .unwrap();
        let _git = iter.next().await;
        let file = match iter.next().await {
            NextResult::Ok(entry) => entry,
            NextResult::Eof => panic!("unknown file missing from root readdirplus"),
            NextResult::Err(error) => panic!("root readdirplus failed: {error:?}"),
        };

        assert_eq!(file.name.as_ref(), b"unknown.txt");
        assert!(file.name_attributes.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfs_readdirplus_rejects_synthetic_gitfile_as_notdir() {
        let fixture = nfs_read_fixture(Vec::new());
        let gitfile = FileHandleU64::new(GITFILE_ID);

        let err =
            match <CrabNfsFs as NfsReadFileSystem>::readdirplus(&fixture.fs, &gitfile, 0).await {
                Ok(_) => panic!("READDIRPLUS on synthetic .git unexpectedly succeeded"),
                Err(error) => error,
            };

        assert_eq!(err, nfsstat3::NFS3ERR_NOTDIR);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfs_readlink_rejects_synthetic_gitfile_as_not_symlink() {
        let fixture = nfs_read_fixture(Vec::new());
        let gitfile = FileHandleU64::new(GITFILE_ID);

        let err = <CrabNfsFs as NfsReadFileSystem>::readlink(&fixture.fs, &gitfile)
            .await
            .unwrap_err();

        assert_eq!(err, nfsstat3::NFS3ERR_INVAL);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfs_lookup_rejects_child_lookup_under_file_handle() {
        let fixture = nfs_read_fixture(vec![base_file("parent.bin")]);
        let parent_id = fixture
            .fs
            .ids
            .id_for_path("parent.bin", NodeType::File)
            .unwrap();
        let parent = FileHandleU64::new(parent_id);

        let child_err = <CrabNfsFs as NfsReadFileSystem>::lookup(
            &fixture.fs,
            &parent,
            &filename3::from(b"child.bin".as_slice()),
        )
        .await
        .unwrap_err();
        let dot_err = <CrabNfsFs as NfsReadFileSystem>::lookup(
            &fixture.fs,
            &parent,
            &filename3::from(b".".as_slice()),
        )
        .await
        .unwrap_err();

        assert_eq!(child_err, nfsstat3::NFS3ERR_NOTDIR);
        assert_eq!(dot_err, nfsstat3::NFS3ERR_NOTDIR);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfs_create_under_file_parent_does_not_mutate_overlay_or_journal() {
        let fixture = nfs_read_fixture(vec![base_file("parent.bin")]);
        let parent_id = fixture
            .fs
            .ids
            .id_for_path("parent.bin", NodeType::File)
            .unwrap();
        let parent = FileHandleU64::new(parent_id);
        let name = filename3::from(b"child.bin".as_slice());

        let err =
            <CrabNfsFs as NfsFileSystem>::create(&fixture.fs, &parent, &name, sattr3::default())
                .await
                .unwrap_err();

        assert_eq!(err, nfsstat3::NFS3ERR_NOTDIR);
        assert!(matches!(
            fixture.fs.resolver.resolve_path("parent.bin/child.bin"),
            Err(CrabError::NotFound { .. })
        ));
        assert!(fixture.fs.write_journal.pending().unwrap().is_empty());
        assert_eq!(fixture.fs.write_journal.snapshot().sync_attempts, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfs_create_rejects_long_name_before_overlay_or_journal_mutation() {
        let fixture = journal_sync_fixture(None);
        let root = FileHandleU64::new(ROOT_ID);
        let name = filename3::from(vec![b'a'; NFS_MAX_COMPONENT_BYTES + 1]);

        let err =
            <CrabNfsFs as NfsFileSystem>::create(&fixture.fs, &root, &name, sattr3::default())
                .await
                .unwrap_err();

        assert_eq!(err, nfsstat3::NFS3ERR_NAMETOOLONG);
        assert!(fixture.fs.write_journal.pending().unwrap().is_empty());
        assert_eq!(fixture.fs.write_journal.snapshot().sync_attempts, 0);
        assert!(fixture.overlay.synced_paths().is_empty());
        assert_eq!(fixture.overlay.checkpoints(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfs_exclusive_create_under_file_parent_does_not_record_verifier() {
        let fixture = nfs_read_fixture(vec![base_file("parent.bin")]);
        let parent_id = fixture
            .fs
            .ids
            .id_for_path("parent.bin", NodeType::File)
            .unwrap();
        let parent = FileHandleU64::new(parent_id);
        let name = filename3::from(b"exclusive.bin".as_slice());
        let verifier = nfs::createverf3([1, 2, 3, 4, 5, 6, 7, 8]);

        let err =
            <CrabNfsFs as NfsFileSystem>::create_exclusive(&fixture.fs, &parent, &name, verifier)
                .await
                .unwrap_err();

        assert_eq!(err, nfsstat3::NFS3ERR_NOTDIR);
        assert_eq!(
            fixture
                .fs
                .ids
                .exclusive_verifier("parent.bin/exclusive.bin")
                .unwrap(),
            None
        );
        assert!(matches!(
            fixture.fs.resolver.resolve_path("parent.bin/exclusive.bin"),
            Err(CrabError::NotFound { .. })
        ));
        assert_eq!(fixture.fs.write_journal.snapshot().sync_attempts, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfs_read_retries_stale_pooled_lease_once() {
        let fixture = nfs_read_fixture(vec![base_file("race.bin")]);
        let id = fixture
            .fs
            .ids
            .id_for_path("race.bin", NodeType::File)
            .unwrap();
        let stale_lease = fixture.engine.open_read("race.bin").unwrap();
        drop(fixture.fs.read_leases.insert_and_pin(id, stale_lease));

        fixture.engine.write("race.bin", 0, b"A").await.unwrap();

        let (data, eof) =
            <CrabNfsFs as NfsReadFileSystem>::read(&fixture.fs, &FileHandleU64::new(id), 0, 8)
                .await
                .unwrap();

        assert_eq!(&data, b"A");
        assert!(eof);
        let leases = fixture.fs.read_leases.snapshot();
        assert_eq!(leases.entries, 1);
        assert_eq!(leases.hits, 1);
        assert_eq!(leases.misses, 0);
        assert_eq!(leases.evictions, 1);
        assert_eq!(leases.stale_retries, 1);
        let metrics = fixture.engine.read_metrics_snapshot();
        assert_eq!(metrics.open_read_calls, 2);
        assert_eq!(metrics.read_at_calls, 2);
        assert_eq!(metrics.stale_overlay_view_rejections, 1);
        assert_eq!(metrics.returned_bytes, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfs_unstable_write_stays_pending_until_commit() {
        let fixture = nfs_read_fixture(vec![base_file("dirty.bin")]);
        let id = fixture
            .fs
            .ids
            .id_for_path("dirty.bin", NodeType::File)
            .unwrap();
        let handle = FileHandleU64::new(id);

        let (attr, committed) = <CrabNfsFs as NfsFileSystem>::write(
            &fixture.fs,
            &handle,
            0,
            b"abc",
            stable_how::UNSTABLE,
        )
        .await
        .unwrap();

        assert_eq!(committed, stable_how::UNSTABLE);
        assert_eq!(attr.size, 3);
        let pending = fixture.fs.write_journal.pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].path, "dirty.bin");
        assert_eq!(pending[0].last_write_stability, NfsWriteStability::Unstable);
        assert_eq!(fixture.fs.write_journal.snapshot().sync_attempts, 0);

        <CrabNfsFs as NfsFileSystem>::commit(&fixture.fs, &handle, 0, 3)
            .await
            .unwrap();

        assert!(fixture.fs.write_journal.pending().unwrap().is_empty());
        let snapshot = fixture.fs.write_journal.snapshot();
        assert_eq!(snapshot.sync_attempts, 1);
        assert_eq!(snapshot.sync_successes, 1);
        assert_eq!(snapshot.sync_failures, 0);
        assert_eq!(
            &fixture.engine.read("dirty.bin", 0, 8).await.unwrap()[..],
            b"abc"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfs_stable_write_syncs_and_clears_journal_before_reply() {
        let fixture = nfs_read_fixture(vec![base_file("stable.bin")]);
        let id = fixture
            .fs
            .ids
            .id_for_path("stable.bin", NodeType::File)
            .unwrap();
        let handle = FileHandleU64::new(id);

        let (attr, committed) = <CrabNfsFs as NfsFileSystem>::write(
            &fixture.fs,
            &handle,
            0,
            b"synced",
            stable_how::DATA_SYNC,
        )
        .await
        .unwrap();

        assert_eq!(committed, stable_how::FILE_SYNC);
        assert_eq!(attr.size, 6);
        assert!(fixture.fs.write_journal.pending().unwrap().is_empty());
        let snapshot = fixture.fs.write_journal.snapshot();
        assert_eq!(snapshot.sync_attempts, 1);
        assert_eq!(snapshot.sync_successes, 1);
        assert_eq!(snapshot.sync_failures, 0);
        assert_eq!(
            &fixture.engine.read("stable.bin", 0, 8).await.unwrap()[..],
            b"synced"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfs_commit_rejects_directory_handles_without_sync_attempt() {
        let fixture = nfs_read_fixture(vec![base_dir("dir")]);
        let root = FileHandleU64::new(ROOT_ID);
        let dir_id = fixture.fs.ids.id_for_path("dir", NodeType::Dir).unwrap();
        let dir = FileHandleU64::new(dir_id);

        let root_err = <CrabNfsFs as NfsFileSystem>::commit(&fixture.fs, &root, 0, 0)
            .await
            .unwrap_err();
        let dir_err = <CrabNfsFs as NfsFileSystem>::commit(&fixture.fs, &dir, 0, 0)
            .await
            .unwrap_err();

        assert_eq!(root_err, nfsstat3::NFS3ERR_INVAL);
        assert_eq!(dir_err, nfsstat3::NFS3ERR_INVAL);
        assert_eq!(fixture.fs.write_journal.snapshot().sync_attempts, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfs_commit_allows_synthetic_gitfile_without_sync_attempt() {
        let fixture = nfs_read_fixture(Vec::new());
        let gitfile = FileHandleU64::new(GITFILE_ID);

        <CrabNfsFs as NfsFileSystem>::commit(&fixture.fs, &gitfile, 0, 0)
            .await
            .unwrap();

        assert_eq!(fixture.fs.write_journal.snapshot().sync_attempts, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfs_setattr_atime_is_a_noop_without_overlay_or_journal_mutation() {
        let fixture = nfs_read_fixture(vec![base_file("time.bin")]);
        let id = fixture
            .fs
            .ids
            .id_for_path("time.bin", NodeType::File)
            .unwrap();
        let mut attr = sattr3::default();
        attr.atime = nfs::set_atime::SET_TO_SERVER_TIME;

        <CrabNfsFs as NfsFileSystem>::setattr(&fixture.fs, &FileHandleU64::new(id), attr)
            .await
            .unwrap();

        assert!(matches!(
            fixture.fs.resolver.resolve_path("time.bin").unwrap(),
            ResolvedNode::Base(_)
        ));
        assert!(fixture.fs.write_journal.pending().unwrap().is_empty());
        assert_eq!(fixture.fs.write_journal.snapshot().sync_attempts, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfs_create_syncs_new_file_once() {
        let fixture = nfs_read_fixture(Vec::new());
        let root = FileHandleU64::new(ROOT_ID);
        let name = filename3::from(b"created.bin".as_slice());

        let (handle, attr) =
            <CrabNfsFs as NfsFileSystem>::create(&fixture.fs, &root, &name, sattr3::default())
                .await
                .unwrap();

        assert_eq!(attr.size, 0);
        assert_eq!(fixture.fs.ids.path(handle.as_u64()).unwrap(), "created.bin");
        assert!(fixture.fs.write_journal.pending().unwrap().is_empty());
        let snapshot = fixture.fs.write_journal.snapshot();
        assert_eq!(snapshot.sync_attempts, 1);
        assert_eq!(snapshot.sync_successes, 1);
        assert_eq!(snapshot.sync_failures, 0);
        assert_eq!(
            fixture.engine.read("created.bin", 0, 1).await.unwrap(),
            Vec::<u8>::new()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfs_exclusive_create_syncs_and_clears_journal_before_reply() {
        let fixture = nfs_read_fixture(Vec::new());
        let root = FileHandleU64::new(ROOT_ID);
        let name = filename3::from(b"exclusive.bin".as_slice());
        let verifier = nfs::createverf3([9, 8, 7, 6, 5, 4, 3, 2]);

        let handle =
            <CrabNfsFs as NfsFileSystem>::create_exclusive(&fixture.fs, &root, &name, verifier)
                .await
                .unwrap();

        assert_eq!(
            fixture.fs.ids.path(handle.as_u64()).unwrap(),
            "exclusive.bin"
        );
        assert_eq!(
            fixture.fs.ids.exclusive_verifier("exclusive.bin").unwrap(),
            Some(verifier)
        );
        assert!(fixture.fs.write_journal.pending().unwrap().is_empty());
        let snapshot = fixture.fs.write_journal.snapshot();
        assert_eq!(snapshot.sync_attempts, 1);
        assert_eq!(snapshot.sync_successes, 1);
        assert_eq!(snapshot.sync_failures, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfs_symlink_applies_mtime_and_syncs_once_before_reply() {
        let fixture = nfs_read_fixture(Vec::new());
        let root = FileHandleU64::new(ROOT_ID);
        let name = filename3::from(b"link.bin".as_slice());
        let target = nfspath3::from(b"target.bin".as_slice());
        let mtime = nfstime3 {
            seconds: 1_700_000_000,
            nseconds: 0,
        };
        let mut attr = sattr3::default();
        attr.mtime = nfs::set_mtime::SET_TO_CLIENT_TIME(mtime);

        let (handle, returned_attr) =
            <CrabNfsFs as NfsFileSystem>::symlink(&fixture.fs, &root, &name, &target, &attr)
                .await
                .unwrap();

        assert_eq!(fixture.fs.ids.path(handle.as_u64()).unwrap(), "link.bin");
        assert_eq!(returned_attr.type_, ftype3::NF3LNK);
        assert_eq!(returned_attr.mtime, mtime);
        let readlink = <CrabNfsFs as NfsReadFileSystem>::readlink(&fixture.fs, &handle)
            .await
            .unwrap();
        assert_eq!(readlink.as_ref(), b"target.bin");
        assert!(fixture.fs.write_journal.pending().unwrap().is_empty());
        let snapshot = fixture.fs.write_journal.snapshot();
        assert_eq!(snapshot.sync_attempts, 1);
        assert_eq!(snapshot.sync_successes, 1);
        assert_eq!(snapshot.sync_failures, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfs_symlink_rejects_size_attr_before_overlay_create() {
        let fixture = nfs_read_fixture(Vec::new());
        let root = FileHandleU64::new(ROOT_ID);
        let name = filename3::from(b"bad-link.bin".as_slice());
        let target = nfspath3::from(b"target.bin".as_slice());
        let mut attr = sattr3::default();
        attr.size = nfs::set_size3::Some(4);

        let err = <CrabNfsFs as NfsFileSystem>::symlink(&fixture.fs, &root, &name, &target, &attr)
            .await
            .unwrap_err();

        assert_eq!(err, nfsstat3::NFS3ERR_ISDIR);
        assert!(matches!(
            fixture.fs.resolver.resolve_path("bad-link.bin"),
            Err(CrabError::NotFound { .. })
        ));
        assert!(fixture.fs.write_journal.pending().unwrap().is_empty());
        assert_eq!(fixture.fs.write_journal.snapshot().sync_attempts, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfs_symlink_rejects_long_target_before_overlay_or_journal_mutation() {
        let fixture = journal_sync_fixture(None);
        let root = FileHandleU64::new(ROOT_ID);
        let name = filename3::from(b"bad-link.bin".as_slice());
        let target = nfspath3::from(vec![b'a'; NFS_MAX_PATH_BYTES + 1]);

        let err = <CrabNfsFs as NfsFileSystem>::symlink(
            &fixture.fs,
            &root,
            &name,
            &target,
            &sattr3::default(),
        )
        .await
        .unwrap_err();

        assert_eq!(err, nfsstat3::NFS3ERR_NAMETOOLONG);
        assert!(fixture.fs.write_journal.pending().unwrap().is_empty());
        assert_eq!(fixture.fs.write_journal.snapshot().sync_attempts, 0);
        assert!(fixture.overlay.synced_paths().is_empty());
        assert_eq!(fixture.overlay.checkpoints(), 0);
    }

    #[test]
    fn nfs_created_path_sync_failure_stays_pending() {
        let fixture = journal_sync_fixture(Some("created.bin"));

        let err = fixture.fs.sync_created_path("created.bin").unwrap_err();

        assert_eq!(err, nfsstat3::NFS3ERR_IO);
        assert_eq!(fixture.overlay.synced_paths(), vec!["created.bin"]);
        assert_eq!(fixture.overlay.checkpoints(), 0);
        let pending = fixture.fs.write_journal.pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].path, "created.bin");
        assert_eq!(pending[0].last_write_stability, NfsWriteStability::FileSync);
        assert_eq!(pending[0].last_sync_error, Some(nfsstat3::NFS3ERR_IO));
        let snapshot = fixture.fs.write_journal.snapshot();
        assert_eq!(snapshot.pending_paths, 1);
        assert_eq!(snapshot.paths_with_sync_errors, 1);
        assert_eq!(snapshot.sync_attempts, 1);
        assert_eq!(snapshot.sync_successes, 0);
        assert_eq!(snapshot.sync_failures, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfs_remove_clears_derived_protocol_state_after_engine_mutation() {
        let fixture = nfs_read_fixture(vec![base_file("victim.bin")]);
        let id = fixture
            .fs
            .ids
            .id_for_path("victim.bin", NodeType::File)
            .unwrap();
        let handle = FileHandleU64::new(id);

        <CrabNfsFs as NfsFileSystem>::write(
            &fixture.fs,
            &handle,
            0,
            b"dirty",
            stable_how::UNSTABLE,
        )
        .await
        .unwrap();
        let lease = fixture.engine.open_read("victim.bin").unwrap();
        drop(fixture.fs.read_leases.insert_and_pin(id, lease));
        let root_page = fixture
            .fs
            .directory_pages
            .key("", fixture.fs.resolver.generation());
        fixture.fs.directory_pages.insert(
            root_page.clone(),
            Arc::new(vec![directory_candidate(id, "victim.bin")]),
        );

        let root = FileHandleU64::new(ROOT_ID);
        let name = filename3::from(b"victim.bin".as_slice());
        <CrabNfsFs as NfsFileSystem>::remove(&fixture.fs, &root, &name)
            .await
            .unwrap();

        assert_eq!(fixture.fs.ids.path(id), Err(nfsstat3::NFS3ERR_STALE));
        let leases = fixture.fs.read_leases.snapshot();
        assert_eq!(leases.entries, 0);
        assert_eq!(leases.evictions, 1);
        assert!(fixture.fs.write_journal.pending().unwrap().is_empty());
        assert!(fixture.fs.directory_pages.get(&root_page).is_none());
        let directory_pages = fixture.fs.directory_pages.snapshot();
        assert_eq!(directory_pages.entries, 0);
        assert_eq!(directory_pages.stale_evictions, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfs_rename_moves_derived_protocol_state_after_engine_mutation() {
        let fixture = nfs_read_fixture(vec![
            base_dir("src"),
            base_file("src/file.bin"),
            base_dir("keep"),
        ]);
        let src_id = fixture.fs.ids.id_for_path("src", NodeType::Dir).unwrap();
        let file_id = fixture
            .fs
            .ids
            .id_for_path("src/file.bin", NodeType::File)
            .unwrap();
        let replaced_id = fixture
            .fs
            .ids
            .id_for_path("dst/stale.bin", NodeType::File)
            .unwrap();
        let lease = fixture.engine.open_read("src/file.bin").unwrap();
        drop(fixture.fs.read_leases.insert_and_pin(file_id, lease));
        fixture
            .fs
            .write_journal
            .mark_write("src/file.bin", NfsWriteStability::Unstable, 1);
        fixture
            .fs
            .write_journal
            .mark_write("dst/stale.bin", NfsWriteStability::Unstable, 2);
        let generation = fixture.fs.resolver.generation();
        let root_page = fixture.fs.directory_pages.key("", generation);
        let src_page = fixture.fs.directory_pages.key("src", generation);
        let dst_page = fixture.fs.directory_pages.key("dst", generation);
        let keep_page = fixture.fs.directory_pages.key("keep", generation);
        fixture.fs.directory_pages.insert(
            root_page.clone(),
            Arc::new(vec![directory_candidate(src_id, "src")]),
        );
        fixture.fs.directory_pages.insert(
            src_page.clone(),
            Arc::new(vec![directory_candidate(file_id, "file.bin")]),
        );
        fixture.fs.directory_pages.insert(
            dst_page.clone(),
            Arc::new(vec![directory_candidate(replaced_id, "stale.bin")]),
        );
        fixture.fs.directory_pages.insert(
            keep_page.clone(),
            Arc::new(vec![directory_candidate(99, "kept.bin")]),
        );

        let root = FileHandleU64::new(ROOT_ID);
        let src = filename3::from(b"src".as_slice());
        let dst = filename3::from(b"dst".as_slice());
        <CrabNfsFs as NfsFileSystem>::rename(&fixture.fs, &root, &src, &root, &dst)
            .await
            .unwrap();

        assert_eq!(fixture.fs.ids.path(src_id).unwrap(), "dst");
        assert_eq!(fixture.fs.ids.path(file_id).unwrap(), "dst/file.bin");
        assert_eq!(
            fixture.fs.ids.path(replaced_id),
            Err(nfsstat3::NFS3ERR_STALE)
        );
        let leases = fixture.fs.read_leases.snapshot();
        assert_eq!(leases.entries, 0);
        assert_eq!(leases.evictions, 1);
        assert_eq!(
            fixture
                .fs
                .write_journal
                .pending()
                .unwrap()
                .into_iter()
                .map(|entry| (entry.path, entry.overlay_version))
                .collect::<Vec<_>>(),
            vec![("dst/file.bin".to_owned(), 1)]
        );
        assert!(fixture.fs.directory_pages.get(&root_page).is_none());
        assert!(fixture.fs.directory_pages.get(&src_page).is_none());
        assert!(fixture.fs.directory_pages.get(&dst_page).is_none());
        assert!(fixture.fs.directory_pages.get(&keep_page).is_some());
        let directory_pages = fixture.fs.directory_pages.snapshot();
        assert_eq!(directory_pages.entries, 1);
        assert_eq!(directory_pages.stale_evictions, 3);
        assert_eq!(
            fixture.engine.read("dst/file.bin", 0, 1).await.unwrap(),
            Vec::<u8>::new()
        );
        assert!(matches!(
            fixture.engine.read("src/file.bin", 0, 1).await,
            Err(CrabError::NotFound { .. })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfs_failed_remove_keeps_derived_protocol_state_unchanged() {
        let fixture = nfs_read_fixture(vec![base_dir("dir"), base_file("dir/file.bin")]);
        let dir_id = fixture.fs.ids.id_for_path("dir", NodeType::Dir).unwrap();
        let file_id = fixture
            .fs
            .ids
            .id_for_path("dir/file.bin", NodeType::File)
            .unwrap();
        let lease = fixture.engine.open_read("dir/file.bin").unwrap();
        drop(fixture.fs.read_leases.insert_and_pin(file_id, lease));
        fixture
            .fs
            .write_journal
            .mark_write("dir/file.bin", NfsWriteStability::Unstable, 1);
        let generation = fixture.fs.resolver.generation();
        let root_page = fixture.fs.directory_pages.key("", generation);
        let dir_page = fixture.fs.directory_pages.key("dir", generation);
        fixture.fs.directory_pages.insert(
            root_page.clone(),
            Arc::new(vec![directory_candidate(dir_id, "dir")]),
        );
        fixture.fs.directory_pages.insert(
            dir_page.clone(),
            Arc::new(vec![directory_candidate(file_id, "file.bin")]),
        );

        let root = FileHandleU64::new(ROOT_ID);
        let name = filename3::from(b"dir".as_slice());
        let err = <CrabNfsFs as NfsFileSystem>::remove(&fixture.fs, &root, &name)
            .await
            .unwrap_err();

        assert_eq!(err, nfsstat3::NFS3ERR_NOTEMPTY);
        assert_eq!(fixture.fs.ids.path(dir_id).unwrap(), "dir");
        assert_eq!(fixture.fs.ids.path(file_id).unwrap(), "dir/file.bin");
        let leases = fixture.fs.read_leases.snapshot();
        assert_eq!(leases.entries, 1);
        assert_eq!(leases.evictions, 0);
        assert_eq!(
            fixture
                .fs
                .write_journal
                .pending()
                .unwrap()
                .into_iter()
                .map(|entry| (entry.path, entry.overlay_version))
                .collect::<Vec<_>>(),
            vec![("dir/file.bin".to_owned(), 1)]
        );
        assert!(fixture.fs.directory_pages.get(&root_page).is_some());
        assert!(fixture.fs.directory_pages.get(&dir_page).is_some());
        let directory_pages = fixture.fs.directory_pages.snapshot();
        assert_eq!(directory_pages.entries, 2);
        assert_eq!(directory_pages.evictions, 0);
        assert_eq!(directory_pages.stale_evictions, 0);
        assert!(fixture.fs.resolver.resolve_path("dir").is_ok());
        assert!(fixture.fs.resolver.resolve_path("dir/file.bin").is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfs_failed_rename_keeps_derived_protocol_state_unchanged() {
        let fixture = nfs_read_fixture(vec![base_dir("src"), base_file("src/file.bin")]);
        let src_id = fixture.fs.ids.id_for_path("src", NodeType::Dir).unwrap();
        let file_id = fixture
            .fs
            .ids
            .id_for_path("src/file.bin", NodeType::File)
            .unwrap();
        let lease = fixture.engine.open_read("src/file.bin").unwrap();
        drop(fixture.fs.read_leases.insert_and_pin(file_id, lease));
        fixture
            .fs
            .write_journal
            .mark_write("src/file.bin", NfsWriteStability::Unstable, 1);
        let generation = fixture.fs.resolver.generation();
        let root_page = fixture.fs.directory_pages.key("", generation);
        let src_page = fixture.fs.directory_pages.key("src", generation);
        fixture.fs.directory_pages.insert(
            root_page.clone(),
            Arc::new(vec![directory_candidate(src_id, "src")]),
        );
        fixture.fs.directory_pages.insert(
            src_page.clone(),
            Arc::new(vec![directory_candidate(file_id, "file.bin")]),
        );

        let root = FileHandleU64::new(ROOT_ID);
        let src_handle = FileHandleU64::new(src_id);
        let src = filename3::from(b"src".as_slice());
        let child = filename3::from(b"child".as_slice());
        let err =
            <CrabNfsFs as NfsFileSystem>::rename(&fixture.fs, &root, &src, &src_handle, &child)
                .await
                .unwrap_err();

        assert_eq!(err, nfsstat3::NFS3ERR_ROFS);
        assert_eq!(fixture.fs.ids.path(src_id).unwrap(), "src");
        assert_eq!(fixture.fs.ids.path(file_id).unwrap(), "src/file.bin");
        let leases = fixture.fs.read_leases.snapshot();
        assert_eq!(leases.entries, 1);
        assert_eq!(leases.evictions, 0);
        assert_eq!(
            fixture
                .fs
                .write_journal
                .pending()
                .unwrap()
                .into_iter()
                .map(|entry| (entry.path, entry.overlay_version))
                .collect::<Vec<_>>(),
            vec![("src/file.bin".to_owned(), 1)]
        );
        assert!(fixture.fs.directory_pages.get(&root_page).is_some());
        assert!(fixture.fs.directory_pages.get(&src_page).is_some());
        let directory_pages = fixture.fs.directory_pages.snapshot();
        assert_eq!(directory_pages.entries, 2);
        assert_eq!(directory_pages.evictions, 0);
        assert_eq!(directory_pages.stale_evictions, 0);
        assert!(fixture.fs.resolver.resolve_path("src").is_ok());
        assert!(fixture.fs.resolver.resolve_path("src/file.bin").is_ok());
        assert!(matches!(
            fixture.fs.resolver.resolve_path("src/child"),
            Err(CrabError::NotFound { .. })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nfs_rename_into_file_parent_keeps_source_unchanged() {
        let fixture = nfs_read_fixture(vec![base_file("src.bin"), base_file("parent.bin")]);
        let src_id = fixture
            .fs
            .ids
            .id_for_path("src.bin", NodeType::File)
            .unwrap();
        let parent_id = fixture
            .fs
            .ids
            .id_for_path("parent.bin", NodeType::File)
            .unwrap();
        let root = FileHandleU64::new(ROOT_ID);
        let parent = FileHandleU64::new(parent_id);
        let src = filename3::from(b"src.bin".as_slice());
        let child = filename3::from(b"child.bin".as_slice());

        let err = <CrabNfsFs as NfsFileSystem>::rename(&fixture.fs, &root, &src, &parent, &child)
            .await
            .unwrap_err();

        assert_eq!(err, nfsstat3::NFS3ERR_NOTDIR);
        assert_eq!(fixture.fs.ids.path(src_id).unwrap(), "src.bin");
        assert!(fixture.fs.resolver.resolve_path("src.bin").is_ok());
        assert!(matches!(
            fixture.fs.resolver.resolve_path("parent.bin/child.bin"),
            Err(CrabError::NotFound { .. })
        ));
        assert!(fixture.fs.write_journal.pending().unwrap().is_empty());
    }

    #[test]
    fn nfs_directory_page_cache_tracks_hits_misses_and_stale_generation() {
        let cache = NfsDirectoryPageCache::new(4, 4096);
        let key = cache.key("models", 1);
        let next_generation = cache.key("models", 2);
        let candidates = Arc::new(vec![directory_candidate(10, "weights.bin")]);

        assert!(cache.get(&key).is_none());
        cache.insert(key.clone(), Arc::clone(&candidates));
        let hit = cache.get(&key).unwrap();
        assert_eq!(hit.len(), 1);
        assert!(Arc::ptr_eq(&hit, &candidates));
        assert!(cache.get(&next_generation).is_none());

        let snapshot = cache.snapshot();
        assert_eq!(snapshot.entries, 0);
        assert_eq!(snapshot.hits, 1);
        assert_eq!(snapshot.misses, 2);
        assert_eq!(snapshot.stale_evictions, 1);
    }

    #[test]
    fn nfs_directory_page_cache_evicts_lru_entries() {
        let cache = NfsDirectoryPageCache::new(1, 4096);
        let first = cache.key("a", 1);
        let second = cache.key("b", 1);

        cache.insert(
            first.clone(),
            Arc::new(vec![directory_candidate(10, "a.bin")]),
        );
        assert!(cache.get(&first).is_some());
        cache.insert(
            second.clone(),
            Arc::new(vec![directory_candidate(20, "b.bin")]),
        );

        assert!(cache.get(&first).is_none());
        assert!(cache.get(&second).is_some());
        let snapshot = cache.snapshot();
        assert_eq!(snapshot.entries, 1);
        assert_eq!(snapshot.evictions, 1);
    }

    #[test]
    fn nfs_directory_page_cache_keeps_unrelated_directory_after_path_invalidation() {
        let cache = NfsDirectoryPageCache::new(4, 4096);
        let mutated = cache.key("models", 1);
        let unrelated = cache.key("datasets", 1);

        cache.insert(
            mutated.clone(),
            Arc::new(vec![directory_candidate(10, "weights.bin")]),
        );
        cache.insert(
            unrelated.clone(),
            Arc::new(vec![directory_candidate(20, "sample.bin")]),
        );

        cache.invalidate_path("models");

        assert!(cache.get(&mutated).is_none());
        assert!(cache.get(&unrelated).is_some());
        let snapshot = cache.snapshot();
        assert_eq!(snapshot.entries, 1);
        assert_eq!(snapshot.stale_evictions, 1);
    }

    #[test]
    fn nfs_directory_page_cache_invalidates_renamed_subtree_without_dropping_sibling() {
        let cache = NfsDirectoryPageCache::new(4, 4096);
        let root = cache.key("models", 1);
        let child = cache.key("models/checkpoints", 1);
        let sibling = cache.key("datasets", 1);

        cache.insert(
            root.clone(),
            Arc::new(vec![directory_candidate(10, "a.bin")]),
        );
        cache.insert(
            child.clone(),
            Arc::new(vec![directory_candidate(20, "b.bin")]),
        );
        cache.insert(
            sibling.clone(),
            Arc::new(vec![directory_candidate(30, "c.bin")]),
        );

        cache.invalidate_rename("models", "renamed-models");

        assert!(cache.get(&root).is_none());
        assert!(cache.get(&child).is_none());
        assert!(cache.get(&sibling).is_some());
        let snapshot = cache.snapshot();
        assert_eq!(snapshot.entries, 1);
        assert_eq!(snapshot.stale_evictions, 2);
    }

    #[test]
    fn nfs_directory_page_cache_compacts_invalidation_versions_when_bounded() {
        let cache = NfsDirectoryPageCache::new(4, 4096);
        let key = cache.key("keep", 1);

        cache.insert(
            key.clone(),
            Arc::new(vec![directory_candidate(10, "keep.bin")]),
        );
        for index in 0..=NFS_DIRECTORY_PAGE_INVALIDATION_MAX_ENTRIES {
            cache.invalidate_path(&format!("dir-{index}"));
        }

        let refreshed = cache.key("keep", 1);
        assert!(cache.get(&key).is_none());
        assert!(cache.get(&refreshed).is_none());
        let snapshot = cache.snapshot();
        assert_eq!(snapshot.entries, 0);
        assert_eq!(snapshot.stale_evictions, 1);
    }

    fn directory_candidate(id: u64, name: &str) -> NfsDirectoryCandidate {
        NfsDirectoryCandidate {
            id,
            name: name.to_owned(),
            path: Some(name.to_owned()),
            node_type: NodeType::File,
            attr: None,
        }
    }

    #[test]
    fn nfs_write_journal_tracks_stability_and_sync_errors() {
        let journal = NfsWriteJournal::new();

        journal.mark_write("z.bin", NfsWriteStability::Unstable, 7);
        journal.mark_write("a.bin", NfsWriteStability::DataSync, 8);
        journal.record_sync_error("a.bin", nfsstat3::NFS3ERR_IO);
        journal.mark_synced("z.bin");

        let pending = journal.pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].path, "a.bin");
        assert_eq!(pending[0].overlay_version, 8);
        assert_eq!(pending[0].last_write_stability, NfsWriteStability::DataSync);
        assert_eq!(pending[0].last_sync_error, Some(nfsstat3::NFS3ERR_IO));
    }

    #[test]
    fn nfs_write_journal_snapshot_reports_pending_state() {
        let journal = NfsWriteJournal::new();

        journal.mark_write("z.bin", NfsWriteStability::Unstable, 7);
        journal.mark_write("a.bin", NfsWriteStability::FileSync, 8);
        journal.record_sync_error("a.bin", nfsstat3::NFS3ERR_NOSPC);
        journal.record_sync_result(4, true);
        journal.record_sync_result(9, false);

        let snapshot = journal.snapshot();

        assert_eq!(snapshot.pending_paths, 2);
        assert_eq!(snapshot.paths_with_sync_errors, 1);
        assert_eq!(snapshot.sync_attempts, 2);
        assert_eq!(snapshot.sync_successes, 1);
        assert_eq!(snapshot.sync_failures, 1);
        assert_eq!(snapshot.total_sync_latency_ms, 13);
        assert_eq!(snapshot.last_sync_latency_ms, Some(9));
        assert_eq!(snapshot.max_sync_latency_ms, Some(9));
        assert!(snapshot.oldest_dirty_age_secs.is_some());
        assert!(!snapshot.poisoned);
        assert_eq!(
            snapshot
                .entries
                .into_iter()
                .map(|entry| {
                    (
                        entry.path,
                        entry.overlay_version,
                        entry.last_write_stability,
                        entry.dirty_age_secs.is_some(),
                        entry.last_sync_error,
                    )
                })
                .collect::<Vec<_>>(),
            vec![
                (
                    "a.bin".to_owned(),
                    8,
                    NfsWriteStability::FileSync,
                    true,
                    Some(nfsstat3::NFS3ERR_NOSPC)
                ),
                (
                    "z.bin".to_owned(),
                    7,
                    NfsWriteStability::Unstable,
                    true,
                    None
                )
            ]
        );
    }

    #[test]
    fn nfs_write_journal_follows_rename_and_remove_subtrees() {
        let journal = NfsWriteJournal::new();

        journal.mark_write("src/file.bin", NfsWriteStability::Unstable, 1);
        journal.mark_write("src/nested/model.bin", NfsWriteStability::Unstable, 2);
        journal.mark_write("dst/stale.bin", NfsWriteStability::Unstable, 3);
        journal.rename_subtree("src", "dst");

        assert_eq!(
            journal
                .pending()
                .unwrap()
                .into_iter()
                .map(|entry| (entry.path, entry.overlay_version))
                .collect::<Vec<_>>(),
            vec![
                ("dst/file.bin".to_owned(), 1),
                ("dst/nested/model.bin".to_owned(), 2)
            ]
        );

        journal.remove_subtree("dst");
        journal.mark_write("keep.bin", NfsWriteStability::FileSync, 4);
        journal.mark_write("dir/file.bin", NfsWriteStability::Unstable, 5);
        journal.mark_write("dir/nested/file.bin", NfsWriteStability::Unstable, 6);
        journal.remove_subtree("dir");

        assert_eq!(
            journal
                .pending()
                .unwrap()
                .into_iter()
                .map(|entry| entry.path)
                .collect::<Vec<_>>(),
            vec!["keep.bin".to_owned()]
        );
    }

    #[test]
    fn nfs_write_journal_sync_all_clears_successful_shutdown_drain() {
        let fixture = journal_sync_fixture(None);
        let journal = NfsWriteJournal::new();

        journal.mark_write("z.bin", NfsWriteStability::Unstable, 1);
        journal.mark_write("a.bin", NfsWriteStability::DataSync, 2);

        journal.sync_all(&fixture.engine).unwrap();

        assert!(journal.pending().unwrap().is_empty());
        assert_eq!(fixture.overlay.synced_paths(), vec!["a.bin", "z.bin"]);
        assert_eq!(fixture.overlay.checkpoints(), 2);
        let snapshot = journal.snapshot();
        assert_eq!(snapshot.pending_paths, 0);
        assert_eq!(snapshot.sync_attempts, 2);
        assert_eq!(snapshot.sync_successes, 2);
        assert_eq!(snapshot.sync_failures, 0);
    }

    #[test]
    fn nfs_write_journal_sync_all_retains_failures_and_continues_shutdown_drain() {
        let fixture = journal_sync_fixture(Some("a-fail.bin"));
        let journal = NfsWriteJournal::new();

        journal.mark_write("z-ok.bin", NfsWriteStability::FileSync, 1);
        journal.mark_write("a-fail.bin", NfsWriteStability::Unstable, 2);

        let error = journal.sync_all(&fixture.engine).unwrap_err();

        assert!(
            error.to_string().contains(
                "NFS write journal sync failed for 1 path(s); first failure at a-fail.bin"
            )
        );
        assert_eq!(
            fixture.overlay.synced_paths(),
            vec!["a-fail.bin", "z-ok.bin"]
        );
        assert_eq!(fixture.overlay.checkpoints(), 1);
        assert_eq!(
            journal
                .pending()
                .unwrap()
                .into_iter()
                .map(|entry| (entry.path, entry.last_sync_error))
                .collect::<Vec<_>>(),
            vec![("a-fail.bin".to_owned(), Some(nfsstat3::NFS3ERR_IO))]
        );
        let snapshot = journal.snapshot();
        assert_eq!(snapshot.pending_paths, 1);
        assert_eq!(snapshot.paths_with_sync_errors, 1);
        assert_eq!(snapshot.sync_attempts, 2);
        assert_eq!(snapshot.sync_successes, 1);
        assert_eq!(snapshot.sync_failures, 1);
    }
}
