use super::*;

#[tokio::test]
async fn compressible_native_pack_cannot_exceed_shared_staging_capacity() {
    use axum::{http::StatusCode, response::IntoResponse};
    for (object_bytes, competing_bytes, expected_status) in [
        (512 * 1024, 0, StatusCode::PAYLOAD_TOO_LARGE),
        (32 * 1024, 96 * 1024, StatusCode::TOO_MANY_REQUESTS),
    ] {
        let owner = tempfile::TempDir::new().unwrap();
        let budget = DiskBudget::new(128 * 1024);
        let staging = LocalStaging::new(owner.path().join("staging"), budget.clone(), 0).unwrap();
        let directory = staging
            .create(4096, &CancellationToken::new())
            .await
            .unwrap();
        let competing = budget.try_reserve(competing_bytes).unwrap();
        let data = vec![b'x'; object_bytes];
        let mut wire = Vec::new();
        crab_git::pack_writer::write_pack(
            &mut wire,
            [Ok((
                gix_object::Kind::Blob,
                data.len() as u64,
                std::io::Cursor::new(data),
            ))]
            .into_iter(),
            1024 * 1024,
            || false,
        )
        .unwrap();
        assert!(wire.len() < 4096);
        std::fs::write(directory.path().join("receive"), &wire).unwrap();
        let directory = Arc::new(directory);
        let path = directory.path().to_owned();
        let scratch = staging.scratch(Arc::clone(&directory)).unwrap();
        let error = crab_git::incoming_pack::quarantine(
            std::io::Cursor::new(wire),
            &scratch,
            crab_git::incoming_pack::ReceiveLimits {
                max_pack_bytes: 1024 * 1024,
                max_objects: 1,
                max_object_bytes: 1024 * 1024,
                max_inflated_bytes: 2 * 1024 * 1024,
                max_delta_depth: 8,
            },
            || false,
            |_| Ok(None),
        )
        .err()
        .unwrap();
        assert!(inventory_bytes(&path).unwrap() <= budget.used() - competing_bytes);
        let response = crate::git::receive::ReceiveError::from(error).into_response();
        assert_eq!(response.status(), expected_status);
        drop(scratch);
        drop(directory);
        assert!(!path.exists());
        assert_eq!(budget.used(), competing_bytes);
        drop(competing);
    }
}

#[test]
fn restart_inventory_counts_stale_sessions_but_not_the_new_session() {
    let data = tempfile::TempDir::new().unwrap();
    let sessions = data.path().join("sessions");
    let stale = sessions.join("stale-session");
    let current = sessions.join("current-session");
    std::fs::create_dir_all(stale.join("cell/.crab-cell-directory-cache")).unwrap();
    std::fs::create_dir_all(&current).unwrap();
    std::fs::write(stale.join("cell.sqlite"), [0_u8; 17]).unwrap();
    std::fs::write(
        stale.join("cell/.crab-cell-directory-cache/node"),
        [0_u8; 23],
    )
    .unwrap();
    std::fs::write(current.join("new.sqlite"), [0_u8; 101]).unwrap();

    let budget = DiskBudget::new(40);
    let inventory = reserve_restart_inventory(data.path(), &current, budget.clone()).unwrap();

    assert_eq!(inventory.bytes(), 40);
    assert_eq!(inventory.sessions(), 1);
    assert_eq!(budget.used(), 40);
}

#[test]
fn restart_inventory_accounts_for_cell_recovery_and_compaction_scratch() {
    let data = tempfile::TempDir::new().unwrap();
    let sessions = data.path().join("sessions");
    let stale = sessions.join("stale-session");
    let current = sessions.join("current-session");
    let cell = stale.join("cell");
    std::fs::create_dir_all(&cell).unwrap();
    std::fs::create_dir_all(&current).unwrap();
    std::fs::write(cell.join(".crab-compaction-source-indexes"), [0_u8; 13]).unwrap();
    std::fs::write(cell.join(".crab-recovery-bundle"), [0_u8; 17]).unwrap();

    let budget = DiskBudget::new(30);
    let inventory = reserve_restart_inventory(data.path(), &current, budget.clone()).unwrap();

    assert_eq!(inventory.bytes(), 30);
    assert_eq!(inventory.sessions(), 1);
    assert_eq!(budget.used(), 30);
}

#[test]
fn restart_inventory_fails_closed_when_stale_bytes_exceed_capacity() {
    let data = tempfile::TempDir::new().unwrap();
    let sessions = data.path().join("sessions");
    let stale = sessions.join("stale-session");
    let current = sessions.join("current-session");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::create_dir_all(&current).unwrap();
    std::fs::write(stale.join("cell.sqlite"), [0_u8; 9]).unwrap();

    let budget = DiskBudget::new(8);
    assert!(matches!(
        reserve_restart_inventory(data.path(), &current, budget.clone()),
        Err(Error::Busy(Some(_)))
    ));
    assert_eq!(budget.used(), 0);
}

#[test]
fn staging_holds_restart_inventory_until_the_owner_drops() {
    let data = tempfile::TempDir::new().unwrap();
    let sessions = data.path().join("sessions");
    let stale = sessions.join("stale-session");
    let current = sessions.join("current-session");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join("cell.sqlite"), [0_u8; 7]).unwrap();
    let budget = DiskBudget::new(16);

    let staging = LocalStaging::new_with_restart_inventory(
        current.join("transfers"),
        budget.clone(),
        0,
        data.path(),
        &current,
    )
    .unwrap();
    assert_eq!(budget.used(), 7);
    assert_eq!(staging.restart_inventory_usage(), Some((7, 1)));
    drop(staging);
    assert_eq!(budget.used(), 0);
}

