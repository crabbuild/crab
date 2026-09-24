use super::*;
use crab_ltx::rusqlite::Connection;
use std::collections::BTreeSet;

#[tokio::test]
async fn multipart_publish_is_atomic_conditional_and_range_readable() {
    use crab_storage::Store;
    use object_store::memory::InMemory;
    let artifacts = BlobArtifactStore::new(Store::new(std::sync::Arc::new(InMemory::new())));
    let mut connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch("PRAGMA foreign_keys = ON")
        .unwrap();
    let transaction = connection.transaction().unwrap();
    install_blob_schema(&transaction).unwrap();
    let key = b"artifacts/build.log".to_vec();
    let upload_id = [1; 16];
    assert_eq!(
        blob_mutate(
            &transaction,
            1,
            1,
            &BlobMutation::Begin {
                key: key.clone(),
                upload_id,
                condition: BlobCondition::Missing,
                content_type: Some("text/plain".into()),
                metadata: b"build=42".to_vec(),
                expires_at_ms: 60_001,
            },
        )
        .unwrap(),
        BlobMutationOutcome::Begun
    );
    for (part_number, payload) in [(1, b"hello ".as_slice()), (2, b"world".as_slice())] {
        let digest = part_digest(payload);
        artifacts.put_part(digest, payload).await.unwrap();
        assert!(matches!(
            blob_mutate(
                &transaction,
                2,
                2,
                &BlobMutation::PutPartRef {
                    key: key.clone(),
                    upload_id,
                    part_number,
                    digest,
                    size: payload.len() as u32,
                },
            )
            .unwrap(),
            BlobMutationOutcome::PartStored { .. }
        ));
    }
    let BlobMutationOutcome::Committed { etag, size } = blob_mutate(
        &transaction,
        3,
        3,
        &BlobMutation::Complete {
            key: key.clone(),
            upload_id,
            part_count: 2,
        },
    )
    .unwrap() else {
        panic!("blob did not commit");
    };
    assert_eq!(size, 11);
    let BlobQueryResult::Read(Some(read)) = blob_query(
        &transaction,
        &BlobQuery::Read {
            key: key.clone(),
            offset: 3,
            limit: 5,
        },
    )
    .unwrap() else {
        panic!("blob range was not returned");
    };
    let mut bytes = Vec::new();
    for part in &read.parts {
        let payload = artifacts.read_part(part.digest, part.size).await.unwrap();
        let start = 3_u64.saturating_sub(part.offset) as usize;
        let end = 8_u64.saturating_sub(part.offset).min(u64::from(part.size)) as usize;
        bytes.extend_from_slice(&payload[start..end]);
    }
    assert_eq!(bytes, b"lo wo");
    assert_eq!(read.metadata.etag, etag);
    let BlobQueryResult::List(page) = blob_query(
        &transaction,
        &BlobQuery::List {
            prefix: b"artifacts/".to_vec(),
            after: None,
            limit: 10,
        },
    )
    .unwrap() else {
        panic!("blob list was not returned");
    };
    assert_eq!(page.objects.len(), 1);
    assert_eq!(page.objects[0].key, key);

    assert_eq!(
        blob_mutate(
            &transaction,
            4,
            4,
            &BlobMutation::Delete {
                key,
                condition: BlobCondition::Etag([9; 32]),
            },
        )
        .unwrap(),
        BlobMutationOutcome::Conflict
    );
}

#[tokio::test]
async fn object_store_sweep_keeps_live_parts_and_reclaims_old_orphans() {
    use crab_storage::{GLOBAL_PREFIX, Store, global_content_prefix};
    use object_store::memory::InMemory;

    let store = Store::new(std::sync::Arc::new(InMemory::new()));
    let artifacts = BlobArtifactStore::new(store.clone());
    let live_payload = b"live";
    let orphan_payload = b"orphan";
    let live_digest = part_digest(live_payload);
    let orphan_digest = part_digest(orphan_payload);
    artifacts.put_part(live_digest, live_payload).await.unwrap();
    artifacts
        .put_part(orphan_digest, orphan_payload)
        .await
        .unwrap();

    let report = artifacts
        .sweep_unreferenced(&BTreeSet::from([live_digest]), i64::MAX)
        .await
        .unwrap();
    assert_eq!(report.scanned(), 2);
    assert_eq!(report.deleted(), 1);
    assert!(!report.has_more());

    let objects = store
        .list_prefix(&global_content_prefix(GLOBAL_PREFIX, BLOB_PART_KIND))
        .await
        .unwrap();
    assert_eq!(objects.len(), 1);
    assert!(
        objects[0]
            .location
            .to_string()
            .ends_with(&blake3::Hash::from_bytes(live_digest).to_hex().to_string())
    );
}

