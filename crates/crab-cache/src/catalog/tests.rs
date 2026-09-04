use super::*;
use crate::ensure_private_cache_directory;

fn open_catalog(root: &Path) -> Result<Database> {
    super::open_catalog(&PinnedRoot::create(root)?, &root.join(CATALOG_FILE))
}

async fn payload(root: &Path, byte: u8, size: usize) -> PathBuf {
    let data = vec![byte; size];
    let hash = crab_xet::hash::compute_data_hash(&data).hex();
    let path = root.join("shards").join(&hash[..2]).join(hash);
    crate::private_fs::atomic_write(root, &path, &data)
        .await
        .unwrap();
    std::fs::File::open(&path)
        .unwrap()
        .set_modified(UNIX_EPOCH + std::time::Duration::from_secs(u64::from(byte)))
        .unwrap();
    path
}

#[tokio::test]
async fn unlimited_admission_and_maintenance_retain_payloads() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let catalog = CacheCatalog::new(root.clone(), None);
    let path = payload(&root, 1, 1024).await;
    let incoming = root.join("xorbs/incoming");
    // Exercise admission beyond the former default without allocating a
    // multi-GiB unit-test payload. Real retention is qualified separately.
    let size = 11 * 1024 * 1024 * 1024;
    let reservation = catalog.reserve(&incoming, size).await.unwrap().unwrap();
    assert_eq!(
        CacheCatalog::read_only_stats(&root)
            .unwrap()
            .reservations_bytes,
        size
    );
    let result = catalog.maintain().await.unwrap();
    assert_eq!(result.evicted_bytes, 0);
    assert!(path.exists());
    drop(reservation);
    assert_eq!(
        CacheCatalog::read_only_stats(&root)
            .unwrap()
            .reservations_bytes,
        0
    );
}

#[tokio::test]
async fn maintenance_evicts_deterministic_lru_to_low_watermark() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let old = payload(&root, 1, 400 * 1024).await;
    let _middle = payload(&root, 2, 400 * 1024).await;
    let new = payload(&root, 3, 400 * 1024).await;
    let catalog = CacheCatalog::new(root.clone(), 1024 * 1024);

    let result = catalog.maintain().await.unwrap();

    assert!(result.final_bytes <= 921_600);
    assert!(!old.exists());
    assert!(new.exists());
    assert_eq!(result.evicted_bytes, 400 * 1024);
}

#[test]
fn read_only_stats_does_not_create_missing_root() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("missing");

    assert_eq!(
        CacheCatalog::read_only_stats(&root).unwrap(),
        CacheCatalogStats::default()
    );
    assert!(!root.exists());
}

#[test]
fn read_only_stats_rejects_malformed_accounting_without_repair() {
    for table in ["cache_entries", "reservations"] {
        for invalid_size in ["-1", "'not-a-size'", "1.5", "x'00'"] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("cache");
            let connection = open_catalog(&root).unwrap();
            connection
                .execute_batch(
                    "PRAGMA journal_mode = DELETE; PRAGMA ignore_check_constraints = ON;",
                )
                .unwrap();
            let sql = match table {
                "cache_entries" => format!(
                    "INSERT INTO cache_entries VALUES ('valid', 'temporary', 'valid', 17, 0, 0), ('entry', 'temporary', 'key', {invalid_size}, 0, 0)"
                ),
                _ => format!(
                    "INSERT INTO reservations VALUES ('valid', 'valid', 17, 1, 0), ('reservation', 'entry', {invalid_size}, 1, 0)"
                ),
            };
            connection.execute_batch(&sql).unwrap();
            drop(connection);
            let before = std::fs::read(root.join(CATALOG_FILE)).unwrap();

            let result = CacheCatalog::read_only_stats(&root);

            assert!(
                matches!(result, Err(CacheError::CorruptObject { .. })),
                "{table}: {invalid_size}: {result:?}"
            );
            assert_eq!(std::fs::read(root.join(CATALOG_FILE)).unwrap(), before);
        }
    }
}

#[test]
fn read_only_stats_rejects_malformed_maintenance_marker() {
    for value in ["not-a-timestamp", "-1", "18446744073709551616"] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let connection = open_catalog(&root).unwrap();
        connection
            .execute(
                "INSERT INTO catalog_meta VALUES ('last_maintenance_unix_ms', ?1)",
                [value],
            )
            .unwrap();
        drop(connection);

        let result = CacheCatalog::read_only_stats(&root);

        assert!(
            matches!(
                &result,
                Err(CacheError::Index {
                    source: rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        source,
                    ),
                    ..
                }) if source.is::<std::num::ParseIntError>()
            ),
            "{value}: {result:?}"
        );
    }
}

#[test]
fn read_only_stats_preserves_absent_and_valid_maintenance_markers() {
    for value in [None, Some(0_u64), Some(u64::MAX)] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let connection = open_catalog(&root).unwrap();
        if let Some(value) = value {
            connection
                .execute(
                    "INSERT INTO catalog_meta VALUES ('last_maintenance_unix_ms', ?1)",
                    [value.to_string()],
                )
                .unwrap();
        }
        drop(connection);

        assert_eq!(
            CacheCatalog::read_only_stats(&root)
                .unwrap()
                .last_maintenance_unix_ms,
            value
        );
    }
}

