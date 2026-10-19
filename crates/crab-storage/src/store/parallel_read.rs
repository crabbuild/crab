use super::{StorageByteStream, StorageReadKind, Store};
use crate::{Result, StorageError, map_object_store_error, retry};
use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt, stream};
use object_store::{GetOptions, GetRange, ObjectMeta, path::Path};
use std::{ops::Range, sync::Arc};
use tokio_util::sync::CancellationToken;

pub(super) const CONCURRENCY: usize = 4;
const CHUNK_BYTES: u64 = 32 * 1024 * 1024;
// Smaller objects do not amortize the metadata round trip and per-range latency.
const PARALLEL_THRESHOLD: u64 = 512 * 1024 * 1024;

fn chunk_bytes(length: u64) -> u64 {
    if length > PARALLEL_THRESHOLD {
        CHUNK_BYTES
    } else {
        length
    }
}

impl Store {
    /// Read an immutable body within its declared size using bounded concurrent transfers.
    ///
    /// Callers still authenticate the complete hash and format. The returned
    /// body occupies its full size in memory; dropping this future cancels reads.
    pub async fn get_immutable_bounded(&self, path: &Path, max_bytes: u64) -> Result<Bytes> {
        if max_bytes <= PARALLEL_THRESHOLD {
            return self
                .get_with_etag_bounded(path, max_bytes)
                .await
                .map(|(bytes, _)| bytes);
        }
        let cancellation = CancellationToken::new();
        let (meta, _, mut stream) = self
            .get_parallel_stream(path, max_bytes, None, &cancellation)
            .await?;
        if meta.size > max_bytes {
            return Err(StorageError::CorruptObject {
                path: path.to_string(),
                reason: format!(
                    "object is {} bytes; bounded read supports at most {max_bytes} bytes",
                    meta.size
                ),
            });
        }
        let capacity = usize::try_from(meta.size).map_err(|source| StorageError::Io {
            source: std::io::Error::other(source),
        })?;
        let mut body = Vec::new();
        body.try_reserve_exact(capacity)
            .map_err(|source| StorageError::Io {
                source: std::io::Error::other(source),
            })?;
        while let Some(bytes) = stream.next().await {
            body.extend_from_slice(&bytes?);
        }
        Ok(Bytes::from(body))
    }

    /// Stream an immutable object with bounded concurrent requests for large bodies.
    ///
    /// The expected size selects transfer strategy; caller admission covers each
    /// request through body consumption. Callers must validate returned
    /// metadata and the complete content hash. Small objects retain one full GET.
    pub async fn get_parallel_stream(
        &self,
        path: &Path,
        expected_size: u64,
        admission: Option<Arc<tokio::sync::Semaphore>>,
        cancel: &CancellationToken,
    ) -> Result<(ObjectMeta, Range<u64>, StorageByteStream)> {
        self.parallel_stream(
            path,
            None,
            expected_size,
            chunk_bytes(expected_size),
            admission,
            cancel,
        )
        .await
    }

    /// Stream an immutable range in order using bounded concurrent requests.
    ///
    /// Like `get_stream`, ranges clamp at EOF; callers verify returned bounds and
    /// the complete content hash. Four active or queued range buffers share
    /// admission across store clones. The internal buffer bound is 128 MiB; dropping the stream
    /// cancels pending reads. Ranges up to 512 MiB retain one GET without an extra HEAD.
    pub async fn get_parallel_range_stream(
        &self,
        path: &Path,
        range: Range<u64>,
        admission: Option<Arc<tokio::sync::Semaphore>>,
        cancel: &CancellationToken,
    ) -> Result<(ObjectMeta, Range<u64>, StorageByteStream)> {
        let length =
            range
                .end
                .checked_sub(range.start)
                .ok_or_else(|| StorageError::CorruptObject {
                    path: path.to_string(),
                    reason: "immutable stream range is reversed".to_owned(),
                })?;
        self.parallel_stream(
            path,
            Some(range),
            length,
            chunk_bytes(length),
            admission,
            cancel,
        )
        .await
    }

