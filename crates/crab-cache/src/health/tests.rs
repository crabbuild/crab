use super::*;
#[cfg(feature = "local-cache")]
use crab_types::storage::{BucketIdentity, StorageProviderKind};
#[cfg(feature = "local-cache")]
use crab_xet::xorb::format::MerkleHash;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

fn fixture(root: &Path, relative: &str, data: &[u8]) {
    let path = root.join(relative);
    crate::ensure_private_cache_directory(path.parent().unwrap()).unwrap();
    std::fs::write(&path, data).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[cfg(feature = "local-cache")]
async fn shard_hint_fixture(root: &Path) {
    let scope = crate::shard_hints::ShardHintScope::new(
        &BucketIdentity::new(StorageProviderKind::S3, "bucket", "bucket"),
        ".crab",
    );
    crate::shard_hints::ShardHintCache::update(
        root,
        &scope,
        vec![(
            MerkleHash::from([1, 2, 3, 4]),
            MerkleHash::from([5, 6, 7, 8]),
        )],
    )
    .await
    .unwrap();
}

#[cfg(feature = "local-cache")]
fn tree_snapshot(
    root: &Path,
) -> std::collections::BTreeMap<PathBuf, (u32, u64, i64, i64, Vec<u8>)> {
    let mut snapshot = std::collections::BTreeMap::new();
    let mut pending = vec![root.to_owned()];
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        let relative = path.strip_prefix(root).unwrap().to_owned();
        let contents = if metadata.is_file() {
            std::fs::read(&path).unwrap()
        } else {
            pending.extend(
                std::fs::read_dir(&path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path()),
            );
            Vec::new()
        };
        snapshot.insert(
            relative,
            (
                metadata.mode(),
                metadata.ino(),
                metadata.mtime(),
                metadata.mtime_nsec(),
                contents,
            ),
        );
    }
    snapshot
}

#[test]
fn non_utf8_root_serializes_as_diagnostic_text_without_changing_identity() {
    use std::os::unix::ffi::OsStringExt as _;
    let root = PathBuf::from(std::ffi::OsString::from_vec(b"cache-\xff".to_vec()));
    let report = CacheHealthReport::new(root.clone(), 1024);
    let data = serde_json::to_value(&report).unwrap();
    assert_eq!(report.root, root);
    assert_eq!(data["root"], "cache-\u{fffd}");
}

// APFS rejects invalid UTF-8 names before cache inspection can run. Keep the
// actual on-disk case on Linux, separately from the portable serialization test.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn non_utf8_paths_remain_inspectable() {
    use std::os::unix::ffi::OsStringExt as _;
    let temp = tempfile::tempdir().unwrap();
    let root = temp
        .path()
        .join(std::ffi::OsString::from_vec(b"cache-\xff".to_vec()));
    fixture(&root, "shards/ab/valid", b"data");
    let path = root
        .join("shards/ab")
        .join(std::ffi::OsString::from_vec(vec![255]));
    std::fs::write(&path, b"other").unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let report = inspect_cache(&root, u64::MAX, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.root, root);
    let data = serde_json::to_value(report).unwrap();
    assert_eq!(data["families"]["shard"]["logical_bytes"], 9);
}

#[tokio::test]
async fn missing_cache_is_not_initialized_and_cancellation_wins() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("missing");
    let cancel = CancellationToken::new();
    let report = inspect_cache(&root, 1024, &cancel).await.unwrap();
    assert_eq!(report.root_state, CacheRootState::Missing);
    assert!(report.is_available());
    assert_eq!(report.observed.allocated_bytes, 0);
    cancel.cancel();
    assert!(matches!(
        inspect_cache(&root, 1024, &cancel).await,
        Err(CacheError::Cancelled)
    ));
    assert!(!root.exists());
}

