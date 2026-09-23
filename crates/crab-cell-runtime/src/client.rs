use std::{
    collections::HashMap,
    fmt,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use crab_ltx::rusqlite::OptionalExtension;
use tokio::sync::Notify;

use crate::cell::actor::CellHandle;
use crate::cell::catalog::CatalogRole;
use crate::cell::executor::{MutationIdentity, Resolution, StoredOutcome};
use crate::codec::{decode_wire, encode_wire};
use crate::identity::{CellId, CellTarget, Digest, IncarnationId, RequestId};
use crate::primitives::workflow::{ActivityContext, ActivityExecution, ActivitySupport};
use crate::registry::{
    Command, CommandInvocation, OperationDescriptor, Query, QueryInvocation, Registry,
};
use crate::{Error, Result};

const CELL_COMMAND_TAG: u16 = 10;
const MAX_STATE_STREAM_CHUNKS: usize = 1_024;

#[cfg(test)]
mod tests;

/// Immutable owner metadata used to fence a routed invocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellDescription {
    pub cell: CellId,
    pub incarnation: IncarnationId,
    pub code: Digest,
    pub schema: u32,
}

/// Durable observation position returned with every typed result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub cell: CellId,
    pub incarnation: IncarnationId,
    pub commit_sequence: u64,
}

/// Typed command result released only after authoritative publication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Committed<T> {
    pub output: T,
    pub receipt: Receipt,
}

/// Typed read result and the exact SQLite position it observed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observed<T> {
    pub output: T,
    pub receipt: Receipt,
}

/// Cancellation capability for one state-observing Cell stream.
#[derive(Clone)]
pub struct StateStreamCancellation {
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    notify: Arc<Notify>,
}

impl StateStreamCancellation {
    /// Cancels the stream and releases any pending output wait.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Returns whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Serial, watermark-bound output capability for state-observing streams.
///
/// Each call to [`Self::emit`] performs one actor-ordered query at or beyond
/// the previous receipt. The mutable borrow prevents two chunks from being
/// released out of order; the deadline and cancellation capability bound the
/// retained stream state. Dropping the stream is equivalent to cancellation.
pub struct CellStateStream<Q: Query> {
    client: CellClient,
    target: CellTarget,
    expected: CellDescription,
    stream_id: RequestId,
    minimum: Option<Receipt>,
    deadline: Instant,
    chunks: usize,
    closed: bool,
    cancellation: StateStreamCancellation,
    marker: std::marker::PhantomData<fn() -> Q>,
}

impl<Q: Query> CellStateStream<Q> {
    /// Returns the opaque stream identity used for lifecycle and telemetry.
    #[must_use]
    pub const fn id(&self) -> RequestId {
        self.stream_id
    }

    /// Returns the latest proven observation, if the stream emitted a chunk.
    #[must_use]
    pub const fn last_receipt(&self) -> Option<Receipt> {
        self.minimum
    }

    /// Returns a capability that cancels this stream from another task.
    #[must_use]
    pub fn cancellation(&self) -> StateStreamCancellation {
        self.cancellation.clone()
    }

    /// Emits one state-observing chunk after its watermark is proven.
    pub async fn emit(
        &mut self,
        input: Q::Input,
    ) -> std::result::Result<Observed<Q::Output>, InvocationError<Q::Output>> {
        if self.closed || self.cancellation.is_cancelled() {
            self.closed = true;
            return Err(stream_error(Error::StreamCancelled));
        }
        if self.chunks >= MAX_STATE_STREAM_CHUNKS {
            self.closed = true;
            return Err(stream_error(Error::Capacity("state stream chunks")));
        }
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| {
                self.closed = true;
                stream_error(Error::Deadline)
            })?;
        let query = self.client.query_with_description::<Q>(
            &self.target,
            self.expected,
            self.minimum,
            input,
        );
        tokio::pin!(query);
        let notified = self.cancellation.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.cancellation.is_cancelled() {
            self.closed = true;
            return Err(stream_error(Error::StreamCancelled));
        }
        let result = match tokio::select! {
            result = &mut query => result,
            () = &mut notified => {
                self.closed = true;
                Err(stream_error(Error::StreamCancelled))
            }
            () = tokio::time::sleep(remaining) => {
                self.closed = true;
                Err(stream_error(Error::Deadline))
            }
        } {
            Ok(result) => result,
            Err(error) => {
                self.closed = true;
                return Err(error);
            }
        };
        if self.cancellation.is_cancelled() {
            self.closed = true;
            return Err(stream_error(Error::StreamCancelled));
        }
        if Instant::now() >= self.deadline {
            self.closed = true;
            return Err(stream_error(Error::Deadline));
        }
        let receipt = result.receipt;
        if self
            .minimum
            .is_some_and(|minimum| receipt.commit_sequence < minimum.commit_sequence)
        {
            self.closed = true;
            return Err(stream_error(Error::Command(
                "state stream watermark moved backwards",
            )));
        }
        self.minimum = Some(receipt);
        self.chunks = self.chunks.saturating_add(1);
        Ok(result)
    }

