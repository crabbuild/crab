//! Session listing, claims, collection, and withdrawal.

use super::*;

#[tokio::test]
async fn stable_follower_node_resolves_a_new_session_for_old_log_recovery() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    let leader = SessionId::from_bytes([1; 16]);
    let old_follower = SessionId::from_bytes([2; 16]);
    let restarted_follower = SessionId::from_bytes([3; 16]);
    let claimant = SessionId::from_bytes([4; 16]);
    let follower_node = NodeId::from_bytes([9; 16]);
    let capacity = NodeCapacity {
        free_memory_bytes: 1_000,
        free_disk_bytes: 2_000,
        follower_free_bytes: 2_000,
        follower_retained_bytes: 0,
        job_credits: 3,
        log_protocol: NODE_LOG_PROTOCOL_VERSION,
    };
    let leader_record = directory
        .create(advertisement_for(leader, &key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    directory
        .create(
            advertisement_for_node_capacity(follower_node, old_follower, &key, 1, NOW_MS, capacity),
            NOW_MS,
        )
        .await
        .unwrap();
    let enrolled = directory
        .recruit_log(&leader_record, 7, 1, 2, NOW_MS + 1)
        .await
        .unwrap();
    directory.activate_log(&enrolled, NOW_MS + 2).await.unwrap();

    directory
        .create(
            advertisement_for_node_capacity(
                follower_node,
                restarted_follower,
                &key,
                1,
                NOW_MS + 10_000,
                capacity,
            ),
            NOW_MS + 10_000,
        )
        .await
        .unwrap();
    directory
        .create(
            advertisement_for(claimant, &key, 1, NOW_MS + 10_000),
            NOW_MS + 10_000,
        )
        .await
        .unwrap();
    let fenced = directory
        .claim_expired(leader, claimant, NOW_MS + 10_000)
        .await
        .unwrap();

    assert_eq!(fenced.log().unwrap().members(), [follower_node]);
    assert_eq!(
        directory
            .resolve_node(follower_node, NOW_MS + 10_001)
            .await
            .unwrap()
            .unwrap()
            .session(),
        restarted_follower
    );
    directory
        .authorize_log_recovery(leader, claimant, follower_node, 7, NOW_MS + 10_001)
        .await
        .unwrap();
}

#[tokio::test]
async fn operational_inspection_preserves_expired_lease_evidence_without_reviving_it() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    let session = SessionId::from_bytes([8; 16]);
    directory
        .create(advertisement_for(session, &key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();

    let expired_at = NOW_MS + 10_000;
    assert!(!directory.is_live(session, expired_at).await.unwrap());
    let inspected = directory
        .inspect_advertisement(session, expired_at)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inspected.session(), session);
    assert_eq!(inspected.expires_at_ms(), expired_at);
    assert!(directory.load(session, expired_at).await.is_err());
}

#[tokio::test]
async fn overlapping_live_sessions_for_one_node_fail_closed() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    let stable = NodeId::from_bytes([8; 16]);
    let capacity = NodeCapacity {
        free_memory_bytes: 1_000,
        free_disk_bytes: 2_000,
        follower_free_bytes: 2_000,
        follower_retained_bytes: 0,
        job_credits: 3,
        log_protocol: NODE_LOG_PROTOCOL_VERSION,
    };
    for session in [
        SessionId::from_bytes([1; 16]),
        SessionId::from_bytes([2; 16]),
    ] {
        directory
            .create(
                advertisement_for_node_capacity(stable, session, &key, 1, NOW_MS, capacity),
                NOW_MS,
            )
            .await
            .unwrap();
    }

    assert!(matches!(
        directory.live(NOW_MS + 1, 2).await,
        Err(Error::Node(_))
    ));
    assert!(matches!(
        directory.resolve_node(stable, NOW_MS + 1).await,
        Err(Error::Node(_))
    ));
}

#[tokio::test]
async fn live_listing_is_sorted_bounded_and_ignores_expired_sessions() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    for session in [
        SessionId::from_bytes([8; 16]),
        SessionId::from_bytes([1; 16]),
    ] {
        directory
            .create(advertisement_for(session, &key, 1, NOW_MS), NOW_MS)
            .await
            .unwrap();
    }

    let live = directory.live(NOW_MS + 1, 2).await.unwrap();
    assert_eq!(
        live.iter()
            .map(NodeAdvertisement::session)
            .collect::<Vec<_>>(),
        [
            SessionId::from_bytes([1; 16]),
            SessionId::from_bytes([8; 16])
        ]
    );
    assert!(matches!(
        directory.live(NOW_MS + 1, 1).await,
        Err(Error::Node(_))
    ));
    assert!(directory.live(NOW_MS + 10_000, 1).await.unwrap().is_empty());
}

