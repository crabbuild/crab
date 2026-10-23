use std::{fmt, sync::atomic::AtomicBool};

use futures_util::stream::BoxStream;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult, path::Path,
};

use super::{tests::*, *};

#[derive(Debug)]
struct PausedCapsuleStore {
    inner: Arc<dyn ObjectStore>,
    capsule_prefix: String,
    lock_path: Path,
    pause_next: AtomicBool,
    waiting: AtomicBool,
    paused: Notify,
    resume: Notify,
    renewal_failed: Notify,
}

impl fmt::Display for PausedCapsuleStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PausedCapsuleStore")
    }
}

#[async_trait::async_trait]
impl ObjectStore for PausedCapsuleStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        if location.as_ref().starts_with(&self.capsule_prefix)
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
    let backend = Arc::new(PausedCapsuleStore {
        inner: Arc::clone(repository.store.inner()),
        capsule_prefix: format!("{}/v2/capsules/", repository.layout.repo_prefix()),
        lock_path: Path::from(
            crab_coordination::push_lock::push_lock_path(
                repository.layout.repo_prefix(),
                "refs/heads/main",
            )
            .unwrap(),
        ),
        pause_next: AtomicBool::new(true),
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
