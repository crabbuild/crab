//! Session fencing is shared; Cell takeover remains independently authorized.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crab_cell_runtime::Error;
use crab_cell_runtime::identity::{Digest, NodeId, SessionId};
use crab_cell_runtime::node::{
    NODE_LOG_PROTOCOL_VERSION, NodeAdvertisement, NodeCapacity, NodeDirectory, NodeFailureDomain,
};
use crab_ltx::CellStorageLayout;
use crab_storage::Store;
use ed25519_dalek::SigningKey;
use object_store::{memory::InMemory, path::Path};

#[tokio::test(flavor = "multi_thread")]
async fn scheduler_membership_and_recovery_share_one_fresh_directory_scan() {
    scheduler_discovery(Store::new(Arc::new(InMemory::new())), "scheduler-discovery").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a pre-created RustFS bucket, isolated prefix and test credentials"]
async fn rustfs_scheduler_membership_and_recovery_share_one_fresh_directory_scan() {
    let required = |name| std::env::var(name).unwrap_or_else(|_| panic!("missing {name}"));
    let store = crab_storage::build_explicit_store(
        &required("CRAB_CELL_TEST_BUCKET"),
        crab_storage::ObjectStoreCredentials::Aws {
            access_key_id: required("AWS_ACCESS_KEY_ID"),
            secret_access_key: required("AWS_SECRET_ACCESS_KEY"),
            session_token: None,
            region: "us-east-1".into(),
        },
        Some(&required("CRAB_CELL_TEST_ENDPOINT")),
        true,
    )
    .unwrap();
    scheduler_discovery(store, &required("CRAB_CELL_TEST_PREFIX")).await;
}

async fn scheduler_discovery(store: Store, prefix: &str) {
    for nodes in [3_u8, 5, 10, 20] {
        let reads = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&reads);
        let store = store.clone().with_read_request_observer(Arc::new(move |_| {
            counted.fetch_add(1, Ordering::Relaxed);
        }));
        let directory = NodeDirectory::new(
            CellStorageLayout::new(store, Path::from(format!("{prefix}/{nodes}")), [1; 16]),
            Digest::from_bytes([2; 32]),
            Digest::from_bytes([3; 32]),
            Digest::from_bytes([4; 32]),
        );
        for index in 1..=nodes {
            directory
                .create(
                    advertisement(SessionId::from_bytes([index; 16]), 2_000),
                    2_000,
                )
                .await
                .unwrap();
            let retired = directory
                .create(
                    advertisement(SessionId::from_bytes([index + nodes; 16]), 1_000),
                    1_000,
                )
                .await
                .unwrap();
            directory.withdraw(&retired, 2_000).await.unwrap();
        }
        let failed = SessionId::from_bytes([2 * nodes + 1; 16]);
        let enrolled = directory
            .create(advertisement(failed, 1_000), 1_000)
            .await
            .unwrap();
        let enrolled = directory
            .recruit_log(&enrolled, 1, 1, usize::from(nodes) + 1, 2_001)
            .await
            .unwrap();
        directory.activate_log(&enrolled, 2_002).await.unwrap();

        reads.store(0, Ordering::Relaxed);
        let live = directory
            .live_for_recovery(11_000, usize::from(nodes))
            .await
            .unwrap();
        assert_eq!(live.len(), usize::from(nodes));
        let candidates = directory
            .clone()
            .recovery_candidates(SessionId::from_bytes([1; 16]), 11_000, 16)
            .await
            .unwrap();
        assert_eq!(candidates, [failed]);
        // Every live/retired/failed record is read once. The extra GET is the
        // fresh claimant admission check, which discovery must never replace.
        assert_eq!(reads.load(Ordering::Relaxed), usize::from(nodes) * 2 + 2);

        let claimant = SessionId::from_bytes([1; 16]);
        let observed = directory.load(claimant, 11_000).await.unwrap().unwrap();
        directory.withdraw(&observed, 11_000).await.unwrap();
        assert!(
            directory
                .recovery_candidates(claimant, 11_000, 16)
                .await
                .is_err()
        );
        assert!(
            directory
                .claim_expired_for_recovery(failed, claimant, 11_000)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn recovery_membership_refresh_is_fresh_and_drops_failed_observations() {
    use bytes::Bytes;
    use object_store::ObjectStoreExt;

    let inner = Arc::new(InMemory::new());
    let layout =
        CellStorageLayout::new(Store::new(inner.clone()), Path::from("fresh-scan"), [1; 16]);
    let directory = NodeDirectory::new(
        layout.clone(),
        Digest::from_bytes([2; 32]),
        Digest::from_bytes([3; 32]),
        Digest::from_bytes([4; 32]),
    );
    let claimant = SessionId::from_bytes([1; 16]);
    let other = SessionId::from_bytes([2; 16]);
    directory
        .create(advertisement(claimant, 1_000), 1_000)
        .await
        .unwrap();
    assert_eq!(
        directory.live_for_recovery(2_000, 2).await.unwrap().len(),
        1
    );
    let remote = NodeDirectory::new(
        layout.clone(),
        directory.fleet(),
        Digest::from_bytes([3; 32]),
        Digest::from_bytes([4; 32]),
    );
    remote
        .create(advertisement(other, 1_000), 1_000)
        .await
        .unwrap();
    assert_eq!(
        directory.live_for_recovery(2_000, 2).await.unwrap().len(),
        2
    );
    assert!(directory.live_for_recovery(2_000, 1).await.is_err());
    directory.live_for_recovery(2_000, 2).await.unwrap();
    inner
        .put(
            &layout.node_path(other.as_bytes()),
            Bytes::from_static(b"invalid node").into(),
        )
        .await
        .unwrap();
    assert!(directory.live_for_recovery(2_000, 2).await.is_err());
    assert!(
        directory
            .recovery_candidates(claimant, 2_000, 2)
            .await
            .is_err()
    );
}

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
