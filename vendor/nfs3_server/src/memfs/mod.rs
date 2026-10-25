//! In-memory file system for `NFSv3`.
//!
//! It is a simple implementation of a file system that stores files and directories in memory.
//! This file system is used for testing purposes and is not intended for production use.
//!
//! # Limitations
//!
//! - It's a very naive implementation and does not guarantee the best performance.
//! - Methods `symlink` and `readlink` are not implemented and return `NFS3ERR_NOTSUPP`.
//!
//! # Examples
//!
//! ```no_run
//! use nfs3_server::memfs::{MemFs, MemFsConfig};
//! use nfs3_server::tcp::{NFSTcp, NFSTcpListener};
//!
//! async fn run() -> anyhow::Result<()> {
//!     let mut config = MemFsConfig::default();
//!     config.add_file("/a.txt", "hello world\n".as_bytes());
//!     config.add_file("/b.txt", "Greetings\n".as_bytes());
//!     config.add_dir("/a directory");
//!
//!     let memfs = MemFs::new(config).unwrap();
//!     let listener = NFSTcpListener::bind("0.0.0.0:11111", memfs).await?;
//!     listener.handle_forever().await?;
//!     Ok(())
//! }
//! ```

mod config;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

pub use config::MemFsConfig;
use nfs3_types::nfs3::{
    self as nfs, cookie3, createverf3, fattr3, filename3, ftype3, nfspath3, nfsstat3, nfstime3,
    sattr3, specdata3,
};
use nfs3_types::xdr_codec::Opaque;

use crate::vfs::{
    DirEntry, DirEntryPlus, FileHandleU64, NextResult, NfsFileSystem, NfsReadFileSystem,
    ReadDirIterator, ReadDirPlusIterator,
};

const DELIMITER: char = '/';

#[derive(Debug)]
struct Dir {
    name: filename3<'static>,
    parent: FileHandleU64,
    attr: fattr3,
    content: HashSet<FileHandleU64>,
}

impl Dir {
    fn new(name: filename3<'static>, id: FileHandleU64, parent: FileHandleU64) -> Self {
        let current_time = current_time();
        let attr = fattr3 {
            type_: ftype3::NF3DIR,
            mode: 0o777,
            nlink: 1,
            uid: 507,
            gid: 507,
            size: 0,
            used: 0,
            rdev: specdata3::default(),
            fsid: 0,
            fileid: id.into(),
            atime: current_time,
            mtime: current_time,
            ctime: current_time,
        };
        Self {
            name,
            parent,
            attr,
            content: HashSet::new(),
        }
    }

    fn root_dir() -> Self {
        let name = filename3(Opaque::borrowed(b"/"));
        let id = 1.into();
        Self::new(name, id, 0.into())
    }

    fn add_entry(&mut self, entry: FileHandleU64) -> bool {
        self.content.insert(entry)
    }
}

#[derive(Debug)]
struct File {
    name: filename3<'static>,
    attr: fattr3,
    content: Vec<u8>,
    verf: createverf3,
}

impl File {
    fn new(
        name: filename3<'static>,
        id: FileHandleU64,
        content: Vec<u8>,
        verf: createverf3,
    ) -> Self {
        let current_time = current_time();
        let attr = fattr3 {
            type_: ftype3::NF3REG,
            mode: 0o755,
            nlink: 1,
            uid: 507,
            gid: 507,
            size: content.len() as u64,
            used: content.len() as u64,
            rdev: specdata3::default(),
            fsid: 0,
            fileid: id.into(),
            atime: current_time,
            mtime: current_time,
            ctime: current_time,
        };
        Self {
            name,
            attr,
            content,
            verf,
        }
    }

    fn fileid(&self) -> FileHandleU64 {
        self.attr.fileid.into()
    }

    fn resize(&mut self, size: u64) {
        self.content
            .resize(usize::try_from(size).expect("size is too large"), 0);
        self.attr.size = size;
        self.attr.used = size;
    }

