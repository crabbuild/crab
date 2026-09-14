use std::{
    fmt,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use crab_ltx::rusqlite::OptionalExtension;

use crate::{
    ActivityContext, ActivityExecution, ActivitySupport, CatalogRole, CellHandle, CellId,
    CellTarget, Command, CommandInvocation, Digest, Error, IncarnationId, MutationIdentity,
    OperationDescriptor, Query, QueryInvocation, Registry, Resolution, Result, StoredOutcome,
    codec::{decode_wire, encode_wire},
};

const CELL_COMMAND_TAG: u16 = 10;

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

/// Outcome-aware typed invocation failure.
pub enum InvocationError<T> {
    Rejected(Committed<T>),
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
struct EncodedCommand {
    target: CellTarget,
    expected: CellDescription,
    identity: MutationIdentity,
    operation_digest: Digest,
    now_ms: i64,
    module: &'static str,
    operation_id: u32,
    codec_version: u32,
    input: Vec<u8>,
    input_limit: u32,
    output_limit: u32,
}

/// Owned encoded query accepted by a local or authenticated peer transport.
struct EncodedQuery {
    target: CellTarget,
    expected: CellDescription,
    minimum: Option<Receipt>,
    now_ms: i64,
    module: &'static str,
    operation_id: u32,
    codec_version: u32,
    input: Vec<u8>,
    input_limit: u32,
    output_limit: u32,
}

/// Owned request-ledger lookup accepted by a routed transport.
struct EncodedResolve {
    target: CellTarget,
    expected: CellDescription,
    identity: MutationIdentity,
    operation_digest: Digest,
    now_ms: i64,
    max_result_bytes: usize,
}

/// Encoded query output carrying the owner-observed commit position.
struct EncodedObservation {
    pub output: Vec<u8>,
    pub receipt: Receipt,
}

/// Internal routing boundary shared by local actors and the private peer client.
trait CellTransport: Send + Sync + 'static {
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
}

impl CellClient {
    #[must_use]
    fn new(registry: Arc<Registry>, transport: Arc<dyn CellTransport>) -> Self {
        Self {
            registry,
            transport,
        }
    }

    /// Builds the canonical single-owner transport used by embedded routes.
    #[must_use]
    pub fn local(registry: Arc<Registry>, handle: CellHandle) -> Self {
        let transport = Arc::new(LocalCellTransport {
            registry: registry.clone(),
            handle,
        });
        Self::new(registry, transport)
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
    ) -> Result<ActivityExecution> {
        self.registry
            .execute_activity(module, definition, activity, context, input)
            .await
    }

    /// Executes one typed command with a digest derived from validated values.
    pub async fn command<C: Command>(
        &self,
        target: &CellTarget,
        identity: MutationIdentity,
        input: C::Input,
    ) -> std::result::Result<Committed<C::Output>, InvocationError<C::Output>> {
        let now_ms = unix_time_ms().map_err(InvocationError::NotStarted)?;
        identity
            .validate(now_ms)
            .map_err(InvocationError::NotStarted)?;
        let description = self.describe::<C::Output>(target).await?;
        let (operation, code) = self
            .registry
            .command_contract::<C>(target.namespace())
            .map_err(InvocationError::NotStarted)?;
        validate_description(description, code, operation).map_err(InvocationError::NotStarted)?;
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
        match self.transport.command(request).await {
            Ok(StoredOutcome::Success {
                result,
                commit_sequence,
            }) => decode_committed(
                &result,
                operation.output_limit,
                receipt(description, commit_sequence),
            ),
            Ok(StoredOutcome::Rejected {
                result,
                commit_sequence,
            }) => Err(InvocationError::Rejected(decode_committed(
                &result,
                operation.output_limit,
                receipt(description, commit_sequence),
            )?)),
            Err(Error::OutcomeUnknown {
                request_id,
                operation_digest,
                ..
            }) if request_id == identity.request_id && operation_digest == digest => {
                Err(InvocationError::Pending(Box::new(PendingMutation {
                    target: target.clone(),
                    incarnation: description.incarnation,
                    identity,
                    operation_digest: digest,
                    max_result_bytes: operation.output_limit as usize,
                })))
            }
            Err(error) => Err(InvocationError::NotStarted(error)),
        }
    }

    /// Runs one typed FIFO read at or beyond an optional receipt.
    pub async fn query<Q: Query>(
        &self,
        target: &CellTarget,
        minimum: Option<Receipt>,
        input: Q::Input,
    ) -> std::result::Result<Observed<Q::Output>, InvocationError<Q::Output>> {
        let now_ms = unix_time_ms().map_err(InvocationError::NotStarted)?;
        let description = self.describe::<Q::Output>(target).await?;
        let (operation, code) = self
            .registry
            .query_contract::<Q>(target.namespace())
            .map_err(InvocationError::NotStarted)?;
        validate_description(description, code, operation).map_err(InvocationError::NotStarted)?;
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

struct LocalCellTransport {
    registry: Arc<Registry>,
    handle: CellHandle,
}

impl CellTransport for LocalCellTransport {
    fn describe(
        &self,
        target: CellTarget,
    ) -> Pin<Box<dyn Future<Output = Result<CellDescription>> + Send + 'static>> {
        let handle = self.handle.clone();
        Box::pin(async move {
            validate_local_target(&handle, &target)?;
            Ok(local_description(&handle))
        })
    }

    fn command(
        &self,
        command: EncodedCommand,
    ) -> Pin<Box<dyn Future<Output = Result<StoredOutcome>> + Send + 'static>> {
        let registry = self.registry.clone();
        let handle = self.handle.clone();
        Box::pin(async move {
            validate_local_target(&handle, &command.target)?;
            validate_expected(&handle, command.expected)?;
            if command.input.len() > command.input_limit as usize {
                return Err(Error::Command("encoded command input exceeds limit"));
            }
            let cell = handle.cell_id();
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
                        registry.execute_command(
                            transaction,
                            CommandInvocation {
                                module: command.module,
                                operation_id: command.operation_id,
                                codec_version: command.codec_version,
                                schema,
                                cell,
                                sequence,
                                now_ms: command.now_ms,
                                input: &command.input,
                            },
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
        let handle = self.handle.clone();
        Box::pin(async move {
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
        let handle = self.handle.clone();
        Box::pin(async move {
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

/// Computes a typed command digest from validated metadata and encoded input.
pub fn command_operation_digest<C: Command>(
    description: CellDescription,
    identity: MutationIdentity,
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
    hasher.update(&C::ID.to_be_bytes());
    hasher.update(&C::CODEC_VERSION.to_be_bytes());
    hasher.update(&input_len.to_be_bytes());
    hasher.update(input);
    Ok(Digest::from_bytes(*hasher.finalize().as_bytes()))
}

fn decode_output<T: crate::WireValue>(
    result: &[u8],
    limit: u32,
) -> std::result::Result<T, InvocationError<T>> {
    decode_wire(result, limit)
        .map_err(Error::from)
        .map_err(InvocationError::NotStarted)
}

fn decode_committed<T: crate::WireValue>(
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

fn validate_description(
    description: CellDescription,
    code: Digest,
    operation: OperationDescriptor,
) -> Result<()> {
    if description.code != code {
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

fn local_description(handle: &CellHandle) -> CellDescription {
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

fn receipt(description: CellDescription, commit_sequence: u64) -> Receipt {
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
