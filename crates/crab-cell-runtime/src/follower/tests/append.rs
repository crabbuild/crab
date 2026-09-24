//! Lane appends, torn-suffix recovery, deduplication, seals, and retirement.

use super::*;

#[tokio::test]
async fn append_recovers_torn_suffix_deduplicates_and_seals() {
    let limits = crab_ltx::Limits::default();
    let source = tempfile::TempDir::new().unwrap();
    let mut database = Db::open(&source.path().join("cell.sqlite"), limits).unwrap();
    database
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE events(id INTEGER PRIMARY KEY, body TEXT NOT NULL);\
                 INSERT INTO events(body) VALUES ('one')",
            )
        })
        .unwrap();
    let first = database.capture().unwrap();
    database
        .transaction(|transaction| {
            transaction.execute("INSERT INTO events(body) VALUES ('two')", [])?;
            Ok(())
        })
        .unwrap();
    let second = database.capture().unwrap();
    let first_frame = frame(1, first.segments.first().unwrap(), limits);
    let second_frame = frame(2, second.segments.first().unwrap(), limits);

    let root = tempfile::TempDir::new().unwrap();
    let store = FollowerStore::open(
        root.path().to_owned(),
        limits,
        crab_ltx::DiskBudget::new(1 << 30),
    )
    .unwrap();
    let leader = SessionId::from_bytes([1; 16]);
    assert_eq!(
        store
            .append(leader, 2, vec![first_frame.clone()], 0)
            .await
            .unwrap()
            .durable_through,
        1
    );
    assert_eq!(
        store
            .append(leader, 2, vec![first_frame.clone()], 0)
            .await
            .unwrap()
            .durable_through,
        1
    );

    let open = lane_directory(root.path(), Lane { leader, epoch: 2 })
        .join("chunks")
        .join("open.log");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&open)
        .unwrap()
        .write_all(b"torn")
        .unwrap();
    drop(store);
    let store = FollowerStore::open(
        root.path().to_owned(),
        limits,
        crab_ltx::DiskBudget::new(1 << 30),
    )
    .unwrap();
    assert_eq!(
        std::fs::metadata(&open).unwrap().len(),
        (RECORD_HEADER_BYTES + first_frame.len()) as u64
    );
    assert_eq!(
        store
            .append(leader, 2, vec![second_frame.clone()], 0)
            .await
            .unwrap()
            .durable_through,
        2
    );
    assert_eq!(store.seal(leader, 2).await.unwrap().base_sequence, 1);
    assert_eq!(store.read_tail(leader, 2, 1).await.unwrap().len(), 2);
    let page = store.read_tail_page(leader, 2, 1).await.unwrap();
    assert_eq!(page.frames.len(), 2);
    assert_eq!(page.next_sequence, None);
    assert!(
        store
            .append(leader, 2, vec![second_frame], 0)
            .await
            .is_err()
    );
    std::fs::write(
        lane_directory(root.path(), Lane { leader, epoch: 2 }).join("sealed"),
        b"torn",
    )
    .unwrap();
    assert!(store.read_tail(leader, 2, 1).await.is_err());
    database.close().unwrap();
}
#[tokio::test]
async fn append_skips_an_object_covered_queued_prefix() {
    let limits = crab_ltx::Limits::default();
    let source = tempfile::TempDir::new().unwrap();
    let mut database = Db::open(&source.path().join("cell.sqlite"), limits).unwrap();
    database
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE events(id INTEGER PRIMARY KEY, body TEXT NOT NULL);\
                 INSERT INTO events(body) VALUES ('one')",
            )
        })
        .unwrap();
    let first = database.capture().unwrap();
    database
        .transaction(|transaction| {
            transaction.execute("INSERT INTO events(body) VALUES ('two')", [])?;
            Ok(())
        })
        .unwrap();
    let second = database.capture().unwrap();
    let first_frame = frame(1, first.segments.first().unwrap(), limits);
    let second_frame = frame(2, second.segments.first().unwrap(), limits);

    for covered in [1, 2] {
        let root = tempfile::TempDir::new().unwrap();
        let store = FollowerStore::open(
            root.path().to_owned(),
            limits,
            crab_ltx::DiskBudget::new(1 << 30),
        )
        .unwrap();
        let leader = SessionId::from_bytes([1; 16]);
        let receipt = store
            .append(
                leader,
                2,
                vec![first_frame.clone(), second_frame.clone()],
                covered,
            )
            .await
            .unwrap();
        assert_eq!(receipt.base_sequence, covered + 1);
        assert_eq!(receipt.durable_through, 2);
        let member = crate::identity::NodeId::from_bytes([2; 16]);
        let recovery = crate::node::log_recovery::NodeLogRecovery::new(
            Arc::new(crate::node::log_transport::LocalFollowerTransport::new(
                member, store,
            )),
            crate::identity::NodeId::from_bytes([1; 16]),
            leader,
            2,
            vec![member],
            covered,
            true,
            limits,
        )
        .unwrap();
        let sealed = recovery.ensure_sealed().await.unwrap();
        assert_eq!(sealed.frames.len(), (2 - covered) as usize);
    }
    database.close().unwrap();
}
#[tokio::test]
async fn covered_prefix_is_pruned_from_open_lane_before_restart() {
    let limits = crab_ltx::Limits::default();
    let source = tempfile::TempDir::new().unwrap();
    let mut database = Db::open(&source.path().join("cell.sqlite"), limits).unwrap();
    database
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE events(id INTEGER PRIMARY KEY, body TEXT NOT NULL);\
                 INSERT INTO events(body) VALUES ('one')",
            )
        })
        .unwrap();
    let capture = database.capture().unwrap();
    let frames = [1, 2, 3, 4, 6]
        .into_iter()
        .map(|sequence| frame(sequence, capture.segments.first().unwrap(), limits))
        .collect::<Vec<_>>();

    let root = tempfile::TempDir::new().unwrap();
    let leader = SessionId::from_bytes([1; 16]);
    let store = FollowerStore::open(
        root.path().to_owned(),
        limits,
        crab_ltx::DiskBudget::new(1 << 30),
    )
    .unwrap();
    store
        .append(leader, 2, frames[..4].to_vec(), 0)
        .await
        .unwrap();
    let receipt = store
        .append(leader, 2, vec![frames[4].clone()], 5)
        .await
        .unwrap();
    assert_eq!(receipt.base_sequence, 6);
    assert_eq!(receipt.durable_through, 6);

    drop(store);
    let store = FollowerStore::open(
        root.path().to_owned(),
        limits,
        crab_ltx::DiskBudget::new(1 << 30),
    )
    .unwrap();
    assert_eq!(store.seal(leader, 2).await.unwrap().base_sequence, 6);
    assert_eq!(store.read_tail(leader, 2, 6).await.unwrap().len(), 1);
    database.close().unwrap();
}
#[tokio::test]
async fn append_reserves_capacity_before_writing_and_seal_accounts_for_marker() {
    let limits = crab_ltx::Limits::default();
    let source = tempfile::TempDir::new().unwrap();
    let mut database = Db::open(&source.path().join("cell.sqlite"), limits).unwrap();
    database
        .transaction(|transaction| transaction.execute_batch("CREATE TABLE values_(v)"))
        .unwrap();
    let capture = database.capture().unwrap();
    let encoded = frame(1, capture.segments.first().unwrap(), limits);
    let required = RECORD_HEADER_BYTES as u64 + encoded.len() as u64;
    let leader = SessionId::from_bytes([1; 16]);

    let rejected_root = tempfile::TempDir::new().unwrap();
    let rejected = FollowerStore::open(
        rejected_root.path().to_owned(),
        limits,
        crab_ltx::DiskBudget::new(required - 1),
    )
    .unwrap();
    assert!(
        rejected
            .append(leader, 2, vec![encoded.clone()], 0)
            .await
            .is_err()
    );
    assert_eq!(directory_bytes(rejected_root.path()).unwrap(), 0);

    let root = tempfile::TempDir::new().unwrap();
    let store = FollowerStore::open(
        root.path().to_owned(),
        limits,
        crab_ltx::DiskBudget::new(required + 8),
    )
    .unwrap();
    store.append(leader, 2, vec![encoded], 0).await.unwrap();
    assert_eq!(store.retained_bytes(), required);
    store.seal(leader, 2).await.unwrap();
    assert_eq!(store.retained_bytes(), required + 8);
    assert_eq!(store.available_bytes(), 0);
    database.close().unwrap();
}
#[tokio::test]
async fn retire_requires_full_coverage_and_persists_an_append_fence() {
    let limits = crab_ltx::Limits::default();
    let source = tempfile::TempDir::new().unwrap();
    let mut database = Db::open(&source.path().join("cell.sqlite"), limits).unwrap();
    database
        .transaction(|transaction| transaction.execute_batch("CREATE TABLE values_(v)"))
        .unwrap();
    let capture = database.capture().unwrap();
    let encoded = frame(1, capture.segments.first().unwrap(), limits);
    let required = RECORD_HEADER_BYTES as u64 + encoded.len() as u64;
    let leader = SessionId::from_bytes([1; 16]);
    let root = tempfile::TempDir::new().unwrap();
    let store = FollowerStore::open(
        root.path().to_owned(),
        limits,
        crab_ltx::DiskBudget::new(required + 8),
    )
    .unwrap();
    store
        .append(leader, 2, vec![encoded.clone()], 0)
        .await
        .unwrap();

    assert!(store.retire(leader, 2, 0).await.is_err());
    assert_eq!(store.retained_bytes(), required);
    assert_eq!(
        store.retire(leader, 2, 1).await.unwrap(),
        FollowerReceipt {
            base_sequence: 2,
            durable_through: 1,
        }
    );
    assert_eq!(store.retained_bytes(), 8);
    assert_eq!(store.retire(leader, 2, 1).await.unwrap().durable_through, 1);
    assert!(store.retire(leader, 2, 2).await.is_err());
    assert!(store.append(leader, 2, vec![encoded], 1).await.is_err());
    assert_eq!(store.seal(leader, 2).await.unwrap().durable_through, 1);

    drop(store);
    let reopened =
        FollowerStore::open(root.path().to_owned(), limits, crab_ltx::DiskBudget::new(8)).unwrap();
    assert_eq!(reopened.retained_bytes(), 8);
    assert_eq!(reopened.seal(leader, 2).await.unwrap().durable_through, 1);
    database.close().unwrap();
}
