//! Bounded, retryable S3 multipart uploads and atomic immutable completion.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::multipart::UPLOAD_FRAME_BYTES;
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt as _;
use futures_util::stream::{BoxStream, FuturesUnordered};
use object_store::aws::AmazonS3;
use object_store::client::HttpRequest;
use object_store::multipart::{MultipartStore, PartId};
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMode, PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use tokio::sync::Semaphore;

const MULTIPART_THRESHOLD: usize = 32 * 1024 * 1024;
const PART_BYTES: usize = 16 * 1024 * 1024;
const MAX_PARTS: usize = 10_000;
const MAX_PART_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const IN_FLIGHT: usize = 4;

tokio::task_local! {
    static CREATE_COMPLETION: ();
}

pub(crate) fn condition_completion(request: &mut HttpRequest) {
    // object_store 0.14.1 adds this header after signing in its conditional-copy
    // implementation too. Scope only completion: parts and ordinary overwrite
    // uploads must retain their own semantics, even in concurrent tasks.
    if CREATE_COMPLETION.try_with(|()| ()).is_ok()
        && request.method() == "POST"
        && request.uri().query().is_some_and(|query| {
            url::form_urlencoded::parse(query.as_bytes()).any(|(key, _)| key == "uploadId")
        })
    {
        request.headers_mut().insert(
            "if-none-match",
            hyper::header::HeaderValue::from_static("*"),
        );
    }
}

#[derive(Debug)]
pub(crate) struct S3Store {
    inner: Arc<AmazonS3>,
    parts: Arc<Semaphore>,
    conditional_create: bool,
}

impl std::fmt::Display for S3Store {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.fmt(formatter)
    }
}

impl S3Store {
    pub(crate) fn new(inner: Arc<AmazonS3>, conditional_create: bool) -> Self {
        Self {
            inner,
            parts: Arc::new(Semaphore::new(IN_FLIGHT)),
            conditional_create,
        }
    }

    async fn create(
        &self,
        path: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        let part_size = PART_BYTES.max(payload.content_length().div_ceil(MAX_PARTS));
        if part_size as u64 > MAX_PART_BYTES {
            return Err(object_store::Error::NotSupported {
                source: "immutable multipart object exceeds S3 part limits".into(),
            });
        }
        let options = PutMultipartOptions {
            tags: opts.tags,
            attributes: opts.attributes,
            extensions: opts.extensions,
        };
        let mut upload = self.put_multipart_opts(path, options).await?;
        let result = Self::upload_parts(&mut *upload, path, payload, part_size).await;
        if result.is_err()
            && let Err(error) = upload.abort().await
        {
            tracing::warn!(%error, "failed to abort immutable multipart upload");
        }
        result
    }

    async fn upload_parts(
        upload: &mut dyn MultipartUpload,
        path: &Path,
        payload: PutPayload,
        part_size: usize,
    ) -> object_store::Result<PutResult> {
        let mut pending = FuturesUnordered::new();
        let mut buffers = Vec::new();
        let mut length = 0;
        for bytes in payload {
            let mut offset = 0;
            while offset < bytes.len() {
                let take = (part_size - length)
                    .min(UPLOAD_FRAME_BYTES)
                    .min(bytes.len() - offset);
                buffers.push(bytes.slice(offset..offset + take));
                offset += take;
                length += take;
                if length == part_size {
                    if pending.len() == IN_FLIGHT
                        && let Some(result) = pending.next().await
                    {
                        result?;
                    }
                    pending
                        .push(upload.put_part(std::mem::take(&mut buffers).into_iter().collect()));
                    length = 0;
                }
            }
        }
        if !buffers.is_empty() {
            if pending.len() == IN_FLIGHT
                && let Some(result) = pending.next().await
            {
                result?;
            }
            pending.push(upload.put_part(buffers.into_iter().collect()));
        }
        while let Some(result) = pending.next().await {
            result?;
        }
        let result = CREATE_COMPLETION.scope((), upload.complete()).await;
        match result {
            // Normalize only completion's occupied-key response. Part failures
            // must not masquerade as a successful competing publication.
            Err(
                source @ (object_store::Error::Precondition { .. }
                | object_store::Error::NotModified { .. }),
            ) => Err(object_store::Error::AlreadyExists {
                path: path.to_string(),
                source: Box::new(source),
            }),
            // S3 requires a new upload after completion's HTTP 409. The
            // dependency maps it to AlreadyExists; unlike HTTP 412 this does
            // not prove an occupied key that logical-create callers can read.
            Err(source @ object_store::Error::AlreadyExists { .. }) => {
                Err(object_store::Error::Generic {
                    store: "S3 multipart create",
                    source: Box::new(crate::StorageError::NetworkTransient { source }),
                })
            }
            other => other,
        }
    }
}

