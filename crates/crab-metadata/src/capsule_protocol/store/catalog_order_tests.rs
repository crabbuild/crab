#![expect(clippy::unwrap_used, reason = "test assertions")]

use super::*;
use crate::capsule_protocol::{
    Capsule, CapsuleGitPack, CapsulePointer, CapsuleRefEdit, CapsuleSection, CapsuleTransaction,
    CapsuleVisibilityDelta, CheckpointPointer, FileCatalogEntry, HistorySegmentState,
    LayeredObjectMember, LayeredVisibilitySnapshot, PackSourceDescriptor, RepositoryRoot,
    ShardCatalogEntry, VisibilityReconciliation, XorbCatalogEntry, XorbChunkEntry,
    apply_control_refs, load_pointer_catalog_lookup_from_root, order_ref_controls,
    source_catalog_digest,
};
use crate::git_visibility::{GitVisibilityEdit, GitVisibilityIndex};
use bytes::Bytes;
use std::sync::Arc;

async fn expose(
    layout: &StoreLayout<Store>,
    root: &RootSnapshot,
    transaction: &CapsuleTransaction,
    pointer: CapsulePointer,
) {
    let edit = &transaction.edits()[0];
    let head = CapsuleRefHead::from_root(
        edit.ref_name(),
        root.record().root().ref_epoch().to_owned(),
        None,
        None,
    )
    .unwrap();
    let state = head
        .successor_state(
            &BTreeSet::new(),
            edit.new_oid().map(str::to_owned),
            None,
            transaction.id().unwrap(),
            vec![pointer],
        )
        .unwrap();
    layout
        .store()
        .create_strict(
            &layout.capsule_ref_head_path(&capsule_ref_name_key(edit.ref_name())),
            head.commit(state).unwrap().encode().unwrap(),
        )
        .await
        .unwrap();
}

struct BorrowingFixture {
    layout: StoreLayout<Store>,
    root: RootSnapshot,
    original: CapsuleTransaction,
    borrowed: CapsuleTransaction,
    earlier: CapsuleTransaction,
    controls: BTreeMap<String, CapsuleControl>,
    pointers: BTreeMap<String, CapsulePointer>,
    original_source: PackSourceDescriptor,
    file: String,
    original_shard: String,
    borrowed_shard: String,
}

// Fixed publication identities keep ordering coverage independent of nonce sampling.
fn ordering_transaction(root: &RootSnapshot, edit: CapsuleRefEdit) -> CapsuleTransaction {
    let edits = vec![edit];
    let identity = blake3::hash(&serde_json::to_vec(&edits).unwrap());
    CapsuleTransaction::for_protected_source(root.record().digest(), &identity.to_hex(), edits)
        .unwrap()
}

