//! Bounded backend operation observation for object-store transports.

use std::fmt;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::{StreamExt as _, stream::BoxStream};
use object_store::multipart::{MultipartStore, PartId};
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, GetResultPayload, ListResult, MultipartId, MultipartUpload,
    ObjectMeta, ObjectStore, PutMultipartOptions, PutOptions, PutPayload, PutResult,
};

/// Fixed operation classes emitted by backend observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum StorageOperation {
    /// Complete object-body read.
    Get,
    /// Metadata-only read.
    Head,
    /// Bounded object-body read.
    Range,
    /// Atomic object write.
    Put,
    /// One logical delete stream.
    Delete,
    /// One logical listing stream or delimiter request.
    List,
    /// Server-side object copy.
    Copy,
    /// High- or low-level multipart session creation.
    MultipartStart,
    /// One multipart part write.
    MultipartPart,
    /// Multipart session completion.
    MultipartComplete,
    /// Multipart session abort.
    MultipartAbort,
}

impl StorageOperation {
    /// Every operation class in stable index order.
    pub const ALL: [Self; 11] = [
        Self::Get,
        Self::Head,
        Self::Range,
        Self::Put,
        Self::Delete,
        Self::List,
        Self::Copy,
        Self::MultipartStart,
        Self::MultipartPart,
        Self::MultipartComplete,
        Self::MultipartAbort,
    ];

    /// Returns the stable array index for this class.
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Returns the bounded metric label for this class.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Head => "head",
            Self::Range => "range",
            Self::Put => "put",
            Self::Delete => "delete",
            Self::List => "list",
            Self::Copy => "copy",
            Self::MultipartStart => "multipart_start",
            Self::MultipartPart => "multipart_part",
            Self::MultipartComplete => "multipart_complete",
            Self::MultipartAbort => "multipart_abort",
        }
    }
}

/// Fixed terminal outcome classes emitted by backend observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum StorageOutcome {
    /// The logical operation completed successfully.
    Success,
    /// The requested backend object or session was absent.
    NotFound,
    /// A create or precondition conflicted with backend state.
    Conflict,
    /// Credentials were missing, expired, invalid, or unauthorized.
    Auth,
    /// The provider or a local transport admission policy throttled work.
    Throttled,
    /// A transport failure may succeed when retried.
    Transient,
    /// The backend does not implement the operation.
    Unsupported,
    /// The operation future or stream ended before a terminal result.
    Cancelled,
    /// A non-retryable failure did not match another bounded class.
    Error,
}

impl StorageOutcome {
    /// Every outcome class in stable index order.
    pub const ALL: [Self; 9] = [
        Self::Success,
        Self::NotFound,
        Self::Conflict,
        Self::Auth,
        Self::Throttled,
        Self::Transient,
        Self::Unsupported,
        Self::Cancelled,
        Self::Error,
    ];

    /// Returns the stable array index for this class.
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Returns the bounded metric label for this class.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::Auth => "auth",
            Self::Throttled => "throttled",
            Self::Transient => "transient",
            Self::Unsupported => "unsupported",
            Self::Cancelled => "cancelled",
            Self::Error => "error",
        }
    }
}

/// Completed logical backend operation without object identity or credentials.
#[derive(Debug, Clone, Copy)]
pub struct StorageObservation {
    /// Logical operation that finished.
    pub operation: StorageOperation,
    /// Bounded terminal result.
    pub outcome: StorageOutcome,
    /// Time from wrapper invocation through response-stream termination.
    pub duration: Duration,
    /// Body bytes yielded before the terminal result.
    pub bytes_read: u64,
    /// Payload bytes accepted by a successful write or part operation.
    pub bytes_written: u64,
}

/// Receives bounded backend lifecycle events.
pub trait StorageObserver: Send + Sync {
    /// Marks a newly active logical backend operation.
    fn started(&self, operation: StorageOperation);

    /// Records the terminal result of a logical backend operation.
    fn finished(&self, observation: StorageObservation);
}

pub(crate) struct ObservedObjectStore {
    inner: Arc<dyn ObjectStore>,
    observer: Arc<dyn StorageObserver>,
}

impl ObservedObjectStore {
    pub(crate) fn new(inner: Arc<dyn ObjectStore>, observer: Arc<dyn StorageObserver>) -> Self {
        Self { inner, observer }
    }
}

