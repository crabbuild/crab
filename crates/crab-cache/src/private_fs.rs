//! Descriptor-relative I/O for private, disposable cache payloads.

use std::path::Path;

#[cfg(test)]
use tokio::io::AsyncWriteExt as _;

use crate::{CacheError, Result};

#[derive(Clone, Copy)]
pub(crate) struct FileStat {
    pub(crate) size: u64,
    pub(crate) modified_ns: u64,
}

#[derive(Clone, Copy)]
pub(crate) struct EntryStat {
    pub(crate) file: FileStat,
    pub(crate) allocated_bytes: u64,
    pub(crate) is_directory: bool,
}

pub(crate) struct PinnedRoot(platform::Directory);

pub(crate) type RemovePayload<'a> =
    dyn FnMut(&Path, &mut dyn FnMut() -> Result<Option<u64>>) -> Result<Option<u64>> + 'a;

pub(crate) use platform::Database;

pub(crate) struct DatabaseLease {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    generation: std::sync::Arc<platform::Generation>,
}

impl DatabaseLease {
    pub(crate) fn capture(_database: &Database) -> Self {
        Self {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            generation: _database.generation(),
        }
    }

    pub(crate) fn open(
        &self,
        root: &PinnedRoot,
        relative: &Path,
        busy_timeout: std::time::Duration,
    ) -> Result<Database> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        return platform::open_database_leased(&root.0, relative, &self.generation, busy_timeout);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        root.open_database(relative, DatabaseMode::ReadWrite, busy_timeout)
    }

    pub(crate) fn validate(&self, _root: &PinnedRoot, relative: &Path) -> Result<()> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        return platform::validate_database_generation(&_root.0, relative, &self.generation);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        Err(CacheError::UnsafeRoot {
            path: relative.display().to_string(),
            reason: "private database generation ownership is unavailable on this platform".into(),
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DatabaseMode {
    ReadOnly,
    ReadWrite,
    Create,
}

#[cfg(any(feature = "local-cache", test))]
pub(crate) fn open_database(
    root: &Path,
    path: &Path,
    mode: DatabaseMode,
    busy_timeout: std::time::Duration,
) -> Result<Database> {
    platform::open_database(root, path, mode, busy_timeout)
}

impl PinnedRoot {
    pub(crate) fn clean(
        &self,
        dry_run: bool,
        cancel: &tokio_util::sync::CancellationToken,
        remove: &mut RemovePayload<'_>,
    ) -> Result<crate::CacheCleanReport> {
        platform::clean(&self.0, dry_run, cancel, remove)
    }

    #[cfg(feature = "xet-chunk-cache")]
    pub(crate) fn open_with_private_parent(path: &Path) -> Result<(Self, Option<Self>)> {
        platform::Directory::root_with_private_parent(path)
            .map(|(root, parent)| (Self(root), parent.map(Self)))
    }

    pub(crate) fn open(root: &Path) -> Result<Self> {
        platform::Directory::root(root, false).map(Self)
    }

    pub(crate) fn create(root: &Path) -> Result<Self> {
        platform::Directory::root(root, true).map(Self)
    }

    pub(crate) fn open_database(
        &self,
        relative: &Path,
        mode: DatabaseMode,
        busy_timeout: std::time::Duration,
    ) -> Result<Database> {
        platform::open_database_at(&self.0, relative, mode, busy_timeout)
    }

    pub(crate) fn pending_file(&self, relative: &Path) -> Result<PendingFile> {
        platform::TemporaryFile::new_at(&self.0, relative).map(PendingFile)
    }

    pub(crate) fn visit_files(
        &self,
        visitor: &mut dyn FnMut(&Path, FileStat) -> Result<()>,
    ) -> Result<()> {
        self.0.visit_files(visitor)
    }

    pub(crate) fn visit_selected_files(
        &self,
        select: &dyn Fn(&Path) -> Result<bool>,
        visitor: &mut dyn FnMut(&Path, FileStat) -> Result<()>,
    ) -> Result<()> {
        self.0.visit_selected_files(select, visitor)
    }

    pub(crate) fn inspect_entries(
        &self,
        visitor: &mut dyn FnMut(&Path, Result<EntryStat>) -> Result<()>,
    ) -> Result<()> {
        self.0.inspect_entries(visitor)
    }

    pub(crate) fn remove_file_if(
        &self,
        relative: &Path,
        dry_run: bool,
        should_remove: &mut dyn FnMut(&mut std::fs::File) -> Result<bool>,
    ) -> Result<Option<u64>> {
        self.0.remove_relative_if(relative, dry_run, should_remove)
    }

    pub(crate) fn remove_file(&self, relative: &Path) -> Result<u64> {
        self.0.remove_relative(relative)
    }

    pub(crate) fn open_lock(&self, relative: &Path) -> Result<std::fs::File> {
        self.0.open_lock(relative)
    }
}

pub(crate) async fn run_blocking<T, F>(
    cancel: &tokio_util::sync::CancellationToken,
    work: F,
) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&tokio_util::sync::CancellationToken) -> Result<T> + Send + 'static,
{
    let cancel = cancel.child_token();
    let _cancel_on_drop = cancel.clone().drop_guard();
    tokio::task::spawn_blocking(move || {
        check_cancelled(&cancel)?;
        let result = work(&cancel)?;
        check_cancelled(&cancel)?;
        Ok(result)
    })
    .await
    .map_err(|error| CacheError::Io(std::io::Error::other(error)))?
}

pub(crate) async fn with_pinned_root<T, F>(
    root: &Path,
    cancel: &tokio_util::sync::CancellationToken,
    work: F,
) -> Result<T>
where
    T: Default + Send + 'static,
    F: FnOnce(&PinnedRoot, &tokio_util::sync::CancellationToken) -> Result<T> + Send + 'static,
{
    let root = root.to_owned();
    run_blocking(cancel, move |cancel| {
        let root = match PinnedRoot::open(&root) {
            Ok(root) => root,
            Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(T::default());
            }
            Err(error) => return Err(error),
        };
        work(&root, cancel)
    })
    .await
}

