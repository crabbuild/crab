use super::*;
use object_store::ObjectStoreExt;
use object_store::memory::InMemory;

#[derive(Debug, Default)]
struct PausedUpdateStore {
    inner: InMemory,
    pause: AtomicBool,
    entered: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

impl std::fmt::Display for PausedUpdateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("paused-gc-update")
    }
}

#[async_trait::async_trait]
impl ObjectStore for PausedUpdateStore {
    async fn put_opts(
        &self,
        path: &Path,
        payload: object_store::PutPayload,
        options: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        if matches!(options.mode, object_store::PutMode::Update(_))
            && self.pause.swap(false, Ordering::SeqCst)
        {
            self.entered.notify_one();
            self.resume.notified().await;
        }
        self.inner.put_opts(path, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        path: &Path,
        options: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(path, options).await
    }

    async fn get_opts(
        &self,
        path: &Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        self.inner.get_opts(path, options).await
    }

    fn delete_stream(
        &self,
        paths: futures_util::stream::BoxStream<'static, object_store::Result<Path>>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(paths)
    }

    fn list(
        &self,
        prefix: Option<&Path>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
    {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn renewal_preserves_concurrent_writer_admission() {
    let backend = Arc::new(PausedUpdateStore::default());
    let store: Arc<dyn ObjectStore> = backend.clone();
    let first = GcFenceLease::acquire_writer(&store, "repo", Duration::from_secs(30))
        .await
        .unwrap();
    backend.pause.store(true, Ordering::SeqCst);
    let renewing = first.clone();
    let task = tokio::spawn(async move { renewing.renew().await });
    tokio::time::timeout(Duration::from_secs(5), backend.entered.notified())
        .await
        .unwrap();
    let second = GcFenceLease::acquire_writer(&store, "repo", Duration::from_secs(30))
        .await
        .unwrap();
    let path = Path::from(gc_fence_path("repo").unwrap());
    let before = backend
        .inner
        .get(&path)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let admitted = deserialize_state(path.as_ref(), &before).unwrap();
    backend.resume.notify_one();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let after = backend
        .inner
        .get(&path)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let renewed = deserialize_state(path.as_ref(), &after).unwrap();
    renewed.validate(path.as_ref()).unwrap();
    first.release().await.unwrap();
    second.release().await.unwrap();
    assert_eq!(
        (renewed.writers.len(), renewed.epoch, renewed.writer_epoch),
        (2, admitted.epoch, admitted.writer_epoch)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn renewal_conflict_cannot_restore_lost_authority() {
    for mode in [GcFenceMode::Writer, GcFenceMode::Sweep] {
        for loss in ["removed", "expired", "recreated", "sweep epoch"] {
            if loss == "sweep epoch" && mode == GcFenceMode::Writer {
                continue;
            }
            let backend = Arc::new(PausedUpdateStore::default());
            let store: Arc<dyn ObjectStore> = backend.clone();
            let lease = GcFenceLease::acquire(&store, "repo", mode, Duration::from_secs(30))
                .await
                .unwrap();
            backend.pause.store(true, Ordering::SeqCst);
            let task = tokio::spawn(async move { lease.renew().await });
            tokio::time::timeout(Duration::from_secs(5), backend.entered.notified())
                .await
                .unwrap();
            let path = Path::from(gc_fence_path("repo").unwrap());
            let body = backend
                .inner
                .get(&path)
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
            let mut state = deserialize_state(path.as_ref(), &body).unwrap();
            match loss {
                "removed" => {
                    state.writers.clear();
                    state.sweep = None;
                }
                "expired" => match mode {
                    GcFenceMode::Writer => state.writers[0].expires_at_backend = 1,
                    GcFenceMode::Sweep => state.sweep.as_mut().unwrap().expires_at_backend = 1,
                },
                "recreated" => state.incarnation = uuid::Uuid::now_v7().to_string(),
                "sweep epoch" => state.epoch += 1,
                _ => unreachable!(),
            }
            let expected = serialize_state(path.as_ref(), &state).unwrap();
            backend
                .inner
                .put(&path, expected.clone().into())
                .await
                .unwrap();
            backend.resume.notify_one();
            let result = tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap();
            let after = backend
                .inner
                .get(&path)
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
            assert!(
                matches!(result, Err(CoordinationError::GcFenceLost { .. })) && after == expected,
                "mode={mode:?}, loss={loss}, result={result:?}"
            );
        }
    }
}
