//! Session fencing is shared; Cell takeover remains independently authorized.

use std::sync::Arc;

use crab_cell_runtime::Error;
use crab_cell_runtime::identity::{Digest, NodeId, SessionId};
use crab_cell_runtime::node::{
    NODE_LOG_PROTOCOL_VERSION, NodeAdvertisement, NodeCapacity, NodeDirectory, NodeFailureDomain,
};
use crab_ltx::CellStorageLayout;
use crab_storage::Store;
use ed25519_dalek::SigningKey;
use object_store::{memory::InMemory, path::Path};

#[tokio::test]
async fn independent_takeovers_share_a_fence_only_without_an_active_log() {
    for log_state in ["absent", "inactive", "active"] {
        let directory = NodeDirectory::new(
            CellStorageLayout::new(
                Store::new(Arc::new(InMemory::new())),
                Path::from(log_state),
                [1; 16],
            ),
            Digest::from_bytes([2; 32]),
            Digest::from_bytes([3; 32]),
            Digest::from_bytes([4; 32]),
        );
        let leader = SessionId::from_bytes([1; 16]);
        let first = SessionId::from_bytes([2; 16]);
        let second = SessionId::from_bytes([3; 16]);
        let mut enrollment = directory
            .create(advertisement(leader, 1_000), 1_000)
            .await
            .unwrap();
        for session in [first, second] {
            directory
                .create(advertisement(session, 2_000), 2_000)
                .await
                .unwrap();
        }
        if log_state != "absent" {
            enrollment = directory
                .recruit_log(&enrollment, 1, 1, 3, 2_001)
                .await
                .unwrap();
            if log_state == "active" {
                enrollment = directory.activate_log(&enrollment, 2_002).await.unwrap();
            }
        }
        let now_ms = 11_000;
        // Both request paths observed the expired advertisement before either
        // fenced it. The later claim must resolve the other caller's CAS.
        for claimant in [first, second] {
            assert!(
                directory
                    .takeover_proof(leader, claimant, now_ms)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        if log_state == "active" {
            let fenced = directory
                .claim_expired_for_recovery(leader, first, now_ms)
                .await
                .unwrap();
            assert!(matches!(
                fenced.direct_takeover(),
                Err(Error::PendingPublication)
            ));
            for claimant in [first, second] {
                assert!(
                    directory
                        .takeover_proof(leader, claimant, now_ms)
                        .await
                        .unwrap()
                        .is_none()
                );
                assert!(matches!(
                    directory
                        .claim_expired_for_takeover(leader, claimant, now_ms)
                        .await,
                    Err(Error::PendingPublication)
                ));
            }
            assert!(
                directory
                    .claim_expired_for_recovery(leader, second, now_ms)
                    .await
                    .is_err()
            );
        } else {
            for claimant in [first, second] {
                let proof = directory
                    .claim_expired_for_takeover(leader, claimant, now_ms)
                    .await
                    .unwrap_or_else(|error| panic!("{log_state}: {claimant:?}: {error}"));
                assert_eq!((proof.session(), proof.claimant()), (leader, claimant));
                assert_eq!(
                    directory
                        .takeover_proof(leader, claimant, now_ms)
                        .await
                        .unwrap(),
                    Some(proof)
                );
            }
        }
        // Sharing proof neither revives the old session nor admits an expired
        // successor, and an old enrollment cannot activate a log after fencing.
        assert!(!directory.is_live(leader, now_ms).await.unwrap());
        if log_state == "inactive" {
            assert!(directory.activate_log(&enrollment, 2_003).await.is_err());
        }
        assert!(
            directory
                .takeover_proof(leader, second, 12_000)
                .await
                .is_err()
        );
    }
}

fn advertisement(session: SessionId, issued_at_ms: i64) -> NodeAdvertisement {
    NodeAdvertisement::sign(
        NodeId::from_bytes(*session.as_bytes()),
        session,
        "https://node.internal:8789".into(),
        Digest::from_bytes([2; 32]),
        Digest::from_bytes([5; 32]),
        Digest::from_bytes([3; 32]),
        Digest::from_bytes([4; 32]),
        &SigningKey::from_bytes(&[7; 32]),
        1,
        issued_at_ms,
        issued_at_ms + 10_000,
        vec![Digest::from_bytes([6; 32])],
        vec![1],
        NodeFailureDomain::default(),
        NodeCapacity {
            free_memory_bytes: 1_024,
            free_disk_bytes: 1_024,
            follower_free_bytes: 1_024,
            job_credits: 1,
            log_protocol: NODE_LOG_PROTOCOL_VERSION,
            ..NodeCapacity::default()
        },
    )
    .unwrap()
}
