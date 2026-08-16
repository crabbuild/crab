//! Filesystem copy-on-write cloning.

use std::path::Path;

/// Clone `source` to a new `destination` through the platform's CoW primitive.
///
/// The destination must not exist. Callers own content verification, fallback,
/// publication, and user-facing error policy.
#[cfg(target_os = "macos")]
pub(crate) fn clone_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "source path contains NUL")
    })?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination path contains NUL",
        )
    })?;

    // SAFETY: clonefile reads two NUL-terminated paths and does not retain
    // them after the call. Both CStrings live for the duration of the call.
    let result = unsafe { libc::clonefile(source.as_ptr(), destination.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Clone `source` to a new `destination` through the platform's CoW primitive.
///
/// The destination must not exist. Callers own content verification, fallback,
/// publication, and user-facing error policy.
#[cfg(target_os = "linux")]
pub(crate) fn clone_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    const FICLONE: libc::c_ulong = 0x4004_9409;

    let source = std::fs::File::open(source)?;
    let destination_file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;

    // SAFETY: ioctl receives valid open file descriptors. FICLONE copies data
    // by reference and does not retain either descriptor after the call.
    let result = unsafe { libc::ioctl(destination_file.as_raw_fd(), FICLONE, source.as_raw_fd()) };
    if result == 0 {
        return Ok(());
    }

    let error = std::io::Error::last_os_error();
    drop(destination_file);
    let _ = std::fs::remove_file(destination);
    Err(error)
}

/// Return an explicit unsupported error on platforms without a clone contract.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn clone_file(_source: &Path, _destination: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "copy-on-write file cloning is unsupported on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};

    #[test]
    fn clone_preserves_bytes_and_writes_are_independent_when_supported() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let destination = dir.path().join("destination");
        std::fs::write(&source, b"immutable shared content").unwrap();

        if let Err(error) = clone_file(&source, &destination) {
            eprintln!("SKIP: filesystem CoW unavailable: {error}");
            return;
        }

        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"immutable shared content"
        );
        let mut destination_file = std::fs::OpenOptions::new()
            .write(true)
            .open(&destination)
            .unwrap();
        destination_file.seek(SeekFrom::Start(0)).unwrap();
        destination_file.write_all(b"changed").unwrap();
        destination_file.flush().unwrap();
        assert_eq!(std::fs::read(&source).unwrap(), b"immutable shared content");
        assert_ne!(
            std::fs::read(&destination).unwrap(),
            std::fs::read(&source).unwrap()
        );
    }

    #[test]
    fn clone_refuses_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let destination = dir.path().join("destination");
        std::fs::write(&source, b"source").unwrap();
        std::fs::write(&destination, b"keep").unwrap();

        assert!(clone_file(&source, &destination).is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), b"keep");
    }

    #[test]
    fn failed_clone_does_not_leave_destination() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("missing");
        let destination = dir.path().join("destination");

        assert!(clone_file(&source, &destination).is_err());
        assert!(!destination.exists());
    }
}
