use super::*;
use crab_cell_app::ApplicationHandle;

pub(super) async fn owner(
    writer: &ApplicationHandle<fixture::ReferenceApplication>,
    observer: &ApplicationHandle<fixture::ReferenceApplication>,
) -> BlobExpiryEvidence {
    let blob = writer
        .blob::<fixture::ReferenceBlob>()
        .expect("owner expiring Blob");
    let observed = observer
        .blob::<fixture::ReferenceBlob>()
        .expect("owner Blob expiry observer");
    let issued_at_ms = now_ms() - 25_000;
    let expires_at_ms = issued_at_ms + 60_000;
    let begun = blob
        .mutate(
            identity(BLOB_EXPIRY_OPERATION_ID, issued_at_ms),
            BlobMutation::Begin {
                key: BLOB_EXPIRY_KEY.to_vec(),
                upload_id: fixed_id(BLOB_EXPIRY_OPERATION_ID),
                condition: BlobCondition::Missing,
                content_type: None,
                metadata: Vec::new(),
                expires_at_ms,
            },
        )
        .await
        .expect("owner expiring Blob begin");
    assert_eq!(begun.output, BlobMutationOutcome::Begun);
    let part = blob
        .mutate(
            identity(BLOB_EXPIRY_OPERATION_ID + 1, now_ms()),
            BlobMutation::PutPart {
                key: BLOB_EXPIRY_KEY.to_vec(),
                upload_id: fixed_id(BLOB_EXPIRY_OPERATION_ID),
                part_number: 1,
                payload: BLOB_EXPIRY_OLD_PAYLOAD.to_vec(),
            },
        )
        .await
        .expect("owner expiring Blob part");
    assert!(matches!(
        part.output,
        BlobMutationOutcome::PartStored { .. }
    ));
    let invisible = observed
        .query(
            BlobQuery::Head {
                key: BLOB_EXPIRY_KEY.to_vec(),
            },
            Some(part.receipt),
        )
        .await
        .expect("owner incomplete Blob observation");
    assert_eq!(invisible.output, BlobQueryResult::Head(None));
    assert!(
        now_ms() < expires_at_ms,
        "Blob upload expired before owner kill"
    );
    BlobExpiryEvidence {
        part_sequence: part.receipt.commit_sequence,
        expires_at_ms,
    }
}

