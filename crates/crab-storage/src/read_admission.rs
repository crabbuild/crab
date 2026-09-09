use std::fmt;
use std::ops::Range;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::{StreamExt, stream::BoxStream};
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, GetResultPayload, ListResult, MultipartUpload, ObjectMeta,
    ObjectStore, ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult,
};

/// Caller-owned admission for object-body reads, HEAD requests and listing invocations.
///
/// Requests are admitted before calling the backend, including each Store retry.
/// Crab-built cloud stores additionally admit provider retries and listing pages
/// at their HTTP boundary and charge every response-body chunk. Other stores
/// reserve successful object-body lengths from response headers; reservations
/// are not refunded on cancellation or incomplete delivery.
#[async_trait::async_trait]
pub trait ReadAdmission: Send + Sync {
    /// Cancellation scope for admission, response headers and streamed body reads.
    fn cancellation(&self) -> &tokio_util::sync::CancellationToken;

    /// Admit one backend GET, HEAD or listing invocation.
    async fn request(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
    /// Reserve the advertised response body length.
    async fn bytes(&self, bytes: u64) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

pub(crate) struct AdmittedStore {
    pub(crate) inner: Arc<dyn ObjectStore>,
    pub(crate) admission: Arc<dyn ReadAdmission>,
}

enum ListingState {
    Start {
        inner: Arc<dyn ObjectStore>,
        admission: Arc<dyn ReadAdmission>,
        prefix: Option<Path>,
        offset: Option<Path>,
    },
    Open {
        stream: BoxStream<'static, object_store::Result<ObjectMeta>>,
        context: Arc<crate::transport_read_admission::TransportReadContext>,
        cancellation: tokio_util::sync::CancellationToken,
    },
}

impl AdmittedStore {
    fn listing(
        &self,
        prefix: Option<Path>,
        offset: Option<Path>,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        let inner = self.inner.clone();
        let admission = self.admission.clone();
        futures_util::stream::try_unfold(
            ListingState::Start {
                inner,
                admission,
                prefix,
                offset,
            },
            |state| async move {
                let (mut stream, context, cancellation) = match state {
                    ListingState::Start {
                        inner,
                        admission,
                        prefix,
                        offset,
                    } => {
                        tokio::select! {
                            biased;
                            () = admission.cancellation().cancelled() => return Err(cancelled()),
                            result = admission.request() => result.map_err(rejected)?,
                        }
                        let cancellation = admission.cancellation().clone();
                        let context =
                            crate::transport_read_admission::TransportReadContext::new(admission);
                        let stream = match offset {
                            Some(offset) => inner.list_with_offset(prefix.as_ref(), &offset),
                            None => inner.list(prefix.as_ref()),
                        };
                        (stream, context, cancellation)
                    }
                    ListingState::Open {
                        stream,
                        context,
                        cancellation,
                    } => (stream, context, cancellation),
                };
                let entry = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return Err(cancelled()),
                    entry = crate::transport_read_admission::scope(
                        context.clone(),
                        stream.next(),
                    ) => entry,
                };
                match entry {
                    Some(entry) => entry.map(|entry| {
                        Some((
                            entry,
                            ListingState::Open {
                                stream,
                                context,
                                cancellation,
                            },
                        ))
                    }),
                    None => Ok(None),
                }
            },
        )
        .boxed()
    }
}

impl fmt::Debug for AdmittedStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AdmittedStore")
    }
}

impl fmt::Display for AdmittedStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AdmittedStore")
    }
}

fn rejected(source: Box<dyn std::error::Error + Send + Sync>) -> object_store::Error {
    // SlateDB retries Generic errors indefinitely. Use its non-retryable
    // category while retaining the typed admission reason for our callers.
    object_store::Error::NotSupported {
        source: Box::new(crate::StorageError::ReadRejected { source }),
    }
}

fn cancelled() -> object_store::Error {
    // Keep cancellation inside the fatal admission marker so cache and retry
    // adapters cannot turn it into another provider attempt.
    rejected(Box::new(crate::StorageError::Cancelled))
}

