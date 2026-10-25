use crate::vfs::{DirEntry, FileHandle, NextResult, ReadDirIterator, ReadDirPlusIterator};

/// Transforms a [`ReadDirPlusIterator`] into a [`ReadDirIterator`].
///
/// This adapter allows to have only one Iterator type for both `readdir` and `readdirplus`
/// methods. The default implementation of [`NfsReadFileSystem::readdir`][1] calls `readdirplus`
/// internally and then adapts the result to `ReadDirIterator`.
///
/// [1]: crate::vfs::NfsReadFileSystem::readdir
pub struct ReadDirPlusToReadDir<H: FileHandle, I: ReadDirPlusIterator<H>> {
    inner: I,
    _phantom: std::marker::PhantomData<H>,
}

impl<H: FileHandle, I: ReadDirPlusIterator<H>> ReadDirPlusToReadDir<H, I> {
    /// Create a new adapter that wraps a [`ReadDirPlusIterator`]
    pub const fn new(inner: I) -> Self {
        Self {
            inner,
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<I: ReadDirPlusIterator<H>, H: FileHandle> ReadDirIterator for ReadDirPlusToReadDir<H, I> {
    async fn next(&mut self) -> NextResult<DirEntry> {
        match self.inner.next().await {
            NextResult::Ok(plus_entry) => NextResult::Ok(DirEntry {
                fileid: plus_entry.fileid,
                name: plus_entry.name,
                cookie: plus_entry.cookie,
            }),
            NextResult::Eof => NextResult::Eof,
            NextResult::Err(err) => NextResult::Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use nfs3_types::nfs3::filename3;

    use super::*;
    use crate::vfs::{DirEntryPlus, FileHandleU64};

    // Mock iterator for testing
    struct MockReadDirPlusIterator {
        entries: Vec<DirEntryPlus<FileHandleU64>>,
        index: usize,
    }

    impl MockReadDirPlusIterator {
        fn new(entries: Vec<DirEntryPlus<FileHandleU64>>) -> Self {
            Self { entries, index: 0 }
        }
    }

    impl ReadDirPlusIterator<FileHandleU64> for MockReadDirPlusIterator {
        async fn next(&mut self) -> NextResult<DirEntryPlus<FileHandleU64>> {
            if self.index >= self.entries.len() {
                NextResult::Eof
            } else {
                let entry = self.entries[self.index].clone();
                self.index += 1;
                NextResult::Ok(entry)
            }
        }
    }

    #[tokio::test]
    async fn test_readdir_plus_to_readdir_adapter() {
        let plus_entries = vec![
            DirEntryPlus {
                fileid: 1,
                name: filename3::from(b"file1.txt".to_vec()),
                cookie: 100,
                name_attributes: None,
                name_handle: Some(42u64.into()),
            },
            DirEntryPlus {
                fileid: 2,
                name: filename3::from(b"file2.txt".to_vec()),
                cookie: 200,
                name_attributes: None,
                name_handle: None,
            },
        ];

        let plus_iter = MockReadDirPlusIterator::new(plus_entries);
        let mut readdir_iter = ReadDirPlusToReadDir::new(plus_iter);

        // Test first entry
        match readdir_iter.next().await {
            NextResult::Ok(entry) => {
                assert_eq!(entry.fileid, 1);
                assert_eq!(&entry.name.as_ref(), b"file1.txt");
                assert_eq!(entry.cookie, 100);
            }
            _ => panic!("Expected Ok result"),
        }

        // Test second entry
        match readdir_iter.next().await {
            NextResult::Ok(entry) => {
                assert_eq!(entry.fileid, 2);
                assert_eq!(&entry.name.as_ref(), b"file2.txt");
                assert_eq!(entry.cookie, 200);
            }
            _ => panic!("Expected Ok result"),
        }

        // Test EOF
        match readdir_iter.next().await {
            NextResult::Eof => {}
            _ => panic!("Expected EOF"),
        }
    }
}
