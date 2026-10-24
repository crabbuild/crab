use super::tests::{hash_from_seed, seed_file_index, shard_with_file};
use super::*;
use bytes::Bytes;
use futures_util::stream::BoxStream;
use object_store::memory::InMemory;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use std::time::Duration;
use tokio::sync::{Notify, Semaphore};
use tokio::time::timeout;

#[tokio::test]
async fn current_lookup_ignores_legacy_authority_without_a_native_root() {
    let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let file = hash_from_seed(42);
    let shard = hash_from_seed(100);
    seed_file_index(Arc::clone(&inner), "shared/native-only", &[(file, shard)]).await;
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = reads.clone();
    let storage = crab_storage::Store::new(inner).with_read_request_observer(Arc::new(move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
    }));
    let prefix = ObjectPath::from("shared/native-only");
    let before = storage.list_prefix(&prefix).await.unwrap();
    reads.store(0, Ordering::SeqCst);
    let lookup = SharedFileIndexLookup::new_for_storage(&storage, "shared/native-only");
    let found = lookup.lookup(&file).await.unwrap();
    lookup.close().await.unwrap();
    let lookup_reads = reads.load(Ordering::SeqCst);
    let after = storage.list_prefix(&prefix).await.unwrap();
    assert_eq!(
        (found, lookup_reads, before),
        (None, 1, after),
        "only the absent native root may be read; legacy data and reader checkpoints stay untouched"
    );
}

#[tokio::test]
async fn current_lookup_preserves_missing_capsule_dependency_errors() {
    use crate::capsule_protocol::{
        Capsule, CapsulePointer, CapsuleRefEdit, CapsuleRefHead, CapsuleRun, CapsuleTransaction,
        CapsuleTransactionRecord, RepositoryRoot, RootRecord, capsule_ref_name_key, create_root,
        load_pointer_catalog,
    };

    let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let prefix = "shared/protocol-authority";
    let file = hash_from_seed(42);
    let (body, shard) = shard_with_file(file);
    seed_file_index(Arc::clone(&inner), prefix, &[(file, shard)]).await;
    let storage = crab_storage::Store::new(inner);
    let router = crab_storage::StoreLayout::new(storage.clone(), prefix.to_owned());
    storage
        .put(&router.shard_path(&shard), Bytes::from(body))
        .await
        .unwrap();

    let root = create_root(
        &router,
        RootRecord::encode(RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap())
            .unwrap(),
    )
    .await
    .unwrap();
    let ref_name = "refs/tags/release";
    let transaction = CapsuleTransaction::new(
        root.record().digest(),
        vec![CapsuleRefEdit::new(
            ref_name,
            None,
            Some("2".repeat(40)),
            None,
        )],
    )
    .unwrap();
    let run = CapsuleRun::leaf(Capsule::build(&transaction, vec![], vec![]).unwrap()).unwrap();
    storage
        .put(&router.capsule_path(run.hash()), run.bytes().clone())
        .await
        .unwrap();
    let pointer = CapsulePointer::new(
        run.hash(),
        run.bytes().len() as u64,
        run.control_offset(),
        run.control_size(),
        run.footer_hash(),
        run.level(),
        run.transaction_ids().to_vec(),
        run.newest_base_root_digest(),
    )
    .unwrap();
    let head = CapsuleRefHead::from_root(
        ref_name,
        root.record().root().ref_epoch().to_owned(),
        None,
        None,
    )
    .unwrap();
    let state = head
        .successor_state(
            &Default::default(),
            Some("2".repeat(40)),
            None,
            transaction.id().unwrap(),
            vec![pointer],
        )
        .unwrap();
    let activation =
        CapsuleTransactionRecord::preparing("3".repeat(64), transaction.id().unwrap()).unwrap();
    let head = head
        .prepare(
            head.visible(&Default::default()).clone(),
            activation.activation_id().to_owned(),
            state,
        )
        .unwrap();
    storage
        .put(
            &router.capsule_ref_head_path(&capsule_ref_name_key(ref_name)),
            head.encode().unwrap(),
        )
        .await
        .unwrap();
    let missing_path = router.capsule_transaction_path(activation.activation_id());
    assert!(matches!(
        load_pointer_catalog(&router).await,
        Err(MetadataError::Storage { source: crab_storage::StorageError::NotFound { path } })
            if path == missing_path.to_string()
    ));

    // A present v2 root owns protocol selection even when older v1 data remains.
    // Retrying the same lazy handle after repair must not reuse a v1 fallback.
    let lookup = SharedFileIndexLookup::new_for_storage(&storage, prefix);
    let missing = lookup.lookup(&file).await;
    storage
        .put(&missing_path, activation.abort().unwrap().encode().unwrap())
        .await
        .unwrap();
    let repaired = lookup.lookup_batch(&[file]).await;
    lookup.close().await.unwrap();
    assert!(
        matches!(
            &missing,
            Err(MetadataError::Storage { source: crab_storage::StorageError::NotFound { path } })
                if path == &missing_path.to_string()
        ),
        "missing v2 activation was hidden: {missing:?}"
    );
    assert_eq!(repaired.unwrap(), vec![None]);
}