async fn borrowing_fixture() -> BorrowingFixture {
    let layout = StoreLayout::new(
        Store::new(Arc::new(object_store::memory::InMemory::new())),
        "checkpoint-precedence".to_owned(),
    );
    let root = create_root(
        &layout,
        RootRecord::encode(RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap())
            .unwrap(),
    )
    .await
    .unwrap();
    let tip = "1".repeat(40);
    let transaction = |name: String| {
        ordering_transaction(
            &root,
            CapsuleRefEdit::new(name, None, Some(tip.clone()), None),
        )
    };
    let borrowed = (0..64)
        .map(|i| transaction(format!("refs/heads/borrow-{i}")))
        .find(|t| (b'4'..b'8').contains(&t.id().unwrap().as_bytes()[0]))
        .unwrap();
    let borrowed_id = borrowed.id().unwrap();
    let mut producers = (0..256)
        .map(|i| transaction(format!("refs/heads/source-{i}")))
        .filter(|t| t.id().unwrap() > borrowed_id)
        .take(2)
        .collect::<Vec<_>>();
    producers.sort_by_key(|t| t.id().unwrap());
    let earlier = producers.remove(0);
    let original = producers.remove(0);
    let original_id = original.id().unwrap();
    let earlier_id = earlier.id().unwrap();
    let file = "3".repeat(64);
    let original_shard = "4".repeat(64);
    let borrowed_shard = "5".repeat(64);
    let xorb = "6".repeat(64);
    let mut descriptors = PointerCatalog::new();
    descriptors
        .insert_xorb(
            xorb.clone(),
            XorbCatalogEntry::new(
                99,
                "8".repeat(64),
                vec![XorbChunkEntry::new("7".repeat(64), 16)],
            ),
        )
        .unwrap();
    for shard in [&original_shard, &borrowed_shard] {
        descriptors
            .insert_shard(
                shard.clone(),
                ShardCatalogEntry::new(80, vec![xorb.clone()]),
            )
            .unwrap();
    }
    let mut controls = BTreeMap::new();
    let mut pointers = BTreeMap::new();
    let mut original_source = None;
    for t in [&original, &borrowed, &earlier] {
        let id = t.id().unwrap();
        let edit = if id == borrowed_id {
            GitVisibilityEdit::from_delta_objects(Some(tip.clone()), tip.clone(), vec![], vec![])
        } else {
            GitVisibilityEdit::from_replacement_objects(None, tip.clone(), vec![tip.clone()])
        };
        let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
            t.edits()[0].ref_name().to_owned(),
            edit,
        )]))
        .unwrap();
        let mut catalog = if id == original_id {
            descriptors.clone()
        } else {
            PointerCatalog::new()
        };
        if id != earlier_id {
            catalog
                .insert_file(
                    file.clone(),
                    FileCatalogEntry::new(
                        16,
                        if id == borrowed_id {
                            borrowed_shard.clone()
                        } else {
                            original_shard.clone()
                        },
                    ),
                )
                .unwrap();
        }
        // These bodies exercise metadata codecs; this is not native Git or
        // reconstruction qualification. The single object admits the shared tip.
        let packs = if id == original_id {
            vec![
                CapsuleGitPack::new(
                    Bytes::from_static(b"pack"),
                    Bytes::from_static(b"index"),
                    Bytes::from_static(b"reverse"),
                    Bytes::from_static(b"locator"),
                    tip.clone(),
                    1,
                )
                .unwrap(),
            ]
        } else {
            vec![]
        };
        let run = CapsuleRun::leaf(
            Capsule::build(
                t,
                packs,
                vec![
                    CapsuleSection::new(
                        CapsuleSectionKind::VisibilityDelta,
                        visibility.encode().unwrap(),
                    ),
                    CapsuleSection::new(
                        CapsuleSectionKind::CatalogDelta,
                        catalog.encode_delta().unwrap(),
                    ),
                ],
            )
            .unwrap(),
        )
        .unwrap();
        layout
            .store()
            .create_strict(&layout.capsule_path(run.hash()), run.bytes().clone())
            .await
            .unwrap();
        let (control, mut capsules) = run.verified_controls().unwrap();
        if id == original_id {
            original_source = Some(control.source_descriptor().unwrap());
        }
        controls.insert(id.clone(), capsules.remove(0));
        pointers.insert(
            id,
            CapsulePointer::new(
                run.hash(),
                run.bytes().len() as u64,
                run.control_offset(),
                run.control_size(),
                run.footer_hash(),
                run.level(),
                run.transaction_ids(),
                run.newest_base_root_digest(),
            )
            .unwrap(),
        );
    }
    BorrowingFixture {
        layout,
        root,
        original,
        borrowed,
        earlier,
        controls,
        pointers,
        original_source: original_source.unwrap(),
        file,
        original_shard,
        borrowed_shard,
    }
}