    /// Closes the stream. No later chunk can be emitted.
    pub fn finish(&mut self) {
        self.closed = true;
        self.cancellation.cancel();
    }

    /// Returns whether the stream is closed or cancelled.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed || self.cancellation.is_cancelled()
    }
}

impl<Q: Query> Drop for CellStateStream<Q> {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

fn stream_error<T>(error: Error) -> InvocationError<T> {
    InvocationError::NotStarted(error)
}

/// Stable mutation evidence retained when acceptance cannot be resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingMutation {
    target: CellTarget,
    incarnation: IncarnationId,
    identity: MutationIdentity,
    operation_digest: Digest,
    max_result_bytes: usize,
}

impl PendingMutation {
    #[must_use]
    pub const fn identity(&self) -> MutationIdentity {
        self.identity
    }

    #[must_use]
    pub const fn operation_digest(&self) -> Digest {
        self.operation_digest
    }

    #[must_use]
    pub const fn incarnation(&self) -> IncarnationId {
        self.incarnation
    }

    #[must_use]
    pub const fn target(&self) -> &CellTarget {
        &self.target
    }
}

/// Typed command whose exact request evidence survives cancellation of execution.
///
/// Clone before dispatch if the caller may be cancelled; only retry the clone
/// after resolving its evidence as absent.
#[must_use]
pub struct PreparedCommand<C: Command> {
    client: CellClient,
    request: EncodedCommand,
    evidence: PendingMutation,
    marker: PhantomData<fn() -> C>,
}

impl<C: Command> Clone for PreparedCommand<C> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            request: self.request.clone(),
            evidence: self.evidence.clone(),
            marker: PhantomData,
        }
    }
}

impl<C: Command> PreparedCommand<C> {
    /// Returns the exact request evidence to resolve after execution is cancelled or ambiguous.
    #[must_use]
    pub const fn evidence(&self) -> &PendingMutation {
        &self.evidence
    }

    /// Executes the prepared request once against its validated owner incarnation.
    ///
    /// Returns pending evidence when acceptance is unknown and rejects an expired identity.
    pub async fn execute(
        mut self,
    ) -> std::result::Result<Committed<C::Output>, InvocationError<C::Output>> {
        let now_ms = unix_time_ms().map_err(InvocationError::NotStarted)?;
        self.evidence
            .identity
            .validate(now_ms)
            .map_err(InvocationError::NotStarted)?;
        self.request.now_ms = now_ms;
        match self.client.transport.command(self.request).await {
            Ok(outcome) => decode_pending::<C::Output>(&self.evidence, outcome),
            Err(Error::OutcomeUnknown {
                request_id,
                operation_digest,
                ..
            }) if request_id == self.evidence.identity.request_id
                && operation_digest == self.evidence.operation_digest =>
            {
                Err(InvocationError::Pending(Box::new(self.evidence)))
            }
            Err(error) => Err(InvocationError::NotStarted(error)),
        }
    }
}

/// Outcome-aware typed invocation failure.
pub enum InvocationError<T> {
    Rejected(Box<Committed<T>>),
    Pending(Box<PendingMutation>),
    InvalidPublishedResult {
        receipt: Receipt,
        source: Box<Error>,
    },
    NotStarted(Error),
}