#[derive(Debug)]
struct PausedLookupStore {
    inner: Arc<dyn ObjectStore>,
    shard: ObjectPath,
    paused: AtomicBool,
    entered: Notify,
    release: Semaphore,
}

impl std::fmt::Display for PausedLookupStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("paused shard fixture")
    }
}

#[async_trait::async_trait]
impl ObjectStore for PausedLookupStore {
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
        location: &ObjectPath,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        if location.prefix_matches(&self.shard) && !self.paused.swap(true, Ordering::AcqRel) {
            self.entered.notify_one();
            self.release.acquire().await.unwrap().forget();
        }
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        paths: BoxStream<'static, object_store::Result<ObjectPath>>,
    ) -> BoxStream<'static, object_store::Result<ObjectPath>> {
        self.inner.delete_stream(paths)
    }

    fn list(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
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

#[tokio::test(flavor = "multi_thread")]
async fn captured_catalog_close_waits_for_active_reads_and_abandonment_releases_them() {
    for abandon in [false, true] {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let storage = crab_storage::Store::new(inner.clone());
        let router = crab_storage::StoreLayout::new(storage, "pinned/close".to_owned());
        let (catalog, _, _) = super::catalog_tests::capture(
            &router,
            super::catalog_tests::catalog(10_000),
            512 * 1024,
        )
        .await;
        let store = Arc::new(PausedLookupStore {
            inner,
            // Pause a cold catalog object after the descriptor is warm.
            shard: router.capsule_catalog_prefix(),
            paused: AtomicBool::new(true),
            entered: Notify::new(),
            release: Semaphore::new(0),
        });
        let router = crab_storage::StoreLayout::new(
            crab_storage::Store::new(store.clone()),
            "pinned/close".to_owned(),
        );
        let lookup = SharedFileIndexLookup::for_pointer_catalog(router, catalog);
        let file = hash_from_seed(0);
        lookup.lookup(&file).await.unwrap().unwrap();
        store.paused.store(false, Ordering::Release);
        let cold_file = hash_from_seed(9_999);
        let task = tokio::spawn({
            let lookup = lookup.clone();
            async move { lookup.lookup_batch(&[cold_file]).await }
        });
        timeout(Duration::from_secs(5), store.entered.notified())
            .await
            .expect("query reached captured page");
        let warm = timeout(Duration::from_secs(2), lookup.lookup(&file))
            .await
            .expect("a cold page read must not serialize a warm query")
            .unwrap();
        assert_eq!(warm, Some(hash_from_seed(1_000_000)));
        let close = lookup.clone().close();
        tokio::pin!(close);
        assert!(futures_util::poll!(close.as_mut()).is_pending());
        assert!(matches!(
            lookup.lookup(&file).await,
            Err(MetadataError::Internal(_))
        ));
        if abandon {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            store.release.add_permits(1);
            let result = timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(result, vec![Some(hash_from_seed(1_009_999))]);
        }
        timeout(Duration::from_secs(5), close)
            .await
            .unwrap()
            .unwrap();
    }
}
