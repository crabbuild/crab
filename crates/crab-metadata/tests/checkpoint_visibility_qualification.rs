//! Wire compatibility and explicit latency checks for checkpoint visibility.
use bytes::Bytes;
use crab_metadata::capsule_protocol::{
    CapsuleGitPack, LayeredCheckpoint, LayeredObjectMember, LayeredVisibilitySnapshot, PackLayer,
    PointerCatalog, source_catalog_digest,
};
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

fn repeated_sets_checkpoint(refs: usize, objects_per_ref: usize) -> LayeredCheckpoint {
    let index = GitVisibilityIndex::new(
        1,
        "1".repeat(64),
        "2".repeat(64),
        (0..refs)
            .map(|r| {
                (
                    format!("refs/heads/branch-{r}"),
                    (0..objects_per_ref)
                        .map(|oid| format!("{oid:040x}"))
                        .collect(),
                )
            })
            .chain(std::iter::once((
                "refs/heads/dictionary".to_owned(),
                (0..2000).map(|oid| format!("{oid:040x}")).collect(),
            )))
            .collect(),
    )
    .unwrap();
    let pack = CapsuleGitPack::new(
        Bytes::from_static(b"pack"),
        Bytes::from_static(b"index"),
        Bytes::from_static(b"reverse"),
        Bytes::from_static(b"locator"),
        "1".repeat(40),
        2000,
    )
    .unwrap();
    let source = PackLayer::build(&pack)
        .unwrap()
        .source_descriptor()
        .unwrap();
    let digest = source_catalog_digest(std::slice::from_ref(&source)).unwrap();
    let snapshot = LayeredVisibilitySnapshot::from_index_with_member_admission(
        &index,
        &digest,
        vec![LayeredObjectMember::new(0, 0); 2000],
    )
    .unwrap();
    LayeredCheckpoint::build(
        1,
        &"3".repeat(64),
        vec![source],
        PointerCatalog::new(),
        Some(snapshot),
    )
    .unwrap()
}

#[test]
fn retention_charge_includes_repeated_sets_and_excludes_unknown_backing_buffers() {
    // Dense closures retain bitmaps; sparse closures retain u32 ordinals.
    // Remove encoded/footer charges before comparing decoded allocations:
    // shared wire pages can be smaller even while resident memberships grow.
    let decoded_charge = |checkpoint: &LayeredCheckpoint| {
        checkpoint.retained_metadata_bytes().unwrap()
            - checkpoint.bytes().len()
            - checkpoint.control_size() as usize * 4
    };
    for (objects_per_ref, minimum_set_bytes) in [(2000, 2000_usize.div_ceil(8)), (62, 62 * 4)] {
        let one = repeated_sets_checkpoint(1, objects_per_ref);
        let repeated = repeated_sets_checkpoint(64, objects_per_ref);
        assert!(decoded_charge(&repeated) >= decoded_charge(&one) + 63 * minimum_set_bytes);
        let mut oversized = vec![0; repeated.bytes().len() + 1024 * 1024];
        oversized[..repeated.bytes().len()].copy_from_slice(repeated.bytes());
        let input = Bytes::from(oversized).slice(..repeated.bytes().len());
        let decoded = LayeredCheckpoint::decode(input).unwrap();
        assert_eq!(decoded, repeated);
        decoded.visibility_ordinal_snapshot().unwrap();
        assert!(decoded.retained_metadata_bytes().is_none());
    }
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