impl<T> fmt::Debug for InvocationError<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(_) => formatter.write_str("InvocationError::Rejected(..)"),
            Self::Pending(pending) => formatter
                .debug_tuple("InvocationError::Pending")
                .field(pending)
                .finish(),
            Self::InvalidPublishedResult { receipt, source } => formatter
                .debug_struct("InvocationError::InvalidPublishedResult")
                .field("receipt", receipt)
                .field("source", source)
                .finish(),
            Self::NotStarted(error) => formatter
                .debug_tuple("InvocationError::NotStarted")
                .field(error)
                .finish(),
        }
    }
}

impl<T> fmt::Display for InvocationError<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(_) => formatter.write_str("Cell command was durably rejected"),
            Self::Pending(_) => formatter.write_str("Cell command outcome requires resolution"),
            Self::InvalidPublishedResult { .. } => {
                formatter.write_str("Cell command committed an invalid typed result")
            }
            Self::NotStarted(error) => write!(formatter, "Cell invocation did not start: {error}"),
        }
    }
}

impl<T> std::error::Error for InvocationError<T> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidPublishedResult { source, .. } => Some(source.as_ref()),
            Self::NotStarted(error) => Some(error),
            Self::Rejected(_) | Self::Pending(_) => None,
        }
    }
}

/// Owned encoded command accepted by a local or authenticated peer transport.
#[derive(Clone)]
pub(super) struct EncodedCommand {
    pub(super) target: CellTarget,
    pub(super) expected: CellDescription,
    pub(super) identity: MutationIdentity,
    pub(super) operation_digest: Digest,
    pub(super) now_ms: i64,
    pub(super) module: &'static str,
    pub(super) operation_id: u32,
    pub(super) codec_version: u32,
    pub(super) input: Vec<u8>,
    pub(super) input_limit: u32,
    pub(super) output_limit: u32,
}

/// Owned encoded query accepted by a local or authenticated peer transport.
pub(super) struct EncodedQuery {
    pub(super) target: CellTarget,
    pub(super) expected: CellDescription,
    pub(super) minimum: Option<Receipt>,
    pub(super) now_ms: i64,
    pub(super) module: &'static str,
    pub(super) operation_id: u32,
    pub(super) codec_version: u32,
    pub(super) input: Vec<u8>,
    pub(super) input_limit: u32,
    pub(super) output_limit: u32,
}

/// Owned request-ledger lookup accepted by a routed transport.
pub(super) struct EncodedResolve {
    pub(super) target: CellTarget,
    pub(super) expected: CellDescription,
    pub(super) identity: MutationIdentity,
    pub(super) operation_digest: Digest,
    pub(super) now_ms: i64,
    pub(super) max_result_bytes: usize,
}

/// Encoded query output carrying the owner-observed commit position.
pub(super) struct EncodedObservation {
    pub output: Vec<u8>,
    pub receipt: Receipt,
}

/// Internal routing boundary shared by local actors and the private peer client.
pub(super) trait CellTransport: Send + Sync + 'static {
    fn describe(
        &self,
        target: CellTarget,
    ) -> Pin<Box<dyn Future<Output = Result<CellDescription>> + Send + 'static>>;

    fn command(
        &self,
        command: EncodedCommand,
    ) -> Pin<Box<dyn Future<Output = Result<StoredOutcome>> + Send + 'static>>;

    fn query(
        &self,
        query: EncodedQuery,
    ) -> Pin<Box<dyn Future<Output = Result<EncodedObservation>> + Send + 'static>>;

    fn resolve(
        &self,
        resolve: EncodedResolve,
    ) -> Pin<Box<dyn Future<Output = Result<Resolution>> + Send + 'static>>;
}

/// Cloneable typed application capability over one routing implementation.
#[derive(Clone)]
pub struct CellClient {
    registry: Arc<Registry>,
    transport: Arc<dyn CellTransport>,
    blob_artifact_store: Option<crate::BlobArtifactStore>,
}

impl CellClient {
    #[must_use]
    fn new(registry: Arc<Registry>, transport: Arc<dyn CellTransport>) -> Self {
        Self {
            registry,
            transport,
            blob_artifact_store: None,
        }
    }

