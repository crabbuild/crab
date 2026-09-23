use super::*;
use crab_ltx::CellStorageLayout;
use crab_ltx::{CellReplica, Limits};
use crab_storage::Store;
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
    incarnation: crate::identity::IncarnationId,
    root: crab_ltx::RootRef,
    database: crab_ltx::CellWritableDatabase,
    destination: PathBuf,
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_and_runtime_reservations_share_one_node_ledger() {
    let pool = SqlWorkerPool::new(1, 1).unwrap();
    pool.configure_retained_capacity(32).unwrap();
    let ledger = pool.resource_ledger();
    let active = pool.reserve_activation().unwrap();
    let retained = ledger
        .try_reserve(ResourceCost::zero().with_retained_bytes(16))
        .unwrap();
    let snapshot = ledger.snapshot().unwrap();
    assert_eq!(snapshot.used.active_cells(), 1);
    assert_eq!(snapshot.used.retained_bytes(), 16);
    assert!(
        ledger
            .try_reserve(ResourceCost::zero().with_retained_bytes(17))
            .is_err()
    );
    let job = ledger
        .try_reserve(ResourceCost::zero().with_primitive_jobs(1))
        .unwrap();
    assert!(
        ledger
            .try_reserve(ResourceCost::zero().with_primitive_jobs(1))
            .is_err()
    );
    drop(job);
    drop(retained);
    drop(active);
    assert_eq!(ledger.snapshot().unwrap().used, ResourceCost::zero());
    pool.shutdown().await.unwrap();
}

async fn sparse_activation(cell_byte: u8, store: Store) -> SparseActivation {
    let cell = CellId::from_bytes([cell_byte; 32]);
    let incarnation = crate::identity::IncarnationId::from_bytes([cell_byte + 16; 16]);
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

    let initial = pool.hydration(first.cell).await.unwrap().unwrap();
    let progressed = pool
        .hydrate(first.cell, 64, Instant::now() + Duration::from_secs(5))
        .await
        .unwrap()
        .unwrap();
    assert!(progressed.resolved >= initial.resolved);
    let mut progress = progressed;
    while !progress.complete() {
        progress = pool
            .hydrate(first.cell, 64, Instant::now() + Duration::from_secs(5))
            .await
            .unwrap()
            .unwrap();
    }
    read_bytes.store(0, Ordering::Relaxed);
    let length = pool
        .query(
            first.cell,
            1024,
            Instant::now() + Duration::from_secs(5),
            Box::new(|connection| {
                let length: i64 =
                    connection
                        .query_row("SELECT length(value) FROM payload", [], |row| row.get(0))?;
                Ok(length.to_le_bytes().to_vec())
            }),
        )
        .await
        .unwrap();
    assert_eq!(i64::from_le_bytes(length.try_into().unwrap()), 262_144);
    assert_eq!(read_bytes.load(Ordering::Relaxed), 0);

    pool.deactivate(first.cell).await.unwrap();
    pool.deactivate(second.cell).await.unwrap();
    pool.shutdown().await.unwrap();
}
