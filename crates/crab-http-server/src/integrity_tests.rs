use bytes::Bytes;
use futures_util::stream::BoxStream;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    ObjectStoreExt as _, PutMode, PutMultipartOptions, PutOptions, PutPayload, PutResult,
    path::Path,
};
use sha2::Digest as _;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

use super::*;

#[derive(Debug)]
struct ScrubLeaseFaultStore {
    inner: Arc<dyn ObjectStore>,
    blocked_read: String,
    lock_path: String,
    block_once: AtomicBool,
    replacement_installed: AtomicBool,
    read_started: Notify,
    release_read: Notify,
    failed_renewal: Notify,
}

impl std::fmt::Display for ScrubLeaseFaultStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("scrub-lease-fault-store")
    }
}

#[async_trait::async_trait]
impl ObjectStore for ScrubLeaseFaultStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        let renewal = location.as_ref() == self.lock_path
            && self.replacement_installed.load(Ordering::Acquire)
            && matches!(options.mode, PutMode::Update(_));
        let result = self.inner.put_opts(location, payload, options).await;
        if renewal && result.is_err() {
            self.failed_renewal.notify_one();
        }
        result
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        if !options.head
            && location.as_ref() == self.blocked_read
            && self.block_once.swap(false, Ordering::AcqRel)
        {
            self.read_started.notify_one();
            self.release_read.notified().await;
        }
        self.inner.get_opts(location, options).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        options: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

#[tokio::test]
async fn scrub_reports_dependency_loss_without_erasing_the_last_complete_proof() {
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let layout = StoreLayout::new(store.clone(), "integrity".into());
    let content = Bytes::from_static(b"content");
    let oid: [u8; 32] = sha2::Sha256::digest(&content).into();
    let pointer = crab_git::LfsPointer {
        oid,
        size: content.len() as u64,
        extensions: Vec::new(),
    };
    crate::test_git::publish_blob(&layout, &pointer.serialize()).await;
    let lfs = crab_lfs::LfsObjectStore::new(store, layout.repo_prefix());
    lfs.put(&oid, content).await.unwrap();
    let status = Status::default();
    let admission = Arc::new(Semaphore::new(1));
    let cancellation = CancellationToken::new();

    scrub_target(&layout, &status, Arc::clone(&admission), &cancellation)
        .await
        .unwrap();
    let complete = status.snapshot();
    assert_eq!(complete.state, State::Complete);
    assert_eq!(
        complete
            .last_complete
            .as_ref()
            .unwrap()
            .reachable_lfs_objects,
        1
    );

    lfs.delete(&oid).await.unwrap();
    assert!(matches!(
        scrub_target(&layout, &status, admission, &cancellation).await,
        Err(Error::Read(crab_read::ReadError::Lfs(
            crab_lfs::LfsError::ObjectMissing { .. }
        )))
    ));
    let failed = status.snapshot();
    assert_eq!(failed.state, State::Failed);
    assert!(failed.error.is_some());
    assert_eq!(failed.last_complete, complete.last_complete);
}

#[tokio::test]
async fn deployment_report_prevents_duplicate_scrubs_and_expires_fail_closed() {
    let mut server = crate::server::maintenance_tests::fixture_without_cells().await;
    let repository = server
        .repositories
        .get(&("team".into(), "repo".into()))
        .unwrap();
    let store = repository.store.clone();
    let layout = repository.layout.clone();
    let content = Bytes::from_static(b"deployment content");
    let oid: [u8; 32] = sha2::Sha256::digest(&content).into();
    let pointer = crab_git::LfsPointer {
        oid,
        size: content.len() as u64,
        extensions: Vec::new(),
    };
    crate::test_git::publish_blob_at_current(&layout, &pointer.serialize()).await;
    let lfs = crab_lfs::LfsObjectStore::new(store.clone(), layout.repo_prefix());
    lfs.put(&oid, content).await.unwrap();
    drop(repository);
    let root = crate::storage_root::StorageRoot::memory(store, "integrity-server");
    Arc::get_mut(&mut server).unwrap().catalog =
        Some(crate::catalog::CatalogStore::new(root.clone()));
    let mut locks = crab_coordination::PushLockAcquireContext::new(root.store.inner().clone());

    assert_eq!(
        coordinated_cycle(&server, &root, &mut locks).await.unwrap(),
        SCRUB_INTERVAL
    );
    let repository = server
        .repositories
        .get(&("team".into(), "repo".into()))
        .unwrap();
    assert_eq!(repository.integrity.snapshot().state, State::Complete);
    let (body, stale_etag) = read_report(&root).await.unwrap().unwrap();
    let report = decode_report(&body).unwrap();
    store_report(&root, &report, Some(stale_etag.clone()))
        .await
        .unwrap();
    assert!(matches!(
        store_report(&root, &report, Some(stale_etag)).await,
        Err(Error::Storage(
            crab_storage::StorageError::StateConflict { .. }
        ))
    ));
    lfs.delete(&oid).await.unwrap();
    repository.integrity.replace(Status::default().snapshot());

    assert!(coordinated_cycle(&server, &root, &mut locks).await.unwrap() > REPORT_RETRY_INTERVAL);
    assert_eq!(repository.integrity.snapshot().state, State::Complete);
    let (body, etag) = read_report(&root).await.unwrap().unwrap();
    let mut stale = decode_report(&body).unwrap();
    stale.generated_at_unix = 0;
    store_report(&root, &stale, Some(etag)).await.unwrap();
    let held = crab_coordination::PushLock::acquire_internal(
        root.store.inner(),
        &root.prefix,
        SCRUB_RESOURCE,
        crab_coordination::DEFAULT_PUSH_LOCK_TTL,
    )
    .await
    .unwrap();
    let mut contender = crab_coordination::PushLockAcquireContext::new(root.store.inner().clone());
    assert_eq!(
        coordinated_cycle(&server, &root, &mut contender)
            .await
            .unwrap(),
        REPORT_RETRY_INTERVAL
    );
    assert_eq!(repository.integrity.snapshot().state, State::Complete);
    held.release().await.unwrap();

    assert_eq!(
        coordinated_cycle(&server, &root, &mut contender)
            .await
            .unwrap(),
        SCRUB_INTERVAL
    );
    assert_eq!(repository.integrity.snapshot().state, State::Failed);
    assert_eq!(
        load_report(&root).await.unwrap().unwrap().repositories[0]
            .proof
            .state,
        State::Failed
    );
    crate::server::maintenance_tests::close(&server).await;
}

