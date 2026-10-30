//! Explicit full-input codec qualification; the retained fixture is outside source control.
use bytes::Bytes;
#[cfg(feature = "storage")]
use crab_metadata::capsule_protocol::VisibilityChange;
use crab_metadata::capsule_protocol::{
    Capsule, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind, CapsuleTransaction,
    CapsuleVisibilityDelta,
};
use crab_metadata::git_visibility::{
    GIT_VISIBILITY_EDIT_VERSION, GitVisibilityEdit, MAX_GIT_VISIBILITY_INDEX_BYTES,
};
use crab_metadata::git_visibility::{
    GitVisibilityDictionary, GitVisibilityObjects, VISIBILITY_CHUNK_BYTES,
};
use std::collections::BTreeMap;
use std::{path::PathBuf, sync::Arc, time::Instant};

fn encode_oid(oid: &[u8; 20]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut bytes = Vec::with_capacity(40);
    for byte in oid {
        bytes.push(HEX[(byte >> 4) as usize]);
        bytes.push(HEX[(byte & 15) as usize]);
    }
    String::from_utf8(bytes).unwrap()
}

struct Input<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Input<'a> {
    fn take(&mut self, len: usize) -> &'a [u8] {
        let end = self
            .offset
            .checked_add(len)
            .expect("fixture length overflow");
        let value = self.bytes.get(self.offset..end).expect("truncated fixture");
        self.offset = end;
        value
    }
    fn u32(&mut self) -> usize {
        u32::from_le_bytes(self.take(4).try_into().unwrap()) as usize
    }
}

