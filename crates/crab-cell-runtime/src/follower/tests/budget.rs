//! Follower byte budgets, admission, and grace-aged lane collection.

use super::*;

#[test]
fn open_reserves_only_existing_follower_bytes_and_rejects_an_undersized_budget() {
    let root = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(root.path().join("followers")).unwrap();
    std::fs::write(root.path().join("followers/retained.log"), [0_u8; 17]).unwrap();
    std::fs::create_dir(root.path().join("sessions")).unwrap();
    std::fs::write(root.path().join("sessions/unrelated.sqlite"), [0_u8; 64]).unwrap();

    assert!(
        FollowerStore::open(
            root.path().to_owned(),
            crab_ltx::Limits::default(),
            crab_ltx::DiskBudget::new(16),
        )
        .is_err()
    );

    let store = FollowerStore::open(
        root.path().to_owned(),
        crab_ltx::Limits::default(),
        crab_ltx::DiskBudget::new(32),
    )
    .unwrap();
    assert_eq!(store.retained_bytes(), 17);
    assert_eq!(store.available_bytes(), 15);
    assert_eq!(store.quarantined_entries(), 1);
    assert!(!root.path().join("followers/retained.log").exists());
}
#[tokio::test]
async fn retired_lane_collection_requires_exact_grace_aged_candidate() {
    let limits = crab_ltx::Limits::default();
    let source = tempfile::TempDir::new().unwrap();
    let mut database = Db::open(&source.path().join("cell.sqlite"), limits).unwrap();
    database
        .transaction(|transaction| transaction.execute_batch("CREATE TABLE values_(v)"))
        .unwrap();
    let capture = database.capture().unwrap();
    let encoded = frame(1, capture.segments.first().unwrap(), limits);
    let leader = SessionId::from_bytes([1; 16]);
    let root = tempfile::TempDir::new().unwrap();
    let store = FollowerStore::open(
        root.path().to_owned(),
        limits,
        crab_ltx::DiskBudget::new(1 << 30),
    )
    .unwrap();
    store.append(leader, 2, vec![encoded], 0).await.unwrap();
    store.retire(leader, 2, 1).await.unwrap();

    let candidates = store.retired_lanes(i64::MAX, 1).await.unwrap();
    assert_eq!(candidates.len(), 1);
    let candidate = candidates[0];
    assert_eq!(candidate.leader(), leader);
    assert_eq!(candidate.epoch(), 2);
    assert_eq!(candidate.covered_through(), 1);
    assert!(
        store
            .remove_retired(candidate, candidate.retired_at_ms().saturating_sub(1))
            .await
            .is_err()
    );

    assert!(store.remove_retired(candidate, i64::MAX).await.unwrap());
    assert!(!store.remove_retired(candidate, i64::MAX).await.unwrap());
    assert_eq!(store.retained_bytes(), 0);
    assert!(store.retired_lanes(i64::MAX, 1).await.unwrap().is_empty());
    database.close().unwrap();
}
