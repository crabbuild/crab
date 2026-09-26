//! Advisory reader selection from signed live node advertisements.

use std::collections::HashSet;
use std::sync::Arc;

use crab_cell_runtime::identity::{CellId, Digest, NodeId, SessionId};
use crab_cell_runtime::node::{NodeAdvertisement, NodeCapacity, NodeDirectory, NodeFailureDomain};
use crab_ltx::CellStorageLayout;
use crab_storage::Store;
use object_store::{memory::InMemory, path::Path};

async fn advertise(
    directory: &NodeDirectory,
    node: u8,
    session: u8,
    zone: &str,
    code: Digest,
    free_memory_bytes: u64,
    issued_at_ms: i64,
    expires_at_ms: i64,
) {
    let advertisement = NodeAdvertisement::sign(
        NodeId::from_bytes([node; 16]),
        SessionId::from_bytes([session; 16]),
        format!("https://node-{node}.internal:8081"),
        directory.fleet(),
        Digest::from_bytes([7; 32]),
        Digest::from_bytes([8; 32]),
        Digest::from_bytes([9; 32]),
        &ed25519_dalek::SigningKey::from_bytes(&[node; 32]),
        1,
        issued_at_ms,
        expires_at_ms,
        vec![code],
        vec![1],
        NodeFailureDomain::new(Some(zone.into()), Some(format!("host-{node}"))).unwrap(),
        NodeCapacity {
            free_memory_bytes,
            free_disk_bytes: 1 << 20,
            job_credits: 1,
            ..NodeCapacity::default()
        },
    )
    .unwrap();
    directory.create(advertisement, issued_at_ms).await.unwrap();
}

#[tokio::test]
async fn desired_readers_follow_live_distinct_nodes_and_replace_a_lost_member() {
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        Path::from("runtime"),
        [1; 16],
    );
    let code = Digest::from_bytes([10; 32]);
    let directory = NodeDirectory::new(
        layout,
        Digest::from_bytes([6; 32]),
        Digest::from_bytes([8; 32]),
        Digest::from_bytes([9; 32]),
    );
    advertise(&directory, 1, 1, "zone-a", code, 32 << 20, 1_000, 16_000).await;
    for (node, zone) in [(2, "zone-b"), (3, "zone-c"), (4, "zone-d"), (5, "zone-a")] {
        let expires_at_ms = if node == 2 { 8_000 } else { 16_000 };
        advertise(
            &directory,
            node,
            node,
            zone,
            code,
            32 << 20,
            1_000,
            expires_at_ms,
        )
        .await;
    }
    advertise(
        &directory,
        7,
        7,
        "zone-e",
        Digest::from_bytes([11; 32]),
        32 << 20,
        1_000,
        16_000,
    )
    .await;
    advertise(&directory, 8, 8, "zone-f", code, 1 << 20, 1_000, 16_000).await;
    let cell = CellId::from_bytes([12; 32]);
    let owner = SessionId::from_bytes([1; 16]);
    assert!(
        directory
            .select_readers(cell, owner, code, 0, 2_000, 16)
            .await
            .unwrap()
            .is_empty()
    );
    let first = directory
        .select_readers(cell, owner, code, 1, 2_000, 16)
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    assert_ne!(first[0].failure_domain().zone(), Some("zone-a"));
    let two = directory
        .select_readers(cell, owner, code, 2, 2_000, 16)
        .await
        .unwrap();
    assert_ne!(
        two[0].failure_domain().zone(),
        two[1].failure_domain().zone()
    );
    let four = directory
        .select_readers(cell, owner, code, 4, 2_000, 16)
        .await
        .unwrap();
    assert_eq!(four.len(), 4);
    assert!(
        four.iter()
            .all(|candidate| candidate.node() != NodeId::from_bytes([8; 16]))
    );
    assert_eq!(
        four.iter()
            .map(NodeAdvertisement::node)
            .collect::<HashSet<_>>()
            .len(),
        4
    );
    let short = directory
        .select_readers(cell, owner, code, 4, 9_000, 16)
        .await
        .unwrap();
    assert_eq!(short.len(), 3);
    advertise(&directory, 6, 6, "zone-b", code, 32 << 20, 9_000, 16_000).await;
    let replacement = directory
        .select_readers(cell, owner, code, 4, 9_000, 16)
        .await
        .unwrap();
    assert_eq!(replacement.len(), 4);
    assert!(
        replacement
            .iter()
            .any(|candidate| candidate.node() == NodeId::from_bytes([6; 16]))
    );
    assert!(
        replacement
            .iter()
            .all(|candidate| candidate.node() != NodeId::from_bytes([2; 16]))
    );
    let advertisement = directory.load(owner, 9_000).await.unwrap().unwrap();
    directory.withdraw(&advertisement, 9_000).await.unwrap();
    let warm = directory
        .select_readers(cell, owner, code, 4, 9_000, 16)
        .await
        .unwrap();
    assert_eq!(
        warm.iter()
            .map(NodeAdvertisement::node)
            .collect::<HashSet<_>>(),
        replacement
            .iter()
            .map(NodeAdvertisement::node)
            .collect::<HashSet<_>>()
    );
}
