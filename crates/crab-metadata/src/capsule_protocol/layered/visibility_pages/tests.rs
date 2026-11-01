use super::*;
use crab_storage::{Store, StoreLayout};
use object_store::memory::InMemory;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[test]
fn overlapping_ref_closures_encode_once_with_exact_outlier_membership() {
    let snapshot = overlapping_snapshot(32_000, 256, 30);
    let encoded = snapshot.encode().unwrap();
    assert!(encoded.len() < 8 * 1024 * 1024, "{} bytes", encoded.len());
    assert_eq!(
        LayeredVisibilitySnapshot::decode(&encoded).unwrap(),
        snapshot
    );
}

#[test]
#[ignore = "explicit large checkpoint qualification: 1000 refs and 1.6 million shared objects"]
fn million_object_team_checkpoint_preserves_all_closures_within_the_byte_bound() {
    let snapshot = overlapping_snapshot(1_620_211, 1_000, 30);
    let started = std::time::Instant::now();
    let encoded = snapshot.encode().unwrap();
    eprintln!(
        "checkpoint bytes={}, encode={:?}",
        encoded.len(),
        started.elapsed()
    );
    assert!(encoded.len() < 110 * 1024 * 1024, "{} bytes", encoded.len());
    assert_eq!(
        LayeredVisibilitySnapshot::decode(&encoded).unwrap(),
        snapshot
    );
}

fn overlapping_snapshot(common: usize, count: usize, per_ref: usize) -> LayeredVisibilitySnapshot {
    let oid = |position: usize| {
        let mut oid = [0; 20];
        oid.copy_from_slice(&blake3::hash(&(position as u64).to_be_bytes()).as_bytes()[..20]);
        oid
    };
    // Scattered insertions shift ordinary fixed-size leaf boundaries. Keep
    // their real shape instead of appending private OIDs after the common set.
    let mut objects = (0..common + count * per_ref + 1)
        .map(oid)
        .collect::<Vec<_>>();
    objects.sort_unstable();
    let position = |index| objects.binary_search(&oid(index)).unwrap() as u32;
    let mut shared = vec![0_u8; objects.len().div_ceil(8)];
    for index in 0..common {
        let position = position(index) as usize;
        shared[position / 8] |= 1 << (position % 8);
    }
    let mut refs = BTreeMap::new();
    for reference in 0..count {
        let mut bitmap = shared.clone();
        for index in common + reference * per_ref..common + (reference + 1) * per_ref {
            let position = position(index) as usize;
            bitmap[position / 8] |= 1 << (position % 8);
        }
        refs.insert(
            format!("refs/heads/developer-{reference}"),
            GitVisibilityClosure::Bitmap(bitmap),
        );
    }
    refs.insert(
        "refs/tags/old-release".to_owned(),
        GitVisibilityClosure::Sparse(vec![position(0)]),
    );
    LayeredVisibilitySnapshot {
        catalog_digest: "1".repeat(64),
        member_admission: Some(vec![LayeredObjectMember::new(0, 0); objects.len()]),
        objects,
        refs,
        transitions: BTreeMap::new(),
        incremental_history: BTreeMap::new(),
    }
}

fn object(value: u32) -> [u8; 20] {
    let mut oid = [0; 20];
    oid[16..].copy_from_slice(&value.to_be_bytes());
    oid
}

async fn reader_for_bytes(bytes: Bytes) -> (StoreLayout<Store>, LayeredVisibilityReader) {
    let descriptor = Descriptor::decode(&bytes).unwrap();
    let layout = StoreLayout::new(
        Store::new(Arc::new(InMemory::new())),
        "base-pages".to_owned(),
    );
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let range = PackRange::new(0, &bytes).unwrap();
    layout
        .store()
        .put_overwrite(&layout.capsule_checkpoint_path(&hash), bytes)
        .await
        .unwrap();
    let reader =
        LayeredVisibilityReader::new(&layout, &hash, range, descriptor, Vec::new(), 128 * 1024)
            .unwrap();
    (layout, reader)
}

