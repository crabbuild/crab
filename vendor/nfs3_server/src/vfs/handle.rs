use nfs3_types::nfs3::{nfs_fh3, nfsstat3, writeverf3};
use nfs3_types::xdr_codec::Opaque;

/// Represents a file handle
///
/// This uniquely identifies a file or folder in the implementation of
/// [`NfsReadFileSystem`][1] and [`NfsFileSystem`][2]. The value is serialized
/// into a [`nfs_fh3`] handle and sent to the client. The server reserves
/// the first 8 bytes of the handle for its own use, while the remaining
/// 56 bytes can be freely used by the implementation.
///
/// [1]: crate::vfs::NfsReadFileSystem
/// [2]: crate::vfs::NfsFileSystem
#[expect(clippy::len_without_is_empty)]
pub trait FileHandle: std::fmt::Debug + Clone + Send + Sync {
    /// The length of the handle in bytes
    fn len(&self) -> usize;
    /// Returns the handle as a byte slice
    fn as_bytes(&self) -> &[u8];
    /// Creates a handle from a byte slice
    fn from_bytes(bytes: &[u8]) -> Option<Self>
    where
        Self: Sized;
}

/// A basic 8-bytes long file handle
///
/// If your implementation of [`NfsReadFileSystem`][1] uses a file handle that is
/// 8 bytes long, you can use this type instead of creating you own.
///
/// [1]: crate::vfs::NfsReadFileSystem
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileHandleU64 {
    id: [u8; 8],
}

impl FileHandleU64 {
    /// Creates a new file handle from a u64
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self {
            id: id.to_ne_bytes(),
        }
    }

    /// Converts the file handle to a u64
    #[must_use]
    pub const fn as_u64(&self) -> u64 {
        u64::from_ne_bytes(self.id)
    }
}

impl FileHandle for FileHandleU64 {
    fn len(&self) -> usize {
        self.id.len()
    }
    fn as_bytes(&self) -> &[u8] {
        &self.id
    }
    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(|id| Self { id })
    }
}

impl std::fmt::Debug for FileHandleU64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("FileHandleU64")
            .field(&u64::from_ne_bytes(self.id))
            .finish()
    }
}

impl std::fmt::Display for FileHandleU64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_u64())
    }
}

impl From<u64> for FileHandleU64 {
    fn from(id: u64) -> Self {
        Self::new(id)
    }
}

impl From<FileHandleU64> for u64 {
    fn from(val: FileHandleU64) -> Self {
        val.as_u64()
    }
}