impl fmt::Debug for ObservedObjectStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ObservedObjectStore")
    }
}

impl fmt::Display for ObservedObjectStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ObservedObjectStore")
    }
}

#[async_trait::async_trait]
impl ObjectStore for ObservedObjectStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        let bytes = saturating_u64(payload.content_length());
        let mut observation = ActiveObservation::new(StorageOperation::Put, &self.observer);
        let result = self.inner.put_opts(location, payload, options).await;
        observation.finish_result(&result, 0, bytes);
        result
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        options: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        let mut observation =
            ActiveObservation::new(StorageOperation::MultipartStart, &self.observer);
        let result = self.inner.put_multipart_opts(location, options).await;
        match result {
            Ok(upload) => {
                observation.finish(StorageOutcome::Success, 0, 0);
                Ok(Box::new(ObservedMultipartUpload {
                    inner: upload,
                    observer: Arc::clone(&self.observer),
                }))
            }
            Err(error) => {
                observation.finish(classify_error(&error), 0, 0);
                Err(error)
            }
        }
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        let operation = if options.head {
            StorageOperation::Head
        } else if options.range.is_some() {
            StorageOperation::Range
        } else {
            StorageOperation::Get
        };
        let mut observation = ActiveObservation::new(operation, &self.observer);
        let result = self.inner.get_opts(location, options).await;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                observation.finish(classify_error(&error), 0, 0);
                return Err(error);
            }
        };
        if operation == StorageOperation::Head {
            observation.finish(StorageOutcome::Success, 0, 0);
            return Ok(result);
        }

        let meta = result.meta.clone();
        let range = result.range.clone();
        let attributes = result.attributes.clone();
        let extensions = result.extensions.clone();
        let payload =
            GetResultPayload::Stream(observe_get_stream(result.into_stream(), observation));
        Ok(GetResult {
            payload,
            meta,
            range,
            attributes,
            extensions,
        })
    }

    async fn get_ranges(
        &self,
        location: &Path,
        ranges: &[Range<u64>],
    ) -> object_store::Result<Vec<Bytes>> {
        let mut observation = ActiveObservation::new(StorageOperation::Range, &self.observer);
        let result = self.inner.get_ranges(location, ranges).await;
        let bytes_read = result.as_ref().map_or(0, |bodies| {
            bodies.iter().fold(0_u64, |total, body| {
                total.saturating_add(saturating_u64(body.len()))
            })
        });
        observation.finish_result(&result, bytes_read, 0);
        result
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        let observation = ActiveObservation::new(StorageOperation::Delete, &self.observer);
        observe_stream(self.inner.delete_stream(locations), observation)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        let observation = ActiveObservation::new(StorageOperation::List, &self.observer);
        observe_stream(self.inner.list(prefix), observation)
    }

    fn list_with_offset(
        &self,
        prefix: Option<&Path>,
        offset: &Path,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        let observation = ActiveObservation::new(StorageOperation::List, &self.observer);
        observe_stream(self.inner.list_with_offset(prefix, offset), observation)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        let mut observation = ActiveObservation::new(StorageOperation::List, &self.observer);
        let result = self.inner.list_with_delimiter(prefix).await;
        observation.finish_result(&result, 0, 0);
        result
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        let mut observation = ActiveObservation::new(StorageOperation::Copy, &self.observer);
        let result = self.inner.copy_opts(from, to, options).await;
        observation.finish_result(&result, 0, 0);
        result
    }
}

pub(crate) struct ObservedMultipartStore {
    inner: Arc<dyn MultipartStore>,
    observer: Arc<dyn StorageObserver>,
}

impl ObservedMultipartStore {
    pub(crate) fn new(inner: Arc<dyn MultipartStore>, observer: Arc<dyn StorageObserver>) -> Self {
        Self { inner, observer }
    }
}

#[async_trait::async_trait]
impl MultipartStore for ObservedMultipartStore {
    async fn create_multipart(&self, path: &Path) -> object_store::Result<MultipartId> {
        let mut observation =
            ActiveObservation::new(StorageOperation::MultipartStart, &self.observer);
        let result = self.inner.create_multipart(path).await;
        observation.finish_result(&result, 0, 0);
        result
    }

