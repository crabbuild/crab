use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::sync::{Arc, Mutex, Weak};

use super::{
    Bytes, CrabError, NodeType, ReadSource, ReadSourceKind, ResolvedNode, Result, VfsEngine,
    VfsReadLease, base_source_oid, ensure_readable_file, now_nanos, promotion_file,
};

/// Content retained by an open inode, independently of its current name.
pub(crate) struct VfsOpenFile {
    lease: VfsReadLease,
    staged: Option<crab_staging::StagedFilePin>,
    local: Option<LocalFile>,
    binding: Option<Arc<OpenFileBinding>>,
    source_oid: Option<String>,
    mode: u32,
    size: u64,
    mtime: i64,
}

#[derive(Default)]
pub(super) struct OpenFileRegistry {
    files: Mutex<Vec<Weak<OpenFileBinding>>>,
}

struct OpenFileBinding {
    identity: (u64, u64),
    published: Mutex<Option<Arc<ReadSource>>>,
}

struct PublishedFile {
    lease: VfsReadLease,
    source_oid: Option<String>,
}

pub(crate) struct OpenFileCommit {
    files: Vec<(Arc<OpenFileBinding>, String)>,
}

impl OpenFileRegistry {
    fn register(&self, file: &File) -> Result<Arc<OpenFileBinding>> {
        let metadata = file.metadata()?;
        let binding = Arc::new(OpenFileBinding {
            identity: (metadata.dev(), metadata.ino()),
            published: Mutex::new(None),
        });
        let mut files = self
            .files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        files.retain(|file| file.strong_count() != 0);
        files.push(Arc::downgrade(&binding));
        Ok(binding)
    }
}

struct LocalFile {
    reader: Option<File>,
    writer: Option<File>,
}

impl LocalFile {
    fn any(&self) -> Result<&File> {
        self.reader
            .as_ref()
            .or(self.writer.as_ref())
            .ok_or_else(|| CrabError::Internal("open file has no local descriptor".into()))
    }
}

impl VfsOpenFile {
    pub(crate) fn attributes(&self) -> Result<(u32, u64, NodeType, i64)> {
        let size = match &self.local {
            Some(local) => local.any()?.metadata()?.len(),
            None => self.size,
        };
        Ok((self.mode, size, NodeType::File, self.mtime))
    }

    pub(crate) fn matches_path(&self, engine: &VfsEngine, path: &str) -> Result<bool> {
        if let Some(local) = &self.local {
            let backing = engine
                .overlay
                .as_ref()
                .and_then(|ov| ov.get_backing_path(path));
            let Some(backing) = backing else {
                return self
                    .published_at(engine, path)
                    .map(|published| published.is_some());
            };
            let current = match std::fs::metadata(backing) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error.into()),
            };
            let captured = local.any()?.metadata()?;
            return Ok(current.dev() == captured.dev() && current.ino() == captured.ino());
        }
        let current = match engine.open_read(path) {
            Ok(lease) => lease,
            Err(CrabError::NotFound { .. }) => return Ok(false),
            Err(error) => return Err(error),
        };
        Ok(same_content(&self.lease.source, &current.source))
    }

    fn published_at(&self, engine: &VfsEngine, path: &str) -> Result<Option<PublishedFile>> {
        let published = self.binding.as_ref().and_then(|binding| {
            binding
                .published
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        });
        let Some(published) = published else {
            return Ok(None);
        };
        let current = match engine.open_read(path) {
            Ok(lease) => lease,
            Err(CrabError::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error),
        };
        if !same_content(&published, &current.source) {
            return Ok(None);
        }
        // Publication may retire local recipes without changing file content.
        // Resume through the current captured reader, not the pre-push catalog;
        // the binding retains only the committed content's identity.
        let source_oid = match current._snapshot_view.as_ref() {
            Some(view) => match engine.resolver.resolve_path_in(view, path)? {
                ResolvedNode::Base(base) => base_source_oid(&base),
                ResolvedNode::Overlay(_) => None,
            },
            None => None,
        };
        Ok(Some(PublishedFile {
            lease: current,
            source_oid,
        }))
    }

    pub(crate) async fn enable_access(
        &mut self,
        engine: &VfsEngine,
        path: Option<&str>,
        read: bool,
        write: bool,
    ) -> Result<()> {
        if write {
            engine.require_overlay()?;
        }
        let Some(local) = &self.local else {
            return Ok(());
        };
        if (!read || local.reader.is_some()) && (!write || local.writer.is_some()) {
            return Ok(());
        }
        let path = path.ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?;
        if !self.matches_path(engine, path)? {
            return Err(CrabError::NotFound {
                path: path.to_owned(),
            });
        }
        let Some(backing) = engine
            .overlay
            .as_ref()
            .and_then(|ov| ov.get_backing_path(path))
        else {
            if self.published_at(engine, path)?.is_some() {
                engine
                    .materialize_open_file(self, Some(path), false)
                    .await?;
                engine.invalidate_overlay_path(path);
                return Ok(());
            }
            return Err(CrabError::NotFound {
                path: path.to_owned(),
            });
        };
        let Some(local) = &mut self.local else {
            return Ok(());
        };
        if read && local.reader.is_none() {
            let reader = File::open(&backing)?;
            ensure_same_inode(local.any()?, &reader)?;
            local.reader = Some(reader);
        }
        if write && local.writer.is_none() {
            let writer = OpenOptions::new().write(true).open(backing)?;
            ensure_same_inode(local.any()?, &writer)?;
            local.writer = Some(writer);
        }
        Ok(())
    }
}

