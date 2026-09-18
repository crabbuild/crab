use std::{future::Future, pin::Pin, sync::Arc};

use prost::Message;

use crate::{
    CellDescription, CellHandle, CellTarget, CommandInvocation, Digest, Error, InboxDelivery,
    IncarnationId, MutationIdentity, Receipt, Registry, RequestId, Resolution, Result,
    StoredOutcome,
    client::{
        CellTransport, EncodedCommand, EncodedObservation, EncodedQuery, EncodedResolve,
        LocalCellTransport, encoded_command_operation_digest, local_description, receipt,
    },
};

use super::{VerifiedPeerRequest, wire};

const MAX_RESULT_BYTES: usize = 1024 * 1024;

/// Resolves only a currently active owner on the receiving node.
pub trait PeerCellResolver: Send + Sync + 'static {
    fn resolve(
        &self,
        target: CellTarget,
    ) -> Pin<Box<dyn Future<Output = Result<CellHandle>> + Send + 'static>>;
}

/// Rechecks current product authorization after peer authentication.
pub trait PeerAuthorizer: Send + Sync + 'static {
    fn authorize(&self, request: &VerifiedPeerRequest) -> Result<()>;
}

/// Executes authenticated peer work through the canonical local Cell transport.
pub struct PeerDispatcher {
    registry: Arc<Registry>,
    resolver: Arc<dyn PeerCellResolver>,
    authorizer: Arc<dyn PeerAuthorizer>,
}

impl PeerDispatcher {
    #[must_use]
    pub fn new(
        registry: Arc<Registry>,
        resolver: Arc<dyn PeerCellResolver>,
        authorizer: Arc<dyn PeerAuthorizer>,
    ) -> Self {
        Self {
            registry,
            resolver,
            authorizer,
        }
    }

    /// Authorizes, resolves and dispatches one verified request without a second SQL path.
    pub async fn dispatch(&self, request: &VerifiedPeerRequest, now_ms: i64) -> wire::PeerReply {
        if let Err(error) = self.authorizer.authorize(request) {
            return error_reply(error);
        }
        let handle = match self.resolver.resolve(request.target().clone()).await {
            Ok(handle) => handle,
            Err(error) => return error_reply(error),
        };
        let transport = LocalCellTransport {
            registry: Arc::clone(&self.registry),
            handle,
        };
        match request.operation() {
            Some(wire::peer_request::Operation::Mutate(mutation)) => {
                self.mutate(&transport, mutation, now_ms).await
            }
            Some(wire::peer_request::Operation::Read(read)) => {
                self.read(&transport, read, now_ms).await
            }
            Some(wire::peer_request::Operation::Resolve(resolve)) => {
                self.resolve(&transport, resolve, now_ms).await
            }
            Some(wire::peer_request::Operation::DeliverEffect(effect)) => {
                self.deliver_effect(&transport, effect, now_ms).await
            }
            Some(wire::peer_request::Operation::ResolveEffect(resolve)) => {
                self.resolve_effect(&transport, resolve, now_ms).await
            }
            Some(wire::peer_request::Operation::Migrate(migration)) => {
                self.migrate(&transport, migration, now_ms).await
            }
            None => error_reply(Error::Peer("peer operation is missing")),
        }
    }

    /// Encodes the validated reply for the management HTTP response body.
    pub async fn dispatch_bytes(
        &self,
        request: &VerifiedPeerRequest,
        now_ms: i64,
    ) -> Result<Vec<u8>> {
        let reply = self.dispatch(request, now_ms).await;
        super::encode_peer_reply(&reply)
    }

    async fn mutate(
        &self,
        transport: &LocalCellTransport,
        request: &wire::MutationRequest,
        now_ms: i64,
    ) -> wire::PeerReply {
        let result = self.mutate_inner(transport, request, now_ms).await;
        let outcome = match result {
            Ok(outcome) => mutation_reply(transport, outcome),
            Err(error) => return error_reply(error),
        };
        wire::PeerReply {
            outcome: Some(wire::peer_reply::Outcome::Mutation(outcome)),
        }
    }

