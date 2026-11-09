//! Preparation overlaps independent durability work and awaits both outcomes.
#![expect(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]

use super::*;
use futures_util::stream::BoxStream;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, memory::InMemory, path::Path,
};
use std::{fmt, future::Future, sync::Arc, task::Poll, time::Duration};
use tokio::sync::Notify;

#[derive(Debug, Default)]
struct Stage {
    started: Notify,
    release: Notify,
    settled: Notify,
}

impl Stage {
    async fn enter(&self) {
        self.started.notify_one();
        self.release.notified().await;
    }
}

#[derive(Debug)]
struct GatedStore {
    inner: Arc<dyn ObjectStore>,
    retention_path: Path,
    leaf_path: Path,
    retention: Stage,
    upload: Stage,
    fail_retention: bool,
    fail_upload: bool,
}

impl fmt::Display for GatedStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("GatedStore")
    }
}

#[async_trait::async_trait]
impl ObjectStore for GatedStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        if location != &self.retention_path {
            return self.inner.put_opts(location, payload, options).await;
        }
        self.retention.enter().await;
        let result = if self.fail_retention {
            Err(object_store::Error::PermissionDenied {
                path: location.to_string(),
                source: Box::new(std::io::Error::other("retention denied")),
            })
        } else {
            self.inner.put_opts(location, payload, options).await
        };
        self.retention.settled.notify_one();
        result
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        if location != &self.leaf_path {
            return self.inner.get_opts(location, options).await;
        }
        self.upload.enter().await;
        let result = if self.fail_upload {
            Err(object_store::Error::Generic {
                store: "durability-test",
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "immutable verification failed",
                )),
            })
        } else {
            self.inner.get_opts(location, options).await
        };
        self.upload.settled.notify_one();
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

async fn failed_preparation(fail_retention: bool, fail_upload: bool) {
    let inner = Arc::new(InMemory::new());
    let seed = StoreLayout::new(
        Store::new(inner.clone()),
        "repositories/durability".to_owned(),
    );
    let mut base = initialize(&seed, &"1".repeat(64), "refs/heads/main")
        .await
        .unwrap();
    let mut previous = None;
    for sequence in 1..=3 {
        let next = format!("{sequence:040x}");
        let edit = super::tests::transaction(&base, previous.as_deref(), &next);
        base = publish(&seed, base, &edit, super::tests::capsule(&edit))
            .await
            .unwrap();
        previous = Some(next);
    }
    let edit = super::tests::transaction(&base, previous.as_deref(), &format!("{:040x}", 4));
    let capsule = super::tests::capsule(&edit);
    let leaf = capsule_leaf_run(capsule.clone()).unwrap();
    let fault = Arc::new(GatedStore {
        inner,
        retention_path: seed.capsule_source_head_path(&blake3::hash(b"refs/heads/main").to_hex()),
        leaf_path: seed.capsule_path(leaf.hash()),
        retention: Stage::default(),
        upload: Stage::default(),
        fail_retention,
        fail_upload,
    });
    let store = Store::with_retry(
        fault.clone(),
        crab_storage::RetryPolicy {
            max_attempts: 1,
            base: Duration::ZERO,
            cap: Duration::ZERO,
        },
    );
    let router = StoreLayout::new(store, seed.repo_prefix().to_owned());
    let preparation = prepare_publication(&router, base.clone(), &edit, capsule);
    fn assert_send(_: &impl Send) {}
    assert_send(&preparation);
    tokio::pin!(preparation);
    tokio::select! {
        _ = async {
            tokio::join!(fault.retention.started.notified(), fault.upload.started.notified());
        } => {}
        result = &mut preparation => panic!("preparation returned before both stages started: {result:?}"),
    }
    let (first, second) = if fail_retention {
        (&fault.retention, &fault.upload)
    } else {
        (&fault.upload, &fault.retention)
    };
    first.release.notify_one();
    tokio::select! {
        biased;
        _ = first.settled.notified() => {}
        result = &mut preparation => panic!("preparation returned while its peer was held: {result:?}"),
    }
    std::future::poll_fn(|context| {
        assert!(preparation.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
    second.release.notify_one();
    let error = preparation
        .await
        .expect_err("a failed durability stage prevents preparation");
    second.settled.notified().await;
    if fail_retention {
        assert!(
            matches!(error, WriteError::Metadata(crab_metadata::error::MetadataError::Storage {
                source: StorageError::Forbidden { path },
            }) if path == fault.retention_path.as_ref())
        );
    } else {
        assert!(matches!(
            error,
            WriteError::Storage(StorageError::NetworkTransient { .. })
        ));
    }
    let visible = read_ref_head(&seed, base.record().root(), "refs/heads/main")
        .await
        .unwrap();
    assert_eq!(visible.visible.oid(), previous.as_deref());
}

#[tokio::test(flavor = "multi_thread")]
async fn retention_failure_awaits_immutable_verification() {
    tokio::time::timeout(Duration::from_secs(10), failed_preparation(true, false))
        .await
        .expect("independent stages must overlap and settle");
}

#[tokio::test(flavor = "multi_thread")]
async fn verification_failure_awaits_source_retention() {
    tokio::time::timeout(Duration::from_secs(10), failed_preparation(false, true))
        .await
        .expect("independent stages must overlap and settle");
}

#[tokio::test(flavor = "multi_thread")]
async fn simultaneous_failures_preserve_retention_error() {
    tokio::time::timeout(Duration::from_secs(10), failed_preparation(true, true))
        .await
        .expect("independent stages must overlap and settle");
}