impl VfsEngine {
    pub(crate) fn capture_open_commit(
        &self,
        records: &[crate::overlay::OverlayRecord],
    ) -> Result<OpenFileCommit> {
        let active = {
            let mut files = self
                .open_files
                .files
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            files.retain(|file| file.strong_count() != 0);
            files.iter().filter_map(Weak::upgrade).collect::<Vec<_>>()
        };
        let mut by_identity: HashMap<_, Vec<_>> = HashMap::new();
        for binding in active {
            by_identity
                .entry(binding.identity)
                .or_default()
                .push(binding);
        }
        let mut files = Vec::new();
        if by_identity.is_empty() {
            return Ok(OpenFileCommit { files });
        }
        for record in records {
            let Some(backing) = &record.backing_path else {
                continue;
            };
            let metadata = std::fs::metadata(backing)?;
            if let Some(bindings) = by_identity.get(&(metadata.dev(), metadata.ino())) {
                files.extend(
                    bindings
                        .iter()
                        .map(|binding| (Arc::clone(binding), record.path.clone())),
                );
            }
        }
        Ok(OpenFileCommit { files })
    }

    pub(crate) fn adopt_open_commit(&self, committed: OpenFileCommit) -> Result<()> {
        let view = self.resolver.capture_view();
        let snapshot = self
            .snapshot
            .as_ref()
            .ok_or_else(|| CrabError::Internal("commit adoption has no snapshot store".into()))?;
        let mut published = Vec::new();
        for (binding, path) in committed.files {
            let base = snapshot
                .get_node(view.generation, &path)?
                .ok_or_else(|| CrabError::NotFound { path: path.clone() })?;
            ensure_readable_file(base.node_type, &path)?;
            let version = self.overlay_view_version_for_path(&path);
            let lease = self.open_base_read(path, Arc::clone(&view), version, base);
            published.push((binding, lease.source));
        }
        for (binding, source) in published {
            *binding
                .published
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(source);
        }
        Ok(())
    }

    pub(crate) async fn open_file(
        &self,
        path: &str,
        read: bool,
        write: bool,
    ) -> Result<VfsOpenFile> {
        let _view = self.overlay_reset_gate.read().await;
        if write {
            self.require_overlay()?;
        }
        let lease = self.open_read(path)?;
        let (mode, size, node_type, mtime) = self.getattr(path)?;
        ensure_readable_file(node_type, path)?;
        let local = match lease.source.as_ref() {
            ReadSource::OverlayFile { backing, .. } => Some(LocalFile {
                reader: if read {
                    Some(File::open(backing)?)
                } else {
                    None
                },
                writer: if write {
                    Some(OpenOptions::new().write(true).open(backing)?)
                } else {
                    None
                },
            }),
            _ => None,
        };
        if let Some(LocalFile {
            reader: Some(reader),
            writer: Some(writer),
        }) = &local
        {
            ensure_same_inode(reader, writer)?;
        }
        let source_oid = match lease._snapshot_view.as_ref() {
            Some(view) => match self.resolver.resolve_path_in(view, path)? {
                ResolvedNode::Base(base) => base_source_oid(&base),
                ResolvedNode::Overlay(_) => None,
            },
            None => None,
        };
        let staged = lease.pin_staged_file().await?;
        let binding = local
            .as_ref()
            .map(|local| self.open_files.register(local.any()?))
            .transpose()?;
        Ok(VfsOpenFile {
            lease,
            staged,
            local,
            binding,
            source_oid,
            mode,
            size,
            mtime,
        })
    }