#[test]
#[ignore = "requires the retained git/git all-ref membership fixture"]
fn retained_git_repository_shared_codec_round_trip() {
    let root = PathBuf::from(
        std::env::var_os("CRAB_SHARED_VISIBILITY_FIXTURE")
            .expect("explicit fixture directory required"),
    );
    let archive = root.join("memberships.bin");
    assert!(std::fs::metadata(&archive).unwrap().len() <= MAX_GIT_VISIBILITY_INDEX_BYTES);
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("report.json")).unwrap()).unwrap();
    let bytes = std::fs::read(&archive).unwrap();
    let mut input = Input {
        bytes: &bytes,
        offset: 0,
    };
    assert_eq!(input.take(8), b"CRBSHPR1");
    let count = input.u32();
    let refs = input.u32();
    let bits = input.u32();
    let pools = input.u32();
    assert_eq!((count, refs, bits, pools), (854492, 4380, 1024, 450021));
    let dictionary = GitVisibilityDictionary::new(
        (0..count)
            .map(|_| encode_oid(input.take(20).try_into().unwrap()))
            .collect(),
    )
    .unwrap();
    let pool = (0..pools)
        .map(|_| {
            Arc::new(
                <[u8; VISIBILITY_CHUNK_BYTES]>::try_from(input.take(VISIBILITY_CHUNK_BYTES))
                    .unwrap(),
            )
        })
        .collect::<Vec<_>>();
    let mut edits = BTreeMap::new();
    for _ in 0..refs {
        let len = input.u32();
        let name = std::str::from_utf8(input.take(len)).unwrap().to_owned();
        let slots = input.u32();
        assert_eq!(slots, count.div_ceil(bits));
        let chunks = (0..slots)
            .map(|_| {
                pool.get(input.u32())
                    .expect("fixture chunk out of range")
                    .clone()
            })
            .collect();
        let added = GitVisibilityObjects::from_shared_chunks(dictionary.clone(), chunks).unwrap();
        assert_eq!(
            added.len() as u64,
            report["refs"][&name]["objects"].as_u64().unwrap()
        );
        let new_oid = report["refs"][&name]["tip"].as_str().unwrap().to_owned();
        let edit = GitVisibilityEdit {
            version: GIT_VISIBILITY_EDIT_VERSION,
            old_oid: None,
            new_oid,
            replaces: true,
            added,
            removed: GitVisibilityObjects::default(),
        };
        assert!(edits.insert(name, edit).is_none());
    }
    assert_eq!(input.offset, bytes.len());
    let delta = CapsuleVisibilityDelta::new(edits).unwrap();
    let started = Instant::now();
    let encoded = delta.encode().unwrap();
    let encode_ms = started.elapsed().as_millis();
    assert!(encoded.len() as u64 <= MAX_GIT_VISIBILITY_INDEX_BYTES);
    let end = encoded.len() - 12;
    let header_length = u32::from_le_bytes(encoded[end..end + 4].try_into().unwrap()) as usize;
    let header: serde_json::Value =
        serde_json::from_slice(&encoded[end - header_length..end]).unwrap();
    assert_eq!(header["version"], 4);
    let transaction = CapsuleTransaction::new(
        &"1".repeat(64),
        delta
            .edits()
            .iter()
            .map(|(name, edit)| {
                CapsuleRefEdit::new(name, edit.old_oid.clone(), Some(edit.new_oid.clone()), None)
            })
            .collect(),
    )
    .unwrap();
    let capsule = Capsule::build(
        &transaction,
        vec![],
        vec![CapsuleSection::new(
            CapsuleSectionKind::VisibilityDelta,
            encoded.clone(),
        )],
    )
    .unwrap();
    let started = Instant::now();
    let decoded = capsule.visibility_delta().unwrap().unwrap();
    let decode_ms = started.elapsed().as_millis();
    for ((name, expected), (decoded_name, actual)) in delta.edits().iter().zip(decoded.edits()) {
        assert_eq!(name, decoded_name);
        assert_eq!(expected.old_oid, actual.old_oid);
        assert_eq!(expected.new_oid, actual.new_oid);
        assert!(actual.replaces);
        assert!(actual.removed.is_empty());
        assert_eq!(expected.added.len(), actual.added.len());
        assert!(
            expected.added == actual.added,
            "every canonical OID must survive {name}"
        );
    }
    let output = root.join("shared-capsule-visibility.bin");
    std::fs::write(&output, &encoded).unwrap();
    let evidence = serde_json::json!({"schema":"crab.shared-capsule-codec-qualification","status":"passed","refs":refs,"objects":count,"encoded_bytes":encoded.len(),"byte_limit":MAX_GIT_VISIBILITY_INDEX_BYTES,"encoded_blake3":blake3::hash(&encoded).to_hex().to_string(),"encode_ms":encode_ms,"decode_including_canonical_reencode_ms":decode_ms,"checks":["exact full fixture shape","every ref tip and object count","every original canonical OID in every ref closure","canonical decode and re-encode","unchanged128MiB encoded ceiling"],"scope":"Offline transaction codec qualification; not a successful Crab all-ref push or layered checkpoint qualification."});
    std::fs::write(
        root.join("shared-capsule-codec-report.json"),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
    println!("{evidence}");
}

