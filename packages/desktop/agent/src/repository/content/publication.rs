use std::io::Read;

use super::*;

pub(super) struct IndexFile {
    pub(super) root: PathBuf,
    pub(super) path: String,
    pub(super) mode: String,
    pub(super) oid: String,
}

impl IndexFile {
    pub(super) async fn capture(root: &Path, path: &str, scope: &ReadScope) -> Result<Self> {
        GitPath::new(path.as_bytes().to_vec())?;
        let (mode, oid) = Self::entry(root, path, scope).await?;
        match (mode, oid) {
            (Some(mode), Some(oid)) if matches!(mode.as_str(), "100644" | "100755") => Ok(Self {
                root: root.into(),
                path: path.into(),
                mode,
                oid,
            }),
            _ => Err(AgentError::Other(
                "The selected file has no resolved regular index entry".into(),
            )),
        }
    }

    pub(super) fn apply_mode(
        &self,
        file: &std::fs::File,
        permissions: Option<std::fs::Permissions>,
    ) -> Result<()> {
        let mut permissions = permissions.unwrap_or(file.metadata()?.permissions());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let executable = if self.mode == "100755" { 0o100 } else { 0 };
            permissions.set_mode((permissions.mode() & !0o111) | executable);
        }
        file.set_permissions(permissions)?;
        Ok(())
    }
    pub(super) async fn entry(
        root: &Path,
        path: &str,
        scope: &ReadScope,
    ) -> Result<(Option<String>, Option<String>)> {
        let entries = command::run(root, &["ls-files", "--stage", "-z", "--", path], scope).await?;
        command::file_entry_at(&entries, path, true)
    }

    pub(super) async fn check(&self, scope: &ReadScope) -> Result<()> {
        let entry = Self::entry(&self.root, &self.path, scope).await?;
        if !self.matches(&entry) {
            return Err(changed());
        }
        Ok(())
    }

    fn matches(&self, entry: &(Option<String>, Option<String>)) -> bool {
        entry.0.as_deref() == Some(self.mode.as_str())
            && entry.1.as_deref() == Some(self.oid.as_str())
    }

    pub(super) async fn check_commit(&self, scope: &ReadScope) -> Result<()> {
        let entries = command::run(
            &self.root,
            &["ls-tree", "-z", "HEAD", "--", &self.path],
            scope,
        )
        .await?;
        if !self.matches(&command::file_entry_at(&entries, &self.path, false)?) {
            return Err(AgentError::Other(
                "Commit the selected pointer and mode before dehydrating".into(),
            ));
        }
        Ok(())
    }
}

pub(super) struct WorkingFile {
    pub(super) root: PathBuf,
    pub(super) requested: PathBuf,
    pub(super) path: PathBuf,
    original: Option<(std::fs::Metadata, blake3::Hash)>,
}

impl WorkingFile {
    pub(super) fn from_bytes(
        root: PathBuf,
        requested: PathBuf,
        path: PathBuf,
        bytes: &[u8],
        metadata: std::fs::Metadata,
    ) -> Self {
        Self {
            root,
            requested,
            path,
            original: Some((metadata, blake3::hash(bytes))),
        }
    }

    pub(super) fn missing(root: &Path, path: &str) -> Result<Self> {
        let (destination, _) = missing_destination(root, Path::new(path))?;
        let target = Self {
            root: root.into(),
            requested: root.join(path),
            path: destination,
            original: None,
        };
        target.check_metadata()?;
        Ok(target)
    }