struct S3Upload {
    state: Arc<UploadState>,
    next_index: usize,
    active: bool,
}

#[derive(Debug)]
struct UploadState {
    inner: Arc<AmazonS3>,
    path: Path,
    id: String,
    completed: Mutex<BTreeMap<usize, PartId>>,
    permits: Arc<Semaphore>,
    runtime: tokio::runtime::Handle,
}

impl std::fmt::Debug for S3Upload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("S3Upload")
            .field("next_index", &self.next_index)
            .finish_non_exhaustive()
    }
}

impl UploadState {
    async fn part(&self, index: usize, payload: PutPayload) -> object_store::Result<PartId> {
        let _permit =
            self.permits
                .acquire()
                .await
                .map_err(|source| object_store::Error::Generic {
                    store: "S3 multipart",
                    source: Box::new(source),
                })?;
        // Retrying high-level put_part would allocate a new index and could
        // duplicate bytes. Retry the same immutable payload and explicit index.
        crate::retry(&crate::RetryPolicy::DEFAULT, || {
            let payload = payload.clone();
            async move {
                MultipartStore::put_part(&*self.inner, &self.path, &self.id, index, payload)
                    .await
                    .map_err(|source| crate::map_object_store_error(source, self.path.as_ref()))
            }
        })
        .await
        .map_err(|source| match source {
            crate::StorageError::ObjectStore { source, .. }
            | crate::StorageError::NetworkTransient { source } => source,
            source => object_store::Error::Generic {
                store: "S3 multipart",
                source: Box::new(source),
            },
        })
    }
}

#[async_trait]
impl MultipartUpload for S3Upload {
    fn put_part(&mut self, payload: PutPayload) -> object_store::UploadPart {
        let index = self.next_index;
        self.next_index += 1;
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let part = state.part(index, payload).await?;
            state
                .completed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(index, part);
            Ok(())
        })
    }

    async fn complete(&mut self) -> object_store::Result<PutResult> {
        let parts = {
            let completed = self
                .state
                .completed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if completed.len() != self.next_index {
                return Err(object_store::Error::Generic {
                    store: "S3 multipart",
                    source: "multipart parts are incomplete".into(),
                });
            }
            completed.values().cloned().collect()
        };
        let result = MultipartStore::complete_multipart(
            &*self.state.inner,
            &self.state.path,
            &self.state.id,
            parts,
        )
        .await;
        if result.is_ok() {
            self.active = false;
        }
        result
    }

    async fn abort(&mut self) -> object_store::Result<()> {
        let result =
            MultipartStore::abort_multipart(&*self.state.inner, &self.state.path, &self.state.id)
                .await;
        if result.is_ok() || matches!(result, Err(object_store::Error::NotFound { .. })) {
            self.active = false;
        }
        result
    }
}

impl Drop for S3Upload {
    fn drop(&mut self) {
        // Ordinary and immutable upload cancellation share abort ownership.
        // Returned failures await abort before their caller retries the upload.
        if self.active {
            let state = Arc::clone(&self.state);
            self.state.runtime.spawn(async move {
                if let Err(error) =
                    MultipartStore::abort_multipart(&*state.inner, &state.path, &state.id).await
                {
                    tracing::warn!(%error, "failed to abort cancelled multipart upload");
                }
            });
        }
    }
}

#[async_trait]
impl ObjectStore for S3Store {
    async fn put_opts(
        &self,
        path: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        if self.conditional_create
            && opts.mode == PutMode::Create
            && payload.content_length() >= MULTIPART_THRESHOLD
        {
            return self.create(path, payload, opts).await;
        }
        self.inner.put_opts(path, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        path: &Path,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|source| {
            object_store::Error::Generic {
                store: "S3 multipart",
                source: Box::new(source),
            }
        })?;
        let id = MultipartStore::create_multipart_opts(&*self.inner, path, opts).await?;
        Ok(Box::new(S3Upload {
            state: Arc::new(UploadState {
                inner: Arc::clone(&self.inner),
                path: path.clone(),
                id,
                completed: Mutex::new(BTreeMap::new()),
                permits: Arc::clone(&self.parts),
                runtime,
            }),
            next_index: 0,
            active: true,
        }))
    }

    async fn get_opts(&self, path: &Path, opts: GetOptions) -> object_store::Result<GetResult> {
        self.inner.get_opts(path, opts).await
    }

    async fn get_ranges(
        &self,
        path: &Path,
        ranges: &[std::ops::Range<u64>],
    ) -> object_store::Result<Vec<Bytes>> {
        self.inner.get_ranges(path, ranges).await
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

    fn list_with_offset(
        &self,
        prefix: Option<&Path>,
        offset: &Path,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list_with_offset(prefix, offset)
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

#[cfg(test)]
mod tests;