    async fn create_multipart_opts(
        &self,
        path: &Path,
        options: PutMultipartOptions,
    ) -> object_store::Result<MultipartId> {
        let mut observation =
            ActiveObservation::new(StorageOperation::MultipartStart, &self.observer);
        let result = self.inner.create_multipart_opts(path, options).await;
        observation.finish_result(&result, 0, 0);
        result
    }

    async fn put_part(
        &self,
        path: &Path,
        id: &MultipartId,
        part_idx: usize,
        data: PutPayload,
    ) -> object_store::Result<PartId> {
        let bytes = saturating_u64(data.content_length());
        let mut observation =
            ActiveObservation::new(StorageOperation::MultipartPart, &self.observer);
        let result = self.inner.put_part(path, id, part_idx, data).await;
        observation.finish_result(&result, 0, bytes);
        result
    }

    async fn complete_multipart(
        &self,
        path: &Path,
        id: &MultipartId,
        parts: Vec<PartId>,
    ) -> object_store::Result<PutResult> {
        let mut observation =
            ActiveObservation::new(StorageOperation::MultipartComplete, &self.observer);
        let result = self.inner.complete_multipart(path, id, parts).await;
        observation.finish_result(&result, 0, 0);
        result
    }

    async fn abort_multipart(&self, path: &Path, id: &MultipartId) -> object_store::Result<()> {
        let mut observation =
            ActiveObservation::new(StorageOperation::MultipartAbort, &self.observer);
        let result = self.inner.abort_multipart(path, id).await;
        observation.finish_result(&result, 0, 0);
        result
    }
}

struct ObservedMultipartUpload {
    inner: Box<dyn MultipartUpload>,
    observer: Arc<dyn StorageObserver>,
}

impl fmt::Debug for ObservedMultipartUpload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ObservedMultipartUpload")
    }
}

#[async_trait::async_trait]
impl MultipartUpload for ObservedMultipartUpload {
    fn put_part(&mut self, data: PutPayload) -> object_store::UploadPart {
        let bytes = saturating_u64(data.content_length());
        let future = self.inner.put_part(data);
        let observer = Arc::clone(&self.observer);
        Box::pin(async move {
            let mut observation =
                ActiveObservation::new(StorageOperation::MultipartPart, &observer);
            let result = future.await;
            observation.finish_result(&result, 0, bytes);
            result
        })
    }

    async fn complete(&mut self) -> object_store::Result<PutResult> {
        let mut observation =
            ActiveObservation::new(StorageOperation::MultipartComplete, &self.observer);
        let result = self.inner.complete().await;
        observation.finish_result(&result, 0, 0);
        result
    }

    async fn abort(&mut self) -> object_store::Result<()> {
        let mut observation =
            ActiveObservation::new(StorageOperation::MultipartAbort, &self.observer);
        let result = self.inner.abort().await;
        observation.finish_result(&result, 0, 0);
        result
    }
}

struct ActiveObservation {
    observer: Arc<dyn StorageObserver>,
    operation: StorageOperation,
    started: Instant,
    bytes_read: u64,
    bytes_written: u64,
    finished: bool,
}

impl ActiveObservation {
    fn new(operation: StorageOperation, observer: &Arc<dyn StorageObserver>) -> Self {
        observer.started(operation);
        Self {
            observer: Arc::clone(observer),
            operation,
            started: Instant::now(),
            bytes_read: 0,
            bytes_written: 0,
            finished: false,
        }
    }

    fn finish(&mut self, outcome: StorageOutcome, bytes_read: u64, bytes_written: u64) {
        if self.finished {
            return;
        }
        self.bytes_read = self.bytes_read.saturating_add(bytes_read);
        self.bytes_written = self.bytes_written.saturating_add(bytes_written);
        self.observer.finished(StorageObservation {
            operation: self.operation,
            outcome,
            duration: self.started.elapsed(),
            bytes_read: self.bytes_read,
            bytes_written: self.bytes_written,
        });
        self.finished = true;
    }

    fn record_read(&mut self, bytes: usize) {
        self.bytes_read = self.bytes_read.saturating_add(saturating_u64(bytes));
    }

