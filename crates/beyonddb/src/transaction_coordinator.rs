//! Sharded, durable decisions for transactions spanning Cell owners.

use std::collections::HashSet;
use std::sync::OnceLock;

use crab_cell_app::CellType;
use crab_cell_runtime::cell::catalog::CatalogRole;
use crab_cell_runtime::identity::{CellTarget, Digest, NamespaceId};
use crab_cell_runtime::registry::{
    Command, CommandContext, CommandResult, MigrationDescriptor, ModuleDescriptor,
    NamespaceDescriptor, OperationDescriptor, RegistryBuilder,
};
use serde::{Deserialize, Serialize};

use crate::items::{TransactionFailure, TransactionOperation};
use crate::table::statement;
use crate::transaction_token::{TOKEN_LIFETIME_MS, TransactionToken};
use crate::{
    APPLICATION, Error, Json, OPERATION_BYTES, Result, SqlValue, account_target, data_target,
};

pub(crate) const MODULE: &str = "beyonddb-coordinator";
pub(crate) const NAMESPACE: NamespaceId = NamespaceId::from_bytes([0x45; 16]);
const SHARDS: u32 = 4_096;
const SCHEMA: &str = include_str!("transaction_coordinator_schema.sql");

static NAMESPACES: [NamespaceDescriptor; 1] = [NamespaceDescriptor {
    id: NAMESPACE,
    name: MODULE,
    role: CatalogRole::Sql,
    shards: SHARDS,
    effect_targets: &[],
    dead_letter: None,
}];
static COMMANDS: [OperationDescriptor; 4] =
    [operation(1), operation(2), operation(3), operation(4)];
static QUERIES: [OperationDescriptor; 6] = [
    operation(1),
    operation(2),
    operation(3),
    operation(4),
    operation(5),
    operation(6),
];

const fn operation(id: u32) -> OperationDescriptor {
    OperationDescriptor {
        id,
        codec_version: 1,
        schema_min: 1,
        schema_max: 1,
        input_limit: OPERATION_BYTES,
        output_limit: OPERATION_BYTES,
    }
}

pub(crate) struct CoordinatorModule;

impl crab_cell_runtime::registry::CellModule for CoordinatorModule {
    const NAME: &'static str = MODULE;

    fn descriptor(&self) -> &'static ModuleDescriptor {
        static DESCRIPTOR: OnceLock<ModuleDescriptor> = OnceLock::new();
        DESCRIPTOR.get_or_init(|| ModuleDescriptor {
            name: MODULE,
            source_digest: {
                let mut source = blake3::Hasher::new();
                source.update(include_bytes!("transaction_coordinator.rs"));
                source.update(include_bytes!("transaction_coordinator/phase.rs"));
                source.update(include_bytes!("transaction_coordinator/token.rs"));
                source.update(include_bytes!("transaction_token.rs"));
                source.update(include_bytes!("items.rs"));
                source.update(include_bytes!("expression_wire.rs"));
                Digest::from_bytes(*source.finalize().as_bytes())
            },
            retained_codes: &[],
            schema_min: 1,
            schema_max: 1,
            migrations: Box::leak(Box::new([MigrationDescriptor {
                version: 1,
                sql: SCHEMA,
                digest: Digest::from_bytes(*blake3::hash(SCHEMA.as_bytes()).as_bytes()),
            }])),
            commands: &COMMANDS,
            queries: &QUERIES,
            workflow_definitions: &[],
            activity_types: &[],
            namespaces: &NAMESPACES,
        })
    }

    fn register(self, registry: &mut RegistryBuilder) -> Result<()> {
        registry.bind_command::<BeginCrossCellTransaction>()?;
        registry.bind_command::<RecordParticipantPrepare>()?;
        registry.bind_command::<DecideCrossCellTransaction>()?;
        registry.bind_command::<RecordParticipantResolution>()?;
        registry.bind_query::<ReadCrossCellTransaction>()?;
        registry.bind_query::<ReadCoordinatorParticipant>()?;
        registry.bind_query::<ReadPendingCrossCellTransactions>()?;
        registry.bind_query::<ReadUnresolvedCoordinatorParticipants>()?;
        registry.bind_query::<ReadPendingTransactionBoundary>()?;
        registry.bind_query::<ReadCoordinatorToken>()
    }
}

pub(crate) fn cell_type() -> Result<CellType> {
    CellType::new(MODULE, MODULE, NAMESPACE, CatalogRole::Sql, SHARDS)?
        .with_limits(512 * 1024 * 1024, 64 * 1024 * 1024)
}

