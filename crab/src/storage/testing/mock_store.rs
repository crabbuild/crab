//! Failure-injecting wrapper over [`object_store::memory::InMemory`].
//!
//! The goal of this module is narrow: let retry/backoff tests script
//! deterministic failure sequences against a backend that otherwise
//! obeys real `ObjectStore` semantics (conditional PUT, Range GET,
//! HEAD, `ETag`) without spinning up a live S3. We intentionally do
//! *not* reimplement those semantics — `InMemory` already gets them
//! right — we only intercept calls to inject latency or return
//! classified errors.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult, UploadPart,
};
use tokio::sync::Mutex;

/// What the next intercepted operation should do.
///
/// The wrapper pops one `FailSpec` off its queue per call. Absent a
/// spec, the call falls through to the real backend untouched.
#[derive(Debug, Clone)]
pub enum FailSpec {
    /// Return a `Generic` error (classified as transient by the retry
    /// layer). Useful for exercising the happy retry path.
    Generic,
    /// Return a transient `Generic` error from the next multipart part
    /// upload, without failing multipart creation.
    MultipartPartGeneric,
    /// Sleep for `Duration` then proceed to the real backend. Lets
    /// tests verify that `Retry-After`-style waits are honored.
    LatencyOnly(Duration),
    /// Announce entry and pause until released or the operation future is dropped.
    Pause {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    },
    /// Return a `NotFound` error.
    NotFound,
    /// Return a `Precondition` error (classified as a CAS conflict).
    Precondition,
}

/// Test double wrapping [`InMemory`] with a failure-injection layer.
///
/// Use [`inject_failure`](Self::inject_failure) to queue one or more
/// `FailSpec` entries; each subsequent `ObjectStore` call consumes one
/// entry and either fails in the scripted way or sleeps then proceeds
/// to the backend. Calls made after the queue drains behave exactly
/// like `InMemory`.
///
/// Safe to clone: the inner store and the failure queue are both
/// shared via `Arc`.
#[derive(Clone)]
pub struct MockStore {
    inner: Arc<InMemory>,
    next_failures: Arc<Mutex<VecDeque<FailSpec>>>,
}

impl MockStore {
    /// Builds a fresh `MockStore` with an empty failure queue.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(InMemory::new()),
            next_failures: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Appends `spec` to the failure queue.
    ///
    /// The next intercepted call pops this entry and either returns
    /// the scripted error or inserts latency before proceeding.
    pub async fn inject_failure(&self, spec: FailSpec) {
        self.next_failures.lock().await.push_back(spec);
    }

    /// Drops every queued failure.
    pub async fn clear_failures(&self) {
        self.next_failures.lock().await.clear();
    }

    async fn check_injection(&self) -> object_store::Result<()> {
        let mut failures = self.next_failures.lock().await;
        let spec = match failures.front() {
            Some(FailSpec::MultipartPartGeneric) => None,
            _ => failures.pop_front(),
        };
        drop(failures);
        apply_spec(spec).await
    }
}

/// Apply a single queued `FailSpec`, if any. Shared by `MockStore`'s
/// method-level injection and `FailingMultipartUpload`'s part-level
/// injection so both paths script the same failure shapes.
async fn apply_spec(spec: Option<FailSpec>) -> object_store::Result<()> {
    match spec {
        None => Ok(()),
        Some(FailSpec::Generic | FailSpec::MultipartPartGeneric) => {
            Err(object_store::Error::Generic {
                store: "MockStore",
                source: boxed("injected generic"),
            })
        }
        Some(FailSpec::NotFound) => Err(object_store::Error::NotFound {
            path: "<injected>".into(),
            source: boxed("injected not found"),
        }),
        Some(FailSpec::Precondition) => Err(object_store::Error::Precondition {
            path: "<injected>".into(),
            source: boxed("injected precondition"),
        }),
        Some(FailSpec::LatencyOnly(d)) => {
            tokio::time::sleep(d).await;
            Ok(())
        }
        Some(FailSpec::Pause { entered, release }) => {
            entered.notify_one();
            release.notified().await;
            Ok(())
        }
    }
}

/// Wraps a real `MultipartUpload` so part PUTs consult the same failure
/// queue as the outer `MockStore`. This lets retry tests script
/// transient failures on individual parts — the exact failure mode that
/// `Store::put_multipart_retry` must recover from by retrying the whole
/// upload.
struct FailingMultipartUpload {
    inner: Box<dyn MultipartUpload>,
    failures: Arc<Mutex<VecDeque<FailSpec>>>,
}

impl std::fmt::Debug for FailingMultipartUpload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FailingMultipartUpload").finish()
    }
}

#[async_trait]
impl MultipartUpload for FailingMultipartUpload {
    fn put_part(&mut self, data: PutPayload) -> UploadPart {
        let inner = self.inner.put_part(data);
        let failures = Arc::clone(&self.failures);
        Box::pin(async move {
            let spec = failures.lock().await.pop_front();
            apply_spec(spec).await?;
            inner.await
        })
    }

    async fn complete(&mut self) -> object_store::Result<PutResult> {
        self.inner.complete().await
    }

    async fn abort(&mut self) -> object_store::Result<()> {
        self.inner.abort().await
    }
}

impl Default for MockStore {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for MockStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MockStore").finish()
    }
}

impl fmt::Display for MockStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MockStore")
    }
}

