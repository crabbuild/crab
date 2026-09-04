use super::*;

#[cfg(feature = "local-cache")]
pub(crate) async fn remove_file(root: &Path, path: &Path) -> Result<()> {
    let relative = PathBuf::from(relative_path(root, path)?);
    let display_root = root.to_owned();
    crate::private_fs::run_blocking(&tokio_util::sync::CancellationToken::new(), move |cancel| {
        let root = match PinnedRoot::open(&display_root) {
            Ok(root) => root,
            Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let mut removal = PayloadRemoval::open(Some(&root), &display_root, false)?;
        let result = removal.remove(&relative, || {
            crate::private_fs::check_cancelled(cancel)?;
            root.remove_file(&relative).map(Some)
        });
        match result {
            Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => result.map(|_| ()),
        }
    })
    .await
}

pub(crate) struct PayloadRead {
    root: PinnedRoot,
    display_root: PathBuf,
    relative: PathBuf,
    original: std::fs::File,
}

impl PayloadRead {
    pub(crate) async fn open(root: &Path, path: &Path) -> Result<(Self, tokio::fs::File)> {
        let relative = PathBuf::from(relative_path(root, path)?);
        let display_root = root.to_owned();
        crate::private_fs::run_blocking(&tokio_util::sync::CancellationToken::new(), move |_| {
            let root = PinnedRoot::open(&display_root)?;
            let original = root.open_read(&relative)?;
            // Only this duplicate reads the shared cursor. The retained
            // descriptor pins identity until validation and repair finish.
            let reader = tokio::fs::File::from_std(original.try_clone()?);
            Ok((Self { root, display_root, relative, original }, reader))
        })
        .await
        .inspect_err(|error| {
            if !matches!(error, CacheError::Io(error) if error.kind() == std::io::ErrorKind::NotFound) {
                let family = path.strip_prefix(root).ok().and_then(Path::to_str)
                    .map(classify_family).unwrap_or("other");
                tracing::warn!(family, operation = "open-read", path = %path.display(),
                    recovery = "bypass-cache", %error, "local cache open failed");
            }
        })
    }

    pub(crate) async fn finish<T, E: std::fmt::Display>(
        self,
        result: std::result::Result<T, E>,
    ) -> std::result::Result<(T, Self), E> {
        // Keep the successful read's identity until its caller decides whether
        // it is a hit worth touching or a non-mutating probe/request miss.
        let error = match result {
            Ok(data) => return Ok((data, self)),
            Err(error) => error,
        };
        // The reader must have finished before releasing its shared lease.
        // Preserve the validation error even when optional cleanup fails.
        let path = self.display_root.join(&self.relative);
        let family = self
            .relative
            .to_str()
            .map(classify_family)
            .unwrap_or("other");
        tracing::warn!(family, operation = "read-and-verify", path = %path.display(),
            recovery = "discard-read-and-use-origin", %error, "local cache read failed");
        if let Err(error) = self.discard().await {
            tracing::warn!(family, operation = "evict", path = %path.display(),
                recovery = "bypass-cache", %error, "local cache repair failed");
        }
        Err(error)
    }

    pub(crate) async fn touch(self) {
        // Recency belongs to the file just read, never a later publication at
        // its path. Keep the descriptor/lease; failure only loses an LRU hint.
        let _ = tokio::task::spawn_blocking(move || {
            self.original.set_modified(std::time::SystemTime::now())
        })
        .await;
    }

    pub(crate) async fn discard(self) -> Result<()> {
        crate::private_fs::run_blocking(
            &tokio_util::sync::CancellationToken::new(),
            move |cancel| {
                let mut removal =
                    PayloadRemoval::open(Some(&self.root), &self.display_root, false)?;
                removal.remove(&self.relative, || {
                    crate::private_fs::check_cancelled(cancel)?;
                    self.root.remove_read_file(&self.relative, &self.original)
                })?;
                Ok(())
            },
        )
        .await
    }
}

pub(crate) struct PayloadRemoval {
    connection: Option<Database>,
    path: PathBuf,
}

impl PayloadRemoval {
    pub(crate) fn open(
        root: Option<&PinnedRoot>,
        display_root: &Path,
        dry_run: bool,
    ) -> Result<Self> {
        let path = display_root.join(CATALOG_FILE);
        // A standalone range directory need not have a private catalog parent.
        // Never inspect that ambient parent or open SQLite during a dry run.
        let connection = if let Some(root) = root.filter(|_| !dry_run) {
            match root.open_database(
                Path::new(CATALOG_FILE),
                DatabaseMode::ReadWrite,
                std::time::Duration::ZERO,
            ) {
                Ok(connection) => Some(connection),
                Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) if busy(&error) => return Err(error),
                Err(error) => {
                    unavailable(&path, &error);
                    None
                }
            }
        } else {
            None
        };
        Ok(Self { connection, path })
    }

    pub(crate) fn remove(
        &mut self,
        relative: &Path,
        operation: impl FnOnce() -> Result<Option<u64>>,
    ) -> Result<Option<u64>> {
        let transaction = match self.connection.as_mut() {
            Some(connection) => match removal_transaction(connection, &self.path, relative) {
                Ok(transaction) => Some(transaction),
                Err(error)
                    if matches!(&error, CacheError::Index { source, .. }
                    if source.sqlite_error().is_some_and(|error|
                        matches!(error.code, rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
                        || error.extended_code == rusqlite::ffi::SQLITE_ERROR)) =>
                {
                    // These fixed statements also fail with SQLITE_ERROR when
                    // disposable tables/columns are missing. Generation and
                    // I/O errors are different codes and must stop deletion.
                    unavailable(&self.path, &error);
                    None
                }
                Err(error) => return Err(error),
            },
            None => None,
        };
        let result = operation();
        let removed_or_missing = matches!(&result, Ok(Some(_)))
            || matches!(&result, Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound);
        if removed_or_missing
            && let Some(transaction) = transaction
            && let Err(source) = transaction.commit()
        {
            // The payload is already gone. Report accounting failure without
            // retrying deletion against a possible replacement file.
            unavailable(&self.path, &index_error(&self.path, source));
        }
        result
    }
}

fn unavailable(path: &Path, error: &CacheError) {
    // v1.1.0 cleanup works without a usable disposable index. Keep that
    // contract until cleanup itself is explicitly changed; doctor cannot
    // repair the catalog as a prerequisite for reclaiming safe payloads.
    tracing::warn!(
        family = "catalog",
        operation = "remove-payload",
        path = %path.display(),
        recovery = "payload-only-accounting-unavailable",
        %error,
        "payload cleanup cannot retire catalog accounting; recorded totals may be stale"
    );
}

fn busy(error: &CacheError) -> bool {
    match error {
        CacheError::Io(error) => error.kind() == std::io::ErrorKind::WouldBlock,
        CacheError::Index { source, .. } => matches!(
            source.sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
        ),
        _ => false,
    }
}

pub(super) fn removal_transaction<'a>(
    connection: &'a mut Database,
    path: &Path,
    relative: &Path,
) -> Result<Transaction<'a>> {
    let relative = relative.to_str().ok_or_else(|| CacheError::UnsafeRoot {
        path: relative.display().to_string(),
        reason: "cache accounting key is not UTF-8".into(),
    })?;
    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|source| index_error(path, source))?;
    let protected: bool = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM leases WHERE relative_path = ?1)
             OR EXISTS(SELECT 1 FROM reservations WHERE relative_path = ?1)",
            [relative],
            |row| row.get(0),
        )
        .map_err(|source| index_error(path, source))?;
    if protected {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "cache payload has an active catalog owner",
        )
        .into());
    }
    // Retire within the uncommitted writer transaction before filesystem I/O.
    // Failed/declined removal rolls this back; publishers cannot register a
    // replacement between the final owner check and commit.
    transaction
        .execute(
            "DELETE FROM cache_entries WHERE relative_path = ?1",
            [relative],
        )
        .map_err(|source| index_error(path, source))?;
    Ok(transaction)
}

#[cfg(all(test, unix))]
mod tests;
