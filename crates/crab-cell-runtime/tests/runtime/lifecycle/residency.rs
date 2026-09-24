//! Resident routes, hydration, and owner progress.

use super::*;

#[tokio::test]
async fn runtime_stats_follow_active_cell_lifecycle() {
    let fixture = fixture_for(b"runtime-stats");
    let (runtime, handle, _pool) = activate_runtime(&fixture, 2 * 1024 * 1024).await;

    assert_eq!(runtime.stats().active_cells(), 1);
    assert_eq!(
        runtime.stats().resident_bytes(),
        crab_cell_runtime::cell::actor::ACTIVE_CELL_NATIVE_BYTES as usize
    );
    assert_eq!(
        runtime.stats().resident_capacity_bytes(),
        10 * crab_cell_runtime::cell::actor::ACTIVE_CELL_NATIVE_BYTES as usize
    );
    assert_eq!(
        runtime.stats().file_descriptors(),
        ACTIVE_CELL_FILE_DESCRIPTORS
    );
    assert_eq!(
        runtime.stats().file_descriptor_capacity(),
        10 * ACTIVE_CELL_FILE_DESCRIPTORS
    );
    handle.drain().await.unwrap();
    assert_eq!(runtime.stats().active_cells(), 0);
    assert_eq!(runtime.stats().resident_bytes(), 0);
    assert_eq!(runtime.stats().file_descriptors(), 0);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn resident_lookup_is_invalidated_before_drain_releases_the_cell() {
    let fixture = fixture_for(b"resident-drain-race");
    let (runtime, handle, _pool) = activate_runtime(&fixture, 2 * 1024 * 1024).await;

    assert!(
        runtime
            .resident_handle(&fixture.target, CatalogRole::Repository)
            .await
            .unwrap()
            .is_some()
    );

    handle.drain().await.unwrap();

    assert!(
        runtime
            .resident_handle(&fixture.target, CatalogRole::Repository)
            .await
            .unwrap()
            .is_none()
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn resident_route_reports_zero_origin_reads_and_latency_percentiles() {
    let origin_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed_reads = Arc::clone(&origin_reads);
    let store =
        Store::new(Arc::new(InMemory::new())).with_read_request_observer(Arc::new(move |_kind| {
            observed_reads.fetch_add(1, Ordering::AcqRel);
        }));
    let fixture =
        fixture_with_limits_and_store(b"resident-warm-qualification", Limits::default(), store);
    let (runtime, handle, _pool) = activate_runtime(&fixture, 2 * 1024 * 1024).await;
    origin_reads.store(0, Ordering::Release);

    let mut samples = Vec::with_capacity(64);
    for _ in 0..64 {
        let started = std::time::Instant::now();
        let resident = runtime
            .resident_handle(&fixture.target, CatalogRole::Repository)
            .await
            .unwrap()
            .expect("bootstrapped Cell must remain resident");
        let value = resident
            .query(64, 64, |connection| {
                let value = connection
                    .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
                Ok(value.to_be_bytes().to_vec())
            })
            .await
            .unwrap();
        assert_eq!(i64::from_be_bytes(value.try_into().unwrap()), 0);
        samples.push(started.elapsed());
    }

    samples.sort_unstable();
    let percentile = |percent: usize| {
        let index = ((samples.len() - 1) * percent).div_ceil(100);
        samples[index]
    };
    println!(
        "resident warm route: samples={} p50_us={} p95_us={} p99_us={} max_us={}",
        samples.len(),
        percentile(50).as_micros(),
        percentile(95).as_micros(),
        percentile(99).as_micros(),
        samples.last().unwrap().as_micros()
    );
    assert_eq!(origin_reads.load(Ordering::Acquire), 0);

    handle.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn restored_sparse_route_promotes_before_zero_origin_reads() {
    let origin_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed_reads = Arc::clone(&origin_reads);
    let store =
        Store::new(Arc::new(InMemory::new())).with_read_request_observer(Arc::new(move |_kind| {
            observed_reads.fetch_add(1, Ordering::AcqRel);
        }));
    let fixture = fixture_with_limits_and_store(
        b"resident-warm-restart-qualification",
        Limits::default(),
        store,
    );
    let session = SessionId::from_bytes([110; 16]);
    let first_runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024, session).unwrap();
    let handle = bootstrap_on(&first_runtime, &fixture, session).await;
    handle.drain().await.unwrap();
    first_runtime.shutdown().await.unwrap();

    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(
        fixture.layout.clone(),
        fixture.target.tenant(),
    );
    let proof = catalog
        .lookup(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let authority = CellAuthority::new(fixture.layout.clone());
    let idle = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let successor = SessionId::from_bytes([111; 16]);
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        16 * 1024 * 1024,
        successor,
    )
    .unwrap();
    let restored = runtime
        .acquire_idle_restored(
            proof,
            fixture.replica.clone(),
            authority,
            idle,
            fixture._directory.path().join("warm-restart.sqlite"),
            Owner {
                session: successor,
                endpoint: "https://warm-restart.internal:8081".into(),
            },
        )
        .await
        .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if runtime
                .resident_handle(&fixture.target, CatalogRole::Repository)
                .await
                .unwrap()
                .is_some()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    origin_reads.store(0, Ordering::Release);

    let resident = runtime
        .resident_handle(&fixture.target, CatalogRole::Repository)
        .await
        .unwrap()
        .expect("verified sparse restore must promote to resident");
    let value = resident
        .query(64, 64, |connection| {
            let value = connection
                .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
            Ok(value.to_be_bytes().to_vec())
        })
        .await
        .unwrap();
    assert_eq!(i64::from_be_bytes(value.try_into().unwrap()), 0);
    assert_eq!(origin_reads.load(Ordering::Acquire), 0);

    restored.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_releases_a_hydration_reservation_after_an_origin_wait() {
    let pausing = Arc::new(PausingStore::new(Arc::new(InMemory::new())));
    let fixture = fixture_with_limits_and_store(
        b"hydration-shutdown-cancellation",
        Limits::default(),
        Store::new(pausing.clone()),
    );
    let first_session = SessionId::from_bytes([113; 16]);
    let first_runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        64 * 1024 * 1024,
        first_session,
    )
    .unwrap();
    let handle = bootstrap_role_on(
        &first_runtime,
        &fixture,
        first_session,
        CatalogRole::Repository,
        |transaction| {
            transaction.execute_batch(
                "CREATE TABLE payload(value BLOB NOT NULL);\
                 WITH RECURSIVE numbers(value) AS (\
                   SELECT 1 UNION ALL SELECT value + 1 FROM numbers WHERE value < 512\
                 )\
                 INSERT INTO payload(value) SELECT zeroblob(16384) FROM numbers;",
            )?;
            Ok(())
        },
    )
    .await;
    handle.drain().await.unwrap();
    first_runtime.shutdown().await.unwrap();

    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(
        fixture.layout.clone(),
        fixture.target.tenant(),
    );
    let proof = catalog
        .lookup(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let authority = CellAuthority::new(fixture.layout.clone());
    let observed = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let successor = SessionId::from_bytes([114; 16]);
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        64 * 1024 * 1024,
        successor,
    )
    .unwrap();
    let restored = runtime
        .acquire_idle_restored(
            proof,
            fixture.replica.clone(),
            authority,
            observed,
            fixture._directory.path().join("hydration-shutdown.sqlite"),
            Owner {
                session: successor,
                endpoint: "https://hydration-shutdown.internal:8081".into(),
            },
        )
        .await
        .unwrap();

    pausing.arm_gets();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        pausing.wait_until_get_blocked(),
    )
    .await
    .unwrap();
    assert_eq!(runtime.stats().hydration_jobs(), 1);

    let shutdown_runtime = runtime.clone();
    let shutdown = tokio::spawn(async move { shutdown_runtime.shutdown().await });
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    pausing.release_gets();
    shutdown.await.unwrap().unwrap();
    assert_eq!(runtime.stats().hydration_jobs(), 0);
    drop(restored);
}

#[tokio::test]
async fn retained_request_outcome_moves_with_exact_root() {
    let fixture = fixture_for(b"eviction-persisted-work");
    let session = SessionId::from_bytes([78; 16]);
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 8 * 1024 * 1024, session).unwrap();
    let handle = bootstrap_on(&runtime, &fixture, session).await;
    assert!(matches!(
        handle
            .execute(
                mutation_identity_window(79, 10, 10_000),
                Digest::from_bytes([79; 32]),
                20,
                64,
                64,
                |transaction| {
                    transaction.execute("UPDATE counter SET value = value + 1", [])?;
                    Ok(HandlerOutcome::Success(Vec::new()))
                },
            )
            .await
            .unwrap(),
        StoredOutcome::Success {
            commit_sequence: 1,
            ..
        }
    ));

    let generation = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some((_, generation, _, _)) =
                runtime.idle_transfer_candidates().await.unwrap().first()
            {
                break *generation;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    runtime
        .release_idle_cell(fixture.target.cell_id(), session, generation)
        .await
        .unwrap();
    assert_eq!(runtime.stats().active_cells(), 0);
    let authority = CellAuthority::new(fixture.layout.clone());
    let idle = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(idle.value().state, ControlState::Idle);
    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(
        fixture.layout.clone(),
        fixture.target.tenant(),
    );
    let proof = catalog
        .lookup(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let successor_session = SessionId::from_bytes([95; 16]);
    let successor_runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        8 * 1024 * 1024,
        successor_session,
    )
    .unwrap();
    let successor = successor_runtime
        .acquire_idle_restored(
            proof,
            fixture.replica.clone(),
            authority,
            idle,
            fixture._directory.path().join("outcome-successor.sqlite"),
            Owner {
                session: successor_session,
                endpoint: "https://outcome-successor.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        successor
            .resolve(
                mutation_identity_window(79, 10, 10_000),
                Digest::from_bytes([79; 32]),
                20,
                64
            )
            .await
            .unwrap(),
        Resolution::Committed(StoredOutcome::Success {
            commit_sequence: 1,
            ..
        })
    ));
    successor.drain().await.unwrap();
    successor_runtime.shutdown().await.unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn idle_owner_progress_is_renewed_without_a_per_cell_task() {
    let fixture = fixture();
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    let authority = CellAuthority::new(fixture.layout.clone());
    let initial = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let initial_progress = initial.value().progress;
    let initial_root = initial.value().root.clone();

    let renewed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let current = authority
                .load(fixture.target.cell_id())
                .await
                .unwrap()
                .unwrap();
            if current.value().progress > initial_progress {
                break current;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();

    assert_eq!(renewed.value().root, initial_root);
    assert_eq!(renewed.value().owner, initial.value().owner);
    assert_eq!(renewed.value().revision, initial.value().revision + 1);
    handle.drain().await.unwrap();
}