#[tokio::test]
async fn object_store_sweep_reaches_orphans_beyond_live_entries() {
    use crab_storage::Store;
    use object_store::memory::InMemory;

    let store = Store::new(std::sync::Arc::new(InMemory::new()));
    let artifacts = BlobArtifactStore::new(store);
    let mut live_digests = BTreeSet::new();
    for index in 0_u32..129 {
        let payload = index.to_be_bytes();
        let digest = part_digest(&payload);
        artifacts.put_part(digest, &payload).await.unwrap();
        live_digests.insert(digest);
    }
    let orphan_payload = b"orphan past the scan budget";
    artifacts
        .put_part(part_digest(orphan_payload), orphan_payload)
        .await
        .unwrap();

    let report = artifacts
        .sweep_unreferenced(&live_digests, i64::MAX)
        .await
        .unwrap();
    assert_eq!(report.scanned(), 130);
    assert_eq!(report.deleted(), 1);
    assert!(!report.has_more());
}

#[tokio::test]
async fn object_store_sweep_bounds_deletions_and_finishes_on_retry() {
    use crab_storage::Store;
    use object_store::memory::InMemory;

    let store = Store::new(std::sync::Arc::new(InMemory::new()));
    let artifacts = BlobArtifactStore::new(store);
    for index in 0_u32..129 {
        let payload = index.to_be_bytes();
        artifacts
            .put_part(part_digest(&payload), &payload)
            .await
            .unwrap();
    }

    let first = artifacts
        .sweep_unreferenced(&BTreeSet::new(), i64::MAX)
        .await
        .unwrap();
    assert_eq!(first.deleted(), MAX_BLOB_GC_DELETIONS);
    assert!(first.has_more());

    let second = artifacts
        .sweep_unreferenced(&BTreeSet::new(), i64::MAX)
        .await
        .unwrap();
    assert_eq!(second.deleted(), 1);
    assert!(!second.has_more());
}

#[test]
fn checked_in_blob_schema_matches_runtime_schema() {
    assert_eq!(
        BLOB_SCHEMA,
        include_str!("../../../docs/contracts/blob.sql")
    );
}

#[test]
fn blob_schema_keeps_part_bytes_out_of_sqlite() {
    let mut connection = Connection::open_in_memory().unwrap();
    let transaction = connection.transaction().unwrap();
    install_blob_schema(&transaction).unwrap();
    let columns = transaction
        .prepare("PRAGMA table_info(blob_parts)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        columns,
        ["upload_id", "part_number", "digest", "size", "byte_offset"]
    );
}

#[test]
fn upload_lifetime_uses_request_issue_time_but_rejects_expired_acceptance() {
    let mut connection = Connection::open_in_memory().unwrap();
    let transaction = connection.transaction().unwrap();
    install_blob_schema(&transaction).unwrap();
    let mutation = BlobMutation::Begin {
        key: b"logs/issue-time".to_vec(),
        upload_id: [2; 16],
        condition: BlobCondition::Missing,
        content_type: None,
        metadata: Vec::new(),
        expires_at_ms: 60_001,
    };

    assert_eq!(
        blob_mutate(&transaction, 1_500, 1, &mutation).unwrap(),
        BlobMutationOutcome::Begun
    );
    assert!(matches!(
        blob_mutate(
            &transaction,
            60_001,
            1,
            &BlobMutation::Begin {
                key: b"logs/issue-time".to_vec(),
                upload_id: [3; 16],
                condition: BlobCondition::Missing,
                content_type: None,
                metadata: Vec::new(),
                expires_at_ms: 60_001,
            },
        ),
        Err(Error::Command("blob upload has already expired"))
    ));
}