fn boxed(msg: &'static str) -> Box<dyn std::error::Error + Send + Sync + 'static> {
    Box::<dyn std::error::Error + Send + Sync>::from(msg)
}

#[async_trait]
impl ObjectStore for MockStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.check_injection().await?;
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.check_injection().await?;
        let inner = self.inner.put_multipart_opts(location, opts).await?;
        Ok(Box::new(FailingMultipartUpload {
            inner,
            failures: Arc::clone(&self.next_failures),
        }))
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        self.check_injection().await?;
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        let store = self.clone();
        Box::pin(locations.then(move |location| {
            let store = store.clone();
            async move {
                let location = location?;
                store.check_injection().await?;
                store.inner.delete(&location).await?;
                Ok(location)
            }
        }))
    }

    // `list` returns a stream synchronously; there is no natural
    // pre-check point without changing the trait signature. The
    // retry-facing methods above are the ones tests care about.
    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        self.check_injection().await?;
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.check_injection().await?;
        self.inner.copy_opts(from, to, options).await
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;
    use crate::core::error::CrabError;
    use bytes::Bytes;
    use crab_storage::map_object_store_error;
    use object_store::{PutMode, PutOptions};

    #[tokio::test]
    async fn put_and_get_round_trip_through_wrapper() {
        let store = MockStore::new();
        let path = Path::from("blobs/hello");

        store
            .put(&path, PutPayload::from_static(b"hi"))
            .await
            .unwrap();
        let body = store.get(&path).await.unwrap().bytes().await.unwrap();
        assert_eq!(body.as_ref(), b"hi");
    }

    #[tokio::test]
    async fn injected_generic_fails_once_then_succeeds() {
        let store = MockStore::new();
        let path = Path::from("blobs/retry");
        store.inject_failure(FailSpec::Generic).await;

        let first = store.put(&path, PutPayload::from_static(b"x")).await;
        assert!(
            matches!(first, Err(object_store::Error::Generic { .. })),
            "first call should fail with injected Generic, got {first:?}"
        );

        // Queue drained; next call proceeds normally.
        store
            .put(&path, PutPayload::from_static(b"x"))
            .await
            .unwrap();
        let body = store.get(&path).await.unwrap().bytes().await.unwrap();
        assert_eq!(body.as_ref(), b"x");
    }

    #[tokio::test]
    async fn injected_precondition_maps_to_cas_conflict() {
        let store = MockStore::new();
        let path = Path::from("refs/heads/main");
        store.inject_failure(FailSpec::Precondition).await;

        let err = store
            .put_opts(
                &path,
                PutPayload::from_static(b"v1"),
                PutOptions::from(PutMode::Create),
            )
            .await
            .expect_err("injected precondition must fail");

        let mapped = CrabError::from(map_object_store_error(err, path.as_ref()));
        assert!(
            matches!(mapped, CrabError::CasConflict { .. }),
            "expected CasConflict, got {mapped:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn latency_only_delays_the_next_call() {
        let store = MockStore::new();
        let path = Path::from("blobs/slow");
        let want = Duration::from_millis(500);
        store.inject_failure(FailSpec::LatencyOnly(want)).await;

        // `tokio::time::Instant` tracks the paused virtual clock so
        // the assertion sees the `sleep` jump even when real time
        // hasn't moved.
        let start = tokio::time::Instant::now();
        store
            .put(&path, PutPayload::from_static(b"z"))
            .await
            .unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed >= want,
            "expected at least {want:?} elapsed, got {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn conditional_create_rejects_duplicate_content() {
        // The wrapper delegates to `InMemory`, so conditional PUT
        // semantics come for free; this check guards against
        // accidentally routing around the backend.
        let store = MockStore::new();
        let path = Path::from("blobs/cas");
        let opts = PutOptions::from(PutMode::Create);

        store
            .put_opts(&path, PutPayload::from_static(b"first"), opts.clone())
            .await
            .unwrap();

        let err = store
            .put_opts(&path, PutPayload::from_static(b"second"), opts)
            .await
            .expect_err("second Create on same path must fail");
        assert!(
            matches!(err, object_store::Error::AlreadyExists { .. }),
            "expected AlreadyExists, got {err:?}"
        );
    }

    #[tokio::test]
    async fn range_get_returns_slice() {
        let store = MockStore::new();
        let path = Path::from("blobs/range");
        store
            .put(&path, PutPayload::from_static(b"0123456789"))
            .await
            .unwrap();

        let slice = store.get_range(&path, 2..7).await.unwrap();
        assert_eq!(slice.as_ref(), b"23456");
    }

    #[tokio::test]
    async fn head_returns_size_for_existing_object() {
        let store = MockStore::new();
        let path = Path::from("blobs/head");
        let body: &[u8] = b"metadata please";
        store
            .put(&path, PutPayload::from_iter([Bytes::copy_from_slice(body)]))
            .await
            .unwrap();

        let meta = store.head(&path).await.unwrap();
        assert_eq!(meta.location, path);
        assert_eq!(meta.size, body.len() as u64);
    }

    #[tokio::test]
    async fn clear_failures_drops_the_queue() {
        let store = MockStore::new();
        store.inject_failure(FailSpec::Generic).await;
        store.inject_failure(FailSpec::NotFound).await;
        store.clear_failures().await;

        let path = Path::from("blobs/cleared");
        store
            .put(&path, PutPayload::from_static(b"ok"))
            .await
            .unwrap();
    }
}
