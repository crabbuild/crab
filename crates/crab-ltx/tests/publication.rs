#![cfg(feature = "replica")]

use crab_ltx::{
    CaptureBatch, CompactionSchedule, Limits, ManagedDb, Replica,
    bundle::{Bundle, BundleEntry},
};
use crab_storage::{Store, StoreLayout};
use object_store::memory::InMemory;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

fn replica(store: Store, epoch: &str, limits: Limits) -> Replica {
    Replica::new(StoreLayout::new(store, "publication".into()), epoch, limits).unwrap()
}

fn observed_store() -> (Store, Arc<AtomicU64>) {
    let bytes = Arc::new(AtomicU64::new(0));
    let observer = bytes.clone();
    let store = Store::new(Arc::new(InMemory::new())).with_read_byte_observer(Arc::new(move |n| {
        observer.fetch_add(n, Ordering::SeqCst);
    }));
    (store, bytes)
}

fn writer(directory: &tempfile::TempDir) -> ManagedDb {
    let mut writer = ManagedDb::open(&directory.path().join("db"), Limits::default()).unwrap();
    writer
        .transaction(|tx| {
            tx.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES(randomblob(2000000))")
        })
        .unwrap();
    writer
}

fn bundle(batch: &CaptureBatch, epoch: &str) -> Bundle {
    Bundle::encode(
        batch
            .segments
            .iter()
            .map(|s| BundleEntry {
                repository: "publication".into(),
                epoch: epoch.into(),
                info: s.info().clone(),
                bytes: std::fs::read(s.path()).unwrap(),
            })
            .collect(),
        Limits::default(),
    )
    .unwrap()
}

#[tokio::test]
async fn range_compaction_does_not_download_unselected_bodies() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer = writer(&directory);
    let (store, reads) = observed_store();
    let remote = replica(store, "one", Limits::default());
    let mut head = remote
        .replicate(&writer.capture().unwrap(), None)
        .await
        .unwrap();
    for _ in 0..2 {
        writer
            .transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(2)"))
            .unwrap();
        head = remote
            .replicate(&writer.capture().unwrap(), Some(&head))
            .await
            .unwrap();
    }
    reads.store(0, Ordering::SeqCst);
    let compacted = remote.compact_range(&head, 1..3, 1).await.unwrap();
    let downloaded = reads.load(Ordering::SeqCst);
    assert!(
        downloaded < 100_000,
        "two small deltas downloaded {downloaded} bytes of history"
    );
    remote
        .restore(&head, &directory.path().join("before"))
        .await
        .unwrap();
    remote
        .restore(&compacted, &directory.path().join("after"))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(directory.path().join("before")).unwrap(),
        std::fs::read(directory.path().join("after")).unwrap()
    );
}

#[tokio::test]
async fn snapshot_returns_pending_cuts_and_publication_continues() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer = writer(&directory);
    let remote = replica(
        Store::new(Arc::new(InMemory::new())),
        "one",
        Limits::default(),
    );
    let head = remote
        .replicate(&writer.capture().unwrap(), None)
        .await
        .unwrap();
    writer
        .transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(2)"))
        .unwrap();
    let (snapshot, pending) = writer
        .snapshot(&directory.path().join("snapshot.ltx"))
        .unwrap();
    let head = remote.replicate(&pending, Some(&head)).await.unwrap();
    assert_eq!(head.position(), snapshot.info().position());
    writer
        .transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(3)"))
        .unwrap();
    let head = remote
        .replicate(&writer.capture().unwrap(), Some(&head))
        .await
        .unwrap();
    let destination = directory.path().join("restored");
    remote.restore(&head, &destination).await.unwrap();
    let sql = crab_ltx::rusqlite::Connection::open(destination).unwrap();
    assert_eq!(
        sql.query_row("SELECT count(*) FROM t", [], |r| r.get::<_, u32>(0))
            .unwrap(),
        3
    );
}

#[tokio::test]
async fn inheritance_admits_every_destination_limit_before_io() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer = writer(&directory);
    let (store, reads) = observed_store();
    let source = replica(store.clone(), "one", Limits::default());
    let head = source
        .replicate(&writer.capture().unwrap(), None)
        .await
        .unwrap();
    writer
        .transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(2)"))
        .unwrap();
    let head = source
        .replicate(&writer.capture().unwrap(), Some(&head))
        .await
        .unwrap();
    for limits in [
        Limits {
            max_file_bytes: 1024,
            ..Limits::default()
        },
        Limits {
            max_file_bytes: 1024,
            max_plan_bytes: 1024,
            ..Limits::default()
        },
        Limits {
            max_database_bytes: 512,
            ..Limits::default()
        },
        Limits {
            max_segments: 1,
            ..Limits::default()
        },
    ] {
        let destination = replica(store.clone(), "two", limits);
        reads.store(0, Ordering::SeqCst);
        assert!(destination.inherit(&source, &head).await.is_err());
        assert_eq!(
            reads.load(Ordering::SeqCst),
            0,
            "admission must precede source and destination reads"
        );
        assert!(destination.head().await.unwrap().is_none());
    }
}

