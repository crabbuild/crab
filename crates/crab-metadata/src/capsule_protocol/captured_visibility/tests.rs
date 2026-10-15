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
    let transaction = CapsuleTransaction::new(
        &"1".repeat(64),
        vec![CapsuleRefEdit::new(
            name,
            expected.map(hex),
            Some(edit.new_oid.clone()),
            None,
        )],
    )
    .unwrap();
    let delta = CapsuleVisibilityDelta::new(BTreeMap::from([(name.to_owned(), edit)])).unwrap();
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