#[tokio::test]
async fn maintenance_inventory_retains_expired_session_until_withdrawal() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    let session = SessionId::from_bytes([1; 16]);
    let observed = directory
        .create(advertisement(&key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    assert!(directory.live(NOW_MS + 10_000, 1).await.unwrap().is_empty());
    assert_eq!(
        directory
            .advertised_sessions(NOW_MS + 10_000, 1)
            .await
            .unwrap(),
        [session]
    );
    directory
        .withdraw(&observed, NOW_MS + 10_001)
        .await
        .unwrap();
    assert!(
        directory
            .advertised_sessions(NOW_MS + 10_001, 1)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn expired_session_claim_is_atomic_idempotent_and_blocks_refresh() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    let session = SessionId::from_bytes([1; 16]);
    let claimant = SessionId::from_bytes([8; 16]);
    let observed = directory
        .create(advertisement(&key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    directory
        .create(
            advertisement_for(claimant, &key, 1, NOW_MS + 9_000),
            NOW_MS + 9_000,
        )
        .await
        .unwrap();
    assert!(
        directory
            .claim_expired(session, claimant, NOW_MS + 9_999)
            .await
            .is_err()
    );

    let fenced = directory
        .claim_expired(session, claimant, NOW_MS + 10_000)
        .await
        .unwrap();
    assert_eq!(fenced.session(), session);
    assert_eq!(
        directory
            .claim_expired(session, claimant, NOW_MS + 10_001)
            .await
            .unwrap(),
        fenced
    );
    assert!(
        directory
            .claim_expired(session, SessionId::from_bytes([9; 16]), NOW_MS + 10_001,)
            .await
            .is_err()
    );
    assert!(
        directory
            .refresh(
                &observed,
                advertisement(&key, 2, NOW_MS + 10_001),
                NOW_MS + 10_001,
            )
            .await
            .is_err()
    );
    assert!(
        directory
            .withdraw(&observed, NOW_MS + 10_001)
            .await
            .is_err()
    );
    assert_eq!(
        directory
            .claim_expired(session, claimant, NOW_MS + 10_002)
            .await
            .unwrap(),
        fenced
    );
}

#[tokio::test]
async fn stale_collection_fences_records_after_the_clock_skew_horizon() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    let stale = SessionId::from_bytes([1; 16]);
    let current = SessionId::from_bytes([8; 16]);
    directory
        .create(advertisement_for(stale, &key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    let collection_ms = NOW_MS + 10_000 + STALE_ADVERTISEMENT_RETENTION_MS;
    directory
        .create(
            advertisement_for(current, &key, 1, collection_ms),
            collection_ms,
        )
        .await
        .unwrap();

    assert_eq!(
        directory.collect_stale(collection_ms - 1, 1).await.unwrap(),
        0
    );
    assert_eq!(directory.collect_stale(collection_ms, 1).await.unwrap(), 1);
    assert!(directory.load_canonical(stale).await.unwrap().is_none());
    assert!(
        !directory
            .advertised_sessions(collection_ms, 2)
            .await
            .unwrap()
            .contains(&stale)
    );
    assert!(directory.is_live(current, collection_ms + 1).await.unwrap());
}

#[tokio::test]
async fn stale_collection_preserves_a_session_refreshed_before_fencing() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    let created = directory
        .create(advertisement(&key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    let collection_ms = NOW_MS + 10_000 + STALE_ADVERTISEMENT_RETENTION_MS;
    directory
        .refresh(
            &created,
            advertisement(&key, 2, collection_ms),
            collection_ms,
        )
        .await
        .unwrap();

    assert_eq!(directory.collect_stale(collection_ms, 1).await.unwrap(), 0);
    assert!(
        directory
            .is_live(SessionId::from_bytes([1; 16]), collection_ms + 1)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn withdrawal_removes_only_the_exact_observed_advertisement() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    let session = SessionId::from_bytes([1; 16]);
    let created = directory
        .create(advertisement(&key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    directory.withdraw(&created, NOW_MS + 1).await.unwrap();
    assert!(directory.load_canonical(session).await.unwrap().is_none());

    let replacement = SessionId::from_bytes([2; 16]);
    let created = directory
        .create(
            advertisement_for(replacement, &key, 1, NOW_MS + 20_000),
            NOW_MS + 20_000,
        )
        .await
        .unwrap();
    let refreshed = directory
        .refresh(
            &created,
            advertisement_for(replacement, &key, 2, NOW_MS + 21_000),
            NOW_MS + 21_000,
        )
        .await
        .unwrap();
    assert!(directory.withdraw(&created, NOW_MS + 21_001).await.is_err());
    assert_eq!(
        directory
            .load(replacement, NOW_MS + 21_001)
            .await
            .unwrap()
            .unwrap()
            .advertisement(),
        refreshed.advertisement()
    );
}

#[tokio::test]
async fn live_listing_rejects_misplaced_or_foreign_active_records() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let subject = directory();
    let advertisement = advertisement(&key, 1, NOW_MS);
    subject
        .layout
        .store()
        .put_overwrite(
            &subject
                .layout
                .node_directory_path()
                .join("ffffffffffffffffffffffffffffffff.json"),
            advertisement.encode().unwrap().into(),
        )
        .await
        .unwrap();
    assert!(matches!(
        subject.live(NOW_MS + 1, 2).await,
        Err(Error::Node(_))
    ));

    let foreign = NodeAdvertisement::sign(
        NodeId::from_bytes([9; 16]),
        SessionId::from_bytes([9; 16]),
        "https://node-1.internal:8789".into(),
        Digest::from_bytes([10; 32]),
        Digest::from_bytes([3; 32]),
        Digest::from_bytes([4; 32]),
        Digest::from_bytes([5; 32]),
        &key,
        1,
        NOW_MS,
        NOW_MS + 10_000,
        vec![Digest::from_bytes([6; 32])],
        vec![1],
        NodeFailureDomain::default(),
        NodeCapacity {
            free_memory_bytes: 1,
            free_disk_bytes: 1,
            job_credits: 1,
            ..NodeCapacity::default()
        },
    )
    .unwrap();
    let clean = directory();
    clean
        .layout
        .store()
        .put_overwrite(
            &clean.layout.node_path(foreign.session().as_bytes()),
            foreign.encode().unwrap().into(),
        )
        .await
        .unwrap();
    assert!(
        clean
            .is_live(SessionId::from_bytes([9; 16]), NOW_MS + 1)
            .await
            .is_err()
    );
    assert!(matches!(
        clean.live(NOW_MS + 1, 2).await,
        Err(Error::Node(_))
    ));
}
