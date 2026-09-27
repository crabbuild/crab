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
async fn current_lookup_preserves_missing_capsule_dependency_errors() {
    use crate::capsule_protocol::{
        Capsule, CapsulePointer, CapsuleRefEdit, CapsuleRefHead, CapsuleRun, CapsuleTransaction,
        CapsuleTransactionRecord, RepositoryRoot, RootRecord, capsule_ref_name_key, create_root,
        load_pointer_catalog,
    };

    for use_acceleration in [false, true] {
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

        let legacy =
            SharedFileIndexLookup::new_with_mode(storage.clone(), prefix, use_acceleration);
        let result = legacy.lookup(&file).await;
        legacy.close().await.unwrap();
        assert_eq!(
            result.unwrap(),
            Some(shard),
            "v1 remains readable without a v2 root"
        );

        let root = create_root(
            &router,
            RootRecord::encode(
                RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap(),
            )
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
        let lookup =
            SharedFileIndexLookup::new_with_mode(storage.clone(), prefix, use_acceleration);
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
}

#[derive(Debug)]
struct PausedLookupStore {
    inner: Arc<dyn ObjectStore>,
    shard: ObjectPath,
    paused: AtomicBool,
    pause_write: AtomicBool,
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
        if self.pause_write.swap(false, Ordering::AcqRel) {
            self.entered.notify_one();
            self.release.acquire().await.unwrap().forget();
        }
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
        if location == &self.shard && !self.paused.swap(true, Ordering::AcqRel) {
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

#[tokio::test]
async fn first_lookup_allows_index_hits_while_close_waits_for_active_reads() {
    for first_is_batch in [false, true] {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let file_hash = hash_from_seed(42);
        let missing_hash = hash_from_seed(43);
        let (body, shard_hash) = shard_with_file(file_hash);
        let prefix = "shared/repo";
        seed_file_index(Arc::clone(&inner), prefix, &[(file_hash, shard_hash)]).await;
        let storage = crab_storage::Store::new(Arc::clone(&inner));
        let layout = crab_storage::StoreLayout::new(storage.clone(), prefix.to_owned());
        let shard = layout.shard_path(&shard_hash);
        storage.put(&shard, Bytes::from(body)).await.unwrap();
        let store = Arc::new(PausedLookupStore {
            inner,
            shard,
            paused: AtomicBool::new(false),
            pause_write: AtomicBool::new(false),
            entered: Notify::new(),
            release: Semaphore::new(0),
        });
        let lookup = SharedFileIndexLookup::new(store.clone(), prefix);
        let first = tokio::spawn({
            let lookup = lookup.clone();
            async move {
                if first_is_batch {
                    lookup.lookup_batch(&[missing_hash]).await
                } else {
                    lookup.lookup(&missing_hash).await.map(|hit| vec![hit])
                }
            }
        });
        timeout(Duration::from_secs(5), store.entered.notified())
            .await
            .expect("first lookup reached the canonical shard read");

        // A canonical miss may wait on origin while an unrelated index hit
        // already has all of its generation-pinned evidence in SlateDB.
        let hit = timeout(Duration::from_secs(2), async {
            if first_is_batch {
                lookup.lookup(&file_hash).await.map(|hit| vec![hit])
            } else {
                lookup.lookup_batch(&[file_hash]).await
            }
        })
        .await;
        let close = lookup.clone().close();
        tokio::pin!(close);
        let close_waited = futures_util::poll!(close.as_mut()).is_pending();
        let after_close = lookup.lookup(&file_hash).await;
        store.release.add_permits(1);
        let first_result = timeout(Duration::from_secs(5), first)
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(5), close)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(first_result.unwrap(), vec![None]);
        assert!(close_waited, "close must wait for the paused read");
        assert!(matches!(after_close, Err(MetadataError::Internal(_))));
        assert_eq!(
            hit.expect("index hit was blocked by the first canonical miss")
                .unwrap(),
            vec![Some(shard_hash)],
            "first_is_batch={first_is_batch}"
        );
    }
}

#[tokio::test]
async fn concurrent_close_waits_for_reader_checkpoint_cleanup() {
    let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let file_hash = hash_from_seed(42);
    let shard_hash = hash_from_seed(43);
    let prefix = "shared/close";
    seed_file_index(Arc::clone(&inner), prefix, &[(file_hash, shard_hash)]).await;
    let store = Arc::new(PausedLookupStore {
        inner: Arc::clone(&inner),
        shard: ObjectPath::from("unused-shard"),
        paused: AtomicBool::new(true),
        pause_write: AtomicBool::new(false),
        entered: Notify::new(),
        release: Semaphore::new(0),
    });
    let admin = slatedb::admin::Admin::builder(file_index_path(prefix), inner).build();
    let lookup = SharedFileIndexLookup::new(store.clone(), prefix);
    assert_eq!(lookup.lookup(&file_hash).await.unwrap(), Some(shard_hash));
    assert_eq!(admin.list_checkpoints(None).await.unwrap().len(), 1);

    // Closing a managed reader persists checkpoint removal. Hold that write
    // so a second owner cannot mistake an empty session slot for completion.
    store.pause_write.store(true, Ordering::Release);
    let first = tokio::spawn(lookup.clone().close());
    timeout(Duration::from_secs(5), store.entered.notified())
        .await
        .expect("reader cleanup reached checkpoint persistence");
    let second = lookup.close();
    tokio::pin!(second);
    let second_waited = futures_util::poll!(second.as_mut()).is_pending();
    let pending_checkpoints = admin.list_checkpoints(None).await.unwrap().len();
    store.release.add_permits(1);
    timeout(Duration::from_secs(5), first)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    if second_waited {
        timeout(Duration::from_secs(5), second)
            .await
            .unwrap()
            .unwrap();
    }

    assert_eq!(pending_checkpoints, 1);
    assert!(admin.list_checkpoints(None).await.unwrap().is_empty());
    assert!(
        second_waited,
        "concurrent close returned before reader cleanup"
    );
}
