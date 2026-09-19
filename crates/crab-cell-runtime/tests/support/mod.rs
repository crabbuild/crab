use crab_cell_runtime::{
    Digest, FencedNodeSession, NodeAdvertisement, NodeCapacity, NodeDirectory, NodeFailureDomain,
    NodeId, SessionId,
};
use crab_storage::CellStorageLayout;

pub async fn fence_session(
    layout: &CellStorageLayout,
    session: SessionId,
    claimant: SessionId,
) -> FencedNodeSession {
    let fleet = Digest::from_bytes([90; 32]);
    let image = Digest::from_bytes([91; 32]);
    let release = Digest::from_bytes([92; 32]);
    let directory = NodeDirectory::new(layout.clone(), fleet, image, release);
    let key = ed25519_dalek::SigningKey::from_bytes(&[93; 32]);
    directory
        .create(
            NodeAdvertisement::sign(
                NodeId::from_bytes(*session.as_bytes()),
                session,
                "https://expired.internal:8081".into(),
                fleet,
                Digest::from_bytes([94; 32]),
                image,
                release,
                &key,
                1,
                1,
                10_001,
                vec![Digest::from_bytes([95; 32])],
                vec![1],
                NodeFailureDomain::default(),
                NodeCapacity {
                    free_memory_bytes: 1,
                    free_disk_bytes: 1,
                    job_credits: 1,
                    ..NodeCapacity::default()
                },
            )
            .unwrap(),
            1,
        )
        .await
        .unwrap();
    directory
        .create(
            NodeAdvertisement::sign(
                NodeId::from_bytes(*claimant.as_bytes()),
                claimant,
                "https://claimant.internal:8081".into(),
                fleet,
                Digest::from_bytes([94; 32]),
                image,
                release,
                &key,
                1,
                10_000,
                20_000,
                vec![Digest::from_bytes([95; 32])],
                vec![1],
                NodeFailureDomain::default(),
                NodeCapacity {
                    free_memory_bytes: 1,
                    free_disk_bytes: 1,
                    job_credits: 1,
                    ..NodeCapacity::default()
                },
            )
            .unwrap(),
            10_000,
        )
        .await
        .unwrap();
    directory
        .claim_expired(session, claimant, 10_001)
        .await
        .unwrap()
}
