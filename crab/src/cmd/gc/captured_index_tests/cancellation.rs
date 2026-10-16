use super::*;
use object_store::ObjectStoreExt;

#[derive(Debug)]
pub(super) struct DeleteGate {
    blocked: Path,
    trigger: Path,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    finished: tokio::sync::Notify,
    cancel: CancellationToken,
}

impl DeleteGate {
    pub(super) async fn delete(
        self: Arc<Self>,
        inner: Arc<dyn ObjectStore>,
        path: Path,
    ) -> object_store::Result<Path> {
        if path == self.blocked {
            let (reply, complete) = tokio::sync::oneshot::channel();
            let gate = self.clone();
            // A sent provider request can commit after its client future drops.
            // Keep that side effect independent from the response waiter.
            tokio::spawn(async move {
                gate.release.notified().await;
                let result = inner.delete(&path).await.map(|()| path);
                let _ = reply.send(result);
                gate.finished.notify_one();
            });
            self.started.notify_one();
            return complete.await.unwrap();
        }
        if path == self.trigger {
            self.started.notified().await;
            self.cancel.cancel();
        }
        inner.delete(&path).await?;
        Ok(path)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capsule_gc_drains_admitted_deletes_before_releasing_its_fence() {
    assert_gc_drains(Arc::new(InMemory::new()), "org/cancel-gc").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires CRAB_E2E_FLAT_BUCKET and explicit scratch S3 credentials"]
async fn live_capsule_gc_drains_admitted_deletes_before_releasing_its_fence() {
    let bucket = std::env::var("CRAB_E2E_FLAT_BUCKET").unwrap();
    let store =
        crab_storage::build_static_env_store(&bucket, crab_storage::StorageProviderKind::S3)
            .unwrap();
    let prefix = format!("gc-drain/{}", uuid::Uuid::now_v7());
    assert_gc_drains(store.inner().clone(), &prefix).await;
}

async fn assert_gc_drains(inner: Arc<dyn ObjectStore>, prefix: &str) {
    let blocked = Path::from(format!("{prefix}/v2/capsules/{}", "1".repeat(64)));
    let trigger = Path::from(format!("{prefix}/v2/capsules/{}", "2".repeat(64)));
    let cancel = CancellationToken::new();
    let gate = Arc::new(DeleteGate {
        blocked: blocked.clone(),
        trigger: trigger.clone(),
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        finished: tokio::sync::Notify::new(),
        cancel: cancel.clone(),
    });
    let backend = Arc::new(AgedObjects {
        inner,
        deletes: Some(gate.clone()),
        paths: Mutex::new(HashSet::from([blocked.clone(), trigger.clone()])),
        modified: SystemTime::now() - Duration::from_secs(7200),
        list_calls: Mutex::new(Vec::new()),
    });
    let store = Store::new(backend);
    let router = crate::storage::StoreLayout::new(store.clone(), prefix.to_owned());
    let layout = StoreLayout::new(store.as_storage().clone(), prefix.to_owned());
    crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
        .await
        .unwrap();
    for path in [blocked, trigger] {
        store
            .put(&path, Bytes::from_static(b"orphan"))
            .await
            .unwrap();
    }
    let args = GcArgs {
        delete_concurrency: 2,
        ..GcArgs::default()
    };
    let gc_store = store.clone();
    let gc_cancel = cancel.clone();
    let protected = HashSet::new();
    let grace = Duration::from_secs(3600);
    let mut sweep = std::pin::pin!(super::super::run_repo_remote_gc(
        &args, &gc_store, &router, &protected, &gc_cancel, grace, None,
    ));
    let mut completed = None;
    tokio::select! {
        cancelled = tokio::time::timeout(Duration::from_secs(5), cancel.cancelled()) => {
            cancelled.unwrap();
        }
        result = &mut sweep => completed = Some(result),
    }
    assert!(
        cancel.is_cancelled(),
        "GC never reached the delete gate: {completed:?}"
    );
    if completed.is_none()
        && let Ok(result) = tokio::time::timeout(Duration::from_millis(100), &mut sweep).await
    {
        completed = Some(result);
    }
    let returned_early = completed.is_some();
    let writer = crab_coordination::GcFenceLease::acquire_writer(
        store.inner(),
        prefix,
        Duration::from_secs(60),
    )
    .await;
    let fenced = writer.is_err();
    if let Ok(writer) = writer {
        writer.release().await.unwrap();
    }
    gate.release.notify_one();
    let result = match completed {
        Some(result) => result,
        None => sweep.await,
    };
    tokio::time::timeout(Duration::from_secs(5), gate.finished.notified())
        .await
        .unwrap();
    assert!(
        !returned_early && fenced,
        "GC released admission while an accepted provider delete was unresolved"
    );
    assert!(matches!(
        result,
        Err(crate::core::error::CrabError::Cancelled)
    ));
    let writer = crab_coordination::GcFenceLease::acquire_writer(
        store.inner(),
        prefix,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    writer.release().await.unwrap();
}
