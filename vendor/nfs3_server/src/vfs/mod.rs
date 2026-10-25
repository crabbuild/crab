//! The basic API to implement to provide an NFS file system
//!
//! Opaque FH
//! ---------
//! Files are only uniquely identified by a 64-bit file id. (basically an inode number)
//! We automatically produce internally the opaque filehandle which is comprised of
//!  - A 64-bit generation number derived from the server startup time (i.e. so the opaque file
//!    handle expires when the NFS server restarts)
//!  - The 64-bit file id
//!
//! readdir pagination
//! ------------------
//! We do not use cookie verifier. We just use the `start_after`.  The
//! implementation should allow startat to start at any position. That is,
//! the next query to readdir may be the last entry in the previous readdir
//! response.
//!
//! Other requirements
//! ------------------
//!  getattr needs to be fast. NFS uses that a lot
//!
//!  The 0 fileid is reserved and should not be used

pub mod adapters;
pub(crate) mod handle;
mod iterator;

pub use handle::{FileHandle, FileHandleU64};
pub use iterator::*;

use crate::nfs3_types::nfs3::{
    FSF3_CANSETTIME, FSF3_HOMOGENEOUS, FSF3_SYMLINK, FSINFO3resok as fsinfo3, createverf3, fattr3,
    filename3, nfspath3, nfsstat3, nfstime3, post_op_attr, sattr3, stable_how,
};
use crate::units::{GIBIBYTE, MEBIBYTE};
use crate::vfs::adapters::ReadDirPlusToReadDir;

/// What capabilities are supported
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VFSCapabilities {
    ReadOnly,
    ReadWrite,
}

/// Read-only file system interface
///
/// This should be enough to implement a read-only NFS server.
/// If you want to implement a read-write server, you should implement
/// the [`NfsFileSystem`] trait too.
pub trait NfsReadFileSystem: Send + Sync {
    /// Type that can be used to indentify a file or folder in the file system.
    ///
    /// For more information, see [`FileHandle`].
    type Handle: FileHandle;

    /// Returns the ID the of the root directory "/"
    fn root_dir(&self) -> Self::Handle;

    /// Look up the id of a path in a directory
    ///
    /// i.e. given a directory dir/ containing a file `a.txt`
    /// this may call `lookup(id_of("dir/"), "a.txt")`
    /// and this should return the id of the file `dir/a.txt`
    ///
    /// This method should be fast as it is used very frequently.
    fn lookup(
        &self,
        dirid: &Self::Handle,
        filename: &filename3<'_>,
    ) -> impl Future<Output = Result<Self::Handle, nfsstat3>> + Send;

    /// This method is used when the client tries to mount a subdirectory.
    /// The default implementation walks the directory structure with [`lookup`](Self::lookup).
    fn lookup_by_path(
        &self,
        path: &str,
    ) -> impl Future<Output = Result<Self::Handle, nfsstat3>> + Send {
        async move {
            let splits = path.split('/');
            let mut fid = self.root_dir();
            for component in splits {
                if component.is_empty() {
                    continue;
                }
                fid = self.lookup(&fid, &component.as_bytes().into()).await?;
            }
            Ok(fid)
        }
    }

    /// Returns the attributes of an id.
    /// This method should be fast as it is used very frequently.
    fn getattr(&self, id: &Self::Handle) -> impl Future<Output = Result<fattr3, nfsstat3>> + Send;

    /// Reads the contents of a file returning (bytes, EOF)
    /// Note that offset/count may go past the end of the file and that
    /// in that case, all bytes till the end of file are returned.
    /// EOF must be flagged if the end of the file is reached by the read.
    fn read(
        &self,
        id: &Self::Handle,
        offset: u64,
        count: u32,
    ) -> impl Future<Output = Result<(Vec<u8>, bool), nfsstat3>> + Send;

