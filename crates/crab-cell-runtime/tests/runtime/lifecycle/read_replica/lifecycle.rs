//! In-flight snapshot work at the node drain and schema boundaries.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_refreshes_coalesce_after_the_first_authority_read() {
    let store = Arc::new(PausingStore::new(Arc::new(InMemory::new())));
    let fixture = fixture_with_limits_and_store(
        b"coalesced-reader-refresh",
        Limits::default(),
        Store::new(store.clone()),
    );
    let owner = SessionId::from_bytes([44; 16]);
    let runtime = CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 8 << 20, owner).unwrap();
    let handle = bootstrap_on(&runtime, &fixture, owner).await;
    let registry = compiled_reader_registry();
    let directory = owner_directory(&fixture, owner, &registry).await;
    let reads = Arc::new(ControlReads::default());
    let authority = CellAuthority::with_telemetry(
        fixture.layout.clone(),
        crab_cell_runtime::fleet::telemetry::CellTelemetryHandle::from_sink(reads.clone()),
    );
    let reader_runtime = CellRuntime::new(
        SqlWorkerPool::new(2, 512).unwrap(),
        8 << 20,
        SessionId::from_bytes([45; 16]),
    )
    .unwrap();
    let original = fixture._directory.path().join("original.sqlite");
    let reader = CellReadReplica::open(
        reader_runtime.clone(),
        registry,
        authority,
        directory,
        fixture.replica.clone(),
        fixture.target.clone(),
        &original,
    )
    .await
    .unwrap();
    let committed = handle
        .execute(
            crate::support::fixtures::mutation_identity(46),
            Digest::from_bytes([47; 32]),
            now_ms(),
            64,
            64,
            |transaction| {
                transaction.execute("UPDATE counter SET value = 1", [])?;
                Ok(HandlerOutcome::Success(Vec::new()))
            },
        )
        .await
        .unwrap();
    store.arm_gets();
    let first_path = fixture._directory.path().join("first.sqlite");
    let second_path = fixture._directory.path().join("second.sqlite");
    let first_reader = reader.clone();
    let destination = first_path.clone();
    let first = tokio::spawn(async move { first_reader.refresh(&destination).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        store.wait_until_get_blocked(),
    )
    .await
    .unwrap();
    let before = reads.0.load(Ordering::Relaxed);
    let (first, second, pending, reads_while_blocked) = {
        let second = reader.refresh(&second_path);
        tokio::pin!(second);
        let pending = futures_util::poll!(second.as_mut()).is_pending();
        let reads_while_blocked = reads.0.load(Ordering::Relaxed) - before;
        // Release the injected I/O stall before assertions so a failed
        // coalescing invariant cannot leave a runtime task parked forever.
        store.release_gets();
        let (first, second) = tokio::join!(first, second);
        (
            first.unwrap().unwrap(),
            second.unwrap(),
            pending,
            reads_while_blocked,
        )
    };
    assert!(pending);
    assert_eq!(reads_while_blocked, 0);
    assert_eq!(first, second);
    assert_eq!(first.commit_sequence, committed.commit_sequence());
    assert!(!original.exists());
    assert!(first_path.exists());
    assert!(!second_path.exists());
    assert_eq!(reader_runtime.stats().resident_bytes(), 12 << 20);
    assert_eq!(
        reader.query::<ReadCounter>(None, 0).await.unwrap().output,
        1
    );
    reader.close();
    drop(reader);
    assert!(!first_path.exists());
    assert_eq!(reader_runtime.stats().resident_bytes(), 0);
    reader_runtime.shutdown().await.unwrap();
    runtime.shutdown().await.unwrap();
}

pub(super) fn migration_query_barriers() -> &'static (Barrier, Barrier) {
    static BARRIERS: OnceLock<(Barrier, Barrier)> = OnceLock::new();
    BARRIERS.get_or_init(|| (Barrier::new(2), Barrier::new(2)))
}

#[tokio::test(flavor = "multi_thread")]
async fn schema_change_fences_an_inflight_old_snapshot_query() {
    exercise_schema_change(fixture_for(b"read-replica-migration")).await;
}

