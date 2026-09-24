//! Tail pages, cached and indexed tails, and fail-closed scans.

use super::*;

#[tokio::test]
async fn restarted_lane_returns_only_the_requested_large_frame_page() {
    let limits = crab_ltx::Limits::default();
    let source = tempfile::TempDir::new().unwrap();
    let mut database = Db::open(&source.path().join("source.sqlite"), limits).unwrap();
    database
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE values_(v); INSERT INTO values_ VALUES(randomblob(2097152))",
            )
        })
        .unwrap();
    let capture = database.capture().unwrap();
    let root = tempfile::TempDir::new().unwrap();
    let leader = SessionId::from_bytes([1; 16]);
    let store = FollowerStore::open(
        root.path().to_owned(),
        limits,
        crab_ltx::DiskBudget::new(1 << 30),
    )
    .unwrap();
    for sequence in 1..=16 {
        store
            .append(
                leader,
                2,
                vec![frame(sequence, &capture.segments[0], limits)],
                0,
            )
            .await
            .unwrap();
    }
    store.seal(leader, 2).await.unwrap();
    drop(store);
    let store = FollowerStore::open(
        root.path().to_owned(),
        limits,
        crab_ltx::DiskBudget::new(1 << 30),
    )
    .unwrap();
    for sequence in [1, 16] {
        let page = store.read_tail_page(leader, 2, sequence).await.unwrap();
        assert_eq!(page.frames.len(), 1);
        assert_eq!(page.next_sequence, (sequence < 16).then_some(sequence + 1));
        let frame = crab_ltx::inspect_node_frame(page.frames[0].clone(), limits).unwrap();
        assert_eq!(frame.scope().node_sequence, sequence);
    }
    assert_eq!(store.scan_count(), 1);
    database.close().unwrap();
}
#[tokio::test]
async fn cached_tail_rechecks_mutated_frame_bytes() {
    let limits = crab_ltx::Limits::default();
    let source = tempfile::TempDir::new().unwrap();
    let mut database = Db::open(&source.path().join("source.sqlite"), limits).unwrap();
    database
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE values_(v); INSERT INTO values_ VALUES(randomblob(2097152))",
            )
        })
        .unwrap();
    let capture = database.capture().unwrap();
    let root = tempfile::TempDir::new().unwrap();
    let leader = SessionId::from_bytes([1; 16]);
    let store = FollowerStore::open(
        root.path().to_owned(),
        limits,
        crab_ltx::DiskBudget::new(1 << 30),
    )
    .unwrap();
    for sequence in 1..=2 {
        store
            .append(
                leader,
                2,
                vec![frame(sequence, &capture.segments[0], limits)],
                0,
            )
            .await
            .unwrap();
    }
    store.seal(leader, 2).await.unwrap();
    drop(store);
    let store = FollowerStore::open(
        root.path().to_owned(),
        limits,
        crab_ltx::DiskBudget::new(1 << 30),
    )
    .unwrap();
    assert_eq!(
        store
            .read_tail_page(leader, 2, 1)
            .await
            .unwrap()
            .next_sequence,
        Some(2)
    );
    let chunks = lane_directory(root.path(), Lane { leader, epoch: 2 }).join("chunks");
    let chunk = std::fs::read_dir(chunks)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().and_then(|value| value.to_str()) == Some("log"))
        .unwrap();
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(chunk)
        .unwrap();
    file.seek(SeekFrom::End(-1)).unwrap();
    let mut byte = [0_u8; 1];
    file.read_exact(&mut byte).unwrap();
    file.seek(SeekFrom::End(-1)).unwrap();
    byte[0] ^= 0xff;
    file.write_all(&byte).unwrap();
    file.sync_data().unwrap();
    assert!(store.read_tail_page(leader, 2, 2).await.is_err());
    assert_eq!(store.scan_count(), 1);
    database.close().unwrap();
}
#[tokio::test]
async fn cached_tail_fails_when_its_chunk_is_deleted() {
    let limits = crab_ltx::Limits::default();
    let source = tempfile::TempDir::new().unwrap();
    let mut database = Db::open(&source.path().join("source.sqlite"), limits).unwrap();
    database
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE values_(v); INSERT INTO values_ VALUES(randomblob(2097152))",
            )
        })
        .unwrap();
    let capture = database.capture().unwrap();
    let root = tempfile::TempDir::new().unwrap();
    let leader = SessionId::from_bytes([1; 16]);
    let store = FollowerStore::open(
        root.path().to_owned(),
        limits,
        crab_ltx::DiskBudget::new(1 << 30),
    )
    .unwrap();
    for sequence in 1..=2 {
        store
            .append(
                leader,
                2,
                vec![frame(sequence, &capture.segments[0], limits)],
                0,
            )
            .await
            .unwrap();
    }
    store.seal(leader, 2).await.unwrap();
    drop(store);
    let store = FollowerStore::open(
        root.path().to_owned(),
        limits,
        crab_ltx::DiskBudget::new(1 << 30),
    )
    .unwrap();
    assert_eq!(
        store
            .read_tail_page(leader, 2, 1)
            .await
            .unwrap()
            .next_sequence,
        Some(2)
    );
    let chunks = lane_directory(root.path(), Lane { leader, epoch: 2 }).join("chunks");
    let chunk = std::fs::read_dir(chunks)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().and_then(|value| value.to_str()) == Some("log"))
        .unwrap();
    std::fs::remove_file(chunk).unwrap();
    assert!(store.read_tail_page(leader, 2, 2).await.is_err());
    assert_eq!(store.scan_count(), 1);
    database.close().unwrap();
}
#[tokio::test]
async fn indexed_tail_revalidates_headers_and_rebuilds_after_disk_mutation() {
    let limits = crab_ltx::Limits::default();
    let source = tempfile::TempDir::new().unwrap();
    let mut database = Db::open(&source.path().join("source.sqlite"), limits).unwrap();
    database
        .transaction(|transaction| {
            transaction.execute_batch("CREATE TABLE values_(v); INSERT INTO values_ VALUES (1)")
        })
        .unwrap();
    let capture = database.capture().unwrap();
    let encoded = frame(1, capture.segments.first().unwrap(), limits);
    let root = tempfile::TempDir::new().unwrap();
    let leader = SessionId::from_bytes([1; 16]);
    let store = FollowerStore::open(
        root.path().to_owned(),
        limits,
        crab_ltx::DiskBudget::new(1 << 30),
    )
    .unwrap();
    store.append(leader, 2, vec![encoded], 0).await.unwrap();
    store.seal(leader, 2).await.unwrap();
    assert_eq!(
        store
            .read_tail_page(leader, 2, 1)
            .await
            .unwrap()
            .frames
            .len(),
        1
    );

    let open = lane_directory(root.path(), Lane { leader, epoch: 2 })
        .join("chunks")
        .join("open.log");
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&open)
        .unwrap();
    use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
    file.seek(SeekFrom::Start(20)).unwrap();
    let mut original = [0_u8; 1];
    file.read_exact(&mut original).unwrap();
    file.seek(SeekFrom::Start(20)).unwrap();
    file.write_all(&[original[0] ^ 1]).unwrap();
    file.sync_data().unwrap();
    assert!(store.read_tail_page(leader, 2, 1).await.is_err());

    file.seek(SeekFrom::Start(20)).unwrap();
    file.write_all(&original).unwrap();
    file.sync_data().unwrap();
    assert_eq!(
        store
            .read_tail_page(leader, 2, 1)
            .await
            .unwrap()
            .frames
            .len(),
        1
    );
    database.close().unwrap();
}
#[tokio::test]
async fn tail_read_falls_back_to_scan_when_index_budget_is_exhausted() {
    let limits = crab_ltx::Limits::default();
    let source = tempfile::TempDir::new().unwrap();
    let mut database = Db::open(&source.path().join("source.sqlite"), limits).unwrap();
    database
        .transaction(|transaction| transaction.execute_batch("CREATE TABLE values_(v)"))
        .unwrap();
    let capture = database.capture().unwrap();
    let encoded = frame(1, capture.segments.first().unwrap(), limits);
    let root = tempfile::TempDir::new().unwrap();
    let leader = SessionId::from_bytes([1; 16]);
    let store = FollowerStore::open(
        root.path().to_owned(),
        limits,
        crab_ltx::DiskBudget::new(1 << 30),
    )
    .unwrap();
    store.append(leader, 2, vec![encoded], 0).await.unwrap();
    store.seal(leader, 2).await.unwrap();
    drop(store);
    let store = FollowerStore::open(
        root.path().to_owned(),
        limits,
        crab_ltx::DiskBudget::new(1 << 30),
    )
    .unwrap();
    *store.index_used.lock().unwrap() = MAX_FOLLOWER_INDEX_BYTES;

    let page = store.read_tail_page(leader, 2, 1).await.unwrap();
    assert_eq!(page.frames.len(), 1);
    assert_eq!(page.next_sequence, None);
    database.close().unwrap();
}
#[tokio::test]
async fn closed_chunk_name_must_match_verified_record_range() {
    let limits = crab_ltx::Limits::default();
    let source = tempfile::TempDir::new().unwrap();
    let mut database = Db::open(&source.path().join("cell.sqlite"), limits).unwrap();
    database
        .transaction(|transaction| transaction.execute_batch("CREATE TABLE values_(v)"))
        .unwrap();
    let capture = database.capture().unwrap();
    let leader = SessionId::from_bytes([1; 16]);
    let root = tempfile::TempDir::new().unwrap();
    let store = FollowerStore::open(
        root.path().to_owned(),
        limits,
        crab_ltx::DiskBudget::new(1 << 30),
    )
    .unwrap();
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
    let reopened = FollowerStore::open(
        root.path().to_owned(),
        limits,
        crab_ltx::DiskBudget::new(1 << 30),
    )
    .unwrap();
    assert_eq!(reopened.quarantined_entries(), 1);
    assert!(!lane_directory(root.path(), Lane { leader, epoch: 2 }).exists());
    assert!(root.path().join(FOLLOWER_QUARANTINE).exists());
    assert!(reopened.seal(leader, 2).await.is_err());
    database.close().unwrap();
}
#[tokio::test]
async fn conflicting_duplicate_and_sequence_gap_fail_closed() {
    let limits = crab_ltx::Limits::default();
    let source = tempfile::TempDir::new().unwrap();
    let mut database = Db::open(&source.path().join("cell.sqlite"), limits).unwrap();
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
    let store = FollowerStore::open(
        root.path().to_owned(),
        limits,
        crab_ltx::DiskBudget::new(1 << 30),
    )
    .unwrap();
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
    let retained_before_rejected_append = store.retained_bytes();
    let available_before_rejected_append = store.available_bytes();
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
    assert_eq!(
        store.retained_bytes(),
        retained_before_rejected_append,
        "a rejected append must not leak disk admission"
    );
    assert_eq!(
        store.available_bytes(),
        available_before_rejected_append,
        "a rejected append must restore shared capacity"
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
    assert_eq!(store.retained_bytes(), retained_before_rejected_append);
    assert_eq!(store.available_bytes(), available_before_rejected_append);
    database.close().unwrap();
}
