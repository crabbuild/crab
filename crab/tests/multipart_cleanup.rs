//! Multipart cleanup and error precedence across shared storage and LFS callers.

#![allow(clippy::unwrap_used, clippy::panic, reason = "test assertions")]

use std::{
    fmt,
    io::Write,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use crab_lfs::LfsObjectStore;
use crab_storage::{RetryPolicy, Store};
use futures_util::stream::BoxStream;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, UploadPart, memory::InMemory,
    path::Path,
};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    None,
    Part,
    Complete,
}

#[derive(Debug)]
struct UploadStore {
    inner: InMemory,
    failure: Failure,
    abort_fails: bool,
    events: Arc<Mutex<Vec<&'static str>>>,
}

impl fmt::Display for UploadStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("upload-fault-store")
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct UploadFailure(&'static str);

fn failure(phase: &'static str) -> object_store::Error {
    object_store::Error::Generic {
        store: "multipart-test",
        source: Box::new(UploadFailure(phase)),
    }
}

#[derive(Debug)]
struct Upload {
    inner: Box<dyn MultipartUpload>,
    failure: Failure,
    abort_fails: bool,
    events: Arc<Mutex<Vec<&'static str>>>,
}

#[async_trait]
impl MultipartUpload for Upload {
    fn put_part(&mut self, payload: PutPayload) -> UploadPart {
        self.events.lock().unwrap().push("part");
        if self.failure == Failure::Part {
            return Box::pin(async { Err(failure("part failure")) });
        }
        self.inner.put_part(payload)
    }
    async fn complete(&mut self) -> object_store::Result<PutResult> {
        self.events.lock().unwrap().push("complete");
        if self.failure == Failure::Complete {
            return Err(failure("complete failure"));
        }
        self.inner.complete().await
    }
    async fn abort(&mut self) -> object_store::Result<()> {
        self.events.lock().unwrap().push("abort");
        if self.abort_fails {
            return Err(failure("abort failure"));
        }
        self.inner.abort().await
    }
}

#[async_trait]
impl ObjectStore for UploadStore {
    async fn put_opts(
        &self,
        path: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.inner.put_opts(path, payload, opts).await
    }
    async fn put_multipart_opts(
        &self,
        path: &Path,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        Ok(Box::new(Upload {
            inner: self.inner.put_multipart_opts(path, opts).await?,
            failure: self.failure,
            abort_fails: self.abort_fails,
            events: Arc::clone(&self.events),
        }))
    }
    async fn get_opts(&self, path: &Path, opts: GetOptions) -> object_store::Result<GetResult> {
        self.inner.get_opts(path, opts).await
    }
    fn delete_stream(
        &self,
        paths: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(paths)
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
        opts: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, opts).await
    }
}

fn has_cause(error: &(dyn std::error::Error + 'static), phase: &str) -> bool {
    std::iter::successors(Some(error), |error| error.source()).any(|error| {
        error
            .downcast_ref::<UploadFailure>()
            .is_some_and(|cause| cause.0 == phase)
    })
}

#[tokio::test]
async fn multipart_failures_abort_once_and_preserve_primary_error() {
    let data = Bytes::from_static(b"multipart payload");
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&data).unwrap();
    file.flush().unwrap();
    let mut violations = Vec::new();
    for surface in ["bytes", "progress", "file", "lfs"] {
        for phase in [Failure::None, Failure::Part, Failure::Complete] {
            for abort_fails in [false, true] {
                let events = Arc::new(Mutex::new(Vec::new()));
                let store = Store::new(Arc::new(UploadStore {
                    inner: InMemory::new(),
                    failure: phase,
                    abort_fails,
                    events: Arc::clone(&events),
                }))
                .with_retry_policy(RetryPolicy {
                    max_attempts: 1,
                    base: Duration::ZERO,
                    cap: Duration::ZERO,
                });
                let path = Path::from("object");
                let cancel = CancellationToken::new();
                let progress = |_: u64| {};
                let result: Result<(), Box<dyn std::error::Error>> = match surface {
                    "bytes" | "progress" => store
                        .put_multipart_retry(
                            &path,
                            data.clone(),
                            8 * 1024 * 1024,
                            &cancel,
                            (surface == "progress").then_some(&progress),
                        )
                        .await
                        .map_err(Into::into),
                    "file" => store
                        .put_multipart_file_retry(
                            &path,
                            file.path(),
                            data.len() as u64,
                            *blake3::hash(&data).as_bytes(),
                            8 * 1024 * 1024,
                            &cancel,
                            None,
                        )
                        .await
                        .map_err(Into::into),
                    "lfs" => LfsObjectStore::new(store.clone(), "repo")
                        .put_stream_with_size(
                            &Sha256::digest(&data).into(),
                            Some(data.len() as u64),
                            file.path(),
                        )
                        .await
                        .map_err(Into::into),
                    _ => unreachable!(),
                };
                let observed = events.lock().unwrap().clone();
                let expected = match phase {
                    Failure::None => vec!["part", "complete"],
                    Failure::Part => vec!["part", "abort"],
                    Failure::Complete => vec!["part", "complete", "abort"],
                };
                let correct_result = match (phase, result) {
                    (Failure::None, Ok(())) => {
                        let path = if surface == "lfs" {
                            LfsObjectStore::object_path_for_prefix(
                                "repo",
                                &Sha256::digest(&data).into(),
                            )
                        } else {
                            path.clone()
                        };
                        store.get_with_etag(&path).await.unwrap().0 == data
                    }
                    (Failure::Part, Err(error)) => has_cause(error.as_ref(), "part failure"),
                    (Failure::Complete, Err(error)) => {
                        has_cause(error.as_ref(), "complete failure")
                    }
                    _ => false,
                };
                if observed != expected || !correct_result {
                    violations.push(format!("{surface}/{phase:?}/abort_fails={abort_fails}: events={observed:?}, original result preserved={correct_result}"));
                }
            }
        }
    }
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[tokio::test]
async fn callback_modes_preserve_part_boundaries_and_bytes() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let data = Bytes::from_static(b"multiple upload parts");
    for report_progress in [false, true] {
        let events = Arc::new(Mutex::new(Vec::new()));
        let store = Store::new(Arc::new(UploadStore {
            inner: InMemory::new(),
            failure: Failure::None,
            abort_fails: false,
            events: Arc::clone(&events),
        }));
        let reported = AtomicU64::new(0);
        let progress = |bytes| {
            reported.fetch_add(bytes, Ordering::Relaxed);
        };
        let path = Path::from("object");
        store
            .put_multipart_retry(
                &path,
                data.clone(),
                5,
                &CancellationToken::new(),
                report_progress.then_some(&progress),
            )
            .await
            .unwrap();
        let mut expected_events = vec!["part"; data.len().div_ceil(5)];
        expected_events.push("complete");
        let observed_events = events.lock().unwrap().clone();
        let stored = store.get_with_etag(&path).await.unwrap().0;
        assert_eq!(
            (observed_events, reported.load(Ordering::Relaxed), stored),
            (
                expected_events,
                if report_progress {
                    data.len() as u64
                } else {
                    0
                },
                data.clone()
            ),
            "report_progress={report_progress}",
        );
    }
}