    async fn parallel_stream(
        &self,
        path: &Path,
        range: Option<Range<u64>>,
        length: u64,
        chunk_bytes: u64,
        admission: Option<Arc<tokio::sync::Semaphore>>,
        cancel: &CancellationToken,
    ) -> Result<(ObjectMeta, Range<u64>, StorageByteStream)> {
        if length == 0 && range.is_some() {
            return Err(StorageError::CorruptObject {
                path: path.to_string(),
                reason: "immutable stream range is empty".to_owned(),
            });
        }
        if length <= chunk_bytes {
            let (meta, actual, body) = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(StorageError::Cancelled),
                result = async {
                    let permit = origin_permit(admission.as_ref()).await?;
                    let (meta, range, body) = self.get_stream(path, range).await?;
                    Ok::<_, StorageError>((meta, range, cancellable_stream(body, cancel, permit)))
                } => result?,
            };
            return Ok((meta, actual, body));
        }
        let meta = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(StorageError::Cancelled),
            result = async {
                let _permit = origin_permit(admission.as_ref()).await?;
                self.head(path).await
            } => result?,
        };
        let range = range
            .map(|range| GetRange::Bounded(range).as_range(meta.size))
            .transpose()
            .map_err(|source| StorageError::CorruptObject {
                path: path.to_string(),
                reason: format!("invalid immutable stream range: {source}"),
            })?
            .unwrap_or(0..meta.size);
        let length = range.end - range.start;
        let pinned = Arc::new(meta.clone());
        let store = self.clone();
        let path = path.clone();
        let read_cancel = cancel.clone();
        let ranges = (range.start..range.end)
            .step_by(chunk_bytes as usize)
            .map(move |start| start..start.saturating_add(chunk_bytes).min(range.end));
        // Reserve in source order before constructing futures. Completed buffers
        // retain capacity until delivery; earlier ranges already own a slot, so
        // a slow first range cannot deadlock behind later completions.
        let requests = stream::iter(ranges).then(move |range| {
            let store = store.clone();
            let path = path.clone();
            let pinned = pinned.clone();
            let cancel = read_cancel.clone();
            let admission = admission.clone();
            async move {
                let permit = store.parallel_reads.clone().acquire_owned().await
                    .map_err(|source| StorageError::Io {
                        source: std::io::Error::other(source),
                    })?;
                Ok::<_, StorageError>(async move {
                    let bytes = tokio::select! {
                        biased;
                        () = cancel.cancelled() => Err(StorageError::Cancelled),
                        result = store.parallel_range(&path, range, &pinned, admission.as_ref()) => result,
                    }?;
                    Ok::<_, StorageError>((bytes, permit))
                })
            }
        });
        let stream = requests
            .try_buffered(CONCURRENCY)
            .map_ok(|(bytes, _permit)| bytes)
            .boxed();
        tracing::debug!(
            range_bytes = length,
            range_count = length.div_ceil(chunk_bytes),
            concurrency = CONCURRENCY,
            "parallel immutable stream opened"
        );
        Ok((meta, range, cancellable_stream(stream, cancel, None)))
    }

    async fn parallel_range(
        &self,
        path: &Path,
        range: Range<u64>,
        pinned: &ObjectMeta,
        admission: Option<&Arc<tokio::sync::Semaphore>>,
    ) -> Result<Bytes> {
        let _origin = origin_permit(admission).await?;
        // Retry one complete range, never a stream already exposed to its caller.
        retry(&self.retry, || async {
            self.record_read_request(StorageReadKind::Stream);
            let options = GetOptions {
                range: Some(GetRange::Bounded(range.clone())),
                if_match: pinned.e_tag.clone(),
                version: pinned.version.clone(),
                ..GetOptions::default()
            };
            let got = self
                .read_inner_for(path)
                .get_opts(path, options)
                .await
                .map_err(|source| map_object_store_error(source, path.as_ref()))?;
            // Cached metadata may have no origin identity. Check tokens that
            // were actually captured; caller content hashes still authenticate
            // the entire immutable body when a cache miss enriches metadata.
            if got.meta.size != pinned.size
                || (pinned.e_tag.is_some() && got.meta.e_tag != pinned.e_tag)
                || (pinned.version.is_some() && got.meta.version != pinned.version)
            {
                return Err(StorageError::CorruptObject {
                    path: path.to_string(),
                    reason: "immutable source identity changed during ranged read".to_owned(),
                });
            }
            let (_, actual, mut body) = self.stream_response(path, Some(range.clone()), got)?;
            if actual != range {
                return Err(StorageError::CorruptObject {
                    path: path.to_string(),
                    reason: "immutable range response was clamped".to_owned(),
                });
            }
            let mut bytes = Vec::with_capacity((range.end - range.start) as usize);
            while let Some(chunk) = body.next().await {
                bytes.extend_from_slice(&chunk?);
            }
            Ok(Bytes::from(bytes))
        })
        .await
    }
}

