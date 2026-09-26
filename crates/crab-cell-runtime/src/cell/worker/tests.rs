use super::*;
use crab_ltx::CellStorageLayout;
use crab_ltx::{CellReplica, Limits};
use crab_storage::Store;
use object_store::{
    memory::InMemory,
    path::Path,
    throttle::{ThrottleConfig, ThrottledStore},
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

struct SparseActivation {
    _source: tempfile::TempDir,
    _destination: tempfile::TempDir,
    cell: CellId,
    incarnation: crate::identity::IncarnationId,
    replica: CellReplica,
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

async fn sparse_activation(cell_byte: u8, store: Store, payload_bytes: usize) -> SparseActivation {
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
            transaction.execute_batch("CREATE TABLE payload(value BLOB NOT NULL)")?;
            transaction.execute(
                "INSERT INTO payload VALUES(randomblob(?1))",
                [payload_bytes],
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
        replica,
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
    let first = sparse_activation(0, store.clone(), 262_144).await;
    let second = sparse_activation(1, store, 262_144).await;
    assert_eq!(worker_index(first.cell, 2), 0);
    assert_eq!(worker_index(second.cell, 2), 1);
    read_bytes.store(0, Ordering::Relaxed);
    backend.config_mut(|config| config.wait_get_per_call = Duration::from_millis(100));

    let pool = SqlWorkerPool::new(2, 2).unwrap();
    pool.configure_retained_capacity(1 << 20).unwrap();
    let first_reservation = pool.reserve_activation().unwrap();
    let second_reservation = pool.reserve_activation().unwrap();
    let activation = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            pool.activate_restored(
                first.cell,
                crate::cell::worker::RestoredDatabase::Paged(Box::new(first.database)),
                first.destination,
                first.incarnation,
                1,
                first.root,
                first_reservation,
            ),
            pool.activate_restored(
                second.cell,
                crate::cell::worker::RestoredDatabase::Paged(Box::new(second.database)),
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
    let HydrationStep::Progress(Some(progressed)) = pool
        .hydrate(first.cell, 64, Instant::now() + Duration::from_secs(5))
        .await
        .unwrap()
    else {
        panic!("hydration unexpectedly deferred")
    };
    assert!(progressed.resolved >= initial.resolved);
    let mut progress = progressed;
    while !progress.complete() {
        let HydrationStep::Progress(Some(next)) = pool
            .hydrate(first.cell, 64, Instant::now() + Duration::from_secs(5))
            .await
            .unwrap()
        else {
            panic!("hydration unexpectedly deferred")
        };
        progress = next;
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

#[tokio::test(flavor = "multi_thread")]
async fn background_hydration_leaves_both_workers_query_admission_available() {
    let backend = Arc::new(ThrottledStore::new(
        InMemory::new(),
        ThrottleConfig::default(),
    ));
    let armed = Arc::new(AtomicBool::new(false));
    let started = Arc::new(tokio::sync::Notify::new());
    let notify = started.clone();
    let watching = armed.clone();
    let store = Store::new(backend.clone()).with_read_request_observer(Arc::new(move |_| {
        if watching.load(Ordering::Acquire) {
            notify.notify_one();
        }
    }));
    let cold = sparse_activation(0, store, 4 << 20).await;
    let resident = sparse_activation(1, Store::new(Arc::new(InMemory::new())), 262_144).await;
    let queued = sparse_activation(2, Store::new(Arc::new(InMemory::new())), 262_144).await;
    let pool = SqlWorkerPool::new(2, 3).unwrap();
    pool.configure_retained_capacity(1 << 20).unwrap();
    let cells = [cold.cell, resident.cell, queued.cell];
    for activation in [&cold, &resident, &queued] {
        pool.activate_restored(
            activation.cell,
            RestoredDatabase::Paged(Box::new(activation.database.clone())),
            activation.destination.clone(),
            activation.incarnation,
            1,
            activation.root,
            pool.reserve_activation().unwrap(),
        )
        .await
        .unwrap();
    }
    for cell in [resident.cell, queued.cell] {
        while !pool.hydration(cell).await.unwrap().unwrap().complete() {
            pool.hydrate(cell, 64, Instant::now() + Duration::from_secs(5))
                .await
                .unwrap();
        }
    }

    backend.config_mut(|config| config.wait_get_per_call = Duration::from_secs(3));
    armed.store(true, Ordering::Release);
    let hydration = {
        let pool = pool.clone();
        tokio::spawn(async move {
            pool.hydrate(cold.cell, 64, Instant::now() + Duration::from_secs(10))
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("hydration must reach a delayed origin read");
    let query = |cell| {
        tokio::time::timeout(
            Duration::from_secs(1),
            pool.query(
                cell,
                8,
                Instant::now() + Duration::from_secs(10),
                Box::new(|connection| {
                    let length: i64 =
                        connection
                            .query_row("SELECT length(value) FROM payload", [], |row| row.get(0))?;
                    Ok(length.to_le_bytes().to_vec())
                }),
            ),
        )
    };
    let (same_worker, other_worker) = tokio::join!(query(queued.cell), query(resident.cell));
    backend.config_mut(|config| config.wait_get_per_call = Duration::ZERO);
    hydration.await.unwrap().unwrap();
    for cell in cells {
        pool.deactivate(cell).await.unwrap();
    }
    pool.shutdown().await.unwrap();
    for result in [same_worker, other_worker] {
        assert_eq!(
            result
                .expect("background hydration blocked a resident query")
                .unwrap(),
            262_144_i64.to_le_bytes()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires isolated RustFS credentials; reports injected-delay worker interference"]
async fn rustfs_sparse_reads_report_worker_interference() {
    fn payload_digest(connection: &crab_ltx::rusqlite::Connection) -> Result<Vec<u8>> {
        let value: Vec<u8> =
            connection.query_row("SELECT value FROM payload", [], |row| row.get(0))?;
        Ok(blake3::hash(&value).as_bytes().to_vec())
    }

    let required = |name| std::env::var(name).unwrap_or_else(|_| panic!("missing {name}"));
    let endpoint = required("CRAB_LTX_TEST_ENDPOINT");
    let store = crab_storage::build_explicit_store(
        &required("CRAB_LTX_TEST_BUCKET"),
        crab_storage::ObjectStoreCredentials::Aws {
            access_key_id: required("AWS_ACCESS_KEY_ID"),
            secret_access_key: required("AWS_SECRET_ACCESS_KEY"),
            session_token: None,
            region: "us-east-1".into(),
        },
        Some(&endpoint),
        endpoint.starts_with("http://"),
    )
    .unwrap();
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let prefix = format!("crab-runtime-tests/worker-interference/{run}");
    let backend = Arc::new(ThrottledStore::new(
        object_store::prefix::PrefixStore::new(store.inner().clone(), Path::from(prefix.clone())),
        ThrottleConfig::default(),
    ));
    let armed = Arc::new(AtomicBool::new(false));
    let started = Arc::new(tokio::sync::Notify::new());
    let notify = started.clone();
    let watching = armed.clone();
    let requests = Arc::new(AtomicU64::new(0));
    let observed = requests.clone();
    let bytes = Arc::new(AtomicU64::new(0));
    let transferred = bytes.clone();
    let store = Store::new(backend.clone())
        .with_read_byte_observer(Arc::new(move |count| {
            transferred.fetch_add(count, Ordering::Relaxed);
        }))
        .with_read_request_observer(Arc::new(move |_| {
            observed.fetch_add(1, Ordering::Relaxed);
            if watching.swap(false, Ordering::AcqRel) {
                notify.notify_one();
            }
        }));
    let cold = sparse_activation(0, store.clone(), 4 << 20).await;
    let same = sparse_activation(2, store.clone(), 262_144).await;
    let other = sparse_activation(1, store, 262_144).await;
    let pool = SqlWorkerPool::new(2, 3).unwrap();
    pool.configure_retained_capacity(1 << 20).unwrap();
    assert_eq!(worker_index(cold.cell, 2), worker_index(same.cell, 2));
    assert_ne!(worker_index(cold.cell, 2), worker_index(other.cell, 2));
    for activation in [&cold, &same, &other] {
        pool.activate_restored(
            activation.cell,
            RestoredDatabase::Paged(Box::new(activation.database.clone())),
            activation.destination.clone(),
            activation.incarnation,
            1,
            activation.root,
            pool.reserve_activation().unwrap(),
        )
        .await
        .unwrap();
    }
    for cell in [same.cell, other.cell] {
        while !pool.hydration(cell).await.unwrap().unwrap().complete() {
            pool.hydrate(cell, 64, Instant::now() + Duration::from_secs(10))
                .await
                .unwrap();
        }
    }
    let query = |cell| {
        let pool = pool.clone();
        async move {
            let started = Instant::now();
            let (timing, measured) = oneshot::channel();
            let output = pool
                .query(
                    cell,
                    8,
                    started + Duration::from_secs(10),
                    Box::new(move |connection| {
                        let entered = Instant::now();
                        let length: i64 = connection.query_row(
                            "SELECT length(value) FROM payload",
                            [],
                            |row| row.get(0),
                        )?;
                        let _ = timing.send((entered.duration_since(started), entered.elapsed()));
                        Ok(length.to_le_bytes().to_vec())
                    }),
                )
                .await
                .unwrap();
            assert_eq!(output, 262_144_i64.to_le_bytes());
            let elapsed = started.elapsed();
            let (admission, sql) = measured.await.unwrap();
            serde_json::json!({
                "elapsed_us": elapsed.as_micros(), "admission_and_queue_us": admission.as_micros(),
                "sql_us": sql.as_micros(),
            })
        }
    };
    let before = requests.load(Ordering::Relaxed);
    let (same_baseline, other_baseline) = tokio::join!(query(same.cell), query(other.cell));
    assert_eq!(
        requests.load(Ordering::Relaxed),
        before,
        "resident queries must use no origin reads"
    );

    backend.config_mut(|config| config.wait_get_per_call = Duration::from_millis(500));
    armed.store(true, Ordering::Release);
    let hydration_started = Instant::now();
    let hydration_bytes = bytes.load(Ordering::Relaxed);
    let hydration = {
        let pool = pool.clone();
        tokio::spawn(async move {
            pool.hydrate(cold.cell, 64, hydration_started + Duration::from_secs(10))
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("hydration must reach the real RustFS path");
    let (hydrated, same_wait, other_wait) =
        tokio::join!(hydration, query(same.cell), query(other.cell));
    let hydration_and_queries = hydration_started.elapsed();
    assert!(matches!(
        hydrated.unwrap().unwrap(),
        HydrationStep::Progress(Some(_))
    ));
    armed.store(false, Ordering::Release);
    backend.config_mut(|config| config.wait_get_per_call = Duration::ZERO);
    let hydration_requests = requests.load(Ordering::Relaxed) - before;
    let hydration_bytes = bytes.load(Ordering::Relaxed) - hydration_bytes;
    pool.deactivate(cold.cell).await.unwrap();

    let source = crab_ltx::rusqlite::Connection::open_with_flags(
        cold._source.path().join("source.sqlite"),
        crab_ltx::rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let expected_digest = payload_digest(&source).unwrap();
    drop(source);
    let mut demand_samples = Vec::new();
    // Each trial starts with a fresh sparse file for the same immutable root.
    // The origin and metadata caches stay warm; no hydrated local pages carry
    // between trials. Alternate order to expose shared-host timing variation.
    for (trial, delay_ms) in [0, 20, 20, 0, 0, 20].into_iter().enumerate() {
        let destination = cold
            ._destination
            .path()
            .join(format!("demand-{trial}.sqlite"));
        let database = cold
            .replica
            .open_root(&cold.root)
            .await
            .unwrap()
            .paged()
            .prepare_writable(&destination)
            .await
            .unwrap();
        pool.activate_restored(
            cold.cell,
            RestoredDatabase::Paged(Box::new(database)),
            destination,
            cold.incarnation,
            1,
            cold.root,
            pool.reserve_activation().unwrap(),
        )
        .await
        .unwrap();
        let before_requests = requests.load(Ordering::Relaxed);
        let before_bytes = bytes.load(Ordering::Relaxed);
        backend.config_mut(|config| config.wait_get_per_call = Duration::from_millis(delay_ms));
        armed.store(true, Ordering::Release);
        let demand_started = Instant::now();
        let demand = {
            let pool = pool.clone();
            tokio::spawn(async move {
                let digest = pool
                    .query(
                        cold.cell,
                        32,
                        demand_started + Duration::from_secs(30),
                        Box::new(payload_digest),
                    )
                    .await
                    .unwrap();
                (digest, demand_started.elapsed())
            })
        };
        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .expect("the demand query must fetch from RustFS");
        let (demand, same_wait, other_wait) =
            tokio::join!(demand, query(same.cell), query(other.cell));
        let (digest, elapsed) = demand.unwrap();
        assert_eq!(digest, expected_digest);
        let demand_requests = requests.load(Ordering::Relaxed) - before_requests;
        let demand_bytes = bytes.load(Ordering::Relaxed) - before_bytes;
        armed.store(false, Ordering::Release);
        backend.config_mut(|config| config.wait_get_per_call = Duration::ZERO);

        let before_warm = requests.load(Ordering::Relaxed);
        let warm_started = Instant::now();
        let digest = pool
            .query(
                cold.cell,
                32,
                warm_started + Duration::from_secs(10),
                Box::new(payload_digest),
            )
            .await
            .unwrap();
        assert_eq!(digest, expected_digest);
        assert_eq!(
            requests.load(Ordering::Relaxed),
            before_warm,
            "the repeated full-payload query must read only materialized pages"
        );
        demand_samples.push(serde_json::json!({
            "trial": trial, "injected_get_delay_ms": delay_ms,
            "demand_query_us": elapsed.as_micros(), "resident_payload_query_us": warm_started.elapsed().as_micros(),
            "same_worker": same_wait, "other_worker": other_wait,
            "origin_requests": demand_requests, "origin_bytes": demand_bytes,
        }));
        pool.deactivate(cold.cell).await.unwrap();
    }
    for cell in [same.cell, other.cell] {
        pool.deactivate(cell).await.unwrap();
    }
    pool.shutdown().await.unwrap();
    eprintln!(
        "worker-interference {}",
        serde_json::json!({
            "schema": 2,
            "prefix": prefix,
            "build_profile": if cfg!(debug_assertions) { "debug" } else { "release" },
            "sqlite_version": crab_ltx::rusqlite::version(),
            "payload_bytes": 4 << 20,
            "sql_workers": 2,
            "cold_root": {"cell": cold.root.cell, "incarnation": cold.root.incarnation,
                          "digest": cold.root.digest, "txid": cold.root.position.txid,
                          "checksum": cold.root.position.checksum, "commit_sequence": cold.root.commit_sequence},
            "payload_digest": expected_digest,
            "injected_get_delay_ms": 500,
            "same_worker_baseline": same_baseline,
            "other_worker_baseline": other_baseline,
            "same_worker_during_hydration": same_wait,
            "other_worker_during_hydration": other_wait,
            "hydration_and_queries_us": hydration_and_queries.as_micros(),
            "origin_requests_during_hydration": hydration_requests,
            "origin_bytes_during_hydration": hydration_bytes,
            "demand_samples": demand_samples,
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_hydration_releases_fetch_bytes_without_installing_pages() {
    let backend = Arc::new(ThrottledStore::new(
        InMemory::new(),
        ThrottleConfig::default(),
    ));
    let armed = Arc::new(AtomicBool::new(false));
    let started = Arc::new(tokio::sync::Notify::new());
    let watching = armed.clone();
    let notify = started.clone();
    let store = Store::new(backend.clone()).with_read_request_observer(Arc::new(move |_| {
        if watching.load(Ordering::Acquire) {
            notify.notify_one();
        }
    }));
    let cold = sparse_activation(4, store, 4 << 20).await;
    let pool = SqlWorkerPool::new(1, 1).unwrap();
    pool.configure_retained_capacity(1 << 20).unwrap();
    pool.activate_restored(
        cold.cell,
        RestoredDatabase::Paged(Box::new(cold.database)),
        cold.destination,
        cold.incarnation,
        1,
        cold.root,
        pool.reserve_activation().unwrap(),
    )
    .await
    .unwrap();
    let before = pool.hydration(cold.cell).await.unwrap();
    backend.config_mut(|config| config.wait_get_per_call = Duration::from_secs(3));
    armed.store(true, Ordering::Release);
    let hydration = {
        let pool = pool.clone();
        tokio::spawn(async move {
            pool.hydrate(cold.cell, 64, Instant::now() + Duration::from_secs(10))
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    let used = pool.resource_ledger().snapshot().unwrap().used;
    assert!(used.retained_bytes() > 0);
    assert_eq!(
        used.worker_jobs(),
        0,
        "fetch must release SQL worker admission"
    );
    hydration.abort();
    assert!(hydration.await.unwrap_err().is_cancelled());
    assert_eq!(
        pool.resource_ledger()
            .snapshot()
            .unwrap()
            .used
            .retained_bytes(),
        0
    );
    assert_eq!(pool.hydration(cold.cell).await.unwrap(), before);
    let deadline = pool
        .hydrate(cold.cell, 64, Instant::now() + Duration::from_millis(20))
        .await
        .unwrap();
    assert!(matches!(deadline, HydrationStep::Deferred(_)));
    assert_eq!(pool.hydration(cold.cell).await.unwrap(), before);
    assert_eq!(
        pool.resource_ledger()
            .snapshot()
            .unwrap()
            .used
            .retained_bytes(),
        0
    );
    let foreground_bytes = pool
        .resource_ledger()
        .try_reserve(ResourceCost::zero().with_retained_bytes(1 << 20))
        .unwrap();
    let pressure = pool
        .hydrate(cold.cell, 64, Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    assert!(matches!(pressure, HydrationStep::Deferred(_)));
    assert_eq!(pool.hydration(cold.cell).await.unwrap(), before);
    drop(foreground_bytes);
    armed.store(false, Ordering::Release);
    backend.config_mut(|config| config.wait_get_per_call = Duration::ZERO);
    let HydrationStep::Progress(Some(after)) = pool
        .hydrate(cold.cell, 64, Instant::now() + Duration::from_secs(10))
        .await
        .unwrap()
    else {
        panic!("hydration unexpectedly deferred")
    };
    assert!(after.resolved > before.unwrap().resolved);
    pool.deactivate(cold.cell).await.unwrap();
    pool.shutdown().await.unwrap();
}
