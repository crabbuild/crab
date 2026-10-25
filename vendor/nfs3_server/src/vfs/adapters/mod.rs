//! Adapters VFS types

mod iterator;

pub use iterator::ReadDirPlusToReadDir;
use nfs3_types::nfs3::{fattr3, filename3, nfsstat3, sattr3};

use super::{
    DirEntryPlus, NextResult, NfsFileSystem, NfsReadFileSystem, ReadDirIterator,
    ReadDirPlusIterator, VFSCapabilities,
};
use crate::vfs::FileHandle;

/// An internal adapter that allows to reuse the same code with `ReadOnly` filesystems.
///
/// In general, you should not use this adapter directly. Instead, use the
/// [`NFSTcpListener::bind_ro`][1] method to bind a read-only NFS server.
///
/// [1]: crate::tcp::NFSTcpListener::bind_ro
pub struct ReadOnlyAdapter<T>(T);

impl<T> ReadOnlyAdapter<T>
where
    T: NfsReadFileSystem,
{
    pub const fn new(inner: T) -> Self {
        Self(inner)
    }
}

impl<T> NfsReadFileSystem for ReadOnlyAdapter<T>
where
    T: NfsReadFileSystem,
{
    type Handle = T::Handle;

    fn root_dir(&self) -> Self::Handle {
        self.0.root_dir()
    }

    async fn lookup(
        &self,
        dirid: &Self::Handle,
        filename: &filename3<'_>,
    ) -> Result<Self::Handle, nfsstat3> {
        self.0.lookup(dirid, filename).await
    }

    async fn getattr(&self, id: &Self::Handle) -> Result<fattr3, nfsstat3> {
        let mut result = self.0.getattr(id).await;
        if let Ok(attr) = &mut result {
            remove_write_permissions(attr);
        }
        result
    }

    async fn read(
        &self,
        id: &Self::Handle,
        offset: u64,
        count: u32,
    ) -> Result<(Vec<u8>, bool), nfsstat3> {
        self.0.read(id, offset, count).await
    }

    async fn readdir(
        &self,
        dirid: &Self::Handle,
        cookie3: u64,
    ) -> Result<impl ReadDirIterator, nfsstat3> {
        self.0.readdir(dirid, cookie3).await
    }

    async fn readdirplus(
        &self,
        dirid: &Self::Handle,
        cookie3: u64,
    ) -> Result<impl ReadDirPlusIterator<Self::Handle>, nfsstat3> {
        self.0
            .readdirplus(dirid, cookie3)
            .await
            .map(ReadOnlyIterator)
    }

    async fn readlink(
        &self,
        id: &Self::Handle,
    ) -> Result<nfs3_types::nfs3::nfspath3<'_>, nfsstat3> {
        self.0.readlink(id).await
    }
}

impl<T> NfsFileSystem for ReadOnlyAdapter<T>
where
    T: NfsReadFileSystem,
{
    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadOnly
    }

    async fn setattr(&self, _id: &Self::Handle, _setattr: sattr3) -> Result<fattr3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn write(
        &self,
        _id: &Self::Handle,
        _offset: u64,
        _data: &[u8],
        _stable: nfs3_types::nfs3::stable_how,
    ) -> Result<(fattr3, nfs3_types::nfs3::stable_how), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn create(
        &self,
        _dirid: &Self::Handle,
        _filename: &filename3<'_>,
        _attr: sattr3,
    ) -> Result<(Self::Handle, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn create_exclusive(
        &self,
        _dirid: &Self::Handle,
        _filename: &filename3<'_>,
        _createverf: nfs3_types::nfs3::createverf3,
    ) -> Result<Self::Handle, nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn mkdir(
        &self,
        _dirid: &Self::Handle,
        _dirname: &filename3<'_>,
    ) -> Result<(Self::Handle, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn remove(
        &self,
        _dirid: &Self::Handle,
        _filename: &filename3<'_>,
    ) -> Result<(), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn rename<'a>(
        &self,
        _from_dirid: &Self::Handle,
        _from_filename: &filename3<'a>,
        _to_dirid: &Self::Handle,
        _to_filename: &filename3<'a>,
    ) -> Result<(), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn symlink<'a>(
        &self,
        _dirid: &Self::Handle,
        _linkname: &filename3<'a>,
        _symlink: &nfs3_types::nfs3::nfspath3<'a>,
        _attr: &sattr3,
    ) -> Result<(Self::Handle, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }

    async fn commit(&self, _id: &Self::Handle, _offset: u64, _count: u32) -> Result<(), nfsstat3> {
        Err(nfsstat3::NFS3ERR_ROFS)
    }
}

#[derive(Debug)]
struct ReadOnlyIterator<T>(T);

impl<H, T> ReadDirPlusIterator<H> for ReadOnlyIterator<T>
where
    H: FileHandle,
    T: ReadDirPlusIterator<H>,
{
    async fn next(&mut self) -> NextResult<DirEntryPlus<H>> {
        let mut result = self.0.next().await;
        if let NextResult::Ok(entry) = &mut result {
            if let Some(attr) = &mut entry.name_attributes {
                remove_write_permissions(attr);
            }
        }
        result
    }
}

const fn remove_write_permissions(attr: &mut fattr3) {
    attr.mode &= 0o555; // Read-only permissions
}