#[cfg(unix)]
#[test]
fn read_only_stats_preserves_quiet_and_uncheckpointed_catalogs() {
    use std::os::unix::fs::MetadataExt as _;

    fn snapshot(root: &Path) -> impl PartialEq + std::fmt::Debug + use<> {
        let mut paths = vec![root.to_owned()];
        paths.extend(
            std::fs::read_dir(root)
                .unwrap()
                .map(|entry| entry.unwrap().path()),
        );
        paths.sort();
        paths
            .into_iter()
            .map(|path| {
                let metadata = std::fs::symlink_metadata(&path).unwrap();
                let bytes = if metadata.is_file() {
                    std::fs::read(&path).unwrap()
                } else {
                    Vec::new()
                };
                (
                    path,
                    metadata.ino(),
                    metadata.mode(),
                    metadata.mtime(),
                    metadata.mtime_nsec(),
                    bytes,
                )
            })
            .collect::<Vec<_>>()
    }

    for retained_wal in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let connection = open_catalog(&root).unwrap();
        connection
            .execute_batch(
                "INSERT INTO cache_entries VALUES ('entry', 'temporary', 'key', 17, 0, 0)",
            )
            .unwrap();
        if retained_wal {
            connection
                .set_db_config(
                    rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
                    true,
                )
                .unwrap();
        }
        drop(connection);
        assert_eq!(
            root.join(format!("{CATALOG_FILE}-wal")).exists(),
            retained_wal
        );
        let before = snapshot(&root);

        let stats = CacheCatalog::read_only_stats(&root).unwrap();

        assert_eq!(
            (stats.entries, stats.total_bytes, stats.temporary_bytes),
            (1, 17, 17)
        );
        assert!(
            snapshot(&root) == before,
            "catalog changed; retained WAL: {retained_wal}"
        );
    }
}

#[test]
fn read_only_stats_does_not_report_an_unbound_catalog_as_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    drop(open_catalog(&root).unwrap());
    let path = root.join(CATALOG_FILE);
    let before = std::fs::read(&path).unwrap();
    let owner = root.join(format!("{CATALOG_FILE}-owner"));
    std::fs::rename(&owner, root.join("saved-owner")).unwrap();
    assert!(matches!(
        CacheCatalog::read_only_stats(&root),
        Err(CacheError::UnsafeRoot { .. })
    ));
    assert_eq!(std::fs::read(path).unwrap(), before);
    assert!(!owner.exists());
}

#[cfg(unix)]
#[test]
fn catalog_access_rejects_database_links_without_changing_the_target() {
    use std::os::unix::fs::PermissionsExt as _;
    let tmp = tempfile::tempdir().unwrap();
    let outside_root = tmp.path().join("outside");
    drop(open_catalog(&outside_root).unwrap());
    let target = outside_root.join(CATALOG_FILE);
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
    let before = std::fs::read(&target).unwrap();
    let root = tmp.path().join("cache");
    ensure_private_cache_directory(&root).unwrap();
    let path = root.join(CATALOG_FILE);
    let generation = DatabaseLease::capture(&open_catalog(&root).unwrap());
    std::fs::rename(&path, root.join("retired.sqlite")).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(matches!(
        open_catalog(&root),
        Err(CacheError::UnsafeRoot { .. })
    ));
    assert!(matches!(
        CacheCatalog::read_only_stats(&root),
        Err(CacheError::UnsafeRoot { .. })
    ));
    remove_owner_row(
        &PinnedRoot::open(&root).unwrap(),
        &generation,
        "DELETE FROM reservations WHERE id = ?1 AND id = ?2",
        "owner",
        "owner",
    );
    assert_eq!(std::fs::read(&target).unwrap(), before);
    assert_eq!(
        std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

#[tokio::test]
async fn maintenance_never_evicts_active_lease() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let active = payload(&root, 1, 400 * 1024).await;
    let idle = payload(&root, 2, 400 * 1024).await;
    let catalog = CacheCatalog::new(root, 600 * 1024);
    let _lease = catalog.lease(&active).await.unwrap();

    catalog.maintain().await.unwrap();

    assert!(active.exists());
    assert!(!idle.exists());
}

#[tokio::test]
async fn object_larger_than_budget_is_not_reserved() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let catalog = CacheCatalog::new(root.clone(), 8);

    assert!(
        catalog
            .reserve(&root.join("xorbs/large"), 9)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!root.exists());
}

#[tokio::test]
async fn incoming_write_displaces_eligible_bytes_below_high_watermark() {
    const MIB: usize = 1024 * 1024;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let old = payload(&root, 1, 8 * MIB).await;
    let catalog = CacheCatalog::new(root.clone(), 10 * MIB as u64);
    let before = catalog.maintain().await.unwrap();
    assert!(before.final_bytes <= catalog.max_bytes().unwrap());
    assert!(before.final_bytes + 3 * MIB as u64 > catalog.max_bytes().unwrap());

    let reservation = catalog
        .reserve(&root.join("incoming"), 3 * MIB as u64)
        .await
        .unwrap()
        .unwrap();

    assert!(!old.exists());
    let stats = CacheCatalog::read_only_stats(&root).unwrap();
    assert!(stats.total_bytes + stats.reservations_bytes <= catalog.max_bytes().unwrap());
    assert_eq!(stats.reservations_bytes, 3 * MIB as u64);
    drop(reservation);
    assert_eq!(
        CacheCatalog::read_only_stats(&root)
            .unwrap()
            .reservations_bytes,
        0
    );
}

