use super::*;
use crate::capsule_protocol::{
    Capsule, CapsulePointer, CapsuleRefEdit, CapsuleRun, CapsuleSection, CapsuleSectionKind,
    CapsuleTransaction, CapsuleVisibilityDelta,
};
use crate::git_visibility::GitVisibilityEdit;
use crab_storage::StoreLayout;
use object_store::memory::InMemory;

fn hex(value: u8) -> String {
    format!("{value:040x}")
}
fn object(value: u8) -> [u8; 20] {
    let mut oid = [0; 20];
    oid[19] = value;
    oid
}
async fn control(
    layout: &StoreLayout<Store>,
    name: &str,
    expected: Option<u8>,
    edit: GitVisibilityEdit,
) -> CapsuleControl {
    control_set(
        layout,
        BTreeMap::from([(name.to_owned(), (expected, edit))]),
    )
    .await
}
async fn control_set(
    layout: &StoreLayout<Store>,
    edits: BTreeMap<String, (Option<u8>, GitVisibilityEdit)>,
) -> CapsuleControl {
    let transaction = CapsuleTransaction::new(
        &"1".repeat(64),
        edits
            .iter()
            .map(|(name, (expected, edit))| {
                CapsuleRefEdit::new(name, expected.map(hex), Some(edit.new_oid.clone()), None)
            })
            .collect(),
    )
    .unwrap();
    let delta = CapsuleVisibilityDelta::new(
        edits
            .into_iter()
            .map(|(name, (_, edit))| (name, edit))
            .collect(),
    )
    .unwrap();
    let capsule = Capsule::build(
        &transaction,
        Vec::new(),
        vec![CapsuleSection::new(
            CapsuleSectionKind::VisibilityDelta,
            delta.encode().unwrap(),
        )],
    )
    .unwrap();
    let run = CapsuleRun::leaf(capsule).unwrap();
    layout
        .store()
        .create_strict(&layout.capsule_path(run.hash()), run.bytes().clone())
        .await
        .unwrap();
    let pointer = CapsulePointer::new(
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
    let (_, mut controls) = crate::capsule_protocol::load_capsule_run_git_control(layout, &pointer)
        .await
        .unwrap();
    controls.remove(0)
}
fn initial() -> GitVisibilityIndex {
    GitVisibilityIndex::new(0, "0".repeat(64), "0".repeat(64), BTreeMap::new()).unwrap()
}

#[tokio::test]
async fn layered_closures_match_complete_edits_and_preserve_captured_bases() {
    let layout = StoreLayout::new(
        Store::new(Arc::new(InMemory::new())),
        "repositories/layers".into(),
    );
    let mut full = initial();
    let mut lazy =
        CapturedVisibility::new(layout.store().clone(), full.clone(), std::iter::empty());
    let edits = [
        (
            "refs/heads/main",
            None,
            GitVisibilityEdit::from_replacement_objects(None, hex(1), vec![hex(1), hex(2), hex(3)]),
        ),
        (
            "refs/heads/feature",
            None,
            GitVisibilityEdit::from_delta_objects(Some(hex(1)), hex(2), vec![hex(4)], vec![hex(3)]),
        ),
        (
            "refs/heads/main",
            Some(1),
            GitVisibilityEdit::from_delta_objects(Some(hex(1)), hex(5), vec![hex(5)], vec![hex(3)]),
        ),
    ];
    for (name, expected, edit) in edits {
        let control = control(&layout, name, expected, edit.clone()).await;
        full.apply_ref_edit(name.to_owned(), &edit).unwrap();
        lazy.apply(&control).await.unwrap();
    }
    let requested = (1..7).map(object).collect::<Vec<_>>();
    let mut actual = BTreeMap::new();
    for (name, index) in &lazy.refs {
        actual.insert(
            name.clone(),
            lazy.contains(Some(*index), &requested).await.unwrap(),
        );
    }
    let expected = lazy
        .refs
        .keys()
        .map(|name| {
            (
                name.clone(),
                requested
                    .iter()
                    .map(|oid| full.contains_in_ref(name, oid))
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        (actual, lazy.dictionary_count),
        (expected, full.objects().len() as u64)
    );
}

#[tokio::test]
async fn removed_invisible_objects_do_not_supply_new_ref_bases() {
    let layout = StoreLayout::new(
        Store::new(Arc::new(InMemory::new())),
        "repositories/invisible".into(),
    );
    let mut lazy = CapturedVisibility::new(layout.store().clone(), initial(), std::iter::empty());
    let create = control(
        &layout,
        "refs/heads/main",
        None,
        GitVisibilityEdit::from_replacement_objects(None, hex(1), vec![hex(1), hex(3)]),
    )
    .await;
    lazy.apply(&create).await.unwrap();
    let remove = control(
        &layout,
        "refs/heads/main",
        Some(1),
        GitVisibilityEdit::from_delta_objects(Some(hex(1)), hex(2), vec![hex(2)], vec![hex(3)]),
    )
    .await;
    lazy.apply(&remove).await.unwrap();
    let branch = control(
        &layout,
        "refs/heads/feature",
        None,
        GitVisibilityEdit::from_delta_objects(Some(hex(3)), hex(4), vec![hex(4)], vec![]),
    )
    .await;
    assert_eq!(
        (
            lazy.dictionary_contains(&[object(3)]).await.unwrap(),
            lazy.ready(
                &branch,
                &BTreeMap::from([("refs/heads/main".into(), hex(2))])
            )
            .await
            .unwrap()
        ),
        (vec![true], false)
    );
}

#[tokio::test]
async fn deleted_ref_objects_remain_in_dictionary_but_cannot_supply_new_bases() {
    let layout = StoreLayout::new(
        Store::new(Arc::new(InMemory::new())),
        "repositories/deleted".into(),
    );
    let mut lazy = CapturedVisibility::new(layout.store().clone(), initial(), std::iter::empty());
    let create = control(
        &layout,
        "refs/heads/main",
        None,
        GitVisibilityEdit::from_replacement_objects(None, hex(1), vec![hex(1), hex(3)]),
    )
    .await;
    lazy.apply(&create).await.unwrap();
    let transaction = CapsuleTransaction::new(
        &"1".repeat(64),
        vec![CapsuleRefEdit::new(
            "refs/heads/main",
            Some(hex(1)),
            None,
            None,
        )],
    )
    .unwrap();
    let run =
        CapsuleRun::leaf(Capsule::build(&transaction, Vec::new(), Vec::new()).unwrap()).unwrap();
    let (_, mut controls) = run.verified_controls().unwrap();
    lazy.apply(&controls.remove(0)).await.unwrap();
    let branch = control(
        &layout,
        "refs/heads/feature",
        None,
        GitVisibilityEdit::from_delta_objects(Some(hex(3)), hex(4), vec![hex(4)], vec![]),
    )
    .await;
    assert_eq!(
        (
            lazy.dictionary_contains(&[object(3)]).await.unwrap(),
            lazy.ready(&branch, &BTreeMap::new()).await.unwrap()
        ),
        (vec![true], false)
    );
}

#[tokio::test]
async fn removal_outside_captured_prior_closure_is_rejected() {
    let layout = StoreLayout::new(
        Store::new(Arc::new(InMemory::new())),
        "repositories/bad-removal".into(),
    );
    let mut lazy = CapturedVisibility::new(layout.store().clone(), initial(), std::iter::empty());
    let create = control(
        &layout,
        "refs/heads/main",
        None,
        GitVisibilityEdit::from_replacement_objects(None, hex(1), vec![hex(1)]),
    )
    .await;
    lazy.apply(&create).await.unwrap();
    let bad = control(
        &layout,
        "refs/heads/main",
        Some(1),
        GitVisibilityEdit::from_delta_objects(Some(hex(1)), hex(2), vec![hex(2)], vec![hex(9)]),
    )
    .await;
    assert!(lazy.apply(&bad).await.is_err());
}

fn shared_edit(
    old: Option<u8>,
    new: u8,
    added: &[u8],
    removed: &[u8],
    replaces: bool,
) -> GitVisibilityEdit {
    use crate::git_visibility::{
        GIT_VISIBILITY_EDIT_VERSION, GitVisibilityDictionary, GitVisibilityObjects,
    };
    let objects = added
        .iter()
        .chain(removed)
        .copied()
        .collect::<BTreeSet<_>>();
    let dictionary =
        GitVisibilityDictionary::new(objects.iter().copied().map(hex).collect()).unwrap();
    let set = |values: &[u8]| {
        GitVisibilityObjects::from_shared_ordinals(
            dictionary.clone(),
            values
                .iter()
                .map(|value| objects.iter().position(|object| object == value).unwrap() as u32)
                .collect(),
        )
        .unwrap()
    };
    GitVisibilityEdit {
        version: GIT_VISIBILITY_EDIT_VERSION,
        old_oid: old.map(hex),
        new_oid: hex(new),
        replaces,
        added: set(added),
        removed: set(removed),
    }
}

#[tokio::test]
async fn shared_dictionaries_count_union_once_without_widening_ref_memberships() {
    let layout = StoreLayout::new(
        Store::new(Arc::new(InMemory::new())),
        "repositories/shared-union".into(),
    );
    let mut lazy = CapturedVisibility::new(layout.store().clone(), initial(), std::iter::empty());
    let both = control_set(
        &layout,
        BTreeMap::from([
            (
                "refs/heads/main".into(),
                (None, shared_edit(None, 1, &[1, 3], &[], true)),
            ),
            (
                "refs/heads/feature".into(),
                (None, shared_edit(None, 2, &[2, 4], &[], true)),
            ),
        ]),
    )
    .await;
    // Loading a verified compact snapshot is preparatory, never retention or authority.
    both.paged_visibility()
        .unwrap()
        .prepare_reconciliation(layout.store())
        .await
        .unwrap();
    assert_eq!(
        lazy.dictionary_contains(&[object(1), object(4)])
            .await
            .unwrap(),
        vec![false; 2]
    );
    let borrowed = control(
        &layout,
        "refs/heads/borrowed",
        None,
        shared_edit(Some(4), 5, &[5], &[], false),
    )
    .await;
    assert!(!lazy.ready(&borrowed, &BTreeMap::new()).await.unwrap());
    lazy.apply(&both).await.unwrap();
    assert_eq!(lazy.dictionary_count, 4);
    let wants = (1..=5).map(object).collect::<Vec<_>>();
    assert_eq!(
        lazy.contains(Some(lazy.refs["refs/heads/main"]), &wants)
            .await
            .unwrap(),
        vec![true, false, true, false, false]
    );
    assert_eq!(
        lazy.contains(Some(lazy.refs["refs/heads/feature"]), &wants)
            .await
            .unwrap(),
        vec![false, true, false, true, false]
    );

    let update = control(
        &layout,
        "refs/heads/main",
        Some(1),
        shared_edit(Some(1), 5, &[5], &[3], false),
    )
    .await;
    lazy.apply(&update).await.unwrap();
    assert_eq!(lazy.dictionary_count, 5);
    assert_eq!(
        lazy.dictionary_contains(&[object(3)]).await.unwrap(),
        vec![true]
    );
    assert_eq!(
        lazy.contains(Some(lazy.refs["refs/heads/main"]), &[object(3), object(5)])
            .await
            .unwrap(),
        vec![false, true]
    );
    let borrow_removed = control(
        &layout,
        "refs/heads/removed",
        None,
        shared_edit(Some(3), 9, &[9], &[], false),
    )
    .await;
    assert!(
        !lazy
            .ready(
                &borrow_removed,
                &BTreeMap::from([
                    ("refs/heads/main".into(), hex(5)),
                    ("refs/heads/feature".into(), hex(2))
                ])
            )
            .await
            .unwrap()
    );

    let legacy = control(
        &layout,
        "refs/heads/legacy",
        None,
        GitVisibilityEdit::from_replacement_objects(None, hex(6), vec![hex(4), hex(6)]),
    )
    .await;
    lazy.apply(&legacy).await.unwrap();
    assert_eq!(lazy.dictionary_count, 6);
    let overlap = control(
        &layout,
        "refs/heads/overlap",
        None,
        shared_edit(None, 7, &[1, 4, 6, 7], &[], true),
    )
    .await;
    lazy.apply(&overlap).await.unwrap();
    assert_eq!(lazy.dictionary_count, 7);
    assert_eq!(
        lazy.dictionary_contains(&(1..=8).map(object).collect::<Vec<_>>())
            .await
            .unwrap(),
        vec![true, true, true, true, true, true, true, false]
    );
    let deletion = CapsuleTransaction::new(
        &"1".repeat(64),
        vec![CapsuleRefEdit::new(
            "refs/heads/feature",
            Some(hex(2)),
            None,
            None,
        )],
    )
    .unwrap();
    let run = CapsuleRun::leaf(Capsule::build(&deletion, vec![], vec![]).unwrap()).unwrap();
    let (_, controls) = run.verified_controls().unwrap();
    lazy.apply(&controls[0]).await.unwrap();
    assert_eq!(lazy.dictionary_count, 7);
    assert_eq!(
        lazy.dictionary_contains(&[object(2)]).await.unwrap(),
        vec![true]
    );
    let deleted_base = control(
        &layout,
        "refs/heads/deleted-base",
        None,
        shared_edit(Some(2), 8, &[8], &[], false),
    )
    .await;
    assert!(!lazy.ready(&deleted_base, &BTreeMap::new()).await.unwrap());
}

#[tokio::test]
async fn shared_removal_cannot_borrow_another_refs_dictionary_entries() {
    let layout = StoreLayout::new(
        Store::new(Arc::new(InMemory::new())),
        "repositories/shared-bad-removal".into(),
    );
    let initial = GitVisibilityIndex::new(
        0,
        "0".repeat(64),
        "0".repeat(64),
        BTreeMap::from([
            ("refs/heads/main".into(), vec![hex(1)]),
            ("refs/heads/other".into(), vec![hex(9)]),
        ]),
    )
    .unwrap();
    let mut lazy = CapturedVisibility::new(
        layout.store().clone(),
        initial,
        ["refs/heads/main".into(), "refs/heads/other".into()].into_iter(),
    );
    let bad = control(
        &layout,
        "refs/heads/main",
        Some(1),
        shared_edit(Some(1), 2, &[2], &[9], false),
    )
    .await;
    assert!(
        lazy.apply(&bad)
            .await
            .unwrap_err()
            .to_string()
            .contains("outside the prior closure")
    );
    assert_eq!(lazy.dictionary_count, 2);
    assert_eq!(
        lazy.dictionary_contains(&[object(2)]).await.unwrap(),
        vec![false]
    );
    assert!(lazy.shared_dictionaries.is_empty());
}

#[tokio::test]
async fn appended_initial_dictionary_entries_are_counted_by_retention_identity() {
    let layout = StoreLayout::new(
        Store::new(Arc::new(InMemory::new())),
        "repositories/appended-dictionary".into(),
    );
    let mut initial = GitVisibilityIndex::new(
        0,
        "0".repeat(64),
        "0".repeat(64),
        BTreeMap::from([("refs/heads/main".into(), vec![hex(4)])]),
    )
    .unwrap();
    initial
        .apply_ref_edit(
            "refs/heads/main".into(),
            &GitVisibilityEdit::from_delta_objects(Some(hex(4)), hex(1), vec![hex(1)], vec![]),
        )
        .unwrap();
    assert_eq!(initial.objects(), &[object(4), object(1)]);
    let mut lazy = CapturedVisibility::new(
        layout.store().clone(),
        initial,
        std::iter::once("refs/heads/main".into()),
    );
    assert_eq!(
        lazy.dictionary_contains(&[object(1), object(4)])
            .await
            .unwrap(),
        vec![true; 2]
    );
    let branch = control(
        &layout,
        "refs/heads/new",
        None,
        shared_edit(None, 3, &[1, 3, 4], &[], true),
    )
    .await;
    lazy.apply(&branch).await.unwrap();
    assert_eq!(lazy.dictionary_count, 3);
}

#[tokio::test]
async fn preparing_shared_controls_caches_only_initial_checkpoint_retention() {
    use crate::capsule_protocol::PointerCatalog;
    use crate::capsule_protocol::{
        CapsuleGitPack, LayeredCheckpoint, LayeredObjectMember, LayeredVisibilitySnapshot,
        PackLayer, source_catalog_digest,
    };
    use bytes::Bytes;
    let layout = StoreLayout::new(
        Store::new(Arc::new(InMemory::new())),
        "repositories/shared-checkpoint".into(),
    );
    let mut index = GitVisibilityIndex::new(
        0,
        "0".repeat(64),
        "0".repeat(64),
        BTreeMap::from([
            ("refs/heads/main".into(), vec![hex(1), hex(2)]),
            ("refs/heads/deleted".into(), vec![hex(3)]),
        ]),
    )
    .unwrap();
    index.remove_ref("refs/heads/deleted");
    let pack = CapsuleGitPack::new(
        Bytes::from_static(b"pack"),
        Bytes::from_static(b"index"),
        Bytes::from_static(b"reverse"),
        Bytes::from_static(b"locator"),
        "1".repeat(40),
        3,
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
        vec![LayeredObjectMember::new(0, 0); 3],
    )
    .unwrap();
    let checkpoint = LayeredCheckpoint::build(
        1,
        &"3".repeat(64),
        vec![source],
        PointerCatalog::new(),
        Some(snapshot),
    )
    .unwrap();
    layout
        .store()
        .create_strict(
            &layout.capsule_checkpoint_path(checkpoint.hash()),
            checkpoint.bytes().clone(),
        )
        .await
        .unwrap();
    let checkpoint_control = LayeredCheckpoint::decode_control(
        checkpoint
            .bytes()
            .slice(checkpoint.control_offset() as usize..),
        checkpoint.bytes().len() as u64,
        checkpoint.control_offset(),
        checkpoint.hash(),
        checkpoint.footer_hash(),
    )
    .unwrap();
    let reader = checkpoint_control
        .visibility_reader(&layout, 128 * 1024 * 1024)
        .unwrap()
        .unwrap();
    let mut captured = CapturedVisibility::from_checkpoint(
        layout.store().clone(),
        Arc::new(reader),
        std::iter::once("refs/heads/main".into()),
    )
    .await
    .unwrap();
    let first = control_set(
        &layout,
        BTreeMap::from([(
            "refs/heads/main".into(),
            (Some(1), shared_edit(Some(1), 4, &[2, 4], &[], false)),
        )]),
    )
    .await;
    let second = control(
        &layout,
        "refs/heads/new",
        None,
        shared_edit(Some(4), 5, &[5], &[], false),
    )
    .await;
    captured
        .prepare_controls([&first, &second].into_iter())
        .await
        .unwrap();
    assert_eq!(captured.dictionary_count, 3);
    assert!(captured.shared_dictionaries.is_empty());
    assert_eq!(
        captured
            .dictionary_contains(&(1..=5).map(object).collect::<Vec<_>>())
            .await
            .unwrap(),
        vec![true, true, true, false, false]
    );
    assert!(
        !captured
            .ready(
                &second,
                &BTreeMap::from([("refs/heads/main".into(), hex(1))])
            )
            .await
            .unwrap()
    );
    captured.apply(&first).await.unwrap();
    assert_eq!(captured.dictionary_count, 4);
    assert!(
        captured
            .ready(
                &second,
                &BTreeMap::from([("refs/heads/main".into(), hex(4))])
            )
            .await
            .unwrap()
    );
    captured.apply(&second).await.unwrap();
    assert_eq!(captured.dictionary_count, 5);
    assert_eq!(
        captured
            .dictionary_contains(&(1..=6).map(object).collect::<Vec<_>>())
            .await
            .unwrap(),
        vec![true, true, true, true, true, false]
    );
}
