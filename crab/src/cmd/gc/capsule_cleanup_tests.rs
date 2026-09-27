use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use crab_metadata::capsule_protocol::{Capsule, CapsuleRefEdit, CapsuleTransaction};
use futures_util::stream::BoxStream;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use tokio_util::sync::CancellationToken;

use super::{GcArgs, run_repo_remote_gc, sweep_capsule_objects};
use crate::core::error::CrabError;
use crate::storage::StoreLayout;
use crate::storage::store::Store;

#[tokio::test]
async fn failed_capsule_view_releases_root_and_sweep_fences() {
    let store = Store::new(Arc::new(InMemory::new()));
    let router = StoreLayout::new(store.clone(), "org/gc-cleanup".to_owned());
    let layout = crab_storage::StoreLayout::with_global_prefix(
        store.as_storage().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    let base =
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
    let transaction = CapsuleTransaction::new(
        base.record().digest(),
        vec![CapsuleRefEdit::new(
            "refs/heads/main",
            None,
            Some("2".repeat(40)),
            None,
        )],
    )
    .unwrap();
    let capsule = Capsule::build(&transaction, Vec::new(), Vec::new()).unwrap();
    crab_write::capsule_protocol::publish(&layout, base, &transaction, &capsule)
        .await
        .unwrap();
    let limits = crab_read::capsule_protocol::CapsuleReadLimits {
        max_capsule_bytes: 1024 * 1024,
        max_frontier_bytes: 8 * 1024 * 1024,
    };
    let view = crab_read::capsule_protocol::open_view(&layout, limits)
        .await
        .unwrap();
    let path = layout.capsule_path(view.capsule_run_pointers()[0].hash());
    let (body, _) = store.get_with_etag(&path).await.unwrap();
    store.delete(&path).await.unwrap();
    let result = run_repo_remote_gc(
        &GcArgs::default(),
        &store,
        &router,
        &HashSet::new(),
        &CancellationToken::new(),
        Duration::from_secs(3600),
        None,
    )
    .await;
    assert!(
        matches!(result, Err(CrabError::NotFound { path: missing }) if missing == path.as_ref())
    );
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    assert!(root.record().root().gc_fence().is_none());

    store.put(&path, body).await.unwrap();
    // Retrying without waiting for expiry proves the independent sweep lease
    // was released too; repairing only the root would leave GC unavailable.
    run_repo_remote_gc(
        &GcArgs::default(),
        &store,
        &router,
        &HashSet::new(),
        &CancellationToken::new(),
        Duration::from_secs(3600),
        None,
    )
    .await
    .unwrap();
    let recovered = crab_read::capsule_protocol::open_view(&layout, limits)
        .await
        .unwrap();
    assert_eq!(recovered.refs(), view.refs());
    assert_eq!(
        recovered.capsule_run_pointers(),
        view.capsule_run_pointers()
    );
    assert!(recovered.root().root().gc_fence().is_none());
}

#[derive(Debug)]
struct ChangeBeforeHead {
    inner: InMemory,
    replacement_path: Path,
    replaced: AtomicBool,
    change: HeadChange,
}

#[derive(Debug, Clone, Copy)]
enum HeadChange {
    Content,
    Freshness(SystemTime),
}

impl std::fmt::Display for ChangeBeforeHead {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ChangeBeforeHead")
    }
}

#[async_trait::async_trait]
impl ObjectStore for ChangeBeforeHead {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.inner.put_opts(location, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        options: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, options).await
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        if options.head
            && location == &self.replacement_path
            && !self.replaced.swap(true, Ordering::SeqCst)
        {
            match self.change {
                HeadChange::Content => {
                    self.inner
                        .put_opts(
                            location,
                            Bytes::from_static(b"newone").into(),
                            PutOptions::default(),
                        )
                        .await?;
                }
                HeadChange::Freshness(last_modified) => {
                    // S3 can retain the ETag on a same-content rewrite while
                    // Last-Modified advances, so identity alone is insufficient.
                    let mut response = self.inner.get_opts(location, options).await?;
                    response.meta.last_modified = last_modified.into();
                    return Ok(response);
                }
            }
        }
        self.inner.get_opts(location, options).await
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
async fn capsule_gc_does_not_report_revalidated_retained_objects_as_deleted() {
    for (force, refresh) in [(false, false), (true, false), (false, true), (true, true)] {
        let snapshot_at = SystemTime::now() + Duration::from_secs(2 * 3600);
        let replacement_path = Path::from(format!("org/gc-race/v2/capsules/aa/{}", "a".repeat(64)));
        let inner = Arc::new(ChangeBeforeHead {
            inner: InMemory::new(),
            replacement_path: replacement_path.clone(),
            replaced: AtomicBool::new(false),
            change: if refresh {
                HeadChange::Freshness(snapshot_at)
            } else {
                HeadChange::Content
            },
        });
        let store = Store::new(inner.clone());
        let layout =
            crab_storage::StoreLayout::new(store.as_storage().clone(), "org/gc-race".to_owned());
        let collectible_path = layout.capsule_path(&"b".repeat(64));
        for path in [&replacement_path, &collectible_path] {
            store
                .put(path, Bytes::from_static(b"orphan"))
                .await
                .unwrap();
        }
        let root = crab_metadata::capsule_protocol::RepositoryRoot::initial(
            &"1".repeat(64),
            "refs/heads/main",
        )
        .unwrap();
        let outcome = sweep_capsule_objects(
            &GcArgs {
                force,
                ..GcArgs::default()
            },
            &store,
            &layout,
            &root,
            &[],
            &HashSet::new(),
            &CancellationToken::new(),
            snapshot_at,
            Duration::from_secs(3600),
            Instant::now(),
        )
        .await
        .unwrap();
        assert!(inner.replaced.load(Ordering::SeqCst));
        let retained = store.get_with_etag(&replacement_path).await;
        assert!(
            retained.is_ok(),
            "force={force}, refresh={refresh}: {retained:?}"
        );
        assert_eq!(
            retained.unwrap().0,
            if refresh {
                &b"orphan"[..]
            } else {
                &b"newone"[..]
            }
        );
        assert!(matches!(
            store.head(&collectible_path).await,
            Err(CrabError::NotFound { .. })
        ));
        assert_eq!(
            (outcome.packs_deleted, outcome.bytes_reclaimed),
            (1, 6),
            "force={force}, refresh={refresh}"
        );
    }
}