#[tokio::test]
async fn every_family_and_directory_allocation_reconciles_with_native_stat() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    let entries = [
        ("chunks/ab/hash", "chunk"),
        ("chunks/ab/key/range", "decoded-range"),
        ("xorbs/ab/hash", "xorb"),
        ("shards/ab/hash", "shard"),
        ("manifests/repo", "manifest"),
        ("stages/repo", "stage"),
        ("buckets/scope/index.sqlite", "chunk-index"),
        ("xorb-index/index.sqlite-wal", "xorb-index"),
        ("hints/clean-bloom.bin", "bloom"),
        ("shard-hints.json", "shard-hint"),
        (".maintenance.lock", "lock"),
        (".tmp-payload", "temporary"),
        ("buckets/scope/.sqlite-temp-1", "temporary"),
        ("profile/retained.json", "other"),
    ];
    for (path, _) in entries {
        fixture(&root, path, b"payload");
    }
    // A sparse retained file distinguishes logical length from disk allocation.
    let sparse = root.join("profile/retained.json");
    std::fs::OpenOptions::new()
        .write(true)
        .open(&sparse)
        .unwrap()
        .set_len(8 * 1024 * 1024)
        .unwrap();
    CacheCatalog::new(root.clone(), u64::MAX)
        .maintain()
        .await
        .unwrap();
    let mut expected = CacheUsage::default();
    let mut pending = vec![root.clone()];
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        expected.allocated_bytes += metadata.blocks() * 512;
        if metadata.is_dir() {
            expected.directories += 1;
            pending.extend(
                std::fs::read_dir(path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path()),
            );
        } else {
            expected.files += 1;
            expected.logical_bytes += metadata.len();
        }
    }
    let report = inspect_cache(&root, expected.allocated_bytes, &CancellationToken::new())
        .await
        .unwrap();
    assert!(report.is_available(), "{:?}", report.issues);
    assert!(report.scan_complete);
    assert_eq!(
        serde_json::to_value(&report.observed).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
    assert_eq!(report.over_budget, Some(false));
    for (_, family) in entries {
        assert!(report.families[family].usage.files > 0, "{family}");
    }
    assert!(report.families["catalog"].usage.files > 0);
    assert!(report.families["directory"].usage.directories > 0);
    assert!(matches!(
        report.catalog,
        CacheCatalogHealth::Readable { .. }
    ));
    let sum: u64 = report
        .families
        .values()
        .map(|family| family.usage.allocated_bytes)
        .sum();
    assert_eq!(sum, report.observed.allocated_bytes);
    let report = inspect_cache(&root, sum - 1, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.over_budget, Some(true));
}

#[tokio::test]
async fn unsafe_family_preserves_independent_counts_and_strict_maintenance_failure() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    fixture(&root, "xorbs/ab/hash", b"valid");
    fixture(&root, "shards/ab/hash", b"private");
    fixture(&root, "chunks/ab/key/range", b"hidden");
    std::fs::set_permissions(root.join("chunks"), std::fs::Permissions::from_mode(0o777)).unwrap();
    let report = inspect_cache(&root, u64::MAX, &CancellationToken::new())
        .await
        .unwrap();
    assert!(!report.is_available());
    assert_eq!(report.over_budget, None);
    assert!(!report.families["chunk"].complete);
    assert!(!report.families["decoded-range"].complete);
    assert!(report.families["shard"].complete);
    assert_eq!(report.families["shard"].usage.logical_bytes, 7);
    assert_eq!(report.families["xorb"].usage.logical_bytes, 5);
    let over_budget = inspect_cache(&root, 1, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(over_budget.over_budget, Some(true));
    assert!(matches!(
        CacheCatalog::new(root.clone(), 1).maintain().await,
        Err(CacheError::UnsafeRoot { .. })
    ));
    assert_eq!(std::fs::read(root.join("xorbs/ab/hash")).unwrap(), b"valid");
}