pub(super) async fn exercise_schema_change(fixture: Fixture) {
    let session = SessionId::from_bytes([34; 16]);
    let runtime = CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 8 << 20, session).unwrap();
    let handle = bootstrap_on(&runtime, &fixture, session).await;
    let registry = compiled_reader_registry();
    let directory = owner_directory(&fixture, session, &registry).await;
    let authority = CellAuthority::new(fixture.layout.clone());
    let reader_runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 512).unwrap(),
        8 << 20,
        SessionId::from_bytes([35; 16]),
    )
    .unwrap();
    let old_path = fixture._directory.path().join("old-schema.sqlite");
    let reader = CellReadReplica::open(
        reader_runtime.clone(),
        Arc::clone(&registry),
        authority.clone(),
        directory.clone(),
        fixture.replica.clone(),
        fixture.target.clone(),
        &old_path,
    )
    .await
    .unwrap();
    let before = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let pending_reader = reader.clone();
    let pending = tokio::spawn(async move { pending_reader.query::<ReadCounter>(None, 98).await });
    tokio::task::spawn_blocking(|| migration_query_barriers().0.wait())
        .await
        .unwrap();
    let plan = registry
        .next_migration(NAMESPACE, handle.code(), handle.schema())
        .unwrap()
        .unwrap();
    let migrated = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        handle.migrate(plan, now_ms()),
    )
    .await;
    tokio::task::spawn_blocking(|| migration_query_barriers().1.wait())
        .await
        .unwrap();
    let migrated = migrated.unwrap().unwrap();
    assert!(matches!(
        pending.await.unwrap(),
        Err(crab_cell_runtime::Error::Fenced)
    ));
    let after = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.value().schema, 2);
    assert_eq!(after.value().code, registry.module_code(MODULE).unwrap());
    assert_eq!(after.value().owner, before.value().owner);
    assert_eq!(after.value().epoch, before.value().epoch);
    assert!(
        after.value().root.as_ref().unwrap().commit_sequence
            > before.value().root.as_ref().unwrap().commit_sequence
    );
    let rejected_path = fixture._directory.path().join("stale-refresh.sqlite");
    assert!(matches!(
        reader.refresh(&rejected_path).await,
        Err(crab_cell_runtime::Error::Fenced)
    ));
    assert!(!rejected_path.exists());
    let new_path = fixture._directory.path().join("new-schema.sqlite");
    let replacement = CellReadReplica::open(
        reader_runtime.clone(),
        registry,
        authority,
        directory,
        fixture.replica,
        fixture.target,
        &new_path,
    )
    .await
    .unwrap();
    assert_eq!(
        replacement
            .query::<ReadCounter>(None, 0)
            .await
            .unwrap()
            .output,
        10
    );
    reader.close();
    replacement.close();
    drop(reader);
    drop(replacement);
    assert!(!old_path.exists());
    assert!(!new_path.exists());
    migrated.handle.drain().await.unwrap();
    reader_runtime.shutdown().await.unwrap();
    runtime.shutdown().await.unwrap();
}

pub(super) async fn drain_waits_for_replica_sql(
    fixture: &Fixture,
    registry: &Arc<crab_cell_runtime::registry::Registry>,
    directory: &NodeDirectory,
) {
    for cancel in [false, true] {
        drain_query(fixture, registry, directory, cancel).await;
    }
}

async fn drain_query(
    fixture: &Fixture,
    registry: &Arc<crab_cell_runtime::registry::Registry>,
    directory: &NodeDirectory,
    cancel: bool,
) {
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 256).unwrap(),
        8 << 20,
        SessionId::from_bytes([33; 16]),
    )
    .unwrap();
    let path = fixture._directory.path().join("draining-reader.sqlite");
    let reader = CellReadReplica::open(
        runtime.clone(),
        Arc::clone(registry),
        CellAuthority::new(fixture.layout.clone()),
        directory.clone(),
        fixture.replica.clone(),
        fixture.target.clone(),
        &path,
    )
    .await
    .unwrap();
    let pending_reader = reader.clone();
    let mut pending =
        tokio::spawn(async move { pending_reader.query::<ReadCounter>(None, 99).await });
    tokio::task::spawn_blocking(|| query_barriers().0.wait())
        .await
        .unwrap();
    reader.close();
    drop(reader);
    let cancelled = if cancel {
        pending.abort();
        Some((&mut pending).await.unwrap_err())
    } else {
        None
    };
    let retained_while_running = runtime.stats().resident_bytes();
    let descriptors_while_running = runtime.stats().file_descriptors();
    let draining = runtime.clone();
    let mut shutdown = tokio::spawn(async move { draining.shutdown().await });
    let early = tokio::time::timeout(std::time::Duration::from_millis(50), &mut shutdown)
        .await
        .ok();
    let completed_early = early.is_some();
    let jobs_during_drain = runtime.stats().worker_jobs();
    // Always release the SQL callback before asserting the drain result, so a
    // regression cannot strand a blocking worker and hang the test process.
    tokio::task::spawn_blocking(|| query_barriers().1.wait())
        .await
        .unwrap();
    let output = match cancelled {
        Some(error) => {
            assert!(error.is_cancelled());
            None
        }
        None => Some(pending.await.unwrap()),
    };
    match early {
        Some(result) => result.unwrap().unwrap(),
        None => shutdown.await.unwrap().unwrap(),
    }
    assert!(matches!(
        output,
        None | Some(Err(crab_cell_runtime::Error::Fenced))
    ));
    assert_eq!(retained_while_running, 12 << 20);
    assert_eq!(descriptors_while_running, 4);
    assert_eq!(jobs_during_drain, 1);
    assert_eq!(runtime.stats().worker_jobs(), 0);
    assert_eq!(runtime.stats().resident_bytes(), 0);
    assert!(!path.exists());
    assert!(
        !completed_early,
        "node drain returned while replica SQL was running"
    );
}
