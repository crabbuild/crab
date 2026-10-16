use super::*;
use crab_metadata::capsule_protocol::{
    CapsuleRefEdit, CapsuleSection, CapsuleSectionKind, CapsuleTransaction, CapsuleVisibilityDelta,
};
use crab_metadata::git_visibility::GitVisibilityEdit;
use object_store::memory::InMemory;

#[tokio::test]
async fn initial_replacement_opens_without_materializing_its_oid_set() {
    use crab_metadata::capsule_protocol::{RepositoryRoot, RootRecord};
    use std::sync::atomic::{AtomicU64, Ordering};
    let bytes = Arc::new(AtomicU64::new(0));
    let observed = bytes.clone();
    let store =
        Store::new(Arc::new(InMemory::new())).with_read_byte_observer(Arc::new(move |size| {
            observed.fetch_add(size, Ordering::Relaxed);
        }));
    let layout = StoreLayout::new(store, "repositories/large-root".into());
    let initial =
        RootRecord::encode(RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap())
            .unwrap();
    crab_metadata::capsule_protocol::create_root(&layout, initial.clone())
        .await
        .unwrap();
    let objects = (0u32..10000)
        .map(|index| format!("{index:040x}"))
        .collect::<Vec<_>>();
    let tip = objects[9000].clone();
    let transaction = CapsuleTransaction::new(
        initial.digest(),
        vec![CapsuleRefEdit::new(
            "refs/heads/main",
            None,
            Some(tip.clone()),
            None,
        )],
    )
    .unwrap();
    let delta = CapsuleVisibilityDelta::new(BTreeMap::from([(
        "refs/heads/main".into(),
        GitVisibilityEdit::from_replacement_objects(None, tip.clone(), objects),
    )]))
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
    let next = RootRecord::encode(
        initial
            .root()
            .advance(
                initial.digest(),
                BTreeMap::from([("refs/heads/main".into(), tip)]),
                BTreeMap::new(),
                vec![pointer],
                transaction.id().unwrap().as_str(),
            )
            .unwrap(),
    )
    .unwrap();
    layout
        .store()
        .put_overwrite(&layout.capsule_root_path(), next.bytes().clone())
        .await
        .unwrap();
    let root = crab_metadata::capsule_protocol::load_root(&layout)
        .await
        .unwrap();
    bytes.store(0, Ordering::Relaxed);
    let view = open_git_view_from_root(
        &layout,
        root,
        CapsuleReadLimits {
            max_capsule_bytes: 1024 * 1024,
            max_frontier_bytes: 1024 * 1024,
        },
    )
    .await
    .unwrap();
    assert!(
        bytes.load(Ordering::Relaxed) < 64 * 1024
            && view.capsule_controls[0].visibility_delta().is_err()
    );
}