    /// Simple version of readdir. Only need to return filename and id
    ///
    /// By default it uses `readdirplus` method to create an iterator
    fn readdir(
        &self,
        dirid: &Self::Handle,
        cookie: u64,
    ) -> impl Future<Output = Result<impl ReadDirIterator, nfsstat3>> + Send {
        async move {
            self.readdirplus(dirid, cookie)
                .await
                .map(ReadDirPlusToReadDir::new)
        }
    }

    /// Returns the contents of a directory with pagination.
    /// Directory listing should be deterministic.
    /// Up to `max_entries` may be returned, and `start_after` is used
    /// to determine where to start returning entries from.
    ///
    /// For instance if the directory has entry with ids `[1,6,2,11,8,9]`
    /// and `start_after=6`, readdir should returning 2,11,8,...
    fn readdirplus(
        &self,
        dirid: &Self::Handle,
        cookie: u64,
    ) -> impl Future<Output = Result<impl ReadDirPlusIterator<Self::Handle>, nfsstat3>> + Send;

    /// Reads a symlink
    fn readlink(
        &self,
        id: &Self::Handle,
    ) -> impl Future<Output = Result<nfspath3<'_>, nfsstat3>> + Send;

    /// Get static file system Information
    fn fsinfo(
        &self,
        root_fileid: &Self::Handle,
    ) -> impl Future<Output = Result<fsinfo3, nfsstat3>> + Send {
        async move {
            let dir_attr = self
                .getattr(root_fileid)
                .await
                .map_or(post_op_attr::None, post_op_attr::Some);

            let res = fsinfo3 {
                obj_attributes: dir_attr,
                rtmax: MEBIBYTE,
                rtpref: MEBIBYTE,
                rtmult: MEBIBYTE,
                wtmax: MEBIBYTE,
                wtpref: MEBIBYTE,
                wtmult: MEBIBYTE,
                dtpref: MEBIBYTE,
                maxfilesize: 128u64 * GIBIBYTE,
                time_delta: nfstime3 {
                    seconds: 0,
                    nseconds: 1_000_000,
                },
                properties: FSF3_SYMLINK | FSF3_HOMOGENEOUS | FSF3_CANSETTIME,
            };
            Ok(res)
        }
    }
}