#[tokio::test]
async fn shared_base_membership_matches_each_ref_and_keeps_retention_separate() {
    let objects = (1..=200).map(object).collect::<Vec<_>>();
    let mut refs = BTreeMap::new();
    for index in 0..6 {
        let positions = match index {
            4 => (0..99).chain(std::iter::once(101)).collect(),
            5 => vec![0],
            _ => (0..100).chain(std::iter::once(100 + index)).collect(),
        };
        refs.insert(
            format!("refs/heads/{index}"),
            GitVisibilityClosure::from_positions(positions, objects.len()).unwrap(),
        );
    }
    let snapshot = LayeredVisibilitySnapshot {
        catalog_digest: source_catalog_digest(&[]).unwrap(),
        objects,
        member_admission: None,
        refs,
        transitions: BTreeMap::new(),
        incremental_history: BTreeMap::new(),
    };
    let bytes = snapshot.encode().unwrap();
    assert_eq!(LayeredVisibilitySnapshot::decode(&bytes).unwrap(), snapshot);
    let (layout, reader) = reader_for_bytes(bytes).await;
    let mut requested = (0..=201).map(object).collect::<Vec<_>>();
    requested.extend([object(100), object(200)]);
    for (name, closure) in &snapshot.refs {
        let expected = requested
            .iter()
            .map(|oid| {
                snapshot
                    .objects
                    .binary_search(oid)
                    .ok()
                    .is_some_and(|p| closure.contains(p as u32))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            reader
                .contains_in_ref(layout.store(), name, &requested)
                .await
                .unwrap(),
            Some(expected)
        );
    }
    assert!(
        reader
            .contains_in_ref(layout.store(), "refs/heads/missing", &requested)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        reader
            .dictionary_contains(layout.store(), &[object(200)])
            .await
            .unwrap(),
        [true]
    );
}

#[tokio::test]
async fn authenticated_but_invalid_base_edits_fail_in_bulk_and_point_reads() {
    for (added, removed, query) in [(vec![0], vec![], 1), (vec![], vec![3], 4)] {
        let objects = (1..=4).map(object).collect::<Vec<_>>();
        let mut writer = Writer::default();
        let dictionary = optional_tree(
            &mut writer,
            Kind::CheckpointObjects,
            objects
                .iter()
                .map(|oid| (oid.to_vec(), Vec::new()))
                .collect(),
        )
        .unwrap();
        let base = object_set(&mut writer, &objects, &[0, 1, 2]).unwrap();
        let added = object_set(&mut writer, &objects, &added).unwrap();
        let removed = object_set(&mut writer, &objects, &removed).unwrap();
        let name = "refs/heads/main";
        let row = RefRow {
            name: name.to_owned(),
            relative_to_base: true,
            added,
            removed,
            recent_present: false,
            recent: None,
            history_present: false,
            history: None,
        };
        let refs = optional_tree(
            &mut writer,
            Kind::CheckpointRefs,
            vec![(
                blake3::hash(name.as_bytes()).as_bytes().to_vec(),
                json(&row).unwrap(),
            )],
        )
        .unwrap();
        let descriptor = Descriptor {
            version: 2,
            catalog_digest: source_catalog_digest(&[]).unwrap(),
            body_bytes: writer.bytes.len() as u64,
            body_hash: *blake3::hash(&writer.bytes).as_bytes(),
            object_count: 4,
            member_admission: false,
            objects: dictionary,
            base,
            refs,
        };
        let header = json(&descriptor).unwrap();
        writer.bytes.extend_from_slice(&header);
        crate::pages::put_u32(&mut writer.bytes, header.len()).unwrap();
        writer.bytes.extend_from_slice(MAGIC);
        let bytes = Bytes::from(writer.bytes);
        assert!(LayeredVisibilitySnapshot::decode(&bytes).is_err());
        let (layout, reader) = reader_for_bytes(bytes).await;
        assert!(
            reader
                .contains_in_ref(layout.store(), name, &[object(query)])
                .await
                .is_err()
        );
    }
}

async fn fixture(
    count: u32,
    maximum: u64,
) -> (
    StoreLayout<Store>,
    LayeredCheckpoint,
    LayeredVisibilityReader,
    Arc<AtomicUsize>,
) {
    let mut index = GitVisibilityIndex::new(
        1,
        "1".repeat(64),
        "2".repeat(64),
        BTreeMap::from([
            (
                "refs/heads/main".to_owned(),
                (1..=count).map(|value| format!("{value:040x}")).collect(),
            ),
            (
                "refs/heads/removed".to_owned(),
                vec![format!("{:040x}", count + 1)],
            ),
        ]),
    )
    .unwrap();
    index.remove_ref("refs/heads/removed");
    let pack = CapsuleGitPack::new(
        Bytes::from_static(b"pack"),
        Bytes::from_static(b"index"),
        Bytes::from_static(b"reverse"),
        Bytes::from_static(b"locator"),
        "1".repeat(40),
        u64::from(count) + 1,
    )
    .unwrap();
    let source = PackLayer::build(&pack)
        .unwrap()
        .source_descriptor()
        .unwrap();
    let digest = source_catalog_digest(std::slice::from_ref(&source)).unwrap();
    let visibility = LayeredVisibilitySnapshot::from_index_with_member_admission(
        &index,
        &digest,
        vec![LayeredObjectMember::new(0, 0); count as usize + 1],
    )
    .unwrap();
    let checkpoint = LayeredCheckpoint::build(
        1,
        &"3".repeat(64),
        vec![source],
        PointerCatalog::new(),
        Some(visibility),
    )
    .unwrap();
    let reads = Arc::new(AtomicUsize::new(0));
    let observed = reads.clone();
    let store =
        Store::new(Arc::new(InMemory::new())).with_read_request_observer(Arc::new(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
        }));
    let layout = StoreLayout::new(store, "checkpoint-pages".to_owned());
    layout
        .store()
        .put_overwrite(
            &layout.capsule_checkpoint_path(checkpoint.hash()),
            checkpoint.bytes().clone(),
        )
        .await
        .unwrap();
    let control = LayeredCheckpoint::decode_control(
        checkpoint
            .bytes()
            .slice(checkpoint.control_offset() as usize..),
        checkpoint.bytes().len() as u64,
        checkpoint.control_offset(),
        checkpoint.hash(),
        checkpoint.footer_hash(),
    )
    .unwrap();
    let reader = control
        .visibility_reader(&layout, maximum)
        .unwrap()
        .unwrap();
    (layout, checkpoint, reader, reads)
}