fn corrupt(path: &Path) -> object_store::Error {
    // SlateDB retries range-body errors inside its unbounded retry loop.
    // The Store facade recovers CorruptObject and owns its bounded retry.
    object_store::Error::NotSupported {
        source: Box::new(crate::StorageError::CorruptObject {
            path: path.to_string(),
            reason: "response body does not match its advertised range".into(),
        }),
    }
}

fn terminal_read_error(error: object_store::Error) -> object_store::Error {
    if matches!(error, object_store::Error::NotSupported { .. })
        || crate::read_rejection(&error).is_none()
    {
        return error;
    }
    // Provider response streams erase their transport error category. Restore
    // the fatal marker before SlateDB can retry an exhausted owner budget.
    object_store::Error::NotSupported {
        source: Box::new(error),
    }
}

#[async_trait::async_trait]
impl ObjectStore for AdmittedStore {
    async fn get_opts(&self, path: &Path, options: GetOptions) -> object_store::Result<GetResult> {
        let cancellation = self.admission.cancellation().clone();
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(cancelled()),
            result = self.admission.request() => result.map_err(rejected)?,
        }
        let context =
            crate::transport_read_admission::TransportReadContext::new(self.admission.clone());
        let head = options.head;
        let result = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(cancelled()),
            result = crate::transport_read_admission::scope(
                context,
                self.inner.get_opts(path, options),
            ) => result.map_err(terminal_read_error)?,
        };
        let expected = if head {
            0
        } else {
            result
                .range
                .end
                .checked_sub(result.range.start)
                .ok_or_else(|| corrupt(path))?
        };
        if result
            .extensions
            .get::<crate::transport_read_admission::TransportReadAccounted>()
            .is_none()
        {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(cancelled()),
                result = self.admission.bytes(expected) => result.map_err(rejected)?,
            }
        }
        let meta = result.meta.clone();
        let range = result.range.clone();
        let attributes = result.attributes.clone();
        let extensions = result.extensions.clone();
        let path = path.clone();
        let stream = futures_util::stream::try_unfold(
            (result.into_stream(), expected, path, cancellation),
            |(mut stream, remaining, path, cancellation)| async move {
                let entry = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return Err(cancelled()),
                    entry = stream.next() => entry,
                };
                match entry {
                    Some(Ok(bytes)) => {
                        let left = remaining
                            .checked_sub(bytes.len() as u64)
                            .ok_or_else(|| corrupt(&path))?;
                        Ok(Some((bytes, (stream, left, path, cancellation))))
                    }
                    Some(Err(error)) => Err(terminal_read_error(error)),
                    None if remaining == 0 => Ok(None),
                    None => Err(corrupt(&path)),
                }
            },
        )
        .boxed();
        Ok(GetResult {
            payload: GetResultPayload::Stream(stream),
            meta,
            range,
            attributes,
            extensions,
        })
    }

    async fn get_ranges(
        &self,
        path: &Path,
        ranges: &[Range<u64>],
    ) -> object_store::Result<Vec<Bytes>> {
        // Coalescing remains bounded by the dependency policy, but every resulting
        // read must pass admission rather than a backend's unobserved bulk path.
        object_store::coalesce_ranges(
            ranges,
            |range| self.get_range(path, range),
            object_store::OBJECT_STORE_COALESCE_DEFAULT,
        )
        .await
    }

    async fn put_opts(
        &self,
        path: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.inner.put_opts(path, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        path: &Path,
        options: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(path, options).await
    }

    fn delete_stream(
        &self,
        paths: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(paths)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.listing(prefix.cloned(), None)
    }

    fn list_with_offset(
        &self,
        prefix: Option<&Path>,
        offset: &Path,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.listing(prefix.cloned(), Some(offset.clone()))
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        tokio::select! {
            biased;
            () = self.admission.cancellation().cancelled() => return Err(cancelled()),
            result = self.admission.request() => result.map_err(rejected)?,
        }
        let context =
            crate::transport_read_admission::TransportReadContext::new(self.admission.clone());
        tokio::select! {
            biased;
            () = self.admission.cancellation().cancelled() => Err(cancelled()),
            result = crate::transport_read_admission::scope(
                context,
                self.inner.list_with_delimiter(prefix),
            ) => result,
        }
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

#[cfg(test)]
mod tests;
