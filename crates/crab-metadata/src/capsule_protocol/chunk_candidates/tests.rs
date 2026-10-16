use super::*;
use crate::capsule_protocol::XorbChunkEntry;

fn fixture() -> (MerkleHash, XorbCatalogEntry, OriginReceipt) {
    let hash = MerkleHash::from([3; 32]);
    let entry = XorbCatalogEntry::new(
        100,
        blake3::hash(b"whole body").to_hex().to_string(),
        vec![
            XorbChunkEntry::new(MerkleHash::from([1; 32]).hex(), 9),
            XorbChunkEntry::new(MerkleHash::from([1; 32]).hex(), 9),
        ],
    );
    let mut origin = OriginReceipt::new(
        "canonical-origin".to_owned(),
        format!(".crab/xorbs/03/{}", hash.hex()),
        hash.into(),
        *blake3::hash(b"payload").as_bytes(),
        100,
        Some("etag".to_owned()),
        Some("version".to_owned()),
    );
    origin.proven_at_unix_secs = 1;
    (hash, entry, origin)
}

#[test]
fn capsule_candidates_preserve_distinct_digests_and_first_occurrence() {
    let (hash, entry, origin) = fixture();
    let rows =
        capsule_chunk_candidate_rows("repo", &"2".repeat(64), hash, &entry, &origin).unwrap();
    let placement = CommittedChunkPlacement::decode(&rows[1].1).unwrap();
    let anchor = CapsuleChunkAnchor::decode(&placement.source_anchor_id, &rows[0].1).unwrap();
    assert_eq!(
        (
            rows.len(),
            placement.chunk_index,
            anchor.body_digest,
            anchor.origin.payload_digest
        ),
        (3, 0, entry.body_digest().to_owned(), origin.payload_digest)
    );
}

#[test]
fn capsule_anchor_rejects_changed_source_identity() {
    let (hash, entry, origin) = fixture();
    let rows =
        capsule_chunk_candidate_rows("repo", &"2".repeat(64), hash, &entry, &origin).unwrap();
    let placement = CommittedChunkPlacement::decode(&rows[1].1).unwrap();
    let mut anchor = CapsuleChunkAnchor::decode(&placement.source_anchor_id, &rows[0].1).unwrap();
    anchor.source_repo_prefix = "other".to_owned();
    assert!(
        CapsuleChunkAnchor::decode(
            &placement.source_anchor_id,
            &serde_json::to_vec(&anchor).unwrap()
        )
        .is_err()
    );
}

#[test]
fn capsule_anchor_requires_the_authenticated_physical_descriptor() {
    let (hash, entry, origin) = fixture();
    let rows =
        capsule_chunk_candidate_rows("repo", &"2".repeat(64), hash, &entry, &origin).unwrap();
    let placement = CommittedChunkPlacement::decode(&rows[1].1).unwrap();
    let anchor = CapsuleChunkAnchor::decode(&placement.source_anchor_id, &rows[0].1).unwrap();
    let other = XorbCatalogEntry::new(100, "4".repeat(64), entry.chunks().to_vec());
    assert!(
        anchor
            .validate_catalog_xorb(&origin.object_key, &other)
            .is_err()
    );
}

#[test]
fn capsule_anchor_rejects_oversized_records_before_decoding() {
    assert!(CapsuleChunkAnchor::decode(&[1; 32], &vec![b' '; 16 * 1024 + 1]).is_err());
}

fn rooted_fixture() -> (
    CapsuleChunkAnchor,
    CommittedChunkPlacement,
    crate::capsule_protocol::PointerCatalog,
) {
    let (hash, entry, origin) = fixture();
    let shard = "2".repeat(64);
    let rows = capsule_chunk_candidate_rows("repo", &shard, hash, &entry, &origin).unwrap();
    let placement = CommittedChunkPlacement::decode(&rows[1].1).unwrap();
    let anchor = CapsuleChunkAnchor::decode(&placement.source_anchor_id, &rows[0].1).unwrap();
    let mut catalog = crate::capsule_protocol::PointerCatalog::new();
    catalog.insert_xorb(hash.hex(), entry).unwrap();
    catalog
        .insert_shard(
            shard,
            crate::capsule_protocol::ShardCatalogEntry::new(20, vec![hash.hex()]),
        )
        .unwrap();
    (anchor, placement, catalog)
}

#[test]
fn capsule_placement_requires_an_authenticated_source_shard() {
    let (anchor, placement, rooted) = rooted_fixture();
    let mut unrooted = crate::capsule_protocol::PointerCatalog::new();
    for (hash, entry) in rooted.xorbs() {
        unrooted.insert_xorb(hash.clone(), entry.clone()).unwrap();
    }
    assert!(
        anchor
            .validate_placement(&placement, &unrooted, &anchor.origin.object_key)
            .is_err()
    );
}

#[test]
fn capsule_placement_accepts_exact_rooted_content() {
    let (anchor, placement, catalog) = rooted_fixture();
    assert!(
        anchor
            .validate_placement(&placement, &catalog, &anchor.origin.object_key)
            .is_ok()
    );
}

#[test]
fn capsule_placement_rejects_tampered_occurrences_and_proofs() {
    let (anchor, placement, catalog) = rooted_fixture();
    for field in ["size", "position", "chunk", "origin", "source"] {
        let mut changed = placement.clone();
        match field {
            "size" => changed.uncompressed_size += 1,
            "position" => changed.chunk_index = 2,
            "chunk" => changed.chunk_hash = [9; 32],
            "origin" => changed.origin_proof_id = [9; 32],
            "source" => changed.source_anchor_id = [9; 32],
            _ => unreachable!(),
        }
        assert!(
            anchor
                .validate_placement(&changed, &catalog, &anchor.origin.object_key)
                .is_err(),
            "accepted changed {field}"
        );
    }
}
