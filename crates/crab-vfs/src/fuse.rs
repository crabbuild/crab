//! FUSE filesystem adapter: maps kernel FUSE operations to crab internals.
//!
//! `CrabFs` implements `fuser::Filesystem` by maintaining an inode table
//! (monotonic allocation, like artifact-fs) and delegating to `FuseResolver`
//! for tree queries and `VfsEngine` for read/write I/O.
//!
//! FUSE callbacks are synchronous (called from fuser's background threads).
//! Async engine/resolver calls are bridged via `Handle::block_on`.
//!
//! Threading model: fuser dispatches callbacks on its own thread pool.
//! `block_on` is safe here because the tokio runtime runs on separate
//! threads — fuser threads never run tokio tasks, so there's no
//! deadlock risk from blocking. The hydration service's synchronous
//! `do_fetch_chunk` also avoids holding the runtime hostage.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use fuser::{
    BsdFileFlags, Errno, FileAttr, FileHandle as FuseFileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, InitFlags, KernelConfig, LockOwner, OpenAccMode, OpenFlags, RenameFlags,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyWrite, ReplyXattr, Request, WriteFlags,
};
use tokio::runtime::Handle;
use tracing::{debug, trace, warn};

use crate::core::error::CrabError;
use crate::engine::{VfsEngine, VfsReadLease};
use crate::resolver::{FuseResolver, ReaddirEntry, ResolvedNode};
use crate::snapshot::NodeType;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Root inode is always 1 in FUSE.
const ROOT_INO: u64 = 1;

/// Special inode for the synthetic `.git` file at the mountpoint root.
const GITFILE_INO: u64 = 2;

/// First dynamically-allocated inode.
const FIRST_DYNAMIC_INO: u64 = 3;

/// TTL for directory entry and attribute caches.
const ENTRY_TTL: Duration = Duration::ZERO;

/// Longer TTL for the synthetic `.git` file (it never changes).
const GITFILE_TTL: Duration = Duration::from_secs(60);

/// Target FUSE readahead window for large sequential reads.
const FUSE_TARGET_MAX_READAHEAD: u32 = 1024 * 1024;

// ---------------------------------------------------------------------------
// Inode table — uses Arc<str> to avoid cloning full strings on every lookup
// ---------------------------------------------------------------------------

/// Maps inodes ↔ paths with monotonic allocation.
struct InodeTable {
    by_ino: HashMap<u64, InodeRef>,
    by_path: HashMap<Arc<str>, u64>,
    next_ino: u64,
}

struct InodeRef {
    path: Arc<str>,
    #[allow(dead_code, reason = "used for future readlink and type-checking")]
    node_type: NodeType,
    refcnt: u64,
}

impl InodeTable {
    fn new() -> Self {
        let root_path: Arc<str> = Arc::from("");
        let git_path: Arc<str> = Arc::from(".git");

        let mut by_ino = HashMap::new();
        let mut by_path = HashMap::new();

        by_ino.insert(
            ROOT_INO,
            InodeRef {
                path: Arc::clone(&root_path),
                node_type: NodeType::Dir,
                refcnt: 1,
            },
        );
        by_path.insert(root_path, ROOT_INO);

        by_ino.insert(
            GITFILE_INO,
            InodeRef {
                path: Arc::clone(&git_path),
                node_type: NodeType::File,
                refcnt: 1,
            },
        );
        by_path.insert(git_path, GITFILE_INO);

        Self {
            by_ino,
            by_path,
            next_ino: FIRST_DYNAMIC_INO,
        }
    }

