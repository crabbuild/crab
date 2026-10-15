use super::*;
use bytes::Bytes;
use crab_metadata::capsule_protocol::{
    Capsule, CapsuleGitPack, CapsulePointer, CapsuleRefEdit, CapsuleRun, CapsuleTransaction,
    LayeredCheckpoint, PackLayer, PackSourceDescriptor, PointerCatalog, RootSnapshot,
};
use crab_storage::{StorageObservation, StorageObserver, StorageOperation};
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult, memory::InMemory,
    path::Path as ObjectPath,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Debug)]
struct CorruptLifetimeDuringRead {
    inner: Arc<dyn ObjectStore>,
    source: ObjectPath,
    head: ObjectPath,
    fired: AtomicBool,
}

impl std::fmt::Display for CorruptLifetimeDuringRead {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str("source lifetime corruption fixture")
    }
}

#[async_trait::async_trait]
impl ObjectStore for CorruptLifetimeDuringRead {
    async fn put_opts(
        &self,
        path: &ObjectPath,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.inner.put_opts(path, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        path: &ObjectPath,
        options: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(path, options).await
    }

    async fn get_opts(
        &self,
        path: &ObjectPath,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        let full_body = !options.head && options.range.is_none();
        let response = self.inner.get_opts(path, options).await?;
        if full_body && *path == self.source && !self.fired.swap(true, Ordering::SeqCst) {
            self.inner
                .put(&self.head, Bytes::from_static(b"corrupt").into())
                .await?;
        }
        Ok(response)
    }

    fn delete_stream(
        &self,
        paths: futures_util::stream::BoxStream<'static, object_store::Result<ObjectPath>>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<ObjectPath>> {
        self.inner.delete_stream(paths)
    }

    fn list(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &ObjectPath,
        to: &ObjectPath,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

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

#[tokio::test]
async fn fsck_rejects_lifetime_corruption_during_original_source_verification() {
    let (layout, root, run) = history_fixture().await;
    let pointer = run_pointer(&run, root.record().digest());
    crab_metadata::capsule_protocol::retain_capsule_sources(
        &layout,
        "refs/heads/main",
        0,
        &[pointer],
    )
    .await
    .unwrap();
    let head = layout
        .store()
        .list_prefix(&layout.capsule_source_heads_prefix())
        .await
        .unwrap()
        .remove(0)
        .location;
    let backend = Arc::new(CorruptLifetimeDuringRead {
        inner: Arc::clone(layout.store().inner()),
        source: layout.capsule_path(run.hash()),
        head,
        fired: AtomicBool::new(false),
    });
    let observed = crab_storage::StoreLayout::new(
        crab_storage::Store::new(backend.clone()),
        layout.repo_prefix().to_owned(),
    );
    let result = verify_capsule_history(&observed, root.record().root()).await;
    assert!(backend.fired.load(Ordering::SeqCst));
    assert!(
        result.is_err(),
        "fsck accepted corrupt lifetime metadata after its original source read: {result:?}"
    );
}

#[tokio::test]
async fn data_checks_reject_browse_corruption_during_xorb_verification() {
    for readiness in [false, true] {
        let (store, prefix, root, xorb_hash, _) = super::tests::capsule_checker_fixture().await;
        let router = StoreLayout::new(store.clone(), prefix.clone());
        let layout = crab_storage::StoreLayout::with_global_prefix(
            store.as_storage().clone(),
            router.repo_prefix().to_owned(),
            router.global_prefix().to_owned(),
        );
        let backend = Arc::new(CorruptLifetimeDuringRead {
            inner: Arc::clone(store.inner()),
            source: router.xorb_path(&xorb_hash),
            head: if readiness {
                layout.capsule_browse_indexes_path()
            } else {
                layout.browse_retained_head_path()
            },
            fired: AtomicBool::new(false),
        });
        let store = crab_storage::Store::new(backend.clone());
        let checker = StoreChecker::for_capsule_repository(
            crate::storage::store::Store::from_storage(store),
            prefix,
            root,
        )
        .await
        .unwrap();
        let result = checker.check_data_chain().await;
        assert!(backend.fired.load(Ordering::SeqCst));
        assert!(
            result.is_err(),
            "fsck accepted corrupt browse metadata after its xorb read: readiness={readiness}, {result:?}"
        );
    }
}

#[tokio::test]
async fn final_snapshot_check_rejects_browse_movement_after_payload_verification() {
    for readiness in [false, true] {
        let (store, prefix, root, _, _) = super::tests::capsule_checker_fixture().await;
        let router = StoreLayout::new(store.clone(), prefix.clone());
        let layout = crab_storage::StoreLayout::with_global_prefix(
            store.as_storage().clone(),
            router.repo_prefix().to_owned(),
            router.global_prefix().to_owned(),
        );
        let checker = StoreChecker::for_capsule_repository(store.clone(), prefix, root)
            .await
            .unwrap();
        assert!(checker.check_data_chain().await.unwrap().is_empty());
        let indexes = crab_metadata::capsule_protocol::BrowseIndexes::new(
            "a".repeat(64),
            "b".repeat(64),
            None,
        )
        .unwrap();
        if readiness {
            store
                .put(
                    &layout.capsule_browse_indexes_path(),
                    indexes.encode().unwrap(),
                )
                .await
                .unwrap();
        } else {
            crab_metadata::capsule_protocol::retain_browse_indexes(&layout, &indexes, 0, None, &[])
                .await
                .unwrap();
        }
        assert!(matches!(
            checker.check_orphan_file_index().await,
            Err(CrabError::Protocol(reason)) if reason.contains("browse retention changed")
        ));
    }
}

#[tokio::test]
async fn capsule_recipe_probe_does_not_require_browse_maintenance() {
    let (store, prefix, root, _, _) = super::tests::capsule_checker_fixture().await;
    let router = StoreLayout::new(store.clone(), prefix.clone());
    store
        .put(
            &router.repo_path("v2/browse-retention"),
            Bytes::from_static(b"corrupt"),
        )
        .await
        .unwrap();
    let checker = StoreChecker::for_capsule_repository(store, prefix, root)
        .await
        .unwrap();
    let (hash, file) = checker
        .capsule
        .as_ref()
        .unwrap()
        .catalog
        .files()
        .first_key_value()
        .unwrap();
    let pointer = Pointer {
        file_hash: MerkleHash::from_hex(hash).unwrap().into(),
        size: file.size(),
        shard_hint: None,
    };
    let proof = checker
        .verify_capsule_pointer_data(&[pointer], &CancellationToken::new())
        .await
        .unwrap();
    assert!(proof.verified == 1 && proof.issues.is_empty(), "{proof:?}");
}

#[tokio::test]
async fn data_checks_reject_lifetime_movement_after_checker_construction() {
    let (layout, root, run) = history_fixture().await;
    let pointer = run_pointer(&run, root.record().digest());
    crab_metadata::capsule_protocol::retain_capsule_sources(
        &layout,
        "refs/heads/main",
        0,
        &[pointer],
    )
    .await
    .unwrap();
    let checker = StoreChecker::for_capsule_repository(
        crate::storage::store::Store::from_storage(layout.store().clone()),
        layout.repo_prefix().to_owned(),
        root,
    )
    .await
    .unwrap();
    assert!(checker.check_data_chain().await.unwrap().is_empty());
    crab_metadata::capsule_protocol::prune_retained_capsule_sources(&layout, 0)
        .await
        .unwrap();
    assert!(matches!(
        checker.check_data_chain().await,
        Err(CrabError::Protocol(reason)) if reason.contains("source lifetime changed")
    ));
}

#[tokio::test]
async fn retired_sources_are_checked_without_checkpoint_history() {
    let (layout, root, run) = history_fixture().await;
    let pointer = run_pointer(&run, root.record().digest());
    crab_metadata::capsule_protocol::retain_capsule_sources(
        &layout,
        "refs/heads/main",
        0,
        &[pointer],
    )
    .await
    .unwrap();
    verify_capsule_history(&layout, root.record().root())
        .await
        .unwrap();
    layout
        .store()
        .delete(&layout.capsule_path(run.hash()))
        .await
        .unwrap();
    assert!(
        verify_capsule_history(&layout, root.record().root())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn retired_sources_share_full_body_verification_with_checkpoint_history() {
    let (layout, root, run) = history_fixture().await;
    let pointer = run_pointer(&run, root.record().digest());
    crab_metadata::capsule_protocol::retain_capsule_sources(
        &layout,
        "refs/heads/main",
        0,
        std::slice::from_ref(&pointer),
    )
    .await
    .unwrap();
    let root = checkpoint(
        &layout,
        root,
        PackSourceDescriptor::from_capsule_run(&run).unwrap(),
        pointer,
    )
    .await;
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
