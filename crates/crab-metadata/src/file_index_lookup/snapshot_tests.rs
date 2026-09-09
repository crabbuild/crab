use super::tests::{hash_from_seed, seed_file_index, shard_with_file, shard_with_xorb};
use super::*;
use bytes::Bytes;
use futures_util::stream::BoxStream;
use object_store::memory::InMemory;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use std::sync::atomic::AtomicUsize;

#[derive(Debug)]
struct ReadOnlyStore {
    inner: Arc<dyn ObjectStore>,
    writes: AtomicUsize,
}

impl ReadOnlyStore {
    fn denied(&self) -> object_store::Error {
        self.writes.fetch_add(1, Ordering::Relaxed);
        object_store::Error::PermissionDenied {
            path: "read-only fixture".to_owned(),
            source: Box::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
        }
    }
}

impl std::fmt::Display for ReadOnlyStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("read-only fixture")
    }
}

#[async_trait::async_trait]
impl ObjectStore for ReadOnlyStore {
    async fn put_opts(
        &self,
        _: &ObjectPath,
        _: PutPayload,
        _: PutOptions,
    ) -> object_store::Result<PutResult> {
        Err(self.denied())
    }

    async fn put_multipart_opts(
        &self,
        _: &ObjectPath,
        _: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        Err(self.denied())
    }

    async fn get_opts(
        &self,
        location: &ObjectPath,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        _: BoxStream<'static, object_store::Result<ObjectPath>>,
    ) -> BoxStream<'static, object_store::Result<ObjectPath>> {
        stream::iter([Err(self.denied())]).boxed()
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
        _: &ObjectPath,
        _: &ObjectPath,
        _: CopyOptions,
    ) -> object_store::Result<()> {
        Err(self.denied())
    }
}

#[tokio::test]
async fn captured_snapshot_lookup_uses_no_write_permissions() {
    for scoped in [false, true] {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let file_hash = hash_from_seed(67);
        let (body, shard_hash) = shard_with_file(file_hash);
        seed_file_index(
            Arc::clone(&inner),
            "captured/repo",
            &[(file_hash, shard_hash)],
        )
        .await;
        let storage = crab_storage::Store::new(Arc::clone(&inner));
        let router = crab_storage::StoreLayout::new(storage.clone(), "captured/repo".to_owned());
        storage
            .put(&router.shard_path(&shard_hash), Bytes::from(body))
            .await
            .unwrap();
        let snapshot = crate::manifest_store::read_repository_snapshot(&storage, &router)
            .await
            .unwrap();
        let readonly = Arc::new(ReadOnlyStore {
            inner,
            writes: AtomicUsize::new(0),
        });
        let mut storage = crab_storage::Store::new(readonly.clone());
        if scoped {
            storage = storage.with_storage_scope(crab_storage::StorageScope {
                repo_prefix: "captured/repo".to_owned(),
                global_prefix: ".crab".to_owned(),
                source_repo: "logical/repo".to_owned(),
                scope_hash: "read-scope".to_owned(),
            });
        }
        let prefix = if scoped {
            "logical/repo"
        } else {
            "captured/repo"
        };
        let router = crab_storage::StoreLayout::new(storage, prefix.to_owned());
        let session = FileIndexLookupSession::from_snapshot(router, &snapshot).unwrap();
        let result = session.lookup_batch(&[file_hash, hash_from_seed(99)]).await;
        session.close().await.unwrap();
        assert_eq!(
            (result.unwrap(), readonly.writes.load(Ordering::Relaxed)),
            (vec![Some(shard_hash), None], 0)
        );
    }
}

