//! Exact idle release generation, inventory, and drain rules.

use super::*;

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
            mutation_identity_window(97, 10, 10_000),
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