    pub(super) async fn capture(root: &Path, path: &str, scope: &ReadScope) -> Result<Self> {
        let root = root.to_owned();
        let path = path.to_owned();
        let scope = scope.clone();
        tokio::task::spawn_blocking(move || {
            scope.check()?;
            let (destination, _) = missing_destination(&root, Path::new(&path))?;
            let requested = root.join(&path);
            match std::fs::symlink_metadata(&requested) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    Self::missing(&root, &path)
                }
                Err(error) => Err(error.into()),
                Ok(metadata) if metadata.is_file() => {
                    let mut file = std::fs::File::open(&requested)?;
                    let hash = hash_working(&mut file, &metadata, &scope)?;
                    let target = Self {
                        root,
                        requested,
                        path: destination,
                        original: Some((metadata, hash)),
                    };
                    target.check_metadata()?;
                    Ok(target)
                }
                Ok(_) => Err(AgentError::Other(
                    "Restore requires a regular working path without symlinks".into(),
                )),
            }
        })
        .await
        .map_err(std::io::Error::other)?
    }

    pub(super) fn exists(&self) -> bool {
        self.original.is_some()
    }

    pub(super) fn temporary_parent(&self) -> Result<PathBuf> {
        let relative = self
            .requested
            .strip_prefix(&self.root)
            .map_err(std::io::Error::other)?;
        Ok(missing_destination(&self.root, relative)?.1)
    }

    pub(super) fn check_metadata(&self) -> Result<Option<std::fs::Permissions>> {
        let Some((before, _)) = &self.original else {
            let relative = self
                .requested
                .strip_prefix(&self.root)
                .map_err(std::io::Error::other)?;
            if missing_destination(&self.root, relative)?.0 != self.path {
                return Err(changed());
            }
            return match std::fs::symlink_metadata(&self.requested) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.into()),
                Ok(_) => Err(changed()),
            };
        };
        let current = std::fs::symlink_metadata(&self.requested)?;
        if !unchanged_file(before, &current) || std::fs::canonicalize(&self.requested)? != self.path
        {
            return Err(changed());
        }
        Ok(Some(current.permissions()))
    }

    pub(super) fn check(&self, scope: &ReadScope) -> Result<Option<std::fs::Permissions>> {
        scope.check()?;
        let permissions = self.check_metadata()?;
        if let Some((before, expected)) = &self.original {
            let mut file = std::fs::File::open(&self.requested)?;
            if hash_working(&mut file, before, scope)? != *expected {
                return Err(changed());
            }
            self.check_metadata()?;
        }
        Ok(permissions)
    }

    pub(super) fn publish<T>(
        &self,
        temporary: tempfile::NamedTempFile,
        scope: &ReadScope,
        prepared: Result<T>,
    ) -> Result<T> {
        let mut parents = ParentDirectories::default();
        let prepared = prepared.and_then(|value| {
            scope.check()?;
            self.check_metadata()?;
            if !self.exists() {
                let parent = self.path.parent().ok_or_else(changed)?;
                parents.prepare(&self.root, parent)?;
            }
            parents.check()?;
            self.check_metadata()?;
            scope.check()?;
            Ok(value)
        });
        parents.finish(publish(temporary, &self.path, self.exists(), prepared))
    }
}

fn hash_working(
    file: &mut std::fs::File,
    before: &std::fs::Metadata,
    scope: &ReadScope,
) -> Result<blake3::Hash> {
    if !unchanged_file(before, &file.metadata()?) {
        return Err(changed());
    }
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut read = 0u64;
    loop {
        scope.check()?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        read = read.checked_add(count as u64).ok_or_else(changed)?;
        if read > before.len() {
            return Err(changed());
        }
        hasher.update(&buffer[..count]);
    }
    if read != before.len() || !unchanged_file(before, &file.metadata()?) {
        return Err(changed());
    }
    scope.check()?;
    Ok(hasher.finalize())
}

pub(super) fn unchanged_file(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    if !after.is_file()
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
        || before.created().ok() != after.created().ok()
        || before.permissions().readonly() != after.permissions().readonly()
    {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
    }
    #[cfg(not(unix))]
    true
}

// Missing worktree paths are literal paths beneath the canonical repository.
// Reject links before any mkdir; otherwise a dangling or replaced parent could
// redirect publication outside the captured working tree.
fn missing_destination(root: &Path, relative: &Path) -> Result<(PathBuf, PathBuf)> {
    if relative
        .components()
        .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Err(changed());
    }
    let root = std::fs::canonicalize(root)?;
    let destination = root.join(relative);
    let mut parent = destination.parent().ok_or_else(changed)?;
    loop {
        match std::fs::symlink_metadata(parent) {
            Ok(metadata) if metadata.is_dir() && !metadata.is_symlink() => {
                if std::fs::canonicalize(parent)? != parent {
                    return Err(changed());
                }
                let existing = parent.to_owned();
                return Ok((destination, existing));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                parent = parent
                    .parent()
                    .filter(|path| path.starts_with(&root))
                    .ok_or_else(changed)?;
            }
            Err(error) => return Err(error.into()),
            Ok(_) => return Err(changed()),
        }
    }
}

#[derive(Default)]
struct ParentDirectories {
    entries: Vec<(PathBuf, std::fs::Metadata, bool)>,
}

impl ParentDirectories {
    fn prepare(&mut self, root: &Path, parent: &Path) -> Result<()> {
        let root = std::fs::canonicalize(root)?;
        let relative = parent.strip_prefix(&root).map_err(std::io::Error::other)?;
        let mut path = root.clone();
        self.entries
            .push((root.clone(), std::fs::symlink_metadata(&root)?, false));
        for component in relative.components() {
            if !matches!(component, std::path::Component::Normal(_)) {
                return Err(changed());
            }
            self.check()?;
            path.push(component);
            let created = match std::fs::create_dir(&path) {
                Ok(()) => true,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
                Err(error) => return Err(error.into()),
            };
            let metadata = std::fs::symlink_metadata(&path)?;
            if !metadata.is_dir() || metadata.is_symlink() || std::fs::canonicalize(&path)? != path
            {
                return Err(changed());
            }
            self.entries.push((path.clone(), metadata, created));
        }
        Ok(())
    }

