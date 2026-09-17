use super::*;
use crab_ltx::{ManagedDb, NodeFrameScope, encode_node_frame};

fn frame(sequence: u64, segment: &crab_ltx::LocalSegment, limits: crab_ltx::Limits) -> Bytes {
    encode_node_frame(
        NodeFrameScope {
            leader_session: [1; 16],
            log_epoch: 2,
            node_sequence: sequence,
            application: [3; 16],
            cell: [4; 32],
            incarnation: [5; 16],
            cell_epoch: 6,
            commit_sequence: sequence,
        },
        segment.info().clone(),
        Bytes::from(std::fs::read(segment.path()).unwrap()),
        limits,
    )
    .unwrap()
    .encoded()
    .clone()
}

#[tokio::test]
async fn append_recovers_torn_suffix_deduplicates_and_seals() {
    let limits = crab_ltx::Limits::default();
    let source = tempfile::TempDir::new().unwrap();
    let mut database = ManagedDb::open(&source.path().join("cell.sqlite"), limits).unwrap();
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
    let store = FollowerStore::open(root.path().to_owned(), limits).unwrap();
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
        .open(open)
        .unwrap()
        .write_all(b"torn")
        .unwrap();
    drop(store);
    let store = FollowerStore::open(root.path().to_owned(), limits).unwrap();
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
async fn closed_chunk_name_must_match_verified_record_range() {
    let limits = crab_ltx::Limits::default();
    let source = tempfile::TempDir::new().unwrap();
    let mut database = ManagedDb::open(&source.path().join("cell.sqlite"), limits).unwrap();
    database
        .transaction(|transaction| transaction.execute_batch("CREATE TABLE values_(v)"))
        .unwrap();
    let capture = database.capture().unwrap();
    let leader = SessionId::from_bytes([1; 16]);
    let root = tempfile::TempDir::new().unwrap();
    let store = FollowerStore::open(root.path().to_owned(), limits).unwrap();
    store
        .append(
            leader,
            2,
            vec![frame(1, capture.segments.first().unwrap(), limits)],
            0,
        )
        .await
        .unwrap();
    let chunks = lane_directory(root.path(), Lane { leader, epoch: 2 }).join("chunks");
    drop(store);
    std::fs::rename(
        chunks.join("open.log"),
        chunks.join("00000000000000000002-00000000000000000002.log"),
    )
    .unwrap();
    let reopened = FollowerStore::open(root.path().to_owned(), limits).unwrap();
    assert!(reopened.seal(leader, 2).await.is_err());
    database.close().unwrap();
}

#[tokio::test]
async fn conflicting_duplicate_and_sequence_gap_fail_closed() {
    let limits = crab_ltx::Limits::default();
    let source = tempfile::TempDir::new().unwrap();
    let mut database = ManagedDb::open(&source.path().join("cell.sqlite"), limits).unwrap();
    database
        .transaction(|transaction| transaction.execute_batch("CREATE TABLE values_(v)"))
        .unwrap();
    let first = database.capture().unwrap();
    database
        .transaction(|transaction| {
            transaction.execute("INSERT INTO values_ VALUES (1)", [])?;
            Ok(())
        })
        .unwrap();
    let second = database.capture().unwrap();
    let root = tempfile::TempDir::new().unwrap();
    let store = FollowerStore::open(root.path().to_owned(), limits).unwrap();
    let leader = SessionId::from_bytes([1; 16]);
    store
        .append(
            leader,
            2,
            vec![frame(1, first.segments.first().unwrap(), limits)],
            0,
        )
        .await
        .unwrap();
    assert!(
        store
            .append(
                leader,
                2,
                vec![frame(1, second.segments.first().unwrap(), limits)],
                0,
            )
            .await
            .is_err()
    );
    assert!(
        store
            .append(
                leader,
                2,
                vec![frame(3, second.segments.first().unwrap(), limits)],
                0,
            )
            .await
            .is_err()
    );
    database.close().unwrap();
}
