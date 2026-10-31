//! Explicit retained-fixture differential; ordinary unit tests need no large repository.
use crab_git::shared_visibility::{Limits, build};
use gix_hash::ObjectId;
use sha2::{Digest, Sha256};
use std::{path::PathBuf, time::Instant};

struct Archive<'a>(&'a [u8]);
impl<'a> Archive<'a> {
    fn take(&mut self, count: usize) -> &'a [u8] {
        let (bytes, remaining) = self.0.split_at(count);
        self.0 = remaining;
        bytes
    }
    fn number(&mut self) -> usize {
        u32::from_le_bytes(self.take(4).try_into().unwrap()) as usize
    }
}
#[test]
#[ignore = "requires the retained4380-ref git/git archive and a complete local physical object database"]
fn retained_git_repository_shared_graph_matches_every_native_qualified_membership_bit() {
    let root = PathBuf::from(
        std::env::var_os("CRAB_SHARED_VISIBILITY_FIXTURE")
            .expect("explicit retained fixture required"),
    );
    let git_dir = PathBuf::from(
        std::env::var_os("CRAB_SHARED_GRAPH_GIT_DIR")
            .expect("explicit Git object database required"),
    );
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("report.json")).unwrap()).unwrap();
    let refs = report["refs"].as_object().unwrap();
    let roots = refs
        .values()
        .map(|value| ObjectId::from_hex(value["tip"].as_str().unwrap().as_bytes()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(roots.len(), 4380);
    let started = Instant::now();
    let result = build(&git_dir, &roots, Limits::default(), || false).unwrap();
    let elapsed = started.elapsed().as_millis();
    assert_eq!(result.objects().len(), 854492);
    assert_eq!(result.distinct_links, 137154748);
    assert_eq!(result.structural_bytes, 6066737217);
    let bytes = std::fs::read(root.join("memberships.bin")).unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(&bytes)),
        "5a75b4365d7995af0e14a24a7026a951e2bb76833e5dce3d65cea25d85f84d6e"
    );
    let mut archive = Archive(&bytes);
    assert_eq!(archive.take(8), b"CRBSHPR1");
    let count = archive.number();
    let ref_count = archive.number();
    let chunk_bits = archive.number();
    let pool_count = archive.number();
    assert_eq!((count, ref_count, chunk_bits), (854492, 4380, 1024));
    for oid in result.objects() {
        assert_eq!(archive.take(20), oid.as_bytes());
    }
    let pool = archive.take(pool_count.checked_mul(128).unwrap());
    let slots = count.div_ceil(chunk_bits);
    let mut chunks_compared = 0_u64;
    let mut logical_memberships = 0_u64;
    for ((name, expected), closure) in refs.iter().zip(result.closures()) {
        let length = archive.number();
        assert_eq!(archive.take(length), name.as_bytes());
        assert_eq!(archive.number(), slots);
        assert_eq!(closure.chunks().len(), slots);
        assert_eq!(closure.len() as u64, expected["objects"].as_u64().unwrap());
        for chunk in closure.chunks() {
            let id = archive.number();
            assert!(id < pool_count);
            assert_eq!(
                chunk.as_slice(),
                &pool[id * 128..(id + 1) * 128],
                "exact membership chunk for {name}"
            );
            chunks_compared += 1;
        }
        logical_memberships += closure.len() as u64;
    }
    assert!(archive.0.is_empty());
    assert_eq!(logical_memberships, 1219056589);
    let evidence = serde_json::json!({"schema":"crab.production-shared-graph-qualification","status":"passed","refs":ref_count,"distinct_objects":count,"distinct_links":result.distinct_links,"parsed_links":result.parsed_links,"verified_structural_bytes":result.structural_bytes,"logical_memberships":logical_memberships,"exact_chunks_compared":chunks_compared,"build_elapsed_ms":elapsed,"archive_sha256":"5a75b4365d7995af0e14a24a7026a951e2bb76833e5dce3d65cea25d85f84d6e","checks":["complete sorted raw OID dictionary equality","every input tip in exact original order","every bit of every4380 ref closures","all per-ref counts","checksum-verified structural objects","unchanged bounded resource defaults"],"scope":"Physical Git graph production module differential against the archive independently qualified with all4380 native closures. Not a completed RustFS publication or final push benchmark."});
    std::fs::write(
        root.join("production-shared-graph-report.json"),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
    println!("{evidence}");
}