#[tokio::test]
async fn busy_catalog_does_not_hide_payload_usage_or_claim_empty_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    fixture(&root, "shards/ab/hash", b"healthy");
    CacheCatalog::new(root.clone(), u64::MAX)
        .maintain()
        .await
        .unwrap();
    let pinned = PinnedRoot::open(&root).unwrap();
    let database = pinned
        .open_database(
            Path::new(".catalog.sqlite"),
            crate::private_fs::DatabaseMode::ReadWrite,
            std::time::Duration::ZERO,
        )
        .unwrap();
    database.execute_batch("BEGIN IMMEDIATE").unwrap();
    let report = inspect_cache(&root, u64::MAX, &CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(report.catalog, CacheCatalogHealth::Unavailable));
    assert_eq!(report.issues[0].kind, CacheIssueKind::Busy);
    assert_eq!(report.families["shard"].usage.logical_bytes, 7);
    assert!(report.families["shard"].complete);
    assert!(report.scan_complete);
    database.execute_batch("ROLLBACK").unwrap();
    drop(database);
    let recovered = inspect_cache(&root, u64::MAX, &CancellationToken::new())
        .await
        .unwrap();
    assert!(recovered.is_available());
}

#[cfg(feature = "local-cache")]
#[tokio::test]
async fn shard_hint_health_validates_without_mutating_database() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    shard_hint_fixture(&root).await;
    let before = tree_snapshot(&root);

    let report = inspect_cache(&root, u64::MAX, &CancellationToken::new())
        .await
        .unwrap();

    assert!(report.is_available(), "{:?}", report.issues);
    assert!(report.scan_complete);
    assert!(report.families["shard-hint"].complete);
    assert_eq!(report.families["shard-hint"].issues, 0);
    assert_eq!(tree_snapshot(&root), before);
}

#[cfg(feature = "local-cache")]
#[tokio::test]
async fn malformed_shard_hint_rows_are_reported_without_repair() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    shard_hint_fixture(&root).await;
    fixture(&root, "shards/ab/hash", b"healthy");
    let pinned = PinnedRoot::open(&root).unwrap();
    let database = pinned
        .open_database(
            Path::new(crate::shard_hints::SHARD_HINTS_DATABASE),
            crate::private_fs::DatabaseMode::ReadWrite,
            std::time::Duration::ZERO,
        )
        .unwrap();
    database
        .execute_batch(
            "INSERT INTO shard_hints(scope, file_hash, shard_hash)
             VALUES ('12345678901234567890123456789012', zeroblob(32), zeroblob(32));",
        )
        .unwrap();
    drop(database);
    drop(pinned);
    let before = tree_snapshot(&root);

    let report = inspect_cache(&root, u64::MAX, &CancellationToken::new())
        .await
        .unwrap();

    assert!(!report.is_available());
    assert!(report.scan_complete);
    assert_eq!(report.issues.len(), 1);
    assert_eq!(report.issues[0].family, Some("shard-hint"));
    assert_eq!(report.issues[0].kind, CacheIssueKind::Corrupt);
    assert!(report.families["shard-hint"].complete);
    assert_eq!(report.families["shard-hint"].issues, 1);
    assert_eq!(report.families["shard"].usage.logical_bytes, 7);
    assert_eq!(tree_snapshot(&root), before);
}

#[cfg(feature = "local-cache")]
#[tokio::test]
async fn busy_shard_hint_database_is_reported_and_recovers() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    shard_hint_fixture(&root).await;
    fixture(&root, "shards/ab/hash", b"healthy");
    let pinned = PinnedRoot::open(&root).unwrap();
    let database = pinned
        .open_database(
            Path::new(crate::shard_hints::SHARD_HINTS_DATABASE),
            crate::private_fs::DatabaseMode::ReadWrite,
            std::time::Duration::ZERO,
        )
        .unwrap();
    database
        .execute_batch("PRAGMA journal_mode = DELETE; BEGIN EXCLUSIVE")
        .unwrap();

    let report = inspect_cache(&root, u64::MAX, &CancellationToken::new())
        .await
        .unwrap();

    assert!(!report.is_available());
    assert!(report.scan_complete);
    assert_eq!(report.issues.len(), 1);
    assert_eq!(report.issues[0].family, Some("shard-hint"));
    assert_eq!(report.issues[0].kind, CacheIssueKind::Busy);
    assert_eq!(report.families["shard"].usage.logical_bytes, 7);
    database.execute_batch("ROLLBACK").unwrap();
    drop(database);
    drop(pinned);

    let recovered = inspect_cache(&root, u64::MAX, &CancellationToken::new())
        .await
        .unwrap();
    assert!(recovered.is_available(), "{:?}", recovered.issues);
}