#[tokio::test]
async fn incoming_write_bypasses_cache_when_existing_payload_is_leased() {
    const MIB: usize = 1024 * 1024;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let active = payload(&root, 1, 8 * MIB).await;
    let catalog = CacheCatalog::new(root.clone(), 10 * MIB as u64);
    let lease = catalog.lease(&active).await.unwrap();
    catalog.maintain().await.unwrap();

    assert!(
        catalog
            .reserve(&root.join("incoming"), 3 * MIB as u64)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(std::fs::read(active).unwrap(), vec![1; 8 * MIB]);
    assert_eq!(
        CacheCatalog::read_only_stats(&root)
            .unwrap()
            .reservations_bytes,
        0
    );
    drop(lease);
}

#[tokio::test]
async fn incoming_space_keeps_other_fills_reserved() {
    const MIB: usize = 1024 * 1024;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let old = payload(&root, 1, 2 * MIB).await;
    let recent = payload(&root, 2, 4 * MIB).await;
    let catalog = CacheCatalog::new(root.clone(), 10 * MIB as u64);
    catalog.maintain().await.unwrap();
    let first = catalog
        .reserve(&root.join("first"), 2 * MIB as u64)
        .await
        .unwrap()
        .unwrap();
    let before = CacheCatalog::read_only_stats(&root).unwrap();
    assert!(before.total_bytes + 3 * MIB as u64 <= catalog.max_bytes().unwrap());
    assert!(
        before.total_bytes + before.reservations_bytes + 3 * MIB as u64
            > catalog.max_bytes().unwrap()
    );

    let second = catalog
        .reserve(&root.join("second"), 3 * MIB as u64)
        .await
        .unwrap()
        .unwrap();

    assert!(!old.exists());
    assert!(recent.exists());
    let stats = CacheCatalog::read_only_stats(&root).unwrap();
    assert_eq!(stats.reservations_bytes, 5 * MIB as u64);
    assert!(stats.total_bytes + stats.reservations_bytes <= catalog.max_bytes().unwrap());
    drop((first, second));
}

#[test]
fn concurrent_reservations_cannot_spend_the_same_capacity() {
    const MIB: u64 = 1024 * 1024;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let catalog = CacheCatalog::new(root.clone(), 10 * MIB);
    catalog.maintain_sync(0).unwrap();
    let start = std::sync::Barrier::new(8);
    let owners = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|id| {
                let catalog = &catalog;
                let start = &start;
                let path = root.join(format!("incoming-{id}"));
                scope.spawn(move || {
                    start.wait();
                    catalog.reserve_sync(&path, 4 * MIB).unwrap()
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(owners.len(), 2);
    let stats = CacheCatalog::read_only_stats(&root).unwrap();
    assert_eq!(stats.reservations_bytes, 8 * MIB);
    assert!(stats.total_bytes + stats.reservations_bytes <= catalog.max_bytes().unwrap());
    drop(owners);
}

#[tokio::test]
async fn maintenance_keeps_catalog_and_inventory_in_the_same_replaced_root() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let original = payload(&root, 1, 400 * 1024).await;
    let catalog = CacheCatalog::new(root.clone(), 200 * 1024);
    catalog
        .record_sync(
            &open_catalog(&root).unwrap(),
            "shard",
            &original,
            "original",
            400 * 1024,
        )
        .unwrap();
    let pinned = PinnedRoot::open(&root).unwrap();
    let mut connection = super::open_catalog(&pinned, &root.join(CATALOG_FILE)).unwrap();
    let moved = tmp.path().join("moved");
    std::fs::rename(&root, &moved).unwrap();
    let replacement = payload(&root, 2, 7).await;
    CacheCatalog::new(root.clone(), 1024 * 1024)
        .record_sync(
            &open_catalog(&root).unwrap(),
            "shard",
            &replacement,
            "replacement",
            7,
        )
        .unwrap();
    let before = std::fs::read(root.join(CATALOG_FILE)).unwrap();

    catalog
        .maintain_locked(&pinned, 0, &mut connection)
        .unwrap();

    assert!(
        std::fs::read(root.join(CATALOG_FILE)).unwrap() == before,
        "replacement catalog changed"
    );
    assert_eq!(std::fs::read(&replacement).unwrap(), vec![2; 7]);
    assert!(!moved.join(original.strip_prefix(&root).unwrap()).exists());
}

#[tokio::test]
async fn owner_cleanup_releases_only_the_original_root_after_replacement() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let catalog = CacheCatalog::new(root.clone(), 1024 * 1024);
    let path = root.join("incoming");
    let lease = catalog.lease(&path).await.unwrap();
    let reservation = catalog.reserve(&path, 7).await.unwrap().unwrap();
    let moved = tmp.path().join("moved");
    std::fs::rename(&root, &moved).unwrap();
    ensure_private_cache_directory(&root).unwrap();
    // A copied catalog retains matching owner tokens. Cleanup authority must
    // come from the retained root, not a coincidentally matching row identifier.
    std::fs::copy(moved.join(CATALOG_FILE), root.join(CATALOG_FILE)).unwrap();
    let before = std::fs::read(root.join(CATALOG_FILE)).unwrap();

    drop((lease, reservation));

    assert!(
        std::fs::read(root.join(CATALOG_FILE)).unwrap() == before,
        "replacement catalog changed"
    );
    let original = CacheCatalog::read_only_stats(&moved).unwrap();
    assert_eq!(original.reservations_bytes, 0);
    let leases: u64 = open_catalog(&moved)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM leases", [], |row| row.get(0))
        .unwrap();
    assert_eq!(leases, 0);
}

#[tokio::test]
async fn reserved_fill_publishes_and_registers_in_its_original_root() {
    use tokio::io::AsyncWriteExt as _;

    for streamed in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let catalog = CacheCatalog::new(root.clone(), 1024 * 1024);
        let relative = "shards/ab/abababababababababababababababababababababababababababababababab";
        let reservation = catalog
            .reserve(&root.join(relative), 7)
            .await
            .unwrap()
            .unwrap();
        let moved = tmp.path().join("moved");
        std::fs::rename(&root, &moved).unwrap();
        crate::private_fs::atomic_write(&root, &root.join(relative), b"outside")
            .await
            .unwrap();
        let reservation = if streamed {
            let pending = reservation.pending_file().await.unwrap();
            let mut writer = pending.file().unwrap();
            writer.write_all(b"content").await.unwrap();
            writer.sync_all().await.unwrap();
            drop(writer);
            pending.commit().await.unwrap()
        } else {
            reservation.write(b"content").await.unwrap()
        };
        catalog
            .record_and_maintain("shard", "fixture".into(), 7, reservation)
            .await
            .unwrap();

        assert_eq!(std::fs::read(root.join(relative)).unwrap(), b"outside");
        assert!(!root.join(CATALOG_FILE).exists());
        assert_eq!(std::fs::read(moved.join(relative)).unwrap(), b"content");
        let stats = CacheCatalog::read_only_stats(&moved).unwrap();
        assert_eq!(stats.reservations_bytes, 0);
        assert_eq!(stats.total_bytes, 7);
    }
}

#[test]
fn reservation_keeps_database_generation_leased_after_connection_close() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let catalog = CacheCatalog::new(root.clone(), 1024 * 1024);
    let reservation = catalog
        .reserve_sync(&root.join("incoming"), 7)
        .unwrap()
        .unwrap();
    let main = root.join(CATALOG_FILE);
    let retired = root.join("retired.sqlite");
    std::fs::rename(&main, &retired).unwrap();
    std::fs::copy(&retired, &main).unwrap();

    assert!(
        open_catalog(&root).is_err(),
        "live reservation allowed rebinding"
    );
    drop(reservation);
    assert!(
        open_catalog(&root).is_ok(),
        "released generation remained locked"
    );
}