    fn read(&self, offset: u64, count: u32) -> (Vec<u8>, bool) {
        let mut start = usize::try_from(offset).unwrap_or(usize::MAX);
        let mut end = start + count as usize;
        let bytes = &self.content;
        let eof = end >= bytes.len();
        if start >= bytes.len() {
            start = bytes.len();
        }
        if end > bytes.len() {
            end = bytes.len();
        }
        (bytes[start..end].to_vec(), eof)
    }

    #[allow(clippy::cast_possible_truncation)]
    fn write(&mut self, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        if offset > self.content.len() as u64 {
            return Err(nfsstat3::NFS3ERR_INVAL);
        }

        let offset = offset as usize;
        let end_offset = offset + data.len();
        if end_offset > self.content.len() {
            self.resize(end_offset as u64);
        }
        self.content[offset..end_offset].copy_from_slice(data);
        Ok(self.attr.clone())
    }
}

fn current_time() -> nfstime3 {
    let d = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("failed to get current time");
    nfstime3 {
        seconds: u32::try_from(d.as_secs()).unwrap_or(u32::MAX),
        nseconds: d.subsec_nanos(),
    }
}

#[derive(Debug)]
enum Entry {
    File(File),
    Dir(Dir),
}

impl Entry {
    fn new_file(
        name: filename3<'static>,
        parent: FileHandleU64,
        content: Vec<u8>,
        verf: createverf3,
    ) -> Self {
        Self::File(File::new(name, parent, content, verf))
    }

    fn new_dir(name: filename3<'static>, id: FileHandleU64, parent: FileHandleU64) -> Self {
        Self::Dir(Dir::new(name, id, parent))
    }

    const fn as_dir(&self) -> Result<&Dir, nfsstat3> {
        match self {
            Self::Dir(dir) => Ok(dir),
            Self::File(_) => Err(nfsstat3::NFS3ERR_NOTDIR),
        }
    }

    const fn as_dir_mut(&mut self) -> Result<&mut Dir, nfsstat3> {
        match self {
            Self::Dir(dir) => Ok(dir),
            Self::File(_) => Err(nfsstat3::NFS3ERR_NOTDIR),
        }
    }

    const fn as_file(&self) -> Result<&File, nfsstat3> {
        match self {
            Self::File(file) => Ok(file),
            Self::Dir(_) => Err(nfsstat3::NFS3ERR_ISDIR),
        }
    }

    const fn as_file_mut(&mut self) -> Result<&mut File, nfsstat3> {
        match self {
            Self::File(file) => Ok(file),
            Self::Dir(_) => Err(nfsstat3::NFS3ERR_ISDIR),
        }
    }

    fn fileid(&self) -> FileHandleU64 {
        match self {
            Self::File(file) => file.attr.fileid,
            Self::Dir(dir) => dir.attr.fileid,
        }
        .into()
    }

    const fn name(&self) -> &filename3<'static> {
        match self {
            Self::File(file) => &file.name,
            Self::Dir(dir) => &dir.name,
        }
    }

    fn set_name(&mut self, name: filename3<'static>) {
        match self {
            Self::File(file) => file.name = name,
            Self::Dir(dir) => dir.name = name,
        }
    }

    const fn attr(&self) -> &fattr3 {
        match self {
            Self::File(file) => &file.attr,
            Self::Dir(dir) => &dir.attr,
        }
    }

    const fn attr_mut(&mut self) -> &mut fattr3 {
        match self {
            Self::File(file) => &mut file.attr,
            Self::Dir(dir) => &mut dir.attr,
        }
    }

    fn set_attr(&mut self, setattr: &sattr3) {
        {
            let attr = self.attr_mut();
            match setattr.atime {
                nfs::set_atime::DONT_CHANGE => {}
                nfs::set_atime::SET_TO_CLIENT_TIME(c) => {
                    attr.atime = c;
                }
                nfs::set_atime::SET_TO_SERVER_TIME => {
                    attr.atime = current_time();
                }
            }
            match setattr.mtime {
                nfs::set_mtime::DONT_CHANGE => {}
                nfs::set_mtime::SET_TO_CLIENT_TIME(c) => {
                    attr.mtime = c;
                }
                nfs::set_mtime::SET_TO_SERVER_TIME => {
                    attr.mtime = current_time();
                }
            }
            if let nfs::set_uid3::Some(u) = setattr.uid {
                attr.uid = u;
            }
            if let nfs::set_gid3::Some(u) = setattr.gid {
                attr.gid = u;
            }
        }
        if let nfs::set_size3::Some(s) = setattr.size {
            if let Self::File(file) = self {
                file.resize(s);
            }
        }
    }
}

