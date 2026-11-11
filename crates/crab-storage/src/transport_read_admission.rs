use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures_util::FutureExt as _;
use futures_util::StreamExt as _;
use hyper::body::{Body, Frame, SizeHint};
use object_store::ClientOptions;
use object_store::client::{
    HttpClient, HttpConnector, HttpError, HttpErrorKind, HttpRequest, HttpResponse,
    HttpResponseBody, HttpService, ReqwestConnector,
};

use crate::{ReadAdmission, StorageError};

tokio::task_local! {
    static READ_CONTEXTS: Vec<Arc<TransportReadContext>>;
}

#[derive(Clone, Debug)]
pub(crate) struct TransportReadAccounted;

pub(crate) struct TransportReadContext {
    admission: Arc<dyn ReadAdmission>,
    prepaid_request: AtomicBool,
    rejected: AtomicBool,
    rejection: Mutex<Option<Arc<dyn std::error::Error + Send + Sync>>>,
}

impl TransportReadContext {
    pub(crate) fn new(admission: Arc<dyn ReadAdmission>) -> Arc<Self> {
        Arc::new(Self {
            admission,
            prepaid_request: AtomicBool::new(true),
            rejected: AtomicBool::new(false),
            rejection: Mutex::new(None),
        })
    }

    async fn admit_request(&self) -> Result<(), HttpError> {
        if self.admission.cancellation().is_cancelled() {
            return Err(cancelled());
        }
        if let Some(error) = self.rejection() {
            return Err(error);
        }
        // AdmittedStore charges the logical call before entering this scope.
        // Its first HTTP request consumes that charge; retries and pages must
        // obtain another admission before they can reach the transport.
        if self
            .prepaid_request
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Ok(());
        }
        tokio::select! {
            biased;
            () = self.admission.cancellation().cancelled() => Err(cancelled()),
            result = self.admission.request() => result.map_err(|source| self.reject(source)),
        }
    }

    async fn admit_bytes(&self, bytes: u64) -> Result<(), HttpError> {
        if let Some(error) = self.rejection() {
            return Err(error);
        }
        tokio::select! {
            biased;
            () = self.admission.cancellation().cancelled() => Err(cancelled()),
            result = self.admission.bytes(bytes) => result.map_err(|source| self.reject(source)),
        }
    }

    fn rejection(&self) -> Option<HttpError> {
        if !self.rejected.load(Ordering::Acquire) {
            return None;
        }
        self.rejection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .map(shared_rejection)
    }

    fn reject(&self, source: Box<dyn std::error::Error + Send + Sync>) -> HttpError {
        let mut rejection = self
            .rejection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let source = rejection.get_or_insert_with(|| Arc::from(source)).clone();
        self.rejected.store(true, Ordering::Release);
        shared_rejection(source)
    }
}

#[derive(Debug)]
struct SharedReadRejection(Arc<dyn std::error::Error + Send + Sync>);

impl std::fmt::Display for SharedReadRejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for SharedReadRejection {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

fn shared_rejection(source: Arc<dyn std::error::Error + Send + Sync>) -> HttpError {
    rejected(Box::new(SharedReadRejection(source)))
}

pub(crate) async fn scope<F>(context: Arc<TransportReadContext>, future: F) -> F::Output
where
    F: Future,
{
    // Preserve outer contexts so repeated with_read_admission calls continue
    // to enforce every nested policy at the same physical transport boundary.
    let mut contexts = READ_CONTEXTS
        .try_with(Clone::clone)
        .unwrap_or_else(|_| Vec::new());
    contexts.push(context);
    READ_CONTEXTS.scope(contexts, future).await
}

fn contexts() -> Vec<Arc<TransportReadContext>> {
    READ_CONTEXTS
        .try_with(Clone::clone)
        .unwrap_or_else(|_| Vec::new())
}

/// Captured admission policies for a read request implemented outside `object_store` providers.
#[derive(Clone)]
pub struct TransportReadReceipt {
    contexts: Vec<Arc<TransportReadContext>>,
}

impl std::fmt::Debug for TransportReadReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransportReadReceipt")
            .field("policies", &self.contexts.len())
            .finish()
    }
}

impl TransportReadReceipt {
    /// Charge bytes returned by this physical request.
    pub async fn bytes(&self, bytes: u64) -> object_store::Result<()> {
        for context in &self.contexts {
            context.admission.received(bytes).await;
        }
        for context in &self.contexts {
            context.admit_bytes(bytes).await.map_err(object_rejected)?;
        }
        Ok(())
    }

    /// Wait until any admission owner cancels the read.
    pub async fn cancelled(&self) {
        cancellation(self.contexts.clone()).await;
    }

    /// Mark a returned object body as already accounted at its transport boundary.
    pub fn mark(&self, extensions: &mut object_store::Extensions) {
        extensions.insert(TransportReadAccounted);
    }
}

