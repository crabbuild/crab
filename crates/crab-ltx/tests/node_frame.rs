#![cfg(feature = "replica")]

use bytes::Bytes;
use crab_ltx::{Limits, ManagedDb, NodeFrameScope, encode_node_frame, inspect_node_frame};

fn scope() -> NodeFrameScope {
    NodeFrameScope {
        leader_session: [1; 16],
        log_epoch: 2,
        node_sequence: 3,
        application: [4; 16],
        cell: [5; 32],
        incarnation: [6; 16],
        cell_epoch: 7,
        commit_sequence: 8,
    }
}

#[test]
fn canonical_frame_roundtrips_verified_ltx() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut database =
        ManagedDb::open(&directory.path().join("frame.sqlite"), Limits::default()).unwrap();
    database
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE events(id INTEGER PRIMARY KEY, body TEXT NOT NULL);\
                 INSERT INTO events(body) VALUES ('durable')",
            )
        })
        .unwrap();
    let capture = database.capture().unwrap();
    let segment = capture.segments.first().unwrap();
    let body = Bytes::from(std::fs::read(segment.path()).unwrap());

    let frame = encode_node_frame(
        scope(),
        segment.info().clone(),
        body.clone(),
        Limits::default(),
    )
    .unwrap();
    let decoded = inspect_node_frame(frame.encoded().clone(), Limits::default()).unwrap();

    assert_eq!(decoded.scope(), scope());
    assert_eq!(decoded.segment(), segment.info());
    assert_eq!(decoded.body(), &body);
    assert_eq!(decoded.digest(), frame.digest());
    database.close().unwrap();
}

#[test]
fn frame_rejects_corruption_trailing_bytes_and_zero_scope() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut database =
        ManagedDb::open(&directory.path().join("frame.sqlite"), Limits::default()).unwrap();
    database
        .transaction(|transaction| transaction.execute_batch("CREATE TABLE values_(v)"))
        .unwrap();
    let capture = database.capture().unwrap();
    let segment = capture.segments.first().unwrap();
    let body = Bytes::from(std::fs::read(segment.path()).unwrap());
    let frame = encode_node_frame(
        scope(),
        segment.info().clone(),
        body.clone(),
        Limits::default(),
    )
    .unwrap();

    let mut corrupted = frame.encoded().to_vec();
    let last = corrupted.len() - 1;
    corrupted[last] ^= 1;
    assert!(inspect_node_frame(Bytes::from(corrupted), Limits::default()).is_err());

    let mut trailing = frame.encoded().to_vec();
    trailing.push(0);
    assert!(inspect_node_frame(Bytes::from(trailing), Limits::default()).is_err());

    let mut invalid_scope = scope();
    invalid_scope.leader_session = [0; 16];
    assert!(
        encode_node_frame(
            invalid_scope,
            segment.info().clone(),
            body,
            Limits::default(),
        )
        .is_err()
    );
    database.close().unwrap();
}