#[tokio::test]
async fn captured_snapshot_does_not_follow_a_newer_manifest() {
    let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let file_hash = hash_from_seed(68);
    let (body, shard_hash) = shard_with_file(file_hash);
    seed_file_index(
        Arc::clone(&inner),
        "captured/repo",
        &[(file_hash, shard_hash)],
    )
    .await;
    let storage = crab_storage::Store::new(inner);
    let router = crab_storage::StoreLayout::new(storage.clone(), "captured/repo".to_owned());
    storage
        .put(&router.shard_path(&shard_hash), Bytes::from(body))
        .await
        .unwrap();
    let snapshot = crate::manifest_store::read_repository_snapshot(&storage, &router)
        .await
        .unwrap();
    let mut next = snapshot.manifest.clone();
    next.generation += 1;
    next.shard_index_hash.clear();
    next.seal_git_validation();
    crate::manifest_store::write_manifest_cas(&storage, &router, &next, &snapshot.manifest_etag)
        .await
        .unwrap();

    let captured = FileIndexLookupSession::from_snapshot(router.clone(), &snapshot).unwrap();
    let current = crate::manifest_store::read_repository_snapshot(&storage, &router)
        .await
        .unwrap();
    let latest = FileIndexLookupSession::from_snapshot(router, &current).unwrap();
    let results = (
        captured.lookup(&file_hash).await.unwrap(),
        latest.lookup(&file_hash).await.unwrap(),
    );
    captured.close().await.unwrap();
    latest.close().await.unwrap();
    assert_eq!(results, (Some(shard_hash), None));
}

#[tokio::test]
async fn duplicate_recipe_selection_is_stable_across_sessions() {
    let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let file_hash = hash_from_seed(69);
    let (first_body, first_hash) = shard_with_file(file_hash);
    let (second_body, second_hash) = shard_with_xorb(file_hash, hash_from_seed(45));
    seed_file_index(
        Arc::clone(&inner),
        "captured/repo",
        &[(file_hash, first_hash), (file_hash, second_hash)],
    )
    .await;
    let storage = crab_storage::Store::new(inner);
    let router = crab_storage::StoreLayout::new(storage.clone(), "captured/repo".to_owned());
    for (hash, body) in [(first_hash, first_body), (second_hash, second_body)] {
        storage
            .put(&router.shard_path(&hash), Bytes::from(body))
            .await
            .unwrap();
    }
    let snapshot = crate::manifest_store::read_repository_snapshot(&storage, &router)
        .await
        .unwrap();
    let mut selected = Vec::new();
    for _ in 0..12 {
        let session = FileIndexLookupSession::from_snapshot(router.clone(), &snapshot).unwrap();
        selected.push(session.lookup(&file_hash).await.unwrap());
        session.close().await.unwrap();
    }
    assert_eq!(selected, vec![Some(first_hash.min(second_hash)); 12]);
}

#[tokio::test]
async fn accelerated_snapshot_lookup_preserves_capture_and_scoped_read_only_access() {
    for scoped in [false, true] {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let file_hash = hash_from_seed(70);
        let (body, shard_hash) = shard_with_file(file_hash);
        seed_file_index(
            Arc::clone(&inner),
            "captured/accelerated",
            &[(file_hash, shard_hash)],
        )
        .await;
        let storage = crab_storage::Store::new(Arc::clone(&inner));
        let router =
            crab_storage::StoreLayout::new(storage.clone(), "captured/accelerated".to_owned());
        storage
            .put(&router.shard_path(&shard_hash), Bytes::from(body))
            .await
            .unwrap();
        let snapshot = crate::manifest_store::read_repository_snapshot(&storage, &router)
            .await
            .unwrap();
        let mut next = snapshot.manifest.clone();
        next.generation += 1;
        next.shard_index_hash.clear();
        next.seal_git_validation();
        crate::manifest_store::write_manifest_cas(
            &storage,
            &router,
            &next,
            &snapshot.manifest_etag,
        )
        .await
        .unwrap();
        let readonly = Arc::new(ReadOnlyStore {
            inner,
            writes: AtomicUsize::new(0),
        });
        let lookup_storage = if scoped {
            crab_storage::Store::new(readonly.clone()).with_storage_scope(
                crab_storage::StorageScope {
                    repo_prefix: "captured/accelerated".to_owned(),
                    global_prefix: ".crab".to_owned(),
                    source_repo: "logical/repo".to_owned(),
                    scope_hash: "read-scope".to_owned(),
                },
            )
        } else {
            storage
        };
        let lookup_router =
            crab_storage::StoreLayout::new(lookup_storage, "captured/accelerated".to_owned());
        let session = FileIndexLookupSession::open_from_snapshot(lookup_router, &snapshot)
            .await
            .unwrap();
        let accelerated = session.reader.is_some();
        let result = session.lookup_batch(&[file_hash, hash_from_seed(99)]).await;
        session.close().await.unwrap();
        assert_eq!(
            (
                result.unwrap(),
                accelerated,
                readonly.writes.load(Ordering::Relaxed)
            ),
            (vec![Some(shard_hash), None], !scoped, 0),
        );
    }
}

