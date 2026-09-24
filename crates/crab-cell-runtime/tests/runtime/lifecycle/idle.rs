//! Idle release, eviction, churn, and capacity release.

use super::*;

#[tokio::test]
async fn actor_pressure_observation_uses_hysteresis_and_shared_eviction_path() {
    let session = SessionId::from_bytes([41; 16]);
    let runtime = CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 1_024, session).unwrap();
    let high = PressureSample {
        at_ms: 0,
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
                at_ms: 1_000,
                ..high
            })
            .await
            .unwrap(),
        PressureState::Shedding
    );
    assert_eq!(runtime.evict_idle(1).await.unwrap(), 0);
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_idle_receiver_does_not_leave_authority_owned() {
    let fixture = fixture_for(b"receiver-activation-failure");
    let session = SessionId::from_bytes([112; 16]);
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
    let expected_root = idle.value().root.clone();
    let successor = SessionId::from_bytes([113; 16]);
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        16 * 1024 * 1024,
        successor,
    )
    .unwrap();
    let missing_parent = fixture
        ._directory
        .path()
        .join("receiver-parent-does-not-exist")
        .join("receiver.sqlite");
    assert!(
        runtime
            .acquire_idle_restored(
                proof,
                fixture.replica.clone(),
                authority.clone(),
                idle,
                missing_parent,
                Owner {
                    session: successor,
                    endpoint: "https://receiver-failure.internal:8081".into(),
                },
            )
            .await
            .is_err()
    );

    let current = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.value().state, ControlState::Idle);
    assert!(current.value().owner.is_none());
    assert_eq!(current.value().root, expected_root);
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
        ReplicaHost::default().with_local_disk_budget(DiskBudget::new(64 * 1024 * 1024)),
    )
    .unwrap();

    let first_handle = bootstrap_on(&runtime, &first, session).await;
    let second_handle = bootstrap_on(&runtime, &second, session).await;

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
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn exact_idle_release_checks_generation_and_confirms_authority_release() {
    let fixture = fixture_for(b"exact-idle-release");
    let session = SessionId::from_bytes([91; 16]);
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 8 * 1024 * 1024, session).unwrap();
    let _handle = bootstrap_on(&runtime, &fixture, session).await;
    let candidates = runtime.idle_transfer_candidates().await.unwrap();
    assert_eq!(candidates.len(), 1);
    let (cell, generation, _, role) = candidates[0];
    assert_eq!(role, CatalogRole::Repository);
    assert!(
        runtime
            .release_idle_cell(cell, SessionId::from_bytes([92; 16]), generation)
            .await
            .is_err()
    );
    assert!(
        runtime
            .release_idle_cell(cell, session, generation + 1)
            .await
            .is_err()
    );
    runtime
        .release_idle_cell(cell, session, generation)
        .await
        .unwrap();
    assert_eq!(runtime.stats().active_cells(), 0);
    let idle = CellAuthority::new(fixture.layout.clone())
        .load(cell)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(idle.value().state, ControlState::Idle);
    assert!(idle.value().owner.is_none());
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn exact_idle_release_blocks_new_work_while_inventory_is_pending() {
    let target_fixture = fixture_for(b"exact-idle-preflight");
    let blocker_fixture = fixture_for(b"exact-idle-preflight-blocker");
    let session = SessionId::from_bytes([96; 16]);
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 2).unwrap(), 8 * 1024 * 1024, session).unwrap();
    let target = bootstrap_on(&runtime, &target_fixture, session).await;
    let blocker = bootstrap_on(&runtime, &blocker_fixture, session).await;
    let candidates = runtime.idle_transfer_candidates().await.unwrap();
    let (cell, generation, _, _) = candidates
        .iter()
        .copied()
        .find(|(cell, _, _, _)| *cell == target_fixture.target.cell_id())
        .unwrap();

    let (started, started_signal) = tokio::sync::oneshot::channel();
    let blocker_task = tokio::spawn(async move {
        blocker
            .query(1, 1, move |_| {
                let _ = started.send(());
                std::thread::sleep(std::time::Duration::from_millis(250));
                Ok(Vec::new())
            })
            .await
    });
    started_signal.await.unwrap();

    let release_runtime = runtime.clone();
    let release_task = tokio::spawn(async move {
        release_runtime
            .release_idle_cell(cell, session, generation)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let still_candidate = runtime
                .idle_transfer_candidates()
                .await
                .unwrap()
                .into_iter()
                .any(|(candidate, _, _, _)| candidate == cell);
            if !still_candidate {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();

    let result = target
        .execute(
            identity(97),
            Digest::from_bytes([97; 32]),
            20,
            64,
            64,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(Vec::new()))
            },
        )
        .await;
    assert!(matches!(
        result,
        Err(crab_cell_runtime::Error::CellDraining)
    ));

    assert!(blocker_task.await.unwrap().is_ok());
    release_task.await.unwrap().unwrap();
    assert_eq!(runtime.stats().active_cells(), 1);
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn exact_idle_release_drains_work_admitted_before_transfer() {
    let fixture = fixture_for(b"exact-idle-admitted-before-transfer");
    let session = SessionId::from_bytes([99; 16]);
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 8 * 1024 * 1024, session).unwrap();
    let handle = bootstrap_on(&runtime, &fixture, session).await;
    let (_, generation, _, _) = runtime.idle_transfer_candidates().await.unwrap()[0];

    let (started, started_signal) = tokio::sync::oneshot::channel();
    let query_handle = handle.clone();
    let query_task = tokio::spawn(async move {
        query_handle
            .query(1, 1, move |_| {
                let _ = started.send(());
                std::thread::sleep(std::time::Duration::from_millis(250));
                Ok(Vec::new())
            })
            .await
    });
    started_signal.await.unwrap();

    let release_runtime = runtime.clone();
    let release_task = tokio::spawn(async move {
        release_runtime
            .release_idle_cell(fixture.target.cell_id(), session, generation)
            .await
    });

    assert!(query_task.await.unwrap().is_ok());
    release_task.await.unwrap().unwrap();
    assert_eq!(runtime.stats().active_cells(), 0);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn exact_idle_release_refuses_persisted_work() {
    let fixture = fixture_for(b"exact-idle-blocked");
    let session = SessionId::from_bytes([93; 16]);
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 8 * 1024 * 1024, session).unwrap();
    let handle = bootstrap_role_on(
        &runtime,
        &fixture,
        session,
        CatalogRole::Repository,
        |transaction| {
            transaction.execute_batch(
                "CREATE TABLE counter(value INTEGER NOT NULL); INSERT INTO counter VALUES (0)",
            )?;
            transaction.execute(
                "INSERT INTO sys_effects(effect_id, destination, operation, state, attempt, due_at_ms, expires_at_ms, token, lease_until_ms, created_sequence, result) VALUES (?1, ?2, ?3, 0, 0, 20, 1000, NULL, NULL, 1, NULL)",
                crab_ltx::rusqlite::params![&[1_u8; 32], &[2_u8; 32], &[3_u8]],
            )?;
            Ok(())
        },
    )
    .await;
    let (_, generation, _, _) = runtime.idle_transfer_candidates().await.unwrap()[0];
    assert!(
        runtime
            .release_idle_cell(fixture.target.cell_id(), session, generation)
            .await
            .is_err()
    );
    assert_eq!(runtime.stats().active_cells(), 1);
    assert!(matches!(
        handle.drain().await,
        Err(crab_cell_runtime::Error::CellDraining)
    ));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn exact_idle_release_allows_settled_queue_rows() {
    let fixture = fixture_for(b"exact-idle-settled-queue");
    let session = SessionId::from_bytes([98; 16]);
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 8 * 1024 * 1024, session).unwrap();
    let _handle = bootstrap_role_on(
        &runtime,
        &fixture,
        session,
        CatalogRole::Queue,
        |transaction| {
            install_queue_schema(transaction)?;
            transaction.execute(
                "INSERT INTO queue_messages(message_id, payload, state, attempt, due_at_ms, expires_at_ms, token, lease_until_ms, result_code) VALUES (zeroblob(16), X'', 2, 0, 0, 1000, NULL, NULL, 0)",
                [],
            )?;
            transaction.execute(
                "INSERT INTO queue_dedup VALUES (zeroblob(16), zeroblob(32), zeroblob(16), 1000)",
                [],
            )?;
            Ok(())
        },
    )
    .await;
    let (_, generation, _, _) = runtime.idle_transfer_candidates().await.unwrap()[0];
    runtime
        .release_idle_cell(fixture.target.cell_id(), session, generation)
        .await
        .unwrap();
    assert_eq!(runtime.stats().active_cells(), 0);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn stop_acquiring_keeps_existing_cell_serving() {
    let first = fixture_for(b"scale-down-serving");
    let second = fixture_for(b"scale-down-new");
    let session = SessionId::from_bytes([96; 16]);
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 2).unwrap(), 8 * 1024 * 1024, session).unwrap();
    let handle = bootstrap_on(&runtime, &first, session).await;
    runtime.stop_acquiring().unwrap();
    assert!(!runtime.is_acquiring());
    assert_eq!(
        handle
            .query(64, 64, |connection| {
                let value = connection
                    .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
                Ok(value.to_be_bytes().to_vec())
            })
            .await
            .unwrap(),
        0_i64.to_be_bytes()
    );
    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(
        second.layout.clone(),
        second.target.tenant(),
    );
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &second.target,
                CatalogRole::Repository,
                Digest::from_bytes([5; 32]),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let authority = CellAuthority::new(second.layout.clone());
    let observed = authority
        .create_initial(
            &proof,
            IncarnationId::from_bytes([2; 16]),
            Owner {
                session,
                endpoint: "https://node.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let result = runtime
        .bootstrap(
            proof,
            second.replica.clone(),
            authority,
            observed,
            second._directory.path().join("blocked.sqlite"),
            |_| Ok(()),
        )
        .await;
    assert!(matches!(
        result,
        Err(crab_cell_runtime::Error::CellDraining)
    ));
    assert_eq!(runtime.unreleased_cell_count().await.unwrap(), 1);
    handle.drain().await.unwrap();
    assert_eq!(runtime.unreleased_cell_count().await.unwrap(), 0);
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

#[tokio::test]
async fn failed_bootstrap_keeps_control_unpublished_and_releases_cell_capacity() {
    let fixture = fixture();
    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(
        fixture.layout.clone(),
        fixture.target.tenant(),
    );
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &fixture.target,
                CatalogRole::Repository,
                Digest::from_bytes([5; 32]),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let session = SessionId::from_bytes([4; 16]);
    let authority = CellAuthority::new(fixture.layout.clone());
    let observed = authority
        .create_initial(
            &proof,
            IncarnationId::from_bytes([2; 16]),
            Owner {
                session,
                endpoint: "https://node.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 2 * 1024 * 1024, session).unwrap();
    assert!(matches!(
        runtime
            .bootstrap(
                proof.clone(),
                fixture.replica.clone(),
                authority.clone(),
                observed,
                fixture.database.clone(),
                |transaction| {
                    transaction.execute("CREATE TABLE should_rollback(value INTEGER)", [])?;
                    Err(crab_cell_runtime::Error::Command("migration rejected"))
                },
            )
            .await,
        Err(crab_cell_runtime::Error::Command("migration rejected"))
    ));
    let current = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.value().state, ControlState::Recovering);
    assert!(current.value().root.is_none());

    let handle = runtime
        .bootstrap(
            proof,
            fixture.replica,
            authority,
            current,
            fixture.database.with_file_name("replacement.sqlite"),
            |transaction| {
                transaction.execute_batch(
                    "CREATE TABLE counter(value INTEGER NOT NULL); INSERT INTO counter VALUES (0)",
                )?;
                Ok(())
            },
        )
        .await
        .unwrap();
    handle.drain().await.unwrap();
}

#[tokio::test]
async fn panicking_bootstrap_keeps_worker_alive_and_releases_cell_capacity() {
    let fixture = fixture_for(b"panicking-bootstrap");
    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(
        fixture.layout.clone(),
        fixture.target.tenant(),
    );
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &fixture.target,
                CatalogRole::Repository,
                Digest::from_bytes([5; 32]),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let session = SessionId::from_bytes([4; 16]);
    let authority = CellAuthority::new(fixture.layout.clone());
    let observed = authority
        .create_initial(
            &proof,
            IncarnationId::from_bytes([2; 16]),
            Owner {
                session,
                endpoint: "https://node.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 2 * 1024 * 1024, session).unwrap();

    assert!(matches!(
        runtime
            .bootstrap(
                proof.clone(),
                fixture.replica.clone(),
                authority.clone(),
                observed,
                fixture.database.clone(),
                |_| panic!("bootstrap initializer panic"),
            )
            .await,
        Err(crab_cell_runtime::Error::NativePanic)
    ));
    let current = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.value().state, ControlState::Recovering);
    assert!(current.value().root.is_none());

    let handle = runtime
        .bootstrap(
            proof,
            fixture.replica,
            authority,
            current,
            fixture.database.with_file_name("panic-replacement.sqlite"),
            |transaction| {
                transaction.execute_batch(
                    "CREATE TABLE counter(value INTEGER NOT NULL); INSERT INTO counter VALUES (0)",
                )?;
                Ok(())
            },
        )
        .await
        .unwrap();
    handle.drain().await.unwrap();
}
