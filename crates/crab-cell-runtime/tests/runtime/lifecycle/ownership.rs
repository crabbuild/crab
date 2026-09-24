//! Leases, fencing, takeover, and successor acquisition.

use super::*;

#[tokio::test]
async fn fleet_runtime_stays_fenced_until_one_live_node_lease_is_installed() {
    let session = SessionId::from_bytes([41; 16]);
    let runtime = CellRuntime::new_with_replica_host_requiring_node_lease(
        SqlWorkerPool::new(1, 1).unwrap(),
        1_024,
        session,
        ReplicaHost::default(),
    )
    .unwrap();

    assert!(matches!(
        runtime.try_reserve_node_bytes(1),
        Err(crab_cell_runtime::Error::Fenced)
    ));
    let lease = NodeLeaseGuard::new(0, 60_000).unwrap();
    runtime.install_node_lease(lease.clone()).unwrap();
    assert!(runtime.install_node_lease(lease.clone()).is_err());
    drop(runtime.try_reserve_node_bytes(1).unwrap());

    lease.fence();
    assert!(matches!(
        runtime.try_reserve_node_bytes(1),
        Err(crab_cell_runtime::Error::Fenced)
    ));
    runtime.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn node_lease_expiry_hides_an_inflight_committed_command() {
    let fixture = fixture_for(b"node-lease-output-gate");
    let session = SessionId::from_bytes([42; 16]);
    let runtime = CellRuntime::new_with_replica_host_requiring_node_lease(
        SqlWorkerPool::new(1, 1).unwrap(),
        2 * 1024 * 1024,
        session,
        ReplicaHost::default(),
    )
    .unwrap();
    let lease = NodeLeaseGuard::new(0, 60_000).unwrap();
    runtime.install_node_lease(lease.clone()).unwrap();
    let handle = bootstrap_on(&runtime, &fixture, session).await;
    let (entered_tx, entered_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();

    let command = tokio::spawn(async move {
        handle
            .execute(
                identity(43),
                Digest::from_bytes([44; 32]),
                20,
                1,
                16,
                move |transaction| {
                    transaction.execute("UPDATE counter SET value = value + 1", [])?;
                    entered_tx.send(()).unwrap();
                    resume_rx.recv().unwrap();
                    Ok(HandlerOutcome::Success(b"hidden".to_vec()))
                },
            )
            .await
    });
    tokio::task::spawn_blocking(move || {
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
    })
    .await
    .unwrap();
    lease.fence();
    resume_tx.send(()).unwrap();

    match command.await.unwrap() {
        Err(crab_cell_runtime::Error::OutcomeUnknown { source, .. }) => {
            assert!(matches!(*source, crab_cell_runtime::Error::Fenced));
        }
        other => panic!("unexpected command result: {other:?}"),
    }
    let control = CellAuthority::new(fixture.layout.clone())
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(control.value().root.as_ref().unwrap().commit_sequence, 0);
    assert!(matches!(
        runtime.shutdown().await,
        Err(crab_cell_runtime::Error::Fenced)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn released_cell_is_acquired_by_one_successor_runtime() {
    let fixture = fixture_for(b"successor-runtime-movement");
    let first_session = SessionId::from_bytes([81; 16]);
    let second_session = SessionId::from_bytes([82; 16]);
    let first_runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        8 * 1024 * 1024,
        first_session,
    )
    .unwrap();
    let handle = bootstrap_on(&first_runtime, &fixture, first_session).await;
    handle.drain().await.unwrap();
    assert_eq!(first_runtime.stats().active_cells(), 0);

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
    let second_runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        8 * 1024 * 1024,
        second_session,
    )
    .unwrap();
    let successor = second_runtime
        .acquire_idle_restored(
            proof,
            fixture.replica.clone(),
            authority.clone(),
            idle,
            fixture._directory.path().join("successor.sqlite"),
            Owner {
                session: second_session,
                endpoint: "https://successor.internal:8081".into(),
            },
        )
        .await
        .unwrap();

    assert_eq!(first_runtime.stats().active_cells(), 0);
    assert_eq!(second_runtime.stats().active_cells(), 1);
    assert_eq!(
        authority
            .load(fixture.target.cell_id())
            .await
            .unwrap()
            .unwrap()
            .value()
            .owner
            .as_ref()
            .map(|owner| owner.session),
        Some(second_session)
    );

    successor.drain().await.unwrap();
    assert_eq!(second_runtime.stats().active_cells(), 0);
    second_runtime.shutdown().await.unwrap();
    first_runtime.shutdown().await.unwrap();
}

#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread")]
async fn independent_processes_allow_one_idle_cell_winner() {
    let object_root = tempfile::TempDir::new().unwrap();
    let fixture = filesystem_fixture(b"process-movement-race", object_root.path());
    let session = SessionId::from_bytes([83; 16]);
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 8 * 1024 * 1024, session).unwrap();
    let handle = bootstrap_on(&runtime, &fixture, session).await;
    handle.drain().await.unwrap();
    runtime.shutdown().await.unwrap();

    let authority = CellAuthority::new(fixture.layout.clone());
    let idle = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(idle.value().state, ControlState::Idle);
    let root = idle.value().ltx_root().unwrap();
    let binary = std::env::var("CARGO_BIN_EXE_cell_movement_probe")
        .expect("Cargo must provide the movement probe binary to integration tests");
    let store_root = object_root.path().to_owned();
    let partition = "process-movement-race";
    let first_destination = fixture._directory.path().join("process-first.sqlite");
    let second_destination = fixture._directory.path().join("process-second.sqlite");
    let first_store_root = store_root.clone();
    let first_binary = binary.clone();
    let first = tokio::task::spawn_blocking(move || {
        std::process::Command::new(first_binary)
            .stderr(std::process::Stdio::null())
            .args([
                first_store_root.as_os_str().to_string_lossy().as_ref(),
                partition,
                "53535353535353535353535353535353",
                first_destination.to_string_lossy().as_ref(),
                "750",
                "drain",
            ])
            .status()
            .unwrap()
    });
    let second_store_root = store_root;
    let second_binary = binary;
    let second = tokio::task::spawn_blocking(move || {
        std::thread::sleep(std::time::Duration::from_millis(25));
        std::process::Command::new(second_binary)
            .stderr(std::process::Stdio::null())
            .args([
                second_store_root.as_os_str().to_string_lossy().as_ref(),
                partition,
                "54545454545454545454545454545454",
                second_destination.to_string_lossy().as_ref(),
                "100",
                "drain",
            ])
            .status()
            .unwrap()
    });
    let (first, second) = tokio::join!(first, second);
    let first = first.unwrap();
    let second = second.unwrap();
    assert_ne!(first.success(), second.success());

    let final_control = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(final_control.value().state, ControlState::Idle);
    assert_eq!(final_control.value().ltx_root(), Some(root));
}

#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread")]
async fn independent_process_receiver_failure_returns_exact_idle_root() {
    let object_root = tempfile::TempDir::new().unwrap();
    let fixture = filesystem_fixture(b"process-movement-receiver-failure", object_root.path());
    let session = SessionId::from_bytes([89; 16]);
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 8 * 1024 * 1024, session).unwrap();
    let handle = bootstrap_on(&runtime, &fixture, session).await;
    handle.drain().await.unwrap();
    runtime.shutdown().await.unwrap();

    let authority = CellAuthority::new(fixture.layout.clone());
    let idle = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let root = idle.value().ltx_root();
    let binary = std::env::var("CARGO_BIN_EXE_cell_movement_probe")
        .expect("Cargo must provide the movement probe binary to integration tests");
    let destination = fixture
        ._directory
        .path()
        .join("process-receiver-parent-does-not-exist")
        .join("process-receiver.sqlite");
    std::fs::create_dir_all(&destination).unwrap();
    let status = tokio::task::spawn_blocking({
        let store_root = object_root.path().to_owned();
        move || {
            std::process::Command::new(binary)
                .args([
                    store_root.as_os_str().to_string_lossy().as_ref(),
                    "process-movement-receiver-failure",
                    "59595959595959595959595959595959",
                    destination.to_string_lossy().as_ref(),
                    "0",
                    "fail-receiver",
                ])
                .output()
                .unwrap()
        }
    })
    .await
    .unwrap();
    assert!(
        status.status.success(),
        "receiver-failure probe failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );

    let current = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.value().state, ControlState::Idle);
    assert!(current.value().owner.is_none());
    assert_eq!(current.value().ltx_root(), root);
}

#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread")]
async fn crashed_process_is_fenced_before_successor_restore() {
    let object_root = tempfile::TempDir::new().unwrap();
    let fixture = filesystem_fixture(b"process-movement-crash", object_root.path());
    let session = SessionId::from_bytes([85; 16]);
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 8 * 1024 * 1024, session).unwrap();
    let handle = bootstrap_on(&runtime, &fixture, session).await;
    handle.drain().await.unwrap();
    runtime.shutdown().await.unwrap();

    let binary = std::env::var("CARGO_BIN_EXE_cell_movement_probe")
        .expect("Cargo must provide the movement probe binary to integration tests");
    let destination = fixture._directory.path().join("process-crashed.sqlite");
    let status = tokio::task::spawn_blocking({
        let store_root = object_root.path().to_owned();
        move || {
            std::process::Command::new(binary)
                .stderr(std::process::Stdio::null())
                .args([
                    store_root.as_os_str().to_string_lossy().as_ref(),
                    "process-movement-crash",
                    "55555555555555555555555555555555",
                    destination.to_string_lossy().as_ref(),
                    "0",
                    "crash",
                ])
                .status()
                .unwrap()
        }
    })
    .await
    .unwrap();
    assert!(status.success());

    let authority = CellAuthority::new(fixture.layout.clone());
    let stale = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stale.value().owner.as_ref().map(|owner| owner.session),
        Some(SessionId::from_bytes([85; 16]))
    );
    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(
        fixture.layout.clone(),
        fixture.target.tenant(),
    );
    let proof = catalog
        .lookup(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let successor = SessionId::from_bytes([86; 16]);
    let fenced = fence_session(&fixture.layout, SessionId::from_bytes([85; 16]), successor).await;
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        8 * 1024 * 1024,
        successor,
    )
    .unwrap();
    let restored = runtime
        .takeover_restored(
            proof,
            fixture.replica.clone(),
            authority.clone(),
            stale,
            fenced.direct_takeover().unwrap(),
            crab_cell_runtime::recovery::manifest::RecoveryManifestStore::new(
                fixture.layout.clone(),
                Limits::default(),
            ),
            fixture._directory.path().join("process-successor.sqlite"),
            Owner {
                session: successor,
                endpoint: "https://process-successor.internal:8081".into(),
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
    runtime.shutdown().await.unwrap();
}

#[cfg(feature = "test-support")]
#[tokio::test(flavor = "multi_thread")]
async fn lost_release_response_is_reconciled_before_successor_acquire() {
    let object_root = tempfile::TempDir::new().unwrap();
    let fixture = filesystem_fixture(b"process-movement-lost-release", object_root.path());
    let session = SessionId::from_bytes([87; 16]);
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 8 * 1024 * 1024, session).unwrap();
    let handle = bootstrap_on(&runtime, &fixture, session).await;
    handle.drain().await.unwrap();
    runtime.shutdown().await.unwrap();

    let binary = std::env::var("CARGO_BIN_EXE_cell_movement_probe")
        .expect("Cargo must provide the movement probe binary to integration tests");
    let status = tokio::task::spawn_blocking({
        let store_root = object_root.path().to_owned();
        let destination = fixture
            ._directory
            .path()
            .join("process-lost-release.sqlite");
        move || {
            std::process::Command::new(binary)
                .stderr(std::process::Stdio::null())
                .args([
                    store_root.as_os_str().to_string_lossy().as_ref(),
                    "process-movement-lost-release",
                    "57575757575757575757575757575757",
                    destination.to_string_lossy().as_ref(),
                    "0",
                    "lost-release",
                ])
                .status()
                .unwrap()
        }
    })
    .await
    .unwrap();
    assert!(status.success());

    let authority = CellAuthority::new(fixture.layout.clone());
    let idle = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(idle.value().state, ControlState::Idle);
    let root = idle.value().ltx_root().unwrap();
    let catalog = crab_cell_runtime::cell::catalog::CellCatalog::new(
        fixture.layout.clone(),
        fixture.target.tenant(),
    );
    let proof = catalog
        .lookup(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let successor = SessionId::from_bytes([88; 16]);
    let successor_runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        8 * 1024 * 1024,
        successor,
    )
    .unwrap();
    let restored = successor_runtime
        .acquire_idle_restored(
            proof,
            fixture.replica.clone(),
            authority.clone(),
            idle,
            fixture
                ._directory
                .path()
                .join("process-lost-release-successor.sqlite"),
            Owner {
                session: successor,
                endpoint: "https://process-lost-release-successor.internal:8081".into(),
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
    successor_runtime.shutdown().await.unwrap();
    assert_eq!(
        authority
            .load(fixture.target.cell_id())
            .await
            .unwrap()
            .unwrap()
            .value()
            .ltx_root(),
        Some(root)
    );
}

#[tokio::test]
async fn observed_takeover_fences_the_old_cell_before_more_work() {
    let fixture = fixture();
    let (runtime, handle, _pool) = activate_runtime(&fixture, 16 * 1024 * 1024).await;
    let authority = CellAuthority::new(fixture.layout.clone());
    let observed = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let successor = observed
        .value()
        .takeover(Owner {
            session: SessionId::from_bytes([99; 16]),
            endpoint: "https://successor.internal:8081".into(),
        })
        .unwrap();
    let successor = authority
        .transition(&observed, successor, Transition::Takeover)
        .await
        .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match handle.query(1, 1, |_| Ok(Vec::new())).await {
                Err(crab_cell_runtime::Error::Fenced) => break,
                Ok(_) => tokio::time::sleep(std::time::Duration::from_millis(25)).await,
                Err(error) => panic!("unexpected query outcome while awaiting fence: {error}"),
            }
        }
    })
    .await
    .unwrap();
    runtime.shutdown().await.unwrap();
    let current = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.value(), successor.value());
}

