use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock, Weak};

use tempfile::NamedTempFile;

use crate::Result;

#[derive(Default)]
struct Publications {
    active: HashSet<PathBuf>,
    snapshots: Vec<Weak<tokio::sync::Mutex<HashSet<PathBuf>>>>,
}

type Registry = Arc<tokio::sync::Mutex<Publications>>;

static REGISTRIES: LazyLock<
    tokio::sync::Mutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<Publications>>>>,
> = LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

async fn registry(root: &Path) -> Registry {
    let mut roots = REGISTRIES.lock().await;
    roots.retain(|_, registry| registry.strong_count() > 0);
    if let Some(registry) = roots.get(root).and_then(Weak::upgrade) {
        return registry;
    }
    let registry = Arc::new(tokio::sync::Mutex::new(Publications::default()));
    roots.insert(root.to_owned(), Arc::downgrade(&registry));
    registry
}

pub(super) struct StatusSnapshot {
    registry: Registry,
    paths: Arc<tokio::sync::Mutex<HashSet<PathBuf>>>,
}

pub(super) async fn status_snapshot(root: &Path) -> StatusSnapshot {
    let registry = registry(root).await;
    let paths = {
        let mut publications = registry.lock().await;
        let paths = Arc::new(tokio::sync::Mutex::new(publications.active.clone()));
        publications
            .snapshots
            .retain(|snapshot| snapshot.strong_count() > 0);
        publications.snapshots.push(Arc::downgrade(&paths));
        paths
    };
    StatusSnapshot { registry, paths }
}

impl StatusSnapshot {
    pub(super) async fn finish(self) -> HashSet<PathBuf> {
        // Creation and registration share this lock. A status scan cannot
        // finish in the gap after exclusive creation but before ownership.
        let _publications = self.registry.lock().await;
        self.paths.lock().await.clone()
    }
}

struct Publication {
    registry: Registry,
    path: PathBuf,
}

impl Drop for Publication {
    fn drop(&mut self) {
        // The guard lives only in the blocking publication owner, including
        // cleanup. An async caller dropping its request cannot release it early.
        self.registry.blocking_lock().active.remove(&self.path);
    }
}

fn owned_temporary(
    registry: &Registry,
    builder: &tempfile::Builder<'_, '_>,
    parent: &Path,
) -> io::Result<(NamedTempFile, Publication)> {
    // Only this physical worktree is held during acquisition. A blocked
    // filesystem must not stall status reads for an unrelated repository.
    let mut publications = registry.blocking_lock();
    let temporary = builder.tempfile_in(parent)?;
    let path = temporary.path().to_owned();
    publications.active.insert(path.clone());
    publications.snapshots.retain(|snapshot| {
        if let Some(paths) = snapshot.upgrade() {
            paths.blocking_lock().insert(path.clone());
            true
        } else {
            false
        }
    });
    Ok((
        temporary,
        Publication {
            registry: registry.clone(),
            path,
        },
    ))
}

pub(super) async fn write(root: PathBuf, path: String, bytes: Vec<u8>) -> Result<usize> {
    let root = tokio::fs::canonicalize(root).await?;
    let registry = registry(&root).await;
    // The blocking task retains its temporary through publication/cleanup.
    // Native shutdown joins admitted requests rather than abandoning this owner.
    Ok(
        tokio::task::spawn_blocking(move || publish(&root, Path::new(&path), &bytes, &registry))
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

fn publish(root: &Path, path: &Path, bytes: &[u8], registry: &Registry) -> io::Result<usize> {
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
    let (mut temporary, _publication) = owned_temporary(registry, &builder, parent)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_publication_registry_does_not_block_another_worktree() -> io::Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let first = PathBuf::from("held-worktree");
        let other = PathBuf::from("independent-worktree");
        let registry = runtime.block_on(registry(&first));
        let held = registry.blocking_lock();
        runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(1), status_snapshot(&other))
                .await
                .map_err(io::Error::other)
        })?;
        drop(held);
        Ok(())
    }

    #[test]
    fn status_captures_only_owned_temporaries_created_before_or_during_its_scan() -> io::Result<()>
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let directory = tempfile::tempdir_in(
            std::env::var_os("CARGO_TARGET_DIR")
                .ok_or_else(|| io::Error::other("CARGO_TARGET_DIR is required"))?,
        )?;
        let root = fs::canonicalize(directory.path())?;
        let registry = runtime.block_on(registry(&root));
        let foreign = root.join(".crab-write-user-file");
        fs::write(&foreign, b"USER")?;
        let before = runtime.block_on(status_snapshot(&root));
        let builder = tempfile::Builder::new();
        let (temporary, publication) = owned_temporary(&registry, &builder, &root)?;
        let path = temporary.path().to_owned();
        let during = runtime.block_on(status_snapshot(&root));
        temporary.close()?;
        drop(publication);
        for snapshot in [before, during] {
            let paths = runtime.block_on(snapshot.finish());
            assert!(paths.contains(&path) && !paths.contains(&foreign));
        }
        Ok(())
    }

    #[test]
    fn a_new_status_scan_does_not_hide_a_completed_user_named_publication() -> io::Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let directory = tempfile::tempdir_in(
            std::env::var_os("CARGO_TARGET_DIR")
                .ok_or_else(|| io::Error::other("CARGO_TARGET_DIR is required"))?,
        )?;
        let root = fs::canonicalize(directory.path())?;
        let registry = runtime.block_on(registry(&root));
        let builder = tempfile::Builder::new();
        let (temporary, publication) = owned_temporary(&registry, &builder, &root)?;
        let path = temporary.path().to_owned();
        let destination = directory.path().join(".crab-write-user-result");
        temporary
            .persist(&destination)
            .map_err(|error| error.error)?;
        drop(publication);
        let paths = runtime.block_on(async { status_snapshot(&root).await.finish().await });
        assert!(!paths.contains(&path) && !paths.contains(&destination));
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{failure}; temporary file cleanup also failed: {cleanup}")]
struct CleanupFailure {
    #[source]
    failure: io::Error,
    cleanup: io::Error,
}