pub(crate) fn check_cancelled(cancel: &tokio_util::sync::CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(CacheError::Cancelled)
    } else {
        Ok(())
    }
}

pub(crate) fn ensure_directory(path: &Path) -> Result<()> {
    platform::check_directory(path, true)
}

pub(crate) fn directory_is_safe(path: &Path) -> bool {
    match platform::check_directory(path, false) {
        Ok(()) => true,
        Err(CacheError::Io(error)) => error.kind() == std::io::ErrorKind::NotFound,
        Err(_) => false,
    }
}

/// Inspect at most `limit` names through a pinned private directory.
#[cfg(feature = "xet-chunk-cache")]
pub(crate) async fn entry_names(
    root: &Path,
    path: &Path,
    limit: usize,
) -> Result<Vec<std::ffi::OsString>> {
    let root = root.to_owned();
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || platform::entry_names(&root, &path, limit))
        .await
        .map_err(|error| CacheError::Io(std::io::Error::other(error)))?
}

/// Open and lease the same file descriptor that the reader will consume.
pub(crate) async fn open_read(root: &Path, path: &Path) -> Result<tokio::fs::File> {
    let root = root.to_owned();
    let path = path.to_owned();
    let result = tokio::task::spawn_blocking(move || platform::open_read(&root, &path))
        .await
        .map_err(|error| CacheError::Io(std::io::Error::other(error)))?;
    result.map(tokio::fs::File::from_std)
}

#[cfg(feature = "local-cache")]
pub(crate) fn read_bounded_sync(root: &Path, path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let file = platform::open_read(root, path)?;
    if file.metadata()?.len() > max_bytes {
        return Err(CacheError::CorruptObject {
            path: path.display().to_string(),
            reason: format!("cache entry exceeds {max_bytes} bytes"),
        });
    }
    let mut data = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut data)?;
    if data.len() as u64 > max_bytes {
        return Err(CacheError::CorruptObject {
            path: path.display().to_string(),
            reason: format!("cache entry grew beyond {max_bytes} bytes"),
        });
    }
    Ok(data)
}

/// Replace a cache entry atomically without resolving its parent again.
#[cfg(test)]
pub(crate) async fn atomic_write(root: &Path, path: &Path, data: &[u8]) -> Result<()> {
    PendingFile::new(root, path).await?.write(data).await
}

/// One unpublished entry whose cleanup and publication share a pinned parent.
pub(crate) struct PendingFile(platform::TemporaryFile);

impl PendingFile {
    #[cfg(test)]
    pub(crate) async fn new(root: &Path, path: &Path) -> Result<Self> {
        let root = root.to_owned();
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || platform::TemporaryFile::new(&root, &path).map(Self))
            .await
            .map_err(|error| CacheError::Io(std::io::Error::other(error)))?
    }

    pub(crate) fn file(&self) -> Result<tokio::fs::File> {
        Ok(tokio::fs::File::from_std(self.0.file().try_clone()?))
    }

    #[cfg(all(feature = "remote-client", feature = "local-cache"))]
    pub(crate) fn into_unlinked_file(self) -> Result<std::fs::File> {
        self.0.into_unlinked_file()
    }

    pub(crate) fn lease(&self) -> Result<std::fs::File> {
        let file = self.0.file().try_clone()?;
        if !fs4::fs_std::FileExt::try_lock_shared(&file)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "cache payload is busy",
            )
            .into());
        }
        // Payload clones intentionally share this flock: rename and closing
        // the writer must retain protection until the reservation is released.
        Ok(file)
    }

    #[cfg(test)]
    pub(crate) async fn write(self, data: &[u8]) -> Result<()> {
        let mut writer = self.file()?;
        writer.write_all(data).await?;
        writer.sync_all().await?;
        drop(writer);
        // The guard removes only its own descriptor-relative temporary on
        // cancellation. Rename can publish complete immutable bytes safely
        // even when the caller stops waiting after publication begins.
        self.commit().await
    }

    #[cfg(feature = "local-cache")]
    pub(crate) fn write_body_sync(&self, data: &[u8]) -> Result<()> {
        use std::io::Write as _;
        let mut writer = self.0.file();
        writer.write_all(data)?;
        writer.sync_all()?;
        Ok(())
    }

    pub(crate) fn commit_sync(self) -> Result<()> {
        self.0.commit()
    }

    #[cfg(test)]
    pub(crate) async fn commit(self) -> Result<()> {
        tokio::task::spawn_blocking(move || self.0.commit())
            .await
            .map_err(|error| CacheError::Io(std::io::Error::other(error)))?
    }
}

