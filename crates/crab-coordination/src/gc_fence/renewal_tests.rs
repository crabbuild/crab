use super::*;
use object_store::ObjectStoreExt;
use object_store::memory::InMemory;

#[derive(Debug, Default)]
struct PausedUpdateStore {
    inner: InMemory,
    pause: AtomicBool,
    lose_response: AtomicBool,
    pause_get_path: Mutex<Option<String>>,
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
        let lose = matches!(options.mode, object_store::PutMode::Update(_))
            && self.lose_response.swap(false, Ordering::SeqCst);
        let result = self.inner.put_opts(path, payload, options).await?;
        if lose {
            return Err(object_store::Error::Precondition {
                path: path.to_string(),
                source: "committed response lost".into(),
            });
        }
        Ok(result)
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
        let pause = {
            let mut target = self.pause_get_path.lock().await;
            if target.as_deref() == Some(path.as_ref()) {
                target.take();
                true
            } else {
                false
            }
        };
        if pause {
            self.entered.notify_one();
            self.resume.notified().await;
        }
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

const TTL: Duration = Duration::from_secs(30);

#[tokio::test(flavor = "multi_thread")]
async fn renewal_is_independent_of_another_writer_admission() {
    let backend = Arc::new(PausedUpdateStore::default());
    let store: Arc<dyn ObjectStore> = backend.clone();
    let first = GcFenceLease::acquire_writer(&store, "repo", TTL)
        .await
        .unwrap();
    let path = gc_fence_path("repo").unwrap();
    let before = get_with_version(&store, &Path::from(path.as_str()))
        .await
        .unwrap();
    backend.pause.store(true, Ordering::SeqCst);
    let renewing = first.clone();
    let task = tokio::spawn(async move { renewing.renew().await });
    tokio::time::timeout(Duration::from_secs(5), backend.entered.notified())
        .await
        .unwrap();
    let second = GcFenceLease::acquire_writer(&store, "repo", TTL)
        .await
        .unwrap();
    backend.resume.notify_one();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    first.release().await.unwrap();
    second.release().await.unwrap();
    assert_eq!(
        get_with_version(&store, &Path::from(path.as_str()))
            .await
            .unwrap(),
        before
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_writer_cannot_enter_after_sweep_closes_and_scans_gate() {
    let backend = Arc::new(PausedUpdateStore::default());
    let store: Arc<dyn ObjectStore> = backend.clone();
    ready_control(&store, &gc_fence_path("repo").unwrap())
        .await
        .unwrap();
    backend.pause.store(true, Ordering::SeqCst);
    let writing_store = store.clone();
    let task =
        tokio::spawn(
            async move { GcFenceLease::acquire_writer(&writing_store, "repo", TTL).await },
        );
    tokio::time::timeout(Duration::from_secs(5), backend.entered.notified())
        .await
        .unwrap();
    let sweep = GcFenceLease::acquire_sweep(&store, "repo", TTL)
        .await
        .unwrap();
    assert_eq!(sweep.writer_epoch().unwrap().epoch, 0);
    backend.resume.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result, Err(CoordinationError::CasConflict { .. })));
    sweep.renew().await.unwrap();
    sweep.release().await.unwrap();
    let sweep = GcFenceLease::acquire_sweep(&store, "repo", TTL)
        .await
        .unwrap();
    assert_eq!(sweep.writer_epoch().unwrap().epoch, 2);
    sweep.release().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn delayed_renewal_cannot_restore_lost_authority() {
    for mode in [GcFenceMode::Writer, GcFenceMode::Sweep] {
        for loss in ["removed", "expired", "recreated", "sweep epoch"] {
            if loss == "sweep epoch" && mode == GcFenceMode::Writer {
                continue;
            }
            let backend = Arc::new(PausedUpdateStore::default());
            let store: Arc<dyn ObjectStore> = backend.clone();
            let lease = match mode {
                GcFenceMode::Writer => GcFenceLease::acquire_writer(&store, "repo", TTL)
                    .await
                    .unwrap(),
                GcFenceMode::Sweep => GcFenceLease::acquire_sweep(&store, "repo", TTL)
                    .await
                    .unwrap(),
            };
            backend.pause.store(true, Ordering::SeqCst);
            let renewing = lease.clone();
            let task = tokio::spawn(async move { renewing.renew().await });
            tokio::time::timeout(Duration::from_secs(5), backend.entered.notified())
                .await
                .unwrap();
            let path = gc_fence_path("repo").unwrap();
            let (mut control, _) = load_control(&store, &path).await.unwrap();
            if mode == GcFenceMode::Writer && loss != "recreated" {
                let Claim::Writer { path, .. } = &lease.inner.claim else {
                    unreachable!();
                };
                let (mut slot, _) = load_slot(&store, path, &control).await.unwrap();
                if loss == "removed" {
                    slot.writer = None;
                    slot.epoch += 1;
                } else {
                    slot.writer.as_mut().unwrap().expires_at_backend = 1;
                }
                store
                    .put(
                        &Path::from(path.as_str()),
                        encode(path, &slot).unwrap().into(),
                    )
                    .await
                    .unwrap();
            } else {
                match loss {
                    "removed" => {
                        control.sweep = None;
                        control.acquisition = None;
                    }
                    "expired" => control.sweep.as_mut().unwrap().expires_at_backend = 1,
                    "recreated" => control.incarnation = uuid::Uuid::now_v7().to_string(),
                    "sweep epoch" => control.epoch += 1,
                    _ => unreachable!(),
                }
                store
                    .put(
                        &Path::from(path.as_str()),
                        encode(&path, &control).unwrap().into(),
                    )
                    .await
                    .unwrap();
            }
            backend.resume.notify_one();
            let result = tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap();
            assert!(
                matches!(result, Err(CoordinationError::GcFenceLost { .. })),
                "mode={mode:?}, loss={loss}, result={result:?}"
            );
        }
    }
}

#[tokio::test]
async fn lost_committed_responses_preserve_one_claim_and_complete_history() {
    for sweep in [false, true] {
        let backend = Arc::new(PausedUpdateStore::default());
        let store: Arc<dyn ObjectStore> = backend.clone();
        ready_control(&store, &gc_fence_path("repo").unwrap())
            .await
            .unwrap();
        backend.lose_response.store(true, Ordering::SeqCst);
        let lease = if sweep {
            GcFenceLease::acquire_sweep(&store, "repo", TTL)
                .await
                .unwrap()
        } else {
            GcFenceLease::acquire_writer(&store, "repo", TTL)
                .await
                .unwrap()
        };
        assert_eq!(lease.epoch(), 1);
        backend.lose_response.store(true, Ordering::SeqCst);
        lease.renew().await.unwrap();
        assert_eq!(lease.epoch(), 1);
        backend.lose_response.store(true, Ordering::SeqCst);
        lease.release().await.unwrap();
        let check = GcFenceLease::acquire_sweep(&store, "repo", TTL)
            .await
            .unwrap();
        assert_eq!(
            check.writer_epoch().unwrap().epoch,
            if sweep { 0 } else { 2 }
        );
        check.release().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn admitted_writer_renews_while_sweep_gate_is_closed() {
    let backend = Arc::new(PausedUpdateStore::default());
    let store: Arc<dyn ObjectStore> = backend.clone();
    let writer = GcFenceLease::acquire_writer(&store, "repo", TTL)
        .await
        .unwrap();
    let Claim::Writer { path, .. } = &writer.inner.claim else {
        unreachable!();
    };
    *backend.pause_get_path.lock().await = Some(path.clone());
    let sweeping_store = store.clone();
    let task =
        tokio::spawn(
            async move { GcFenceLease::acquire_sweep(&sweeping_store, "repo", TTL).await },
        );
    tokio::time::timeout(Duration::from_secs(5), backend.entered.notified())
        .await
        .unwrap();
    writer.renew().await.unwrap();
    backend.resume.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        result,
        Err(CoordinationError::GcFenceHeld { sweep: false, .. })
    ));
    writer.release().await.unwrap();
    GcFenceLease::acquire_sweep(&store, "repo", TTL)
        .await
        .unwrap()
        .release()
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_sweep_scan_releases_its_closed_gate() {
    let backend = Arc::new(PausedUpdateStore::default());
    let store: Arc<dyn ObjectStore> = backend.clone();
    let path = gc_fence_path("repo").unwrap();
    let (control, _) = ready_control(&store, &path).await.unwrap();
    *backend.pause_get_path.lock().await = Some(slot_path(&path, &control.incarnation, 0));
    let sweeping_store = store.clone();
    let task =
        tokio::spawn(
            async move { GcFenceLease::acquire_sweep(&sweeping_store, "repo", TTL).await },
        );
    tokio::time::timeout(Duration::from_secs(5), backend.entered.notified())
        .await
        .unwrap();
    assert!(load_control(&store, &path).await.unwrap().0.sweep.is_some());
    task.abort();
    assert!(matches!(task.await, Err(error) if error.is_cancelled()));
    tokio::time::timeout(Duration::from_secs(5), async {
        while load_control(&store, &path).await.unwrap().0.sweep.is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let writer = GcFenceLease::acquire_writer(&store, "repo", TTL)
        .await
        .unwrap();
    writer.release().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn migration_conflict_requires_new_inspection_and_preserves_racing_history() {
    let backend = Arc::new(PausedUpdateStore::default());
    let store: Arc<dyn ObjectStore> = backend.clone();
    let path = gc_fence_path("repo").unwrap();
    let mut old = serde_json::json!({"schema_version":2,"incarnation":uuid::Uuid::now_v7().to_string(),"epoch":12,"writer_epoch":8,"writers":[],"sweep":null,"quarantine":[],"quarantine_block_until_backend":null});
    store
        .put(
            &Path::from(path.as_str()),
            encode(&path, &old).unwrap().into(),
        )
        .await
        .unwrap();
    backend.pause.store(true, Ordering::SeqCst);
    let upgrading_store = store.clone();
    let task = tokio::spawn(async move { upgrade_gc_fence(&upgrading_store, "repo", true).await });
    tokio::time::timeout(Duration::from_secs(5), backend.entered.notified())
        .await
        .unwrap();
    old["epoch"] = 14.into();
    old["writer_epoch"] = 10.into();
    let changed = encode(&path, &old).unwrap();
    store
        .put(&Path::from(path.as_str()), changed.clone().into())
        .await
        .unwrap();
    backend.resume.notify_one();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_eq!(
        store
            .get(&Path::from(path.as_str()))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
        changed
    );
    assert!(upgrade_gc_fence(&store, "repo", true).await.unwrap());
    let sweep = GcFenceLease::acquire_sweep(&store, "repo", TTL)
        .await
        .unwrap();
    assert_eq!(sweep.writer_epoch().unwrap().epoch, 10);
    sweep.release().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn resumable_context_cannot_adopt_another_live_acquisition() {
    let backend = Arc::new(PausedUpdateStore::default());
    let store: Arc<dyn ObjectStore> = backend.clone();
    ready_control(&store, &gc_fence_path("repo").unwrap())
        .await
        .unwrap();
    backend.pause.store(true, Ordering::SeqCst);
    let first_store = store.clone();
    let task = tokio::spawn(async move {
        GcFenceLease::acquire_resumable_sweep(&first_store, "repo", "gc-run-shared", TTL).await
    });
    tokio::time::timeout(Duration::from_secs(5), backend.entered.notified())
        .await
        .unwrap();
    let second = GcFenceLease::acquire_resumable_sweep(&store, "repo", "gc-run-shared", TTL)
        .await
        .unwrap();
    backend.resume.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        result,
        Err(CoordinationError::GcFenceHeld { sweep: true, .. })
    ));
    second.renew().await.unwrap();
    second.release().await.unwrap();
}
