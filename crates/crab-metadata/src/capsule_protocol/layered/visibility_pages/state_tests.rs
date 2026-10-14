use super::*;
use crate::git_visibility::GitVisibilityIndex;
use crab_storage::StoreLayout;
use object_store::memory::InMemory;
use std::sync::atomic::{AtomicU64, Ordering};

fn oid(value: u32) -> String {
    format!("{value:040x}")
}

async fn fixture(index: &GitVisibilityIndex) -> (PagedGitVisibility, Arc<AtomicU64>) {
    let count = Arc::new(AtomicU64::new(0));
    let observed = count.clone();
    let store =
        Store::new(Arc::new(InMemory::new())).with_read_byte_observer(Arc::new(move |bytes| {
            observed.fetch_add(bytes, Ordering::Relaxed);
        }));
    let layout = StoreLayout::new(store.clone(), "paged-visibility".into());
    let pack = CapsuleGitPack::new(
        Bytes::from_static(b"PACK fixture"),
        Bytes::from_static(b"index"),
        Bytes::from_static(b"reverse"),
        Bytes::from_static(b"locator"),
        "a".repeat(40),
        index.objects().len() as u64,
    )
    .unwrap();
    let source = PackLayer::build(&pack)
        .unwrap()
        .source_descriptor()
        .unwrap();
    let catalog = source_catalog_digest(std::slice::from_ref(&source)).unwrap();
    let visibility = LayeredVisibilitySnapshot::from_index(index, &catalog).unwrap();
    let checkpoint = LayeredCheckpoint::build_with_ordinal_visibility(
        0,
        &"b".repeat(64),
        vec![source],
        PointerCatalog::new(),
        Some(visibility),
    )
    .unwrap();
    let path = layout.capsule_checkpoint_path(checkpoint.hash());
    store.put(&path, checkpoint.bytes().clone()).await.unwrap();
    let reader = checkpoint
        .visibility_reader(path, 128 * 1024)
        .unwrap()
        .unwrap();
    let state = PagedGitVisibility::new(Some(reader), store).await.unwrap();
    (state, count)
}

fn initial() -> GitVisibilityIndex {
    GitVisibilityIndex::new(
        0,
        "a".repeat(64),
        "b".repeat(64),
        BTreeMap::from([
            ("refs/heads/a".into(), [1, 2, 3].map(oid).to_vec()),
            ("refs/heads/z".into(), [2, 4].map(oid).to_vec()),
        ]),
    )
    .unwrap()
}

async fn equivalent(state: &mut PagedGitVisibility, index: &GitVisibilityIndex) {
    let objects = (1..12)
        .map(|value| decode_oid(&oid(value)).unwrap())
        .collect::<Vec<_>>();
    for name in ["refs/heads/a", "refs/heads/b", "refs/heads/z"] {
        let expected = (1..12)
            .map(|value| index.contains_hex_in_ref(name, &oid(value)))
            .collect::<Vec<_>>();
        assert_eq!(
            state.contains_in_ref(name, &objects).await.unwrap(),
            expected
        );
    }
    assert_eq!(
        state.ref_names().collect::<Vec<_>>(),
        index
            .ref_closures()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn edits_preserve_lexical_inheritance_snapshot_removals_and_recreation() {
    let mut index = initial();
    let (mut state, _) = fixture(&index).await;
    let edits = [
        (
            "refs/heads/b",
            GitVisibilityEdit::from_delta_objects(Some(oid(2)), oid(5), vec![oid(5)], vec![]),
        ),
        (
            "refs/heads/a",
            GitVisibilityEdit::from_delta_objects(Some(oid(2)), oid(6), vec![oid(6)], vec![oid(1)]),
        ),
        (
            "refs/heads/b",
            GitVisibilityEdit::from_delta_objects(Some(oid(5)), oid(7), vec![oid(7)], vec![oid(3)]),
        ),
        (
            "refs/heads/b",
            GitVisibilityEdit::from_replacement_objects(Some(oid(7)), oid(4), vec![oid(4)]),
        ),
    ];
    for (name, edit) in edits {
        index.apply_ref_edit(name.into(), &edit).unwrap();
        state.apply_ref_edit(name.into(), &edit).await.unwrap();
        equivalent(&mut state, &index).await;
    }
    index.remove_ref("refs/heads/a");
    state.remove_ref("refs/heads/a");
    let edit = GitVisibilityEdit::from_delta_objects(Some(oid(4)), oid(8), vec![oid(8)], vec![]);
    index.apply_ref_edit("refs/heads/a".into(), &edit).unwrap();
    state
        .apply_ref_edit("refs/heads/a".into(), &edit)
        .await
        .unwrap();
    equivalent(&mut state, &index).await;
}

#[tokio::test]
async fn invalid_edits_match_complete_visibility_rejection() {
    let cases = [
        (
            "refs/heads/a",
            GitVisibilityEdit::from_delta_objects(Some(oid(9)), oid(6), vec![oid(6)], vec![]),
        ),
        (
            "refs/heads/a",
            GitVisibilityEdit::from_delta_objects(Some(oid(4)), oid(6), vec![oid(6)], vec![]),
        ),
        (
            "refs/heads/a",
            GitVisibilityEdit::from_delta_objects(Some(oid(2)), oid(6), vec![oid(6)], vec![oid(4)]),
        ),
        (
            "refs/heads/a",
            GitVisibilityEdit::from_delta_objects(Some(oid(2)), oid(4), vec![], vec![]),
        ),
        (
            "refs/heads/b",
            GitVisibilityEdit::from_replacement_objects(Some(oid(2)), oid(6), vec![oid(6)]),
        ),
    ];
    for (name, edit) in cases {
        let mut index = initial();
        let (mut state, _) = fixture(&index).await;
        assert!(index.apply_ref_edit(name.into(), &edit).is_err());
        assert!(state.apply_ref_edit(name.into(), &edit).await.is_err());
        equivalent(&mut state, &initial()).await;
    }
}

#[tokio::test]
async fn fixed_edit_reads_pages_instead_of_historical_dictionary_and_closures() {
    for objects in [10_000, 100_000] {
        let index = GitVisibilityIndex::new(
            0,
            "a".repeat(64),
            "b".repeat(64),
            BTreeMap::from([("refs/heads/a".into(), (1..=objects).map(oid).collect())]),
        )
        .unwrap();
        let (mut state, bytes) = fixture(&index).await;
        assert!(bytes.swap(0, Ordering::Relaxed) < 16 * 1024);
        let edit = GitVisibilityEdit::from_delta_objects(
            Some(oid(1)),
            oid(objects + 1),
            vec![oid(objects + 1)],
            vec![oid(2)],
        );
        state
            .apply_ref_edit("refs/heads/a".into(), &edit)
            .await
            .unwrap();
        assert!(bytes.load(Ordering::Relaxed) < 128 * 1024);
    }
}
