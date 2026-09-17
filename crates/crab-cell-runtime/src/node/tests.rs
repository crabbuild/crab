use std::sync::Arc;

use bytes::Bytes;
use crab_storage::{CellStorageLayout, Store};
use ed25519_dalek::SigningKey;
use futures_util::future::BoxFuture;
use object_store::{memory::InMemory, path::Path};

use super::*;
use crate::{
    AppendRequest, ApplicationId, CellTarget, NamespaceId, NodeLogTransport, PeerOperation,
    PeerPrincipal, PeerSigner, RetireRequest, SealRequest, TailRequest, TenantId, peer_wire,
};

const NOW_MS: i64 = 1_000_000;

fn node(session: SessionId) -> NodeId {
    NodeId::from_bytes(*session.as_bytes())
}

struct UnavailableFollowerTransport;

impl NodeLogTransport for UnavailableFollowerTransport {
    fn append<'a>(
        &'a self,
        _member: NodeId,
        _request: AppendRequest,
    ) -> BoxFuture<'a, Result<crate::FollowerReceipt>> {
        Box::pin(async { Err(Error::Node("injected unavailable follower")) })
    }

    fn seal<'a>(
        &'a self,
        _member: NodeId,
        _request: SealRequest,
    ) -> BoxFuture<'a, Result<crate::FollowerReceipt>> {
        Box::pin(async { Err(Error::Node("injected unavailable follower")) })
    }

    fn retire<'a>(
        &'a self,
        _member: NodeId,
        _request: RetireRequest,
    ) -> BoxFuture<'a, Result<crate::FollowerReceipt>> {
        Box::pin(async { Err(Error::Node("injected unavailable follower")) })
    }

    fn tail<'a>(
        &'a self,
        _member: NodeId,
        _request: TailRequest,
    ) -> BoxFuture<'a, Result<Vec<Bytes>>> {
        Box::pin(async { Err(Error::Node("injected unavailable follower")) })
    }
}

fn advertisement(key: &SigningKey, progress: u64, issued_at_ms: i64) -> NodeAdvertisement {
    advertisement_for(SessionId::from_bytes([1; 16]), key, progress, issued_at_ms)
}

fn advertisement_for(
    session: SessionId,
    key: &SigningKey,
    progress: u64,
    issued_at_ms: i64,
) -> NodeAdvertisement {
    advertisement_for_node_capacity(
        node(session),
        session,
        key,
        progress,
        issued_at_ms,
        NodeCapacity {
            free_memory_bytes: 1_000,
            free_disk_bytes: 2_000,
            follower_free_bytes: 2_000,
            job_credits: 3,
            log_protocol: NODE_LOG_PROTOCOL_VERSION,
        },
    )
}

fn advertisement_for_capacity(
    session: SessionId,
    key: &SigningKey,
    progress: u64,
    issued_at_ms: i64,
    capacity: NodeCapacity,
) -> NodeAdvertisement {
    advertisement_for_node_capacity(
        node(session),
        session,
        key,
        progress,
        issued_at_ms,
        capacity,
    )
}