#[tokio::test]
async fn lazy_pinned_index_ignores_latest_state_and_never_attempts_writes() {
    let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let file = hash_from_seed(71);
    let (body, shard) = shard_with_file(file);
    seed_file_index(inner.clone(), "lazy/repo", &[(file, shard)]).await;
    let storage = crab_storage::Store::new(inner.clone());
    let router = crab_storage::StoreLayout::new(storage.clone(), "lazy/repo".to_owned());
    storage
        .put(&router.shard_path(&shard), Bytes::from(body))
        .await
        .unwrap();
    let snapshot = crate::manifest_store::read_repository_snapshot(&storage, &router)
        .await
        .unwrap();
    let readonly = Arc::new(ReadOnlyStore {
        inner,
        writes: AtomicUsize::new(0),
    });
    let layout = crab_storage::StoreLayout::new(
        crab_storage::Store::new(readonly.clone()),
        "lazy/repo".to_owned(),
    );
    let lookup = SharedFileIndexLookup::for_shard_index(
        layout.clone(),
        snapshot.manifest.shard_index_hash.clone(),
        snapshot.manifest.generation,
        super::tests::lookup_limits(),
    );
    let mut next = snapshot.manifest.clone();
    next.generation += 1;
    next.shard_index_hash.clear();
    next.seal_git_validation();
    crate::manifest_store::write_manifest_cas(&storage, &router, &next, &snapshot.manifest_etag)
        .await
        .unwrap();
    assert_eq!(
        lookup
            .lookup_batch(&[file, hash_from_seed(99)])
            .await
            .unwrap(),
        vec![Some(shard), None]
    );
    let closed = lookup.clone();
    lookup.close().await.unwrap();
    assert!(closed.lookup(&file).await.is_err());
    let denied = SharedFileIndexLookup::for_shard_index(
        layout,
        snapshot.manifest.shard_index_hash,
        snapshot.manifest.generation,
        FileIndexLookupLimits {
            max_shard_visits: 0,
            ..super::tests::lookup_limits()
        },
    );
    assert!(matches!(
        denied.lookup(&file).await,
        Err(MetadataError::FileLookupLimit {
            resource: "shard visits",
            maximum: 0
        })
    ));
    denied.close().await.unwrap();
    assert_eq!(readonly.writes.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn lazy_pinned_index_retains_invalid_hash_source_without_writes() {
    let readonly = Arc::new(ReadOnlyStore {
        inner: Arc::new(InMemory::new()),
        writes: AtomicUsize::new(0),
    });
    let layout = crab_storage::StoreLayout::new(
        crab_storage::Store::new(readonly.clone()),
        "lazy/repo".to_owned(),
    );
    let lookup = SharedFileIndexLookup::for_shard_index(
        layout,
        "invalid".to_owned(),
        1,
        super::tests::lookup_limits(),
    );
    let error = lookup.lookup(&hash_from_seed(1)).await.unwrap_err();
    lookup.close().await.unwrap();
    match error {
        MetadataError::Io { source } => {
            assert_eq!(source.kind(), std::io::ErrorKind::InvalidData);
            assert!(source.get_ref().is_some());
        }
        other => panic!("unexpected error: {other:?}"),
    }
    assert_eq!(readonly.writes.load(Ordering::Relaxed), 0);
}
