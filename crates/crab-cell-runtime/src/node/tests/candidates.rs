//! Recovery-candidate windows and scan snapshots.

use super::*;

#[test]
fn recovery_candidate_window_rotates_without_growing_with_directory_size() {
    let sessions = [
        SessionId::from_bytes([1; 16]),
        SessionId::from_bytes([2; 16]),
        SessionId::from_bytes([3; 16]),
        SessionId::from_bytes([4; 16]),
    ];
    let mut first = RecoveryCandidateWindow::with_start([2; 16], 2);
    for session in sessions {
        first.push(session);
    }
    assert_eq!(
        first.finish(),
        [
            SessionId::from_bytes([2; 16]),
            SessionId::from_bytes([3; 16])
        ]
    );

    let mut wrapped = RecoveryCandidateWindow::with_start([4; 16], 2);
    for session in sessions {
        wrapped.push(session);
    }
    assert_eq!(
        wrapped.finish(),
        [
            SessionId::from_bytes([4; 16]),
            SessionId::from_bytes([3; 16])
        ]
    );
}

#[tokio::test]
async fn cloned_directories_share_only_a_fresh_recovery_scan_snapshot() {
    let directory = directory();
    let clone = directory.clone();
    let first = directory
        .recovery_scan_snapshot(NOW_MS, false)
        .await
        .unwrap();
    let reused = clone
        .recovery_scan_snapshot(NOW_MS + 1, false)
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&first, &reused));

    let refreshed = clone
        .recovery_scan_snapshot(NOW_MS + RECOVERY_SCAN_CACHE_TTL_MS, false)
        .await
        .unwrap();
    assert!(!Arc::ptr_eq(&first, &refreshed));
}

#[tokio::test]
async fn empty_live_node_snapshot_is_reused_within_its_ttl() {
    let directory = directory();
    let first = directory
        .recovery_scan_snapshot(NOW_MS, true)
        .await
        .unwrap();
    assert!(first.live_nodes.is_empty());
    let reused = directory
        .recovery_scan_snapshot(NOW_MS + 1, true)
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&first, &reused));
}

#[test]
fn recovery_candidate_snapshot_rechecks_claim_and_expiry() {
    let record = RecoveryCandidateRecord {
        session: SessionId::from_bytes([1; 16]),
        expires_at_ms: NOW_MS + 10,
        claimant: Some(SessionId::from_bytes([2; 16])),
        claim_expires_at_ms: Some(NOW_MS + 20),
        active: true,
        phase: NodeLogPhase::Recovering,
        members: Vec::new(),
    };
    let other = SessionId::from_bytes([3; 16]);
    assert!(!record.eligible_for(other, NOW_MS + 15));
    assert!(record.eligible_for(other, NOW_MS + 20));
    assert!(record.eligible_for(record.claimant.unwrap(), NOW_MS + 15));
}

