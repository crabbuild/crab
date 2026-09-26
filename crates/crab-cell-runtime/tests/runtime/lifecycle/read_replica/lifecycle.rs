//! In-flight snapshot work at the node drain and schema boundaries.

use super::*;

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
