use super::{
    Directory, DirectoryStream, Path, component_name, io, unsafe_path, validate_permissions,
};
use crate::private_fs::{EntryStat, FileStat};
use crate::{CacheError, Result};

const MAX_SCAN_DEPTH: usize = 32;

impl Directory {
    pub(in crate::private_fs) fn visit_files(
        &self,
        visitor: &mut dyn FnMut(&Path, FileStat) -> Result<()>,
    ) -> Result<()> {
        self.visit_selected_files(&|_| Ok(true), visitor)
    }

    pub(in crate::private_fs) fn visit_selected_files(
        &self,
        select: &dyn Fn(&Path) -> Result<bool>,
        visitor: &mut dyn FnMut(&Path, FileStat) -> Result<()>,
    ) -> Result<()> {
        // Maintenance remains fail-closed: inspection's recoverable entry
        // errors must never let reconciliation/eviction use a partial inventory.
        visit_directory(self, Path::new(""), 0, select, &mut |path, entry| {
            let entry = entry?;
            if !entry.is_directory {
                visitor(path, entry.file)?;
            }
            Ok(())
        })
    }

    pub(in crate::private_fs) fn inspect_entries(
        &self,
        visitor: &mut dyn FnMut(&Path, Result<EntryStat>) -> Result<()>,
    ) -> Result<()> {
        visit_directory(self, Path::new(""), 0, &|_| Ok(true), visitor)
    }
}

fn visit_directory(
    directory: &Directory,
    relative: &Path,
    depth: usize,
    select: &dyn Fn(&Path) -> Result<bool>,
    visitor: &mut dyn FnMut(&Path, Result<EntryStat>) -> Result<()>,
) -> Result<()> {
    if depth > MAX_SCAN_DEPTH {
        return visitor(
            relative,
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cache inventory exceeds maximum directory depth",
            )
            .into()),
        );
    }
    // Include directory allocation through the retained descriptor. This is
    // overhead, not payload length; strict file-only callers omit the event.
    let own_stat = directory
        .stat_component(c".")
        .map_err(CacheError::from)
        .and_then(|stat| entry_stat(&stat, &directory.path));
    let unavailable = own_stat.is_err();
    visitor(relative, own_stat)?;
    if unavailable {
        return Ok(());
    }
    // Retain only the current directory chain and one entry. fstatat does not
    // open/close SQLite files, which could release this process's POSIX locks.
    let mut stream = match DirectoryStream::new(directory) {
        Ok(stream) => stream,
        Err(error) => return visitor(relative, Err(error)),
    };
    loop {
        let name = match stream.next_name() {
            Ok(Some(name)) => name,
            Ok(None) => return Ok(()),
            Err(error) => return visitor(relative, Err(error)),
        };
        let path = directory.path.join(&name);
        let relative = relative.join(&name);
        if !select(&relative)? {
            continue;
        }
        let name_c = component_name(&name)?;
        let stat = match directory.stat_component(&name_c) {
            Ok(stat) => stat,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                visitor(&relative, Err(error.into()))?;
                continue;
            }
        };
        let entry = match entry_stat(&stat, &path) {
            Ok(entry) => entry,
            Err(error) => {
                visitor(&relative, Err(error))?;
                continue;
            }
        };
        if entry.is_directory {
            // Inspect descendants through the opened child, not its pathname.
            // A replacement symlink fails here; maintenance aborts while
            // diagnostics retain the error and inspect independent siblings.
            match directory.child(&name, false) {
                Ok(child) => visit_directory(&child, &relative, depth + 1, select, visitor)?,
                Err(CacheError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => visitor(&relative, Err(error))?,
            }
            continue;
        }
        visitor(&relative, Ok(entry))?;
    }
}