/// Update recency through a private file handle; failure only loses an LRU hint.
pub(crate) async fn touch(root: &Path, path: &Path) {
    let root = root.to_owned();
    let path = path.to_owned();
    let _ = tokio::task::spawn_blocking(move || {
        let file = platform::open_read(&root, &path)?;
        file.set_modified(std::time::SystemTime::now())?;
        Ok::<_, CacheError>(())
    })
    .await;
}

#[cfg(unix)]
mod platform {
    use std::ffi::{CStr, OsString};
    use std::ffi::{CString, OsStr};
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::fd::IntoRawFd as _;
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::ffi::OsStringExt as _;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
    use std::path::{Component, Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use fs4::fs_std::FileExt;

    use crate::{CacheError, Result};

    mod cleanup;
    pub(super) use cleanup::clean;
    mod database;
    mod scan;
    pub(crate) use database::Database;
    #[cfg(any(feature = "local-cache", test))]
    pub(super) use database::open_database;
    pub(super) use database::open_database_at;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(super) use database::{Generation, open_database_leased, validate_database_generation};

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    pub(super) struct Directory {
        file: File,
        path: PathBuf,
    }

    impl Directory {
        fn stat_component(&self, name: &CStr) -> io::Result<libc::stat> {
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: the parent descriptor and NUL-terminated name are live;
            // fstatat initializes stat on success without opening the file.
            let result = unsafe {
                libc::fstatat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful fstatat initialized the output buffer.
            Ok(unsafe { stat.assume_init() })
        }

        pub(super) fn root(path: &Path, create: bool) -> Result<Self> {
            let absolute = std::path::absolute(path)?;
            let name = absolute
                .file_name()
                .ok_or_else(|| unsafe_path(path, "cache root has no name"))?;
            let parent = absolute
                .parent()
                .ok_or_else(|| unsafe_path(path, "cache root has no parent"))?;
            let parent = Self::ambient_parent(parent, create)?;
            parent.child(name, create)
        }

        #[cfg(feature = "xet-chunk-cache")]
        pub(super) fn root_with_private_parent(path: &Path) -> Result<(Self, Option<Self>)> {
            let absolute = std::path::absolute(path)?;
            let name = absolute
                .file_name()
                .ok_or_else(|| unsafe_path(path, "cache root has no name"))?;
            let parent = absolute
                .parent()
                .ok_or_else(|| unsafe_path(path, "cache root has no parent"))?;
            let parent = Self::ambient_parent(parent, false)?;
            let root = parent.child(name, false)?;
            // Standalone range maintenance owns only the private leaf. A
            // private captured parent may own its catalog; never reopen the
            // ambient pathname to find that sibling database after a rename.
            let private = validate_metadata(&parent.file.metadata()?, &parent.path, true).is_ok();
            Ok((root, private.then_some(parent)))
        }

        fn ambient_parent(path: &Path, create: bool) -> Result<Self> {
            // Ambient ancestors can include OS aliases such as macOS /var.
            // Only the configured root and its descendants are cache-owned;
            // pin the ambient directory before creating/opening that root.
            match std::fs::canonicalize(path) {
                Ok(canonical) => Ok(Self {
                    file: OpenOptions::new()
                        .read(true)
                        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                        .open(canonical)?,
                    path: path.to_owned(),
                }),
                Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                    let parent = path
                        .parent()
                        .ok_or_else(|| unsafe_path(path, "missing ambient parent"))?;
                    let name = path
                        .file_name()
                        .ok_or_else(|| unsafe_path(path, "missing directory name"))?;
                    Self::ambient_parent(parent, true)?.child(name, true)
                }
                Err(error) => Err(error.into()),
            }
        }

        pub(super) fn child(&self, name: &OsStr, create: bool) -> Result<Self> {
            let path = self.path.join(name);
            let name = component_name(name)?;
            if create {
                // SAFETY: the parent descriptor is live and name is a single
                // NUL-terminated component; mkdirat never follows the leaf.
                let result = unsafe { libc::mkdirat(self.file.as_raw_fd(), name.as_ptr(), 0o700) };
                if result != 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::AlreadyExists {
                        return Err(error.into());
                    }
                }
            }
            let file = self.open_component(&name, libc::O_RDONLY | libc::O_DIRECTORY, &path)?;
            validate_metadata(&file.metadata()?, &path, true)?;
            Ok(Self { file, path })
        }

        fn parent(root: &Path, path: &Path, create: bool) -> Result<(Self, CString)> {
            let relative = path
                .strip_prefix(root)
                .map_err(|_| unsafe_path(path, "entry is outside cache root"))?;
            Self::root(root, create)?.descendant_parent(relative, create)
        }

        fn descendant_parent(&self, relative: &Path, create: bool) -> Result<(Self, CString)> {
            let path = self.path.join(relative);
            let mut components = relative.components().peekable();
            // dup/try_clone shares flock ownership. Each operation needs a
            // separate open-file description so one guard cannot release a
            // concurrent operation's namespace lock on this pinned root.
            let file = self.open_component(c".", libc::O_RDONLY | libc::O_DIRECTORY, &self.path)?;
            validate_metadata(&file.metadata()?, &self.path, true)?;
            let mut directory = Self {
                file,
                path: self.path.clone(),
            };
            while let Some(component) = components.next() {
                let Component::Normal(name) = component else {
                    return Err(unsafe_path(
                        &path,
                        "entry contains an unsafe path component",
                    ));
                };
                if components.peek().is_none() {
                    return Ok((directory, component_name(name)?));
                }
                directory = directory.child(name, create)?;
            }
            Err(unsafe_path(&path, "entry has no filename"))
        }

        pub(super) fn remove_relative(&self, relative: &Path) -> Result<u64> {
            let (directory, name) = self.descendant_parent(relative, false)?;
            directory.remove_payload(&name, false)
        }

        pub(super) fn remove_relative_if(
            &self,
            relative: &Path,
            dry_run: bool,
            should_remove: &mut dyn FnMut(&mut File) -> Result<bool>,
        ) -> Result<Option<u64>> {
            let (directory, name) = self.descendant_parent(relative, false)?;
            directory.remove_payload_if(&name, dry_run, should_remove)
        }

        pub(super) fn open_lock(&self, relative: &Path) -> Result<File> {
            let (directory, name) = self.descendant_parent(relative, true)?;
            let path = self.path.join(relative);
            let file = directory.open_component(&name, libc::O_RDWR | libc::O_CREAT, &path)?;
            validate_metadata(&file.metadata()?, &path, false)?;
            Ok(file)
        }

        fn open_component(&self, name: &CStr, flags: libc::c_int, path: &Path) -> Result<File> {
            // SAFETY: name is one NUL-terminated component and the owned parent
            // fd stays open. A successful call transfers a new fd to File.
            let fd = unsafe {
                libc::openat(
                    self.file.as_raw_fd(),
                    name.as_ptr(),
                    flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                    0o600 as libc::c_uint,
                )
            };
            if fd < 0 {
                let error = io::Error::last_os_error();
                return match error.raw_os_error() {
                    Some(libc::ELOOP | libc::ENOTDIR) => Err(unsafe_path(
                        path,
                        "symlink or non-directory cache component",
                    )),
                    _ => Err(error.into()),
                };
            }
            // SAFETY: openat returned a fresh fd that no other owner closes.
            Ok(unsafe { File::from_raw_fd(fd) })
        }

        fn remove(&self, name: &CString) -> io::Result<()> {
            // SAFETY: the live parent fd and one-component name identify only
            // this entry; flags=0 unlinks a leaf, never a directory or target.
            if unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }

        fn mutation(&self) -> Result<MutationGuard<'_>> {
            // Publication and deletion must serialize on the pinned parent,
            // otherwise an unlink could remove a replacement with a new lease.
            if !FileExt::try_lock_exclusive(&self.file)? {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "cache directory is being maintained",
                )
                .into());
            }
            Ok(MutationGuard(&self.file))
        }

        fn remove_payload(&self, name: &CString, dry_run: bool) -> Result<u64> {
            self.remove_payload_if(name, dry_run, &mut |_| Ok(true))?
                .ok_or_else(|| io::Error::other("unconditional cache removal was skipped").into())
        }

        fn remove_payload_if(
            &self,
            name: &CString,
            dry_run: bool,
            should_remove: &mut dyn FnMut(&mut File) -> Result<bool>,
        ) -> Result<Option<u64>> {
            let _mutation = self.mutation()?;
            let path = self.path.join(OsStr::from_bytes(name.as_bytes()));
            let mut file = self.open_component(name, libc::O_RDONLY, &path)?;
            let metadata = file.metadata()?;
            validate_metadata(&metadata, &path, false)?;
            if !FileExt::try_lock_exclusive(&file)? {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "cache entry has an active reader",
                )
                .into());
            }
            // Verify the leased descriptor while publication is excluded. A
            // pathname reopen or releasing either lock before unlink could
            // delete a healthy replacement instead of the inspected entry.
            if !should_remove(&mut file)? {
                return Ok(None);
            }
            if !dry_run {
                self.remove(name)?;
            }
            Ok(Some(metadata.len()))
        }
    }

    struct MutationGuard<'a>(&'a File);

    impl Drop for MutationGuard<'_> {
        fn drop(&mut self) {
            let _ = FileExt::unlock(self.0);
        }
    }

    fn component_name(name: &OsStr) -> Result<CString> {
        let path = Path::new(name);
        if path.components().count() != 1
            || !matches!(path.components().next(), Some(Component::Normal(_)))
        {
            return Err(unsafe_path(path, "expected one normal path component"));
        }
        CString::new(name.as_bytes())
            .map_err(|error| CacheError::Io(io::Error::new(io::ErrorKind::InvalidInput, error)))
    }

    fn unsafe_path(path: &Path, reason: &str) -> CacheError {
        CacheError::UnsafeRoot {
            path: path.display().to_string(),
            reason: reason.to_owned(),
        }
    }

    fn validate_metadata(metadata: &std::fs::Metadata, path: &Path, directory: bool) -> Result<()> {
        validate_permissions(metadata.mode(), metadata.uid(), path)?;
        if (directory && !metadata.is_dir())
            || (!directory && (!metadata.is_file() || metadata.nlink() != 1))
        {
            return Err(unsafe_path(
                path,
                "entry is a special file or has another hard link",
            ));
        }
        Ok(())
    }

    fn validate_permissions(mode: impl Into<u32>, owner: libc::uid_t, path: &Path) -> Result<()> {
        // SAFETY: geteuid only reads the calling process's effective identity.
        let uid = unsafe { libc::geteuid() };
        if owner != uid || mode.into() & 0o077 != 0 {
            return Err(unsafe_path(
                path,
                "entry is not private to the current user",
            ));
        }
        Ok(())
    }

    pub(super) fn open_read(root: &Path, path: &Path) -> Result<File> {
        let (directory, name) = Directory::parent(root, path, false)?;
        let file = directory.open_component(&name, libc::O_RDONLY, path)?;
        validate_metadata(&file.metadata()?, path, false)?;
        if !FileExt::try_lock_shared(&file)? {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "cache entry is being maintained",
            )
            .into());
        }
        Ok(file)
    }

    pub(super) fn check_directory(path: &Path, create: bool) -> Result<()> {
        Directory::root(path, create).map(|_| ())
    }

    #[cfg(feature = "xet-chunk-cache")]
    pub(super) fn entry_names(root: &Path, path: &Path, limit: usize) -> Result<Vec<OsString>> {
        let mut directory = Directory::root(root, false)?;
        let relative = path
            .strip_prefix(root)
            .map_err(|_| unsafe_path(path, "directory is outside cache root"))?;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(unsafe_path(path, "directory contains an unsafe component"));
            };
            directory = directory.child(name, false)?;
        }
        let mut stream = DirectoryStream::new(&directory)?;
        let mut names = Vec::new();
        while names.len() < limit {
            let Some(name) = stream.next_name()? else {
                break;
            };
            names.push(name);
        }
        Ok(names)
    }

    struct DirectoryStream(*mut libc::DIR);

    impl DirectoryStream {
        fn new(directory: &Directory) -> Result<Self> {
            // A fresh description keeps directory cursors independent across
            // inspections of the same pinned root; dup would share its offset.
            let file = directory.open_component(
                c".",
                libc::O_RDONLY | libc::O_DIRECTORY,
                &directory.path,
            )?;
            // SAFETY: fdopendir takes ownership only on success; File retains
            // the descriptor on error, and closedir owns it after success.
            let pointer = unsafe { libc::fdopendir(file.as_raw_fd()) };
            if pointer.is_null() {
                return Err(io::Error::last_os_error().into());
            }
            let _ = file.into_raw_fd();
            Ok(Self(pointer))
        }

        fn next_name(&mut self) -> Result<Option<OsString>> {
            loop {
                errno::set_errno(errno::Errno(0));
                // SAFETY: this worker exclusively owns the live stream. Copy
                // d_name before another call can invalidate the entry pointer.
                let entry = unsafe { libc::readdir(self.0) };
                if entry.is_null() {
                    let error = errno::errno().0;
                    return if error == 0 {
                        Ok(None)
                    } else {
                        Err(io::Error::from_raw_os_error(error).into())
                    };
                }
                // SAFETY: readdir returned a live, NUL-terminated d_name.
                let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
                if name != b"." && name != b".." {
                    return Ok(Some(OsString::from_vec(name.to_vec())));
                }
            }
        }
    }

    impl Drop for DirectoryStream {
        fn drop(&mut self) {
            // SAFETY: this value exclusively owns the fdopendir stream.
            unsafe { libc::closedir(self.0) };
        }
    }

    #[cfg(test)]
    pub(super) fn remove_file(root: &Path, path: &Path) -> Result<u64> {
        let (directory, name) = Directory::parent(root, path, false)?;
        directory.remove_payload(&name, false)
    }

    pub(super) struct TemporaryFile {
        directory: Directory,
        temporary: CString,
        destination: CString,
        file: File,
        published: bool,
    }

    impl TemporaryFile {
        #[cfg(test)]
        pub(super) fn new(root: &Path, path: &Path) -> Result<Self> {
            let relative = path
                .strip_prefix(root)
                .map_err(|_| unsafe_path(path, "entry is outside cache root"))?;
            Self::new_at(&Directory::root(root, true)?, relative)
        }

        pub(super) fn new_at(root: &Directory, relative: &Path) -> Result<Self> {
            let (directory, destination) = root.descendant_parent(relative, true)?;
            let path = root.path.join(relative);
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let temporary = component_name(OsStr::new(&format!(
                ".tmp-{}-{sequence}",
                std::process::id()
            )))?;
            let file = directory.open_component(
                &temporary,
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                &path,
            )?;
            let temporary = Self {
                directory,
                temporary,
                destination,
                file,
                published: false,
            };
            temporary
                .file
                .set_permissions(std::fs::Permissions::from_mode(0o600))?;
            Ok(temporary)
        }

        pub(super) fn file(&self) -> &File {
            &self.file
        }

        #[cfg(all(feature = "remote-client", feature = "local-cache"))]
        pub(super) fn into_unlinked_file(mut self) -> Result<File> {
            let file = self.file.try_clone()?;
            self.directory.remove(&self.temporary)?;
            // No pathname may outlive this unpublished stream. Closing its
            // final descriptor also reclaims bytes after cancellation or exit.
            self.published = true;
            Ok(file)
        }

        pub(super) fn commit(mut self) -> Result<()> {
            let _mutation = self.directory.mutation()?;
            // SAFETY: both single-component names are relative to the same
            // live directory descriptor. renameat replaces the leaf itself,
            // even if a competing process changed it into a symlink.
            let result = unsafe {
                libc::renameat(
                    self.directory.file.as_raw_fd(),
                    self.temporary.as_ptr(),
                    self.directory.file.as_raw_fd(),
                    self.destination.as_ptr(),
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error().into());
            }
            self.published = true;
            Ok(())
        }
    }

    impl Drop for TemporaryFile {
        fn drop(&mut self) {
            if !self.published {
                let _ = self.directory.remove(&self.temporary);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::{Read as _, Write as _};

        #[test]
        fn operations_from_one_pinned_root_have_independent_namespace_locks() {
            let temporary = tempfile::tempdir().unwrap();
            let root = Directory::root(&temporary.path().join("cache"), true).unwrap();
            for relative in ["entry", "child/entry"] {
                let (first, _) = root.descendant_parent(Path::new(relative), true).unwrap();
                let (second, _) = root.descendant_parent(Path::new(relative), true).unwrap();
                let guard = first.mutation().unwrap();
                assert!(
                    matches!(second.mutation(), Err(CacheError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock)
                );
                drop(guard);
                let _guard = second.mutation().unwrap();
                assert!(
                    matches!(first.mutation(), Err(CacheError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock)
                );
            }
        }
        use std::os::unix::fs::symlink;

        #[test]
        fn parent_swap_cannot_redirect_atomic_publication() {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("cache");
            let path = root.join("chunks/entry");
            let mut pending = TemporaryFile::new(&root, &path).unwrap();
            pending.file.write_all(b"cached").unwrap();
            let outside = tmp.path().join("outside");
            std::fs::create_dir(&outside).unwrap();
            std::fs::write(outside.join("entry"), b"untouched").unwrap();
            std::fs::rename(root.join("chunks"), root.join("moved")).unwrap();
            symlink(&outside, root.join("chunks")).unwrap();
            pending.commit().unwrap();
            assert_eq!(std::fs::read(outside.join("entry")).unwrap(), b"untouched");
            assert_eq!(std::fs::read(root.join("moved/entry")).unwrap(), b"cached");
        }

        #[test]
        fn read_handle_and_lease_survive_path_replacement_together() {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("cache");
            let path = root.join("chunks/entry");
            let mut pending = TemporaryFile::new(&root, &path).unwrap();
            pending.file.write_all(b"original").unwrap();
            pending.commit().unwrap();
            let mut reader = open_read(&root, &path).unwrap();
            let mut replacement = TemporaryFile::new(&root, &path).unwrap();
            replacement.file.write_all(b"replacement").unwrap();
            replacement.commit().unwrap();
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"original");
        }

        #[test]
        fn abandoned_temporary_is_removed_from_its_pinned_directory() {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("cache");
            let pending = TemporaryFile::new(&root, &root.join("chunks/entry")).unwrap();
            std::fs::rename(root.join("chunks"), root.join("moved")).unwrap();
            drop(pending);
            assert_eq!(std::fs::read_dir(root.join("moved")).unwrap().count(), 0);
        }

        #[test]
        fn intermediate_symlink_and_parent_traversal_are_rejected() {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("cache");
            Directory::root(&root, true).unwrap();
            symlink(tmp.path(), root.join("chunks")).unwrap();
            for path in [root.join("chunks/entry"), root.join("../entry")] {
                assert!(matches!(
                    open_read(&root, &path),
                    Err(CacheError::UnsafeRoot { .. })
                ));
            }
        }

        #[test]
        fn locked_entry_is_a_nonblocking_miss() {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("cache");
            let path = root.join("chunks/entry");
            TemporaryFile::new(&root, &path).unwrap().commit().unwrap();
            let owner = OpenOptions::new().write(true).open(&path).unwrap();
            assert!(owner.try_lock_exclusive().unwrap());
            assert!(
                matches!(open_read(&root, &path), Err(CacheError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock)
            );
        }

        #[test]
        fn eviction_retains_an_active_read_descriptor() {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("cache");
            let path = root.join("chunks/entry");
            TemporaryFile::new(&root, &path).unwrap().commit().unwrap();
            let reader = open_read(&root, &path).unwrap();
            assert!(
                matches!(remove_file(&root, &path), Err(CacheError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock)
            );
            assert!(path.exists());
            drop(reader);
            remove_file(&root, &path).unwrap();
            assert!(!path.exists());
        }

        #[test]
        fn verification_excludes_publication_and_releases_both_locks() {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("cache");
            let relative = Path::new("chunks/entry");
            let path = root.join(relative);
            let mut original = TemporaryFile::new(&root, &path).unwrap();
            original.file.write_all(b"original").unwrap();
            original.commit().unwrap();
            let pinned = Directory::root(&root, false).unwrap();
            for fail in [false, true] {
                let result = pinned.remove_relative_if(relative, false, &mut |file| {
                    let mut data = Vec::new();
                    file.read_to_end(&mut data).unwrap();
                    assert_eq!(data, b"original");
                    let replacement = TemporaryFile::new(&root, &path).unwrap();
                    assert!(matches!(replacement.commit(), Err(CacheError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock));
                    assert!(matches!(open_read(&root, &path), Err(CacheError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock));
                    if fail { Err(CacheError::Cancelled) } else { Ok(false) }
                });
                if fail {
                    assert!(matches!(result, Err(CacheError::Cancelled)));
                } else {
                    assert_eq!(result.unwrap(), None);
                }
                assert_eq!(std::fs::read(&path).unwrap(), b"original");
                drop(open_read(&root, &path).unwrap());
            }
            TemporaryFile::new(&root, &path).unwrap().commit().unwrap();
            assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        }

        #[test]
        fn verification_and_unlink_keep_the_same_parent_after_root_replacement() {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("cache");
            let replacement = tmp.path().join("replacement");
            let moved = tmp.path().join("moved");
            let relative = Path::new("chunks/entry");
            for (directory, data) in [(&root, b"bad".as_slice()), (&replacement, b"healthy")] {
                let mut file = TemporaryFile::new(directory, &directory.join(relative)).unwrap();
                file.file.write_all(data).unwrap();
                file.commit().unwrap();
            }
            let pinned = Directory::root(&root, false).unwrap();
            let removed = pinned
                .remove_relative_if(relative, false, &mut |file| {
                    std::fs::rename(&root, &moved).unwrap();
                    std::fs::rename(&replacement, &root).unwrap();
                    let mut data = Vec::new();
                    file.read_to_end(&mut data).unwrap();
                    assert_eq!(data, b"bad");
                    Ok(true)
                })
                .unwrap();
            assert_eq!(removed, Some(3));
            assert!(!moved.join(relative).exists());
            assert_eq!(std::fs::read(root.join(relative)).unwrap(), b"healthy");
        }

        #[test]
        fn mutation_lock_is_shared_between_processes() {
            const FIXTURE_ROOT: &str = "CRAB_TEST_PRIVATE_FS_MUTATION_ROOT";
            if let Some(root) = std::env::var_os(FIXTURE_ROOT) {
                let root = PathBuf::from(root);
                let path = root.join("chunks/entry");
                for result in [
                    remove_file(&root, &path).map(|_| ()),
                    TemporaryFile::new(&root, &path).unwrap().commit(),
                ] {
                    assert!(
                        matches!(result, Err(CacheError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock)
                    );
                }
                return;
            }
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("cache");
            let path = root.join("chunks/entry");
            TemporaryFile::new(&root, &path).unwrap().commit().unwrap();
            let (directory, _) = Directory::parent(&root, &path, false).unwrap();
            let mutation = directory.mutation().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "private_fs::platform::tests::mutation_lock_is_shared_between_processes",
                    "--nocapture",
                ])
                .env(FIXTURE_ROOT, &root)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(path.exists());
            drop(mutation);
            remove_file(&root, &path).unwrap();
            assert!(!path.exists());
        }

        #[test]
        fn special_and_hard_linked_files_are_never_consumed() {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("cache");
            Directory::root(&root, true).unwrap();
            let fifo = root.join("fifo");
            let fifo_name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
            // SAFETY: fifo_name is a live NUL-terminated fixture path.
            assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
            assert!(matches!(
                open_read(&root, &fifo),
                Err(CacheError::UnsafeRoot { .. })
            ));

            let body = root.join("body");
            TemporaryFile::new(&root, &body).unwrap().commit().unwrap();
            std::fs::hard_link(&body, tmp.path().join("other-name")).unwrap();
            assert!(matches!(
                open_read(&root, &body),
                Err(CacheError::UnsafeRoot { .. })
            ));
        }
    }
}

#[cfg(not(unix))]
mod platform {
    use std::fs::File;
    use std::path::Path;

    use crate::{CacheError, Result};

    fn unsupported(path: &Path) -> CacheError {
        CacheError::UnsafeRoot {
            path: path.display().to_string(),
            reason: "private descriptor-relative cache I/O is unavailable on this platform".into(),
        }
    }

    pub(super) struct Directory;

    pub(crate) type Database = rusqlite::Connection;

    #[cfg(any(feature = "local-cache", test))]
    pub(super) fn open_database(
        _root: &Path,
        path: &Path,
        _mode: super::DatabaseMode,
        _busy_timeout: std::time::Duration,
    ) -> Result<rusqlite::Connection> {
        Err(unsupported(path))
    }

    pub(super) fn open_database_at(
        _root: &Directory,
        relative: &Path,
        _mode: super::DatabaseMode,
        _busy_timeout: std::time::Duration,
    ) -> Result<rusqlite::Connection> {
        Err(unsupported(relative))
    }

    impl Directory {
        pub(super) fn root(path: &Path, _create: bool) -> Result<Self> {
            Err(unsupported(path))
        }
        #[cfg(feature = "xet-chunk-cache")]
        pub(super) fn root_with_private_parent(path: &Path) -> Result<(Self, Option<Self>)> {
            Err(unsupported(path))
        }
        pub(super) fn visit_files(
            &self,
            _visitor: &mut dyn FnMut(&Path, super::FileStat) -> Result<()>,
        ) -> Result<()> {
            Err(unsupported(Path::new("cache")))
        }
        pub(super) fn visit_selected_files(
            &self,
            _select: &dyn Fn(&Path) -> Result<bool>,
            _visitor: &mut dyn FnMut(&Path, super::FileStat) -> Result<()>,
        ) -> Result<()> {
            Err(unsupported(Path::new("cache")))
        }
        pub(super) fn inspect_entries(
            &self,
            _visitor: &mut dyn FnMut(&Path, Result<super::EntryStat>) -> Result<()>,
        ) -> Result<()> {
            Err(unsupported(Path::new("cache")))
        }
        pub(super) fn remove_relative_if(
            &self,
            relative: &Path,
            _dry_run: bool,
            _should_remove: &mut dyn FnMut(&mut File) -> Result<bool>,
        ) -> Result<Option<u64>> {
            Err(unsupported(relative))
        }
        pub(super) fn remove_relative(&self, relative: &Path) -> Result<u64> {
            Err(unsupported(relative))
        }
        pub(super) fn open_lock(&self, relative: &Path) -> Result<File> {
            Err(unsupported(relative))
        }
    }

    pub(super) fn clean(
        _root: &Directory,
        _dry_run: bool,
        _cancel: &tokio_util::sync::CancellationToken,
        _remove: &mut super::RemovePayload<'_>,
    ) -> Result<crate::CacheCleanReport> {
        Err(unsupported(Path::new("cache")))
    }

    pub(super) fn open_read(_root: &Path, path: &Path) -> Result<File> {
        Err(unsupported(path))
    }

    pub(super) fn check_directory(path: &Path, _create: bool) -> Result<()> {
        Err(unsupported(path))
    }

    #[cfg(feature = "xet-chunk-cache")]
    pub(super) fn entry_names(
        _root: &Path,
        path: &Path,
        _limit: usize,
    ) -> Result<Vec<std::ffi::OsString>> {
        Err(unsupported(path))
    }

    #[cfg(test)]
    pub(super) fn remove_file(_root: &Path, path: &Path) -> Result<u64> {
        Err(unsupported(path))
    }

    pub(super) struct TemporaryFile(File);

    impl TemporaryFile {
        #[cfg(test)]
        pub(super) fn new(_root: &Path, path: &Path) -> Result<Self> {
            Err(unsupported(path))
        }

        pub(super) fn new_at(_root: &Directory, relative: &Path) -> Result<Self> {
            Err(unsupported(relative))
        }

        pub(super) fn file(&self) -> &File {
            &self.0
        }

        #[cfg(all(feature = "remote-client", feature = "local-cache"))]
        pub(super) fn into_unlinked_file(self) -> Result<File> {
            Err(unsupported(Path::new("cache")))
        }

        pub(super) fn commit(self) -> Result<()> {
            Err(unsupported(Path::new("cache")))
        }
    }
}