    fn get_or_alloc(&mut self, path: &str, node_type: NodeType) -> u64 {
        if let Some(&ino) = self.by_path.get(path) {
            if let Some(r) = self.by_ino.get_mut(&ino) {
                r.refcnt += 1;
            }
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        let arc_path: Arc<str> = Arc::from(path);
        self.by_ino.insert(
            ino,
            InodeRef {
                path: Arc::clone(&arc_path),
                node_type,
                refcnt: 1,
            },
        );
        self.by_path.insert(arc_path, ino);
        ino
    }

    /// Return a cheap Arc clone of the path — no string copy.
    fn get_path(&self, ino: u64) -> Option<Arc<str>> {
        self.by_ino.get(&ino).map(|r| Arc::clone(&r.path))
    }

    fn ino_for_path(&self, path: &str) -> Option<u64> {
        self.by_path.get(path).copied()
    }

    fn rename_path(&mut self, old_path: &str, new_path: &str) {
        let old_prefix = format!("{old_path}/");
        let updates = self
            .by_ino
            .iter()
            .filter_map(|(ino, inode_ref)| {
                let path = inode_ref.path.as_ref();
                if path == old_path {
                    Some((*ino, new_path.to_owned()))
                } else {
                    path.strip_prefix(&old_prefix)
                        .map(|suffix| (*ino, format!("{new_path}/{suffix}")))
                }
            })
            .collect::<Vec<_>>();

        for (ino, moved_path) in updates {
            if let Some(inode_ref) = self.by_ino.get_mut(&ino) {
                self.by_path.remove(&inode_ref.path);
                let arc_path: Arc<str> = Arc::from(moved_path.as_str());
                inode_ref.path = Arc::clone(&arc_path);
                self.by_path.insert(arc_path, ino);
            }
        }
    }

    fn forget(&mut self, ino: u64, nlookup: u64) {
        if ino == ROOT_INO || ino == GITFILE_INO {
            return;
        }
        if let Some(r) = self.by_ino.get_mut(&ino) {
            r.refcnt = r.refcnt.saturating_sub(nlookup);
            if r.refcnt == 0 {
                let path = Arc::clone(&r.path);
                self.by_ino.remove(&ino);
                self.by_path.remove(&path);
            }
        }
    }
}

/// Shared view of the live inode table used for out-of-band invalidation.
#[derive(Clone)]
pub struct FuseInvalidationIndex {
    inodes: Arc<std::sync::RwLock<InodeTable>>,
}

/// Kernel entry/inode target for a changed path.
pub struct FuseInvalidationTarget {
    pub parent: INodeNo,
    pub name: OsString,
    pub inode: Option<INodeNo>,
}

impl FuseInvalidationIndex {
    /// Return the cached parent/child inode pair for a relative path.
    pub fn target_for_path(&self, path: &str) -> Option<FuseInvalidationTarget> {
        let path = normalize_relative_path(path)?;
        let (parent_path, name) = split_parent_name(path)?;
        let inodes = self.inodes.read().unwrap_or_else(|e| {
            warn!("inodes RwLock was poisoned; recovering");
            e.into_inner()
        });
        let parent = inodes.ino_for_path(parent_path)?;
        let inode = inodes.ino_for_path(path);
        Some(FuseInvalidationTarget {
            parent: INodeNo(parent),
            name: OsString::from(name),
            inode: inode.map(INodeNo),
        })
    }
}

fn normalize_relative_path(path: &str) -> Option<&str> {
    let path = path.trim_matches('/');
    if path.is_empty() { None } else { Some(path) }
}

fn split_parent_name(path: &str) -> Option<(&str, &str)> {
    let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
    if name.is_empty() {
        None
    } else {
        Some((parent, name))
    }
}

// ---------------------------------------------------------------------------
// Handle tables — also Arc<str> for zero-copy path sharing
// ---------------------------------------------------------------------------

struct HandleTable {
    file_handles: HashMap<u64, FileHandle>,
    dir_handles: HashMap<u64, DirHandle>,
    next_handle: u64,
}

struct FileHandle {
    path: Arc<str>,
    read_lease: Option<VfsReadLease>,
}

struct FileHandleSnapshot {
    path: Arc<str>,
    read_lease: Option<VfsReadLease>,
}

struct DirHandle {
    /// Each entry paired with its allocated inode.
    entries: Vec<(ReaddirEntry, u64)>,
}

impl HandleTable {
    fn new() -> Self {
        Self {
            file_handles: HashMap::new(),
            dir_handles: HashMap::new(),
            next_handle: 1,
        }
    }

    fn alloc_file(&mut self, path: Arc<str>, read_lease: Option<VfsReadLease>) -> u64 {
        let fh = self.next_handle;
        self.next_handle += 1;
        self.file_handles
            .insert(fh, FileHandle { path, read_lease });
        fh
    }

    fn alloc_dir(&mut self, entries: Vec<(ReaddirEntry, u64)>) -> u64 {
        let fh = self.next_handle;
        self.next_handle += 1;
        self.dir_handles.insert(fh, DirHandle { entries });
        fh
    }

    fn get_file_path(&self, fh: u64) -> Option<Arc<str>> {
        self.file_handles.get(&fh).map(|h| Arc::clone(&h.path))
    }

    fn get_file(&self, fh: u64) -> Option<FileHandleSnapshot> {
        self.file_handles.get(&fh).map(|h| FileHandleSnapshot {
            path: Arc::clone(&h.path),
            read_lease: h.read_lease.clone(),
        })
    }

    fn replace_file_read_lease(&mut self, fh: u64, read_lease: VfsReadLease) {
        if let Some(handle) = self.file_handles.get_mut(&fh) {
            handle.read_lease = Some(read_lease);
        }
    }

    fn clear_file_read_lease(&mut self, fh: u64) {
        if let Some(handle) = self.file_handles.get_mut(&fh) {
            handle.read_lease = None;
        }
    }

    fn get_dir_entries(&self, fh: u64) -> Option<&[(ReaddirEntry, u64)]> {
        self.dir_handles.get(&fh).map(|h| h.entries.as_slice())
    }