    /// Returns a client clone wired to the configured object-store Blob data.
    #[must_use]
    pub fn with_blob_artifact_store(&self, store: crate::BlobArtifactStore) -> Self {
        let mut client = self.clone();
        client.blob_artifact_store = Some(store);
        client
    }

    pub(crate) fn blob_artifact_store(&self) -> Option<crate::BlobArtifactStore> {
        self.blob_artifact_store.clone()
    }

    /// Builds the canonical single-owner transport used by embedded routes.
    #[must_use]
    pub fn local(registry: Arc<Registry>, handle: CellHandle) -> Self {
        let primary = handle.clone();
        let transport = Arc::new(LocalCellTransport {
            registry: registry.clone(),
            handles: Arc::new(HashMap::from([(handle.cell_id(), handle)])),
            handle: primary,
        });
        Self::new(registry, transport)
    }

    /// Builds a bounded in-process transport for a set of locally owned Cells.
    ///
    /// This is the qualification and single-process embedding path. Production
    /// multi-node routing still uses [`Self::peer`], while every target remains
    /// checked against the exact local Cell identity before execution.
    pub fn local_many(
        registry: Arc<Registry>,
        handles: impl IntoIterator<Item = CellHandle>,
    ) -> Result<Self> {
        let mut local = HashMap::new();
        for handle in handles {
            if local.insert(handle.cell_id(), handle).is_some() {
                return Err(Error::Identity("duplicate local Cell handle"));
            }
        }
        if local.is_empty() {
            return Err(Error::Identity("local transport has no Cell handles"));
        }
        let primary = local
            .values()
            .next()
            .cloned()
            .ok_or(Error::Identity("local transport has no Cell handles"))?;
        Ok(Self::new(
            registry.clone(),
            Arc::new(LocalCellTransport {
                registry,
                handles: Arc::new(local),
                handle: primary,
            }),
        ))
    }

    /// Builds a typed capability over authenticated private peer routing.
    #[must_use]
    pub fn peer(
        registry: Arc<Registry>,
        signer: Arc<crate::peer::PeerSigner>,
        principal: crate::peer::PeerPrincipal,
        round_trip: Arc<dyn crate::peer::PeerRoundTrip>,
    ) -> Self {
        let transport = Arc::new(crate::peer::PeerClientTransport::new(
            signer, principal, round_trip,
        ));
        Self::new(registry, transport)
    }

