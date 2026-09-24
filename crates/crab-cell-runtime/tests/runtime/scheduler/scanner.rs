//! Rendezvous scanner selection and scanner liveness.

use super::*;

#[test]
fn rendezvous_scanner_choice_is_order_independent_and_uses_both_nodes() {
    let first = SessionId::from_bytes([1; 16]);
    let second = SessionId::from_bytes([2; 16]);
    let mut winners = std::collections::HashSet::new();
    for shard in 0_u8..=u8::MAX {
        let forward = preferred_scanner(shard, &[first, second]).unwrap().unwrap();
        let reverse = preferred_scanner(shard, &[second, first]).unwrap().unwrap();
        assert_eq!(forward, reverse);
        winners.insert(*forward.as_bytes());
    }
    assert_eq!(winners.len(), 2);
    assert!(preferred_scanner(0, &[first, first]).is_err());
}
#[test]
fn stalled_scanner_is_removed_until_its_advertised_progress_advances() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let first = advertisement(1, 1, 0, &key);
    let second = advertisement(2, 1, 0, &key);
    let mut fleet = SchedulerFleet::default();
    assert_eq!(
        fleet
            .eligible_sessions(&[first.clone(), second], 0, 100)
            .unwrap()
            .len(),
        2
    );

    let second = advertisement(2, 2, 50, &key);
    assert_eq!(
        fleet
            .eligible_sessions(&[first.clone(), second.clone()], 101, 100)
            .unwrap(),
        vec![SessionId::from_bytes([2; 16])]
    );
    assert_eq!(
        fleet
            .eligible_sessions(&[advertisement(1, 2, 102, &key), second], 102, 100)
            .unwrap(),
        vec![
            SessionId::from_bytes([1; 16]),
            SessionId::from_bytes([2; 16])
        ]
    );
    assert!(
        fleet
            .eligible_sessions(&[advertisement(1, 1, 103, &key)], 103, 100)
            .is_err()
    );
}
#[test]
fn exhausted_scanner_is_removed_until_its_capacity_recovers() {
    let key = SigningKey::from_bytes(&[9; 32]);
    let exhausted = [
        NodeCapacity {
            free_memory_bytes: 0,
            free_disk_bytes: 1,
            job_credits: 1,
            ..NodeCapacity::default()
        },
        NodeCapacity {
            free_memory_bytes: 1,
            free_disk_bytes: 0,
            job_credits: 1,
            ..NodeCapacity::default()
        },
        NodeCapacity {
            free_memory_bytes: 1,
            free_disk_bytes: 1,
            job_credits: 0,
            ..NodeCapacity::default()
        },
    ];
    let available = advertisement(2, 1, 0, &key);
    let mut fleet = SchedulerFleet::default();

    for capacity in exhausted {
        assert_eq!(
            fleet
                .eligible_sessions(
                    &[
                        advertisement_with_capacity(1, 1, 0, &key, capacity),
                        available.clone(),
                    ],
                    0,
                    100,
                )
                .unwrap(),
            vec![SessionId::from_bytes([2; 16])]
        );
    }
    assert_eq!(
        fleet
            .eligible_sessions(&[advertisement(1, 1, 50, &key), available], 50, 100)
            .unwrap(),
        vec![
            SessionId::from_bytes([1; 16]),
            SessionId::from_bytes([2; 16])
        ]
    );
}
fn advertisement(
    session: u8,
    progress: u64,
    issued_at_ms: i64,
    key: &SigningKey,
) -> NodeAdvertisement {
    advertisement_with_capacity(
        session,
        progress,
        issued_at_ms,
        key,
        NodeCapacity {
            free_memory_bytes: 1,
            free_disk_bytes: 1,
            job_credits: 1,
            ..NodeCapacity::default()
        },
    )
}
fn advertisement_with_capacity(
    session: u8,
    progress: u64,
    issued_at_ms: i64,
    key: &SigningKey,
    capacity: NodeCapacity,
) -> NodeAdvertisement {
    NodeAdvertisement::sign(
        crab_cell_runtime::identity::NodeId::from_bytes([session; 16]),
        SessionId::from_bytes([session; 16]),
        format!("https://node-{session}.internal:8789"),
        Digest::from_bytes([1; 32]),
        Digest::from_bytes([2; 32]),
        Digest::from_bytes([3; 32]),
        Digest::from_bytes([4; 32]),
        key,
        progress,
        issued_at_ms,
        issued_at_ms + 10_000,
        vec![Digest::from_bytes([5; 32])],
        vec![1],
        crab_cell_runtime::node::NodeFailureDomain::default(),
        capacity,
    )
    .unwrap()
}