#[tokio::test]
async fn recovery_claim_rechecks_signed_admission_before_fencing() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    let leader = SessionId::from_bytes([1; 16]);
    let claimant = SessionId::from_bytes([2; 16]);
    directory
        .create(advertisement_for(leader, &key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    directory
        .create(
            advertisement_for_capacity(claimant, &key, 1, NOW_MS + 1, NodeCapacity::default()),
            NOW_MS + 1,
        )
        .await
        .unwrap();

    assert!(matches!(
        directory
            .claim_expired_for_recovery(leader, claimant, NOW_MS + 10_000)
            .await,
        Err(Error::Capacity("node recovery claimant is not eligible"))
    ));
    assert!(
        directory
            .takeover_proof(leader, claimant, NOW_MS + 10_000)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn expired_enrolled_log_becomes_a_renewable_recovery_claim() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    let leader = SessionId::from_bytes([1; 16]);
    let member = SessionId::from_bytes([2; 16]);
    let claimant = SessionId::from_bytes([3; 16]);
    let created = directory
        .create(advertisement_for(leader, &key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    let member_record = directory
        .create(advertisement_for(member, &key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    let enrolled = directory
        .recruit_log(&created, 7, 1, 2, NOW_MS + 1)
        .await
        .unwrap();
    directory.activate_log(&enrolled, NOW_MS + 2).await.unwrap();
    let claimant_record = directory
        .create(advertisement_for(claimant, &key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    directory
        .refresh(
            &member_record,
            advertisement_for(member, &key, 2, NOW_MS + 9_000),
            NOW_MS + 9_000,
        )
        .await
        .unwrap();
    directory
        .refresh(
            &claimant_record,
            advertisement_for(claimant, &key, 2, NOW_MS + 9_000),
            NOW_MS + 9_000,
        )
        .await
        .unwrap();

    assert_eq!(
        directory
            .recovery_candidates(claimant, NOW_MS + 10_000, 2)
            .await
            .unwrap(),
        [leader]
    );
    let fenced = directory
        .claim_expired(leader, claimant, NOW_MS + 10_000)
        .await
        .unwrap();
    let log = fenced.log().unwrap();
    assert_eq!(log.phase(), NodeLogPhase::Recovering);
    assert_eq!(log.recovery().unwrap().claimant(), claimant);
    assert!(matches!(
        fenced.direct_takeover(),
        Err(Error::PendingPublication)
    ));
    directory
        .authorize_log_recovery(leader, claimant, node(member), 7, NOW_MS + 10_001)
        .await
        .unwrap();
    assert!(
        directory
            .authorize_log_append(leader, node(member), 7, 0, NOW_MS + 10_001)
            .await
            .is_err()
    );

    let renewed = directory
        .refresh_recovery_claim(&fenced, NOW_MS + 15_000)
        .await
        .unwrap();
    assert_eq!(renewed.claim_generation(), fenced.claim_generation());
    assert!(renewed.claim_expires_at_ms() > fenced.claim_expires_at_ms());
    let sealed = directory
        .seal_recovery(&renewed, None, NOW_MS + 15_001)
        .await
        .unwrap();
    assert_eq!(sealed.log().phase(), NodeLogPhase::Sealed);
    assert!(
        directory
            .recovery_candidates(claimant, NOW_MS + 15_002, 2)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        directory
            .takeover_proof(leader, claimant, NOW_MS + 15_002)
            .await
            .unwrap()
            .unwrap()
            .session(),
        leader
    );
}

#[tokio::test]
async fn live_original_follower_is_the_only_affine_recovery_candidate() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    let leader = SessionId::from_bytes([1; 16]);
    let follower = SessionId::from_bytes([2; 16]);
    let follower_node = NodeId::from_bytes([9; 16]);
    let leader_record = directory
        .create(advertisement_for(leader, &key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    let follower_record = directory
        .create(
            advertisement_for_node_capacity(
                follower_node,
                follower,
                &key,
                1,
                NOW_MS,
                NodeCapacity {
                    free_memory_bytes: 1_000,
                    free_disk_bytes: 2_000,
                    follower_free_bytes: 2_000,
                    follower_retained_bytes: 0,
                    job_credits: 3,
                    log_protocol: NODE_LOG_PROTOCOL_VERSION,
                },
            ),
            NOW_MS,
        )
        .await
        .unwrap();
    let enrolled = directory
        .recruit_log(&leader_record, 7, 1, 2, NOW_MS + 1)
        .await
        .unwrap();
    directory.activate_log(&enrolled, NOW_MS + 2).await.unwrap();
    let follower_record = directory
        .refresh(
            &follower_record,
            advertisement_for_node_capacity(
                follower_node,
                follower,
                &key,
                2,
                NOW_MS + 9_000,
                NodeCapacity {
                    free_memory_bytes: 1_000,
                    free_disk_bytes: 2_000,
                    follower_free_bytes: 2_000,
                    follower_retained_bytes: 0,
                    job_credits: 3,
                    log_protocol: NODE_LOG_PROTOCOL_VERSION,
                },
            ),
            NOW_MS + 9_000,
        )
        .await
        .unwrap();

    assert_eq!(
        directory
            .recovery_candidates_for_node(follower, follower_node, NOW_MS + 10_000, 2)
            .await
            .unwrap(),
        [leader]
    );
    assert!(
        directory
            .recovery_candidates_without_live_followers(follower, NOW_MS + 10_000, 2)
            .await
            .unwrap()
            .is_empty()
    );
    let fenced = directory
        .claim_expired(leader, follower, NOW_MS + 10_000)
        .await
        .unwrap();
    assert_eq!(fenced.claimant(), follower);
    directory
        .seal_recovery(&fenced, None, NOW_MS + 10_001)
        .await
        .unwrap();
    assert_eq!(
        directory
            .preferred_recovery_node(leader, NOW_MS + 10_002)
            .await
            .unwrap()
            .unwrap()
            .session(),
        follower
    );
    let drained = directory
        .refresh(
            &follower_record,
            advertisement_for_node_capacity(
                follower_node,
                follower,
                &key,
                3,
                NOW_MS + 10_003,
                NodeCapacity {
                    free_memory_bytes: 0,
                    free_disk_bytes: 2_000,
                    follower_free_bytes: 2_000,
                    follower_retained_bytes: 0,
                    job_credits: 3,
                    log_protocol: NODE_LOG_PROTOCOL_VERSION,
                },
            ),
            NOW_MS + 10_003,
        )
        .await
        .unwrap();
    assert!(
        directory
            .preferred_recovery_node(leader, NOW_MS + 10_004)
            .await
            .unwrap()
            .is_none()
    );
    drop(drained);
}

#[tokio::test]
async fn non_member_recovery_candidate_is_allowed_when_all_followers_are_expired() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    let leader = SessionId::from_bytes([1; 16]);
    let first_member = SessionId::from_bytes([2; 16]);
    let second_member = SessionId::from_bytes([3; 16]);
    let fallback = SessionId::from_bytes([4; 16]);
    let leader_record = directory
        .create(advertisement_for(leader, &key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    let first_record = directory
        .create(advertisement_for(first_member, &key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    let second_record = directory
        .create(advertisement_for(second_member, &key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    let fallback_record = directory
        .create(advertisement_for(fallback, &key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    let enrolled = directory
        .recruit_log(&leader_record, 7, 1, 8, NOW_MS + 1)
        .await
        .unwrap();
    directory.activate_log(&enrolled, NOW_MS + 2).await.unwrap();
    let members = enrolled.advertisement().log().unwrap().members().to_vec();
    let fallback_record = [first_record, second_record, fallback_record]
        .into_iter()
        .find(|record| !members.contains(&record.advertisement().node()))
        .expect("the bounded two-member log leaves one non-member");
    let fallback_session = fallback_record.advertisement().session();
    directory
        .refresh(
            &fallback_record,
            advertisement_for(fallback_session, &key, 2, NOW_MS + 10_000),
            NOW_MS + 10_000,
        )
        .await
        .unwrap();

    assert_eq!(
        directory
            .recovery_candidates_without_live_followers(fallback_session, NOW_MS + 10_000, 2)
            .await
            .unwrap(),
        [leader]
    );
}

#[tokio::test]
async fn expired_recovery_claim_moves_to_a_new_live_claimant_and_fences_the_old_one() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    let leader = SessionId::from_bytes([1; 16]);
    let member = SessionId::from_bytes([2; 16]);
    let first_claimant = SessionId::from_bytes([3; 16]);
    let second_claimant = SessionId::from_bytes([4; 16]);
    let created = directory
        .create(advertisement_for(leader, &key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    directory
        .create(advertisement_for(member, &key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    let enrolled = directory
        .recruit_log(&created, 7, 1, 2, NOW_MS + 1)
        .await
        .unwrap();
    directory.activate_log(&enrolled, NOW_MS + 2).await.unwrap();
    directory
        .create(
            advertisement_for(first_claimant, &key, 1, NOW_MS + 9_000),
            NOW_MS + 9_000,
        )
        .await
        .unwrap();
    let first = directory
        .claim_expired(leader, first_claimant, NOW_MS + 10_000)
        .await
        .unwrap();
    directory
        .create(
            advertisement_for(second_claimant, &key, 1, NOW_MS + 39_000),
            NOW_MS + 39_000,
        )
        .await
        .unwrap();

    assert!(
        directory
            .claim_expired(leader, second_claimant, NOW_MS + 39_999)
            .await
            .is_err()
    );
    let second = directory
        .claim_expired(leader, second_claimant, NOW_MS + 40_000)
        .await
        .unwrap();

    assert_eq!(second.claimant(), second_claimant);
    assert_eq!(second.claim_generation(), first.claim_generation() + 1);
    assert_eq!(
        second.log().unwrap().recovery().unwrap().generation(),
        second.claim_generation()
    );
    assert!(
        directory
            .refresh_recovery_claim(&first, NOW_MS + 40_001)
            .await
            .is_err()
    );
}