    async fn mutate_inner(
        &self,
        transport: &LocalCellTransport,
        request: &wire::MutationRequest,
        now_ms: i64,
    ) -> Result<StoredOutcome> {
        let expected = local_description(&transport.handle);
        let identity = mutation_identity(
            request
                .identity
                .as_ref()
                .ok_or(Error::Peer("mutation identity is missing"))?,
            expected.incarnation,
        )?;
        let command = match request.operation.as_ref() {
            Some(wire::mutation_request::Operation::CellCommand(command)) => command,
            _ => return Err(Error::Peer("typed mutation operation is not implemented")),
        };
        let (module, descriptor) = self.registry.routed_command_contract(
            request_target(request.target.as_ref())?.namespace(),
            command.command_id,
            command.codec_version,
        )?;
        validate_description(
            &self.registry,
            module,
            expected,
            descriptor.schema_min,
            descriptor.schema_max,
        )?;
        let operation_digest = encoded_command_operation_digest(
            expected,
            identity,
            command.command_id,
            command.codec_version,
            &command.input,
        )?;
        transport
            .command(EncodedCommand {
                target: request_target(request.target.as_ref())?,
                expected,
                identity,
                operation_digest,
                now_ms,
                module,
                operation_id: command.command_id,
                codec_version: command.codec_version,
                input: command.input.clone(),
                input_limit: descriptor.input_limit,
                output_limit: descriptor.output_limit,
            })
            .await
    }

    async fn read(
        &self,
        transport: &LocalCellTransport,
        request: &wire::ReadRequest,
        now_ms: i64,
    ) -> wire::PeerReply {
        let expected = local_description(&transport.handle);
        let result = match request.operation.as_ref() {
            Some(wire::read_request::Operation::Describe(true)) => {
                wire::read_reply::Result::Description(description(expected))
            }
            Some(wire::read_request::Operation::CellQuery(query)) => {
                match self
                    .query(transport, request, query, expected, now_ms)
                    .await
                {
                    Ok(observation) => {
                        return wire::PeerReply {
                            outcome: Some(wire::peer_reply::Outcome::Read(wire::ReadReply {
                                receipt: Some(wire_receipt(observation.receipt)),
                                result: Some(wire::read_reply::Result::CommandOutput(
                                    observation.output,
                                )),
                            })),
                        };
                    }
                    Err(error) => return error_reply(error),
                }
            }
            _ => return error_reply(Error::Peer("typed read operation is not implemented")),
        };
        wire::PeerReply {
            outcome: Some(wire::peer_reply::Outcome::Read(wire::ReadReply {
                receipt: None,
                result: Some(result),
            })),
        }
    }

    async fn query(
        &self,
        transport: &LocalCellTransport,
        request: &wire::ReadRequest,
        query: &wire::CellQuery,
        expected: CellDescription,
        now_ms: i64,
    ) -> Result<EncodedObservation> {
        let target = request_target(request.target.as_ref())?;
        let (module, descriptor) = self.registry.routed_query_contract(
            target.namespace(),
            query.query_id,
            query.codec_version,
        )?;
        validate_description(
            &self.registry,
            module,
            expected,
            descriptor.schema_min,
            descriptor.schema_max,
        )?;
        transport
            .query(EncodedQuery {
                target,
                expected,
                minimum: request.minimum.as_ref().map(runtime_receipt).transpose()?,
                now_ms,
                module,
                operation_id: query.query_id,
                codec_version: query.codec_version,
                input: query.input.clone(),
                input_limit: descriptor.input_limit,
                output_limit: descriptor.output_limit,
            })
            .await
    }