#[tokio::test]
async fn lease_loss_during_scrub_cancels_before_report_publication() {
    let mut server = crate::server::maintenance_tests::fixture_without_cells().await;
    let repository = server
        .repositories
        .get(&("team".into(), "repo".into()))
        .unwrap();
    let origin = repository.store.clone();
    let content = Bytes::from_static(b"lease-loss content");
    let oid: [u8; 32] = sha2::Sha256::digest(&content).into();
    let pointer = crab_git::LfsPointer {
        oid,
        size: content.len() as u64,
        extensions: Vec::new(),
    };
    crate::test_git::publish_blob_at_current(&repository.layout, &pointer.serialize()).await;
    let lfs = crab_lfs::LfsObjectStore::new(origin.clone(), repository.layout.repo_prefix());
    lfs.put(&oid, content).await.unwrap();
    let blocked_read = lfs.object_path_for(&oid).to_string();
    drop(repository);

    let root_prefix = "integrity-server";
    let lock_path = crab_coordination::internal_lock_path(root_prefix, SCRUB_RESOURCE).unwrap();
    let fault = Arc::new(ScrubLeaseFaultStore {
        inner: Arc::clone(origin.inner()),
        blocked_read,
        lock_path: lock_path.clone(),
        block_once: AtomicBool::new(true),
        replacement_installed: AtomicBool::new(false),
        read_started: Notify::new(),
        release_read: Notify::new(),
        failed_renewal: Notify::new(),
    });
    let fault_store = Store::with_retry(
        fault.clone(),
        crab_storage::RetryPolicy {
            max_attempts: 1,
            base: Duration::ZERO,
            cap: Duration::ZERO,
        },
    );
    let mutable = Arc::get_mut(&mut server).unwrap();
    let repository = mutable
        .repositories
        .get_mut(&("team".into(), "repo".into()))
        .unwrap();
    repository.store = fault_store.clone();
    repository.layout = StoreLayout::new(fault_store.clone(), repository.config.prefix.clone());
    let root = crate::storage_root::StorageRoot::memory(fault_store, root_prefix);
    mutable.catalog = Some(crate::catalog::CatalogStore::new(root.clone()));

    let cancellation = CancellationToken::new();
    let cycle_cancellation = cancellation.clone();
    let cycle_server = Arc::clone(&server);
    let cycle_root = root.clone();
    let cycle = tokio::spawn(async move {
        let mut locks =
            crab_coordination::PushLockAcquireContext::new(cycle_root.store.inner().clone());
        coordinated_cycle_with_lease(
            &cycle_server,
            &cycle_root,
            &mut locks,
            cycle_cancellation,
            Some(Duration::from_secs(1)),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), fault.read_started.notified())
        .await
        .unwrap();
    let replacement = crab_coordination::PushLockPayload::new("replacement", u64::MAX, 60);
    origin
        .inner()
        .put(
            &Path::from(lock_path),
            Bytes::from(serde_json::to_vec(&replacement).unwrap()).into(),
        )
        .await
        .unwrap();
    fault.replacement_installed.store(true, Ordering::Release);
    tokio::time::timeout(Duration::from_secs(5), fault.failed_renewal.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), cancellation.cancelled())
        .await
        .unwrap();
    fault.release_read.notify_one();

    assert!(matches!(cycle.await.unwrap(), Err(Error::Cancelled)));
    assert!(load_report(&root).await.unwrap().is_none());
    let lock: crab_coordination::PushLockPayload = serde_json::from_slice(
        &origin
            .inner()
            .get(&Path::from(
                crab_coordination::internal_lock_path(root_prefix, SCRUB_RESOURCE).unwrap(),
            ))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(lock, replacement);
    crate::server::maintenance_tests::close(&server).await;
}
