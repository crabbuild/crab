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

#[derive(Debug)]
struct PausedShardStore {
    inner: Arc<dyn ObjectStore>,
    shard: ObjectPath,
    paused: AtomicBool,
    entered: Notify,
    release: Semaphore,
}

impl std::fmt::Display for PausedShardStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("paused shard fixture")
    }
}

#[async_trait::async_trait]
impl ObjectStore for PausedShardStore {
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
        let store = Arc::new(PausedShardStore {
            inner,
            shard,
            paused: AtomicBool::new(false),
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