    async fn resolve(
        &self,
        transport: &LocalCellTransport,
        request: &wire::ResolveRequest,
        now_ms: i64,
    ) -> wire::PeerReply {
        let expected = local_description(&transport.handle);
        let result = async {
            let identity = mutation_identity(
                request
                    .identity
                    .as_ref()
                    .ok_or(Error::Peer("resolve identity is missing"))?,
                expected.incarnation,
            )?;
            let operation_digest = Digest::try_from(request.operation_digest.as_slice())?;
            transport
                .resolve(EncodedResolve {
                    target: request_target(request.target.as_ref())?,
                    expected,
                    identity,
                    operation_digest,
                    now_ms,
                    max_result_bytes: MAX_RESULT_BYTES,
                })
                .await
        }
        .await;
        let resolution = match result {
            Ok(value) => value,
            Err(error) => return error_reply(error),
        };
        wire::PeerReply {
            outcome: Some(wire::peer_reply::Outcome::Resolve(resolve_reply(
                transport, resolution,
            ))),
        }
    }

    async fn deliver_effect(
        &self,
        transport: &LocalCellTransport,
        request: &wire::EffectRequest,
        now_ms: i64,
    ) -> wire::PeerReply {
        let result = self.deliver_effect_inner(transport, request, now_ms).await;
        let outcome = match result {
            Ok(outcome) => mutation_reply(transport, outcome),
            Err(error) => return error_reply(error),
        };
        wire::PeerReply {
            outcome: Some(wire::peer_reply::Outcome::Mutation(outcome)),
        }
    }

    async fn deliver_effect_inner(
        &self,
        transport: &LocalCellTransport,
        request: &wire::EffectRequest,
        now_ms: i64,
    ) -> Result<StoredOutcome> {
        let expected = local_description(&transport.handle);
        validate_effect_incarnation(request.destination_incarnation.as_slice(), expected)?;
        let target = request_target(request.target.as_ref())?;
        let identity = request
            .identity
            .as_ref()
            .ok_or(Error::Peer("effect identity is missing"))?;
        let command = match request.operation.as_ref() {
            Some(wire::effect_request::Operation::CellCommand(command)) => command,
            _ => return Err(Error::Peer("typed effect operation is not implemented")),
        };
        let (module, descriptor) = self.registry.routed_command_contract(
            target.namespace(),
            command.command_id,
            command.codec_version,
        )?;
        validate_description(
            &self.registry,
            module,
            expected,
            descriptor.schema_min,
            descriptor.schema_max,
        )?;
        if command.input.len() > descriptor.input_limit as usize {
            return Err(Error::Command("encoded effect input exceeds limit"));
        }
        let effect_id = exact_effect_id(identity)?;
        let mut canonical_operation = request.clone();
        canonical_operation.destination_incarnation.clear();
        let encoded_operation = canonical_operation.encode_to_vec();
        let encoded_request = request.encode_to_vec();
        let delivery = InboxDelivery {
            effect_id,
            operation_digest: crate::effect_operation_digest(
                target.cell_id(),
                effect_id,
                &encoded_operation,
            ),
            expires_at_ms: identity.expires_at_ms,
        };
        let registry = Arc::clone(&self.registry);
        let schema = transport.handle.schema();
        let input = command.input.clone();
        let operation_id = command.command_id;
        let codec_version = command.codec_version;
        transport
            .handle
            .deliver_effect(
                delivery,
                now_ms,
                encoded_request.len(),
                descriptor.output_limit as usize,
                move |transaction| {
                    registry.execute_command(
                        transaction,
                        CommandInvocation {
                            module,
                            operation_id,
                            codec_version,
                            schema,
                            target: target.clone(),
                            sequence: next_sequence(transaction)?,
                            now_ms,
                            input: &input,
                        },
                    )
                },
            )
            .await
    }