#[derive(Debug)]
struct Fs {
    entries: HashMap<FileHandleU64, Entry>,
    root: FileHandleU64,
}

impl Fs {
    fn new() -> Self {
        let root = Entry::Dir(Dir::root_dir());
        let fileid = root.fileid();
        let mut flat_list = HashMap::new();
        flat_list.insert(fileid, root);
        Self {
            entries: flat_list,
            root: fileid,
        }
    }

    fn push(&mut self, parent: FileHandleU64, entry: Entry) -> Result<(), nfsstat3> {
        use std::collections::hash_map::Entry as MapEntry;

        let id = entry.fileid();

        let map_entry = self.entries.entry(id);
        match map_entry {
            MapEntry::Occupied(_) => {
                tracing::warn!("object with same id already exists: {id}");
                return Err(nfsstat3::NFS3ERR_EXIST);
            }
            MapEntry::Vacant(v) => {
                v.insert(entry);
            }
        }

        let parent_entry = self.entries.get_mut(&parent);
        match parent_entry {
            None => {
                tracing::warn!("parent not found: {parent}");
                self.entries.remove(&id); // remove the entry we just added
                Err(nfsstat3::NFS3ERR_NOENT)
            }
            Some(Entry::File(_)) => {
                tracing::warn!("parent is not a directory: {parent}");
                self.entries.remove(&id); // remove the entry we just added
                Err(nfsstat3::NFS3ERR_NOTDIR)
            }
            Some(Entry::Dir(dir)) => {
                let added = dir.add_entry(id);
                assert!(added, "failed to add a new entry to directory");
                Ok(())
            }
        }
    }

    fn remove(&mut self, dirid: FileHandleU64, filename: &filename3) -> Result<(), nfsstat3> {
        if filename.as_ref() == b"." || filename.as_ref() == b".." {
            return Err(nfsstat3::NFS3ERR_INVAL);
        }

        let object_id = {
            let entry = self.entries.get(&dirid).ok_or(nfsstat3::NFS3ERR_NOENT)?;
            let dir = entry.as_dir()?;
            let id = dir
                .content
                .iter()
                .find(|i| self.entries.get(i).is_some_and(|f| f.name() == filename));
            id.copied().ok_or(nfsstat3::NFS3ERR_NOENT)?
        };

        let entry = self
            .entries
            .get(&object_id)
            .ok_or(nfsstat3::NFS3ERR_NOENT)?;
        if let Entry::Dir(dir) = entry {
            if !dir.content.is_empty() {
                return Err(nfsstat3::NFS3ERR_NOTEMPTY);
            }
        }

        self.entries.remove(&object_id);
        self.entries
            .get_mut(&dirid)
            .expect("entry not found")
            .as_dir_mut()?
            .content
            .remove(&object_id);
        Ok(())
    }

    fn get(&self, id: FileHandleU64) -> Option<&Entry> {
        self.entries.get(&id)
    }

    fn get_mut(&mut self, id: FileHandleU64) -> Option<&mut Entry> {
        self.entries.get_mut(&id)
    }