impl PartialEq<u64> for FileHandleU64 {
    fn eq(&self, other: &u64) -> bool {
        &self.as_u64() == other
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FileHandleConverter {
    generation_number: u64,
    generation_number_le: [u8; 8],
}

impl FileHandleConverter {
    #[allow(clippy::cast_possible_truncation)] // it's ok to truncate the generation number
    pub(crate) fn new() -> Self {
        let generation_number = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("failed to get system time")
            .as_millis() as u64;

        Self::with_generation_number(generation_number)
    }

    /// Creates a `FileHandleConverter` with a specific generation number.
    ///
    /// Use this when multiple NFS server instances need to share the same
    /// generation number so that file handles remain valid across all
    /// instances (e.g. behind a load balancer).
    #[must_use]
    pub const fn with_generation_number(generation_number: u64) -> Self {
        Self {
            generation_number,
            generation_number_le: generation_number.to_le_bytes(),
        }
    }

    pub(crate) fn fh_to_nfs(&self, id: &impl FileHandle) -> nfs_fh3 {
        let mut ret: Vec<u8> = Vec::with_capacity(8 + id.len());
        ret.extend_from_slice(&self.generation_number_le);
        ret.extend_from_slice(id.as_bytes());
        nfs_fh3 {
            data: Opaque::owned(ret),
        }
    }

    pub(crate) fn fh_from_nfs<FH>(&self, id: &nfs_fh3) -> Result<FH, nfsstat3>
    where
        FH: FileHandle,
    {
        self.check_handle(id)?;

        FH::from_bytes(&id.data[8..]).ok_or(nfsstat3::NFS3ERR_BADHANDLE)
    }

    fn check_handle(&self, id: &nfs_fh3) -> Result<(), nfsstat3> {
        if id.data.len() < 8 {
            return Err(nfsstat3::NFS3ERR_BADHANDLE);
        }
        if id.data[0..8] == self.generation_number_le {
            Ok(())
        } else {
            let id_gen = u64::from_le_bytes(
                id.data[0..8]
                    .try_into()
                    .map_err(|_| nfsstat3::NFS3ERR_BADHANDLE)?,
            );
            if id_gen < self.generation_number {
                Err(nfsstat3::NFS3ERR_STALE)
            } else {
                Err(nfsstat3::NFS3ERR_BADHANDLE)
            }
        }
    }

    /// This is a cookie that the client can use to determine
    /// whether the server has rebooted between a call to WRITE
    /// and a subsequent call to COMMIT. This cookie must be
    /// consistent during a single boot session and must be
    /// unique between instances of the NFS version 3 protocol
    /// server where uncommitted data may be lost.
    pub const fn verf(&self) -> writeverf3 {
        writeverf3(self.generation_number_le)
    }
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn test_file_handle_u64() {
        let handle = FileHandleU64::new(42);
        assert_eq!(handle.as_u64(), 42);
        assert_eq!(handle.len(), 8);
        assert_eq!(handle.as_bytes(), &[42, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            FileHandleU64::from_bytes(&[42, 0, 0, 0, 0, 0, 0, 0]),
            Some(handle)
        );
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct TestHandle<const N: usize> {
        id: [u8; N],
    }
    impl<const N: usize> FileHandle for TestHandle<N> {
        fn len(&self) -> usize {
            self.id.len()
        }
        fn as_bytes(&self) -> &[u8] {
            &self.id
        }
        fn from_bytes(bytes: &[u8]) -> Option<Self> {
            bytes.try_into().ok().map(|id| Self { id })
        }
    }

    #[test]
    fn test_file_handle_converter_3bytes() {
        let converter = FileHandleConverter::new();
        let handle = TestHandle { id: [1, 2, 3] };
        let nfs_handle = converter.fh_to_nfs(&handle);
        assert_eq!(nfs_handle.data.len(), 11);
        assert_eq!(nfs_handle.data[0..8], converter.generation_number_le);
        assert_eq!(&nfs_handle.data[8..], handle.as_bytes());

        let converted_handle = converter.fh_from_nfs::<TestHandle<3>>(&nfs_handle).unwrap();
        assert_eq!(converted_handle, handle);
    }

    #[test]
    fn test_with_generation_number_round_trip() {
        let converter = FileHandleConverter::with_generation_number(12345);
        let handle = TestHandle { id: [1, 2, 3] };
        let nfs_handle = converter.fh_to_nfs(&handle);

        assert_eq!(nfs_handle.data[0..8], 12345u64.to_le_bytes());

        let round_tripped = converter.fh_from_nfs::<TestHandle<3>>(&nfs_handle).unwrap();
        assert_eq!(round_tripped, handle);
    }

    #[test]
    fn test_with_generation_number_shared_across_instances() {
        // Two converters with the same generation (simulating cluster nodes)
        // must accept each other's handles.
        let node_a = FileHandleConverter::with_generation_number(42);
        let node_b = FileHandleConverter::with_generation_number(42);

        let handle = TestHandle { id: [7, 8, 9] };
        let nfs_handle = node_a.fh_to_nfs(&handle);

        let decoded = node_b.fh_from_nfs::<TestHandle<3>>(&nfs_handle).unwrap();
        assert_eq!(decoded, handle);
    }

    #[test]
    fn test_with_generation_number_stale_vs_badhandle() {
        let converter = FileHandleConverter::with_generation_number(1000);
        let handle = TestHandle { id: [1, 2, 3] };
        let nfs_handle = converter.fh_to_nfs(&handle);

        // Newer generation → STALE (handle is from an older server)
        let newer = FileHandleConverter::with_generation_number(2000);
        assert_eq!(
            newer.fh_from_nfs::<TestHandle<3>>(&nfs_handle),
            Err(nfsstat3::NFS3ERR_STALE)
        );

        // Older generation → BADHANDLE (handle is from a future server, unexpected)
        let older = FileHandleConverter::with_generation_number(500);
        assert_eq!(
            older.fh_from_nfs::<TestHandle<3>>(&nfs_handle),
            Err(nfsstat3::NFS3ERR_BADHANDLE)
        );
    }

    #[test]
    fn test_with_generation_number_verf() {
        let converter = FileHandleConverter::with_generation_number(12345);
        assert_eq!(converter.verf().0, 12345u64.to_le_bytes());
    }

    #[test]
    fn test_file_handle_converter_19bytes() {
        let converter = FileHandleConverter::new();
        let handle = TestHandle {
            id: [
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19,
            ],
        };
        let nfs_handle = converter.fh_to_nfs(&handle);
        assert_eq!(nfs_handle.data.len(), 27);
        assert_eq!(nfs_handle.data[0..8], converter.generation_number_le);
        assert_eq!(&nfs_handle.data[8..], handle.as_bytes());

        let converted_handle = converter
            .fh_from_nfs::<TestHandle<19>>(&nfs_handle)
            .unwrap();
        assert_eq!(converted_handle, handle);
    }
}