#[test]
fn reopened_generation_connections_exclude_independent_writers() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let generation = DatabaseLease::capture(&open_catalog(&root).unwrap());
    let root = PinnedRoot::open(&root).unwrap();
    let mut first = generation
        .open(&root, Path::new(CATALOG_FILE), std::time::Duration::ZERO)
        .unwrap();
    let second = generation
        .open(&root, Path::new(CATALOG_FILE), std::time::Duration::ZERO)
        .unwrap();
    second.busy_timeout(std::time::Duration::ZERO).unwrap();
    let transaction = first
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let error = second.execute_batch("BEGIN IMMEDIATE").unwrap_err();
    assert!(
        matches!(error, rusqlite::Error::SqliteFailure(failure, _) if failure.code == rusqlite::ErrorCode::DatabaseBusy)
    );
    drop(transaction);
    second.execute_batch("BEGIN IMMEDIATE; COMMIT").unwrap();
}

#[test]
fn configured_catalog_open_does_not_require_a_writer_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let mut writer = open_catalog(&root).unwrap();
    let transaction = writer
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let pinned = PinnedRoot::open(&root).unwrap();
    let reader = pinned
        .open_database(
            Path::new(CATALOG_FILE),
            DatabaseMode::ReadWrite,
            std::time::Duration::ZERO,
        )
        .unwrap();

    configure_catalog(reader, &root.join(CATALOG_FILE)).unwrap();
    transaction.commit().unwrap();
}

#[cfg(windows)]
#[test]
fn windows_pid_liveness_distinguishes_current_and_missing_processes() {
    assert!(pid_is_alive(std::process::id()));
    assert!(!pid_is_alive(u32::MAX));
}