    fn lookup(&self, dirid: FileHandleU64, filename: &filename3) -> Result<&Entry, nfsstat3> {
        let entry = self.get(dirid).ok_or(nfsstat3::NFS3ERR_NOENT)?;

        if let Entry::File(_) = entry {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        } else if let Entry::Dir(dir) = &entry {
            // if looking for dir/. its the current directory
            if filename.as_ref() == b"." {
                return Ok(entry);
            }
            // if looking for dir/.. its the parent directory
            if filename.as_ref() == b".." {
                let parent = self.get(dir.parent).ok_or(nfsstat3::NFS3ERR_SERVERFAULT)?;
                return Ok(parent);
            }
            for i in dir.content.iter().copied() {
                match self.get(i) {
                    None => {
                        tracing::error!("invalid entry: {i}");
                        return Err(nfsstat3::NFS3ERR_SERVERFAULT);
                    }
                    Some(f) => {
                        if f.name() == filename {
                            return Ok(f);
                        }
                    }
                }
            }
        }
        Err(nfsstat3::NFS3ERR_NOENT)
    }

    fn rename(
        &mut self,
        from_dirid: FileHandleU64,
        from_filename: &filename3<'_>,
        to_dirid: FileHandleU64,
        to_filename: &filename3<'_>,
    ) -> Result<(), nfsstat3> {
        let from_entry = self.lookup(from_dirid, from_filename)?;
        let from_id = from_entry.fileid();
        let is_dir = matches!(from_entry, Entry::Dir(_));

        // ✔️ source entry exists

        if from_dirid == to_dirid && from_filename == to_filename {
            // if source and target are the same, we can skip the rename
            return Ok(());
        }

        let to_entry = self.lookup(to_dirid, to_filename);

        // ✔️ target directory exists

        match (from_entry, to_entry) {
            (_, Err(nfsstat3::NFS3ERR_NOENT)) => {
                // the entry does not exist, we can rename
            }
            (Entry::File(_), Ok(Entry::File(_))) => {
                // if both entries are files, we can rename
                self.remove(to_dirid, to_filename)?;
            }
            (Entry::Dir(_), Ok(Entry::Dir(tgt_dir))) => {
                // if both entries are directories, we can rename if the target directory is empty
                if !tgt_dir.content.is_empty() {
                    tracing::warn!("target directory is not empty");
                    return Err(nfsstat3::NFS3ERR_NOTEMPTY);
                }
                self.remove(to_dirid, to_filename)?;
            }
            (Entry::File(_), Ok(Entry::Dir(_))) => {
                // cannot rename a file to a directory
                tracing::warn!("cannot rename file to directory");
                return Err(nfsstat3::NFS3ERR_NOTDIR);
            }
            (Entry::Dir(_), Ok(Entry::File(_))) => {
                // cannot rename a directory to a file
                tracing::warn!("cannot rename directory to file");
                return Err(nfsstat3::NFS3ERR_NOTDIR);
            }
            (_, Err(e)) => {
                // unexpected error, we should not continue
                return Err(e);
            }
        }

        // ✔️ target entry doesn't exist

        // Prevent renaming a directory into its own subdirectory
        if is_dir {
            let mut current = to_dirid;
            loop {
                if current == from_id {
                    tracing::warn!("cannot move a directory into its own subdirectory");
                    return Err(nfsstat3::NFS3ERR_INVAL);
                }
                let entry = self.get(current).ok_or(nfsstat3::NFS3ERR_NOENT)?;
                match entry {
                    Entry::Dir(dir) => {
                        if current == self.root {
                            // Reached root
                            break;
                        }
                        current = dir.parent;
                    }
                    Entry::File(_) => {
                        tracing::error!("expected a directory, found a file");
                        return Err(nfsstat3::NFS3ERR_SERVERFAULT);
                    }
                }
            }
        }

        // Remove from old parent directory
        {
            let from_dir = self
                .get_mut(from_dirid)
                .ok_or(nfsstat3::NFS3ERR_SERVERFAULT)?;
            from_dir.as_dir_mut()?.content.remove(&from_id);
        }

        // Add to new parent directory
        {
            let to_dir = self
                .get_mut(to_dirid)
                .ok_or(nfsstat3::NFS3ERR_SERVERFAULT)?;
            let added = to_dir.as_dir_mut()?.content.insert(from_id);
            if !added {
                tracing::error!("failed to add entry to target directory");
                return Err(nfsstat3::NFS3ERR_SERVERFAULT);
            }
        }

        // Update entry's name and parent if needed
        let entry = self.get_mut(from_id).ok_or(nfsstat3::NFS3ERR_SERVERFAULT)?;
        entry.set_name(to_filename.clone_to_owned());
        if let Entry::Dir(dir) = entry {
            dir.parent = to_dirid;
        }

        Ok(())
    }
}