    fn finish_result<T>(
        &mut self,
        result: &object_store::Result<T>,
        bytes_read: u64,
        bytes_written: u64,
    ) {
        match result {
            Ok(_) => self.finish(StorageOutcome::Success, bytes_read, bytes_written),
            Err(error) => self.finish(classify_error(error), 0, 0),
        }
    }
}

impl Drop for ActiveObservation {
    fn drop(&mut self) {
        self.finish(StorageOutcome::Cancelled, 0, 0);
    }
}

fn observe_get_stream(
    stream: BoxStream<'static, object_store::Result<Bytes>>,
    observation: ActiveObservation,
) -> BoxStream<'static, object_store::Result<Bytes>> {
    Box::pin(futures_util::stream::unfold(
        (stream, Some(observation)),
        |(mut stream, mut observation)| async move {
            let active = observation.as_mut()?;
            match stream.next().await {
                Some(Ok(bytes)) => {
                    active.record_read(bytes.len());
                    Some((Ok(bytes), (stream, observation)))
                }
                Some(Err(error)) => {
                    active.finish(classify_error(&error), 0, 0);
                    Some((Err(error), (stream, None)))
                }
                None => {
                    active.finish(StorageOutcome::Success, 0, 0);
                    None
                }
            }
        },
    ))
}

fn observe_stream<T: Send + 'static>(
    stream: BoxStream<'static, object_store::Result<T>>,
    observation: ActiveObservation,
) -> BoxStream<'static, object_store::Result<T>> {
    Box::pin(futures_util::stream::unfold(
        (stream, Some(observation)),
        |(mut stream, mut observation)| async move {
            let active = observation.as_mut()?;
            match stream.next().await {
                Some(Ok(value)) => Some((Ok(value), (stream, observation))),
                Some(Err(error)) => {
                    active.finish(classify_error(&error), 0, 0);
                    Some((Err(error), (stream, None)))
                }
                None => {
                    active.finish(StorageOutcome::Success, 0, 0);
                    None
                }
            }
        },
    ))
}

fn classify_error(error: &object_store::Error) -> StorageOutcome {
    if let Some(storage) = storage_error(error) {
        return classify_storage_error(storage);
    }

    match error {
        object_store::Error::NotFound { .. } => StorageOutcome::NotFound,
        object_store::Error::AlreadyExists { .. }
        | object_store::Error::Precondition { .. }
        | object_store::Error::NotModified { .. } => StorageOutcome::Conflict,
        object_store::Error::PermissionDenied { .. }
        | object_store::Error::Unauthenticated { .. } => StorageOutcome::Auth,
        object_store::Error::NotSupported { .. } | object_store::Error::NotImplemented { .. } => {
            StorageOutcome::Unsupported
        }
        object_store::Error::Generic { .. }
            if crate::error_map::is_throttling_message(&error.to_string().to_ascii_lowercase()) =>
        {
            StorageOutcome::Throttled
        }
        object_store::Error::Generic { .. } => StorageOutcome::Transient,
        _ => StorageOutcome::Error,
    }
}

fn classify_storage_error(error: &crate::StorageError) -> StorageOutcome {
    match error {
        crate::StorageError::NotFound { .. } => StorageOutcome::NotFound,
        crate::StorageError::StateConflict { .. } => StorageOutcome::Conflict,
        crate::StorageError::AuthFailed { .. }
        | crate::StorageError::AuthExpired { .. }
        | crate::StorageError::NoCredentials
        | crate::StorageError::Forbidden { .. } => StorageOutcome::Auth,
        crate::StorageError::Throttled { .. } => StorageOutcome::Throttled,
        crate::StorageError::NetworkTransient { .. } => StorageOutcome::Transient,
        crate::StorageError::NotSupported { .. } => StorageOutcome::Unsupported,
        crate::StorageError::Cancelled => StorageOutcome::Cancelled,
        crate::StorageError::ReadRejected { source } => {
            storage_error(source.as_ref()).map_or(StorageOutcome::Error, classify_storage_error)
        }
        _ => StorageOutcome::Error,
    }
}

fn storage_error<'a>(
    error: &'a (dyn std::error::Error + 'static),
) -> Option<&'a crate::StorageError> {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(storage) = error.downcast_ref::<crate::StorageError>() {
            return Some(storage);
        }
        current = error.source();
    }
    None
}

fn saturating_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test assertions")]
mod tests;