fn replace_catalog_generation(root: &Path, replace_main: bool) {
    let main = root.join(CATALOG_FILE);
    let retired = root.join("retired.sqlite");
    if replace_main {
        std::fs::rename(&main, &retired).unwrap();
        std::fs::copy(retired, main).unwrap();
    }
    std::fs::rename(
        root.join(format!("{CATALOG_FILE}-owner")),
        root.join("retired-owner"),
    )
    .unwrap();
    // A valid new owner bypasses any lock held on the old owner inode. The
    // copied SQL tokens must not grant old reservations mutation authority.
    drop(open_catalog(root).unwrap());
}

#[tokio::test]
async fn owner_cleanup_preserves_copied_rows_after_main_and_owner_replacement() {
    for replace_main in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let catalog = CacheCatalog::new(root.clone(), 1024 * 1024);
        let path = root.join("incoming");
        let lease = catalog.lease(&path).await.unwrap();
        let reservation = catalog.reserve(&path, 7).await.unwrap().unwrap();
        replace_catalog_generation(&root, replace_main);
        let before = std::fs::read(root.join(CATALOG_FILE)).unwrap();

        drop((lease, reservation));

        assert!(
            std::fs::read(root.join(CATALOG_FILE)).unwrap() == before,
            "replacement catalog changed"
        );
        let owners: u64 = open_catalog(&root)
            .unwrap()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM leases) + (SELECT COUNT(*) FROM reservations)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(owners, 2);
    }
}

#[tokio::test]
async fn stale_generation_cannot_publish_or_register_a_fill() {
    use tokio::io::AsyncWriteExt as _;

    for stage in ["before-temporary", "before-publish", "before-registration"] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let catalog = CacheCatalog::new(root.clone(), 1024 * 1024);
        let path = root.join("incoming");
        let reservation = catalog.reserve(&path, 7).await.unwrap().unwrap();
        let result = if stage == "before-temporary" {
            replace_catalog_generation(&root, true);
            reservation.write(b"content").await.map(|_| ())
        } else if stage == "before-publish" {
            let pending = reservation.pending_file().await.unwrap();
            let mut writer = pending.file().unwrap();
            writer.write_all(b"content").await.unwrap();
            writer.sync_all().await.unwrap();
            drop(writer);
            replace_catalog_generation(&root, true);
            pending.commit().await.map(|_| ())
        } else {
            let reservation = reservation.write(b"content").await.unwrap();
            replace_catalog_generation(&root, true);
            catalog
                .record_and_maintain("other", "fixture".into(), 7, reservation)
                .await
                .map(|_| ())
        };
        assert!(result.is_err(), "{stage}: stale fill accepted");
        assert_eq!(path.exists(), stage == "before-registration", "{stage}");
        let stats = CacheCatalog::read_only_stats(&root).unwrap();
        assert_eq!(stats.entries, 0, "{stage}: replacement registered old fill");
        assert_eq!(
            stats.reservations_bytes, 7,
            "{stage}: replacement owner removed"
        );
    }
}

#[cfg(feature = "local-cache")]
#[test]
fn synchronous_fill_rejects_generation_replacement_before_publication() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let catalog = CacheCatalog::new(root.clone(), 1024 * 1024);
    let path = root.join("hints/clean-bloom.bin");
    let reservation = catalog.reserve_sync(&path, 7).unwrap().unwrap();
    let pending = reservation.pending_file_sync().unwrap();
    pending.pending.write_body_sync(b"content").unwrap();
    replace_catalog_generation(&root, true);

    assert!(pending.commit_sync().is_err());
    assert!(!path.exists());
    assert_eq!(
        std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
        0
    );
    assert_eq!(
        CacheCatalog::read_only_stats(&root)
            .unwrap()
            .reservations_bytes,
        7
    );
}

#[cfg(feature = "local-cache")]
#[tokio::test]
async fn publication_lease_survives_cleanup_until_registration() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let data = b"verified payload";
    let hash = crab_xet::hash::compute_data_hash(data);
    let hex = hash.hex();
    let path = root.join("chunks").join(&hex[..2]).join(&hex);
    let catalog = CacheCatalog::new(root.clone(), 1024 * 1024);
    let reservation = catalog
        .reserve(&path, data.len() as u64)
        .await
        .unwrap()
        .unwrap();
    let reservation = reservation.write(data).await.unwrap();
    let cache = crate::LocalCache::with_limits(root.clone(), 0, Some(0));
    let cancel = tokio_util::sync::CancellationToken::new();
    for dry_run in [false, true] {
        let clean = crate::clean_cache(&root, dry_run, &cancel).await.unwrap();
        assert_eq!(clean.files_removed, 0);
        assert_eq!(clean.busy_entries, 1);
        let pruned = cache
            .prune_with_options(crate::PruneOptions {
                dry_run,
                record_entries: true,
            })
            .await
            .unwrap();
        assert_eq!(pruned.bytes_freed, 0);
    }
    assert_eq!(cache.verify().await.unwrap().total, 0);
    assert_eq!(cache.evict_bytes(u64::MAX).await.unwrap().bytes_freed, 0);
    assert!(
        matches!(cache.evict(&crate::CacheKey::Chunk(hash)).await, Err(CacheError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    assert_eq!(std::fs::read(&path).unwrap(), data);

    catalog
        .record_and_maintain("chunk", hex, data.len() as u64, reservation)
        .await
        .unwrap();
    assert_eq!(
        crate::clean_cache(&root, false, &cancel)
            .await
            .unwrap()
            .files_removed,
        1
    );
}

#[tokio::test]
async fn dropping_an_unpublished_fill_releases_temporary_and_reservation() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let catalog = CacheCatalog::new(root.clone(), 1024 * 1024);
    let path = root.join("incoming");
    let reservation = catalog.reserve(&path, 7).await.unwrap().unwrap();
    let pending = reservation.pending_file().await.unwrap();
    drop(pending);
    assert!(!path.exists());
    assert!(std::fs::read_dir(&root).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".tmp-")
    }));
    assert_eq!(
        CacheCatalog::read_only_stats(&root)
            .unwrap()
            .reservations_bytes,
        0
    );
}