/// In-memory file system for `NFSv3`.
///
/// `MemFs` implements the [`NfsFileSystem`] trait and provides a simple in-memory file system
#[derive(Debug)]
pub struct MemFs {
    fs: Arc<RwLock<Fs>>,
    rootdir: FileHandleU64,
    nextid: AtomicU64,
}

impl Default for MemFs {
    fn default() -> Self {
        let root = Fs::new();
        let rootdir = root.root;
        let nextid = AtomicU64::new(rootdir.as_u64() + 1);
        Self {
            fs: Arc::new(RwLock::new(root)),
            rootdir,
            nextid,
        }
    }
}

impl MemFs {
    /// Creates a new in-memory file system with the given configuration.
    pub fn new(config: MemFsConfig) -> Result<Self, nfsstat3> {
        tracing::info!("creating memfs. Entries count: {}", config.entries.len());
        let fs = Self::default();

        for entry in config.entries {
            let id = fs.path_to_id_impl(&entry.parent)?;
            let name = filename3(Opaque::owned(entry.name.into_bytes()));
            if entry.is_dir {
                fs.add_dir(id, name)?;
            } else {
                fs.add_file(id, name, &sattr3::default(), entry.content, None)?;
            }
        }

        Ok(fs)
    }

    fn add_dir(
        &self,
        dirid: FileHandleU64,
        dirname: filename3<'static>,
    ) -> Result<(FileHandleU64, fattr3), nfsstat3> {
        let newid: FileHandleU64 = self
            .nextid
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .into();

        let dir = Entry::new_dir(dirname, newid, dirid);
        let attr = dir.attr().clone();

        self.fs
            .write()
            .expect("lock is poisoned")
            .push(dirid, dir)?;

        Ok((newid, attr))
    }

    fn add_file(
        &self,
        dirid: FileHandleU64,
        filename: filename3<'static>,
        attr: &sattr3,
        content: Vec<u8>,
        verf: Option<createverf3>,
    ) -> Result<(FileHandleU64, fattr3), nfsstat3> {
        let newid: FileHandleU64 = self
            .nextid
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .into();

        let mut file = Entry::new_file(filename, newid, content, verf.unwrap_or_default());
        file.set_attr(attr);
        let attr = file.attr().clone();

        let mut fs_lock = self.fs.write().expect("lock is poisoned");
        match fs_lock.lookup(dirid, file.name()) {
            Err(nfsstat3::NFS3ERR_NOENT) => {
                // the existing file does not exist, we can add the new file
            }
            Ok(existing_file) => {
                if let Entry::File(existing_file) = existing_file {
                    if verf.is_some_and(|v| v == existing_file.verf) {
                        return Ok((existing_file.fileid(), attr));
                    }
                }
                return Err(nfsstat3::NFS3ERR_EXIST);
            }
            Err(e) => {
                // unexpected error, we should not continue
                return Err(e);
            }
        }
        fs_lock.push(dirid, file)?;

        Ok((newid, attr))
    }

