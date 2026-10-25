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
use tracing::{debug, warn};

use crate::core::error::CrabError;
use crate::engine::{VfsEngine, VfsOpenFile};
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
const GITFILE_TTL: Duration = Duration::from_mins(1);

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
    opens: u64,
    linked: bool,
    file: Option<Arc<OpenNode>>,
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
                opens: 0,
                linked: true,
                file: None,
            },
        );
        by_path.insert(root_path, ROOT_INO);

        by_ino.insert(
            GITFILE_INO,
            InodeRef {
                path: Arc::clone(&git_path),
                node_type: NodeType::File,
                refcnt: 1,
                opens: 0,
                linked: true,
                file: None,
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
                opens: 0,
                linked: true,
                file: None,
            },
        );
        self.by_path.insert(arc_path, ino);
        ino
    }

    /// Return a cheap Arc clone of the path — no string copy.
    fn get_path(&self, ino: u64) -> Option<Arc<str>> {
        self.by_ino
            .get(&ino)
            .filter(|r| r.linked)
            .map(|r| Arc::clone(&r.path))
    }

    fn ino_for_path(&self, path: &str) -> Option<u64> {
        self.by_path.get(path).copied()
    }

    fn rename_path(&mut self, old_path: &str, new_path: &str) {
        if old_path == new_path {
            return;
        }
        self.detach_path(new_path);
        let old_prefix = format!("{old_path}/");
        let updates = self
            .by_ino
            .iter()
            .filter_map(|(ino, inode_ref)| {
                if !inode_ref.linked {
                    return None;
                }
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
                if let Some(node) = &inode_ref.file {
                    node.set_path(Some(Arc::clone(&arc_path)));
                }
                self.by_path.insert(arc_path, ino);
            }
        }
    }

    fn detach_inode(&mut self, ino: u64) {
        if let Some(inode) = self.by_ino.get_mut(&ino) {
            if self.by_path.get(&inode.path) == Some(&ino) {
                self.by_path.remove(&inode.path);
            }
            inode.linked = false;
            if let Some(node) = &inode.file {
                node.set_path(None);
            }
        }
    }

    fn detach_path(&mut self, path: &str) {
        let prefix = format!("{path}/");
        for inode in self.by_ino.values_mut() {
            if inode.linked && (inode.path.as_ref() == path || inode.path.starts_with(&prefix)) {
                self.by_path.remove(&inode.path);
                inode.linked = false;
                if let Some(node) = &inode.file {
                    node.set_path(None);
                }
            }
        }
    }

    fn open_node(&mut self, ino: u64) -> Option<Arc<OpenNode>> {
        let inode = self.by_ino.get_mut(&ino)?;
        let node = match &inode.file {
            Some(node) => Arc::clone(node),
            None if inode.linked => Arc::new(OpenNode {
                path: std::sync::RwLock::new(Some(Arc::clone(&inode.path))),
                file: tokio::sync::Mutex::new(None),
            }),
            None => return None,
        };
        inode.file = Some(Arc::clone(&node));
        inode.opens += 1;
        Some(node)
    }

    fn release_open(&mut self, ino: u64) {
        if let Some(inode) = self.by_ino.get_mut(&ino) {
            inode.opens = inode.opens.saturating_sub(1);
            if inode.opens == 0 {
                inode.file = None;
            }
        }
        self.forget(ino, 0);
    }

    fn forget(&mut self, ino: u64, nlookup: u64) {
        if ino == ROOT_INO || ino == GITFILE_INO {
            return;
        }
        if let Some(r) = self.by_ino.get_mut(&ino) {
            r.refcnt = r.refcnt.saturating_sub(nlookup);
            if r.refcnt == 0 && r.opens == 0 {
                let path = Arc::clone(&r.path);
                self.by_ino.remove(&ino);
                if self.by_path.get(&path) == Some(&ino) {
                    self.by_path.remove(&path);
                }
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

struct OpenNode {
    path: std::sync::RwLock<Option<Arc<str>>>,
    // All descriptors of an inode share promotion and content changes. Take
    // this lock before the engine publication gate; controls never take it.
    file: tokio::sync::Mutex<Option<VfsOpenFile>>,
}

impl OpenNode {
    fn path(&self) -> Option<Arc<str>> {
        self.path
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    fn set_path(&self, path: Option<Arc<str>>) {
        *self
            .path
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = path;
    }
}

struct OpenNodePin {
    ino: u64,
    node: Arc<OpenNode>,
    inodes: Arc<std::sync::RwLock<InodeTable>>,
}

impl Drop for OpenNodePin {
    fn drop(&mut self) {
        self.inodes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .release_open(self.ino);
    }
}

#[derive(Clone)]
struct FileHandle {
    pin: Arc<OpenNodePin>,
    readable: bool,
    writable: bool,
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

    fn alloc_file(&mut self, pin: Arc<OpenNodePin>, readable: bool, writable: bool) -> u64 {
        let fh = self.next_handle;
        self.next_handle += 1;
        self.file_handles.insert(
            fh,
            FileHandle {
                pin,
                readable,
                writable,
            },
        );
        fh
    }

    fn alloc_dir(&mut self, entries: Vec<(ReaddirEntry, u64)>) -> u64 {
        let fh = self.next_handle;
        self.next_handle += 1;
        self.dir_handles.insert(fh, DirHandle { entries });
        fh
    }

    fn get_file(&self, ino: u64, fh: u64) -> Option<FileHandle> {
        self.file_handles
            .get(&fh)
            .filter(|handle| handle.pin.ino == ino)
            .cloned()
    }

    fn get_dir_entries(&self, fh: u64) -> Option<&[(ReaddirEntry, u64)]> {
        self.dir_handles
            .get(&fh)
            .map(|handle| handle.entries.as_slice())
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
    namespace: tokio::sync::RwLock<()>,
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
            namespace: tokio::sync::RwLock::new(()),
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

    fn file_handle(&self, ino: u64, fh: u64) -> Option<FileHandle> {
        self.handles
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_file(ino, fh)
    }

    fn pin_file(&self, ino: u64) -> crate::Result<Arc<OpenNodePin>> {
        let node = self
            .inodes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .open_node(ino)
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?;
        Ok(Arc::new(OpenNodePin {
            ino,
            node,
            inodes: Arc::clone(&self.inodes),
        }))
    }

    async fn prepare_file(&self, pin: &OpenNodePin, read: bool, write: bool) -> crate::Result<()> {
        let mut state = pin.node.file.lock().await;
        let path = pin.node.path();
        if state.is_none() {
            let path = path
                .as_deref()
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?;
            *state = Some(self.engine.open_file(path, read, write).await?);
        } else if let Some(file) = state.as_mut() {
            let _view = self.engine.namespace_read().await;
            let path = self.linked_path(pin, file)?;
            let path = path
                .as_deref()
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?;
            file.enable_access(&self.engine, Some(path), read, write)
                .await?;
        }
        Ok(())
    }

    fn linked_path(
        &self,
        pin: &OpenNodePin,
        file: &VfsOpenFile,
    ) -> crate::Result<Option<Arc<str>>> {
        let path = pin.node.path();
        if let Some(path) = &path
            && !file.matches_path(&self.engine, path)?
        {
            self.inodes
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .detach_inode(pin.ino);
            pin.node.set_path(None);
            return Ok(None);
        }
        Ok(path)
    }

    fn open_pin(&self, ino: u64) -> Option<Arc<OpenNodePin>> {
        let mut inodes = self
            .inodes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inodes.by_ino.get(&ino)?.file.as_ref()?;
        let node = inodes.open_node(ino)?;
        // Metadata operations retain the inode through a concurrent final
        // release/forget, rather than reattaching content through a reused name.
        Some(Arc::new(OpenNodePin {
            ino,
            node,
            inodes: Arc::clone(&self.inodes),
        }))
    }

    async fn open_node_attr(&self, ino: u64, node: &OpenNode) -> crate::Result<FileAttr> {
        let _namespace = self.namespace.read().await;
        let state = node.file.lock().await;
        let _view = self.engine.namespace_read().await;
        let file = state
            .as_ref()
            .ok_or_else(|| CrabError::Internal("open inode has no content".into()))?;
        let mut path = node.path();
        if let Some(current) = &path
            && !file.matches_path(&self.engine, current)?
        {
            self.inodes
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .detach_inode(ino);
            node.set_path(None);
            path = None;
        }
        let (mode, size, kind, mtime) = file.attributes()?;
        let (mode, mtime) = match path.as_deref() {
            Some(path) => {
                let (mode, _, _, mtime) = self.engine.getattr(path)?;
                (mode, mtime)
            }
            None => (mode, mtime),
        };
        let mut attr = self.make_attr(ino, mode, size, kind, mtime);
        if path.is_none() {
            attr.nlink = 0;
        }
        Ok(attr)
    }

    fn reply_sync(&self, ino: u64, fh: u64, datasync: bool, reply: ReplyEmpty) {
        let Some(handle) = self.file_handle(ino, fh) else {
            reply.error(Errno::EBADF);
            return;
        };
        let synced = self.rt.block_on(async {
            let state = handle.pin.node.file.lock().await;
            match state.as_ref() {
                Some(file) => self.engine.sync_open_file(file, datasync),
                None if handle.pin.ino == GITFILE_INO => Ok(()),
                None => Err(CrabError::Internal("open inode has no content".into())),
            }
        });
        match synced {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(file_errno(&error)),
        }
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
        // Writable mounts use direct I/O to keep descriptors coherent through
        // COW. Linux requires this capability for shared mmap of those files.
        #[cfg(target_os = "linux")]
        if config
            .add_capabilities(InitFlags::FUSE_DIRECT_IO_ALLOW_MMAP)
            .is_ok()
        {
            debug!("enabled shared mmap for FUSE direct I/O files");
        }
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

        let found = self.rt.block_on(async {
            let _namespace = self.namespace.read().await;
            let child_path = self
                .child_path(parent, name)
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?;
            let existing = {
                let inodes = self
                    .inodes
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                inodes.ino_for_path(&child_path).and_then(|ino| {
                    inodes
                        .by_ino
                        .get(&ino)
                        .and_then(|inode| inode.file.clone())
                        .map(|node| (ino, node))
                })
            };
            let state = match &existing {
                Some((_, node)) => Some(node.file.lock().await),
                None => None,
            };
            let _view = self.engine.namespace_read().await;
            let attrs = self.engine.getattr(&child_path)?;
            if let (Some((ino, _)), Some(state)) = (&existing, &state)
                && let Some(file) = state.as_ref()
                && !file.matches_path(&self.engine, &child_path)?
            {
                self.inodes
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .detach_inode(*ino);
            }
            let ino = self
                .inodes
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_or_alloc(&child_path, attrs.2);
            Ok::<_, CrabError>((ino, attrs))
        });
        match found {
            Ok((ino, (mode, size, kind, mtime))) => {
                let attr = self.make_attr(ino, mode, size, kind, mtime);
                reply.entry(&ENTRY_TTL, &attr, Generation(0));
            }
            Err(error) => reply.error(file_errno(&error)),
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        if let Ok(mut inodes) = self.inodes.write() {
            inodes.forget(u64::from(ino), nlookup);
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FuseFileHandle>, reply: ReplyAttr) {
        let ino = u64::from(ino);
        let handle = match fh {
            Some(fh) => match self.file_handle(ino, u64::from(fh)) {
                Some(handle) => Some(handle),
                None => {
                    reply.error(Errno::EBADF);
                    return;
                }
            },
            None => None,
        };

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

        if let Some(pin) = handle
            .map(|handle| handle.pin)
            .or_else(|| self.open_pin(ino))
        {
            match self.rt.block_on(self.open_node_attr(ino, &pin.node)) {
                Ok(attr) => reply.attr(&ENTRY_TTL, &attr),
                Err(error) => reply.error(file_errno(&error)),
            }
            return;
        }

        let _namespace = self.rt.block_on(self.namespace.read());
        let Some(path) = self.inode_path(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };

        match self.engine.getattr(&path) {
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
        fh: Option<FuseFileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let ino = u64::from(ino);
        // Serialize the open-inode check with open and namespace changes. An
        // unopened file must not gain a captured reader halfway through truncate.
        let namespace = self.rt.block_on(self.namespace.write());
        let handle = match fh {
            Some(fh) => match self.file_handle(ino, u64::from(fh)) {
                Some(handle) if size.is_none() || handle.writable => Some(handle),
                _ => {
                    reply.error(Errno::EBADF);
                    return;
                }
            },
            None => None,
        };
        if let Some(pin) = handle
            .map(|handle| handle.pin)
            .or_else(|| self.open_pin(ino))
        {
            if uid.is_some() || gid.is_some() {
                reply.error(Errno::EROFS);
                return;
            }
            if ino == GITFILE_INO {
                reply.error(Errno::EROFS);
                return;
            }
            let changed = self.rt.block_on(async {
                if size.is_some() && fh.is_none() {
                    self.prepare_file(&pin, false, true).await?;
                }
                let mut state = pin.node.file.lock().await;
                let file = state
                    .as_mut()
                    .ok_or_else(|| CrabError::Internal("open inode has no content".into()))?;
                let path = self.linked_path(&pin, file)?;
                if let Some(mode) = mode {
                    self.engine
                        .set_open_file_mode(file, path.as_deref(), mode)
                        .await?;
                }
                if let Some(size) = size {
                    self.engine
                        .truncate_open_file(file, path.as_deref(), size)
                        .await?;
                }
                if let Some(mtime) = mtime {
                    let ns = match mtime {
                        fuser::TimeOrNow::SpecificTime(time) => {
                            match time.duration_since(UNIX_EPOCH) {
                                Ok(duration) => i64::try_from(duration.as_nanos())
                                    .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?,
                                Err(error) => -i64::try_from(error.duration().as_nanos())
                                    .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?,
                            }
                        }
                        fuser::TimeOrNow::Now => std::time::SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map_or(0, |time| time.as_nanos() as i64),
                    };
                    self.engine
                        .set_open_file_mtime(file, path.as_deref(), ns)
                        .await?;
                }
                Ok::<_, CrabError>(())
            });
            drop(namespace);
            match changed {
                Ok(()) => match self.rt.block_on(self.open_node_attr(ino, &pin.node)) {
                    Ok(attr) => reply.attr(&ENTRY_TTL, &attr),
                    Err(error) => reply.error(file_errno(&error)),
                },
                Err(error) => reply.error(file_errno(&error)),
            }
            return;
        }

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

        match self.engine.getattr(&path) {
            Ok((mode, size, node_type, mt)) => {
                let attr = self.make_attr(ino, mode, size, node_type, mt);
                reply.attr(&ENTRY_TTL, &attr);
            }
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let ino = u64::from(ino);
        let _namespace = self.rt.block_on(self.namespace.read());

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
        let readable = open_allows_read(flags);
        let writable = open_allows_write(flags);
        if ino == GITFILE_INO && writable {
            reply.error(Errno::EROFS);
            return;
        }
        let opened = self.rt.block_on(async {
            let _namespace = self.namespace.read().await;
            let pin = self.pin_file(ino)?;
            if ino != GITFILE_INO {
                self.prepare_file(&pin, readable, writable).await?;
                if open_requests_truncate(flags) {
                    let mut state = pin.node.file.lock().await;
                    let file = state
                        .as_mut()
                        .ok_or_else(|| CrabError::Internal("open inode has no content".into()))?;
                    let path = self.linked_path(&pin, file)?;
                    self.engine
                        .truncate_open_file(file, path.as_deref(), 0)
                        .await?;
                }
            }
            Ok::<_, CrabError>(pin)
        });
        match opened {
            Ok(pin) => {
                let fh = self
                    .handles
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .alloc_file(pin, readable, writable);
                let cache = if self.engine.overlay().is_some() {
                    FopenFlags::FOPEN_DIRECT_IO
                } else {
                    fopen_flags(flags)
                };
                reply.opened(FuseFileHandle(fh), cache);
            }
            Err(error) => reply.error(file_errno(&error)),
        }
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

        let Some(handle) = self.file_handle(ino, fh) else {
            reply.error(Errno::EBADF);
            return;
        };
        if !handle.readable {
            reply.error(Errno::EBADF);
            return;
        }

        if ino == GITFILE_INO {
            let content = &self.gitfile_content;
            let start = (offset as usize).min(content.len());
            let end = (start + size as usize).min(content.len());
            reply.data(&content[start..end]);
            return;
        }

        let read = self.rt.block_on(async {
            let state = handle.pin.node.file.lock().await;
            let file = state
                .as_ref()
                .ok_or_else(|| CrabError::Internal("open inode has no content".into()))?;
            self.engine.read_open_file(file, offset, size).await
        });
        match read {
            Ok(bytes) => reply.data(&bytes),
            Err(error) => reply.error(file_errno(&error)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let ino = u64::from(ino);
        let _namespace = self.rt.block_on(self.namespace.read());

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
        ino: INodeNo,
        fh: FuseFileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        if self.file_handle(u64::from(ino), u64::from(fh)).is_none() {
            reply.error(Errno::EBADF);
            return;
        }
        if let Ok(mut handles) = self.handles.write() {
            handles.release_file(u64::from(fh));
        }
        reply.ok();
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FuseFileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let fh = u64::from(fh);

        let Some(handle) = self.file_handle(u64::from(ino), fh) else {
            reply.error(Errno::EBADF);
            return;
        };
        if !handle.writable {
            reply.error(Errno::EBADF);
            return;
        }
        let written = self.rt.block_on(async {
            let _namespace = self.namespace.read().await;
            let mut state = handle.pin.node.file.lock().await;
            let file = state
                .as_mut()
                .ok_or_else(|| CrabError::Internal("open inode has no content".into()))?;
            let path = self.linked_path(&handle.pin, file)?;
            self.engine
                .write_open_file(file, path.as_deref(), offset, data)
                .await
        });
        match written {
            Ok(size) => reply.written(size as u32),
            Err(error) => reply.error(file_errno(&error)),
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
        let readable = raw_open_flags_allow_read(flags);
        let writable = open_allows_write(OpenFlags(flags));
        let created = self.rt.block_on(async {
            let _namespace = self.namespace.write().await;
            let child_path = self
                .child_path(parent, name)
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?;
            let entry = self.engine.create(&child_path, mode).await?;
            let ino = {
                let mut inodes = self
                    .inodes
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                inodes.detach_path(&child_path);
                inodes.get_or_alloc(&child_path, NodeType::File)
            };
            let pin = self.pin_file(ino)?;
            self.prepare_file(&pin, readable, writable).await?;
            Ok::<_, CrabError>((ino, pin, entry))
        });
        match created {
            Ok((ino, pin, entry)) => {
                let fh = self
                    .handles
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .alloc_file(pin, readable, writable);
                let attr = self.make_attr(
                    ino,
                    entry.mode,
                    entry.size,
                    NodeType::File,
                    entry.mtime_ns / 1_000_000_000,
                );
                reply.created(
                    &ENTRY_TTL,
                    &attr,
                    Generation(0),
                    FuseFileHandle(fh),
                    FopenFlags::FOPEN_DIRECT_IO,
                );
            }
            Err(error) => reply.error(file_errno(&error)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let parent = u64::from(parent);
        let removed = self.rt.block_on(async {
            let _namespace = self.namespace.write().await;
            let child_path = self
                .child_path(parent, name)
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?;
            self.engine.unlink(&child_path).await?;
            self.inodes
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .detach_path(&child_path);
            Ok::<_, CrabError>(())
        });
        match removed {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(file_errno(&error)),
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
        let created = self.rt.block_on(async {
            let _namespace = self.namespace.write().await;
            let child_path = self
                .child_path(parent, name)
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?;
            self.engine.mkdir(&child_path, mode).await?;
            let mut inodes = self
                .inodes
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            inodes.detach_path(&child_path);
            Ok::<_, CrabError>(inodes.get_or_alloc(&child_path, NodeType::Dir))
        });
        match created {
            Ok(ino) => {
                let attr = self.make_attr(ino, mode | 0o040_000, 4096, NodeType::Dir, now_unix());
                reply.entry(&ENTRY_TTL, &attr, Generation(0));
            }
            Err(error) => reply.error(file_errno(&error)),
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

        let Some(target) = link.to_str() else {
            reply.error(Errno::ENOENT);
            return;
        };
        let created = self.rt.block_on(async {
            let _namespace = self.namespace.write().await;
            let child_path = self
                .child_path(parent, name)
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?;
            let entry = self
                .engine
                .create_symlink(&child_path, target, 0o777)
                .await?;
            let mut inodes = self
                .inodes
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            inodes.detach_path(&child_path);
            let ino = inodes.get_or_alloc(&child_path, NodeType::Symlink);
            Ok::<_, CrabError>((ino, entry))
        });
        match created {
            Ok((ino, entry)) => {
                let attr = self.make_attr(
                    ino,
                    entry.mode,
                    entry.size,
                    NodeType::Symlink,
                    entry.mtime_ns / 1_000_000_000,
                );
                reply.entry(&ENTRY_TTL, &attr, Generation(0));
            }
            Err(error) => reply.error(file_errno(&error)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let parent = u64::from(parent);
        let removed = self.rt.block_on(async {
            let _namespace = self.namespace.write().await;
            let child_path = self
                .child_path(parent, name)
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?;
            self.engine.rmdir(&child_path).await?;
            self.inodes
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .detach_path(&child_path);
            Ok::<_, CrabError>(())
        });
        match removed {
            Ok(()) => reply.ok(),
            Err(CrabError::Forbidden { path }) if path.starts_with("directory not empty:") => {
                reply.error(Errno::ENOTEMPTY);
            }
            Err(error) => reply.error(file_errno(&error)),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let parent = u64::from(parent);
        let newparent = u64::from(newparent);
        let replace = match rename_replaces(flags) {
            Ok(replace) => replace,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        let renamed = self.rt.block_on(async {
            let _namespace = self.namespace.write().await;
            let old_path = self
                .child_path(parent, name)
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?;
            let new_path = self
                .child_path(newparent, newname)
                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?;
            if replace {
                self.engine.rename(&old_path, &new_path).await?;
            } else {
                self.engine.rename_no_replace(&old_path, &new_path).await?;
            }
            self.inodes
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .rename_path(&old_path, &new_path);
            Ok::<_, CrabError>(())
        });
        match renamed {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(file_errno(&error)),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FuseFileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        let fh = u64::from(fh);

        self.reply_sync(u64::from(ino), fh, false, reply);
    }

    fn fsync(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FuseFileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        self.reply_sync(u64::from(ino), u64::from(fh), datasync, reply);
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

fn file_errno(error: &CrabError) -> Errno {
    match error {
        CrabError::NotFound { .. } => Errno::ENOENT,
        CrabError::Forbidden { .. } => Errno::EROFS,
        CrabError::Io(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Errno::EEXIST,
        CrabError::Io(error) => error.raw_os_error().map_or(Errno::EIO, Errno::from_i32),
        _ => Errno::EIO,
    }
}

fn rename_replaces(flags: RenameFlags) -> Result<bool, Errno> {
    #[cfg(target_os = "linux")]
    if flags == RenameFlags::RENAME_NOREPLACE {
        return Ok(false);
    }
    if flags.is_empty() {
        Ok(true)
    } else {
        // Exchange/whiteout require different namespace transactions. Treating
        // either as ordinary rename would silently remove a destination inode.
        Err(Errno::EINVAL)
    }
}

fn open_allows_write(flags: OpenFlags) -> bool {
    matches!(
        flags.acc_mode(),
        OpenAccMode::O_WRONLY | OpenAccMode::O_RDWR
    )
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

    #[test]
    fn final_inflight_handle_releases_forgotten_inode() {
        let inodes = Arc::new(std::sync::RwLock::new(InodeTable::new()));
        let (ino, node) = {
            let mut table = inodes.write().unwrap();
            let ino = table.get_or_alloc("held", NodeType::File);
            (ino, table.open_node(ino).unwrap())
        };
        let mut handles = HandleTable::new();
        let fh = handles.alloc_file(
            Arc::new(OpenNodePin {
                ino,
                node,
                inodes: inodes.clone(),
            }),
            true,
            false,
        );
        let inflight = handles.get_file(ino, fh).unwrap();
        inodes.write().unwrap().forget(ino, 1);
        handles.release_file(fh);
        assert!(inodes.read().unwrap().by_ino.contains_key(&ino));
        drop(inflight);
        assert!(!inodes.read().unwrap().by_ino.contains_key(&ino));
    }

    #[test]
    fn forgetting_replaced_inode_preserves_replacement_mapping() {
        let mut inodes = InodeTable::new();
        let old = inodes.get_or_alloc("same", NodeType::File);
        let node = inodes.open_node(old).unwrap();
        inodes.detach_path("same");
        let new = inodes.get_or_alloc("same", NodeType::File);
        inodes.forget(old, 1);
        inodes.release_open(old);
        assert_eq!(
            (node.path(), inodes.ino_for_path("same")),
            (None, Some(new))
        );
    }

    #[test]
    fn handle_is_valid_only_for_its_open_inode() {
        let inodes = Arc::new(std::sync::RwLock::new(InodeTable::new()));
        let (ino, node) = {
            let mut table = inodes.write().unwrap();
            let ino = table.get_or_alloc("file", NodeType::File);
            (ino, table.open_node(ino).unwrap())
        };
        let mut handles = HandleTable::new();
        let fh = handles.alloc_file(Arc::new(OpenNodePin { ino, node, inodes }), true, false);
        assert!(handles.get_file(ino + 1, fh).is_none());
        assert!(handles.get_file(ino, fh).is_some());
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
