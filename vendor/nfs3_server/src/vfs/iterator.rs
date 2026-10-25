use crate::nfs3_types::nfs3::{
    cookie3, entry3, entryplus3, fattr3, fileid3, filename3, nfsstat3, post_op_attr, post_op_fh3,
};
use crate::vfs::FileHandle;
use crate::vfs::handle::FileHandleConverter;

/// Same as `entry3`
pub type DirEntry = entry3<'static>;

/// Represents `entryplus3` with Handle instead of `nfs3_fh`
#[derive(Debug, Clone)]
pub struct DirEntryPlus<H: FileHandle> {
    pub fileid: fileid3,
    pub name: filename3<'static>,
    pub cookie: cookie3,
    pub name_attributes: Option<fattr3>,
    pub name_handle: Option<H>,
}

impl<H: FileHandle> DirEntryPlus<H> {
    pub(crate) fn into_entry(self, converter: &FileHandleConverter) -> entryplus3<'static> {
        entryplus3 {
            fileid: self.fileid,
            name: self.name,
            cookie: self.cookie,
            name_attributes: self
                .name_attributes
                .map_or(post_op_attr::None, post_op_attr::Some),
            name_handle: self.name_handle.map_or(post_op_fh3::None, |h| {
                post_op_fh3::Some(converter.fh_to_nfs(&h))
            }),
        }
    }
}

/// Represents the result of `next()` in [`ReadDirIterator`] and [`ReadDirPlusIterator`].
pub enum NextResult<T> {
    /// The next entry in the directory. It's either [`DirEntry`] or [`DirEntryPlus`].
    Ok(T),
    /// The end of the directory has been reached. It is not an error.
    Eof,
    /// An error occurred while reading the directory.
    Err(nfsstat3),
}

/// Iterator for [`NfsReadFileSystem::readdir`](super::NfsReadFileSystem::readdir)
pub trait ReadDirIterator: Send + Sync {
    /// Returns the next entry in the directory.
    fn next(&mut self) -> impl Future<Output = NextResult<DirEntry>> + Send;
}

/// Iterator for [`NfsReadFileSystem::readdirplus`](super::NfsReadFileSystem::readdirplus)
pub trait ReadDirPlusIterator<H: FileHandle>: Send + Sync {
    /// Returns the next entry in the directory.
    fn next(&mut self) -> impl Future<Output = NextResult<DirEntryPlus<H>>> + Send;
}