    async fn resolve_effect(
        &self,
        transport: &LocalCellTransport,
        request: &wire::EffectResolveRequest,
        now_ms: i64,
    ) -> wire::PeerReply {
        let result = async {
            let expected = local_description(&transport.handle);
            validate_effect_incarnation(request.destination_incarnation.as_slice(), expected)?;
            let identity = request
                .identity
                .as_ref()
                .ok_or(Error::Peer("effect Resolve identity is missing"))?;
            let delivery = InboxDelivery {
                effect_id: exact_effect_id(identity)?,
                operation_digest: Digest::try_from(request.operation_digest.as_slice())?,
                expires_at_ms: identity.expires_at_ms,
            };
            transport
                .handle
                .resolve_effect(delivery, now_ms, MAX_RESULT_BYTES)
                .await
        }
        .await;
        let resolution = match result {
            Ok(value) => value,
            Err(error) => return error_reply(error),
        };
        wire::PeerReply {
            outcome: Some(wire::peer_reply::Outcome::Resolve(resolve_reply(
                transport, resolution,
            ))),
        }
    }

    async fn migrate(
        &self,
        transport: &LocalCellTransport,
        request: &wire::MigrationRequest,
        now_ms: i64,
    ) -> wire::PeerReply {
        let result = self.migrate_inner(transport, request, now_ms).await;
        match result {
            Ok(current) => wire::PeerReply {
                outcome: Some(wire::peer_reply::Outcome::Migration(wire::MigrationReply {
                    description: Some(description(current)),
                })),
            },
            Err(error) => error_reply(error),
        }
    }

    async fn migrate_inner(
        &self,
        transport: &LocalCellTransport,
        request: &wire::MigrationRequest,
        now_ms: i64,
    ) -> Result<CellDescription> {
        let target = request_target(request.target.as_ref())?;
        let from_code = Digest::try_from(request.from_code.as_slice())?;
        let to_code = Digest::try_from(request.to_code.as_slice())?;
        let plan = self
            .registry
            .next_migration(target.namespace(), from_code, request.from_schema)?
            .ok_or(Error::Registry("requested Cell migration has no successor"))?;
        if plan.to_code() != to_code || plan.to_schema() != request.to_schema {
            return Err(Error::Registry(
                "requested Cell migration differs from the compiled successor",
            ));
        }

        let current = local_description(&transport.handle);
        if current.cell != target.cell_id()
            || current.incarnation != IncarnationId::try_from(request.incarnation.as_slice())?
        {
            return Err(Error::Fenced);
        }
        if current.code == plan.to_code() && current.schema >= plan.to_schema() {
            return Ok(current);
        }
        if current.code != plan.from_code() || current.schema != plan.from_schema() {
            return Err(Error::Fenced);
        }

        let migrated = transport.handle.migrate(plan, now_ms).await?;
        let description = local_description(&migrated.handle);
        if description.code != migrated.outcome.code
            || description.schema != migrated.outcome.schema
        {
            return Err(Error::Control(
                "published migration capability and outcome differ",
            ));
        }
        Ok(description)
    }
}

fn validate_effect_incarnation(value: &[u8], expected: CellDescription) -> Result<()> {
    if IncarnationId::try_from(value)? != expected.incarnation {
        return Err(Error::Fenced);
    }
    Ok(())
}

fn exact_effect_id(identity: &wire::EffectIdentity) -> Result<[u8; 32]> {
    let effect_id = <[u8; 32]>::try_from(identity.effect_id.as_slice())
        .map_err(|_| Error::Peer("invalid effect ID length"))?;
    let expected = crate::effect_id(
        crate::CellId::try_from(identity.source_cell.as_slice())?,
        IncarnationId::try_from(identity.source_incarnation.as_slice())?,
        identity.source_sequence,
        identity.ordinal,
    );
    if effect_id != expected {
        return Err(Error::Peer("effect identity derivation does not match"));
    }
    Ok(effect_id)
}