#[tokio::test]
async fn idle_control_is_acquired_before_exact_root_restore() {
    let fixture = fixture();
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    handle.drain().await.unwrap();

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
    let session = SessionId::from_bytes([40; 16]);
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        session,
    )
    .unwrap();
    let restored = runtime
        .acquire_idle_restored(
            proof,
            fixture.replica.clone(),
            authority.clone(),
            idle,
            fixture._directory.path().join("idle-acquire.sqlite"),
            Owner {
                session,
                endpoint: "https://idle-successor.internal:8081".into(),
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
    let owned = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owned.value().state, ControlState::Serving);
    assert_eq!(owned.value().epoch, 2);
    assert_eq!(owned.value().owner.as_ref().unwrap().session, session);
    restored.drain().await.unwrap();
}

#[tokio::test]
async fn unchanged_dead_owner_is_taken_over_then_restored() {
    let fixture = fixture();
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    drop(handle);

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
    let stale = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let fenced = fence_session(
        &fixture.layout,
        stale.value().owner.as_ref().unwrap().session,
        SessionId::from_bytes([41; 16]),
    )
    .await;
    let takeover = fenced.direct_takeover().unwrap();
    let session = SessionId::from_bytes([41; 16]);
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        session,
    )
    .unwrap();
    let restored = runtime
        .takeover_restored(
            proof,
            fixture.replica.clone(),
            authority.clone(),
            stale,
            takeover,
            crab_cell_runtime::recovery::manifest::RecoveryManifestStore::new(
                fixture.layout.clone(),
                Limits::default(),
            ),
            fixture._directory.path().join("takeover.sqlite"),
            Owner {
                session,
                endpoint: "https://takeover-successor.internal:8081".into(),
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
    let owned = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owned.value().state, ControlState::Serving);
    assert_eq!(owned.value().epoch, 2);
    assert_eq!(owned.value().owner.as_ref().unwrap().session, session);
    restored.drain().await.unwrap();
}