#[tokio::test]
async fn paged_control_replay_batches_checkpoint_dictionary_reads() {
    use crate::capsule_protocol::{
        Capsule, CapsuleRefEdit, CapsuleRun, CapsuleSection, CapsuleSectionKind,
        CapsuleTransaction, CapsuleVisibilityDelta, CapturedVisibility, VisibilityReconciliation,
        order_ref_controls,
    };
    use crate::git_visibility::GitVisibilityEdit;
    let (layout, _, reader, reads) = fixture(400_000, 128 * 1024 * 1024).await;
    let name = "refs/heads/main";
    let mut pending = BTreeMap::new();
    let mut old = format!("{:040x}", 1);
    for batch in 0_u32..64 {
        let added = (0..64)
            .map(|index| format!("{:040x}", ((batch * 64 + index + 1) * 9973) % 400_000 + 1))
            .collect::<Vec<_>>();
        let new = added[0].clone();
        let transaction = CapsuleTransaction::new(
            &"1".repeat(64),
            vec![CapsuleRefEdit::new(
                name,
                Some(old.clone()),
                Some(new.clone()),
                None,
            )],
        )
        .unwrap();
        let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
            name.to_owned(),
            GitVisibilityEdit::from_delta_objects(Some(old), new.clone(), added, Vec::new()),
        )]))
        .unwrap();
        let capsule = Capsule::build(
            &transaction,
            Vec::new(),
            vec![CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                visibility.encode().unwrap(),
            )],
        )
        .unwrap();
        let run = CapsuleRun::leaf(capsule).unwrap();
        layout
            .store()
            .create_strict(&layout.capsule_path(run.hash()), run.bytes().clone())
            .await
            .unwrap();
        let pointer = crate::capsule_protocol::CapsulePointer::new(
            run.hash(),
            run.bytes().len() as u64,
            run.control_offset(),
            run.control_size(),
            run.footer_hash(),
            run.level(),
            run.transaction_ids(),
            run.newest_base_root_digest(),
        )
        .unwrap();
        let (_, controls) =
            crate::capsule_protocol::load_capsule_run_git_control(&layout, &pointer)
                .await
                .unwrap();
        for control in controls {
            pending.insert(control.transaction_id().to_owned(), control);
        }
        old = new;
    }
    let initial = CapturedVisibility::from_checkpoint(
        layout.store().clone(),
        Arc::new(reader),
        std::iter::once(name.to_owned()),
    )
    .await
    .unwrap();
    reads.store(0, Ordering::SeqCst);
    let ordered = order_ref_controls(
        &BTreeMap::from([(name.to_owned(), format!("{:040x}", 1))]),
        &BTreeMap::new(),
        &mut VisibilityReconciliation::Paged(Box::new(initial)),
        &BTreeMap::new(),
        pending,
    )
    .await
    .unwrap();
    assert_eq!(ordered.len(), 64);
    assert!(
        reads.load(Ordering::SeqCst) < 1000,
        "checkpoint dictionary pages were repeatedly fetched: {}",
        reads.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn paged_member_selection_preserves_order_misses_and_cache_reuse() {
    let (layout, checkpoint, reader, reads) = fixture(50_000, 128 * 1024).await;
    let requested = [object(42), object(50_001), object(99_999), object(42)];
    let expected = vec![
        Some(&checkpoint.sources()[0].members()[0]),
        Some(&checkpoint.sources()[0].members()[0]),
        None,
        Some(&checkpoint.sources()[0].members()[0]),
    ];
    assert_eq!(
        reader
            .object_members(layout.store(), &requested)
            .await
            .unwrap(),
        expected
    );
    let before = reads.load(Ordering::SeqCst);
    assert_eq!(
        reader
            .object_members(layout.store(), &requested)
            .await
            .unwrap(),
        expected
    );
    assert_eq!(reads.load(Ordering::SeqCst), before);
}

#[tokio::test]
async fn checkpoint_membership_is_bounded_and_retained_objects_do_not_grant_visibility() {
    let (layout, checkpoint, reader, _) = fixture(50_000, 128 * 1024).await;
    assert!(checkpoint.bytes().len() > 2 * 1024 * 1024);
    let requested = [object(42), object(50_001), object(99_999), object(42)];
    assert_eq!(
        reader
            .contains_in_ref(layout.store(), "refs/heads/main", &requested)
            .await
            .unwrap(),
        Some(vec![true, false, false, true])
    );
    assert!(
        reader
            .dictionary_contains(layout.store(), &[object(50_001)])
            .await
            .unwrap()[0]
    );
    assert!(
        reader
            .contains_in_ref(layout.store(), "refs/heads/removed", &requested)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn warm_checkpoint_membership_rereads_no_origin_pages() {
    let (layout, _, reader, reads) = fixture(50_000, 128 * 1024).await;
    let requested = [object(42), object(50_000)];
    reader
        .contains_in_ref(layout.store(), "refs/heads/main", &requested)
        .await
        .unwrap();
    let before = reads.load(Ordering::SeqCst);
    reader
        .contains_in_ref(layout.store(), "refs/heads/main", &requested)
        .await
        .unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), before);
}

#[tokio::test]
async fn bulk_checkpoint_visibility_matches_the_captured_complete_proof() {
    let (layout, checkpoint, reader, _) = fixture(1000, 128 * 1024 * 1024).await;
    assert_eq!(
        reader.materialize(layout.store()).await.unwrap().as_ref(),
        checkpoint.visibility_ordinal_snapshot().unwrap().unwrap()
    );
}

#[tokio::test]
async fn checkpoint_page_tampering_and_missing_pages_fail_instead_of_membership_misses() {
    for missing in [false, true] {
        let (layout, checkpoint, reader, _) = fixture(1000, 128 * 1024).await;
        let descriptor = checkpoint.footer.visibility_pages.as_ref().unwrap();
        let section = checkpoint
            .footer
            .sections
            .iter()
            .find(|section| section.kind == CapsuleSectionKind::VisibilityOrdinalSnapshot)
            .unwrap();
        let path = layout.capsule_checkpoint_path(checkpoint.hash());
        if missing {
            layout.store().delete(&path).await.unwrap();
        } else {
            let mut bytes = checkpoint.bytes().to_vec();
            bytes[(section.offset + descriptor.objects.as_ref().unwrap().offset) as usize] ^= 1;
            layout
                .store()
                .put_overwrite(&path, bytes.into())
                .await
                .unwrap();
        }
        assert!(
            reader
                .dictionary_contains(layout.store(), &[object(42)])
                .await
                .is_err()
        );
        assert!(
            reader
                .object_members(layout.store(), &[object(42)])
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn checkpoint_clones_share_the_verified_complete_visibility_proof() {
    let (_, checkpoint, _, _) = fixture(1000, 128 * 1024 * 1024).await;
    let checkpoint = LayeredCheckpoint::decode(checkpoint.bytes().clone()).unwrap();
    let cloned = checkpoint.clone();
    assert!(std::ptr::eq(
        checkpoint.visibility_ordinal_snapshot().unwrap().unwrap(),
        cloned.visibility_ordinal_snapshot().unwrap().unwrap(),
    ));
}
