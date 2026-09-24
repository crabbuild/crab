//! Advertisement records: create, load, refresh, and validation.

use super::*;

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
            NodeFailureDomain::default(),
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
    let mut tampered_capacity = original.clone();
    tampered_capacity.capacity.free_memory_bytes = tampered_capacity
        .capacity
        .free_memory_bytes
        .saturating_add(1);
    // Capacity is part of the signed heartbeat; changing it without a new
    // signature must fail closed. The optional placement block is authenticated
    // independently because it drives ownership placement.
    assert!(tampered_capacity.verify_signature().is_err());
    let signed = original
        .clone()
        .with_placement_capacity(
            NodePlacementCapacity {
                memory_capacity_bytes: 8_192,
                disk_capacity_bytes: 16_384,
                active_cells: 1,
                max_active_cells: 8,
                running_jobs: 1,
                job_capacity: 4,
                publication_backlog: 0,
                hydration_backlog: 0,
                primitive_backlog: 0,
            }
            .validated()
            .unwrap(),
            &key,
        )
        .unwrap();
    let mut tampered_placement = signed;
    tampered_placement
        .placement
        .as_mut()
        .expect("signed placement is present")
        .memory_capacity_bytes = 8_193;
    assert!(tampered_placement.verify_signature().is_err());
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
async fn refresh_accepts_new_signed_capacity_for_same_boot_session() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    let session = SessionId::from_bytes([1; 16]);
    let created = directory
        .create(advertisement_for(session, &key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    let next_capacity = NodeCapacity {
        free_memory_bytes: 900,
        free_disk_bytes: 1_800,
        follower_free_bytes: 1_700,
        follower_retained_bytes: 700,
        job_credits: 2,
        log_protocol: NODE_LOG_PROTOCOL_VERSION,
    };
    let next = advertisement_for_capacity(session, &key, 2, NOW_MS + 1_000, next_capacity);
    assert_ne!(next.signature, created.advertisement().signature);
    let refreshed = directory
        .refresh(&created, next, NOW_MS + 1_000)
        .await
        .unwrap();
    assert_eq!(refreshed.advertisement().capacity(), next_capacity);
    assert!(refreshed.advertisement().verify_signature().is_ok());
}