#[tokio::test]
async fn idle_singleton_advances_through_all_scheduled_levels() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer = writer(&directory);
    let remote = replica(
        Store::new(Arc::new(InMemory::new())),
        "one",
        Limits::default(),
    );
    let mut head = remote
        .replicate(&writer.capture().unwrap(), None)
        .await
        .unwrap();
    let initial = head.clone();
    let mut schedule = CompactionSchedule::default();
    for seconds in [30, 300, 3600] {
        let previous = head.manifest_digest();
        head = schedule
            .run_due(&remote, &head, Duration::from_secs(seconds))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(head.manifest_digest(), previous);
        assert_eq!(head.position(), initial.position());
    }
    remote
        .restore(&initial, &directory.path().join("before"))
        .await
        .unwrap();
    remote
        .restore(&head, &directory.path().join("after"))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(directory.path().join("before")).unwrap(),
        std::fs::read(directory.path().join("after")).unwrap()
    );
}

#[tokio::test]
async fn native_and_bundle_appends_reuse_verified_maps_and_reopen_from_indexes() {
    for bundled in [false, true] {
        let directory = tempfile::TempDir::new().unwrap();
        let mut writer = writer(&directory);
        let (store, reads) = observed_store();
        let remote = replica(store.clone(), "one", Limits::default());
        let mut head = remote
            .replicate(&writer.capture().unwrap(), None)
            .await
            .unwrap();
        for cold in [false, true] {
            if cold {
                head = remote.head().await.unwrap().unwrap();
            }
            writer
                .transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(2)"))
                .unwrap();
            let batch = writer.capture().unwrap();
            reads.store(0, Ordering::SeqCst);
            head = if bundled {
                remote
                    .replicate_bundle(&bundle(&batch, "one"), Some(&head))
                    .await
                    .unwrap()
            } else {
                remote.replicate(&batch, Some(&head)).await.unwrap()
            };
            let downloaded = reads.load(Ordering::SeqCst);
            if cold {
                assert!(
                    downloaded > 0 && downloaded < 100_000,
                    "cold append downloaded {downloaded} bytes"
                );
            } else {
                assert_eq!(downloaded, 0, "live append must not redownload history");
            }
        }
        remote
            .restore(&head, &directory.path().join("restored"))
            .await
            .unwrap();
        let sql = crab_ltx::rusqlite::Connection::open(directory.path().join("restored")).unwrap();
        assert_eq!(
            sql.query_row("SELECT count(*) FROM t", [], |r| r.get::<_, u32>(0))
                .unwrap(),
            3
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn takeover_opens_sparse_sql_without_downloading_predecessor_bodies() {
    for bundled in [false, true] {
        let directory = tempfile::TempDir::new().unwrap();
        let mut writer = writer(&directory);
        let (store, reads) = observed_store();
        let source = replica(store.clone(), "one", Limits::default());
        let batch = writer.capture().unwrap();
        let head = if bundled {
            source
                .replicate_bundle(&bundle(&batch, "one"), None)
                .await
                .unwrap()
        } else {
            source.replicate(&batch, None).await.unwrap()
        };
        writer.close().unwrap();
        let destination = replica(store, "two", Limits::default());
        reads.store(0, Ordering::SeqCst);
        let inherited = destination.inherit(&source, &head).await.unwrap();
        assert!(
            reads.load(Ordering::SeqCst) < 50_000,
            "inheritance should fetch only indexes"
        );
        let paged = destination.paged(&inherited).await.unwrap();
        let path = directory.path().join("sparse");
        let mut writer = tokio::task::spawn_blocking(move || paged.open_writable(&path).unwrap())
            .await
            .unwrap();
        assert!(!writer.hydration().unwrap().unwrap().complete());
        assert!(
            reads.load(Ordering::SeqCst) < 500_000,
            "activation must not read the 2 MB predecessor body"
        );
        writer
            .transaction(|tx| tx.execute_batch("INSERT INTO t VALUES('continued')"))
            .unwrap();
        let batch = writer.capture().unwrap();
        reads.store(0, Ordering::SeqCst);
        let head = destination
            .replicate(&batch, Some(&inherited))
            .await
            .unwrap();
        assert_eq!(
            reads.load(Ordering::SeqCst),
            0,
            "inherited map must seed incremental publication"
        );
        destination
            .restore(&head, &directory.path().join("restored"))
            .await
            .unwrap();
        let sql = crab_ltx::rusqlite::Connection::open(directory.path().join("restored")).unwrap();
        assert_eq!(
            sql.query_row(
                "SELECT length(v) FROM t ORDER BY rowid DESC LIMIT 1",
                [],
                |r| r.get::<_, u32>(0)
            )
            .unwrap(),
            9
        );
    }
}

#[tokio::test]
async fn a_receipt_never_reuses_cached_indexes_on_a_different_replica_store() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer = writer(&directory);
    let source = replica(
        Store::new(Arc::new(InMemory::new())),
        "one",
        Limits::default(),
    );
    let head = source
        .replicate(&writer.capture().unwrap(), None)
        .await
        .unwrap();
    writer
        .transaction(|tx| tx.execute_batch("INSERT INTO t VALUES(2)"))
        .unwrap();
    let empty = Store::new(Arc::new(InMemory::new()));
    let other = replica(empty.clone(), "one", Limits::default());
    let result = other
        .replicate(&writer.capture().unwrap(), Some(&head))
        .await;
    assert!(
        matches!(result,
            Err(crab_ltx::CrabError::Storage(crab_storage::StorageError::NotFound { path }))
                if path.ends_with(".idx")
        ),
        "a foreign store must resolve predecessor indexes before attempting head CAS"
    );
    let next = replica(empty, "two", Limits::default());
    assert!(next.inherit(&source, &head).await.is_err());
    assert!(next.head().await.unwrap().is_none());
}

#[tokio::test]
async fn lazy_inheritance_rejects_missing_objects_and_checks_corrupt_pages_on_read() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer = writer(&directory);
    let store = Store::new(Arc::new(InMemory::new()));
    let source = replica(store.clone(), "one", Limits::default());
    let batch = writer.capture().unwrap();
    let head = source.replicate(&batch, None).await.unwrap();
    let segment = &batch.segments[0];
    let hash = blake3::Hash::from_bytes(segment.info().blake3).to_hex();
    let key = object_store::path::Path::from(format!("publication/ltx/one/objects/{hash}.ltx"));
    store.delete(&key).await.unwrap();
    let next = replica(store.clone(), "two", Limits::default());
    assert!(next.inherit(&source, &head).await.is_err());
    assert!(next.head().await.unwrap().is_none());
    let mut bytes = std::fs::read(segment.path()).unwrap();
    bytes[110] ^= 1;
    store.put(&key, bytes.into()).await.unwrap();
    let inherited = next.inherit(&source, &head).await.unwrap();
    let pages = next.paged(&inherited).await.unwrap();
    assert!(matches!(
        pages.read_page(1).await,
        Err(crab_ltx::CrabError::ChecksumMismatch)
    ));
    assert!(
        next.restore(&inherited, &directory.path().join("must-not-exist"))
            .await
            .is_err()
    );
    assert!(!directory.path().join("must-not-exist").exists());
}

#[tokio::test]
async fn recovery_admission_is_released_before_returning_long_lived_handles() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer = writer(&directory);
    let slots = Arc::new(tokio::sync::Semaphore::new(1));
    let host = crab_ltx::Host::default().with_recovery_slots(slots.clone());
    let remote = replica(
        Store::new(Arc::new(InMemory::new())),
        "one",
        Limits::default(),
    )
    .with_host(host);
    let head = remote
        .replicate(&writer.capture().unwrap(), None)
        .await
        .unwrap();
    let head = remote.compact(&head).await.unwrap();
    assert_eq!(
        slots.available_permits(),
        1,
        "cached page maps must not retain recovery admission"
    );
    let resumed = remote
        .resume(&head, &directory.path().join("resumed"))
        .await
        .unwrap();
    assert_eq!(
        slots.available_permits(),
        1,
        "a live writer must not retain recovery admission"
    );
    let head = remote.bundle(&head).await.unwrap();
    assert_eq!(slots.available_permits(), 1);
    remote
        .restore(&head, &directory.path().join("restored"))
        .await
        .unwrap();
    assert_eq!(slots.available_permits(), 1);
    resumed.close().unwrap();
}