fn require_reservation_at_registration(root: &Path) {
    ensure_private_cache_directory(root).unwrap();
    open_catalog(root)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER require_reservation BEFORE INSERT ON cache_entries
         BEGIN
           SELECT RAISE(ABORT, 'published entry has no live reservation')
           WHERE NOT EXISTS (
             SELECT 1 FROM reservations
             WHERE relative_path = NEW.relative_path AND size >= NEW.size
           );
         END;",
        )
        .unwrap();
}

#[cfg(feature = "local-cache")]
#[tokio::test]
async fn object_writers_keep_reservations_until_registration() {
    use crate::{CacheKey, LocalCache};
    use crab_xet::xorb::builder::{RunId, XorbBuilder};
    use crab_xet::xorb::format::Chunk;

    for writer in ["bytes", "xorb-file", "preverified-xorb-file"] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        require_reservation_at_registration(&root);
        let cache = LocalCache::new(root.clone());
        let data = bytes::Bytes::from_static(b"published bytes");
        let key = if writer == "bytes" {
            let key = CacheKey::Chunk(crab_xet::hash::compute_data_hash(&data));
            cache.put(&key, &data).await.unwrap();
            key
        } else {
            let mut builder = XorbBuilder::new();
            builder.push(&Chunk::new(data), RunId(0)).unwrap();
            let xorb = builder.finalize().unwrap().pop().unwrap();
            let source = tmp.path().join("input.xorb");
            std::fs::write(&source, &xorb.bytes).unwrap();
            let len = xorb.bytes.len() as u64;
            if writer == "xorb-file" {
                cache.put_xorb_file(&xorb.hash, &source, len).await.unwrap();
            } else {
                let digest = *blake3::hash(&xorb.bytes).as_bytes();
                cache
                    .put_preverified_xorb_file(&xorb.hash, &source, len, digest)
                    .await
                    .unwrap();
            }
            CacheKey::Xorb(xorb.hash)
        };

        assert!(cache.contains(&key).await, "{writer}");
        let stats = CacheCatalog::read_only_stats(&root).unwrap();
        assert_eq!(
            stats.entries, 1,
            "{writer}: registration must not be skipped"
        );
        assert_eq!(stats.reservations_bytes, 0, "{writer}");
    }
}

#[cfg(feature = "xet-chunk-cache")]
#[tokio::test]
async fn range_writer_keeps_reservation_until_registration() {
    use xet_client::cas_types::{ChunkRange, Key};
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    require_reservation_at_registration(&root);
    let handle = crate::XetChunkCacheHandle::open(root.join("chunks"), 1024 * 1024).unwrap();
    let key = Key {
        prefix: "repo".to_owned(),
        hash: Default::default(),
    };
    let range = ChunkRange::new(0, 1);

    handle
        .cache
        .put(&key, &range, &[0, 7], b"payload")
        .await
        .unwrap();

    assert_eq!(
        handle.cache.get(&key, &range).await.unwrap().unwrap().data,
        b"payload"
    );
    let stats = CacheCatalog::read_only_stats(&root).unwrap();
    assert_eq!(stats.entries, 1);
    assert_eq!(stats.reservations_bytes, 0);
}

#[cfg(feature = "local-cache")]
#[tokio::test]
async fn object_cache_replaces_an_old_working_set_under_pressure() {
    const MIB: usize = 1024 * 1024;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let cache = crate::LocalCache::with_limits(root.clone(), 10 * MIB as u64, None);
    let old_data = vec![1; 8 * MIB];
    let old_key = crate::CacheKey::Chunk(crab_xet::hash::compute_data_hash(&old_data));
    cache.put(&old_key, &old_data).await.unwrap();
    let data = vec![2; 3 * MIB];
    let key = crate::CacheKey::Chunk(crab_xet::hash::compute_data_hash(&data));

    cache.put(&key, &data).await.unwrap();

    assert!(!cache.contains(&old_key).await);
    assert_eq!(
        cache
            .get_or_fetch(&key, || async { panic!("new working set must be cached") })
            .await
            .unwrap(),
        data
    );
    let stats = CacheCatalog::read_only_stats(&root).unwrap();
    assert_eq!(stats.reservations_bytes, 0);
    assert!(stats.total_bytes <= cache.max_bytes().unwrap());
}