    /// Opens a bounded, serial state-observing stream for one Cell target.
    ///
    /// The first emitted chunk establishes the response watermark. Every later
    /// chunk is queried at or beyond the prior receipt and is therefore gated
    /// by the same actor durability proof before it can be returned.
    #[must_use = "await the stream setup result"]
    pub async fn open_state_stream<Q: Query>(
        &self,
        target: &CellTarget,
        deadline: Instant,
    ) -> std::result::Result<CellStateStream<Q>, InvocationError<Q::Output>> {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| InvocationError::NotStarted(Error::Deadline))?;
        let expected = tokio::time::timeout(remaining, self.describe::<Q::Output>(target))
            .await
            .map_err(|_| InvocationError::NotStarted(Error::Deadline))??;
        Ok(CellStateStream {
            client: self.clone(),
            target: target.clone(),
            expected,
            stream_id: RequestId::from_bytes(rand::random()),
            minimum: None,
            deadline,
            chunks: 0,
            closed: false,
            cancellation: StateStreamCancellation {
                cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                notify: Arc::new(Notify::new()),
            },
            marker: std::marker::PhantomData,
        })
    }

    pub(crate) fn require_namespace(
        &self,
        namespace: crate::NamespaceId,
        module: &'static str,
        role: CatalogRole,
    ) -> Result<u32> {
        let Some((owner, descriptor)) = self.registry.namespace_contract(namespace) else {
            return Err(Error::Registry("namespace is not registered"));
        };
        if owner != module || descriptor.role != role {
            return Err(Error::Registry(
                "namespace module or role does not match primitive",
            ));
        }
        Ok(descriptor.shards)
    }

    /// Builds a typed source-effect capability after validating the compiled
    /// module and its source namespace. This keeps effect operation IDs inside
    /// the registry instead of allowing callers to construct arbitrary ones.
    pub fn effect_source<M: crate::EffectModule>(
        &self,
        target: CellTarget,
    ) -> Result<crate::EffectSource<M>> {
        let Some((module, _)) = self.registry.namespace_contract(target.namespace()) else {
            return Err(Error::Registry("effect source namespace is not registered"));
        };
        if module != M::MODULE || !self.registry.has_effect_runner(target.namespace()) {
            return Err(Error::Registry("effect source module is not registered"));
        }
        Ok(crate::EffectSource::new(self.clone(), target))
    }

    pub(crate) fn activity_support(
        &self,
        module: &'static str,
        definition: Digest,
    ) -> Result<Vec<ActivitySupport>> {
        self.registry.activity_support(module, definition)
    }

    pub(crate) async fn execute_activity(
        &self,
        module: &'static str,
        definition: Digest,
        activity: &str,
        context: ActivityContext,
        input: Vec<u8>,
        blocking: Option<crate::primitives::activity_pool::BlockingActivityReservation>,
    ) -> Result<ActivityExecution> {
        self.registry
            .execute_activity(module, definition, activity, context, input, blocking)
            .await
    }

    /// Executes one typed command with a digest derived from validated values.
    pub async fn command<C: Command>(
        &self,
        target: &CellTarget,
        identity: MutationIdentity,
        input: C::Input,
    ) -> std::result::Result<Committed<C::Output>, InvocationError<C::Output>> {
        self.prepare_command::<C>(target, identity, input)
            .await?
            .execute()
            .await
    }

    /// Prepares one exact typed command so its evidence survives cancellation during dispatch.
    ///
    /// Validates the owner contract and bounded input without dispatching a mutation.
    pub async fn prepare_command<C: Command>(
        &self,
        target: &CellTarget,
        identity: MutationIdentity,
        input: C::Input,
    ) -> std::result::Result<PreparedCommand<C>, InvocationError<C::Output>> {
        let now_ms = unix_time_ms().map_err(InvocationError::NotStarted)?;
        identity
            .validate(now_ms)
            .map_err(InvocationError::NotStarted)?;
        let description = self.describe::<C::Output>(target).await?;
        let operation = self
            .registry
            .command_contract::<C>(target.namespace())
            .map_err(InvocationError::NotStarted)?;
        validate_description(&self.registry, C::MODULE, description, operation)
            .map_err(InvocationError::NotStarted)?;
        let input = encode_wire(&input, operation.input_limit)
            .map_err(Error::from)
            .map_err(InvocationError::NotStarted)?;
        let digest = command_operation_digest::<C>(description, identity, &input)
            .map_err(InvocationError::NotStarted)?;
        let request = EncodedCommand {
            target: target.clone(),
            expected: description,
            identity,
            operation_digest: digest,
            now_ms,
            module: C::MODULE,
            operation_id: C::ID,
            codec_version: C::CODEC_VERSION,
            input,
            input_limit: operation.input_limit,
            output_limit: operation.output_limit,
        };
        Ok(PreparedCommand {
            client: self.clone(),
            request,
            evidence: PendingMutation {
                target: target.clone(),
                incarnation: description.incarnation,
                identity,
                operation_digest: digest,
                max_result_bytes: operation.output_limit as usize,
            },
            marker: PhantomData,
        })
    }

    /// Runs one typed FIFO read at or beyond an optional receipt.
    pub async fn query<Q: Query>(
        &self,
        target: &CellTarget,
        minimum: Option<Receipt>,
        input: Q::Input,
    ) -> std::result::Result<Observed<Q::Output>, InvocationError<Q::Output>> {
        let description = self.describe::<Q::Output>(target).await?;
        self.query_with_description::<Q>(target, description, minimum, input)
            .await
    }

    async fn query_with_description<Q: Query>(
        &self,
        target: &CellTarget,
        description: CellDescription,
        minimum: Option<Receipt>,
        input: Q::Input,
    ) -> std::result::Result<Observed<Q::Output>, InvocationError<Q::Output>> {
        let now_ms = unix_time_ms().map_err(InvocationError::NotStarted)?;
        let operation = self
            .registry
            .query_contract::<Q>(target.namespace())
            .map_err(InvocationError::NotStarted)?;
        validate_description(&self.registry, Q::MODULE, description, operation)
            .map_err(InvocationError::NotStarted)?;
        validate_minimum(description, minimum).map_err(InvocationError::NotStarted)?;
        let input = encode_wire(&input, operation.input_limit)
            .map_err(Error::from)
            .map_err(InvocationError::NotStarted)?;
        let observation = self
            .transport
            .query(EncodedQuery {
                target: target.clone(),
                expected: description,
                minimum,
                now_ms,
                module: Q::MODULE,
                operation_id: Q::ID,
                codec_version: Q::CODEC_VERSION,
                input,
                input_limit: operation.input_limit,
                output_limit: operation.output_limit,
            })
            .await
            .map_err(InvocationError::NotStarted)?;
        if observation.receipt.cell != description.cell
            || observation.receipt.incarnation != description.incarnation
            || minimum.is_some_and(|minimum| {
                observation.receipt.commit_sequence < minimum.commit_sequence
            })
        {
            return Err(InvocationError::NotStarted(Error::Command(
                "query did not satisfy minimum receipt",
            )));
        }
        Ok(Observed {
            output: decode_output(&observation.output, operation.output_limit)?,
            receipt: observation.receipt,
        })
    }

    /// Resolves a pending mutation without executing its handler again.
    pub async fn resolve(
        &self,
        pending: &PendingMutation,
    ) -> std::result::Result<Resolution, InvocationError<Vec<u8>>> {
        let description = self.describe::<Vec<u8>>(pending.target()).await?;
        if description.incarnation != pending.incarnation {
            return Err(InvocationError::NotStarted(Error::Command(
                "pending mutation incarnation changed",
            )));
        }
        let now_ms = unix_time_ms().map_err(InvocationError::NotStarted)?;
        self.transport
            .resolve(EncodedResolve {
                target: pending.target.clone(),
                expected: description,
                identity: pending.identity,
                operation_digest: pending.operation_digest,
                now_ms,
                max_result_bytes: pending.max_result_bytes,
            })
            .await
            .map_err(InvocationError::NotStarted)
    }

    async fn describe<T>(
        &self,
        target: &CellTarget,
    ) -> std::result::Result<CellDescription, InvocationError<T>> {
        let description = self
            .transport
            .describe(target.clone())
            .await
            .map_err(InvocationError::NotStarted)?;
        if description.cell != target.cell_id() {
            return Err(InvocationError::NotStarted(Error::Command(
                "transport described a different Cell",
            )));
        }
        Ok(description)
    }
}

