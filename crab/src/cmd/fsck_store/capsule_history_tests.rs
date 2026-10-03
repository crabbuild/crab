use super::*;
use bytes::Bytes;
use crab_metadata::capsule_protocol::{
    Capsule, CapsuleGitPack, CapsulePointer, CapsuleRefEdit, CapsuleRun, CapsuleTransaction,
    LayeredCheckpoint, PackLayer, PackSourceDescriptor, PointerCatalog, RootSnapshot,
};
use crab_storage::{StorageObservation, StorageObserver, StorageOperation};
use object_store::{ObjectStoreExt, memory::InMemory};
use std::sync::atomic::{AtomicUsize, Ordering};

async fn history_fixture() -> (
    crab_storage::StoreLayout<crab_storage::Store>,
    RootSnapshot,
    CapsuleRun,
) {
    let layout = crab_storage::StoreLayout::new(
        crab_storage::Store::new(Arc::new(InMemory::new())),
        "history-test".to_owned(),
    );
    let root =
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
    let transaction = CapsuleTransaction::new(
        root.record().digest(),
        vec![CapsuleRefEdit::new(
            "refs/heads/main",
            None,
            Some("2".repeat(40)),
            None,
        )],
    )
    .unwrap();
    let run =
        CapsuleRun::leaf(Capsule::build(&transaction, vec![fixture_pack()], Vec::new()).unwrap())
            .unwrap();
    layout
        .store()
        .put(&layout.capsule_path(run.hash()), run.bytes().clone())
        .await
        .unwrap();
    (layout, root, run)
}

fn fixture_pack() -> CapsuleGitPack {
    CapsuleGitPack::new(
        Bytes::from(vec![0x42; 128 * 1024]),
        Bytes::from_static(b"index"),
        Bytes::from_static(b"reverse"),
        Bytes::from_static(b"locator"),
        "3".repeat(40),
        1,
    )
    .unwrap()
}

fn run_pointer(run: &CapsuleRun, base_digest: &str) -> CapsulePointer {
    CapsulePointer::new(
        run.hash(),
        run.bytes().len() as u64,
        run.control_offset(),
        run.control_size(),
        run.footer_hash(),
        run.level(),
        run.transaction_ids(),
        base_digest,
    )
    .unwrap()
}

async fn checkpoint(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    root: RootSnapshot,
    source: PackSourceDescriptor,
    pointer: CapsulePointer,
) -> RootSnapshot {
    let checkpoint = LayeredCheckpoint::build(
        root.record().root().generation(),
        root.record().digest(),
        vec![source],
        PointerCatalog::new(),
        None,
    )
    .unwrap();
    let transactions = BTreeMap::from([(
        "refs/heads/main".to_owned(),
        pointer.transaction_ids()[0].clone(),
    )]);
    crab_write::capsule_protocol::publish_ref_layered_checkpoint(
        layout,
        root,
        &checkpoint,
        BTreeMap::from([("refs/heads/main".to_owned(), "2".repeat(40))]),
        BTreeMap::new(),
        transactions,
        vec![pointer],
    )
    .await
    .unwrap()
}

struct SourceReads {
    size: u64,
    count: AtomicUsize,
}

impl StorageObserver for SourceReads {
    fn started(&self, _operation: StorageOperation) {}

    fn finished(&self, observation: StorageObservation) {
        if matches!(
            observation.operation,
            StorageOperation::Get | StorageOperation::Range
        ) && observation.bytes_read == self.size
        {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[tokio::test]
async fn retained_checkpoints_and_run_pointers_read_shared_source_once() {
    let (layout, mut root, run) = history_fixture().await;
    for _ in 0..3 {
        root = checkpoint(
            &layout,
            root,
            PackSourceDescriptor::from_capsule_run(&run).unwrap(),
            run_pointer(&run, run.newest_base_root_digest()),
        )
        .await;
    }
    let reads = Arc::new(SourceReads {
        size: run.bytes().len() as u64,
        count: AtomicUsize::new(0),
    });
    let observed = crab_storage::StoreLayout::new(
        layout.store().clone().with_storage_observer(reads.clone()),
        layout.repo_prefix().to_owned(),
    );

    verify_capsule_history(&observed, root.record().root())
        .await
        .unwrap();

    assert_eq!(reads.count.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn retained_checkpoints_read_shared_pack_layer_once() {
    let (layout, mut root, run) = history_fixture().await;
    let layer = PackLayer::build(&fixture_pack()).unwrap();
    layout
        .store()
        .put(
            &layout.capsule_pack_layer_path(layer.hash()),
            layer.bytes().clone(),
        )
        .await
        .unwrap();
    for _ in 0..3 {
        root = checkpoint(
            &layout,
            root,
            layer.source_descriptor().unwrap(),
            run_pointer(&run, run.newest_base_root_digest()),
        )
        .await;
    }
    let reads = Arc::new(SourceReads {
        size: layer.bytes().len() as u64,
        count: AtomicUsize::new(0),
    });
    let observed = crab_storage::StoreLayout::new(
        layout.store().clone().with_storage_observer(reads.clone()),
        layout.repo_prefix().to_owned(),
    );

    verify_capsule_history(&observed, root.record().root())
        .await
        .unwrap();

    assert_eq!(reads.count.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn shared_source_body_corruption_is_not_hidden_by_reuse() {
    let (layout, mut root, run) = history_fixture().await;
    for _ in 0..2 {
        root = checkpoint(
            &layout,
            root,
            PackSourceDescriptor::from_capsule_run(&run).unwrap(),
            run_pointer(&run, run.newest_base_root_digest()),
        )
        .await;
    }
    let mut bytes = run.bytes().to_vec();
    bytes[0] ^= 1;
    layout
        .store()
        .inner()
        .put(&layout.capsule_path(run.hash()), Bytes::from(bytes).into())
        .await
        .unwrap();

    assert!(
        verify_capsule_history(&layout, root.record().root())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn source_reuse_still_validates_the_retained_run_pointer() {
    let (layout, root, run) = history_fixture().await;
    let root = checkpoint(
        &layout,
        root,
        PackSourceDescriptor::from_capsule_run(&run).unwrap(),
        run_pointer(&run, &"e".repeat(64)),
    )
    .await;

    assert!(
        verify_capsule_history(&layout, root.record().root())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn shared_sources_reject_conflicting_checkpoint_descriptors() {
    let (layout, root, run) = history_fixture().await;
    let source = PackSourceDescriptor::from_capsule_run(&run).unwrap();
    let root = checkpoint(
        &layout,
        root,
        source.clone(),
        run_pointer(&run, run.newest_base_root_digest()),
    )
    .await;
    let conflicting = PackSourceDescriptor::new(
        source.kind(),
        source.object_hash(),
        source.object_size(),
        source.control_offset(),
        source.control_size(),
        "f".repeat(64),
        source.members().to_vec(),
    )
    .unwrap();
    let root = checkpoint(
        &layout,
        root,
        conflicting,
        run_pointer(&run, run.newest_base_root_digest()),
    )
    .await;

    assert!(
        verify_capsule_history(&layout, root.record().root())
            .await
            .is_err()
    );
}
