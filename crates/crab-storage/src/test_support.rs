//! Test-only object-store instrumentation shared by read-path integration tests.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use object_store::path::Path;
use object_store::{
    GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};

/// Kind of object read observed by [`CountingObjectStore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectReadKind {
    /// Metadata-only request.
    Head,
    /// Request carrying an explicit byte range.
    Range,
    /// Request for the complete object body.
    Full,
}

/// One observed object read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectReadRequest {
    /// Object-store path passed to the wrapped store.
    pub location: String,
    /// Read shape requested by the caller.
    pub kind: ObjectReadKind,
}

/// Point-in-time aggregate read counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ObjectReadCounts {
    /// Metadata-only requests.
    pub heads: usize,
    /// Explicit range requests.
    pub ranges: usize,
    /// Complete-body requests.
    pub full: usize,
}

impl ObjectReadCounts {
    /// Return the number of requests that can transfer object bytes.
    #[must_use]
    pub const fn body_requests(self) -> usize {
        self.ranges + self.full
    }
}

/// Object-store decorator that classifies reads and can reject body requests.
///
/// This type is compiled only for tests or consumers enabling the
/// `test-support` feature. Rejected requests are counted and logged before the
/// deterministic test error is returned.
#[derive(Debug)]
pub struct CountingObjectStore {
    inner: Arc<dyn ObjectStore>,
    heads: AtomicUsize,
    ranges: AtomicUsize,
    full: AtomicUsize,
    body_reads_enabled: AtomicBool,
    blocked_body_paths: std::sync::Mutex<HashSet<String>>,
    requests: std::sync::Mutex<Vec<ObjectReadRequest>>,
}

impl CountingObjectStore {
    /// Wrap an object store with read instrumentation.
    #[must_use]
    pub fn new(inner: Arc<dyn ObjectStore>) -> Self {
        Self {
            inner,
            heads: AtomicUsize::new(0),
            ranges: AtomicUsize::new(0),
            full: AtomicUsize::new(0),
            body_reads_enabled: AtomicBool::new(true),
            blocked_body_paths: std::sync::Mutex::new(HashSet::new()),
            requests: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Enable or disable range and full-body reads while continuing to allow HEAD.
    pub fn set_body_reads_enabled(&self, enabled: bool) {
        self.body_reads_enabled.store(enabled, Ordering::Release);
    }

    /// Reject range and full-body reads for one exact object path.
    pub fn block_body_reads_for(&self, location: &Path) {
        let location = location.to_string();
        match self.blocked_body_paths.lock() {
            Ok(mut paths) => {
                paths.insert(location);
            }
            Err(poisoned) => {
                poisoned.into_inner().insert(location);
            }
        }
    }

    /// Allow body reads for one path that was previously blocked.
    pub fn unblock_body_reads_for(&self, location: &Path) {
        let location = location.to_string();
        match self.blocked_body_paths.lock() {
            Ok(mut paths) => {
                paths.remove(&location);
            }
            Err(poisoned) => {
                poisoned.into_inner().remove(&location);
            }
        }
    }

    /// Return aggregate counters without resetting them.
    #[must_use]
    pub fn counts(&self) -> ObjectReadCounts {
        ObjectReadCounts {
            heads: self.heads.load(Ordering::Acquire),
            ranges: self.ranges.load(Ordering::Acquire),
            full: self.full.load(Ordering::Acquire),
        }
    }

    /// Return all observed requests in call order.
    #[must_use]
    pub fn requests(&self) -> Vec<ObjectReadRequest> {
        self.requests
            .lock()
            .map_or_else(|poisoned| poisoned.into_inner().clone(), |log| log.clone())
    }

    /// Clear counters and the request log.
    pub fn reset(&self) {
        self.heads.store(0, Ordering::Release);
        self.ranges.store(0, Ordering::Release);
        self.full.store(0, Ordering::Release);
        self.requests.lock().map_or_else(
            |poisoned| poisoned.into_inner().clear(),
            |mut log| log.clear(),
        );
    }

    fn record(&self, location: &Path, options: &GetOptions) -> ObjectReadKind {
        let kind = if options.head {
            self.heads.fetch_add(1, Ordering::AcqRel);
            ObjectReadKind::Head
        } else if options.range.is_some() {
            self.ranges.fetch_add(1, Ordering::AcqRel);
            ObjectReadKind::Range
        } else {
            self.full.fetch_add(1, Ordering::AcqRel);
            ObjectReadKind::Full
        };
        let request = ObjectReadRequest {
            location: location.to_string(),
            kind,
        };
        match self.requests.lock() {
            Ok(mut log) => log.push(request),
            Err(poisoned) => poisoned.into_inner().push(request),
        }
        kind
    }

    fn body_read_is_blocked(&self, location: &Path) -> bool {
        let location = location.to_string();
        self.blocked_body_paths.lock().map_or_else(
            |poisoned| poisoned.into_inner().contains(&location),
            |paths| paths.contains(&location),
        )
    }
}

impl fmt::Display for CountingObjectStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CountingObjectStore")
    }
}

#[async_trait]
impl ObjectStore for CountingObjectStore {
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
        let kind = self.record(location, &options);
        if kind != ObjectReadKind::Head
            && (!self.body_reads_enabled.load(Ordering::Acquire)
                || self.body_read_is_blocked(location))
        {
            return Err(object_store::Error::Generic {
                store: "CountingObjectStore",
                source: Box::new(std::io::Error::other("object body reads disabled by test")),
            });
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
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use object_store::memory::InMemory;
    use object_store::path::Path;
    use object_store::{GetOptions, GetRange, ObjectStore, ObjectStoreExt};

    use super::{CountingObjectStore, ObjectReadCounts, ObjectReadKind};

    #[tokio::test]
    async fn classifies_head_range_and_full_reads_by_path() {
        let inner = Arc::new(InMemory::new());
        let path = Path::from(".crab/xorbs/ab/hash");
        inner
            .put(&path, Bytes::from_static(b"abcdef").into())
            .await
            .unwrap();
        let store = CountingObjectStore::new(inner);

        store.head(&path).await.unwrap();
        store
            .get_opts(
                &path,
                GetOptions {
                    range: Some(GetRange::Bounded(1..3)),
                    ..GetOptions::default()
                },
            )
            .await
            .unwrap();
        store.get(&path).await.unwrap();

        assert_eq!(
            store.counts(),
            ObjectReadCounts {
                heads: 1,
                ranges: 1,
                full: 1,
            }
        );
        assert_eq!(
            store
                .requests()
                .into_iter()
                .map(|request| (request.location, request.kind))
                .collect::<Vec<_>>(),
            vec![
                (path.to_string(), ObjectReadKind::Head),
                (path.to_string(), ObjectReadKind::Range),
                (path.to_string(), ObjectReadKind::Full),
            ]
        );
    }

    #[tokio::test]
    async fn disabled_body_reads_still_allow_head() {
        let inner = Arc::new(InMemory::new());
        let path = Path::from(".crab/xorbs/ab/hash");
        inner
            .put(&path, Bytes::from_static(b"abcdef").into())
            .await
            .unwrap();
        let store = CountingObjectStore::new(inner);
        store.set_body_reads_enabled(false);

        store.head(&path).await.unwrap();
        let error = store.get(&path).await.unwrap_err();

        assert!(error.to_string().contains("body reads disabled"));
        assert_eq!(store.counts().heads, 1);
        assert_eq!(store.counts().body_requests(), 1);
    }
}