fn entry_stat(stat: &libc::stat, path: &Path) -> Result<EntryStat> {
    validate_permissions(stat.st_mode, stat.st_uid, path)?;
    let is_directory = stat.st_mode & libc::S_IFMT == libc::S_IFDIR;
    if !is_directory && (stat.st_mode & libc::S_IFMT != libc::S_IFREG || stat.st_nlink != 1) {
        return Err(unsafe_path(
            path,
            "inventory entry is a special file or has another hard link",
        ));
    }
    let size = u64::try_from(stat.st_size)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    // stat's allocation unit is 512 bytes, not st_blksize's preferred I/O size.
    let allocated_bytes = u64::try_from(stat.st_blocks)
        .ok()
        .and_then(|blocks| blocks.checked_mul(512))
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid cache allocation count")
        })?;
    let modified_ns = u64::try_from(stat.st_mtime).map_or(0, |seconds| {
        seconds
            .saturating_mul(1_000_000_000)
            .saturating_add(u64::try_from(stat.st_mtime_nsec).unwrap_or(0))
    });
    Ok(EntryStat {
        file: FileStat { size, modified_ns },
        allocated_bytes,
        is_directory,
    })
}

#[cfg(test)]
mod tests {
    use super::super::TemporaryFile;
    use super::*;
    use std::io::Write as _;

    fn visit_files(
        root: &Path,
        visitor: &mut dyn FnMut(&Path, FileStat) -> Result<()>,
    ) -> Result<()> {
        Directory::root(root, false)?.visit_files(visitor)
    }

    fn fixture(root: &Path, relative: &str, data: &[u8]) {
        let mut file = TemporaryFile::new(root, &root.join(relative)).unwrap();
        file.file.write_all(data).unwrap();
        file.commit().unwrap();
    }

    #[test]
    fn replacing_root_during_inventory_keeps_the_opened_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        fixture(&root, "objects/first", b"one");
        fixture(&root, "objects/second", b"two");
        let moved = tmp.path().join("moved");
        let mut count = 0;
        visit_files(&root, &mut |_, metadata| {
            if count == 0 {
                std::fs::rename(&root, &moved).unwrap();
                fixture(&root, "objects/first", b"replacement");
                fixture(&root, "objects/second", b"replacement");
            }
            assert_eq!(metadata.size, 3);
            count += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn repeated_inventory_uses_independent_directory_cursors() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        fixture(&root, "first", b"one");
        fixture(&root, "objects/second", b"two");
        let pinned = crate::private_fs::PinnedRoot::open(&root).unwrap();
        for _ in 0..2 {
            let mut names = Vec::new();
            pinned
                .visit_files(&mut |path, _| {
                    names.push(path.to_owned());
                    Ok(())
                })
                .unwrap();
            names.sort();
            assert_eq!(names, [Path::new("first"), Path::new("objects/second")]);
        }
    }

    #[test]
    fn deletion_after_inventory_uses_the_held_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        fixture(&root, "objects/first", b"old");
        let pinned = crate::private_fs::PinnedRoot::open(&root).unwrap();
        let mut names = Vec::new();
        pinned
            .visit_files(&mut |path, _| {
                names.push(path.to_owned());
                Ok(())
            })
            .unwrap();
        let moved = tmp.path().join("moved");
        std::fs::rename(&root, &moved).unwrap();
        fixture(&root, "objects/first", b"replacement");

        assert_eq!(names.len(), 1);
        assert_eq!(pinned.remove_file(&names[0]).unwrap(), 3);
        assert!(!moved.join("objects/first").exists());
        assert_eq!(
            std::fs::read(root.join("objects/first")).unwrap(),
            b"replacement"
        );
    }

    #[test]
    fn wide_inventory_streams_every_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        for n in 0..5000 {
            fixture(&root, &format!("objects/{n}"), b"one");
        }
        let mut count = 0;
        let mut bytes = 0;
        visit_files(&root, &mut |_, metadata| {
            count += 1;
            bytes += metadata.size;
            Ok(())
        })
        .unwrap();
        assert_eq!((count, bytes), (5000, 15000));
    }

    #[test]
    fn inventory_depth_is_bounded_without_creating_missing_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        assert!(
            matches!(visit_files(&root, &mut |_, _| Ok(())), Err(CacheError::Io(error)) if error.kind() == io::ErrorKind::NotFound)
        );
        assert!(!root.exists());
        let mut path = root.clone();
        for _ in 0..=MAX_SCAN_DEPTH {
            path.push("d");
        }
        Directory::root(&path, true).unwrap();
        assert!(
            matches!(visit_files(&root, &mut |_, _| Ok(())), Err(CacheError::Io(error)) if error.kind() == io::ErrorKind::InvalidData)
        );
    }
}