    fn path_to_id_impl(&self, path: &str) -> Result<FileHandleU64, nfsstat3> {
        let splits = path.split(DELIMITER);
        let mut fid = self.root_dir();
        let fs = self.fs.read().expect("lock is poisoned");
        for component in splits {
            if component.is_empty() {
                continue;
            }
            let entry = fs.lookup(fid, &component.as_bytes().into())?;
            fid = entry.fileid();
        }
        Ok(fid)
    }

    fn make_iter(
        &self,
        dirid: FileHandleU64,
        start_after: cookie3,
    ) -> Result<MemFsIterator, nfsstat3> {
        let fs = self.fs.read().expect("lock is poisoned");
        let entry = fs.get(dirid).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        let dir = entry.as_dir()?;

        let mut iter = dir.content.iter();
        if start_after != 0 {
            // skip to the start_after entry
            let find_result = iter.find(|i| **i == start_after);
            if find_result.is_none() {
                return Err(nfsstat3::NFS3ERR_BAD_COOKIE);
            }
        }
        let content: Vec<_> = iter.copied().collect();
        Ok(MemFsIterator::new(self.fs.clone(), content))
    }
}

impl NfsReadFileSystem for MemFs {
    type Handle = FileHandleU64;

    fn root_dir(&self) -> FileHandleU64 {
        self.rootdir
    }

    async fn lookup(
        &self,
        dirid: &FileHandleU64,
        filename: &filename3<'_>,
    ) -> Result<FileHandleU64, nfsstat3> {
        let fs = self.fs.read().expect("lock is poisoned");
        fs.lookup(*dirid, filename).map(Entry::fileid)
    }

    async fn getattr(&self, id: &FileHandleU64) -> Result<fattr3, nfsstat3> {
        let fs = self.fs.read().expect("lock is poisoned");
        let entry = fs.get(*id).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        Ok(entry.attr().clone())
    }

    async fn read(
        &self,
        id: &FileHandleU64,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        let fs = self.fs.read().expect("lock is poisoned");
        let entry = fs.get(*id).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        let file = entry.as_file()?;
        Ok(file.read(offset, count))
    }

    async fn readdir(
        &self,
        dirid: &FileHandleU64,
        cookie: u64,
    ) -> Result<impl ReadDirIterator, nfsstat3> {
        let iter = Self::make_iter(self, *dirid, cookie)?;
        Ok(iter)
    }

    async fn readdirplus(
        &self,
        dirid: &FileHandleU64,
        cookie: u64,
    ) -> Result<impl ReadDirPlusIterator<FileHandleU64>, nfsstat3> {
        let iter = Self::make_iter(self, *dirid, cookie)?;
        Ok(iter)
    }

    async fn readlink(&self, _id: &FileHandleU64) -> Result<nfspath3<'_>, nfsstat3> {
        tracing::warn!("readlink not implemented");
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }

    async fn lookup_by_path(&self, path: &str) -> Result<FileHandleU64, nfsstat3> {
        self.path_to_id_impl(path)
    }
}

impl NfsFileSystem for MemFs {
    async fn setattr(&self, id: &FileHandleU64, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let mut fs = self.fs.write().expect("lock is poisoned");
        let entry = fs.get_mut(*id).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        entry.set_attr(&setattr);
        Ok(entry.attr().clone())
    }

    async fn write(
        &self,
        id: &FileHandleU64,
        offset: u64,
        data: &[u8],
        _stable: nfs::stable_how, // MemFs writes are always `FILE_SYNC`
    ) -> Result<(fattr3, nfs::stable_how), nfsstat3> {
        let mut fs = self.fs.write().expect("lock is poisoned");

        let entry = fs.get_mut(*id).ok_or(nfsstat3::NFS3ERR_NOENT)?;
        let file = entry.as_file_mut().map_err(|_| nfsstat3::NFS3ERR_INVAL)?;
        let attr = file.write(offset, data)?;
        Ok((attr, nfs::stable_how::FILE_SYNC))
    }