#[tokio::test]
async fn checkpoint_replay_base_binds_catalog_precedence() {
    let BorrowingFixture {
        layout,
        root,
        original,
        borrowed,
        earlier,
        controls,
        pointers,
        original_source,
        file,
        original_shard,
        borrowed_shard,
    } = borrowing_fixture().await;
    let original_id = original.id().unwrap();
    let borrowed_id = borrowed.id().unwrap();
    let earlier_id = earlier.id().unwrap();
    for t in [&original, &borrowed] {
        expose(&layout, &root, t, pointers[&t.id().unwrap()].clone()).await;
    }
    let before = load_pointer_catalog_from_root(&layout, &root)
        .await
        .unwrap();
    let empty =
        GitVisibilityIndex::new(0, "0".repeat(64), "0".repeat(64), BTreeMap::new()).unwrap();
    let ordered = order_ref_controls(
        &BTreeMap::new(),
        &BTreeMap::new(),
        &mut VisibilityReconciliation::Complete(empty.clone()),
        &BTreeMap::new(),
        controls
            .iter()
            .filter(|(id, _)| **id != earlier_id)
            .map(|(id, c)| (id.clone(), c.clone()))
            .collect(),
    )
    .await
    .unwrap();
    let mut visibility = VisibilityReconciliation::Complete(empty);
    let mut refs = BTreeMap::new();
    let mut peeled = BTreeMap::new();
    for control in &ordered {
        apply_control_refs(control, &mut refs, &mut peeled).unwrap();
        visibility.apply(control).await.unwrap();
    }
    let VisibilityReconciliation::Complete(index) = visibility else {
        unreachable!()
    };
    let sources = vec![original_source];
    let snapshot = LayeredVisibilitySnapshot::from_index_with_member_admission(
        &index,
        &source_catalog_digest(&sources).unwrap(),
        vec![LayeredObjectMember::new(0, 0)],
    )
    .unwrap();
    let checkpoint = LayeredCheckpoint::build(
        root.record().root().generation(),
        root.record().digest(),
        sources,
        before.clone(),
        Some(snapshot),
    )
    .unwrap();
    checkpoint.publish_catalog(&layout).await.unwrap();
    layout
        .store()
        .create_strict(
            &layout.capsule_checkpoint_path(checkpoint.hash()),
            checkpoint.bytes().clone(),
        )
        .await
        .unwrap();
    let pointer = CheckpointPointer::new_layered(
        checkpoint.hash(),
        checkpoint.bytes().len() as u64,
        checkpoint.control_offset(),
        checkpoint.control_size(),
        checkpoint.footer_hash(),
        checkpoint.covered_generation(),
        checkpoint.covered_root_digest(),
        checkpoint.pack_count().unwrap(),
        checkpoint.object_count().unwrap(),
    )
    .unwrap();
    let positions = [&original, &borrowed]
        .into_iter()
        .map(|t| (t.edits()[0].ref_name().to_owned(), t.id().unwrap()))
        .collect::<BTreeMap<_, _>>();
    let history = HistorySegment::build(
        pointer.clone(),
        None,
        HistorySegmentState::new(
            refs.clone(),
            peeled.clone(),
            root.record().root().head().to_owned(),
            positions.clone(),
            vec![
                pointers[&original_id].clone(),
                pointers[&borrowed_id].clone(),
            ],
        ),
    )
    .unwrap();
    layout
        .store()
        .create_strict(
            &layout.capsule_history_segment_path(history.hash()),
            history.bytes().clone(),
        )
        .await
        .unwrap();
    let next = RootRecord::encode(
        root.record()
            .root()
            .install_ref_checkpoint(
                root.record().digest(),
                pointer,
                history.pointer().unwrap(),
                refs,
                peeled,
                positions,
            )
            .unwrap(),
    )
    .unwrap();
    let etag = layout
        .store()
        .update(
            &layout.capsule_root_path(),
            next.bytes().clone(),
            root.etag().clone(),
        )
        .await
        .unwrap();
    let compacted = root.committed_ref_checkpoint(next, etag).unwrap();
    // Keep the original base to characterize explicitly pinned snapshot inputs;
    // concurrent writer admission is a separate contract from this metadata API.
    expose(&layout, &root, &earlier, pointers[&earlier_id].clone()).await;
    let mut winners = Vec::new();
    for captured in [&root, &compacted] {
        let full = load_pointer_catalog_from_root(&layout, captured)
            .await
            .unwrap();
        let selected = load_pointer_catalog_lookup_from_root(&layout, captured)
            .await
            .unwrap()
            .files(layout.store(), std::slice::from_ref(&file))
            .await
            .unwrap();
        assert_eq!(selected.files().get(&file), full.files().get(&file));
        winners.push(selected.files()[&file].shard_hash().to_owned());
    }
    // A checkpoint is an authority boundary, not a transparent reader shortcut.
    // It freezes the old replay base even when later visible refs supply an old tip.
    assert_eq!(
        (before.files()[&file].shard_hash().to_owned(), winners),
        (borrowed_shard.clone(), vec![original_shard, borrowed_shard])
    );
}

#[tokio::test]
async fn independent_visibility_can_reorder_prior_borrowing_controls() {
    let fixture = borrowing_fixture().await;
    let original_id = fixture.original.id().unwrap();
    let borrowed_id = fixture.borrowed.id().unwrap();
    let earlier_id = fixture.earlier.id().unwrap();
    let mut paged = BTreeMap::new();
    for pointer in fixture.pointers.values() {
        let (_, mut controls) = load_capsule_run_git_control(&fixture.layout, pointer)
            .await
            .unwrap();
        let control = controls.remove(0);
        paged.insert(control.transaction_id().to_owned(), control);
    }
    for (is_paged, controls) in [(false, fixture.controls), (true, paged)] {
        for with_earlier in [false, true] {
            let index = GitVisibilityIndex::new(0, "0".repeat(64), "0".repeat(64), BTreeMap::new())
                .unwrap();
            let mut visibility = if is_paged {
                VisibilityReconciliation::Paged(Box::new(
                    crate::capsule_protocol::CapturedVisibility::new(
                        fixture.layout.store().clone(),
                        index,
                        std::iter::empty(),
                    ),
                ))
            } else {
                VisibilityReconciliation::Complete(index)
            };
            let pending = controls
                .iter()
                .filter(|(id, _)| with_earlier || **id != earlier_id)
                .map(|(id, c)| (id.clone(), c.clone()))
                .collect();
            let ordered = order_ref_controls(
                &BTreeMap::new(),
                &BTreeMap::new(),
                &mut visibility,
                &BTreeMap::new(),
                pending,
            )
            .await
            .unwrap();
            let expected = if with_earlier {
                vec![
                    earlier_id.as_str(),
                    borrowed_id.as_str(),
                    original_id.as_str(),
                ]
            } else {
                vec![original_id.as_str(), borrowed_id.as_str()]
            };
            // Another visible producer can release an old borrowing control sooner;
            // physical run presence alone must never change the captured selection.
            assert_eq!(
                ordered
                    .iter()
                    .map(CapsuleControl::transaction_id)
                    .collect::<Vec<_>>(),
                expected,
                "paged={is_paged}, with_earlier={with_earlier}"
            );
        }
    }
}

