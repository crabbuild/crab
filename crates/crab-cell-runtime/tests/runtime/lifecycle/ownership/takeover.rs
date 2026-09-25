//! Fenced takeover of idle, dead, and unpublished owners.

use super::*;
use object_store::ObjectStoreExt;

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

#[tokio::test]
async fn missing_authoritative_root_cannot_become_a_serving_cell() {
    unavailable_authoritative_root_cannot_serve(false).await;
}

#[tokio::test]
async fn torn_authoritative_root_cannot_become_a_serving_cell() {
    unavailable_authoritative_root_cannot_serve(true).await;
}

async fn unavailable_authoritative_root_cannot_serve(corrupt: bool) {
    let backend = Arc::new(InMemory::new());
    let fixture = fixture_with_limits_and_store(
        b"unavailable-authoritative-root",
        Limits::default(),
        Store::new(backend.clone()),
    );
    let handle = activate(&fixture, 16 * 1024 * 1024).await;
    handle.drain().await.unwrap();
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
    let root_path = fixture.layout.incarnation_object_path(
        fixture.target.cell_id().as_bytes(),
        idle.value().incarnation.as_bytes(),
        &root.digest,
        CellObjectKind::Root,
    );
    if corrupt {
        backend
            .put(&root_path, Bytes::from_static(b"torn-root").into())
            .await
            .unwrap();
    } else {
        backend.delete(&root_path).await.unwrap();
    }

    let session = SessionId::from_bytes([138; 16]);
    let receiver = tempfile::TempDir::new().unwrap();
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 16 * 1024 * 1024, session).unwrap();
    let error = runtime
        .acquire_idle_restored(
            proof,
            fixture.replica.clone(),
            authority.clone(),
            idle,
            receiver.path().join("missing-root-restored.sqlite"),
            Owner {
                session,
                endpoint: "https://missing-root.internal:8081".into(),
            },
        )
        .await
        .err()
        .expect("unavailable root must not activate");
    let current = authority
        .load(fixture.target.cell_id())
        .await
        .unwrap()
        .unwrap();
    eprintln!(
        "fault_seed={} schedule={} request=none committed_sequence={} selected_follower_tickets=[] root={root:?} observed_control={:?} error={error:?}",
        if corrupt { 139 } else { 138 },
        if corrupt {
            "torn_authoritative_root"
        } else {
            "missing_authoritative_root"
        },
        root.commit_sequence,
        current.value(),
    );
    assert_eq!(current.value().state, ControlState::Idle);
    assert!(current.value().owner.is_none());
    assert_eq!(current.value().ltx_root(), Some(root));
    assert_eq!(runtime.stats().active_cells(), 0);
    runtime.shutdown().await.unwrap();
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