    fn release_file(&mut self, fh: u64) {
        self.file_handles.remove(&fh);
    }
    fn release_dir(&mut self, fh: u64) {
        self.dir_handles.remove(&fh);
    }
}

// ---------------------------------------------------------------------------
// CrabFs
// ---------------------------------------------------------------------------

/// FUSE filesystem backed by crab's resolver and engine.
pub struct CrabFs {
    resolver: Arc<FuseResolver>,
    engine: Arc<VfsEngine>,
    gitfile_content: Bytes,
    inodes: Arc<std::sync::RwLock<InodeTable>>,
    handles: std::sync::RwLock<HandleTable>,
    rt: Handle,
    /// Cached uid/gid — these never change during the process lifetime.
    uid: u32,
    gid: u32,
}

impl CrabFs {
    /// Create a new FUSE filesystem adapter.
    ///
    /// # Runtime sizing
    ///
    /// FUSE callbacks run on fuser's own thread pool. Each callback that
    /// needs async work (read, write, hydration) calls `rt.block_on(...)`
    /// to bridge into the tokio runtime identified by `rt`. Under heavy
    /// concurrent FUSE reads, many fuser threads can be blocked in
    /// `block_on` simultaneously, each waiting for tokio worker threads
    /// to poll the inner future and any tasks it spawns (hydration,
    /// prefetch, decompression). If the tokio runtime has too few worker
    /// threads, throughput suffers and in the worst case the user sees
    /// the mount become unresponsive while hydration workers wait their
    /// turn.
    ///
    /// **Recommended:** the runtime backing `rt` should have at least
    /// `max(4, num_cpus)` worker threads. Use
    /// `tokio::runtime::Builder::new_multi_thread().worker_threads(...)`
    /// rather than the current-thread runtime for any production mount.
    pub fn new(
        resolver: Arc<FuseResolver>,
        engine: Arc<VfsEngine>,
        git_dir: &str,
        rt: Handle,
    ) -> Self {
        let gitfile_content = Bytes::from(format!("gitdir: {git_dir}\n"));
        // SAFETY: getuid/getgid are always safe on Unix.
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        Self {
            resolver,
            engine,
            gitfile_content,
            inodes: Arc::new(std::sync::RwLock::new(InodeTable::new())),
            handles: std::sync::RwLock::new(HandleTable::new()),
            rt,
            uid,
            gid,
        }
    }

    /// Return a handle that can map live FUSE paths back to kernel inodes.
    pub fn invalidation_index(&self) -> FuseInvalidationIndex {
        FuseInvalidationIndex {
            inodes: Arc::clone(&self.inodes),
        }
    }

    // --- helpers ---

    /// Cheap Arc clone — no string copy on the hot path.
    fn inode_path(&self, ino: u64) -> Option<Arc<str>> {
        let inodes = self.inodes.read().unwrap_or_else(|e| {
            warn!("inodes RwLock was poisoned; recovering");
            e.into_inner()
        });
        inodes.get_path(ino)
    }

    fn exact_getattr(&self, path: &str) -> crate::core::error::Result<(u32, u64, NodeType, i64)> {
        let (mode, size, node_type, mtime) = self.resolver.getattr(path)?;
        if node_type != NodeType::File || size != 0 {
            return Ok((mode, size, node_type, mtime));
        }
        let size = self.engine.exact_file_size(path)?;
        Ok((mode, size, node_type, mtime))
    }

    fn handle_path(&self, fh: u64) -> Option<Arc<str>> {
        let handles = self.handles.read().unwrap_or_else(|e| {
            warn!("handles RwLock was poisoned; recovering");
            e.into_inner()
        });
        handles.get_file_path(fh)
    }

    fn file_handle(&self, fh: u64) -> Option<FileHandleSnapshot> {
        let handles = self.handles.read().unwrap_or_else(|e| {
            warn!("handles RwLock was poisoned; recovering");
            e.into_inner()
        });
        handles.get_file(fh)
    }

    fn replace_file_read_lease(&self, fh: u64, read_lease: VfsReadLease) {
        let mut handles = self.handles.write().unwrap_or_else(|e| {
            warn!("handles RwLock was poisoned; recovering");
            e.into_inner()
        });
        handles.replace_file_read_lease(fh, read_lease);
    }

    fn clear_file_read_lease(&self, fh: u64) {
        let mut handles = self.handles.write().unwrap_or_else(|e| {
            warn!("handles RwLock was poisoned; recovering");
            e.into_inner()
        });
        handles.clear_file_read_lease(fh);
    }

    fn child_path(&self, parent: u64, name: &OsStr) -> Option<String> {
        let name = name.to_str()?;
        let inodes = self.inodes.read().unwrap_or_else(|e| {
            warn!("inodes RwLock was poisoned; recovering");
            e.into_inner()
        });
        let parent_path = inodes.get_path(parent)?;
        if parent_path.is_empty() {
            Some(name.to_owned())
        } else {
            Some(format!("{parent_path}/{name}"))
        }
    }

    fn make_attr(
        &self,
        ino: u64,
        mode: u32,
        size: u64,
        node_type: NodeType,
        mtime: i64,
    ) -> FileAttr {
        let kind = node_type_to_fuse(node_type);
        let perm = (mode & 0o7777) as u16;
        let mtime_sys = unix_to_system_time(mtime);
        let blocks = size.div_ceil(512);
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks,
            atime: mtime_sys,
            mtime: mtime_sys,
            ctime: mtime_sys,
            crtime: mtime_sys,
            kind,
            perm,
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn gitfile_attr(&self) -> FileAttr {
        let size = self.gitfile_content.len() as u64;
        let now = SystemTime::now();
        FileAttr {
            ino: INodeNo(GITFILE_INO),
            size,
            blocks: 1,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind: FileType::RegularFile,
            perm: 0o444,
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }
}

fn node_type_to_fuse(nt: NodeType) -> FileType {
    match nt {
        NodeType::File => FileType::RegularFile,
        NodeType::Dir => FileType::Directory,
        NodeType::Symlink => FileType::Symlink,
    }
}

fn unix_to_system_time(secs: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH
    }
}

fn configure_readahead(config: &mut KernelConfig, target: u32) {
    match config.set_max_readahead(target) {
        Ok(previous) => {
            let configured = if previous > target {
                let _ = config.set_max_readahead(previous);
                previous
            } else {
                target
            };
            debug!(previous, configured, "configured FUSE max_readahead");
        }
        Err(max_supported) if max_supported > 0 => {
            if config.set_max_readahead(max_supported).is_ok() {
                debug!(
                    requested = target,
                    configured = max_supported,
                    "configured FUSE max_readahead at kernel cap"
                );
            }
        }
        Err(_) => {}
    }
}

// ---------------------------------------------------------------------------
// fuser::Filesystem implementation
// ---------------------------------------------------------------------------

impl Filesystem for CrabFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        configure_readahead(config, FUSE_TARGET_MAX_READAHEAD);
        if config
            .add_capabilities(InitFlags::FUSE_ATOMIC_O_TRUNC)
            .is_ok()
        {
            debug!("enabled FUSE atomic O_TRUNC handling");
        }
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let parent = u64::from(parent);