#[tokio::test]
async fn deleting_a_visible_producer_can_delay_borrowing_controls() {
    let mut fixture = borrowing_fixture().await;
    let borrowed_id = fixture.borrowed.id().unwrap();
    let original_id = fixture.original.id().unwrap();
    let tip = fixture.original.edits()[0].new_oid().unwrap().to_owned();
    let (producer, deletion) = (0..16_384)
        .find_map(|i| {
            let name = format!("refs/heads/transient-{i}");
            let producer = ordering_transaction(
                &fixture.root,
                CapsuleRefEdit::new(name.clone(), None, Some(tip.clone()), None),
            );
            let deletion = ordering_transaction(
                &fixture.root,
                CapsuleRefEdit::new(name, Some(tip.clone()), None, None),
            );
            let producer_id = producer.id().unwrap();
            let deletion_id = deletion.id().unwrap();
            (producer_id < deletion_id && deletion_id < borrowed_id).then_some((producer, deletion))
        })
        .unwrap();
    let producer_id = producer.id().unwrap();
    let deletion_id = deletion.id().unwrap();
    for (transaction, edits) in [
        (
            &producer,
            BTreeMap::from([(
                producer.edits()[0].ref_name().to_owned(),
                GitVisibilityEdit::from_replacement_objects(None, tip.clone(), vec![tip.clone()]),
            )]),
        ),
        (&deletion, BTreeMap::new()),
    ] {
        let sections = if edits.is_empty() {
            Vec::new()
        } else {
            vec![CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                CapsuleVisibilityDelta::new(edits)
                    .unwrap()
                    .encode()
                    .unwrap(),
            )]
        };
        let run =
            CapsuleRun::leaf(Capsule::build(transaction, Vec::new(), sections).unwrap()).unwrap();
        fixture
            .layout
            .store()
            .create_strict(
                &fixture.layout.capsule_path(run.hash()),
                run.bytes().clone(),
            )
            .await
            .unwrap();
        let (_, mut controls) = run.verified_controls().unwrap();
        let id = transaction.id().unwrap();
        fixture.controls.insert(id.clone(), controls.remove(0));
        fixture.pointers.insert(
            id,
            CapsulePointer::new(
                run.hash(),
                run.bytes().len() as u64,
                run.control_offset(),
                run.control_size(),
                run.footer_hash(),
                run.level(),
                run.transaction_ids(),
                run.newest_base_root_digest(),
            )
            .unwrap(),
        );
    }
    fixture.controls.remove(&fixture.earlier.id().unwrap());
    let mut paged = BTreeMap::new();
    for id in fixture.controls.keys() {
        let (_, mut controls) =
            load_capsule_run_git_control(&fixture.layout, &fixture.pointers[id])
                .await
                .unwrap();
        paged.insert(id.clone(), controls.remove(0));
    }
    let mut observed = Vec::new();
    for (is_paged, controls) in [(false, fixture.controls), (true, paged)] {
        let index =
            GitVisibilityIndex::new(0, "0".repeat(64), "0".repeat(64), BTreeMap::new()).unwrap();
        let mut visibility = if is_paged {
            VisibilityReconciliation::Paged(Box::new(
                crate::capsule_protocol::CapturedVisibility::new(
                    fixture.layout.store().clone(),
                    index,
                    std::iter::empty(),
                ),
            ))
        } else {
            VisibilityReconciliation::Complete(index)
        };
        let ordered = order_ref_controls(
            &BTreeMap::new(),
            &BTreeMap::new(),
            &mut visibility,
            &BTreeMap::new(),
            controls,
        )
        .await
        .unwrap();
        // The transient source disappears before borrowing wins admission.
        // Treating visibility as monotone would incorrectly put borrowing
        // before the surviving producer and reverse their file-binding winner.
        observed.push(
            ordered
                .iter()
                .map(|control| control.transaction_id().to_owned())
                .collect::<Vec<_>>(),
        );
    }
    let expected = vec![producer_id, deletion_id, original_id, borrowed_id];
    assert_eq!(observed, vec![expected.clone(), expected]);
}