#[tokio::test]
async fn maintenance_retains_unowned_state_and_workspaces() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let shard = payload(&root, 1, 400 * 1024).await;
    let retained = [
        "mirrors/repository.git/HEAD",
        "maintenance/inflight/input",
        "shards/notes",
        "xorbs/ab/index.sqlite",
        "profiles/session.json",
    ];
    for path in retained {
        crate::private_fs::atomic_write(&root, &root.join(path), &vec![2; 400 * 1024])
            .await
            .unwrap();
    }

    CacheCatalog::new(root.clone(), 600 * 1024)
        .maintain()
        .await
        .unwrap();

    for path in retained {
        assert!(root.join(path).exists(), "{path}");
    }
    assert!(!shard.exists());
}

#[tokio::test]
async fn candidate_rechecks_owners_before_deleting() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let path = payload(&root, 1, 7).await;
    let relative = relative_path(&root, &path).unwrap();
    let catalog = CacheCatalog::new(root.clone(), 1024 * 1024);
    catalog
        .record_sync(&open_catalog(&root).unwrap(), "shard", &path, "fixture", 7)
        .unwrap();
    let mut connection = open_catalog(&root).unwrap();
    let pinned = PinnedRoot::open(&root).unwrap();
    // Model an owner arriving after the LRU candidate was selected.
    let lease = catalog.lease(&path).await.unwrap();
    assert!(matches!(
        catalog
            .evict_candidate(&pinned, &mut connection, &relative)
            .unwrap(),
        Eviction::Retained
    ));
    drop(lease);
    let reservation = catalog.reserve(&path, 7).await.unwrap().unwrap();
    assert!(matches!(
        catalog
            .evict_candidate(&pinned, &mut connection, &relative)
            .unwrap(),
        Eviction::Retained
    ));
    assert_eq!(std::fs::read(&path).unwrap(), vec![1; 7]);
    drop(reservation);
    assert!(matches!(
        catalog
            .evict_candidate(&pinned, &mut connection, &relative)
            .unwrap(),
        Eviction::Removed(7)
    ));
    assert!(!path.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn candidate_cannot_follow_a_replaced_payload_parent() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let path = payload(&root, 1, 7).await;
    let relative = relative_path(&root, &path).unwrap();
    let catalog = CacheCatalog::new(root.clone(), 1024 * 1024);
    catalog
        .record_sync(&open_catalog(&root).unwrap(), "shard", &path, "fixture", 7)
        .unwrap();
    let mut connection = open_catalog(&root).unwrap();
    let pinned = PinnedRoot::open(&root).unwrap();
    let outside = tmp.path().join("outside");
    crate::private_fs::atomic_write(&outside, &outside.join(&relative), b"outside")
        .await
        .unwrap();
    std::fs::rename(root.join("shards"), tmp.path().join("moved")).unwrap();
    std::os::unix::fs::symlink(outside.join("shards"), root.join("shards")).unwrap();
    assert!(matches!(
        catalog
            .evict_candidate(&pinned, &mut connection, &relative)
            .unwrap(),
        Eviction::Retained
    ));
    assert_eq!(std::fs::read(outside.join(relative)).unwrap(), b"outside");
}

#[tokio::test]
async fn recorded_family_cannot_authorize_unknown_or_database_deletion() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    ensure_private_cache_directory(&root).unwrap();
    let catalog = CacheCatalog::new(root.clone(), 1024 * 1024);
    let mut connection = open_catalog(&root).unwrap();
    let pinned = PinnedRoot::open(&root).unwrap();
    for relative in [CATALOG_FILE, "shards/notes", "profiles/session.json"] {
        let path = root.join(relative);
        if relative != CATALOG_FILE {
            crate::private_fs::atomic_write(&root, &path, b"keep")
                .await
                .unwrap();
        }
        catalog
            .record_sync(&connection, "shard", &path, "not-a-payload", 4)
            .unwrap();
        assert!(matches!(
            catalog
                .evict_candidate(&pinned, &mut connection, relative)
                .unwrap(),
            Eviction::Retained
        ));
        assert!(path.exists());
    }
}

#[tokio::test]
async fn maintenance_preserves_an_actual_read_descriptor() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let active = payload(&root, 1, 400 * 1024).await;
    let idle = payload(&root, 2, 400 * 1024).await;
    let reader = crate::private_fs::open_read(&root, &active).await.unwrap();
    let catalog = CacheCatalog::new(root, 600 * 1024);
    let result = catalog.maintain().await.unwrap();
    assert_eq!(result.evicted_bytes, 400 * 1024);
    assert!(active.exists());
    assert!(!idle.exists());
    drop(reader);
}