pub(super) struct LocalCellTransport {
    pub(super) registry: Arc<Registry>,
    pub(super) handles: Arc<HashMap<CellId, CellHandle>>,
    pub(super) handle: CellHandle,
}

impl CellTransport for LocalCellTransport {
    fn describe(
        &self,
        target: CellTarget,
    ) -> Pin<Box<dyn Future<Output = Result<CellDescription>> + Send + 'static>> {
        let handles = Arc::clone(&self.handles);
        Box::pin(async move {
            let handle = local_handle(&handles, &target)?;
            validate_local_target(&handle, &target)?;
            Ok(local_description(&handle))
        })
    }

    fn command(
        &self,
        command: EncodedCommand,
    ) -> Pin<Box<dyn Future<Output = Result<StoredOutcome>> + Send + 'static>> {
        let registry = self.registry.clone();
        let handles = Arc::clone(&self.handles);
        Box::pin(async move {
            let handle = local_handle(&handles, &command.target)?;
            validate_local_target(&handle, &command.target)?;
            validate_expected(&handle, command.expected)?;
            if command.input.len() > command.input_limit as usize {
                return Err(Error::Command("encoded command input exceeds limit"));
            }
            let schema = handle.schema();
            let input_bytes = command.input.len();
            let output_limit = command.output_limit as usize;
            handle
                .execute(
                    command.identity,
                    command.operation_digest,
                    command.now_ms,
                    input_bytes,
                    output_limit,
                    move |transaction| {
                        let sequence = next_sequence(transaction)?;
                        registry.execute_command_with_issue_time(
                            transaction,
                            CommandInvocation {
                                module: command.module,
                                operation_id: command.operation_id,
                                codec_version: command.codec_version,
                                schema,
                                target: command.target.clone(),
                                sequence,
                                now_ms: command.now_ms,
                                input: &command.input,
                            },
                            command.identity.issued_at_ms,
                        )
                    },
                )
                .await
        })
    }

    fn query(
        &self,
        query: EncodedQuery,
    ) -> Pin<Box<dyn Future<Output = Result<EncodedObservation>> + Send + 'static>> {
        let registry = self.registry.clone();
        let handles = Arc::clone(&self.handles);
        Box::pin(async move {
            let handle = local_handle(&handles, &query.target)?;
            validate_local_target(&handle, &query.target)?;
            validate_expected(&handle, query.expected)?;
            validate_minimum(query.expected, query.minimum)?;
            if query.input.len() > query.input_limit as usize {
                return Err(Error::Command("encoded query input exceeds limit"));
            }
            let sequence = Arc::new(AtomicU64::new(0));
            let observed_sequence = sequence.clone();
            let cell = handle.cell_id();
            let schema = handle.schema();
            let input_bytes = query.input.len();
            let output_limit = query.output_limit as usize;
            let output = handle
                .query(input_bytes, output_limit, move |connection| {
                    let commit_sequence = current_sequence(connection)?;
                    observed_sequence.store(commit_sequence, Ordering::Release);
                    registry.execute_query(
                        connection,
                        QueryInvocation {
                            module: query.module,
                            operation_id: query.operation_id,
                            codec_version: query.codec_version,
                            schema,
                            cell,
                            commit_sequence,
                            now_ms: query.now_ms,
                            input: &query.input,
                        },
                    )
                })
                .await?;
            let commit_sequence = sequence.load(Ordering::Acquire);
            if query
                .minimum
                .is_some_and(|minimum| commit_sequence < minimum.commit_sequence)
            {
                return Err(Error::Command("query did not satisfy minimum receipt"));
            }
            Ok(EncodedObservation {
                output,
                receipt: receipt(query.expected, commit_sequence),
            })
        })
    }

    fn resolve(
        &self,
        resolve: EncodedResolve,
    ) -> Pin<Box<dyn Future<Output = Result<Resolution>> + Send + 'static>> {
        let handles = Arc::clone(&self.handles);
        Box::pin(async move {
            let handle = local_handle(&handles, &resolve.target)?;
            validate_local_target(&handle, &resolve.target)?;
            validate_expected(&handle, resolve.expected)?;
            handle
                .resolve(
                    resolve.identity,
                    resolve.operation_digest,
                    resolve.now_ms,
                    resolve.max_result_bytes,
                )
                .await
        })
    }
}

