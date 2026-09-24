use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc, time::Instant};

use prost::Message;

use crate::cell::actor::CellHandle;
use crate::cell::executor::{MutationIdentity, Resolution, StoredOutcome};
use crate::client::{CellDescription, Receipt};
use crate::client::{
    CellTransport, EncodedCommand, EncodedObservation, EncodedQuery, EncodedResolve,
    LocalCellTransport, encoded_command_operation_digest, local_description, receipt,
};
use crate::fleet::telemetry::{
    CellTelemetryHandle, PrimitiveOperationKind, PrimitiveOperationOutcome,
};
use crate::identity::{CellTarget, Digest, IncarnationId, RequestId};
use crate::primitives::effects::InboxDelivery;
use crate::registry::{CommandInvocation, Registry};
use crate::{Error, Result};

use super::{VerifiedPeerRequest, wire};

const MAX_RESULT_BYTES: usize = 1024 * 1024;

/// Resolves only a currently active owner on the receiving node.
pub trait PeerCellResolver: Send + Sync + 'static {
    /// Resolves one target to an active local Cell handle.
    fn resolve(
        &self,
        target: CellTarget,
    ) -> Pin<Box<dyn Future<Output = Result<CellHandle>> + Send + 'static>>;
}

/// Rechecks current product authorization after peer authentication.
pub trait PeerAuthorizer: Send + Sync + 'static {
    /// Rechecks product authorization for one verified request.
    fn authorize(&self, request: &VerifiedPeerRequest) -> Result<()>;
}

/// Executes authenticated peer work through the canonical local Cell transport.
pub struct PeerDispatcher {
    registry: Arc<Registry>,
    resolver: Arc<dyn PeerCellResolver>,
    authorizer: Arc<dyn PeerAuthorizer>,
    telemetry: CellTelemetryHandle,
}

impl PeerDispatcher {
    /// Creates a dispatcher over the compiled registry and its resolver and
    /// authorizer.
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
            telemetry: CellTelemetryHandle::default(),
        }
    }

    /// Reports primitive operations executed for peer requests.
    #[must_use]
    pub fn with_telemetry(mut self, telemetry: CellTelemetryHandle) -> Self {
        self.telemetry = telemetry;
        self
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
            handles: Arc::new(HashMap::from([(handle.cell_id(), handle.clone())])),
            handle,
            telemetry: self.telemetry.clone(),
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
            operation_digest: crate::primitives::effects::effect_operation_digest(
                target.cell_id(),
                effect_id,
                &encoded_operation,
            ),
            expires_at_ms: identity.expires_at_ms,
        };
        let registry = Arc::clone(&self.registry);
        let telemetry = self.telemetry.clone();
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
                    let started = Instant::now();
                    let result = registry.execute_command(
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
                    );
                    telemetry.primitive_operation(
                        module,
                        PrimitiveOperationKind::Command,
                        PrimitiveOperationOutcome::from(&result),
                        started.elapsed(),
                    );
                    result
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

mod convert;

use convert::*;