    fn check(&self) -> Result<()> {
        for (path, before, _) in &self.entries {
            if !same_directory(before, &std::fs::symlink_metadata(path)?)
                || std::fs::canonicalize(path)? != *path
            {
                return Err(changed());
            }
        }
        Ok(())
    }

    fn finish<T>(self, result: Result<T>) -> Result<T> {
        let Err(mut operation) = result else {
            return result;
        };
        // Remove only empty directories created by this operation, in reverse
        // order. A replaced directory or another writer's content belongs to
        // that writer and must survive cancellation/failure cleanup.
        for (path, before, created) in self.entries.into_iter().rev() {
            if !created {
                continue;
            }
            let cleanup = (|| -> Result<()> {
                let current = match std::fs::symlink_metadata(&path) {
                    Ok(current) => current,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Err(error) => return Err(error.into()),
                };
                if !same_directory(&before, &current) || std::fs::canonicalize(&path)? != path {
                    return Ok(());
                }
                match std::fs::remove_dir(&path) {
                    Ok(()) => Ok(()),
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                        ) =>
                    {
                        Ok(())
                    }
                    Err(error) => Err(error.into()),
                }
            })();
            if let Err(cleanup) = cleanup {
                operation = AgentError::Cleanup {
                    operation: Box::new(operation),
                    cleanup: Box::new(cleanup),
                };
            }
        }
        Err(operation)
    }
}

fn same_directory(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    if !after.is_dir() || after.is_symlink() || before.created().ok() != after.created().ok() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        before.dev() == after.dev() && before.ino() == after.ino()
    }
    #[cfg(not(unix))]
    true
}

pub(super) fn refresh_stat_cache(root: &Path, update: crab_git::index_stat::IndexStatUpdate) {
    // Publication is already complete. Like CLI hydration, this advisory cache
    // must not turn verified bytes into a failed transfer when Git holds a lock.
    if let Err(error) = crab_git::index_stat::refresh(root, &[update]) {
        tracing::warn!(error = %error, "published file but could not refresh Git stat cache");
    }
}

pub(super) fn publish<T>(
    temporary: tempfile::NamedTempFile,
    path: &Path,
    replace: bool,
    prepared: Result<T>,
) -> Result<T> {
    // A successful same-filesystem publication wins a late cancellation.
    let (operation, temporary) = match prepared {
        Ok(value) => {
            let identity = check_temporary(&temporary);
            if let Err(error) = identity {
                return match close_temporary(temporary) {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(AgentError::Cleanup {
                        operation: Box::new(error),
                        cleanup: Box::new(cleanup),
                    }),
                };
            }
            let result = if replace {
                temporary.persist(path)
            } else {
                temporary.persist_noclobber(path)
            };
            match result {
                Ok(_) => return Ok(value),
                Err(error) => (AgentError::from(error.error), error.file),
            }
        }
        Err(error) => (error, temporary),
    };
    match close_temporary(temporary) {
        Ok(()) => Err(operation),
        Err(cleanup) => Err(AgentError::Cleanup {
            operation: Box::new(operation),
            cleanup: Box::new(cleanup),
        }),
    }
}

fn check_temporary(temporary: &tempfile::NamedTempFile) -> Result<()> {
    let opened = temporary.as_file().metadata()?;
    let current = std::fs::symlink_metadata(temporary.path())?;
    if !unchanged_file(&opened, &current)
        || std::fs::canonicalize(temporary.path())? != temporary.path()
    {
        return Err(changed());
    }
    Ok(())
}

fn close_temporary(temporary: tempfile::NamedTempFile) -> Result<()> {
    let identity = check_temporary(&temporary);
    if let Err(error) = identity {
        let mut path = temporary.into_temp_path();
        path.disable_cleanup(true);
        return Err(error);
    }
    temporary.close()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_publication_never_cleans_a_replaced_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let temporary = tempfile::NamedTempFile::new_in(&root).unwrap();
        let path = temporary.path().to_owned();
        std::fs::write(root.join("replacement"), b"user content").unwrap();
        std::fs::rename(root.join("replacement"), &path).unwrap();
        let result: Result<()> =
            publish(temporary, &root.join("destination"), true, Err(changed()));
        assert!(result.is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"user content");
    }

    #[test]
    fn failed_publication_cleans_only_owned_empty_directories() {
        for retain_content in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(directory.path()).unwrap();
            let mut parents = ParentDirectories::default();
            parents.prepare(&root, &root.join("models/deep")).unwrap();
            if retain_content {
                std::fs::write(root.join("models/deep/user-file"), "keep").unwrap();
            }
            let result: Result<()> = parents.finish(Err(changed()));
            assert!(result.is_err());
            assert_eq!(root.join("models").exists(), retain_content);
            if retain_content {
                assert_eq!(
                    std::fs::read(root.join("models/deep/user-file")).unwrap(),
                    b"keep"
                );
            }
        }
    }
}