fn local_handle(handles: &HashMap<CellId, CellHandle>, target: &CellTarget) -> Result<CellHandle> {
    handles
        .get(&target.cell_id())
        .cloned()
        .ok_or(Error::Control("target Cell is not locally owned"))
}

/// Computes a typed command digest from validated metadata and encoded input.
pub fn command_operation_digest<C: Command>(
    description: CellDescription,
    identity: MutationIdentity,
    input: &[u8],
) -> Result<Digest> {
    encoded_command_operation_digest(description, identity, C::ID, C::CODEC_VERSION, input)
}

pub(super) fn encoded_command_operation_digest(
    description: CellDescription,
    identity: MutationIdentity,
    operation_id: u32,
    codec_version: u32,
    input: &[u8],
) -> Result<Digest> {
    let input_len = u32::try_from(input.len())
        .map_err(|_| Error::Command("command input exceeds canonical digest range"))?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.op.v1\0");
    hasher.update(description.cell.as_bytes());
    hasher.update(description.incarnation.as_bytes());
    hasher.update(identity.request_id.as_bytes());
    hasher.update(&identity.issued_at_ms.to_be_bytes());
    hasher.update(&identity.expires_at_ms.to_be_bytes());
    hasher.update(&CELL_COMMAND_TAG.to_be_bytes());
    hasher.update(&operation_id.to_be_bytes());
    hasher.update(&codec_version.to_be_bytes());
    hasher.update(&input_len.to_be_bytes());
    hasher.update(input);
    Ok(Digest::from_bytes(*hasher.finalize().as_bytes()))
}