fn next_sequence(transaction: &crab_ltx::rusqlite::Transaction<'_>) -> Result<u64> {
    let sequence = transaction.query_row(
        "SELECT commit_sequence + 1 FROM sys_meta WHERE singleton = 1",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    u64::try_from(sequence)
        .ok()
        .filter(|sequence| *sequence != 0)
        .ok_or(Error::Command("invalid next Cell sequence"))
}

fn mutation_identity(
    identity: &wire::MutationIdentity,
    expected_incarnation: IncarnationId,
) -> Result<MutationIdentity> {
    if IncarnationId::try_from(identity.incarnation.as_slice())? != expected_incarnation {
        return Err(Error::Fenced);
    }
    Ok(MutationIdentity {
        request_id: RequestId::try_from(identity.request_id.as_slice())?,
        issued_at_ms: identity.issued_at_ms,
        expires_at_ms: identity.expires_at_ms,
    })
}

fn request_target(target: Option<&wire::Target>) -> Result<CellTarget> {
    let target = target.ok_or(Error::Peer("peer target is missing"))?;
    CellTarget::new(
        crate::TenantId::try_from(target.tenant_id.as_slice())?,
        crate::ApplicationId::try_from(target.application_id.as_slice())?,
        crate::NamespaceId::try_from(target.namespace_id.as_slice())?,
        &target.partition,
    )
}

fn validate_description(
    registry: &Registry,
    module: &str,
    description: CellDescription,
    schema_min: u32,
    schema_max: u32,
) -> Result<()> {
    if !registry.supports_module_code(module, description.code, description.schema)
        || !(schema_min..=schema_max).contains(&description.schema)
    {
        return Err(Error::Fenced);
    }
    Ok(())
}

fn mutation_reply(transport: &LocalCellTransport, outcome: StoredOutcome) -> wire::MutationReply {
    let description = local_description(&transport.handle);
    match outcome {
        StoredOutcome::Success {
            result,
            commit_sequence,
        } => wire::MutationReply {
            receipt: Some(wire_receipt(receipt(description, commit_sequence))),
            outcome: Some(wire::mutation_reply::Outcome::Result(
                wire::MutationResult {
                    result: Some(wire::mutation_result::Result::CommandOutput(result)),
                },
            )),
        },
        StoredOutcome::Rejected {
            result,
            commit_sequence,
        } => wire::MutationReply {
            receipt: Some(wire_receipt(receipt(description, commit_sequence))),
            outcome: Some(wire::mutation_reply::Outcome::Error(wire::Error {
                code: wire::error::Code::PreconditionFailed as i32,
                outcome: wire::error::Outcome::Rejected as i32,
                message: "Cell command was durably rejected".into(),
                retry_after_ms: 0,
                application_details: result,
            })),
        },
    }
}

fn resolve_reply(transport: &LocalCellTransport, resolution: Resolution) -> wire::ResolveReply {
    match resolution {
        Resolution::Committed(outcome) => wire::ResolveReply {
            state: match &outcome {
                StoredOutcome::Success { .. } => wire::resolve_reply::State::Committed as i32,
                StoredOutcome::Rejected { .. } => wire::resolve_reply::State::Rejected as i32,
            },
            reply: Some(mutation_reply(transport, outcome)),
        },
        Resolution::Absent => wire::ResolveReply {
            state: wire::resolve_reply::State::Absent as i32,
            reply: None,
        },
        Resolution::Unknown => wire::ResolveReply {
            state: wire::resolve_reply::State::Unknown as i32,
            reply: None,
        },
        Resolution::Expired => wire::ResolveReply {
            state: wire::resolve_reply::State::Expired as i32,
            reply: None,
        },
    }
}

fn description(value: CellDescription) -> wire::CellDescription {
    wire::CellDescription {
        cell_id: value.cell.as_bytes().to_vec(),
        incarnation: value.incarnation.as_bytes().to_vec(),
        code: value.code.as_bytes().to_vec(),
        schema: value.schema,
    }
}

fn runtime_receipt(value: &wire::Receipt) -> Result<Receipt> {
    Ok(Receipt {
        cell: crate::CellId::try_from(value.cell_id.as_slice())?,
        incarnation: IncarnationId::try_from(value.incarnation.as_slice())?,
        commit_sequence: value.commit_sequence,
    })
}

fn wire_receipt(value: Receipt) -> wire::Receipt {
    wire::Receipt {
        cell_id: value.cell.as_bytes().to_vec(),
        incarnation: value.incarnation.as_bytes().to_vec(),
        commit_sequence: value.commit_sequence,
    }
}

fn error_reply(error: Error) -> wire::PeerReply {
    let (code, outcome, message, retry_after_ms) = match error {
        Error::RequestConflict => (
            wire::error::Code::RequestIdConflict,
            wire::error::Outcome::Rejected,
            "request identity conflicts with a stored operation",
            0,
        ),
        Error::OutcomeUnknown { .. } | Error::EffectOutcomeUnknown { .. } => (
            wire::error::Code::OutcomeUnknown,
            wire::error::Outcome::Unknown,
            "accepted command outcome requires resolution",
            100,
        ),
        Error::EffectExpired => (
            wire::error::Code::RequestExpired,
            wire::error::Outcome::Rejected,
            "Cell effect expired before delivery",
            0,
        ),
        Error::Capacity(_) => (
            wire::error::Code::ResourceExhausted,
            wire::error::Outcome::NotStarted,
            "Cell runtime capacity is exhausted",
            100,
        ),
        Error::Fenced
        | Error::CellNotActive
        | Error::CellDraining
        | Error::RuntimeClosed
        | Error::StreamCancelled
        | Error::PendingPublication => (
            wire::error::Code::Unavailable,
            wire::error::Outcome::NotStarted,
            "Cell owner is unavailable",
            100,
        ),
        Error::Registry(_) => (
            wire::error::Code::SchemaIncompatible,
            wire::error::Outcome::Rejected,
            "compiled Cell operation is unavailable",
            0,
        ),
        Error::Identity(_) | Error::Command(_) | Error::Peer(_) | Error::PeerDecode(_) => (
            wire::error::Code::InvalidArgument,
            wire::error::Outcome::Rejected,
            "invalid Cell peer request",
            0,
        ),
        Error::Deadline => (
            wire::error::Code::Unavailable,
            wire::error::Outcome::Unknown,
            "Cell operation deadline expired",
            100,
        ),
        Error::PeerSignature(_) => (
            wire::error::Code::PermissionDenied,
            wire::error::Outcome::Rejected,
            "Cell peer authorization failed",
            0,
        ),
        Error::PeerAuthorization(_) => (
            wire::error::Code::PermissionDenied,
            wire::error::Outcome::Rejected,
            "Cell peer principal is not authorized",
            0,
        ),
        Error::Control(_)
        | Error::Catalog(_)
        | Error::Node(_)
        | Error::Release(_)
        | Error::Backup(_)
        | Error::Retention(_)
        | Error::RetentionIo(_)
        | Error::RetentionWorkerJoin(_)
        | Error::FollowerIo(_)
        | Error::FollowerWorkerJoin(_)
        | Error::PeerTransport { .. }
        | Error::PeerTransportUnknown { .. }
        | Error::CatalogCollision
        | Error::CatalogFull
        | Error::Json(_)
        | Error::Codec(_)
        | Error::Sqlite(_)
        | Error::Utf8(_)
        | Error::Storage(_)
        | Error::Ltx(_)
        | Error::WorkerStart(_)
        | Error::WorkerJoin(_)
        | Error::WorkerPanic
        | Error::ActivityWorkerStart(_)
        | Error::ActivityWorkerJoin(_)
        | Error::ActivityWorkerPanic
        | Error::ActivityPanic
        | Error::NativePanic
        | Error::RuntimeStart(_)
        | Error::CellAlreadyActive => (
            wire::error::Code::Internal,
            wire::error::Outcome::Unknown,
            "Cell runtime failed",
            100,
        ),
    };
    wire::PeerReply {
        outcome: Some(wire::peer_reply::Outcome::Error(wire::Error {
            code: code as i32,
            outcome: outcome as i32,
            message: message.into(),
            retry_after_ms,
            application_details: Vec::new(),
        })),
    }
}