fn advertisement_for_node_capacity(
    node: NodeId,
    session: SessionId,
    key: &SigningKey,
    progress: u64,
    issued_at_ms: i64,
    capacity: NodeCapacity,
) -> NodeAdvertisement {
    NodeAdvertisement::sign(
        node,
        session,
        "https://node-1.internal:8789".into(),
        Digest::from_bytes([2; 32]),
        Digest::from_bytes([3; 32]),
        Digest::from_bytes([4; 32]),
        Digest::from_bytes([5; 32]),
        key,
        progress,
        issued_at_ms,
        issued_at_ms + 10_000,
        vec![Digest::from_bytes([6; 32]), Digest::from_bytes([7; 32])],
        vec![1],
        capacity,
    )
    .unwrap()
}

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
async fn overlapping_live_sessions_for_one_node_fail_closed() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    let stable = NodeId::from_bytes([8; 16]);
    let capacity = NodeCapacity {
        free_memory_bytes: 1_000,
        free_disk_bytes: 2_000,
        follower_free_bytes: 2_000,
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
async fn follower_selection_is_capacity_aware_deterministic_and_requires_full_shape() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let leader = SessionId::from_bytes([1; 16]);
    let first = SessionId::from_bytes([2; 16]);
    let second = SessionId::from_bytes([3; 16]);
    let pressured = SessionId::from_bytes([4; 16]);
    let directory = directory();
    for (session, follower_free_bytes) in [
        (leader, 2_000),
        (first, 2_000),
        (second, 1_000),
        (pressured, 9),
    ] {
        directory
            .create(
                advertisement_for_capacity(
                    session,
                    &key,
                    1,
                    NOW_MS,
                    NodeCapacity {
                        free_memory_bytes: 1_000,
                        free_disk_bytes: 2_000,
                        follower_free_bytes,
                        job_credits: 3,
                        log_protocol: NODE_LOG_PROTOCOL_VERSION,
                    },
                ),
                NOW_MS,
            )
            .await
            .unwrap();
    }

    assert_eq!(
        directory
            .select_log_members(leader, 1_000, NOW_MS + 1, 4)
            .await
            .unwrap(),
        [node(first), node(second)]
    );
    assert!(
        directory
            .select_log_members(leader, 1_001, NOW_MS + 1, 4)
            .await
            .unwrap()
            .is_empty()
    );
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
async fn node_log_enrollment_activation_and_coverage_are_authoritative() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    let leader = SessionId::from_bytes([1; 16]);
    let first = SessionId::from_bytes([2; 16]);
    let second = SessionId::from_bytes([3; 16]);
    let created = directory
        .create(advertisement_for(leader, &key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    for member in [first, second] {
        directory
            .create(advertisement_for(member, &key, 1, NOW_MS), NOW_MS)
            .await
            .unwrap();
    }

    let enrolled = directory
        .recruit_log(&created, 4, 1, 3, NOW_MS + 1)
        .await
        .unwrap();
    let log = enrolled.advertisement().log().unwrap();
    assert_eq!(enrolled.advertisement().generation(), 2);
    assert_eq!(log.phase(), NodeLogPhase::Open);
    assert!(!log.active());
    assert_eq!(log.members(), [node(first), node(second)]);
    directory
        .authorize_log_append(leader, node(first), 4, NOW_MS + 2)
        .await
        .unwrap();
    assert!(
        directory
            .authorize_log_append(leader, NodeId::from_bytes([8; 16]), 4, NOW_MS + 2)
            .await
            .is_err()
    );

    let refreshed = directory
        .refresh(
            &created,
            advertisement_for(leader, &key, 2, NOW_MS + 1_000),
            NOW_MS + 1_000,
        )
        .await
        .unwrap();
    assert_eq!(refreshed.advertisement().generation(), 3);
    assert_eq!(refreshed.advertisement().log(), Some(log));
    let active = directory
        .activate_log(&refreshed, NOW_MS + 1_001)
        .await
        .unwrap();
    assert!(active.advertisement().log().unwrap().active());
    let covered = directory
        .advance_log_coverage(&active, 27, NOW_MS + 1_002)
        .await
        .unwrap();
    assert_eq!(covered.advertisement().log().unwrap().tiered_through(), 27);
    directory
        .authorize_log_retire(leader, node(first), 4, 27, NOW_MS + 1_003)
        .await
        .unwrap();
    let gate =
        crate::DurabilityGate::new(leader, node(leader), 4, [node(first), node(second)]).unwrap();
    let ticket = gate.issue(27).unwrap();
    gate.prove_object(ticket).unwrap();
    let rotated = crate::rotate_node_log(
        &directory,
        Arc::new(UnavailableFollowerTransport),
        &covered,
        &gate,
        1,
        3,
        NOW_MS + 1_003,
    )
    .await
    .unwrap();
    let rotated_log = rotated.enrollment.advertisement().log().unwrap();
    assert_eq!(rotated_log.epoch(), 5);
    assert_eq!(rotated_log.phase(), NodeLogPhase::Open);
    assert!(!rotated_log.active());
    assert_eq!(rotated_log.tiered_through(), 0);
    assert_eq!(rotated.gate.issue(1).unwrap().first_sequence(), 1);
    assert!(
        directory
            .authorize_log_retire(leader, node(first), 4, 27, NOW_MS + 1_004)
            .await
            .is_err()
    );
    assert!(
        directory
            .withdraw(&rotated.enrollment, NOW_MS + 1_005)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn clean_node_log_close_clears_authority_before_session_withdrawal() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    let leader = SessionId::from_bytes([1; 16]);
    let member = SessionId::from_bytes([2; 16]);
    let created = directory
        .create(advertisement_for(leader, &key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    directory
        .create(advertisement_for(member, &key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    let enrolled = directory
        .recruit_log(&created, 4, 1, 2, NOW_MS + 1)
        .await
        .unwrap();
    let active = directory.activate_log(&enrolled, NOW_MS + 2).await.unwrap();
    let covered = directory
        .advance_log_coverage(&active, 2, NOW_MS + 3)
        .await
        .unwrap();
    let gate = crate::DurabilityGate::new(leader, node(leader), 4, [node(member)]).unwrap();
    let ticket = gate.issue(2).unwrap();
    gate.prove_object(ticket).unwrap();

    let closed = crate::close_node_log(
        &directory,
        Arc::new(UnavailableFollowerTransport),
        &covered,
        &gate,
        NOW_MS + 4,
    )
    .await
    .unwrap();

    assert!(closed.advertisement().log().is_none());
    assert!(
        directory
            .authorize_log_append(leader, node(member), 4, NOW_MS + 5)
            .await
            .is_err()
    );
    directory.withdraw(&closed, NOW_MS + 5).await.unwrap();
    assert!(directory.load(leader, NOW_MS + 6).await.unwrap().is_none());
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
            .authorize_log_append(leader, node(member), 7, NOW_MS + 10_001)
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

fn directory() -> NodeDirectory {
    NodeDirectory::new(
        CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            Path::from("root"),
            [9; 16],
        ),
        Digest::from_bytes([2; 32]),
        Digest::from_bytes([4; 32]),
        Digest::from_bytes([5; 32]),
    )
}

#[tokio::test]
async fn create_load_and_refresh_preserve_signed_boot_identity() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    let created = directory
        .create(advertisement(&key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    let adopted = directory
        .create(advertisement(&key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    assert_eq!(adopted.advertisement(), created.advertisement());
    let loaded = directory
        .load(SessionId::from_bytes([1; 16]), NOW_MS + 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loaded.advertisement(), created.advertisement());
    assert!(
        directory
            .is_live(SessionId::from_bytes([1; 16]), NOW_MS + 1)
            .await
            .unwrap()
    );
    assert!(
        !directory
            .is_live(SessionId::from_bytes([9; 16]), NOW_MS + 1)
            .await
            .unwrap()
    );

    let refreshed = directory
        .refresh(
            &created,
            advertisement(&key, 1, NOW_MS + 1_000),
            NOW_MS + 1_000,
        )
        .await
        .unwrap();
    assert_eq!(refreshed.advertisement().progress(), 1);
    assert_eq!(
        refreshed.advertisement().signature,
        created.advertisement().signature
    );
    let refreshed = directory
        .refresh(
            &refreshed,
            advertisement(&key, 2, NOW_MS + 2_000),
            NOW_MS + 2_000,
        )
        .await
        .unwrap();
    assert_eq!(refreshed.advertisement().progress(), 2);
}

#[tokio::test]
async fn invalid_signature_expiry_and_identity_change_fail_closed() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    assert!(
        NodeAdvertisement::sign(
            NodeId::from_bytes([1; 16]),
            SessionId::from_bytes([1; 16]),
            "https:///not-an-authority".into(),
            Digest::from_bytes([2; 32]),
            Digest::from_bytes([3; 32]),
            Digest::from_bytes([4; 32]),
            Digest::from_bytes([5; 32]),
            &key,
            1,
            NOW_MS,
            NOW_MS + 10_000,
            vec![Digest::from_bytes([6; 32])],
            vec![1],
            NodeCapacity {
                free_memory_bytes: 1,
                free_disk_bytes: 1,
                job_credits: 1,
                ..NodeCapacity::default()
            },
        )
        .is_err()
    );
    let original = advertisement(&key, 1, NOW_MS);
    let mut tampered = original.encode().unwrap();
    let endpoint_byte = tampered
        .windows(b"node-1".len())
        .position(|window| window == b"node-1")
        .unwrap()
        + b"node-".len();
    tampered[endpoint_byte] = b'2';
    assert!(NodeAdvertisement::decode_canonical(&tampered).is_err());

    let created = directory.create(original, NOW_MS).await.unwrap();
    assert!(
        directory
            .load(SessionId::from_bytes([1; 16]), NOW_MS + 10_000)
            .await
            .is_err()
    );
    assert!(
        !directory
            .is_live(SessionId::from_bytes([1; 16]), NOW_MS + 10_000)
            .await
            .unwrap()
    );

    let other_key = SigningKey::from_bytes(&[8; 32]);
    assert!(
        directory
            .refresh(
                &created,
                advertisement(&other_key, 2, NOW_MS + 1_000),
                NOW_MS + 1_000,
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn request_requires_the_live_sessions_mtls_certificate_and_signing_key() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    directory
        .create(advertisement(&key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    let target = CellTarget::new(
        TenantId::from_bytes([8; 16]),
        ApplicationId::from_bytes([9; 16]),
        NamespaceId::from_bytes([10; 16]),
        b"partition",
    )
    .unwrap();
    let signer = PeerSigner::new(
        SessionId::from_bytes([1; 16]),
        Digest::from_bytes([5; 32]),
        key,
    );
    let certificate_key = signer.verifying_key().to_bytes();
    let request = signer
        .sign(
            PeerPrincipal {
                issuer: "https://identity.example".into(),
                subject: "user".into(),
                actions: vec!["repository.read".into()],
            },
            NOW_MS,
            NOW_MS + 10_000,
            5_000,
            PeerOperation::Read(peer_wire::ReadRequest {
                target: Some(peer_wire::Target {
                    tenant_id: target.tenant().as_bytes().to_vec(),
                    application_id: target.application().as_bytes().to_vec(),
                    namespace_id: target.namespace().as_bytes().to_vec(),
                    partition: target.partition().to_vec(),
                }),
                timeout_ms: 5_000,
                minimum: None,
                operation: Some(peer_wire::read_request::Operation::Describe(true)),
            }),
        )
        .unwrap();

    assert!(
        directory
            .verify_peer_request(
                &request,
                Digest::from_bytes([3; 32]),
                certificate_key,
                NOW_MS + 1,
            )
            .await
            .is_ok()
    );
    assert!(matches!(
        directory
            .verify_peer_request(
                &request,
                Digest::from_bytes([11; 32]),
                certificate_key,
                NOW_MS + 1,
            )
            .await,
        Err(Error::PeerAuthorization(_))
    ));
    assert!(matches!(
        directory
            .verify_peer_request(&request, Digest::from_bytes([3; 32]), [12; 32], NOW_MS + 1,)
            .await,
        Err(Error::PeerAuthorization(_))
    ));
}
