//! Independent-process successor election and acquire races.
//!
//! Every test here needs the `test-support` filesystem CAS store, so the whole
//! module is gated on that feature.

#![cfg(feature = "test-support")]

use super::*;

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