    pub(crate) async fn read_open_file(
        &self,
        file: &VfsOpenFile,
        offset: u64,
        size: u32,
    ) -> Result<Bytes> {
        self.read_metrics.record_read_at_call();
        let Some(local) = &file.local else {
            return self.read_captured_at(&file.lease, offset, size).await;
        };
        let class = file.lease.record_read_pattern(offset, size).class;
        let reader = local
            .reader
            .as_ref()
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EBADF))?;
        let available = reader
            .metadata()?
            .len()
            .saturating_sub(offset)
            .min(u64::from(size));
        let mut bytes = vec![0; available as usize];
        let mut read = 0;
        while read < bytes.len() {
            let n = reader.read_at(&mut bytes[read..], offset + read as u64)?;
            if n == 0 {
                break;
            }
            read += n;
        }
        bytes.truncate(read);
        self.read_metrics
            .record_read_success(ReadSourceKind::OverlayFile, class, read);
        Ok(bytes.into())
    }

    pub(crate) async fn write_open_file(
        &self,
        file: &mut VfsOpenFile,
        path: Option<&str>,
        offset: u64,
        data: &[u8],
    ) -> Result<usize> {
        let ov = self.require_overlay()?;
        #[cfg(all(test, target_os = "linux", feature = "fuse", feature = "nfs"))]
        self.pause_writeback().await?;
        let _mutation = self.begin_overlay_mutation().await?;
        let path = self.open_file_link(file, path)?;
        self.materialize_open_file(file, path, false).await?;
        let local = file
            .local
            .as_ref()
            .ok_or_else(|| CrabError::Internal("write has no backing descriptor".into()))?;
        let writer = local
            .writer
            .as_ref()
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EBADF))?;
        let mtime = now_nanos();
        let mut written = 0;
        ov.mutate_content(path, writer, mtime, &mut |writer| {
            written = writer.write_at(data, offset)?;
            Ok(())
        })?;
        file.size = writer.metadata()?.len();
        file.mtime = mtime / 1_000_000_000;
        if let Some(path) = path {
            self.invalidate_overlay_path(path);
        }
        Ok(written)
    }

    pub(crate) async fn truncate_open_file(
        &self,
        file: &mut VfsOpenFile,
        path: Option<&str>,
        size: u64,
    ) -> Result<()> {
        let ov = self.require_overlay()?;
        let _mutation = self.begin_overlay_mutation().await?;
        let path = self.open_file_link(file, path)?;
        self.materialize_open_file(file, path, size == 0).await?;
        let local = file
            .local
            .as_ref()
            .ok_or_else(|| CrabError::Internal("truncate has no backing descriptor".into()))?;
        let writer = local
            .writer
            .as_ref()
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EBADF))?;
        let mtime = now_nanos();
        ov.mutate_content(path, writer, mtime, &mut |writer| {
            writer.set_len(size)?;
            Ok(())
        })?;
        file.size = size;
        file.mtime = mtime / 1_000_000_000;
        if let Some(path) = path {
            self.invalidate_overlay_path(path);
        }
        Ok(())
    }

    pub(crate) async fn set_open_file_mode(
        &self,
        file: &mut VfsOpenFile,
        path: Option<&str>,
        mode: u32,
    ) -> Result<()> {
        let ov = self.require_overlay()?;
        let _mutation = self.begin_overlay_mutation().await?;
        let path = self.open_file_link(file, path)?;
        if path.is_some() {
            self.materialize_open_file(file, path, false).await?;
        }
        let mode = (file.mode & 0o170_000) | (mode & 0o7777);
        if let Some(path) = path {
            ov.set_mode(path, mode)?;
            self.invalidate_overlay_path(path);
        } else if let Some(local) = &file.local {
            local
                .any()?
                .set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        file.mode = mode;
        Ok(())
    }

    pub(crate) async fn set_open_file_mtime(
        &self,
        file: &mut VfsOpenFile,
        path: Option<&str>,
        mtime: i64,
    ) -> Result<()> {
        let ov = self.require_overlay()?;
        let _mutation = self.begin_overlay_mutation().await?;
        let path = self.open_file_link(file, path)?;
        if path.is_some() {
            self.materialize_open_file(file, path, false).await?;
        }
        if path.is_none()
            && let Some(local) = &file.local
        {
            let duration = std::time::Duration::from_nanos(mtime.unsigned_abs());
            let time = if mtime >= 0 {
                std::time::UNIX_EPOCH.checked_add(duration)
            } else {
                std::time::UNIX_EPOCH.checked_sub(duration)
            }
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "file timestamp is out of range",
                )
            })?;
            local.any()?.set_modified(time)?;
        }
        if let Some(path) = path {
            ov.set_mtime(path, mtime)?;
            self.invalidate_overlay_path(path);
        }
        file.mtime = mtime / 1_000_000_000;
        Ok(())
    }

    pub(crate) fn sync_open_file(&self, file: &VfsOpenFile, datasync: bool) -> Result<()> {
        if let Some(local) = &file.local {
            if datasync {
                local.any()?.sync_data()?;
            } else {
                local.any()?.sync_all()?;
            }
            if let Some(ov) = &self.overlay {
                ov.checkpoint()?;
            }
        }
        Ok(())
    }

    fn open_file_link<'p>(
        &self,
        file: &VfsOpenFile,
        path: Option<&'p str>,
    ) -> Result<Option<&'p str>> {
        match path {
            Some(path) if file.matches_path(self, path)? => Ok(Some(path)),
            _ => Ok(None),
        }
    }

    async fn materialize_open_file(
        &self,
        file: &mut VfsOpenFile,
        path: Option<&str>,
        empty: bool,
    ) -> Result<()> {
        let published = if file.local.is_some() {
            let needs_cow = path.is_some_and(|path| {
                self.overlay
                    .as_ref()
                    .and_then(|ov| ov.get_backing_path(path))
                    .is_none()
            });
            if !needs_cow {
                return Ok(());
            }
            let path =
                path.ok_or_else(|| CrabError::Internal("published open inode has no path".into()))?;
            Some(
                file.published_at(self, path)?
                    .ok_or_else(|| CrabError::NotFound {
                        path: path.to_owned(),
                    })?,
            )
        } else {
            None
        };
        // Keep native content and its publication binding until replacement
        // succeeds. Catalog, disk or access-upgrade failure must not revoke
        // another descriptor's reads or its ability to retry promotion.
        let lease = published.as_ref().map_or(&file.lease, |next| &next.lease);
        let source_oid = published
            .as_ref()
            .map_or(file.source_oid.as_deref(), |next| {
                next.source_oid.as_deref()
            });
        let ov = self.require_overlay()?;
        let destination = ov.backing_path_for(path.unwrap_or_else(|| file.lease.path()));
        let mut temporary = promotion_file(&destination)?;
        if !empty {
            match lease.source.as_ref() {
                ReadSource::BasePointer { pointer, .. } => {
                    let hydration = lease
                        ._snapshot_view
                        .as_ref()
                        .and_then(|view| view.hydration.as_ref())
                        .ok_or_else(|| {
                            CrabError::Internal("open pointer has no captured reader".into())
                        })?;
                    if hydration
                        .reconstruct_to_path(pointer, temporary.path())
                        .await?
                        .is_none()
                    {
                        for term in hydration.resolve_terms(pointer)? {
                            let bytes = hydration.fetch_chunk(&term).await?;
                            std::io::Write::write_all(temporary.as_file_mut(), &bytes)?;
                        }
                    }
                }
                ReadSource::BaseBlob { oid, .. } => {
                    let reader = self
                        .odb_reader
                        .as_ref()
                        .ok_or_else(|| CrabError::Internal("open Git file has no reader".into()))?;
                    (temporary, _) = reader.copy_blob(oid, temporary).await?;
                }
                ReadSource::BaseEmpty => {}
                ReadSource::OverlayFile { .. } => {
                    return Err(CrabError::Internal(
                        "open overlay has no retained descriptor".into(),
                    ));
                }
            }
        }
        let size = temporary.as_file().metadata()?.len();
        let reader = temporary.as_file().try_clone()?;
        let binding = self.open_files.register(temporary.as_file())?;
        let descriptor = if let Some(path) = path {
            let descriptor = temporary
                .persist(&destination)
                .map_err(|error| CrabError::Io(error.error))?;
            ov.promote_from_file(path, file.mode, size, source_oid)?;
            descriptor
        } else {
            // Keep only the descriptor: an unlinked inode must never recreate
            // its former name or become part of a later overlay publication.
            let (descriptor, temporary_path) = temporary.into_parts();
            temporary_path.close()?;
            descriptor
        };
        let local = LocalFile {
            reader: Some(reader),
            writer: Some(descriptor),
        };
        if let Some(published) = published {
            file.lease = published.lease;
            file.source_oid = published.source_oid;
        }
        file.binding = Some(binding);
        file.local = Some(local);
        file.staged = None;
        file.size = size;
        Ok(())
    }
}

fn same_content(first: &ReadSource, second: &ReadSource) -> bool {
    match (first, second) {
        (
            ReadSource::BasePointer { pointer: a, .. },
            ReadSource::BasePointer { pointer: b, .. },
        ) => a.file_hash == b.file_hash && a.size == b.size,
        (ReadSource::BaseBlob { oid: a, .. }, ReadSource::BaseBlob { oid: b, .. }) => a == b,
        (ReadSource::BaseEmpty, ReadSource::BaseEmpty) => true,
        _ => false,
    }
}

fn ensure_same_inode(first: &File, second: &File) -> Result<()> {
    let first = first.metadata()?;
    let second = second.metadata()?;
    if first.dev() == second.dev() && first.ino() == second.ino() {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "open inode was replaced during access upgrade",
    )
    .into())
}