pub(super) async fn successor(
    typed: &ApplicationHandle<fixture::ReferenceApplication>,
    sync: &FilePath,
    tenant: TenantId,
    application: ApplicationId,
    evidence: Option<&BlobExpiryEvidence>,
) {
    let blob = typed
        .blob::<fixture::ReferenceBlob>()
        .expect("successor expiring Blob");
    let head = blob
        .query(
            BlobQuery::Head {
                key: BLOB_EXPIRY_KEY.to_vec(),
            },
            None,
        )
        .await
        .expect("successor unpublished Blob head");
    assert_eq!(head.output, BlobQueryResult::Head(None));
    let Some(evidence) = evidence else {
        return;
    };
    assert!(head.receipt.commit_sequence >= evidence.part_sequence);
    let remaining_ms = evidence
        .expires_at_ms
        .saturating_sub(now_ms())
        .max(0)
        .saturating_add(100);
    tokio::time::sleep(Duration::from_millis(
        u64::try_from(remaining_ms).expect("nonnegative Blob expiry wait"),
    ))
    .await;
    assert!(now_ms() > evidence.expires_at_ms);
    let stale = blob
        .mutate(
            identity(BLOB_EXPIRY_OPERATION_ID + 2, now_ms()),
            BlobMutation::Complete {
                key: BLOB_EXPIRY_KEY.to_vec(),
                upload_id: fixed_id(BLOB_EXPIRY_OPERATION_ID),
                part_count: 1,
            },
        )
        .await;
    assert!(matches!(
        stale,
        Err(InvocationError::Rejected(outcome))
            if matches!(outcome.output, BlobMutationOutcome::Conflict | BlobMutationOutcome::NotFound)
    ));
    let after = blob
        .query(
            BlobQuery::Head {
                key: BLOB_EXPIRY_KEY.to_vec(),
            },
            None,
        )
        .await
        .expect("successor expired Blob head");
    assert_eq!(after.output, BlobQueryResult::Head(None));
    let target = CellTarget::new(
        tenant,
        application,
        fixture::BLOB_NAMESPACE,
        &partition_for_shard(0),
    )
    .expect("successor Blob target");
    let tick = typed
        .command::<MaintenanceTickCommand<fixture::ReferenceBlob>>(
            &target,
            identity(BLOB_EXPIRY_OPERATION_ID + 3, now_ms()),
            MaintenanceTickRequest {
                expected_commit_sequence: after.receipt.commit_sequence,
            },
        )
        .await
        .expect("successor expired Blob cleanup Tick");
    assert!(matches!(
        tick.output,
        MaintenanceTickOutcome::Applied { processed } if processed >= 1
    ));
    let old_upload = blob
        .mutate(
            identity(BLOB_EXPIRY_OPERATION_ID + 4, now_ms()),
            BlobMutation::Abort {
                key: BLOB_EXPIRY_KEY.to_vec(),
                upload_id: fixed_id(BLOB_EXPIRY_OPERATION_ID),
            },
        )
        .await;
    assert!(matches!(
        old_upload,
        Err(InvocationError::Rejected(outcome))
            if outcome.output == BlobMutationOutcome::NotFound
    ));
    let issued_at_ms = now_ms();
    let begun = blob
        .mutate(
            identity(BLOB_EXPIRY_OPERATION_ID + 5, issued_at_ms),
            BlobMutation::Begin {
                key: BLOB_EXPIRY_KEY.to_vec(),
                upload_id: fixed_id(BLOB_EXPIRY_OPERATION_ID + 5),
                condition: BlobCondition::Missing,
                content_type: None,
                metadata: Vec::new(),
                expires_at_ms: issued_at_ms + 60_000,
            },
        )
        .await
        .expect("successor fresh Blob begin");
    assert_eq!(begun.output, BlobMutationOutcome::Begun);
    let part = blob
        .mutate(
            identity(BLOB_EXPIRY_OPERATION_ID + 6, now_ms()),
            BlobMutation::PutPart {
                key: BLOB_EXPIRY_KEY.to_vec(),
                upload_id: fixed_id(BLOB_EXPIRY_OPERATION_ID + 5),
                part_number: 1,
                payload: BLOB_EXPIRY_PUBLISHED_PAYLOAD.to_vec(),
            },
        )
        .await
        .expect("successor fresh Blob part");
    assert!(matches!(
        part.output,
        BlobMutationOutcome::PartStored { .. }
    ));
    let committed = blob
        .mutate(
            identity(BLOB_EXPIRY_OPERATION_ID + 7, now_ms()),
            BlobMutation::Complete {
                key: BLOB_EXPIRY_KEY.to_vec(),
                upload_id: fixed_id(BLOB_EXPIRY_OPERATION_ID + 5),
                part_count: 1,
            },
        )
        .await
        .expect("successor fresh Blob completion");
    let BlobMutationOutcome::Committed { etag, size } = committed.output else {
        panic!("successor fresh Blob was not committed");
    };
    let read = blob
        .query(
            BlobQuery::Read {
                key: BLOB_EXPIRY_KEY.to_vec(),
                offset: 0,
                limit: 128,
            },
            Some(committed.receipt),
        )
        .await
        .expect("successor fresh Blob read");
    let BlobQueryResult::Read(Some(read)) = read.output else {
        panic!("successor fresh Blob is absent");
    };
    assert_eq!(read.bytes, BLOB_EXPIRY_PUBLISHED_PAYLOAD);
    assert_eq!(read.metadata.etag, etag);
    assert_eq!(read.metadata.size, size);
    publish_marker(
        sync,
        "blob-expiry-publication",
        &serde_json::to_vec(&BlobExpiryPublication {
            sequence: committed.receipt.commit_sequence,
            etag,
            size,
        })
        .expect("encode successor Blob publication"),
    );
}

pub(super) async fn observer(
    typed: &ApplicationHandle<fixture::ReferenceApplication>,
    sync: &FilePath,
    evidence: Option<&BlobExpiryEvidence>,
) {
    let blob = typed
        .blob::<fixture::ReferenceBlob>()
        .expect("observer expiring Blob");
    let Some(evidence) = evidence else {
        let head = blob
            .query(
                BlobQuery::Head {
                    key: BLOB_EXPIRY_KEY.to_vec(),
                },
                None,
            )
            .await
            .expect("observer absent Blob expiry key");
        assert_eq!(head.output, BlobQueryResult::Head(None));
        return;
    };
    let publication = std::fs::read(sync.join("blob-expiry-publication"))
        .expect("successor Blob publication evidence");
    let publication: BlobExpiryPublication =
        serde_json::from_slice(&publication).expect("decode successor Blob publication");
    let observed = blob
        .query(
            BlobQuery::Read {
                key: BLOB_EXPIRY_KEY.to_vec(),
                offset: 0,
                limit: 128,
            },
            None,
        )
        .await
        .expect("observer published Blob read");
    let BlobQueryResult::Read(Some(read)) = observed.output else {
        panic!("observer published Blob is absent");
    };
    assert_eq!(read.bytes, BLOB_EXPIRY_PUBLISHED_PAYLOAD);
    assert_eq!(read.metadata.etag, publication.etag);
    assert_eq!(read.metadata.size, publication.size);
    assert_eq!(read.metadata.part_count, 1);
    assert!(observed.receipt.commit_sequence >= publication.sequence);
    assert!(publication.sequence > evidence.part_sequence);
    let old_upload = blob
        .mutate(
            identity(BLOB_EXPIRY_OPERATION_ID + 8, now_ms()),
            BlobMutation::Abort {
                key: BLOB_EXPIRY_KEY.to_vec(),
                upload_id: fixed_id(BLOB_EXPIRY_OPERATION_ID),
            },
        )
        .await;
    assert!(matches!(
        old_upload,
        Err(InvocationError::Rejected(outcome))
            if outcome.output == BlobMutationOutcome::NotFound
    ));
}
