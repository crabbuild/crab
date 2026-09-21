use super::*;
use crab_ltx::rusqlite::Connection;

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

#[test]
fn checked_in_blob_schema_matches_runtime_schema() {
    assert_eq!(BLOB_SCHEMA, include_str!("../../docs/contracts/blob.sql"));
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
