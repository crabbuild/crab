use super::*;
use crate::private_fs::{FileIdentity, PendingFile};
use rusqlite::Connection;

fn connection(reservation: &CacheReservation) -> Result<Database> {
    configure_catalog(
        reservation.generation.open(
            &reservation.root,
            Path::new(CATALOG_FILE),
            CATALOG_BUSY_TIMEOUT,
        )?,
        &reservation.catalog_path,
    )
}

pub(super) fn record(reservation: &CacheReservation, pending: &PendingFile) -> Result<String> {
    let (relative, identity) = pending.record(&reservation.relative_path)?;
    let relative = relative_path(Path::new(""), &relative)?;
    let identity = serde_json::to_vec(&identity).map_err(std::io::Error::other)?;
    let connection = connection(reservation)?;
    // Recovery authority must reach stable storage before payload bytes can
    // outlive this process. WAL/NORMAL may lose a committed identity on power loss.
    connection
        .pragma_update(None, "synchronous", "FULL")
        .map_err(|source| index_error(&reservation.catalog_path, source))?;
    connection
        .execute(
            "INSERT INTO pending_files(relative_path, owner, pid, identity) VALUES (?1, ?2, ?3, ?4)",
            params![relative, reservation.id, std::process::id(), identity],
        )
        .map_err(|source| index_error(&reservation.catalog_path, source))?;
    Ok(relative)
}

pub(super) fn retire(reservation: &CacheReservation, relative: &str) -> Result<()> {
    // A completed rename or proven absence retires this journal. Failed
    // temporary cleanup keeps its exact identity available to recovery.
    connection(reservation)?
        .execute(
            "DELETE FROM pending_files WHERE relative_path = ?1 AND owner = ?2",
            params![relative, reservation.id],
        )
        .map_err(|source| index_error(&reservation.catalog_path, source))?;
    Ok(())
}

pub(super) fn release(reservation: &CacheReservation) -> Result<()> {
    let Some(relative) = &reservation.pending_path else {
        return Ok(());
    };
    // A failed rename/unlink flush may already have removed the temporary name.
    // Persist absence before retiring authority, including cancellation cleanup.
    if reservation.root.confirm_absent(Path::new(relative))? {
        retire(reservation, relative)?;
    }
    Ok(())
}

pub(super) fn available(connection: &Connection, path: &Path) -> Result<bool> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'pending_files')",
            [],
            |row| row.get(0),
        )
        .map_err(|source| index_error(path, source))
}

pub(super) fn remove(
    connection: &Connection,
    catalog_path: &Path,
    relative: &Path,
    dry_run: bool,
    operation: impl FnOnce(&FileIdentity) -> Result<Option<u64>>,
) -> Result<Option<u64>> {
    if !matches!(entry_kind(relative), EntryKind::Temporary) {
        return Ok(None);
    }
    let relative = relative_path(Path::new(""), relative)?;
    let recorded = connection
        .query_row(
            "SELECT owner, pid, identity,
                    EXISTS(SELECT 1 FROM reservations WHERE id = pending_files.owner)
             FROM pending_files WHERE relative_path = ?1",
            [&relative],
            |row| {
                let bytes = row.get_ref(2)?.as_blob()?;
                if bytes.len() > 256 {
                    return Err(rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Blob,
                        Box::new(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "cache fill identity exceeds its bound",
                        )),
                    ));
                }
                let identity = serde_json::from_slice::<FileIdentity>(bytes).map_err(|source| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Blob,
                        Box::new(source),
                    )
                })?;
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u32>(1)?,
                    identity,
                    row.get::<_, bool>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|source| index_error(catalog_path, source))?;
    let Some((owner, pid, identity, reserved)) = recorded else {
        return Ok(None);
    };
    // A released reservation can leave failed cleanup in a live process.
    // Process liveness alone neither proves an active fill nor grants removal.
    if reserved && pid_is_alive(pid) {
        return Err(
            std::io::Error::new(std::io::ErrorKind::WouldBlock, "cache fill is active").into(),
        );
    }
    let result = operation(&identity);
    if !dry_run && removal_finished(&result) {
        connection
            .execute(
                "DELETE FROM pending_files WHERE relative_path = ?1 AND owner = ?2",
                params![relative, owner],
            )
            .map_err(|source| index_error(catalog_path, source))?;
        connection
            .execute(
                "DELETE FROM cache_entries WHERE relative_path = ?1",
                [&relative],
            )
            .map_err(|source| index_error(catalog_path, source))?;
    }
    result
}

pub(super) fn removal_finished(result: &Result<Option<u64>>) -> bool {
    matches!(result, Ok(Some(_)))
        || matches!(result, Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound)
}

