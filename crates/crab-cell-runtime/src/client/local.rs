//! The in-process transport: a Cell client served by a handle in this process.
//!
//! Every call re-checks the receipt the caller expects against the handle's
//! current description before it touches the registry or the executor.

use super::*;

pub(crate) struct LocalCellTransport {
    pub(crate) registry: Arc<Registry>,
    pub(crate) handles: Arc<HashMap<CellId, CellHandle>>,
    pub(crate) handle: CellHandle,
    pub(crate) telemetry: CellTelemetryHandle,
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
        let telemetry = self.telemetry.clone();
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
                        let started = Instant::now();
                        let result = registry.execute_command_with_issue_time(
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
                        );
                        telemetry.primitive_operation(
                            command.module,
                            PrimitiveOperationKind::Command,
                            PrimitiveOperationOutcome::from(&result),
                            started.elapsed(),
                        );
                        result
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
        let telemetry = self.telemetry.clone();
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
                    let started = Instant::now();
                    let result = registry.execute_query(
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
                    );
                    telemetry.primitive_operation(
                        query.module,
                        PrimitiveOperationKind::Query,
                        match &result {
                            Ok(_) => PrimitiveOperationOutcome::Success,
                            Err(_) => PrimitiveOperationOutcome::Failed,
                        },
                        started.elapsed(),
                    );
                    result
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

pub(crate) fn encoded_command_operation_digest(
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

pub(super) fn decode_output<T: crate::codec::WireValue>(
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

pub(crate) fn validate_description(
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

pub(super) fn validate_minimum(
    description: CellDescription,
    minimum: Option<Receipt>,
) -> Result<()> {
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

pub(crate) fn local_description(handle: &CellHandle) -> CellDescription {
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

pub(crate) fn current_sequence(connection: &crab_ltx::rusqlite::Connection) -> Result<u64> {
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

pub(crate) fn receipt(description: CellDescription, commit_sequence: u64) -> Receipt {
    Receipt {
        cell: description.cell,
        incarnation: description.incarnation,
        commit_sequence,
    }
}

pub(super) fn unix_time_ms() -> Result<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Command("system clock precedes Unix epoch"))?;
    i64::try_from(duration.as_millis()).map_err(|_| Error::Command("system clock overflow"))
}
