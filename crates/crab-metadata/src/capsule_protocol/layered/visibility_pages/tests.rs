use super::*;
use crab_storage::{Store, StoreLayout};
use object_store::memory::InMemory;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn object(value: u32) -> [u8; 20] {
    let mut oid = [0; 20];
    oid[16..].copy_from_slice(&value.to_be_bytes());
    oid
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
