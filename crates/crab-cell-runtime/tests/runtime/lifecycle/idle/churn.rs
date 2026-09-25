//! Pressure observation, churn eviction, and primitive inventory gates.

use super::*;

#[tokio::test]
async fn actor_pressure_observation_uses_hysteresis_and_shared_eviction_path() {
    let session = SessionId::from_bytes([41; 16]);
    let runtime = CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 1_024, session).unwrap();
    // The actor samples its own ledger on the wall clock, so keep both
    // observations ahead of any tick that could run while this test is
    // scheduled.
    let base_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
        + 5_000;
    let high = PressureSample {
        at_ms: base_ms,
        memory_used_permille: 900,
        disk_used_permille: 100,
        jobs_used_permille: 100,
        stale: false,
    };
    assert_eq!(
        runtime.observe_pressure(high).await.unwrap(),
        PressureState::Normal
    );
    assert_eq!(
        runtime
            .observe_pressure(PressureSample {
                at_ms: base_ms + 1_000,
                ..high
            })
            .await
            .unwrap(),
        PressureState::Shedding
    );
    assert_eq!(runtime.evict_idle(1).await.unwrap(), 0);
    runtime.shutdown().await.unwrap();
}

/// Reserves retained bytes until the memory ledger reaches `target_permille`.
fn press_memory_ledger(
    runtime: &CellRuntime,
    target_permille: u64,
) -> crab_cell_runtime::cell::actor::NodeByteReservation {
    let stats = runtime.stats();
    let limit =
        u64::try_from(stats.resident_capacity_bytes() + stats.retained_capacity_bytes()).unwrap();
    let used = u64::try_from(stats.resident_bytes() + stats.retained_bytes()).unwrap();
    let target = limit * target_permille / 1_000;
    assert!(target > used, "the reservation already exceeds the target");
    runtime
        .try_reserve_node_bytes(usize::try_from(target - used).unwrap())
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn node_ledger_pressure_sheds_a_settled_cell_without_an_external_sample() {
    let fixture = fixture_for(b"ledger-pressure-shed");
    let session = SessionId::from_bytes([121; 16]);
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 8 * 1024 * 1024, session).unwrap();
    let handle = bootstrap_on(&runtime, &fixture, session).await;
    // Hold the reservation across the samples the classifier needs to see.
    let reservation = press_memory_ledger(&runtime, 850);

    let authority = CellAuthority::new(fixture.layout.clone());
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            if runtime.unreleased_cell_count().await.unwrap() == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("sustained ledger pressure must shed the settled Cell");

    let idle = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(idle.value().state, ControlState::Idle);
    assert!(idle.value().owner.is_none());
    assert_eq!(runtime.stats().active_cells(), 0);

    drop(reservation);
    drop(handle);
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn node_ledger_below_the_soft_reserve_keeps_the_settled_cell() {
    let fixture = fixture_for(b"ledger-pressure-hold");
    let session = SessionId::from_bytes([122; 16]);
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 8 * 1024 * 1024, session).unwrap();
    let handle = bootstrap_on(&runtime, &fixture, session).await;
    let reservation = press_memory_ledger(&runtime, 500);

    // Longer than the classifier's dwell and the actor's sample interval.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_eq!(runtime.unreleased_cell_count().await.unwrap(), 1);
    assert_eq!(runtime.stats().active_cells(), 1);

    drop(reservation);
    handle.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn churn_evicts_idle_cells_and_restores_exact_roots() {
    let first = fixture_for(b"churn-first");
    let second = fixture_for(b"churn-second");
    let third = fixture_for(b"churn-third");
    let session = SessionId::from_bytes([74; 16]);
    let runtime = CellRuntime::new_with_replica_host(
        SqlWorkerPool::new(1, 2).unwrap(),
        16 * 1024 * 1024,
        session,
        ReplicaHost::default()
            .with_local_disk_budget(DiskBudget::new(4 * Limits::default().max_capture_bytes)),
    )
    .unwrap();

    let first_handle = bootstrap_on(&runtime, &first, session).await;
    let second_handle = bootstrap_on(&runtime, &second, session).await;
    assert!(runtime.local_disk_budget().used() > 0);

    let authority_first = CellAuthority::new(first.layout.clone());
    let authority_second = CellAuthority::new(second.layout.clone());
    let root_first = authority_first
        .load(first.target.cell_id())
        .await
        .unwrap()
        .unwrap()
        .value()
        .ltx_root()
        .unwrap();
    let root_second = authority_second
        .load(second.target.cell_id())
        .await
        .unwrap()
        .unwrap()
        .value()
        .ltx_root()
        .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if runtime.evict_idle(1).await.unwrap() == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();

    let (evicted_fixture, evicted_authority, evicted_root, evicted_idle) =
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let first_control = authority_first
                    .load(first.target.cell_id())
                    .await
                    .unwrap()
                    .unwrap();
                if first_control.value().state == ControlState::Idle {
                    break (&first, &authority_first, root_first, first_control);
                }
                let second_control = authority_second
                    .load(second.target.cell_id())
                    .await
                    .unwrap()
                    .unwrap();
                if second_control.value().state == ControlState::Idle {
                    break (&second, &authority_second, root_second, second_control);
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
    assert_eq!(evicted_idle.value().ltx_root(), Some(evicted_root));
    assert_eq!(runtime.stats().active_cells(), 1);

    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(
        evicted_fixture.layout.clone(),
        evicted_fixture.target.tenant(),
    );
    let proof = catalog
        .lookup(evicted_fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let restored = runtime
        .acquire_idle_restored(
            proof,
            evicted_fixture.replica.clone(),
            evicted_authority.clone(),
            evicted_idle,
            evicted_fixture
                ._directory
                .path()
                .join("churn-restored.sqlite"),
            Owner {
                session,
                endpoint: "https://churn-successor.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        restored
            .query(64, 64, |connection| {
                let value = connection
                    .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
                Ok(value.to_be_bytes().to_vec())
            })
            .await
            .unwrap(),
        0_i64.to_be_bytes()
    );
    restored.drain().await.unwrap();

    let third_handle = bootstrap_on(&runtime, &third, session).await;
    assert_eq!(runtime.stats().active_cells(), 2);
    third_handle.drain().await.unwrap();
    if evicted_fixture.target.cell_id() == first.target.cell_id() {
        second_handle.drain().await.unwrap();
    } else {
        first_handle.drain().await.unwrap();
    }
    assert_eq!(runtime.stats().active_cells(), 0);
    assert_eq!(runtime.local_disk_budget().used(), 0);
    runtime.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread")]
async fn mixed_primitive_inventory_blocks_churn_until_drain_and_restores_root() {
    mixed_primitive_inventory_churn(Store::new(Arc::new(InMemory::new())), Path::from("runtime"))
        .await;
}
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an isolated pre-created RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_mixed_primitive_inventory_churn_preserves_exact_roots() {
    let required = |name| std::env::var(name).unwrap_or_else(|_| panic!("missing {name}"));
    let store = build_explicit_store(
        &required("CRAB_CELL_TEST_BUCKET"),
        ObjectStoreCredentials::Aws {
            access_key_id: required("AWS_ACCESS_KEY_ID"),
            secret_access_key: required("AWS_SECRET_ACCESS_KEY"),
            session_token: None,
            region: "us-east-1".into(),
        },
        Some(&required("CRAB_CELL_TEST_ENDPOINT")),
        true,
    )
    .unwrap();
    let prefix = Path::from(format!(
        "{}/mixed-primitive-churn",
        required("CRAB_CELL_TEST_PREFIX")
    ));
    mixed_primitive_inventory_churn(store, prefix).await;
}
async fn mixed_primitive_inventory_churn(store: Store, prefix: Path) {
    let queue = fixture_with_limits_and_store_at_prefix(
        b"mixed-queue",
        Limits::default(),
        store.clone(),
        prefix.clone(),
    );
    let workflow = fixture_with_limits_and_store_at_prefix(
        b"mixed-workflow",
        Limits::default(),
        store.clone(),
        prefix.clone(),
    );
    let third =
        fixture_with_limits_and_store_at_prefix(b"mixed-third", Limits::default(), store, prefix);
    let session = SessionId::from_bytes([80; 16]);
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 2).unwrap(), 16 * 1024 * 1024, session).unwrap();

    let queue_handle = bootstrap_role_on(
        &runtime,
        &queue,
        session,
        CatalogRole::Queue,
        |transaction| {
            install_queue_schema(transaction)?;
            transaction.execute(
                "INSERT INTO queue_messages VALUES (zeroblob(16), X'01', 0, 0, 1, 1000, NULL, NULL, NULL, NULL)",
                [],
            )?;
            Ok(())
        },
    )
    .await;
    let workflow_handle = bootstrap_role_on(
        &runtime,
        &workflow,
        session,
        CatalogRole::Workflow,
        |transaction| {
            install_workflow_schema(transaction)?;
            transaction.execute(
                "INSERT INTO workflow_runs VALUES (X'02', zeroblob(16), zeroblob(32), 0, X'', 0, NULL, NULL)",
                [],
            )?;
            Ok(())
        },
    )
    .await;

    wait_for_persisted_work(
        &queue_handle,
        CatalogRole::Queue,
        "maintenance release is blocked by retained Queue messages",
    )
    .await;
    wait_for_persisted_work(
        &workflow_handle,
        CatalogRole::Workflow,
        "maintenance release is blocked by retained Workflow runs",
    )
    .await;

    assert_eq!(runtime.evict_idle(2).await.unwrap(), 0);
    assert_eq!(runtime.stats().active_cells(), 2);

    let queue_authority = CellAuthority::new(queue.layout.clone());
    let queue_root = queue_authority
        .load(queue.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let queue_root = queue_root.value().ltx_root().unwrap();

    queue_handle.drain().await.unwrap();
    workflow_handle.drain().await.unwrap();
    assert_eq!(runtime.stats().active_cells(), 0);
    let queue_idle = queue_authority
        .load(queue.target.cell_id())
        .await
        .unwrap()
        .unwrap();

    let queue_proof = crab_cell_runtime::cell::catalog::CellCatalog::new(
        queue.layout.clone(),
        queue.target.tenant(),
    )
    .lookup(queue.target.cell_id())
    .await
    .unwrap()
    .unwrap();
    let restored = runtime
        .acquire_idle_restored(
            queue_proof,
            queue.replica.clone(),
            queue_authority,
            queue_idle,
            queue._directory.path().join("mixed-queue-restored.sqlite"),
            Owner {
                session,
                endpoint: "https://mixed-successor.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        restored
            .query(64, 64, |connection| {
                let count =
                    connection.query_row("SELECT count(*) FROM queue_messages", [], |row| {
                        row.get::<_, i64>(0)
                    })?;
                Ok(count.to_be_bytes().to_vec())
            })
            .await
            .unwrap(),
        1_i64.to_be_bytes()
    );
    assert_eq!(
        crab_cell_runtime::control::authority::CellAuthority::new(queue.layout.clone())
            .load(queue.target.cell_id())
            .await
            .unwrap()
            .unwrap()
            .value()
            .ltx_root(),
        Some(queue_root)
    );
    restored.drain().await.unwrap();

    let third_handle = bootstrap_on(&runtime, &third, session).await;
    assert_eq!(runtime.stats().active_cells(), 1);
    third_handle.drain().await.unwrap();
    assert_eq!(runtime.stats().active_cells(), 0);
    runtime.shutdown().await.unwrap();
}