#[cfg(feature = "storage")]
#[tokio::test]
#[ignore = "requires the retained git/git all-ref membership fixture and encoded proof"]
async fn retained_git_repository_shared_point_reads_stay_bounded() {
    use crab_metadata::capsule_protocol::CapsuleRun;
    use crab_storage::{Store, StoreLayout};
    use std::sync::atomic::{AtomicU64, Ordering};
    let root = PathBuf::from(
        std::env::var_os("CRAB_SHARED_VISIBILITY_FIXTURE")
            .expect("explicit fixture directory required"),
    );
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("report.json")).unwrap()).unwrap();
    let codec_report: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("shared-capsule-codec-report.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(codec_report["status"], "passed");
    let encoded = Bytes::from(std::fs::read(root.join("shared-capsule-visibility.bin")).unwrap());
    assert_eq!(
        blake3::hash(&encoded).to_hex().to_string(),
        codec_report["encoded_blake3"].as_str().unwrap()
    );
    let archive = std::fs::read(root.join("memberships.bin")).unwrap();
    let mut input = Input {
        bytes: &archive,
        offset: 0,
    };
    assert_eq!(input.take(8), b"CRBSHPR1");
    let count = input.u32();
    let refs = input.u32();
    let bits = input.u32();
    let pools = input.u32();
    assert_eq!((count, refs, bits, pools), (854492, 4380, 1024, 450021));
    let dictionary = (0..count)
        .map(|_| <[u8; 20]>::try_from(input.take(20)).unwrap())
        .collect::<Vec<_>>();
    let positions = [0, 1, 1023, 1024, 4096, count / 2, count - 1];
    let mut wants = positions.map(|index| dictionary[index]).to_vec();
    assert!(dictionary.binary_search(&[0xff; 20]).is_err());
    wants.push([0xff; 20]);
    wants.push(dictionary[0]);
    let pool = (0..pools)
        .map(|_| {
            <[u8; VISIBILITY_CHUNK_BYTES]>::try_from(input.take(VISIBILITY_CHUNK_BYTES)).unwrap()
        })
        .collect::<Vec<_>>();
    let mut expected = BTreeMap::new();
    for _ in 0..refs {
        let length = input.u32();
        let name = std::str::from_utf8(input.take(length)).unwrap().to_owned();
        let slots = input.u32();
        assert_eq!(slots, count.div_ceil(bits));
        let ids = (0..slots).map(|_| input.u32()).collect::<Vec<_>>();
        let mut values = positions
            .map(|position| {
                pool[ids[position / bits]][position % bits / 8] & (1 << (position % 8)) != 0
            })
            .to_vec();
        values.push(false);
        values.push(values[0]);
        expected.insert(name, values);
    }
    assert_eq!(input.offset, archive.len());
    let transaction = CapsuleTransaction::new(
        &"1".repeat(64),
        report["refs"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, value)| {
                CapsuleRefEdit::new(
                    name,
                    None,
                    Some(value["tip"].as_str().unwrap().to_owned()),
                    None,
                )
            })
            .collect(),
    )
    .unwrap();
    let capsule = Capsule::build(
        &transaction,
        vec![],
        vec![CapsuleSection::new(
            CapsuleSectionKind::VisibilityDelta,
            encoded.clone(),
        )],
    )
    .unwrap();
    let run = CapsuleRun::leaf(capsule.clone()).unwrap();
    let observed = Arc::new(AtomicU64::new(0));
    let counter = observed.clone();
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_read_byte_observer(Arc::new(move |bytes| {
            counter.fetch_add(bytes, Ordering::Relaxed);
        }));
    let layout = StoreLayout::new(store, "repositories/retained-shared-point-reads".into());
    layout
        .store()
        .create_strict(&layout.capsule_path(run.hash()), run.bytes().clone())
        .await
        .unwrap();
    let (control, _) = run.verified_controls().unwrap();
    let reader = control
        .visibility_reader(&layout, capsule.hash(), 2 * 1024 * 1024)
        .unwrap()
        .unwrap();
    observed.store(0, Ordering::Relaxed);
    let started = Instant::now();
    let mut calls = 0;
    for (name, bits) in expected.iter().step_by(250).take(16) {
        let actual = reader
            .lookup(layout.store(), name, &wants)
            .await
            .unwrap()
            .unwrap();
        let expected = bits
            .iter()
            .map(|present| present.then_some(VisibilityChange::Added))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "exact ref membership {name}");
        calls += 1;
    }
    assert_eq!(
        reader
            .lookup(layout.store(), "refs/heads/does-not-exist", &wants)
            .await
            .unwrap(),
        None
    );
    let bytes_read = observed.load(Ordering::Relaxed);
    assert!(
        bytes_read > 0 && bytes_read < encoded.len() as u64 / 4,
        "point reads must not download the full126MiB proof: {bytes_read}"
    );
    let evidence = serde_json::json!({"schema":"crab.shared-capsule-point-read-qualification","status":"passed","fixture_refs":refs,"dictionary_objects":count,"point_calls":calls,"oids_per_call":wants.len(),"maximum_per_call_proof_work_bytes":2*1024*1024,"backend_bytes_read":bytes_read,"full_proof_bytes":encoded.len(),"elapsed_ms":started.elapsed().as_millis(),"checks":["authenticated capsule/run control path","cold page-cache point reads","exact per-ref membership including misses","request ordering and duplicate OIDs","authenticated absent ref","no full proof download"],"scope":"Offline in-memory provider integration. Not RustFS all-ref publication or final performance qualification."});
    std::fs::write(
        root.join("shared-capsule-point-read-report.json"),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .unwrap();
    println!("{evidence}");
}
