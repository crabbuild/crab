use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use tempfile::NamedTempFile;

use crate::Result;

pub(super) async fn write(root: PathBuf, path: String, bytes: Vec<u8>) -> Result<usize> {
    // The blocking task retains its temporary through publication/cleanup.
    // Native shutdown joins admitted requests rather than abandoning this owner.
    Ok(
        tokio::task::spawn_blocking(move || publish(&root, Path::new(&path), &bytes))
            .await
            .map_err(io::Error::other)??,
    )
}

fn destination(root: &Path, path: &Path) -> io::Result<PathBuf> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "file path must be relative to the working tree",
        ));
    }
    let root = fs::canonicalize(root)?;
    let requested = root.join(path);
    let mut ancestor = requested.as_path();
    let mut missing = Vec::new();
    loop {
        match fs::canonicalize(ancestor) {
            Ok(mut resolved) => {
                if !resolved.starts_with(&root) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "file resolves outside the working tree",
                    ));
                }
                for name in missing.iter().rev() {
                    resolved.push(name);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // A dangling link is an existing entry, not a missing file to
                // replace. Resolving before mkdir also rejects escaping parents.
                if fs::symlink_metadata(ancestor).is_ok() {
                    return Err(error);
                }
                let name = ancestor.file_name().ok_or(error)?;
                missing.push(name.to_owned());
                ancestor = ancestor.parent().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "file has no parent directory")
                })?;
            }
            Err(error) => return Err(error),
        }
    }
}

fn publish(root: &Path, path: &Path, bytes: &[u8]) -> io::Result<usize> {
    let target = destination(root, path)?;
    let permissions = match fs::metadata(&target) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "write destination is not a regular file",
                ));
            }
            let permissions = metadata.permissions();
            if permissions.readonly() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "write destination is read-only",
                ));
            }
            // Atomic rename requires directory permission; also respect the
            // target's write permission/ACL instead of bypassing it by replacement.
            fs::OpenOptions::new().write(true).open(&target)?;
            Some(permissions)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let parent = target.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "file has no parent directory")
    })?;
    fs::create_dir_all(parent)?;
    let mut builder = tempfile::Builder::new();
    builder.prefix(".crab-write-");
    #[cfg(unix)]
    if permissions.is_none() {
        use std::os::unix::fs::PermissionsExt;
        // New worktree files keep normal create/umask semantics. Existing
        // content stays private in its temporary until its mode is restored.
        builder.permissions(fs::Permissions::from_mode(0o666));
    }
    let mut temporary = builder.tempfile_in(parent)?;
    let prepared = (|| {
        temporary.write_all(bytes)?;
        if let Some(permissions) = permissions {
            temporary.as_file().set_permissions(permissions)?;
        }
        temporary.as_file().sync_all()
    })();
    if let Err(error) = prepared {
        return Err(cleanup(temporary, error));
    }
    // Same-directory persist atomically replaces the target. Every operation
    // owns a unique name, including concurrent writes to different extensions.
    if let Err(error) = temporary.persist(target) {
        return Err(cleanup(error.file, error.error));
    }
    Ok(bytes.len())
}

fn cleanup(file: NamedTempFile, failure: io::Error) -> io::Error {
    match file.close() {
        Ok(()) => failure,
        Err(cleanup) => io::Error::other(CleanupFailure { failure, cleanup }),
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{failure}; temporary file cleanup also failed: {cleanup}")]
struct CleanupFailure {
    #[source]
    failure: io::Error,
    cleanup: io::Error,
}