#[tokio::test]
async fn issue_details_are_bounded_without_hiding_counts_or_following_links() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    fixture(&root, "shards/ab/hash", b"private");
    for n in 0..MAX_ISSUES + 5 {
        std::os::unix::fs::symlink(temp.path(), root.join(format!("bad-{n}"))).unwrap();
    }
    let report = inspect_cache(&root, u64::MAX, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.issues.len(), MAX_ISSUES);
    assert_eq!(report.omitted_issues, 5);
    assert_eq!(report.families["other"].issues, (MAX_ISSUES + 5) as u64);
    assert_eq!(report.families["shard"].usage.logical_bytes, 7);
    assert!(report.families["shard"].complete);
    assert!(!report.families["bloom"].complete);
    assert!(
        report
            .issues
            .iter()
            .all(|issue| issue.kind == CacheIssueKind::UnsafePath)
    );
}

#[tokio::test]
async fn corrupt_and_orphaned_catalogs_are_unavailable_not_empty_or_repaired() {
    for (filename, kind) in [
        (".catalog.sqlite", CacheIssueKind::Corrupt),
        (".catalog.sqlite-wal", CacheIssueKind::UnsafePath),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        fixture(&root, "shards/ab/hash", b"healthy");
        if filename == ".catalog.sqlite" {
            CacheCatalog::new(root.clone(), u64::MAX)
                .maintain()
                .await
                .unwrap();
        }
        // Overwrite the existing main inode so its real generation owner is
        // valid and the probe reaches SQLite's corrupt-header check.
        fixture(&root, filename, b"invalid sqlite");
        let report = inspect_cache(&root, u64::MAX, &CancellationToken::new())
            .await
            .unwrap();
        assert!(matches!(report.catalog, CacheCatalogHealth::Unavailable));
        assert_eq!(report.issues[0].kind, kind, "{:?}", report.issues);
        assert!(report.scan_complete);
        assert_eq!(report.families["shard"].usage.logical_bytes, 7);
        assert_eq!(
            std::fs::read(root.join(filename)).unwrap(),
            b"invalid sqlite"
        );
        assert_eq!(
            root.join(".catalog.sqlite").exists(),
            filename == ".catalog.sqlite"
        );
    }
}

#[tokio::test]
async fn malformed_catalog_totals_preserve_independent_payload_health() {
    for (sql, kind) in [
        (
            "PRAGMA ignore_check_constraints = ON;
             INSERT INTO reservations VALUES ('valid', 'valid', 17, 1, 0), ('bad', 'bad', -1, 1, 0);",
            CacheIssueKind::Corrupt,
        ),
        (
            "UPDATE catalog_meta SET value = 'invalid' WHERE key = 'last_maintenance_unix_ms';",
            CacheIssueKind::Unavailable,
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        fixture(&root, "shards/ab/hash", b"healthy");
        CacheCatalog::new(root.clone(), u64::MAX)
            .maintain()
            .await
            .unwrap();
        let pinned = PinnedRoot::open(&root).unwrap();
        let database = pinned
            .open_database(
                Path::new(".catalog.sqlite"),
                crate::private_fs::DatabaseMode::Create,
                std::time::Duration::from_secs(1),
            )
            .unwrap();
        database.execute_batch("PRAGMA journal_mode = DELETE;").unwrap();
        database.execute_batch(sql).unwrap();
        drop(database);
        let before = std::fs::read(root.join(".catalog.sqlite")).unwrap();

        let report = inspect_cache(&root, u64::MAX, &CancellationToken::new())
            .await
            .unwrap();

        assert!(matches!(report.catalog, CacheCatalogHealth::Unavailable));
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].family, Some("catalog"));
        assert_eq!(report.issues[0].kind, kind);
        assert!(report.scan_complete);
        assert_eq!(report.families["shard"].usage.logical_bytes, 7);
        assert_eq!(std::fs::read(root.join(".catalog.sqlite")).unwrap(), before);
    }
}