fn decode_output<T: crate::codec::WireValue>(
    result: &[u8],
    limit: u32,
) -> std::result::Result<T, InvocationError<T>> {
    decode_wire(result, limit)
        .map_err(Error::from)
        .map_err(InvocationError::NotStarted)
}

fn decode_committed<T: crate::codec::WireValue>(
    result: &[u8],
    limit: u32,
    receipt: Receipt,
) -> std::result::Result<Committed<T>, InvocationError<T>> {
    let output =
        decode_wire(result, limit).map_err(|error| InvocationError::InvalidPublishedResult {
            receipt,
            source: Box::new(Error::from(error)),
        })?;
    Ok(Committed { output, receipt })
}

pub(crate) fn decode_pending<T: crate::codec::WireValue>(
    pending: &PendingMutation,
    outcome: StoredOutcome,
) -> std::result::Result<Committed<T>, InvocationError<T>> {
    let limit = u32::try_from(pending.max_result_bytes).map_err(|_| {
        InvocationError::NotStarted(Error::Command("pending result limit overflow"))
    })?;
    let decode = |result: Vec<u8>, commit_sequence| {
        decode_committed(
            &result,
            limit,
            Receipt {
                cell: pending.target.cell_id(),
                incarnation: pending.incarnation,
                commit_sequence,
            },
        )
    };
    match outcome {
        StoredOutcome::Success {
            result,
            commit_sequence,
        } => decode(result, commit_sequence),
        StoredOutcome::Rejected {
            result,
            commit_sequence,
        } => Err(InvocationError::Rejected(Box::new(decode(
            result,
            commit_sequence,
        )?))),
    }
}

fn validate_description(
    registry: &Registry,
    module: &str,
    description: CellDescription,
    operation: OperationDescriptor,
) -> Result<()> {
    if !registry.supports_module_code(module, description.code, description.schema) {
        return Err(Error::Command("Cell code does not match operation module"));
    }
    if !(operation.schema_min..=operation.schema_max).contains(&description.schema) {
        return Err(Error::Command(
            "registered operation does not support Cell schema",
        ));
    }
    Ok(())
}

fn validate_minimum(description: CellDescription, minimum: Option<Receipt>) -> Result<()> {
    if minimum.is_some_and(|minimum| {
        minimum.cell != description.cell || minimum.incarnation != description.incarnation
    }) {
        return Err(Error::Command("minimum receipt does not match Cell"));
    }
    Ok(())
}

fn validate_local_target(handle: &CellHandle, target: &CellTarget) -> Result<()> {
    let entry = handle.catalog().entry();
    if target.cell_id() != handle.cell_id()
        || target.namespace() != entry.namespace()
        || target.partition() != entry.partition()
    {
        return Err(Error::Command("target does not match active Cell"));
    }
    Ok(())
}

fn validate_expected(handle: &CellHandle, expected: CellDescription) -> Result<()> {
    if local_description(handle) != expected {
        return Err(Error::Fenced);
    }
    Ok(())
}

pub(super) fn local_description(handle: &CellHandle) -> CellDescription {
    CellDescription {
        cell: handle.cell_id(),
        incarnation: handle.incarnation(),
        code: handle.code(),
        schema: handle.schema(),
    }
}

fn next_sequence(transaction: &crab_ltx::rusqlite::Transaction<'_>) -> Result<u64> {
    current_sequence(transaction)?
        .checked_add(1)
        .ok_or(Error::Command("commit sequence overflow"))
}

fn current_sequence(connection: &crab_ltx::rusqlite::Connection) -> Result<u64> {
    let sequence = connection
        .query_row(
            "SELECT commit_sequence FROM sys_meta WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .ok_or(Error::Command("runtime metadata row missing"))?;
    u64::try_from(sequence).map_err(|_| Error::Command("invalid commit sequence"))
}

pub(super) fn receipt(description: CellDescription, commit_sequence: u64) -> Receipt {
    Receipt {
        cell: description.cell,
        incarnation: description.incarnation,
        commit_sequence,
    }
}

fn unix_time_ms() -> Result<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Command("system clock precedes Unix epoch"))?;
    i64::try_from(duration.as_millis()).map_err(|_| Error::Command("system clock overflow"))
}