pub(super) fn recover(
    root: &PinnedRoot,
    connection: &Connection,
    path: &Path,
    result: &mut CacheMaintenanceStats,
) -> Result<()> {
    let mut cursor = String::new();
    loop {
        let page = {
            let mut statement = connection.prepare(
                "SELECT relative_path FROM pending_files WHERE relative_path > ?1 ORDER BY relative_path LIMIT 512",
            ).map_err(|source| index_error(path, source))?;
            statement
                .query_map([&cursor], |row| row.get::<_, String>(0))
                .map_err(|source| index_error(path, source))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|source| index_error(path, source))?
        };
        if page.is_empty() {
            return Ok(());
        }
        for relative in page {
            cursor = relative;
            let removed = remove(connection, path, Path::new(&cursor), false, |identity| {
                root.remove_file_if(Path::new(&cursor), false, &mut |file| {
                    identity.matches(file)
                })
            });
            match removed {
                Ok(Some(bytes)) => {
                    result.evicted_entries = result.evicted_entries.saturating_add(1);
                    result.evicted_bytes = result.evicted_bytes.saturating_add(bytes);
                }
                Ok(None) => {}
                Err(CacheError::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::NotFound
                    ) => {}
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(feature = "remote-client", feature = "local-cache"))]
    #[tokio::test]
    async fn anonymous_response_keeps_identity_until_its_final_handle_closes() {
        use std::io::Write as _;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("cache");
        let catalog = CacheCatalog::new(root.clone(), None);
        let reservation = catalog
            .reserve(&root.join("responses/.streamed-object"), 7)
            .await
            .unwrap()
            .unwrap();
        let (mut file, reservation) = reservation.anonymous_file().await.unwrap();
        file.write_all(b"payload").unwrap();
        let pinned = PinnedRoot::open(&root).unwrap();
        let connection = super::super::open_catalog(&pinned, &root.join(CATALOG_FILE)).unwrap();
        let count = || {
            connection
                .query_row("SELECT count(*) FROM pending_files", [], |row| {
                    row.get::<_, u64>(0)
                })
                .unwrap()
        };
        let active = count();
        drop(file);
        drop(reservation);
        assert_eq!((active, count()), (1, 0));
    }

    #[tokio::test]
    async fn rejected_identity_record_never_exposes_payload_or_leaks_reservation() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("cache");
        let pinned = PinnedRoot::create(&root).unwrap();
        let catalog = CacheCatalog::new(root.clone(), None);
        let directory = root.join("chunks/aa");
        let destination = directory.join("a".repeat(64));
        let reservation = catalog.reserve(&destination, 7).await.unwrap().unwrap();
        let connection = super::super::open_catalog(&pinned, &root.join(CATALOG_FILE)).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER reject_identity BEFORE INSERT ON pending_files
                 BEGIN SELECT RAISE(ABORT, 'identity record unavailable'); END;",
            )
            .unwrap();
        drop(connection);

        let rejected = reservation.write(b"payload").await.is_err();
        let reserved = CacheCatalog::read_only_stats(&root)
            .unwrap()
            .reservations_bytes;
        assert_eq!(
            (
                rejected,
                reserved,
                destination.exists(),
                std::fs::read_dir(directory).unwrap().count(),
            ),
            (true, 0, false, 0)
        );
    }

    #[tokio::test]
    async fn invalid_journal_identity_keeps_temporary_and_allows_payload_cleanup() {
        for identity in ["x'FF'", "'not-a-blob'", "zeroblob(257)"] {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().join("cache");
            let pinned = PinnedRoot::create(&root).unwrap();
            let connection = super::super::open_catalog(&pinned, &root.join(CATALOG_FILE)).unwrap();
            let pending = "chunks/aa/.tmp-unreadable";
            let payload = format!("chunks/aa/{}", "a".repeat(64));
            for (path, bytes) in [
                (pending, b"retain".as_slice()),
                (payload.as_str(), b"data".as_slice()),
            ] {
                crate::private_fs::atomic_write(&root, &root.join(path), bytes)
                    .await
                    .unwrap();
            }
            connection
                .execute(
                    &format!("INSERT INTO pending_files VALUES (?1, 'fixture', ?2, {identity})"),
                    params![pending, std::process::id()],
                )
                .unwrap();
            drop(connection);
            let report =
                crate::clean_cache(&root, false, &tokio_util::sync::CancellationToken::new())
                    .await
                    .unwrap();
            assert_eq!(report.files_removed, 1);
            assert_eq!(std::fs::read(root.join(pending)).unwrap(), b"retain");
        }
    }
}
