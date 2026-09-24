//! Signed-capacity placement and follower selection.

use super::*;

#[tokio::test]
async fn placement_uses_signed_capacity_only() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    let session = SessionId::from_bytes([1; 16]);
    directory
        .create(
            advertisement_for(session, &key, 1, NOW_MS)
                .with_placement_capacity(
                    NodePlacementCapacity {
                        memory_capacity_bytes: 2_000,
                        disk_capacity_bytes: 4_000,
                        active_cells: 1,
                        max_active_cells: 8,
                        running_jobs: 0,
                        job_capacity: 3,
                        publication_backlog: 0,
                        hydration_backlog: 0,
                        primitive_backlog: 0,
                    }
                    .validated()
                    .unwrap(),
                    &key,
                )
                .unwrap(),
            NOW_MS,
        )
        .await
        .unwrap();
    let planner = PlacementPlanner::default();
    let cell = crate::CellId::from_bytes([9; 32]);
    let advertised = directory
        .choose_advertised_placement(&planner, cell, NOW_MS + 1, 4)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(advertised.node, node(session));
}

#[tokio::test]
async fn cold_placement_does_not_reward_the_requesting_node() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    let local = SessionId::from_bytes([1; 16]);
    let remote = SessionId::from_bytes([2; 16]);
    for (session, free_memory_bytes) in [(local, 100), (remote, 1_000)] {
        let advertisement = advertisement_for_capacity(
            session,
            &key,
            1,
            NOW_MS,
            NodeCapacity {
                free_memory_bytes,
                free_disk_bytes: 1_000,
                job_credits: 4,
                ..NodeCapacity::default()
            },
        )
        .with_placement_capacity(
            NodePlacementCapacity {
                memory_capacity_bytes: 1_000,
                disk_capacity_bytes: 1_000,
                active_cells: 0,
                max_active_cells: 10,
                running_jobs: 0,
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
        directory.create(advertisement, NOW_MS).await.unwrap();
    }
    let chosen = directory
        .choose_advertised_placement(
            &PlacementPlanner::default(),
            crate::CellId::from_bytes([9; 32]),
            NOW_MS + 1,
            4,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(chosen.session, remote);
}

#[tokio::test]
async fn idle_placement_prefers_peer_with_more_active_cell_headroom() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    let requester = SessionId::from_bytes([1; 16]);
    let peer = SessionId::from_bytes([2; 16]);
    for (session, active_cells) in [(requester, 4), (peer, 3)] {
        let placement = NodePlacementCapacity {
            memory_capacity_bytes: 1_000,
            disk_capacity_bytes: 2_000,
            active_cells,
            max_active_cells: 10,
            running_jobs: 0,
            job_capacity: 3,
            publication_backlog: 0,
            hydration_backlog: 0,
            primitive_backlog: 0,
        }
        .validated()
        .unwrap();
        let advertisement = advertisement_for(session, &key, 1, NOW_MS)
            .with_placement_capacity(placement, &key)
            .unwrap();
        directory.create(advertisement, NOW_MS).await.unwrap();
    }

    let chosen = directory
        .choose_advertised_placement(
            &PlacementPlanner::default(),
            crate::CellId::from_bytes([9; 32]),
            NOW_MS + 1,
            4,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(chosen.session, peer);
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
                        follower_retained_bytes: 0,
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
async fn follower_selection_prefers_proven_zone_then_host_separation() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let leader = SessionId::from_bytes([1; 16]);
    let same_zone = SessionId::from_bytes([2; 16]);
    let remote_zone = SessionId::from_bytes([3; 16]);
    let third_zone_same_host = SessionId::from_bytes([4; 16]);
    let directory = directory();
    let capacity = NodeCapacity {
        free_memory_bytes: 1_000,
        free_disk_bytes: 2_000,
        follower_free_bytes: 2_000,
        follower_retained_bytes: 0,
        job_credits: 3,
        log_protocol: NODE_LOG_PROTOCOL_VERSION,
    };
    for (session, zone, host) in [
        (leader, "zone-a", "host-a"),
        (same_zone, "zone-a", "host-b"),
        (remote_zone, "zone-b", "host-c"),
        (third_zone_same_host, "zone-c", "host-a"),
    ] {
        directory
            .create(
                advertisement_for_node_capacity_in_domain(
                    node(session),
                    session,
                    &key,
                    1,
                    NOW_MS,
                    NodeFailureDomain::new(Some(zone.into()), Some(host.into())).unwrap(),
                    capacity,
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
        [node(remote_zone), node(third_zone_same_host)]
    );
}

#[test]
fn failure_domain_rejects_unknown_or_ambiguous_labels() {
    for invalid in [
        String::new(),
        " zone-a".into(),
        "zone a".into(),
        "zone-a\n".into(),
        "a".repeat(254),
    ] {
        assert!(NodeFailureDomain::new(Some(invalid), None).is_err());
    }
    assert!(NodeFailureDomain::new(None, None).is_ok());
}

#[test]
fn placement_schema_is_mixed_version_safe_and_fail_closed() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let current = advertisement(&key, 1, NOW_MS)
        .with_placement_capacity(
            NodePlacementCapacity {
                memory_capacity_bytes: 8_192,
                disk_capacity_bytes: 16_384,
                active_cells: 3,
                max_active_cells: 16,
                running_jobs: 2,
                job_capacity: 8,
                publication_backlog: 4,
                hydration_backlog: 5,
                primitive_backlog: 6,
            }
            .validated()
            .unwrap(),
            &key,
        )
        .unwrap();
    assert!(current.has_signed_placement());
    let decoded_current = NodeAdvertisement::decode_canonical(&current.encode().unwrap()).unwrap();
    assert_eq!(decoded_current.placement_version, 2);
    assert_eq!(
        decoded_current.placement_capacity(),
        current.placement_capacity()
    );
    let observation =
        PlacementObservation::from_signed_advertisement(&decoded_current, NOW_MS + 1, true)
            .unwrap();
    assert_eq!(observation.memory_capacity_bytes, 8_192);
    assert_eq!(observation.active_cells, 3);
    assert_eq!(observation.running_jobs, 2);
    assert_eq!(observation.publication_backlog, 4);
    assert_eq!(observation.hydration_backlog, 5);
    assert_eq!(observation.primitive_backlog, 6);

    let mut legacy = current.clone();
    legacy.placement_version = 0;
    legacy.placement_signature = [0; 64];
    let decoded_legacy = NodeAdvertisement::decode_canonical(&legacy.encode().unwrap()).unwrap();
    assert!(!decoded_legacy.has_signed_placement());

    let mut previous = current.clone();
    previous.placement_version = 1;
    let decoded_previous =
        NodeAdvertisement::decode_canonical(&previous.encode().unwrap()).unwrap();
    assert!(!decoded_previous.has_signed_placement());
    assert_eq!(
        decoded_previous
            .placement_capacity()
            .unwrap()
            .publication_backlog,
        0
    );

    let mut future = current;
    future.placement_version = 3;
    future.placement_signature = [0; 64];
    let decoded_future = NodeAdvertisement::decode_canonical(&future.encode().unwrap()).unwrap();
    assert!(!decoded_future.has_signed_placement());
    assert!(
        PlacementObservation::from_signed_advertisement(&decoded_future, NOW_MS + 1, false)
            .is_err()
    );
}

#[test]
fn placement_upgrade_sets_schema_when_legacy_capacity_was_unusable() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let legacy = advertisement_for_capacity(
        SessionId::from_bytes([1; 16]),
        &key,
        1,
        NOW_MS,
        NodeCapacity::default(),
    );
    assert!(!legacy.has_signed_placement());
    let upgraded = legacy
        .with_placement_capacity(
            NodePlacementCapacity {
                memory_capacity_bytes: 8_192,
                disk_capacity_bytes: 16_384,
                active_cells: 0,
                max_active_cells: 16,
                running_jobs: 0,
                job_capacity: 8,
                publication_backlog: 0,
                hydration_backlog: 0,
                primitive_backlog: 0,
            }
            .validated()
            .unwrap(),
            &key,
        )
        .unwrap();
    assert!(upgraded.has_signed_placement());
    let decoded = NodeAdvertisement::decode_canonical(&upgraded.encode().unwrap()).unwrap();
    assert_eq!(decoded.placement_version, 2);
    assert_eq!(decoded.placement_capacity(), upgraded.placement_capacity());
}
