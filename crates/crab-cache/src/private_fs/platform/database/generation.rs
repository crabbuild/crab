use std::ffi::{CStr, CString, OsStr};
use std::fs::File;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{FileExt as _, MetadataExt as _};

use fs4::fs_std::FileExt;
use rusqlite::ffi;

use super::super::{Directory, unsafe_path, validate_metadata};
use super::vfs::io_code;
use crate::private_fs::DatabaseMode;
use crate::{CacheError, Result};

// SQLite's journal headers do not identify the main inode. Keep a separate
// binding so a new connection cannot replay an old WAL into a replaced main.
pub(crate) struct Generation {
    pub(super) main: File,
    owner: File,
    owner_name: CString,
}

impl Generation {
    pub(super) fn matches(&self, other: &Self) -> Result<bool> {
        for (file, expected) in [(&self.main, &other.main), (&self.owner, &other.owner)] {
            let actual = file.metadata()?;
            let expected = expected.metadata()?;
            if actual.dev() != expected.dev() || actual.ino() != expected.ino() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    // The caller holds the directory mutation lock through initialization.
    pub(super) fn open(directory: &Directory, name: &CStr, mode: DatabaseMode) -> Result<Self> {
        let path = directory.path.join(OsStr::from_bytes(name.to_bytes()));
        let mut owner_name = name.to_bytes().to_vec();
        owner_name.extend_from_slice(b"-owner");
        let owner_name = CString::new(owner_name)
            .map_err(|error| CacheError::Io(std::io::Error::other(error)))?;
        let owner_path = directory
            .path
            .join(OsStr::from_bytes(owner_name.to_bytes()));
        let flags = if mode == DatabaseMode::ReadOnly {
            libc::O_RDONLY
        } else {
            libc::O_RDWR
        };
        let owner = directory
            .open_component(
                &owner_name,
                flags
                    | if mode == DatabaseMode::Create {
                        libc::O_CREAT
                    } else {
                        0
                    },
                &owner_path,
            )
            .map_err(|error| match error {
                // The main file exists. Missing ownership is not an absent
                // catalog: read-only stats must report uncertainty, not zero.
                CacheError::Io(source) if source.kind() == std::io::ErrorKind::NotFound => {
                    unsafe_path(&path, "database has no generation owner")
                }
                error => error,
            })?;
        let metadata = owner.metadata()?;
        validate_metadata(&metadata, &owner_path, false)?;
        // OFD exclusive byte locks require a writable OS description. The
        // read-only SQLite connection and VFS deny data/namespace writes;
        // this capability exists only to exclude writers during inspection.
        let main = directory.open_component(name, libc::O_RDWR, &path)?;
        let main_metadata = main.metadata()?;
        validate_metadata(&main_metadata, &path, false)?;
        let mut expected = [0; 24];
        expected[..8].copy_from_slice(b"CRABDB01");
        expected[8..16].copy_from_slice(&main_metadata.dev().to_le_bytes());
        expected[16..].copy_from_slice(&main_metadata.ino().to_le_bytes());
        let mut stored = [0; 24];
        let matches = if metadata.len() == stored.len() as u64 {
            owner.read_exact_at(&mut stored, 0)?;
            stored == expected
        } else {
            false
        };
        if !matches {
            // A live old generation or any recovery side file makes rebinding
            // ambiguous. Never delete those files or guess which main owns them.
            if mode != DatabaseMode::Create || !FileExt::try_lock_exclusive(&owner)? {
                return Err(unsafe_path(
                    &path,
                    "database generation does not match its owner",
                ));
            }
            for suffix in [b"-journal".as_slice(), b"-wal", b"-shm"] {
                let mut side = name.to_bytes().to_vec();
                side.extend_from_slice(suffix);
                let side = CString::new(side)
                    .map_err(|error| CacheError::Io(std::io::Error::other(error)))?;
                match directory.stat_component(&side) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                    Err(error) => return Err(error.into()),
                    Ok(_) => {
                        return Err(unsafe_path(
                            &path,
                            "unbound database has recovery side files",
                        ));
                    }
                }
            }
            owner.write_all_at(&expected, 0)?;
            owner.set_len(expected.len() as u64)?;
            owner.sync_all()?;
            directory.file.sync_all()?;
        }
        if !FileExt::try_lock_shared(&owner)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "database generation is being changed",
            )
            .into());
        }
        Ok(Self {
            main,
            owner,
            owner_name,
        })
    }

    pub(super) fn validate(
        &self,
        directory: &Directory,
        main_name: &CStr,
    ) -> std::result::Result<(), i32> {
        if !same_file(directory, main_name, &self.main)?
            || !same_file(directory, &self.owner_name, &self.owner)?
        {
            return Err(ffi::SQLITE_READONLY_DBMOVED);
        }
        Ok(())
    }
}

pub(super) fn same_file(
    directory: &Directory,
    name: &CStr,
    file: &File,
) -> std::result::Result<bool, i32> {
    let metadata = file.metadata().map_err(io_code)?;
    let stat = match directory.stat_component(name) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(io_code(error)),
    };
    #[cfg(target_os = "macos")]
    let device = stat.st_dev as u64;
    #[cfg(target_os = "linux")]
    let device = stat.st_dev;
    Ok(device == metadata.dev() && stat.st_ino == metadata.ino() && stat.st_nlink == 1)
}
