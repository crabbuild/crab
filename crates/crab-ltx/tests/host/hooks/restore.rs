//! Restore write coalescing and scratch cleanup.

use super::*;

#[cfg(feature = "replica")]
#[tokio::test(flavor = "multi_thread")]
async fn cell_restore_write_and_install_failures_clean_owned_scratch() {
    let (directory, faults, host, mut writer) = fixture();
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            ObjectPath::from("cell-restore"),
            [1; 16],
        ),
        [2; 32],
        [3; 16],
        Limits::default(),
    )
    .unwrap()
    .with_host(host);
    let root = replica
        .prepare(None, &writer.capture().unwrap(), 1, 1)
        .await
        .unwrap()
        .root();
    writer.close().unwrap();
    let verified = replica.open_root(&root).await.unwrap();
    let destination = directory.path().join("cell-restored.sqlite");

    faults.arm(Some("write_all"));
    injected(verified.restore(&destination).await);
    assert!(!destination.exists());
    assert!(!std::fs::read_dir(directory.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".crab-restore-")
    }));

    faults.arm(Some("persist_file_new"));
    injected(verified.restore(&destination).await);
    assert!(!destination.exists());
    assert!(!std::fs::read_dir(directory.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".crab-restore-")
    }));

    faults.arm(None);
    assert_eq!(verified.restore(&destination).await.unwrap(), root.position);
}
#[cfg(feature = "replica")]
#[tokio::test(flavor = "multi_thread")]
async fn cell_restore_coalesces_page_writes_within_bounded_windows() {
    let directory = tempfile::TempDir::new().unwrap();
    let faults = Arc::new(Faults::default());
    let host = Host::default().with_filesystem(faults.clone());
    let mut writer = Db::open_with_host(
        &directory.path().join("source.sqlite"),
        Limits::default(),
        host.clone(),
    )
    .unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE payload(value BLOB NOT NULL);\
                 INSERT INTO payload VALUES(randomblob(3000000))",
            )
        })
        .unwrap();
    let expected: Vec<u8> = writer
        .query_with(|connection| {
            connection.query_row("SELECT value FROM payload", [], |row| row.get(0))
        })
        .unwrap();
    let replica = CellReplica::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            ObjectPath::from("coalesced-restore-output"),
            [69; 16],
        ),
        [70; 32],
        [71; 16],
        Limits::default(),
    )
    .unwrap()
    .with_host(host);
    let root = replica
        .prepare(None, &writer.capture().unwrap(), 1, 1)
        .await
        .unwrap()
        .root();
    writer.close().unwrap();
    let verified = replica.open_root(&root).await.unwrap();
    faults.track_all.store(true, Ordering::Relaxed);
    faults.write_calls.store(0, Ordering::Relaxed);
    let destination = directory.path().join("restored.sqlite");

    assert_eq!(verified.restore(&destination).await.unwrap(), root.position);
    let actual: Vec<u8> = crab_ltx::rusqlite::Connection::open(&destination)
        .unwrap()
        .query_row("SELECT value FROM payload", [], |row| row.get(0))
        .unwrap();
    assert_eq!(actual, expected);
    assert!(
        faults.write_calls.load(Ordering::Relaxed) <= 16,
        "restore used {} local writes",
        faults.write_calls.load(Ordering::Relaxed)
    );
    assert!(faults.largest_write.load(Ordering::Relaxed) <= 1 << 20);
}