#[tokio::test]
async fn repeated_restarts_charge_every_unreclaimed_session_until_capacity_is_exhausted() {
    let data = tempfile::TempDir::new().unwrap();
    let sessions = data.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();

    for restart in 0..3 {
        let current = sessions.join(format!("session-{restart}"));
        std::fs::create_dir_all(&current).unwrap();
        let budget = DiskBudget::new(50);
        let staging = LocalStaging::new_with_restart_inventory(
            current.join("transfers"),
            budget.clone(),
            0,
            data.path(),
            &current,
        )
        .unwrap();
        assert_eq!(
            staging.restart_inventory_usage(),
            Some((17 * restart, restart as usize))
        );
        assert_eq!(staging.available_bytes(), 50 - 17 * restart);
        let file_bytes = if restart == 2 { 16 } else { 17 };
        std::fs::write(current.join("cell.sqlite"), vec![0_u8; file_bytes]).unwrap();
        drop(staging);
        assert_eq!(budget.used(), 0);
    }

    let current = sessions.join("session-3");
    std::fs::create_dir_all(&current).unwrap();
    let budget = DiskBudget::new(50);
    let staging = LocalStaging::new_with_restart_inventory(
        current.join("transfers"),
        budget.clone(),
        0,
        data.path(),
        &current,
    )
    .unwrap();
    assert_eq!(staging.restart_inventory_usage(), Some((50, 3)));
    assert_eq!(staging.available_bytes(), 0);
    assert!(matches!(
        staging.create(1, &CancellationToken::new()).await,
        Err(Error::Busy(Some(_)))
    ));
    drop(staging);
    assert_eq!(budget.used(), 0);
}

#[test]
fn restart_inventory_rejects_symlinked_owned_files() {
    let data = tempfile::TempDir::new().unwrap();
    let sessions = data.path().join("sessions");
    let stale = sessions.join("stale-session");
    let current = sessions.join("current-session");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::create_dir_all(&current).unwrap();
    std::fs::write(data.path().join("outside"), [0_u8; 1]).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(data.path().join("outside"), stale.join("link")).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(data.path().join("outside"), stale.join("link")).unwrap();

    let error = reserve_restart_inventory(data.path(), &current, DiskBudget::new(16)).unwrap_err();
    assert!(matches!(error, Error::Io(source) if source.kind() == io::ErrorKind::InvalidData));
}

#[tokio::test]
async fn capacity_precedes_creation_and_releases_with_the_directory() {
    let owner = tempfile::TempDir::new().unwrap();
    let staging =
        LocalStaging::new(owner.path().join("staging"), DiskBudget::new(2 * MIB), 0).unwrap();
    let cancellation = CancellationToken::new();
    let first = staging.create(2 * MIB, &cancellation).await.unwrap();
    assert!(first.path().starts_with(owner.path()));
    assert_eq!(staging.available_bytes(), 0);
    assert!(matches!(
        staging.create(1, &cancellation).await,
        Err(Error::Busy(Some(_)))
    ));
    drop(first);
    assert_eq!(staging.available_bytes(), 2 * MIB);
    drop(staging.create(1, &cancellation).await.unwrap());
    assert_eq!(staging.available_bytes(), 2 * MIB);
}

#[tokio::test]
async fn staging_shares_capacity_with_cell_disk_reservations() {
    let owner = tempfile::TempDir::new().unwrap();
    let budget = DiskBudget::new(2 * MIB);
    let staging = LocalStaging::new(owner.path().join("staging"), budget.clone(), 0).unwrap();
    let cell = budget.try_reserve(MIB + 1).unwrap();

    assert!(matches!(
        staging.create(MIB, &CancellationToken::new()).await,
        Err(Error::Busy(Some(_)))
    ));
    drop(cell);
    assert!(staging.create(MIB, &CancellationToken::new()).await.is_ok());
}

#[tokio::test]
async fn staging_reservation_grows_without_overcommitting_shared_capacity() {
    let owner = tempfile::TempDir::new().unwrap();
    let staging =
        LocalStaging::new(owner.path().join("staging"), DiskBudget::new(3 * MIB), 0).unwrap();
    let directory = staging
        .create(MIB, &CancellationToken::new())
        .await
        .unwrap();

    staging.resize(&directory, 3 * MIB).unwrap();
    assert_eq!(staging.available_bytes(), 0);
    assert!(matches!(
        staging.create(1, &CancellationToken::new()).await,
        Err(Error::Busy(Some(_)))
    ));
    drop(directory);
    assert_eq!(staging.available_bytes(), 3 * MIB);
}

#[tokio::test]
async fn oversized_and_cancelled_requests_never_create_a_directory() {
    let owner = tempfile::TempDir::new().unwrap();
    let root = owner.path().join("staging");
    let staging = LocalStaging::new(root.clone(), DiskBudget::new(MIB), 0).unwrap();
    assert!(matches!(
        staging.create(2 * MIB, &CancellationToken::new()).await,
        Err(Error::TooLarge)
    ));
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        staging.create(1, &cancellation).await,
        Err(Error::Cancelled)
    ));
    assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
}

#[tokio::test]
async fn actual_free_space_preserves_the_configured_node_reserve() {
    let owner = tempfile::TempDir::new().unwrap();
    let root = owner.path().join("staging");
    std::fs::create_dir_all(&root).unwrap();
    let reserve = u64::MAX - MIB;
    let staging = LocalStaging::new(root.clone(), DiskBudget::new(MIB), reserve).unwrap();
    assert!(matches!(
        staging.create(1, &CancellationToken::new()).await,
        Err(Error::Busy(None))
    ));
    assert_eq!(staging.available_bytes(), MIB);
    assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
}
