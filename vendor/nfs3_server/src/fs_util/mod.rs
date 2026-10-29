#![cfg_attr(target_os = "windows", allow(unused_imports))]

pub(crate) mod metadata_ext;

use std::fs::Metadata;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use metadata_ext::NfsMetadataExt;
use nfs3_types::nfs3::{
    fattr3, fileid3, nfsstat3, nfstime3, sattr3, set_atime, set_gid3, set_mode3, set_mtime,
    set_size3, set_uid3, specdata3,
};
use tokio::fs::OpenOptions;
use tracing::debug;

/// Compares if file metadata has changed in a significant way
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[must_use]
pub fn metadata_differ(lhs: &Metadata, rhs: &Metadata) -> bool {
    lhs.ino() != rhs.ino()
        || lhs.mtime() != rhs.mtime()
        || lhs.len() != rhs.len()
        || lhs.file_type() != rhs.file_type()
}
#[must_use]
pub fn fattr3_differ(lhs: &fattr3, rhs: &fattr3) -> bool {
    lhs.fileid != rhs.fileid
        || lhs.mtime != rhs.mtime
        || lhs.size != rhs.size
        || lhs.type_ != rhs.type_
}

/// `path.exists()` is terrifyingly unsafe as that
/// traverses symlinks. This can cause deadlocks if we have a
/// recursive symlink.
#[must_use]
pub fn exists_no_traverse(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

const fn mode_unmask(mode: u32) -> u32 {
    // it is possible to create a file we cannot write to.
    // we force writable always.
    // let mode = mode | 0x80;
    // let mode = Permissions::from_mode(mode);
    // mode.mode() & 0x1FF

    (mode | 0x80) & 0x1FF
}

#[allow(clippy::option_if_let_else, clippy::needless_pass_by_value)]
fn to_nfstime3(time: std::io::Result<std::time::SystemTime>) -> nfstime3 {
    match time {
        Ok(time) => time.try_into().unwrap_or_default(),
        Err(_) => nfstime3::default(),
    }
}

/// Converts fs Metadata to NFS fattr3
#[must_use]
pub fn metadata_to_fattr3(fileid: fileid3, meta: &Metadata) -> fattr3 {
    let meta_ext = NfsMetadataExt(meta);
    let size = meta.len();
    let mode = mode_unmask(meta_ext.mode());
    fattr3 {
        type_: meta_ext.file_type(),
        mode,
        nlink: meta_ext.nlink(),
        uid: meta_ext.uid(),
        gid: meta_ext.gid(),
        size,
        used: size,
        rdev: specdata3::default(),
        fsid: 0,
        fileid,
        atime: to_nfstime3(meta.accessed()),
        mtime: to_nfstime3(meta.modified()),
        ctime: to_nfstime3(meta.created()),
    }
}

/// Set attributes of a path
pub async fn path_setattr(path: &Path, setattr: &sattr3) -> Result<(), nfsstat3> {
    match &setattr.atime {
        set_atime::SET_TO_SERVER_TIME => {
            let _ = filetime::set_file_atime(path, filetime::FileTime::now());
        }
        set_atime::SET_TO_CLIENT_TIME(time) => {
            let time = filetime::FileTime::from_unix_time(i64::from(time.seconds), time.nseconds);
            let _ = filetime::set_file_atime(path, time);
        }
        set_atime::DONT_CHANGE => {}
    }
    match &setattr.mtime {
        set_mtime::SET_TO_SERVER_TIME => {
            let _ = filetime::set_file_mtime(path, filetime::FileTime::now());
        }
        set_mtime::SET_TO_CLIENT_TIME(time) => {
            let time = filetime::FileTime::from_unix_time(i64::from(time.seconds), time.nseconds);
            let _ = filetime::set_file_mtime(path, time);
        }
        set_mtime::DONT_CHANGE => {}
    }
    if let set_mode3::Some(mode) = setattr.mode {
        debug!(" -- set permissions {:?} {:?}", path, mode);
        let mode = mode_unmask(mode);
        let _ = NfsMetadataExt::set_mode_on_path(path, mode);
    }
    if let set_uid3::Some(_) = setattr.uid {
        debug!("Set uid not implemented");
    }
    if let set_gid3::Some(_) = setattr.gid {
        debug!("Set gid not implemented");
    }
    if let set_size3::Some(size3) = setattr.size {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .await
            .or(Err(nfsstat3::NFS3ERR_IO))?;
        debug!(" -- set size {:?} {:?}", path, size3);
        file.set_len(size3).await.or(Err(nfsstat3::NFS3ERR_IO))?;
    }
    Ok(())
}

/// Set attributes of a file
#[allow(clippy::unused_async)] // keeping it async for API compatibility
pub async fn file_setattr(file: &std::fs::File, setattr: &sattr3) -> Result<(), nfsstat3> {
    if let set_mode3::Some(mode) = setattr.mode {
        debug!(" -- set permissions {:?}", mode);
        let mode = mode_unmask(mode);
        let _ = NfsMetadataExt::set_mode_on_file(file, mode);
    }
    if let set_size3::Some(size3) = setattr.size {
        debug!(" -- set size {:?}", size3);
        file.set_len(size3).or(Err(nfsstat3::NFS3ERR_IO))?;
    }
    Ok(())
}
