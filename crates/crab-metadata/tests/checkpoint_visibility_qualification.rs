//! Wire compatibility and explicit latency checks for checkpoint visibility.
use crab_metadata::capsule_protocol::{LayeredObjectMember, LayeredVisibilitySnapshot};
use crab_metadata::git_visibility::GitVisibilityIndex;
use std::collections::BTreeMap;

fn large_visibility_snapshot() -> LayeredVisibilitySnapshot {
    let index = GitVisibilityIndex::new(
        1,
        "1".repeat(64),
        "2".repeat(64),
        BTreeMap::from([(
            "refs/heads/main".to_owned(),
            (0..200_000).map(|value| format!("{value:040x}")).collect(),
        )]),
    )
    .unwrap();
    LayeredVisibilitySnapshot::from_index_with_member_admission(
        &index,
        &"3".repeat(64),
        vec![LayeredObjectMember::new(0, 0); 200_000],
    )
    .unwrap()
}

#[test]
fn streamed_checkpoint_pages_keep_the_existing_wire_identity() {
    let snapshot = large_visibility_snapshot();
    let bytes = snapshot.encode().unwrap();
    // Captured with the original owned-row encoder, including its page splits,
    // deduplication, member byte order, directories and canonical descriptor.
    assert_eq!(bytes.len(), 10_434_356);
    assert_eq!(
        blake3::hash(&bytes).to_hex().as_str(),
        "b9f620b1aaf6b3d3f26e1ae5461036f455247f4aa4c1afa265421b00ff0e5c0d"
    );
}

#[test]
#[ignore = "explicit checkpoint encoder latency diagnostic"]
fn checkpoint_visibility_encode_latency() {
    let snapshot = large_visibility_snapshot();
    for iteration in 0..5 {
        let started = std::time::Instant::now();
        let bytes = snapshot.encode().unwrap();
        let encode_ms = started.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "checkpoint_visibility_latency iteration={iteration} encode_ms={encode_ms:.3} bytes={} hash={}",
            bytes.len(),
            blake3::hash(&bytes)
        );
    }
}