        if parent == ROOT_INO && name == ".git" {
            reply.entry(&GITFILE_TTL, &self.gitfile_attr(), Generation(0));
            return;
        }

        let Some(child_path) = self.child_path(parent, name) else {
            reply.error(Errno::ENOENT);
            return;
        };

        match self.exact_getattr(&child_path) {
            Ok((mode, size, node_type, mtime)) => {
                let ino = {
                    let Ok(mut inodes) = self.inodes.write() else {
                        reply.error(Errno::EIO);
                        return;
                    };
                    inodes.get_or_alloc(&child_path, node_type)
                };
                let attr = self.make_attr(ino, mode, size, node_type, mtime);
                reply.entry(&ENTRY_TTL, &attr, Generation(0));
            }
            Err(CrabError::NotFound { .. }) => reply.error(Errno::ENOENT),
            Err(e) => {
                warn!(path = %child_path, error = %e, "lookup failed");
                reply.error(Errno::EIO);
            }
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        if let Ok(mut inodes) = self.inodes.write() {
            inodes.forget(u64::from(ino), nlookup);
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FuseFileHandle>, reply: ReplyAttr) {
        let ino = u64::from(ino);

        if ino == GITFILE_INO {
            reply.attr(&GITFILE_TTL, &self.gitfile_attr());
            return;
        }

        if ino == ROOT_INO {
            let mtime = unix_to_system_time(self.resolver.commit_time());
            let attr = FileAttr {
                ino: INodeNo(ROOT_INO),
                size: 4096,
                blocks: 8,
                atime: mtime,
                mtime,
                ctime: mtime,
                crtime: mtime,
                kind: FileType::Directory,
                perm: 0o755,
                nlink: 2,
                uid: self.uid,
                gid: self.gid,
                rdev: 0,
                blksize: 4096,
                flags: 0,
            };
            reply.attr(&ENTRY_TTL, &attr);
            return;
        }

        let Some(path) = self.inode_path(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };

        match self.exact_getattr(&path) {
            Ok((mode, size, node_type, mtime)) => {
                let attr = self.make_attr(ino, mode, size, node_type, mtime);
                reply.attr(&ENTRY_TTL, &attr);
            }
            Err(CrabError::NotFound { .. }) => reply.error(Errno::ENOENT),
            Err(e) => {
                warn!(path = %path, error = %e, "getattr failed");
                reply.error(Errno::EIO);
            }
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FuseFileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let ino = u64::from(ino);

        let Some(path) = self.inode_path(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };

        // Ownership is not stored in the CoW overlay, so chown remains
        // unsupported even for writable mounts.
        if uid.is_some() || gid.is_some() {
            reply.error(Errno::EROFS);
            return;
        }

        if let Some(new_mode) = mode {
            match self.rt.block_on(self.engine.set_mode(&path, new_mode)) {
                Ok(()) => {}
                Err(CrabError::Forbidden { .. }) => {
                    reply.error(Errno::EROFS);
                    return;
                }
                Err(CrabError::NotFound { .. }) => {
                    reply.error(Errno::ENOENT);
                    return;
                }
                Err(e) => {
                    warn!(path = %path, error = %e, "set_mode failed");
                    reply.error(Errno::EIO);
                    return;
                }
            }
        }

        if let Some(new_size) = size {
            match self.rt.block_on(self.engine.truncate(&path, new_size)) {
                Ok(()) => {}
                Err(CrabError::Forbidden { .. }) => {
                    reply.error(Errno::EROFS);
                    return;
                }
                Err(e) => {
                    warn!(path = %path, error = %e, "truncate failed");
                    reply.error(Errno::EIO);
                    return;
                }
            }
        }

        if let Some(mtime_val) = mtime {
            let mtime_ns = match mtime_val {
                fuser::TimeOrNow::SpecificTime(t) => t
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos() as i64),
                fuser::TimeOrNow::Now => SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos() as i64),
            };
            if let Err(e) = self.rt.block_on(self.engine.set_mtime(&path, mtime_ns)) {
                match e {
                    CrabError::Forbidden { .. } => reply.error(Errno::EROFS),
                    CrabError::NotFound { .. } => reply.error(Errno::ENOENT),
                    other => {
                        warn!(path = %path, error = %other, "set_mtime failed");
                        reply.error(Errno::EIO);
                    }
                }
                return;
            }
        }

        match self.exact_getattr(&path) {
            Ok((mode, size, node_type, mt)) => {
                let attr = self.make_attr(ino, mode, size, node_type, mt);
                reply.attr(&ENTRY_TTL, &attr);
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let ino = u64::from(ino);

        let Some(path) = self.inode_path(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };

        match self.resolver.readdir(&path) {
            Ok(mut entries) => {
                if ino == ROOT_INO {
                    entries.insert(
                        0,
                        ReaddirEntry {
                            name: ".git".to_owned(),
                            node_type: NodeType::File,
                        },
                    );
                }

                // Allocate inodes for each entry via InodeTable so readdir
                // reports correct, unique inode numbers instead of a single
                // hardcoded value.
                let dir_entries: Vec<(ReaddirEntry, u64)> = {
                    let Ok(mut inodes) = self.inodes.write() else {
                        reply.error(Errno::EIO);
                        return;
                    };
                    entries
                        .into_iter()
                        .map(|e| {
                            let full_path = if path.is_empty() {
                                e.name.clone()
                            } else {
                                format!("{path}/{}", e.name)
                            };
                            let ino = inodes.get_or_alloc(&full_path, e.node_type);
                            (e, ino)
                        })
                        .collect()
                };

                // Speculative prefetch via tokio task (not std::thread).
                let engine = Arc::clone(&self.engine);
                let prefetch_path: Arc<str> = Arc::clone(&path);
                let prefetch_entries: Vec<String> = dir_entries
                    .iter()
                    .filter(|(e, _)| e.node_type == NodeType::File)
                    .map(|(e, _)| {
                        if prefetch_path.is_empty() {
                            e.name.clone()
                        } else {
                            format!("{prefetch_path}/{}", e.name)
                        }
                    })
                    .collect();

                if !prefetch_entries.is_empty() {
                    self.rt.spawn(async move {
                        if let Err(e) = engine.prefetch_dir(&prefetch_entries) {
                            debug!(error = %e, "prefetch_dir failed (non-fatal)");
                        }
                    });
                }

                let fh = {
                    let Ok(mut handles) = self.handles.write() else {
                        reply.error(Errno::EIO);
                        return;
                    };
                    handles.alloc_dir(dir_entries)
                };
                reply.opened(FuseFileHandle(fh), FopenFlags::empty());
            }
            Err(e) => {
                warn!(path = %path, error = %e, "opendir failed");
                reply.error(Errno::EIO);
            }
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FuseFileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let ino = u64::from(ino);
        let fh = u64::from(fh);
        let Ok(handles) = self.handles.read() else {
            reply.error(Errno::EIO);
            return;
        };
        let Some(entries) = handles.get_dir_entries(fh) else {
            reply.error(Errno::EBADF);
            return;
        };

        let start = usize::try_from(offset).unwrap_or(usize::MAX);

        // Resolve the correct parent inode for the ".." entry.
        let parent_ino = if ino == ROOT_INO {
            ROOT_INO // root's parent is root per FUSE convention
        } else {
            let inodes = self
                .inodes
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            inodes
                .get_path(ino)
                .and_then(|p| {
                    p.rfind('/').and_then(|pos| {
                        let parent = &p[..pos];
                        inodes.by_path.get(parent).copied()
                    })
                })
                .unwrap_or(ROOT_INO)
        };

        let virtual_entries = [
            (ino, FileType::Directory, "."),
            (parent_ino, FileType::Directory, ".."),
        ];

        for (i, &(entry_ino, kind, name)) in virtual_entries.iter().enumerate() {
            if i < start {
                continue;
            }
            if reply.add(INodeNo(entry_ino), (i + 1) as u64, kind, name) {
                reply.ok();
                return;
            }
        }

        let real_offset = start.saturating_sub(2);
        for (i, (entry, entry_ino)) in entries.iter().enumerate().skip(real_offset) {
            let fuse_offset = (i + 3) as u64;
            let kind = node_type_to_fuse(entry.node_type);
            if reply.add(INodeNo(*entry_ino), fuse_offset, kind, &entry.name) {
                reply.ok();
                return;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FuseFileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        if let Ok(mut handles) = self.handles.write() {
            handles.release_dir(u64::from(fh));
        }
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let ino = u64::from(ino);

        let Some(path) = self.inode_path(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if open_requests_truncate(flags) {
            match self.rt.block_on(self.engine.truncate(&path, 0)) {
                Ok(()) => {}
                Err(CrabError::Forbidden { .. }) => {
                    reply.error(Errno::EROFS);
                    return;
                }
                Err(CrabError::NotFound { .. }) => {
                    reply.error(Errno::ENOENT);
                    return;
                }
                Err(e) => {
                    warn!(path = %path, error = %e, "open truncate failed");
                    reply.error(Errno::EIO);
                    return;
                }
            }
        }
        let read_lease = if ino == GITFILE_INO || !open_allows_read(flags) {
            None
        } else {
            match self.engine.open_read(&path) {
                Ok(lease) => Some(lease),
                Err(CrabError::NotFound { .. }) => {
                    reply.error(Errno::ENOENT);
                    return;
                }
                Err(e) => {
                    warn!(path = %path, error = %e, "open read lease failed");
                    reply.error(Errno::EIO);
                    return;
                }
            }
        };
        let fh = {
            let Ok(mut handles) = self.handles.write() else {
                reply.error(Errno::EIO);
                return;
            };
            handles.alloc_file(path, read_lease)
        };
        reply.opened(FuseFileHandle(fh), fopen_flags(flags));
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FuseFileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let ino = u64::from(ino);
        let fh = u64::from(fh);

        if ino == GITFILE_INO {
            let content = &self.gitfile_content;
            let start = (offset as usize).min(content.len());
            let end = (start + size as usize).min(content.len());
            reply.data(&content[start..end]);
            return;
        }

        let Some(handle) = self.file_handle(fh) else {
            reply.error(Errno::EBADF);
            return;
        };
        let path = handle.path;

        let read = self.rt.block_on(async {
            let lease = match handle.read_lease {
                Some(lease) => lease,
                None => self.engine.open_read(&path)?,
            };
            match self.engine.read_at(&lease, offset, size).await {
                Ok(data) => Ok((data, None)),
                Err(error) if VfsEngine::is_stale_read_lease_error(&error) => {
                    let retry_lease = self.engine.open_read(&path)?;
                    let data = self.engine.read_at(&retry_lease, offset, size).await?;
                    Ok((data, Some(retry_lease)))
                }
                Err(error) => Err(error),
            }
        });
        match read {
            Ok((data, replacement_lease)) => {
                if let Some(lease) = replacement_lease {
                    self.replace_file_read_lease(fh, lease);
                }
                trace!(path = %path, offset, size, bytes = data.len(), "fuse read ok");
                reply.data(&data);
            }
            Err(CrabError::NotFound { .. }) => reply.error(Errno::ENOENT),
            Err(e) => {
                warn!(path = %path, error = %e, "read failed");
                reply.error(Errno::EIO);
            }
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let ino = u64::from(ino);

        let Some(path) = self.inode_path(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };

        match self.resolver.resolve_path(&path) {
            Ok(ResolvedNode::Base(base)) => {
                if let Some(ref oid) = base.object_oid {
                    match self.engine.read_symlink_target(oid) {
                        Ok(target) => reply.data(target.as_bytes()),
                        Err(e) => {
                            warn!(path = %path, oid = %oid, error = %e, "readlink failed");
                            reply.error(Errno::EIO);
                        }
                    }
                } else {
                    reply.error(Errno::ENOENT);
                }
            }
            Ok(ResolvedNode::Overlay(entry)) if entry.node_type == NodeType::Symlink => {
                let Some(ref ov) = *self.engine.overlay() else {
                    reply.error(Errno::ENOENT);
                    return;
                };
                let Some(backing) = ov.get_backing_path(&path) else {
                    reply.error(Errno::ENOENT);
                    return;
                };
                match std::fs::read(&backing) {
                    Ok(target) => reply.data(&target),
                    Err(e) => {
                        warn!(path = %path, backing = %backing.display(), error = %e, "overlay readlink failed");
                        reply.error(Errno::EIO);
                    }
                }
            }
            Ok(_) | Err(CrabError::NotFound { .. }) => reply.error(Errno::ENOENT),
            Err(e) => {
                warn!(path = %path, error = %e, "readlink resolve failed");
                reply.error(Errno::EIO);
            }
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FuseFileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        if let Ok(mut handles) = self.handles.write() {
            handles.release_file(u64::from(fh));
        }
        reply.ok();
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FuseFileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let fh = u64::from(fh);

        let Some(path) = self.handle_path(fh) else {
            reply.error(Errno::EBADF);
            return;
        };
        match self.rt.block_on(self.engine.write(&path, offset, data)) {
            Ok(n) => {
                self.clear_file_read_lease(fh);
                reply.written(n as u32);
            }
            Err(CrabError::Forbidden { .. }) => reply.error(Errno::EROFS),
            Err(e) => {
                warn!(path = %path, error = %e, "write failed");
                reply.error(Errno::EIO);
            }
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let parent = u64::from(parent);

        let Some(child_path) = self.child_path(parent, name) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.rt.block_on(self.engine.create(&child_path, mode)) {
            Ok(_entry) => {
                let arc_path: Arc<str> = Arc::from(child_path.as_str());
                let ino = {
                    let Ok(mut inodes) = self.inodes.write() else {
                        reply.error(Errno::EIO);
                        return;
                    };
                    inodes.get_or_alloc(&child_path, NodeType::File)
                };
                let read_lease = if raw_open_flags_allow_read(flags) {
                    match self.engine.open_read(&child_path) {
                        Ok(lease) => Some(lease),
                        Err(error) => {
                            warn!(
                                path = %child_path,
                                error = %error,
                                "create read lease failed"
                            );
                            None
                        }
                    }
                } else {
                    None
                };
                let fh = {
                    let Ok(mut handles) = self.handles.write() else {
                        reply.error(Errno::EIO);
                        return;
                    };
                    handles.alloc_file(arc_path, read_lease)
                };
                let attr = self.make_attr(ino, mode | 0o100_000, 0, NodeType::File, now_unix());
                reply.created(
                    &ENTRY_TTL,
                    &attr,
                    Generation(0),
                    FuseFileHandle(fh),
                    FopenFlags::FOPEN_DIRECT_IO,
                );
            }
            Err(CrabError::Forbidden { .. }) => reply.error(Errno::EROFS),
            Err(e) => {
                warn!(error = %e, "create failed");
                reply.error(Errno::EIO);
            }
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let parent = u64::from(parent);

        let Some(child_path) = self.child_path(parent, name) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.rt.block_on(self.engine.unlink(&child_path)) {
            Ok(()) => reply.ok(),
            Err(CrabError::Forbidden { .. }) => reply.error(Errno::EROFS),
            Err(e) => {
                warn!(path = %child_path, error = %e, "unlink failed");
                reply.error(Errno::EIO);
            }
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let parent = u64::from(parent);

        let Some(child_path) = self.child_path(parent, name) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.rt.block_on(self.engine.mkdir(&child_path, mode)) {
            Ok(()) => {
                let ino = {
                    let Ok(mut inodes) = self.inodes.write() else {
                        reply.error(Errno::EIO);
                        return;
                    };
                    inodes.get_or_alloc(&child_path, NodeType::Dir)
                };
                let attr = self.make_attr(ino, mode | 0o040_000, 4096, NodeType::Dir, now_unix());
                reply.entry(&ENTRY_TTL, &attr, Generation(0));
            }
            Err(CrabError::Forbidden { .. }) => reply.error(Errno::EROFS),
            Err(e) => {
                warn!(error = %e, "mkdir failed");
                reply.error(Errno::EIO);
            }
        }
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        link: &std::path::Path,
        reply: ReplyEntry,
    ) {
        let parent = u64::from(parent);

        let Some(name_str) = name.to_str() else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(child_path) = self.child_path(parent, OsStr::new(name_str)) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(target) = link.to_str() else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self
            .rt
            .block_on(self.engine.create_symlink(&child_path, target, 0o777))
        {
            Ok(entry) => {
                let ino = {
                    let Ok(mut inodes) = self.inodes.write() else {
                        reply.error(Errno::EIO);
                        return;
                    };
                    inodes.get_or_alloc(&child_path, NodeType::Symlink)
                };
                let attr = self.make_attr(
                    ino,
                    entry.mode,
                    entry.size,
                    NodeType::Symlink,
                    entry.mtime_ns,
                );
                reply.entry(&ENTRY_TTL, &attr, Generation(0));
            }
            Err(CrabError::Forbidden { .. }) => reply.error(Errno::EROFS),
            Err(e) => {
                warn!(error = %e, "symlink failed");
                reply.error(Errno::EIO);
            }
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let parent = u64::from(parent);

        let Some(child_path) = self.child_path(parent, name) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.rt.block_on(self.engine.rmdir(&child_path)) {
            Ok(()) => reply.ok(),
            Err(CrabError::Forbidden { path }) if path.starts_with("directory not empty:") => {
                reply.error(Errno::ENOTEMPTY);
            }
            Err(CrabError::Forbidden { .. }) => reply.error(Errno::EROFS),
            Err(e) => {
                warn!(path = %child_path, error = %e, "rmdir failed");
                reply.error(Errno::EIO);
            }
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let parent = u64::from(parent);
        let newparent = u64::from(newparent);

        let Some(old_path) = self.child_path(parent, name) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(new_path) = self.child_path(newparent, newname) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.rt.block_on(self.engine.rename(&old_path, &new_path)) {
            Ok(()) => {
                if let Ok(mut inodes) = self.inodes.write() {
                    inodes.rename_path(&old_path, &new_path);
                }
                reply.ok();
            }
            Err(CrabError::Forbidden { .. }) => reply.error(Errno::EROFS),
            Err(e) => {
                warn!(old_path, new_path, error = %e, "rename failed");
                reply.error(Errno::EIO);
            }
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FuseFileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        let fh = u64::from(fh);

        // Best-effort: sync the overlay backing file if one is open.
        if let Some(ref ov) = *self.engine.overlay()
            && let Some(path) = self.handle_path(fh)
            && let Some(backing) = ov.get_backing_path(&path)
            && let Ok(file) = std::fs::File::open(&backing)
        {
            let _ = file.sync_all();
        }
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FuseFileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        let fh = u64::from(fh);

        // Best-effort: sync the overlay backing file if one is open.
        if let Some(ref ov) = *self.engine.overlay()
            && let Some(path) = self.handle_path(fh)
            && let Some(backing) = ov.get_backing_path(&path)
            && let Ok(file) = std::fs::File::open(&backing)
        {
            let _ = file.sync_all();
        }
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        const BLOCK_SIZE: u32 = 4096;
        const TOTAL_BLOCKS: u64 = 1024 * 1024 * 1024;
        reply.statfs(
            TOTAL_BLOCKS,
            TOTAL_BLOCKS,
            TOTAL_BLOCKS,
            1_000_000_000,
            1_000_000_000,
            BLOCK_SIZE,
            255,
            0,
        );
    }

    fn getxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        reply.error(Errno::ENOSYS);
    }

    fn setxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::ENOSYS);
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, _size: u32, reply: ReplyXattr) {
        reply.error(Errno::ENOSYS);
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn fopen_flags(flags: OpenFlags) -> FopenFlags {
    match flags.acc_mode() {
        OpenAccMode::O_RDONLY => FopenFlags::empty(),
        OpenAccMode::O_WRONLY | OpenAccMode::O_RDWR => FopenFlags::FOPEN_DIRECT_IO,
    }
}

fn open_allows_read(flags: OpenFlags) -> bool {
    matches!(
        flags.acc_mode(),
        OpenAccMode::O_RDONLY | OpenAccMode::O_RDWR
    )
}

fn raw_open_flags_allow_read(flags: i32) -> bool {
    let acc_mode = flags & libc::O_ACCMODE;
    acc_mode == libc::O_RDONLY || acc_mode == libc::O_RDWR
}

fn open_requests_truncate(flags: OpenFlags) -> bool {
    flags.0 & libc::O_TRUNC != 0
        && matches!(
            flags.acc_mode(),
            OpenAccMode::O_WRONLY | OpenAccMode::O_RDWR
        )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use crate::engine::ReadSourceKey;

    fn test_lease(path: &str) -> VfsReadLease {
        VfsReadLease::for_test(ReadSourceKey::BaseEmpty {
            generation: 1,
            overlay_version: 0,
            path: path.to_owned(),
        })
    }

    #[test]
    fn inode_rename_updates_descendant_paths() {
        let mut inodes = InodeTable::new();
        let dir = inodes.get_or_alloc("dir-before", NodeType::Dir);
        let child = inodes.get_or_alloc("dir-before/nested/note.txt", NodeType::File);

        inodes.rename_path("dir-before", "dir-after");

        assert_eq!(inodes.get_path(dir).unwrap().as_ref(), "dir-after");
        assert_eq!(
            inodes.get_path(child).unwrap().as_ref(),
            "dir-after/nested/note.txt"
        );
    }

    #[test]
    fn invalidation_index_targets_cached_nested_entry() {
        let inodes = Arc::new(std::sync::RwLock::new(InodeTable::new()));
        let (dir, child) = {
            let mut table = inodes.write().unwrap();
            let dir = table.get_or_alloc("models", NodeType::Dir);
            let child = table.get_or_alloc("models/delete-me.bin", NodeType::File);
            (dir, child)
        };
        let index = FuseInvalidationIndex { inodes };

        let target = index.target_for_path("models/delete-me.bin").unwrap();

        assert_eq!(target.parent.0, dir);
        assert_eq!(target.name, OsString::from("delete-me.bin"));
        assert_eq!(target.inode.map(|ino| ino.0), Some(child));
    }

    #[test]
    fn invalidation_index_targets_negative_nested_entry() {
        let inodes = Arc::new(std::sync::RwLock::new(InodeTable::new()));
        let dir = {
            let mut table = inodes.write().unwrap();
            table.get_or_alloc("models", NodeType::Dir)
        };
        let index = FuseInvalidationIndex { inodes };

        let target = index.target_for_path("models/missing.bin").unwrap();

        assert_eq!(target.parent.0, dir);
        assert_eq!(target.name, OsString::from("missing.bin"));
        assert!(target.inode.is_none());
    }

    #[test]
    fn file_handles_store_replace_and_clear_read_leases() {
        let mut handles = HandleTable::new();
        let original = test_lease("model.bin");
        let fh = handles.alloc_file(Arc::from("model.bin"), Some(original.clone()));

        let opened = handles.get_file(fh).unwrap();
        assert_eq!(opened.path.as_ref(), "model.bin");
        assert_eq!(opened.read_lease.as_ref().unwrap().key(), original.key());

        let replacement = test_lease("model-after-write.bin");
        handles.replace_file_read_lease(fh, replacement.clone());
        let replaced = handles.get_file(fh).unwrap();
        assert_eq!(
            replaced.read_lease.as_ref().unwrap().key(),
            replacement.key()
        );

        handles.clear_file_read_lease(fh);
        let cleared = handles.get_file(fh).unwrap();
        assert!(cleared.read_lease.is_none());
    }

    #[test]
    fn read_only_open_uses_default_cache_flags() {
        assert_eq!(fopen_flags(OpenFlags(libc::O_RDONLY)), FopenFlags::empty());
    }

    #[test]
    fn only_read_capable_open_flags_preopen_read_lease() {
        assert!(open_allows_read(OpenFlags(libc::O_RDONLY)));
        assert!(open_allows_read(OpenFlags(libc::O_RDWR)));
        assert!(!open_allows_read(OpenFlags(libc::O_WRONLY)));

        assert!(raw_open_flags_allow_read(libc::O_RDONLY));
        assert!(raw_open_flags_allow_read(libc::O_RDWR));
        assert!(!raw_open_flags_allow_read(libc::O_WRONLY));
    }

    #[test]
    fn write_capable_open_uses_direct_io() {
        assert_eq!(
            fopen_flags(OpenFlags(libc::O_WRONLY)),
            FopenFlags::FOPEN_DIRECT_IO
        );
        assert_eq!(
            fopen_flags(OpenFlags(libc::O_RDWR)),
            FopenFlags::FOPEN_DIRECT_IO
        );
    }

    #[test]
    fn open_truncate_requires_writable_access() {
        assert!(open_requests_truncate(OpenFlags(
            libc::O_WRONLY | libc::O_TRUNC
        )));
        assert!(open_requests_truncate(OpenFlags(
            libc::O_RDWR | libc::O_TRUNC
        )));
        assert!(!open_requests_truncate(OpenFlags(
            libc::O_RDONLY | libc::O_TRUNC
        )));
        assert!(!open_requests_truncate(OpenFlags(libc::O_WRONLY)));
    }
}