    async fn create(
        &self,
        dirid: &FileHandleU64,
        filename: &filename3<'_>,
        attr: sattr3,
    ) -> Result<(FileHandleU64, fattr3), nfsstat3> {
        self.add_file(*dirid, filename.clone_to_owned(), &attr, Vec::new(), None)
    }

    async fn create_exclusive(
        &self,
        dirid: &FileHandleU64,
        filename: &filename3<'_>,
        createverf: nfs::createverf3,
    ) -> Result<FileHandleU64, nfsstat3> {
        self.add_file(
            *dirid,
            filename.clone_to_owned(),
            &sattr3::default(),
            Vec::new(),
            Some(createverf),
        )
        .map(|(id, _attr)| id)
    }

    async fn mkdir(
        &self,
        dirid: &FileHandleU64,
        dirname: &filename3<'_>,
    ) -> Result<(FileHandleU64, fattr3), nfsstat3> {
        self.add_dir(*dirid, dirname.clone_to_owned())
    }

    async fn remove(
        &self,
        dirid: &FileHandleU64,
        filename: &filename3<'_>,
    ) -> Result<(), nfsstat3> {
        self.fs
            .write()
            .expect("lock is poisoned")
            .remove(*dirid, filename)
    }

    async fn rename<'a>(
        &self,
        from_dirid: &FileHandleU64,
        from_filename: &filename3<'a>,
        to_dirid: &FileHandleU64,
        to_filename: &filename3<'a>,
    ) -> Result<(), nfsstat3> {
        let mut fs = self.fs.write().expect("lock is poisoned");
        fs.rename(*from_dirid, from_filename, *to_dirid, to_filename)
    }

    async fn symlink<'a>(
        &self,
        _dirid: &FileHandleU64,
        _linkname: &filename3<'a>,
        _symlink: &nfspath3<'a>,
        _attr: &sattr3,
    ) -> Result<(FileHandleU64, fattr3), nfsstat3> {
        tracing::warn!("symlink not implemented");
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }

    async fn commit(&self, _id: &FileHandleU64, _offset: u64, _count: u32) -> Result<(), nfsstat3> {
        // In-memory filesystem: all writes are immediately "committed".
        Ok(())
    }
}

struct MemFsIterator {
    fs: Arc<RwLock<Fs>>,
    entries: Vec<FileHandleU64>,
    index: usize,
}

impl MemFsIterator {
    const fn new(fs: Arc<RwLock<Fs>>, entries: Vec<FileHandleU64>) -> Self {
        Self {
            fs,
            entries,
            index: 0,
        }
    }

    fn visit_next_entry<R>(&mut self, f: fn(FileHandleU64, &Entry) -> R) -> NextResult<R> {
        loop {
            if self.index >= self.entries.len() {
                return NextResult::Eof;
            }
            let id = self.entries[self.index];
            self.index += 1;

            let fs = self.fs.read().expect("lock is poisoned");
            let entry = fs.get(id);
            let Some(entry) = entry else {
                // skip missing entries
                tracing::warn!("entry not found: {id}");
                continue;
            };
            return NextResult::Ok(f(id, entry));
        }
    }
}

impl ReadDirIterator for MemFsIterator {
    async fn next(&mut self) -> NextResult<DirEntry> {
        self.visit_next_entry(|id, entry| DirEntry {
            fileid: id.into(),
            name: entry.name().clone_to_owned(),
            cookie: id.into(),
        })
    }
}

impl ReadDirPlusIterator<FileHandleU64> for MemFsIterator {
    async fn next(&mut self) -> NextResult<DirEntryPlus<FileHandleU64>> {
        self.visit_next_entry(|id, entry| {
            let attr = entry.attr().clone();
            DirEntryPlus {
                fileid: id.into(),
                name: entry.name().clone_to_owned(),
                cookie: id.into(),
                name_attributes: Some(attr),
                name_handle: Some(id),
            }
        })
    }
}