#[tokio::test]
async fn failed_takeover_receiver_does_not_leave_authority_owned() {
    let fixture = fixture_for(b"takeover-receiver-activation-failure");
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    drop(handle);

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
    let stale = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let expected_root = stale.value().root.clone();
    let previous = stale.value().owner.as_ref().unwrap().session;
    let successor = SessionId::from_bytes([44; 16]);
    let takeover = fence_session(&fixture.layout, previous, successor).await;
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        16 * 1024 * 1024,
        successor,
    )
    .unwrap();
    let missing_parent = fixture
        ._directory
        .path()
        .join("takeover-receiver-parent-does-not-exist")
        .join("takeover-receiver.sqlite");
    assert!(
        runtime
            .takeover_restored(
                proof,
                fixture.replica.clone(),
                authority.clone(),
                stale,
                takeover.direct_takeover().unwrap(),
                crab_cell_runtime::recovery::manifest::RecoveryManifestStore::new(
                    fixture.layout.clone(),
                    Limits::default(),
                ),
                missing_parent,
                Owner {
                    session: successor,
                    endpoint: "https://takeover-receiver-failure.internal:8081".into(),
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

#[tokio::test]
async fn takeover_resumes_pinned_recovery_before_serving() {
    recover_retained_tail(false).await;
}

#[tokio::test]
async fn recovery_seals_already_rooted_tail_without_an_empty_manifest() {
    recover_retained_tail(true).await;
}

async fn recover_retained_tail(rooted: bool) {
    let fixture = fixture_for(b"recovered-takeover");
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    drop(handle);

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
    let stale = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let predecessor = stale.value().ltx_root().unwrap();
    let leader = stale.value().owner.as_ref().unwrap().session;

    let tail_directory = tempfile::TempDir::new().unwrap();
    let tail_path = tail_directory.path().join("tail.sqlite");
    let writable = fixture
        .replica
        .open_root(&predecessor)
        .await
        .unwrap()
        .paged()
        .prepare_writable(&tail_path)
        .await
        .unwrap();
    let mut writer = writable.open_writable(&tail_path).unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute("UPDATE counter SET value = value + 1", [])?;
            transaction.execute(
                "UPDATE sys_meta SET commit_sequence = commit_sequence + 1, logical_time_ms = logical_time_ms + 1 WHERE singleton = 1",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let capture = writer.capture().unwrap();
    let mut frames = Vec::with_capacity(capture.segments.len());
    for (index, segment) in capture.segments.iter().enumerate() {
        frames.push(
            crab_ltx::encode_node_frame(
                crab_ltx::NodeFrameScope {
                    leader_session: *leader.as_bytes(),
                    log_epoch: 1,
                    node_sequence: u64::try_from(index).unwrap() + 1,
                    application: *fixture.layout.application_id(),
                    cell: *fixture.target.cell_id().as_bytes(),
                    incarnation: *stale.value().incarnation.as_bytes(),
                    cell_epoch: stale.value().epoch,
                    commit_sequence: predecessor.commit_sequence + 1,
                },
                segment.info().clone(),
                Bytes::from(std::fs::read(segment.path()).unwrap()),
                Limits::default(),
            )
            .unwrap()
            .encoded()
            .clone(),
        );
    }
    if rooted {
        // Crash after the exact Cell root CAS but before shared node coverage.
        let prepared = fixture
            .replica
            .prepare(
                Some(&predecessor),
                &capture,
                predecessor.commit_sequence + 1,
                stale.value().schema,
            )
            .await
            .unwrap();
        authority
            .transition(
                &stale,
                stale.value().publish_prepared(&prepared, None).unwrap(),
                Transition::Publish,
            )
            .await
            .unwrap();
    }
    writer.close().unwrap();
    let follower = SessionId::from_bytes([43; 16]);
    let follower_directory = tempfile::TempDir::new().unwrap();
    let follower_store = crab_cell_runtime::FollowerStore::open(
        follower_directory.path().to_owned(),
        Limits::default(),
        crab_cell_runtime::ltx::DiskBudget::new(1 << 30),
    )
    .unwrap();
    let transport: Arc<dyn crab_cell_runtime::node::log_transport::NodeLogTransport> = Arc::new(
        crab_cell_runtime::node::log_transport::LocalFollowerTransport::new(
            crab_cell_runtime::identity::NodeId::from_bytes(*follower.as_bytes()),
            follower_store,
        ),
    );
    transport
        .append(
            crab_cell_runtime::identity::NodeId::from_bytes(*follower.as_bytes()),
            crab_cell_runtime::node::log_transport::AppendRequest {
                leader_session: leader,
                log_epoch: 1,
                frames,
                covered_through: 0,
            },
        )
        .await
        .unwrap();
    let manifests = crab_cell_runtime::recovery::manifest::RecoveryManifestStore::new(
        fixture.layout.clone(),
        Limits::default(),
    );
    let successor = SessionId::from_bytes([42; 16]);
    let fenced = fence_log_session(&fixture.layout, leader, successor, follower, 0).await;
    assert!(matches!(
        fenced.direct_takeover(),
        Err(crab_cell_runtime::Error::PendingPublication)
    ));
    let recovery = crab_cell_runtime::node::log_recovery::NodeLogRecovery::from_fenced(
        Arc::clone(&transport),
        &fenced,
        Limits::default(),
    )
    .unwrap();
    let coordinator = crab_cell_runtime::node::log_recovery::RecoveryCoordinator::new(
        recovery,
        manifests.clone(),
    );
    let inventory =
        crab_cell_runtime::node::log_recovery::recoverable_cells(&catalog, &authority, leader, 10)
            .await
            .unwrap();
    assert_eq!(inventory.len(), 1);
    if rooted {
        let directory = crab_cell_runtime::node::NodeDirectory::new(
            fixture.layout.clone(),
            Digest::from_bytes([90; 32]),
            Digest::from_bytes([91; 32]),
            Digest::from_bytes([92; 32]),
        );
        let completed = coordinator
            .recover_and_seal(&directory, fenced, inventory, 10_002)
            .await
            .unwrap();
        assert!(completed.controls.is_empty());
        assert_eq!(
            completed.sealed.log().phase(),
            crab_cell_runtime::node::log_state::NodeLogPhase::Sealed
        );
        return;
    }
    let attached = coordinator
        .recover(fenced.clone(), inventory)
        .await
        .unwrap();
    assert_eq!(attached.len(), 1);
    drop(coordinator);
    let resumed_recovery = crab_cell_runtime::node::log_recovery::NodeLogRecovery::from_fenced(
        transport,
        &fenced,
        Limits::default(),
    )
    .unwrap();
    let resumed = crab_cell_runtime::node::log_recovery::RecoveryCoordinator::new(
        resumed_recovery,
        manifests.clone(),
    );
    let directory = crab_cell_runtime::node::NodeDirectory::new(
        fixture.layout.clone(),
        Digest::from_bytes([90; 32]),
        Digest::from_bytes([91; 32]),
        Digest::from_bytes([92; 32]),
    );
    let completed = resumed
        .recover_and_seal(
            &directory,
            fenced.clone(),
            vec![crab_cell_runtime::node::log_recovery::RecoveryCell {
                application: fixture.target.application(),
                authority: authority.clone(),
                observed: attached[0].clone(),
            }],
            10_002,
        )
        .await
        .unwrap();
    assert_eq!(completed.controls[0].value(), attached[0].value());
    assert_eq!(
        completed.sealed.log().phase(),
        crab_cell_runtime::node::log_state::NodeLogPhase::Sealed
    );
    let repeated = resumed
        .recover_and_seal(
            &directory,
            fenced.clone(),
            vec![crab_cell_runtime::node::log_recovery::RecoveryCell {
                application: fixture.target.application(),
                authority: authority.clone(),
                observed: completed.controls[0].clone(),
            }],
            10_003,
        )
        .await
        .unwrap();
    assert_eq!(repeated.sealed, completed.sealed);
    let attached = repeated.controls.into_iter().next().unwrap();
    let takeover = repeated.takeover;
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        successor,
    )
    .unwrap();
    let restored = runtime
        .takeover_restored(
            proof,
            fixture.replica.clone(),
            authority.clone(),
            attached,
            takeover,
            manifests,
            fixture._directory.path().join("recovered-takeover.sqlite"),
            Owner {
                session: successor,
                endpoint: "https://recovered-successor.internal:8081".into(),
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
        1_i64.to_be_bytes()
    );
    let serving = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(serving.value().state, ControlState::Serving);
    assert!(serving.value().recovery.is_none());
    assert_eq!(
        serving.value().root.as_ref().unwrap().commit_sequence,
        predecessor.commit_sequence + 1
    );
    restored.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn unchanged_unpublished_owner_is_taken_over_then_bootstrapped() {
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
    let authority = CellAuthority::new(fixture.layout.clone());
    let stale = authority
        .create_initial(
            &proof,
            IncarnationId::from_bytes([2; 16]),
            Owner {
                session: SessionId::from_bytes([4; 16]),
                endpoint: "https://stopped-import.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let fenced = fence_session(
        &fixture.layout,
        stale.value().owner.as_ref().unwrap().session,
        SessionId::from_bytes([42; 16]),
    )
    .await;
    let takeover = fenced.direct_takeover().unwrap();
    let session = SessionId::from_bytes([42; 16]);
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        session,
    )
    .unwrap();
    let restored = runtime
        .takeover_unpublished(
            proof,
            fixture.replica.clone(),
            authority.clone(),
            stale,
            takeover,
            fixture
                ._directory
                .path()
                .join("takeover-unpublished.sqlite"),
            Owner {
                session,
                endpoint: "https://import-successor.internal:8081".into(),
            },
            |transaction| {
                transaction.execute_batch(
                    "CREATE TABLE counter(value INTEGER NOT NULL); INSERT INTO counter VALUES (7)",
                )?;
                Ok(())
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
        7_i64.to_be_bytes()
    );
    let owned = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owned.value().epoch, 2);
    assert_eq!(owned.value().owner.as_ref().unwrap().session, session);
    restored.drain().await.unwrap();
}

#[tokio::test]
async fn slow_bootstrap_renews_unpublished_ownership_before_publication() {
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
    let authority = CellAuthority::new(fixture.layout.clone());
    let session = SessionId::from_bytes([43; 16]);
    let observed = authority
        .create_initial(
            &proof,
            IncarnationId::from_bytes([2; 16]),
            Owner {
                session,
                endpoint: "https://slow-bootstrap.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        session,
    )
    .unwrap();
    let handle = runtime
        .bootstrap(
            proof,
            fixture.replica.clone(),
            authority.clone(),
            observed,
            fixture._directory.path().join("slow-bootstrap.sqlite"),
            |transaction| {
                std::thread::sleep(std::time::Duration::from_secs(4));
                transaction.execute_batch(
                    "CREATE TABLE counter(value INTEGER NOT NULL); INSERT INTO counter VALUES (0)",
                )?;
                Ok(())
            },
        )
        .await
        .unwrap();
    let published = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(published.value().state, ControlState::Serving);
    assert!(published.value().revision >= 3);
    handle.drain().await.unwrap();
}

#[tokio::test]
async fn activation_rejects_control_owned_by_another_node_session() {
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
    let authority = CellAuthority::new(fixture.layout.clone());
    let observed = authority
        .create_initial(
            &proof,
            IncarnationId::from_bytes([2; 16]),
            Owner {
                session: SessionId::from_bytes([4; 16]),
                endpoint: "https://node.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 1).unwrap(),
        2 * 1024 * 1024,
        SessionId::from_bytes([9; 16]),
    )
    .unwrap();
    assert!(matches!(
        runtime
            .bootstrap(
                proof,
                fixture.replica,
                authority,
                observed,
                fixture.database,
                |_| Ok(()),
            )
            .await,
        Err(crab_cell_runtime::Error::Fenced)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn source_loss_takeover_restores_exact_root_and_continues_publication() {
    source_loss_takeover(
        Store::new(Arc::new(InMemory::new())),
        Path::from("cold-runtime"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an isolated pre-created RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_source_loss_takeover_restores_exact_root_and_continues_publication() {
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
        "{}/cold-runtime",
        required("CRAB_CELL_TEST_PREFIX")
    ));
    source_loss_takeover(store, prefix).await;
}

async fn source_loss_takeover(store: Store, prefix: Path) {
    let target = CellTarget::new(
        TenantId::from_bytes([41; 16]),
        ApplicationId::from_bytes([42; 16]),
        NamespaceId::from_bytes([43; 16]),
        b"repository-cold-start",
    )
    .unwrap();
    let cell = target.cell_id();
    let incarnation = IncarnationId::from_bytes([44; 16]);
    let layout = CellStorageLayout::new(store, prefix, [42; 16]);
    let replica = CellReplica::new(
        layout.clone(),
        *cell.as_bytes(),
        *incarnation.as_bytes(),
        Limits::default(),
    )
    .unwrap();
    let catalog =
        crab_cell_runtime::cell::catalog::CellCatalog::new(layout.clone(), target.tenant());
    let proof = catalog
        .provision(
            CatalogEntry::new(
                &target,
                CatalogRole::Repository,
                Digest::from_bytes([45; 32]),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();

    let first_session = SessionId::from_bytes([46; 16]);
    let authority = CellAuthority::new(layout.clone());
    let recovering = authority
        .create_initial(
            &proof,
            incarnation,
            Owner {
                session: first_session,
                endpoint: "https://node-one.internal:8081".into(),
            },
        )
        .await
        .unwrap();

    let first_local = tempfile::TempDir::new().unwrap();
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        first_session,
    )
    .unwrap();
    let first = runtime
        .bootstrap(
            proof.clone(),
            replica.clone(),
            authority.clone(),
            recovering,
            first_local.path().join("cell.sqlite"),
            |transaction| {
                transaction.execute_batch(
                    "CREATE TABLE counter(value INTEGER NOT NULL); INSERT INTO counter VALUES (0)",
                )?;
                Ok(())
            },
        )
        .await
        .unwrap();
    let first_identity = identity(47);
    let first_digest = Digest::from_bytes([48; 32]);
    let first_outcome = first
        .execute(
            first_identity,
            first_digest,
            20,
            1_024,
            1_024,
            |transaction| {
                transaction.execute("UPDATE counter SET value = value + 1", [])?;
                Ok(HandlerOutcome::Success(b"first".to_vec()))
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        first_outcome,
        StoredOutcome::Success {
            commit_sequence: 1,
            ..
        }
    ));
    first.drain().await.unwrap();
    drop(runtime);
    first_local.close().unwrap();

    let current = authority.load(cell).await.unwrap().unwrap();
    let second_session = SessionId::from_bytes([49; 16]);
    let mut takeover = current.value().clone();
    takeover.epoch += 1;
    takeover.revision += 1;
    takeover.progress += 1;
    takeover.state = ControlState::Recovering;
    takeover.owner = Some(Owner {
        session: second_session,
        endpoint: "https://node-two.internal:8081".into(),
    });
    let takeover = authority
        .transition(&current, takeover, Transition::Takeover)
        .await
        .unwrap();

    let second_local = tempfile::TempDir::new().unwrap();
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 10).unwrap(),
        16 * 1024 * 1024,
        second_session,
    )
    .unwrap();
    let second = runtime
        .activate_restored(
            proof,
            replica,
            authority.clone(),
            takeover,
            crab_cell_runtime::recovery::manifest::RecoveryManifestStore::new(
                layout.clone(),
                Limits::default(),
            ),
            second_local.path().join("cell.sqlite"),
        )
        .await
        .unwrap();
    assert_eq!(
        authority.load(cell).await.unwrap().unwrap().value().state,
        ControlState::Serving
    );
    assert_eq!(
        second
            .resolve(first_identity, first_digest, 21, 1_024)
            .await
            .unwrap(),
        Resolution::Committed(first_outcome)
    );
    assert!(matches!(
        second
            .execute(
                identity(50),
                Digest::from_bytes([51; 32]),
                21,
                1_024,
                1_024,
                |transaction| {
                    let value = transaction
                        .query_row("SELECT value FROM counter", [], |row| row.get::<_, i64>(0))?;
                    transaction.execute("UPDATE counter SET value = value + 1", [])?;
                    Ok(HandlerOutcome::Success(value.to_be_bytes().to_vec()))
                },
            )
            .await
            .unwrap(),
        StoredOutcome::Success { ref result, commit_sequence: 2 }
            if result == &1_i64.to_be_bytes()
    ));
    second.drain().await.unwrap();
    assert_eq!(
        authority
            .load(cell)
            .await
            .unwrap()
            .unwrap()
            .value()
            .root
            .as_ref()
            .unwrap()
            .commit_sequence,
        2
    );
}