/// Choose a stable account-scoped coordinator shard from a client token or ID.
pub fn coordinator_target(account_id: &str, routing_key: &[u8]) -> Result<CellTarget> {
    if routing_key.is_empty() || routing_key.len() > 128 {
        return Err(Error::Identity("invalid transaction routing key"));
    }
    let account = account_target(account_id)?;
    CellTarget::new(
        account.tenant(),
        APPLICATION,
        NAMESPACE,
        &cell_type()?.partition_for_scope(routing_key)?,
    )
}

/// Install the coordinator schema during Cell bootstrap.
pub fn initialize_coordinator(transaction: &crab_ltx::rusqlite::Transaction<'_>) -> Result<()> {
    transaction.execute_batch(SCHEMA)?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CoordinatorParticipantTarget {
    Account,
    Data {
        table_id: String,
        partition_id: [u8; 16],
        epoch: u64,
    },
}

impl CoordinatorParticipantTarget {
    pub(crate) fn cell_id(&self, account_id: &str) -> Result<[u8; 32]> {
        let target = match self {
            Self::Account => account_target(account_id)?,
            Self::Data {
                table_id,
                partition_id,
                ..
            } => data_target(account_id, table_id, partition_id)?,
        };
        Ok(*target.cell_id().as_bytes())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexedTransactionOperation {
    pub index: u8,
    pub operation: TransactionOperation,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorParticipant {
    pub target: CoordinatorParticipantTarget,
    pub operations: Vec<IndexedTransactionOperation>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BeginCrossCellTransactionInput {
    pub account_id: String,
    pub transaction_id: [u8; 16],
    pub token: Option<TransactionToken>,
    pub participants: Vec<CoordinatorParticipant>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CoordinatorDecision {
    Begin,
    Commit,
    Abort {
        index: Option<u8>,
        reason: Option<TransactionFailure>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum BeginCrossCellTransactionOutcome {
    Begun,
    Existing {
        transaction_id: [u8; 16],
        decision: CoordinatorDecision,
    },
    Mismatch,
    InvalidParticipants,
}

/// Fix the participant set and token before any data Cell prepare.
pub struct BeginCrossCellTransaction;

impl Command for BeginCrossCellTransaction {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 1;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<BeginCrossCellTransactionInput>;
    type Output = Json<BeginCrossCellTransactionOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let key = input
            .token
            .as_ref()
            .map_or(input.transaction_id.as_slice(), |token| {
                token.token.as_bytes()
            });
        if coordinator_target(&input.account_id, key)? != *context.target()
            || input
                .token
                .as_ref()
                .is_some_and(|token| token.account_id != input.account_id)
        {
            return Err(Error::Identity("transaction reached the wrong coordinator"));
        }
        let participants = serde_json::to_vec(&input.participants)?;
        let digest = blake3::hash(&participants);
        let fingerprint = input.token.as_ref().map_or_else(
            || digest.to_hex().to_string(),
            |token| token.fingerprint.clone(),
        );
        let rows = context.sql(&statement(
            "SELECT transaction_id, fingerprint, request_digest, state, abort_reason, token \
             FROM ddb_coordinator_transactions \
             WHERE transaction_id = ?1 OR (token = ?2 AND \
             (completed_at_ms IS NULL OR completed_at_ms > ?3))",
            vec![
                SqlValue::Blob(input.transaction_id.to_vec()),
                input
                    .token
                    .as_ref()
                    .map_or(SqlValue::Null, |token| SqlValue::Text(token.token.clone())),
                SqlValue::Integer(context.now_ms().saturating_sub(TOKEN_LIFETIME_MS)),
            ],
        ))?;
        if rows[0].rows.len() > 1 {
            return Ok(CommandResult::Rejected(Json(
                BeginCrossCellTransactionOutcome::Mismatch,
            )));
        }
        if let Some(row) = rows[0].rows.first() {
            let [
                SqlValue::Blob(id),
                SqlValue::Text(old_fingerprint),
                SqlValue::Blob(old_digest),
                SqlValue::Integer(state),
                reason,
                old_token,
            ] = row.as_slice()
            else {
                return Err(Error::Command("invalid coordinator transaction record"));
            };
            // Token retries use the engine's request fingerprint. A new proposal
            // may observe split routes; the original participant set remains fixed.
            let same_token = input
                .token
                .as_ref()
                .is_some_and(|token| old_token == &SqlValue::Text(token.token.clone()));
            if old_fingerprint != &fingerprint
                || (!same_token && old_digest.as_slice() != digest.as_bytes())
            {
                return Ok(CommandResult::Rejected(Json(
                    BeginCrossCellTransactionOutcome::Mismatch,
                )));
            }
            let transaction_id: [u8; 16] = id
                .as_slice()
                .try_into()
                .map_err(|_| Error::Command("invalid coordinator transaction ID"))?;
            return Ok(CommandResult::Success(Json(
                BeginCrossCellTransactionOutcome::Existing {
                    transaction_id,
                    decision: decode_decision(*state, reason)?,
                },
            )));
        }
        if input.participants.is_empty() || input.participants.len() > 100 {
            return Ok(CommandResult::Rejected(Json(
                BeginCrossCellTransactionOutcome::InvalidParticipants,
            )));
        }
        let mut indexes = HashSet::new();
        let mut previous_cell = None;
        let mut participants_rows = Vec::with_capacity(input.participants.len());
        for (position, participant) in input.participants.iter().enumerate() {
            let cell_id = participant.target.cell_id(&input.account_id)?;
            if previous_cell.is_some_and(|previous| previous >= cell_id)
                || participant.operations.is_empty()
            {
                return Ok(CommandResult::Rejected(Json(
                    BeginCrossCellTransactionOutcome::InvalidParticipants,
                )));
            }
            previous_cell = Some(cell_id);
            for operation in &participant.operations {
                if !indexes.insert(operation.index) {
                    return Ok(CommandResult::Rejected(Json(
                        BeginCrossCellTransactionOutcome::InvalidParticipants,
                    )));
                }
            }
            participants_rows.push((
                position,
                cell_id,
                serde_json::to_vec(&participant.target)?,
                serde_json::to_vec(&participant.operations)?,
            ));
        }
        if indexes.is_empty()
            || indexes.len() > 100
            || indexes.iter().copied().max()
                != Some(
                    u8::try_from(indexes.len() - 1)
                        .map_err(|_| Error::Command("transaction operation count overflow"))?,
                )
        {
            return Ok(CommandResult::Rejected(Json(
                BeginCrossCellTransactionOutcome::InvalidParticipants,
            )));
        }
        if let Some(token) = &input.token {
            // Release only a completed token's replay slot. Keep the old decision
            // and participant records available to delayed phase invocations.
            context.sql(&statement(
                "UPDATE ddb_coordinator_transactions SET token = NULL WHERE token = ?1 \
                 AND unresolved_count = 0 AND completed_at_ms <= ?2",
                vec![
                    SqlValue::Text(token.token.clone()),
                    SqlValue::Integer(context.now_ms().saturating_sub(TOKEN_LIFETIME_MS)),
                ],
            ))?;
        }
        context.sql(&statement(
            "INSERT INTO ddb_coordinator_transactions \
             (transaction_id, account_id, token, fingerprint, request_digest, state, unresolved_count, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7)",
            vec![
                SqlValue::Blob(input.transaction_id.to_vec()),
                SqlValue::Text(input.account_id),
                input.token.map_or(SqlValue::Null, |token| SqlValue::Text(token.token)),
                SqlValue::Text(fingerprint),
                SqlValue::Blob(digest.as_bytes().to_vec()),
                SqlValue::Integer(
                    i64::try_from(input.participants.len())
                        .map_err(|_| Error::Command("participant count overflow"))?,
                ),
                SqlValue::Integer(context.now_ms()),
            ],
        ))?;
        for (position, cell_id, target, operations) in participants_rows {
            context.sql(&statement(
                "INSERT INTO ddb_coordinator_participants \
                 (transaction_id, position, cell_id, target, operations) VALUES (?1, ?2, ?3, ?4, ?5)",
                vec![
                    SqlValue::Blob(input.transaction_id.to_vec()),
                    SqlValue::Integer(
                        i64::try_from(position)
                            .map_err(|_| Error::Command("participant index overflow"))?,
                    ),
                    SqlValue::Blob(cell_id.to_vec()),
                    SqlValue::Blob(target),
                    SqlValue::Blob(operations),
                ],
            ))?;
        }
        Ok(CommandResult::Success(Json(
            BeginCrossCellTransactionOutcome::Begun,
        )))
    }
}

fn decode_decision(state: i64, reason: &SqlValue) -> Result<CoordinatorDecision> {
    match state {
        0 => Ok(CoordinatorDecision::Begin),
        1 => Ok(CoordinatorDecision::Commit),
        2 => {
            let SqlValue::Blob(bytes) = reason else {
                return Err(Error::Command("aborted transaction has no reason"));
            };
            Ok(serde_json::from_slice(bytes)?)
        }
        _ => Err(Error::Command("invalid coordinator decision")),
    }
}

mod phase;
mod registry;
mod token;
pub use phase::*;
pub use registry::*;
pub use token::*;