/// Admit one physical request when called inside a Store read operation.
pub async fn request() -> object_store::Result<Option<TransportReadReceipt>> {
    let contexts = contexts();
    if contexts.is_empty() {
        return Ok(None);
    }
    for context in &contexts {
        context.admit_request().await.map_err(object_rejected)?;
    }
    Ok(Some(TransportReadReceipt { contexts }))
}

async fn cancellation(contexts: Vec<Arc<TransportReadContext>>) {
    let mut pending = futures_util::stream::FuturesUnordered::new();
    for context in contexts {
        pending.push(context.admission.cancellation().clone().cancelled_owned());
    }
    let _ = pending.next().await;
}

fn rejected(source: Box<dyn std::error::Error + Send + Sync>) -> HttpError {
    HttpError::new(
        HttpErrorKind::Unknown,
        StorageError::ReadRejected { source },
    )
}

fn object_rejected(source: HttpError) -> object_store::Error {
    object_store::Error::NotSupported {
        source: Box::new(source),
    }
}

fn cancelled() -> HttpError {
    rejected(Box::new(StorageError::Cancelled))
}

/// Connector used by Crab-built cloud stores to expose physical HTTP reads.
#[derive(Debug, Default)]
pub(crate) struct ReadAdmissionConnector(ReqwestConnector);

impl HttpConnector for ReadAdmissionConnector {
    fn connect(&self, options: &ClientOptions) -> object_store::Result<HttpClient> {
        let client = self.0.connect(options)?;
        Ok(HttpClient::new(ReadAdmissionService(client)))
    }
}

#[derive(Debug)]
struct ReadAdmissionService(HttpClient);

#[async_trait::async_trait]
impl HttpService for ReadAdmissionService {
    async fn call(&self, mut request: HttpRequest) -> Result<HttpResponse, HttpError> {
        crate::s3_upload::condition_completion(&mut request);
        let contexts = contexts();
        if contexts.is_empty() {
            return self.0.execute(request).await;
        }
        for context in &contexts {
            context.admit_request().await?;
        }
        let response = tokio::select! {
            biased;
            () = cancellation(contexts.clone()) => return Err(cancelled()),
            response = self.0.execute(request) => response?,
        };
        let (mut parts, body) = response.into_parts();
        // The outer wrapper still verifies response framing, but this marker
        // prevents its header reservation from double-charging streamed bytes.
        parts.extensions.insert(TransportReadAccounted);
        let body = HttpResponseBody::new(AdmissionBody::new(body, contexts));
        Ok(HttpResponse::from_parts(parts, body))
    }
}

struct AdmissionBody {
    state: Mutex<AdmissionBodyState>,
}

struct AdmissionBodyState {
    inner: Pin<Box<HttpResponseBody>>,
    contexts: Vec<Arc<TransportReadContext>>,
    cancellation: futures_util::future::BoxFuture<'static, ()>,
    pending: Option<PendingFrame>,
}

type PendingFrame =
    futures_util::future::BoxFuture<'static, Result<Frame<bytes::Bytes>, HttpError>>;

impl AdmissionBody {
    fn new(inner: HttpResponseBody, contexts: Vec<Arc<TransportReadContext>>) -> Self {
        let cancellation = cancellation(contexts.clone()).boxed();
        Self {
            state: Mutex::new(AdmissionBodyState {
                inner: Box::pin(inner),
                contexts,
                cancellation,
                pending: None,
            }),
        }
    }
}

impl std::fmt::Debug for AdmissionBody {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AdmissionBody")
    }
}

impl Body for AdmissionBody {
    type Data = bytes::Bytes;
    type Error = HttpError;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            // A pending frame was already consumed from the transport. Drain
            // its observation before cancellation can discard that receipt.
            if let Some(mut pending) = state.pending.take() {
                match pending.poll_unpin(context) {
                    Poll::Ready(result) => return Poll::Ready(Some(result)),
                    Poll::Pending => {
                        state.pending = Some(pending);
                        return Poll::Pending;
                    }
                }
            }
            if state.cancellation.poll_unpin(context).is_ready() {
                return Poll::Ready(Some(Err(cancelled())));
            }
            match state.inner.as_mut().poll_frame(context) {
                Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                    Ok(bytes) => {
                        let contexts = state.contexts.clone();
                        state.pending = Some(
                            async move {
                                for context in &contexts {
                                    context.admission.received(bytes.len() as u64).await;
                                }
                                for context in contexts {
                                    context.admit_bytes(bytes.len() as u64).await?;
                                }
                                Ok(Frame::data(bytes))
                            }
                            .boxed(),
                        );
                    }
                    Err(frame) => return Poll::Ready(Some(Ok(frame))),
                },
                other => return other,
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.pending.is_none() && state.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.pending.is_some() {
            SizeHint::default()
        } else {
            state.inner.size_hint()
        }
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests;
