use crab_cell_runtime::{
    CellStorageLayout, Digest, FencedNodeSession, NodeAdvertisement, NodeCapacity, NodeDirectory,
    NodeFailureDomain, NodeId, SessionId,
};
use ed25519_dalek::SigningKey;

pub async fn fence_public_session(
    layout: &CellStorageLayout,
    source: SessionId,
    successor: SessionId,
) -> FencedNodeSession {
    let fleet = Digest::from_bytes([90; 32]);
    let image = Digest::from_bytes([91; 32]);
    let release = Digest::from_bytes([92; 32]);
    let directory = NodeDirectory::new(layout.clone(), fleet, image, release);
    let key = SigningKey::from_bytes(&[93; 32]);
    for (session, issued_at_ms, expires_at_ms) in [(source, 1, 10_001), (successor, 10_000, 20_000)]
    {
        directory
            .create(
                NodeAdvertisement::sign(
                    NodeId::from_bytes(*session.as_bytes()),
                    session,
                    "https://public-qualification.internal:8081".into(),
                    fleet,
                    Digest::from_bytes([94; 32]),
                    image,
                    release,
                    &key,
                    1,
                    issued_at_ms,
                    expires_at_ms,
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
                .expect("public node advertisement"),
                issued_at_ms,
            )
            .await
            .expect("public node enrollment");
    }
    directory
        .claim_expired(source, successor, 10_001)
        .await
        .expect("source session is fenced")
}