fn begin(key: &[u8], metadata: &[u8], expires_at_ms: i64, upload_id: [u8; 16]) -> BlobMutation {
    BlobMutation::Begin {
        key: key.to_vec(),
        upload_id,
        condition: BlobCondition::Missing,
        content_type: None,
        metadata: metadata.to_vec(),
        expires_at_ms,
    }
}

fn blob_connection() -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    let transaction = connection.unchecked_transaction().unwrap();
    install_blob_schema(&transaction).unwrap();
    transaction.commit().unwrap();
    connection
}

#[test]
fn key_bounds_accept_the_documented_range() {
    let mut connection = blob_connection();
    let transaction = connection.transaction().unwrap();
    assert_eq!(
        blob_mutate(&transaction, 1, 1, &begin(b"k", &[], 60_001, [10; 16])).unwrap(),
        BlobMutationOutcome::Begun
    );
    assert_eq!(
        blob_mutate(
            &transaction,
            1,
            1,
            &begin(&[b'k'; 1_024], &[], 60_001, [11; 16])
        )
        .unwrap(),
        BlobMutationOutcome::Begun
    );
}

#[test]
fn key_bounds_reject_empty_and_oversized_keys() {
    let mut connection = blob_connection();
    let transaction = connection.transaction().unwrap();
    assert!(blob_mutate(&transaction, 1, 1, &begin(b"", &[], 60_001, [12; 16])).is_err());
    assert!(
        blob_mutate(
            &transaction,
            1,
            1,
            &begin(&[b'k'; 1_025], &[], 60_001, [13; 16])
        )
        .is_err()
    );
}

#[test]
fn metadata_bounds_accept_exactly_eight_kib() {
    let mut connection = blob_connection();
    let transaction = connection.transaction().unwrap();
    assert_eq!(
        blob_mutate(
            &transaction,
            1,
            1,
            &begin(b"k", &vec![b'm'; 8 * 1_024], 60_001, [14; 16]),
        )
        .unwrap(),
        BlobMutationOutcome::Begun
    );
}

#[test]
fn metadata_bounds_reject_more_than_eight_kib() {
    let mut connection = blob_connection();
    let transaction = connection.transaction().unwrap();
    assert!(
        blob_mutate(
            &transaction,
            1,
            1,
            &begin(b"k", &vec![b'm'; 8 * 1_024 + 1], 60_001, [15; 16]),
        )
        .is_err()
    );
}

#[test]
fn part_size_bounds_reject_more_than_256_kib() {
    let mut connection = blob_connection();
    let transaction = connection.transaction().unwrap();
    blob_mutate(&transaction, 1, 1, &begin(b"k", &[], 60_001, [16; 16])).unwrap();
    assert!(
        blob_mutate(
            &transaction,
            1,
            1,
            &BlobMutation::PutPartRef {
                key: b"k".to_vec(),
                upload_id: [16; 16],
                part_number: 1,
                digest: [17; 32],
                size: MAX_BLOB_PART_BYTES as u32 + 1,
            },
        )
        .is_err()
    );
}

#[test]
fn upload_lifetime_accepts_the_one_minute_and_seven_day_bounds() {
    let mut connection = blob_connection();
    let transaction = connection.transaction().unwrap();
    assert_eq!(
        blob_mutate(&transaction, 1, 1, &begin(b"k", &[], 60_001, [18; 16])).unwrap(),
        BlobMutationOutcome::Begun
    );
    assert_eq!(
        blob_mutate(
            &transaction,
            1,
            1,
            &begin(b"k", &[], 7 * 24 * 60 * 60_000 + 1, [19; 16]),
        )
        .unwrap(),
        BlobMutationOutcome::Begun
    );
}

#[test]
fn upload_lifetime_rejects_below_one_minute_and_past_seven_days() {
    let mut connection = blob_connection();
    let transaction = connection.transaction().unwrap();
    assert!(blob_mutate(&transaction, 1, 1, &begin(b"k", &[], 60_000, [20; 16])).is_err());
    assert!(
        blob_mutate(
            &transaction,
            1,
            1,
            &begin(b"k", &[], 7 * 24 * 60 * 60_000 + 2, [21; 16]),
        )
        .is_err()
    );
}