#[cfg(unix)]
#[tokio::test]
async fn maintenance_does_not_follow_a_lock_symlink_or_change_its_target() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let path = payload(&root, 1, 7).await;
    let outside = tmp.path().join("outside");
    std::fs::write(&outside, b"keep").unwrap();
    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o644)).unwrap();
    symlink(&outside, root.join(MAINTENANCE_LOCK)).unwrap();
    assert!(matches!(
        CacheCatalog::new(root, 1).maintain().await,
        Err(CacheError::UnsafeRoot { .. })
    ));
    assert!(path.exists());
    assert_eq!(std::fs::read(&outside).unwrap(), b"keep");
    assert_eq!(
        std::fs::metadata(outside).unwrap().permissions().mode() & 0o777,
        0o644
    );
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_owners_never_recreates_or_follows_a_replaced_catalog() {
    for link in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let path = payload(&root, 1, 7).await;
        let catalog = CacheCatalog::new(root.clone(), 1024 * 1024);
        let lease = catalog.lease(&path).await.unwrap();
        let reservation = catalog.reserve(&path, 7).await.unwrap().unwrap();
        let old = tmp.path().join("retired.sqlite");
        std::fs::rename(root.join(CATALOG_FILE), &old).unwrap();
        if link {
            std::os::unix::fs::symlink(&old, root.join(CATALOG_FILE)).unwrap();
        }
        drop(lease);
        drop(reservation);
        if !link {
            assert!(!root.join(CATALOG_FILE).exists());
        }
        assert!(!root.join(format!("{CATALOG_FILE}-wal")).exists());
        assert!(!root.join(format!("{CATALOG_FILE}-shm")).exists());
        let old = Connection::open_with_flags(old, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let owners: u64 = old
            .query_row(
                "SELECT (SELECT COUNT(*) FROM leases) + (SELECT COUNT(*) FROM reservations)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(owners, 2);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn unsafe_inventory_rolls_back_before_eviction() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    for kind in [
        "symlink",
        "fifo",
        "hard-link",
        "readable-file",
        "readable-directory",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let path = payload(&root, 1, 7).await;
        let catalog = CacheCatalog::new(root.clone(), 1024 * 1024);
        catalog.maintain().await.unwrap();
        let before = CacheCatalog::read_only_stats(&root).unwrap();
        let unsafe_path = root.join("unexpected");
        match kind {
            "symlink" => symlink(&path, &unsafe_path).unwrap(),
            "fifo" => {
                use std::os::unix::ffi::OsStrExt as _;
                let name = std::ffi::CString::new(unsafe_path.as_os_str().as_bytes()).unwrap();
                // SAFETY: the NUL-terminated path is a disposable fixture.
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            }
            "hard-link" => std::fs::hard_link(&path, &unsafe_path).unwrap(),
            "readable-file" => {
                std::fs::write(&unsafe_path, b"keep").unwrap();
                std::fs::set_permissions(&unsafe_path, std::fs::Permissions::from_mode(0o644))
                    .unwrap();
            }
            "readable-directory" => {
                std::fs::create_dir(&unsafe_path).unwrap();
                std::fs::set_permissions(&unsafe_path, std::fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let error = CacheCatalog::new(root.clone(), 1)
            .maintain()
            .await
            .unwrap_err();
        assert!(
            matches!(error, CacheError::UnsafeRoot { .. }),
            "{kind}: {error}"
        );
        assert_eq!(
            CacheCatalog::read_only_stats(&root).unwrap(),
            before,
            "{kind}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), vec![1; 7], "{kind}");
    }
}

#[cfg(unix)]
#[test]
fn inventory_preserves_sqlite_writer_locks_across_processes() {
    const CHILD_ROOT: &str = "CRAB_TEST_INVENTORY_LOCK_ROOT";
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let connection = Connection::open(PathBuf::from(root).join(CATALOG_FILE)).unwrap();
        connection.busy_timeout(std::time::Duration::ZERO).unwrap();
        let error = connection.execute_batch("BEGIN IMMEDIATE").unwrap_err();
        assert!(
            matches!(error, rusqlite::Error::SqliteFailure(failure, _) if failure.code == rusqlite::ErrorCode::DatabaseBusy)
        );
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    ensure_private_cache_directory(&root).unwrap();
    let mut connection = open_catalog(&root).unwrap();
    let pinned = PinnedRoot::open(&root).unwrap();
    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let mut seen = Vec::new();
    pinned
        .visit_files(&mut |relative, _| {
            seen.push(relative.to_owned());
            Ok(())
        })
        .unwrap();
    for file in [CATALOG_FILE, ".catalog.sqlite-wal", ".catalog.sqlite-shm"] {
        assert!(seen.contains(&PathBuf::from(file)), "{file}");
    }
    drop(
        open_database(
            &root,
            &root.join(CATALOG_FILE),
            DatabaseMode::ReadOnly,
            std::time::Duration::ZERO,
        )
        .unwrap(),
    );
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "catalog::tests::inventory_preserves_sqlite_writer_locks_across_processes",
            "--nocapture",
        ])
        .env(CHILD_ROOT, &root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    transaction.commit().unwrap();
    let mut second = open_catalog(&root).unwrap();
    second
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap()
        .commit()
        .unwrap();
}

#[cfg(unix)]
#[test]
fn catalog_keys_reject_non_utf8_without_lossy_aliases() {
    use std::os::unix::ffi::OsStringExt as _;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let name = std::ffi::OsString::from_vec(vec![0xff]);
    let catalog = CacheCatalog::new(root.clone(), 1024);
    assert!(matches!(
        catalog.reserve_sync(&root.join(name), 1),
        Err(CacheError::UnsafeRoot { .. })
    ));
    assert!(!root.join(CATALOG_FILE).exists());
}