/// Write file system interface
///
/// This is the interface to implement if you want to provide a writable NFS server.
pub trait NfsFileSystem: NfsReadFileSystem {
    /// Returns the set of capabilities supported
    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }

    /// Sets the attributes of an id
    /// this should return `Err(nfsstat3::NFS3ERR_ROFS)` if readonly
    fn setattr(
        &self,
        id: &Self::Handle,
        setattr: sattr3,
    ) -> impl Future<Output = Result<fattr3, nfsstat3>> + Send;

    /// Writes the contents of a file.
    ///
    /// If the offset and count go past the end of the file, the file is extended.
    /// If not supported due to readonly file system this should return
    /// `Err(nfsstat3::NFS3ERR_ROFS)`
    ///
    /// # Returns
    ///
    /// On success, returns `(fattr3, stable_how)` where:
    /// - [`fattr3`] contains the updated file attributes after the write.
    /// - [`stable_how`] reflects the level of stability actually achieved, which must be equal to
    ///   or greater than the requested `stable` level.
    ///
    /// # `stable` parameter
    ///
    /// - [`stable_how::FILE_SYNC`]: all data and metadata must be committed to stable storage
    ///   before returning. Any other behavior is a protocol violation.
    /// - [`stable_how::DATA_SYNC`]: all data and enough metadata to retrieve it must be committed
    ///   before returning. Could be implemented identically to `FILE_SYNC`.
    /// - [`stable_how::UNSTABLE`]: the server may defer committing any data or metadata.
    ///   Uncommitted data can later be flushed via [`commit`][NfsFileSystem::commit]. Usually,
    ///   clients will send a series of UNSTABLE writes followed by a
    ///   [`commit`][NfsFileSystem::commit], so servers can optimize for this case by deferring the
    ///   actual disk writes until the [`commit`][NfsFileSystem::commit] is received.
    ///
    /// # `NFS3ERR_INVAL`:
    ///
    /// Some NFS version 2 protocol server implementations
    /// incorrectly returned `NFSERR_ISDIR` if the file system
    /// object type was not a regular file. The correct return
    /// value for the NFS version 3 protocol is `NFS3ERR_INVAL`.
    fn write(
        &self,
        id: &Self::Handle,
        offset: u64,
        data: &[u8],
        stable: stable_how,
    ) -> impl Future<Output = Result<(fattr3, stable_how), nfsstat3>> + Send;

    /// Creates a file with the following attributes.
    /// If not supported due to readonly file system
    /// this should return `Err(nfsstat3::NFS3ERR_ROFS)`
    fn create(
        &self,
        dirid: &Self::Handle,
        filename: &filename3<'_>,
        attr: sattr3,
    ) -> impl Future<Output = Result<(Self::Handle, fattr3), nfsstat3>> + Send;

    /// Creates a file if it does not already exist.
    /// If not supported due to readonly file system
    /// this should return `Err(nfsstat3::NFS3ERR_ROFS)`
    ///
    /// # NOTE:
    /// If the server can not support these exclusive create
    /// semantics, possibly because of the requirement to commit
    /// the verifier to stable storage, it should fail the CREATE
    /// request with the error, `NFS3ERR_NOTSUPP`.
    fn create_exclusive(
        &self,
        dirid: &Self::Handle,
        filename: &filename3<'_>,
        createverf: createverf3,
    ) -> impl Future<Output = Result<Self::Handle, nfsstat3>> + Send;

    /// Makes a directory with the following attributes.
    /// If not supported dur to readonly file system
    /// this should return `Err(nfsstat3::NFS3ERR_ROFS)`
    fn mkdir(
        &self,
        dirid: &Self::Handle,
        dirname: &filename3<'_>,
    ) -> impl Future<Output = Result<(Self::Handle, fattr3), nfsstat3>> + Send;

    /// Removes a file.
    /// If not supported due to readonly file system
    /// this should return `Err(nfsstat3::NFS3ERR_ROFS)`
    fn remove(
        &self,
        dirid: &Self::Handle,
        filename: &filename3<'_>,
    ) -> impl Future<Output = Result<(), nfsstat3>> + Send;

    /// Removes a file.
    /// If not supported due to readonly file system
    /// this should return `Err(nfsstat3::NFS3ERR_ROFS)`
    ///
    /// # NOTE:
    ///
    /// If the directory, `to_dirid`, already contains an entry with
    /// the name, `to_filename`, the source object must be compatible
    /// with the target: either both are non-directories or both
    /// are directories and the target must be empty. If
    /// compatible, the existing target is removed before the
    /// rename occurs. If they are not compatible or if the target
    /// is a directory but not empty, the server should return the
    /// error, `NFS3ERR_EXIST`.
    fn rename<'a>(
        &self,
        from_dirid: &Self::Handle,
        from_filename: &filename3<'a>,
        to_dirid: &Self::Handle,
        to_filename: &filename3<'a>,
    ) -> impl Future<Output = Result<(), nfsstat3>> + Send;

    /// Makes a symlink with the following attributes.
    /// If not supported due to readonly file system
    /// this should return `Err(nfsstat3::NFS3ERR_ROFS)`
    fn symlink<'a>(
        &self,
        dirid: &Self::Handle,
        linkname: &filename3<'a>,
        symlink: &nfspath3<'a>,
        attr: &sattr3,
    ) -> impl Future<Output = Result<(Self::Handle, fattr3), nfsstat3>> + Send;

    /// Commits previously written data to stable storage.
    ///
    /// `offset` and `count` indicate the region to commit. If `count` is 0
    /// the entire file should be committed.
    fn commit(
        &self,
        id: &Self::Handle,
        offset: u64,
        count: u32,
    ) -> impl Future<Output = Result<(), nfsstat3>> + Send;
}
