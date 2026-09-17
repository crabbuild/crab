use std::sync::Arc;

use crab_storage::{CellStorageLayout, Store};
use ed25519_dalek::SigningKey;
use object_store::{memory::InMemory, path::Path};

use super::*;
use crate::{
    ApplicationId, CellTarget, NamespaceId, PeerOperation, PeerPrincipal, PeerSigner, TenantId,
    peer_wire,
};

const NOW_MS: i64 = 1_000_000;

fn advertisement(key: &SigningKey, progress: u64, issued_at_ms: i64) -> NodeAdvertisement {
    advertisement_for(SessionId::from_bytes([1; 16]), key, progress, issued_at_ms)
}

fn advertisement_for(
    session: SessionId,
    key: &SigningKey,
    progress: u64,
    issued_at_ms: i64,
) -> NodeAdvertisement {
    NodeAdvertisement::sign(
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
        NodeCapacity {
            free_memory_bytes: 1_000,
            free_disk_bytes: 2_000,
            job_credits: 3,
        },
    )
    .unwrap()
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

    let created = directory
        .create(advertisement(&key, 1, NOW_MS + 20_000), NOW_MS + 20_000)
        .await
        .unwrap();
    let refreshed = directory
        .refresh(
            &created,
            advertisement(&key, 2, NOW_MS + 21_000),
            NOW_MS + 21_000,
        )
        .await
        .unwrap();
    assert!(directory.withdraw(&created, NOW_MS + 21_001).await.is_err());
    assert_eq!(
        directory
            .load(session, NOW_MS + 21_001)
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
