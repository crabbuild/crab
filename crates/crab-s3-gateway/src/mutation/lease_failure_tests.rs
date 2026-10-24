use std::{fmt, sync::atomic::AtomicBool};

use futures_util::stream::BoxStream;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult, path::Path,
};

use super::{tests::*, *};

#[derive(Debug)]
struct ControlledUploadStore {
    inner: Arc<dyn ObjectStore>,
    upload_prefix: String,
    lock_path: Path,
    pause_next: AtomicBool,
    fail_next: AtomicBool,
    waiting: AtomicBool,
    paused: Notify,
    resume: Notify,
    renewal_failed: Notify,
}

impl fmt::Display for ControlledUploadStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ControlledUploadStore")
    }
}

#[async_trait::async_trait]
impl ObjectStore for ControlledUploadStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        if location.as_ref().starts_with(&self.upload_prefix)
            && self.fail_next.swap(false, Ordering::AcqRel)
        {
            return Err(object_store::Error::PermissionDenied {
                path: location.to_string(),
                source: Box::new(std::io::Error::other("injected immutable upload failure")),
            });
        }
        if location.as_ref().starts_with(&self.upload_prefix)
            && self.pause_next.swap(false, Ordering::AcqRel)
        {
            self.waiting.store(true, Ordering::Release);
            self.paused.notify_one();
            self.resume.notified().await;
            self.waiting.store(false, Ordering::Release);
        }
        let result = self.inner.put_opts(location, payload, options).await;
        if location == &self.lock_path && self.waiting.load(Ordering::Acquire) && result.is_err() {
            self.renewal_failed.notify_one();
        }
        result
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        let result = self.inner.get_opts(location, options).await;
        if location == &self.lock_path && self.waiting.load(Ordering::Acquire) && result.is_err() {
            self.renewal_failed.notify_one();
        }
        result
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lost_write_lease_does_not_cancel_later_requests() {
    let (repository, runtime, cancel) = capsule_fixture().await;
    let backend = Arc::new(ControlledUploadStore {
        inner: Arc::clone(repository.store.inner()),
        upload_prefix: format!("{}/v2/capsules/", repository.layout.repo_prefix()),
        lock_path: Path::from(
            crab_coordination::push_lock::push_lock_path(
                repository.layout.repo_prefix(),
                "refs/heads/main",
            )
            .unwrap(),
        ),
        pause_next: AtomicBool::new(true),
        fail_next: AtomicBool::new(false),
        waiting: AtomicBool::new(false),
        paused: Notify::new(),
        resume: Notify::new(),
        renewal_failed: Notify::new(),
    });
    let repository = Arc::new(
        Repository::new(
            repository.config.clone(),
            crab_storage::Store::new(backend.clone()),
        )
        .unwrap(),
    );
    let coordinator = Arc::new(Coordinator::new(
        Arc::clone(&runtime),
        crab_remote_git::RepositoryOptions::default(),
        cancel.clone(),
        Metrics::new().unwrap(),
    ));
    let pending = {
        let repository = Arc::clone(&repository);
        let coordinator = Arc::clone(&coordinator);
        let cancel = cancel.clone();
        tokio::spawn(async move {
            coordinator
                .apply(
                    &repository,
                    "refs/heads/main",
                    &crab_remote_git::GitPath::new(b"interrupted".to_vec()).unwrap(),
                    Change::Put {
                        bytes: Bytes::from_static(b"uncommitted"),
                        track_lfs: false,
                        attributes: Box::default(),
                        condition: PutCondition::None,
                    },
                    "user",
                    &cancel,
                )
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), backend.paused.notified())
        .await
        .unwrap();
    backend.inner.delete(&backend.lock_path).await.unwrap();
    tokio::time::timeout(REF_LOCK_TTL, backend.renewal_failed.notified())
        .await
        .unwrap();
    // Let the renewal failure reach its cancellation boundary before the
    // deliberately blocked immutable upload returns to publication admission.
    tokio::time::sleep(Duration::from_millis(100)).await;
    backend.resume.notify_one();
    assert!(pending.await.unwrap().is_err());
    assert!(!cancel.is_cancelled());
    coordinated_put(&coordinator, &repository, &cancel, "after", b"recovered").await;
    assert_eq!(
        read(&repository, Arc::clone(&runtime), &cancel, "after").await,
        Bytes::from_static(b"recovered")
    );
    coordinator.shutdown().await;
    runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attribute_upload_failure_cannot_publish_and_retry_recovers() {
    let (repository, runtime, cancel) = capsule_fixture().await;
    let backend = Arc::new(ControlledUploadStore {
        inner: Arc::clone(repository.store.inner()),
        upload_prefix: format!("{}/s3/attributes/", repository.layout.repo_prefix()),
        lock_path: Path::from(
            crab_coordination::push_lock::push_lock_path(
                repository.layout.repo_prefix(),
                "refs/heads/main",
            )
            .unwrap(),
        ),
        pause_next: AtomicBool::new(false),
        fail_next: AtomicBool::new(true),
        waiting: AtomicBool::new(false),
        paused: Notify::new(),
        resume: Notify::new(),
        renewal_failed: Notify::new(),
    });
    let repository =
        Repository::new(repository.config.clone(), crab_storage::Store::new(backend)).unwrap();
    let coordinator = Coordinator::new(
        Arc::clone(&runtime),
        crab_remote_git::RepositoryOptions::default(),
        cancel.clone(),
        Metrics::new().unwrap(),
    );
    let result = coordinator
        .apply(
            &repository,
            "refs/heads/main",
            &crab_remote_git::GitPath::new("object").unwrap(),
            Change::Put {
                bytes: Bytes::from(vec![7; 1024 * 1024]),
                track_lfs: false,
                attributes: Box::default(),
                condition: PutCondition::None,
            },
            "user",
            &cancel,
        )
        .await;
    let refs_after_failure = crab_read::capsule_protocol::read_visible_refs(&repository.layout)
        .await
        .unwrap();
    coordinated_put(&coordinator, &repository, &cancel, "object", b"retried").await;
    let bytes = read(&repository, Arc::clone(&runtime), &cancel, "object").await;
    coordinator.shutdown().await;
    runtime.shutdown().await;

    assert!(result.is_err());
    assert!(refs_after_failure.is_empty());
    assert_eq!(bytes, b"retried".as_slice());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_drains_admitted_upload_and_releases_its_lease() {
    let (repository, runtime, cancel) = capsule_fixture().await;
    let backend = Arc::new(ControlledUploadStore {
        inner: Arc::clone(repository.store.inner()),
        upload_prefix: format!("{}/v2/capsules/", repository.layout.repo_prefix()),
        lock_path: Path::from(
            crab_coordination::push_lock::push_lock_path(
                repository.layout.repo_prefix(),
                "refs/heads/main",
            )
            .unwrap(),
        ),
        pause_next: AtomicBool::new(true),
        fail_next: AtomicBool::new(false),
        waiting: AtomicBool::new(false),
        paused: Notify::new(),
        resume: Notify::new(),
        renewal_failed: Notify::new(),
    });
    let repository = Arc::new(
        Repository::new(
            repository.config.clone(),
            crab_storage::Store::new(backend.clone()),
        )
        .unwrap(),
    );
    let coordinator = Arc::new(Coordinator::new(
        Arc::clone(&runtime),
        crab_remote_git::RepositoryOptions::default(),
        cancel.clone(),
        Metrics::new().unwrap(),
    ));
    let pending = {
        let repository = Arc::clone(&repository);
        let coordinator = Arc::clone(&coordinator);
        let cancel = cancel.clone();
        tokio::spawn(async move {
            coordinator
                .apply(
                    &repository,
                    "refs/heads/main",
                    &crab_remote_git::GitPath::new("interrupted").unwrap(),
                    Change::Put {
                        bytes: Bytes::from_static(b"uncommitted"),
                        track_lfs: false,
                        attributes: Box::default(),
                        condition: PutCondition::None,
                    },
                    "user",
                    &cancel,
                )
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), backend.paused.notified())
        .await
        .unwrap();
    let shutdown = coordinator.shutdown();
    tokio::pin!(shutdown);
    let returned_before_upload = tokio::time::timeout(Duration::from_millis(100), &mut shutdown)
        .await
        .is_ok();
    backend.resume.notify_one();
    if !returned_before_upload {
        tokio::time::timeout(Duration::from_secs(5), shutdown)
            .await
            .unwrap();
    }
    let result = pending.await.unwrap();
    let lease_released = !PushLock::ref_lease_is_claimed(
        &backend.inner,
        repository.layout.repo_prefix(),
        "refs/heads/main",
    )
    .await
    .unwrap();
    // Complete cleanup even when the regression returns early, so the failed
    // assertion cannot strand a storage future during Tokio runtime teardown.
    tokio::time::timeout(Duration::from_secs(5), async {
        while PushLock::ref_lease_is_claimed(
            &backend.inner,
            repository.layout.repo_prefix(),
            "refs/heads/main",
        )
        .await
        .unwrap()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    runtime.shutdown().await;
    assert!(
        !returned_before_upload,
        "shutdown abandoned an admitted upload"
    );
    assert!(result.is_err());
    assert!(
        lease_released,
        "shutdown returned before releasing the ref lease"
    );
}
