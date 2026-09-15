use super::*;
use crab_ltx::{CellReplica, Limits};
use crab_storage::{CellStorageLayout, Store};
use object_store::{
    memory::InMemory,
    path::Path,
    throttle::{ThrottleConfig, ThrottledStore},
};
use std::sync::atomic::{AtomicU64, Ordering};

struct SparseActivation {
    _source: tempfile::TempDir,
    _destination: tempfile::TempDir,
    cell: CellId,
    incarnation: crate::IncarnationId,
    root: crab_ltx::RootRef,
    database: crab_ltx::CellWritableDatabase,
    destination: PathBuf,
}

async fn sparse_activation(cell_byte: u8, store: Store) -> SparseActivation {
    let cell = CellId::from_bytes([cell_byte; 32]);
    let incarnation = crate::IncarnationId::from_bytes([cell_byte + 16; 16]);
    let layout = CellStorageLayout::new(store, Path::from("sparse-workers"), [9; 16]);
    let replica = CellReplica::new(
        layout,
        *cell.as_bytes(),
        *incarnation.as_bytes(),
        Limits::default(),
    )
    .unwrap();
    let source = tempfile::TempDir::new().unwrap();
    let managed = replica
        .open_new(&source.path().join("source.sqlite"))
        .unwrap();
    let (executor, cuts, _) =
        CellExecutor::bootstrap(managed, cell, incarnation, 1, |transaction| {
            transaction.execute_batch(
                "CREATE TABLE payload(value BLOB NOT NULL); INSERT INTO payload VALUES(randomblob(262144))",
            )?;
            Ok(())
        })
        .unwrap();
    executor.close().unwrap();
    let root = replica.prepare(None, &cuts, 0, 1).await.unwrap().root();
    let verified = replica.open_root(&root).await.unwrap();
    let destination_dir = tempfile::TempDir::new().unwrap();
    let destination = destination_dir.path().join("active.sqlite");
    let database = verified
        .paged()
        .prepare_writable(&destination)
        .await
        .unwrap();
    SparseActivation {
        _source: source,
        _destination: destination_dir,
        cell,
        incarnation,
        root,
        database,
        destination,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sparse_fault_pool_progresses_under_saturated_sql_workers() {
    let backend = Arc::new(ThrottledStore::new(
        InMemory::new(),
        ThrottleConfig::default(),
    ));
    let read_bytes = Arc::new(AtomicU64::new(0));
    let observed = read_bytes.clone();
    let store = Store::new(backend.clone()).with_read_byte_observer(Arc::new(move |bytes| {
        observed.fetch_add(bytes, Ordering::Relaxed);
    }));
    // Repeated-byte Cell prefixes select distinct workers modulo two.
    let first = sparse_activation(0, store.clone()).await;
    let second = sparse_activation(1, store).await;
    assert_eq!(worker_index(first.cell, 2), 0);
    assert_eq!(worker_index(second.cell, 2), 1);
    read_bytes.store(0, Ordering::Relaxed);
    backend.config_mut(|config| config.wait_get_per_call = Duration::from_millis(100));

    let pool = SqlWorkerPool::new(2, 2).unwrap();
    let first_reservation = pool.reserve_activation().unwrap();
    let second_reservation = pool.reserve_activation().unwrap();
    let activation = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            pool.activate_restored(
                first.cell,
                first.database,
                first.destination,
                first.incarnation,
                1,
                first.root,
                first_reservation,
            ),
            pool.activate_restored(
                second.cell,
                second.database,
                second.destination,
                second.incarnation,
                1,
                second.root,
                second_reservation,
            ),
        )
    })
    .await
    .expect("dedicated sparse I/O worker must progress while both SQL workers wait");
    activation.0.unwrap();
    activation.1.unwrap();
    assert!(read_bytes.load(Ordering::Relaxed) > 0);

    pool.deactivate(first.cell).await.unwrap();
    pool.deactivate(second.cell).await.unwrap();
    pool.shutdown().await.unwrap();
}