async fn origin_permit(
    admission: Option<&Arc<tokio::sync::Semaphore>>,
) -> Result<Option<tokio::sync::OwnedSemaphorePermit>> {
    match admission {
        Some(admission) => admission
            .clone()
            .acquire_owned()
            .await
            .map(Some)
            .map_err(|source| StorageError::Io {
                source: std::io::Error::other(source),
            }),
        None => Ok(None),
    }
}

fn cancellable_stream(
    body: StorageByteStream,
    cancel: &CancellationToken,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
) -> StorageByteStream {
    let cancel = cancel.clone();
    stream::try_unfold(
        (body, cancel, permit),
        |(mut body, cancel, permit)| async move {
            let chunk = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(StorageError::Cancelled),
                chunk = body.next() => chunk,
            };
            chunk
                .transpose()
                .map(|chunk| chunk.map(|chunk| (chunk, (body, cancel, permit))))
        },
    )
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures_util::{StreamExt, stream::BoxStream};
    use object_store::memory::InMemory;
    use object_store::{
        CopyOptions, GetOptions, GetResult, GetResultPayload, ListResult, MultipartUpload,
        ObjectStore, ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult,
    };
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::sync::Semaphore;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Fault {
        Range,
        Short,
        Long,
        Identity,
        Transient,
        HoldFirst,
        HeadIdentityMissing,
    }

    #[derive(Debug)]
    struct Endpoint {
        inner: InMemory,
        started: Semaphore,
        release: Semaphore,
        active: AtomicUsize,
        maximum: AtomicUsize,
        requests: Mutex<Vec<GetOptions>>,
        fault: Mutex<Option<Fault>>,
    }

    struct Active(Arc<Endpoint>);
    impl Drop for Active {
        fn drop(&mut self) {
            self.0.active.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[derive(Debug)]
    struct Backend(Arc<Endpoint>);
    impl std::fmt::Display for Backend {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "range test")
        }
    }
    #[async_trait::async_trait]
    impl ObjectStore for Backend {
        async fn put_opts(
            &self,
            p: &Path,
            b: PutPayload,
            o: PutOptions,
        ) -> object_store::Result<PutResult> {
            self.0.inner.put_opts(p, b, o).await
        }
        async fn put_multipart_opts(
            &self,
            p: &Path,
            o: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            self.0.inner.put_multipart_opts(p, o).await
        }
        async fn get_opts(&self, p: &Path, o: GetOptions) -> object_store::Result<GetResult> {
            self.0.requests.lock().unwrap().push(o.clone());
            let mut got = self.0.inner.get_opts(p, o.clone()).await?;
            if o.head && *self.0.fault.lock().unwrap() == Some(Fault::HeadIdentityMissing) {
                got.meta.e_tag = None;
                got.meta.version = None;
            }
            if !o.head {
                let active = self.0.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.0.maximum.fetch_max(active, Ordering::SeqCst);
                self.0.started.add_permits(1);
                let endpoint = self.0.clone();
                let configured = *self.0.fault.lock().unwrap();
                let hold_first = configured == Some(Fault::HoldFirst);
                let targeted = if hold_first {
                    got.range.start == 0
                } else {
                    matches!(o.range, Some(GetRange::Bounded(ref r)) if r.start==4)
                };
                let released = hold_first && !targeted;
                let fault = if targeted {
                    let mut configured = self.0.fault.lock().unwrap();
                    let fault = *configured;
                    if fault == Some(Fault::Transient) {
                        *configured = None;
                    }
                    fault
                } else {
                    None
                };
                let mut meta = got.meta.clone();
                if fault == Some(Fault::Identity) {
                    meta.e_tag = Some("changed".to_owned());
                }
                let mut range = got.range.clone();
                if fault == Some(Fault::Range) {
                    range.start += 1;
                    range.end += 1;
                }
                let attributes = got.attributes.clone();
                let extensions = got.extensions.clone();
                let body = got.into_stream();
                let state = (body, Active(endpoint), released, fault);
                let stream = futures_util::stream::try_unfold(
                    state,
                    |(mut body, active, released, fault)| async move {
                        if !released {
                            active.0.release.acquire().await.unwrap().forget();
                        }
                        if released && fault == Some(Fault::Transient) {
                            return Err(object_store::Error::Generic {
                                store: "test range",
                                source: Box::new(std::io::Error::new(
                                    std::io::ErrorKind::ConnectionReset,
                                    "range interrupted",
                                )),
                            });
                        }
                        match body.next().await {
                            Some(result) => {
                                let mut bytes = result?;
                                if fault == Some(Fault::Short) || fault == Some(Fault::Transient) {
                                    bytes = bytes.slice(..bytes.len() - 1);
                                }
                                if fault == Some(Fault::Long) {
                                    let mut grown = bytes.to_vec();
                                    grown.push(0);
                                    bytes = Bytes::from(grown);
                                }
                                Ok(Some((bytes, (body, active, true, fault))))
                            }
                            None => Ok(None),
                        }
                    },
                )
                .boxed();
                got = GetResult {
                    meta,
                    range,
                    attributes,
                    extensions,
                    payload: GetResultPayload::Stream(stream),
                };
            }
            Ok(got)
        }
        fn delete_stream(
            &self,
            p: BoxStream<'static, object_store::Result<Path>>,
        ) -> BoxStream<'static, object_store::Result<Path>> {
            self.0.inner.delete_stream(p)
        }
        fn list(&self, p: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            self.0.inner.list(p)
        }
        async fn list_with_delimiter(&self, p: Option<&Path>) -> object_store::Result<ListResult> {
            self.0.inner.list_with_delimiter(p).await
        }
        async fn copy_opts(&self, a: &Path, b: &Path, o: CopyOptions) -> object_store::Result<()> {
            self.0.inner.copy_opts(a, b, o).await
        }
    }

    async fn endpoint() -> (Store, Arc<Endpoint>, Path) {
        let e = Arc::new(Endpoint {
            inner: InMemory::new(),
            started: Semaphore::new(0),
            release: Semaphore::new(0),
            active: AtomicUsize::new(0),
            maximum: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
            fault: Mutex::new(None),
        });
        let path = Path::from("capsule");
        e.inner
            .put(
                &path,
                Bytes::from_static(b"0123456789abcdef0123456789abcdef").into(),
            )
            .await
            .unwrap();
        (
            Store::with_retry(
                Arc::new(Backend(e.clone())),
                crate::RetryPolicy {
                    max_attempts: 2,
                    base: std::time::Duration::ZERO,
                    cap: std::time::Duration::ZERO,
                },
            ),
            e,
            path,
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn range_downloads_overlap_and_preserve_order() {
        let (store, e, path) = endpoint().await;
        let cancel = CancellationToken::new();
        let download = tokio::spawn(async move {
            let (_, range, mut body) = store
                .parallel_stream(&path, Some(0..32), 32, 4, None, &cancel)
                .await
                .unwrap();
            assert_eq!(range, 0..32);
            let mut bytes = Vec::new();
            while let Some(chunk) = body.next().await {
                bytes.extend_from_slice(&chunk.unwrap());
            }
            bytes
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), e.started.acquire_many(4))
            .await
            .expect("four concurrent ranged requests")
            .unwrap()
            .forget();
        assert_eq!(e.maximum.load(Ordering::SeqCst), 4);
        e.release.add_permits(32);
        assert_eq!(download.await.unwrap(), b"0123456789abcdef0123456789abcdef");
    }

    async fn collect(mut stream: StorageByteStream) -> Result<Vec<u8>> {
        let mut body = Vec::new();
        while let Some(bytes) = stream.next().await {
            body.extend_from_slice(&bytes?);
        }
        Ok(body)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cloned_stores_share_range_admission_without_ordered_read_deadlock() {
        let (store, e, path) = endpoint().await;
        let mut tasks = Vec::new();
        for _ in 0..3 {
            let store = store.clone();
            let path = path.clone();
            tasks.push(tokio::spawn(async move {
                let (_, _, body) = store
                    .parallel_stream(&path, Some(1..31), 30, 4, None, &CancellationToken::new())
                    .await
                    .unwrap();
                collect(body).await.unwrap()
            }));
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), e.started.acquire_many(4))
            .await
            .unwrap()
            .unwrap()
            .forget();
        assert_eq!(e.active.load(Ordering::SeqCst), CONCURRENCY);
        e.release.add_permits(64);
        for task in tasks {
            assert_eq!(
                tokio::time::timeout(std::time::Duration::from_secs(5), task)
                    .await
                    .unwrap()
                    .unwrap(),
                b"123456789abcdef0123456789abcde"
            );
        }
        assert_eq!(e.maximum.load(Ordering::SeqCst), CONCURRENCY);
    }

    #[tokio::test]
    async fn malformed_ranges_and_changed_identity_fail_closed() {
        for fault in [Fault::Range, Fault::Short, Fault::Long, Fault::Identity] {
            let (store, e, path) = endpoint().await;
            e.release.add_permits(64);
            *e.fault.lock().unwrap() = Some(fault);
            let (_, _, body) = store
                .parallel_stream(&path, Some(0..32), 32, 4, None, &CancellationToken::new())
                .await
                .unwrap();
            assert!(
                matches!(collect(body).await, Err(StorageError::CorruptObject { .. })),
                "{fault:?}"
            );
            assert_eq!(e.active.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn immutable_body_bounds_preserve_payload_and_admission() {
        let (store, endpoint, path) = endpoint().await;
        endpoint.release.add_permits(2);
        let observed = Arc::new(AtomicUsize::new(0));
        let counter = observed.clone();
        let store = store.with_read_byte_observer(Arc::new(move |bytes| {
            counter.fetch_add(bytes as usize, Ordering::SeqCst);
        }));
        for (bound, expected_bytes) in [(32, 32), (31, 32), (33, 64)] {
            let result = store.get_immutable_bounded(&path, bound).await;
            if bound < 32 {
                assert!(matches!(result, Err(StorageError::CorruptObject { .. })));
            } else {
                assert_eq!(
                    result.unwrap().as_ref(),
                    b"0123456789abcdef0123456789abcdef"
                );
            }
            assert_eq!(observed.load(Ordering::SeqCst), expected_bytes);
            assert_eq!(endpoint.active.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn cache_metadata_without_identity_allows_origin_range_metadata() {
        let (store, endpoint, path) = endpoint().await;
        endpoint.release.add_permits(64);
        *endpoint.fault.lock().unwrap() = Some(Fault::HeadIdentityMissing);
        let (_, _, body) = store
            .parallel_stream(&path, Some(0..32), 32, 4, None, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            collect(body).await.unwrap(),
            b"0123456789abcdef0123456789abcdef"
        );
    }

    #[tokio::test]
    async fn interrupted_range_retries_without_exposing_partial_bytes() {
        let (store, e, path) = endpoint().await;
        e.release.add_permits(64);
        *e.fault.lock().unwrap() = Some(Fault::Transient);
        let (_, _, body) = store
            .parallel_stream(&path, Some(0..32), 32, 4, None, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            collect(body).await.unwrap(),
            b"0123456789abcdef0123456789abcdef"
        );
        let requests = e.requests.lock().unwrap();
        let bodies = requests.iter().filter(|r| !r.head).collect::<Vec<_>>();
        assert_eq!(
            bodies
                .iter()
                .filter(|r| matches!(r.range,Some(GetRange::Bounded(ref r)) if *r==(4..8)))
                .count(),
            2
        );
        assert!(bodies.iter().all(|r| r.if_match.is_some()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancellation_and_drop_release_every_inflight_read() {
        for drop_reader in [false, true] {
            let (store, e, path) = endpoint().await;
            let cancel = CancellationToken::new();
            let worker_cancel = cancel.clone();
            let worker_store = store.clone();
            let task = tokio::spawn(async move {
                let (_, _, body) = worker_store
                    .parallel_stream(&path, Some(0..32), 32, 4, None, &worker_cancel)
                    .await
                    .unwrap();
                collect(body).await
            });
            tokio::time::timeout(std::time::Duration::from_secs(5), e.started.acquire_many(4))
                .await
                .unwrap()
                .unwrap()
                .forget();
            if drop_reader {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                cancel.cancel();
                assert!(matches!(task.await.unwrap(), Err(StorageError::Cancelled)));
            }
            assert_eq!(e.active.load(Ordering::SeqCst), 0);
            assert_eq!(store.parallel_reads.available_permits(), CONCURRENCY);
        }
    }

    #[tokio::test]
    async fn small_ranges_use_one_get_without_head_and_keep_cancellation() {
        let (store, e, path) = endpoint().await;
        e.release.add_permits(1);
        let (_, range, body) = store
            .get_parallel_range_stream(&path, 3..9, None, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            (range, collect(body).await.unwrap()),
            (3..9, b"345678".to_vec())
        );
        assert_eq!(e.requests.lock().unwrap().len(), 1);
        let empty = Path::from("empty");
        e.inner.put(&empty, Bytes::new().into()).await.unwrap();
        e.release.add_permits(1);
        let (meta, range, body) = store
            .get_parallel_stream(&empty, 0, None, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            (meta.size, range, collect(body).await.unwrap()),
            (0, 0..0, Vec::<u8>::new())
        );
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            store
                .get_parallel_range_stream(&path, 0..4, None, &cancel)
                .await,
            Err(StorageError::Cancelled)
        ));
    }

    struct Admission {
        cancel: CancellationToken,
        requests: AtomicUsize,
        bytes: AtomicUsize,
        reject: bool,
    }
    #[async_trait::async_trait]
    impl crate::ReadAdmission for Admission {
        fn cancellation(&self) -> &CancellationToken {
            &self.cancel
        }
        async fn request(
            &self,
        ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.requests.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn bytes(
            &self,
            bytes: u64,
        ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.bytes.fetch_add(bytes as usize, Ordering::SeqCst);
            if self.reject && bytes > 0 {
                Err(Box::new(std::io::Error::other("read budget exceeded")))
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn every_range_preserves_caller_admission_and_byte_observers() {
        for reject in [false, true] {
            let (store, e, path) = endpoint().await;
            e.release.add_permits(64);
            let admission = Arc::new(Admission {
                cancel: CancellationToken::new(),
                requests: AtomicUsize::new(0),
                bytes: AtomicUsize::new(0),
                reject,
            });
            let observed = Arc::new(AtomicUsize::new(0));
            let observer = observed.clone();
            let store = store
                .with_read_admission(admission.clone())
                .with_read_byte_observer(Arc::new(move |bytes| {
                    observer.fetch_add(bytes as usize, Ordering::SeqCst);
                }));
            let (_, _, body) = store
                .parallel_stream(&path, Some(0..32), 32, 4, None, &CancellationToken::new())
                .await
                .unwrap();
            let result = collect(body).await;
            if reject {
                assert!(matches!(result, Err(StorageError::ReadRejected { .. })));
                assert_eq!(observed.load(Ordering::SeqCst), 0);
            } else {
                assert_eq!(result.unwrap(), b"0123456789abcdef0123456789abcdef");
                assert_eq!(
                    (
                        admission.requests.load(Ordering::SeqCst),
                        admission.bytes.load(Ordering::SeqCst),
                        observed.load(Ordering::SeqCst)
                    ),
                    (9, 32, 32)
                );
            }
            assert_eq!(e.active.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn caller_origin_limit_is_held_through_each_response_body() {
        for chunk_bytes in [4, 64] {
            let (store, e, path) = endpoint().await;
            let admission = Arc::new(Semaphore::new(1));
            let origin = admission.clone();
            let task = tokio::spawn(async move {
                let (_, _, body) = store
                    .parallel_stream(
                        &path,
                        Some(0..32),
                        32,
                        chunk_bytes,
                        Some(origin),
                        &CancellationToken::new(),
                    )
                    .await
                    .unwrap();
                collect(body).await
            });
            tokio::time::timeout(std::time::Duration::from_secs(5), e.started.acquire())
                .await
                .unwrap()
                .unwrap()
                .forget();
            let reserved = admission.available_permits();
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert_eq!(
                (
                    reserved,
                    e.maximum.load(Ordering::SeqCst),
                    admission.available_permits()
                ),
                (0, 1, 1)
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn completed_ranges_share_the_global_buffer_budget_until_ordered_delivery() {
        let (store, e, path) = endpoint().await;
        *e.fault.lock().unwrap() = Some(Fault::HoldFirst);
        let mut tasks = Vec::new();
        for _ in 0..3 {
            let store = store.clone();
            let path = path.clone();
            tasks.push(tokio::spawn(async move {
                let (_, _, body) = store
                    .parallel_stream(&path, Some(0..32), 32, 4, None, &CancellationToken::new())
                    .await
                    .unwrap();
                collect(body).await.unwrap()
            }));
        }
        // Paused time expires only once runnable requests settle. Completed
        // later ranges still own capacity while first ranges remain blocked.
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), e.started.acquire_many(5))
                .await
                .is_err()
        );
        e.release.add_permits(64);
        for task in tasks {
            assert_eq!(task.await.unwrap(), b"0123456789abcdef0123456789abcdef");
        }
    }
}
