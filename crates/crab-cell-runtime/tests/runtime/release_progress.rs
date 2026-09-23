//! Release progress tests extracted from `src/release_progress.rs`.
use crab_cell_runtime::cell::application::ApplicationIdentity;
use crab_cell_runtime::identity::RequestId;
use crab_cell_runtime::ltx::CellStorageLayout;
use crab_cell_runtime::recovery::release_progress::{
    MigrationFailure, MigrationProgressAttempt, MigrationProgressState, MigrationProgressStore,
};

use std::sync::Arc;

use crab_storage::Store;
use object_store::{memory::InMemory, path::Path};

use crab_cell_runtime::identity::{ApplicationId, TenantId};
use crab_cell_runtime::*;

fn fixture() -> (MigrationProgressStore, MigrationProgressAttempt) {
    let identity = ApplicationIdentity::new(
        TenantId::from_bytes([1; 16]),
        ApplicationId::from_bytes([2; 16]),
    );
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("progress"),
        *identity.application().as_bytes(),
    );
    let attempt = MigrationProgressAttempt::new(
        RequestId::from_bytes([3; 16]),
        Digest::from_bytes([4; 32]),
        SessionId::from_bytes([5; 16]),
        CellId::from_bytes([6; 32]),
        (Digest::from_bytes([7; 32]), 1),
        (Digest::from_bytes([8; 32]), 2),
    )
    .unwrap();
    (
        MigrationProgressStore::new(layout, identity).unwrap(),
        attempt,
    )
}

#[tokio::test]
async fn failure_retry_and_completion_are_canonical_and_monotonic() {
    let (store, attempt) = fixture();
    let failed = store
        .failed(attempt, MigrationFailure::Unavailable, 10)
        .await
        .unwrap();
    assert_eq!(failed.revision(), 1);
    assert_eq!(failed.attempts(), 1);
    assert_eq!(failed.state(), MigrationProgressState::Failed);
    assert_eq!(failed.failure(), Some(MigrationFailure::Unavailable));

    let completed = store.completed(attempt, 20).await.unwrap();
    assert_eq!(completed.revision(), 2);
    assert_eq!(completed.attempts(), 2);
    assert_eq!(completed.state(), MigrationProgressState::Completed);
    assert_eq!(completed.failure(), None);
    assert_eq!(store.completed(attempt, 30).await.unwrap(), completed);
    assert_eq!(
        store
            .failed(attempt, MigrationFailure::Internal, 40)
            .await
            .unwrap(),
        completed
    );
    assert_eq!(
        store
            .load(attempt.cell(), attempt.operation())
            .await
            .unwrap()
            .unwrap(),
        completed
    );
}

#[tokio::test]
async fn one_operation_path_rejects_version_scope_drift() {
    let (store, attempt) = fixture();
    store.completed(attempt, 10).await.unwrap();
    let changed = MigrationProgressAttempt::new(
        attempt.operation(),
        attempt.release(),
        attempt.session(),
        attempt.cell(),
        attempt.from(),
        (Digest::from_bytes([9; 32]), attempt.to().1),
    )
    .unwrap();
    assert!(store.completed(changed, 20).await.is_err());
}
