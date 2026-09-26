//! Decision publication and participant progress in a coordinator Cell.

use crab_cell_runtime::registry::{Command, CommandContext, CommandResult, Query, QueryContext};
use serde::{Deserialize, Serialize};

use super::{
    CoordinatorDecision, CoordinatorParticipant, CoordinatorParticipantTarget, MODULE,
    coordinator_target, decode_decision,
};
use crate::table::statement;
use crate::{Error, Json, Result, SqlValue};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorPhaseInput {
    pub account_id: String,
    pub transaction_id: [u8; 16],
    pub routing_key: Vec<u8>,
    pub position: u8,
    pub participant_cell: [u8; 32],
    pub sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CoordinatorPhaseOutcome {
    Recorded,
    Replay,
    Missing,
    WrongParticipant,
    WrongDecision,
}

fn phase_identity(context: &CommandContext<'_, '_>, account_id: &str, key: &[u8]) -> Result<()> {
    if coordinator_target(account_id, key)? != *context.target() {
        return Err(Error::Identity(
            "transaction phase reached the wrong coordinator",
        ));
    }
    Ok(())
}

enum PhaseRow {
    Missing,
    WrongParticipant,
    Found {
        state: i64,
        prepared: Option<i64>,
        resolved: Option<i64>,
    },
}

fn phase_row(
    context: &mut CommandContext<'_, '_>,
    input: &CoordinatorPhaseInput,
) -> Result<PhaseRow> {
    let rows = context.sql(&statement(
        "SELECT p.cell_id, p.prepared_sequence, p.resolved_sequence, t.state \
         FROM ddb_coordinator_participants p JOIN ddb_coordinator_transactions t \
         ON t.transaction_id = p.transaction_id \
         WHERE p.transaction_id = ?1 AND p.position = ?2 AND t.account_id = ?3",
        vec![
            SqlValue::Blob(input.transaction_id.to_vec()),
            SqlValue::Integer(i64::from(input.position)),
            SqlValue::Text(input.account_id.clone()),
        ],
    ))?;
    let Some(row) = rows[0].rows.first() else {
        return Ok(PhaseRow::Missing);
    };
    let [
        SqlValue::Blob(cell),
        prepared,
        resolved,
        SqlValue::Integer(state),
    ] = row.as_slice()
    else {
        return Err(Error::Command("invalid coordinator participant row"));
    };
    if cell.as_slice() != input.participant_cell {
        return Ok(PhaseRow::WrongParticipant);
    }
    Ok(PhaseRow::Found {
        state: *state,
        prepared: optional_sequence(prepared)?,
        resolved: optional_sequence(resolved)?,
    })
}

fn optional_sequence(value: &SqlValue) -> Result<Option<i64>> {
    match value {
        SqlValue::Null => Ok(None),
        SqlValue::Integer(sequence) if *sequence > 0 => Ok(Some(*sequence)),
        _ => Err(Error::Command("invalid coordinator participant sequence")),
    }
}

/// Record evidence that one participant prepare was published.
pub struct RecordParticipantPrepare;

impl Command for RecordParticipantPrepare {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 2;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<CoordinatorPhaseInput>;
    type Output = Json<CoordinatorPhaseOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        phase_identity(context, &input.account_id, &input.routing_key)?;
        let (state, prepared) = match phase_row(context, &input)? {
            PhaseRow::Missing => {
                return Ok(CommandResult::Rejected(Json(
                    CoordinatorPhaseOutcome::Missing,
                )));
            }
            PhaseRow::WrongParticipant => {
                return Ok(CommandResult::Rejected(Json(
                    CoordinatorPhaseOutcome::WrongParticipant,
                )));
            }
            PhaseRow::Found {
                state, prepared, ..
            } => (state, prepared),
        };
        if state != 0 {
            return Ok(CommandResult::Rejected(Json(
                CoordinatorPhaseOutcome::WrongDecision,
            )));
        }
        if prepared.is_some() {
            return Ok(CommandResult::Success(Json(
                CoordinatorPhaseOutcome::Replay,
            )));
        }
        let sequence = i64::try_from(input.sequence)
            .ok()
            .filter(|value| *value > 0)
            .ok_or(Error::Command("invalid prepare sequence"))?;
        context.sql(&statement(
            "UPDATE ddb_coordinator_participants SET prepared_sequence = ?1 \
             WHERE transaction_id = ?2 AND position = ?3 AND prepared_sequence IS NULL",
            vec![
                SqlValue::Integer(sequence),
                SqlValue::Blob(input.transaction_id.to_vec()),
                SqlValue::Integer(i64::from(input.position)),
            ],
        ))?;
        Ok(CommandResult::Success(Json(
            CoordinatorPhaseOutcome::Recorded,
        )))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DecideCrossCellTransactionInput {
    pub account_id: String,
    pub transaction_id: [u8; 16],
    pub routing_key: Vec<u8>,
    pub decision: CoordinatorDecision,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DecideCrossCellTransactionOutcome {
    Decided(CoordinatorDecision),
    Missing,
    NotPrepared,
    DecisionConflict,
}

/// Publish the sole transaction decision after every prepare, or abort.
pub struct DecideCrossCellTransaction;

impl Command for DecideCrossCellTransaction {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 3;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<DecideCrossCellTransactionInput>;
    type Output = Json<DecideCrossCellTransactionOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        phase_identity(context, &input.account_id, &input.routing_key)?;
        if input.decision == CoordinatorDecision::Begin {
            return Ok(CommandResult::Rejected(Json(
                DecideCrossCellTransactionOutcome::DecisionConflict,
            )));
        }
        let rows = context.sql(&statement(
            "SELECT state, abort_reason FROM ddb_coordinator_transactions \
             WHERE transaction_id = ?1 AND account_id = ?2",
            vec![
                SqlValue::Blob(input.transaction_id.to_vec()),
                SqlValue::Text(input.account_id),
            ],
        ))?;
        let Some(row) = rows[0].rows.first() else {
            return Ok(CommandResult::Rejected(Json(
                DecideCrossCellTransactionOutcome::Missing,
            )));
        };
        let [SqlValue::Integer(state), reason] = row.as_slice() else {
            return Err(Error::Command("invalid coordinator transaction record"));
        };
        if *state != 0 {
            let current = decode_decision(*state, reason)?;
            if current == input.decision {
                return Ok(CommandResult::Success(Json(
                    DecideCrossCellTransactionOutcome::Decided(current),
                )));
            }
            return Ok(CommandResult::Rejected(Json(
                DecideCrossCellTransactionOutcome::DecisionConflict,
            )));
        }
        if input.decision == CoordinatorDecision::Commit {
            let rows = context.sql(&statement(
                "SELECT 1 FROM ddb_coordinator_participants \
                 WHERE transaction_id = ?1 AND prepared_sequence IS NULL LIMIT 1",
                vec![SqlValue::Blob(input.transaction_id.to_vec())],
            ))?;
            if !rows[0].rows.is_empty() {
                return Ok(CommandResult::Rejected(Json(
                    DecideCrossCellTransactionOutcome::NotPrepared,
                )));
            }
        }
        let state = if input.decision == CoordinatorDecision::Commit {
            1
        } else {
            2
        };
        let reason = if state == 2 {
            SqlValue::Blob(serde_json::to_vec(&input.decision)?)
        } else {
            SqlValue::Null
        };
        context.sql(&statement(
            "UPDATE ddb_coordinator_transactions SET state = ?1, abort_reason = ?2, decided_at_ms = ?3 \
             WHERE transaction_id = ?4 AND state = 0",
            vec![SqlValue::Integer(state), reason, SqlValue::Integer(context.now_ms()), SqlValue::Blob(input.transaction_id.to_vec())],
        ))?;
        Ok(CommandResult::Success(Json(
            DecideCrossCellTransactionOutcome::Decided(input.decision),
        )))
    }
}

/// Record evidence that one participant resolution was published.
pub struct RecordParticipantResolution;

impl Command for RecordParticipantResolution {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 4;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<CoordinatorPhaseInput>;
    type Output = Json<CoordinatorPhaseOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        phase_identity(context, &input.account_id, &input.routing_key)?;
        let (state, resolved) = match phase_row(context, &input)? {
            PhaseRow::Missing => {
                return Ok(CommandResult::Rejected(Json(
                    CoordinatorPhaseOutcome::Missing,
                )));
            }
            PhaseRow::WrongParticipant => {
                return Ok(CommandResult::Rejected(Json(
                    CoordinatorPhaseOutcome::WrongParticipant,
                )));
            }
            PhaseRow::Found {
                state, resolved, ..
            } => (state, resolved),
        };
        if state == 0 {
            return Ok(CommandResult::Rejected(Json(
                CoordinatorPhaseOutcome::WrongDecision,
            )));
        }
        if resolved.is_some() {
            return Ok(CommandResult::Success(Json(
                CoordinatorPhaseOutcome::Replay,
            )));
        }
        let sequence = i64::try_from(input.sequence)
            .ok()
            .filter(|value| *value > 0)
            .ok_or(Error::Command("invalid resolution sequence"))?;
        context.sql(&statement(
            "UPDATE ddb_coordinator_participants SET resolved_sequence = ?1 \
             WHERE transaction_id = ?2 AND position = ?3 AND resolved_sequence IS NULL",
            vec![
                SqlValue::Integer(sequence),
                SqlValue::Blob(input.transaction_id.to_vec()),
                SqlValue::Integer(i64::from(input.position)),
            ],
        ))?;
        context.sql(&statement(
            "UPDATE ddb_coordinator_transactions SET unresolved_count = unresolved_count - 1, \
             completed_at_ms = CASE WHEN unresolved_count = 1 THEN ?2 ELSE completed_at_ms END \
             WHERE transaction_id = ?1 AND unresolved_count > 0",
            vec![
                SqlValue::Blob(input.transaction_id.to_vec()),
                SqlValue::Integer(context.now_ms()),
            ],
        ))?;
        Ok(CommandResult::Success(Json(
            CoordinatorPhaseOutcome::Recorded,
        )))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadCrossCellTransactionInput {
    pub account_id: String,
    pub transaction_id: [u8; 16],
    pub routing_key: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CrossCellTransactionStatus {
    pub decision: CoordinatorDecision,
    pub participant_count: u8,
    pub prepared_count: u8,
    pub resolved_count: u8,
}

/// Read the durable decision and bounded participant progress.
pub struct ReadCrossCellTransaction;

impl Query for ReadCrossCellTransaction {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 1;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<ReadCrossCellTransactionInput>;
    type Output = Json<Option<CrossCellTransactionStatus>>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        if coordinator_target(&input.account_id, &input.routing_key)?.cell_id() != context.cell_id()
        {
            return Err(Error::Identity(
                "transaction query reached the wrong coordinator",
            ));
        }
        let rows = context.sql(&statement(
            "SELECT t.state, t.abort_reason, COUNT(p.position), COUNT(p.prepared_sequence), COUNT(p.resolved_sequence) \
             FROM ddb_coordinator_transactions t JOIN ddb_coordinator_participants p \
             ON p.transaction_id = t.transaction_id \
             WHERE t.transaction_id = ?1 AND t.account_id = ?2 GROUP BY t.transaction_id",
            vec![SqlValue::Blob(input.transaction_id.to_vec()), SqlValue::Text(input.account_id)],
        ))?;
        let Some(row) = rows[0].rows.first() else {
            return Ok(Json(None));
        };
        let [
            SqlValue::Integer(state),
            reason,
            SqlValue::Integer(total),
            SqlValue::Integer(prepared),
            SqlValue::Integer(resolved),
        ] = row.as_slice()
        else {
            return Err(Error::Command("invalid coordinator status row"));
        };
        let count = |value: i64| {
            u8::try_from(value).map_err(|_| Error::Command("invalid coordinator participant count"))
        };
        Ok(Json(Some(CrossCellTransactionStatus {
            decision: decode_decision(*state, reason)?,
            participant_count: count(*total)?,
            prepared_count: count(*prepared)?,
            resolved_count: count(*resolved)?,
        })))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadCoordinatorParticipantInput {
    pub account_id: String,
    pub transaction_id: [u8; 16],
    pub routing_key: Vec<u8>,
    pub position: u8,
}

/// Read one immutable participant payload for recovery.
pub struct ReadCoordinatorParticipant;

impl Query for ReadCoordinatorParticipant {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 2;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<ReadCoordinatorParticipantInput>;
    type Output = Json<Option<CoordinatorParticipant>>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        if coordinator_target(&input.account_id, &input.routing_key)?.cell_id() != context.cell_id()
        {
            return Err(Error::Identity(
                "participant query reached the wrong coordinator",
            ));
        }
        let rows = context.sql(&statement(
            "SELECT p.target, p.operations FROM ddb_coordinator_participants p \
             JOIN ddb_coordinator_transactions t ON t.transaction_id = p.transaction_id \
             WHERE t.transaction_id = ?1 AND t.account_id = ?2 AND p.position = ?3",
            vec![
                SqlValue::Blob(input.transaction_id.to_vec()),
                SqlValue::Text(input.account_id),
                SqlValue::Integer(i64::from(input.position)),
            ],
        ))?;
        let Some(row) = rows[0].rows.first() else {
            return Ok(Json(None));
        };
        let [SqlValue::Blob(target), SqlValue::Blob(operations)] = row.as_slice() else {
            return Err(Error::Command("invalid coordinator payload"));
        };
        Ok(Json(Some(CoordinatorParticipant {
            target: serde_json::from_slice(target)?,
            operations: serde_json::from_slice(operations)?,
        })))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UnresolvedCoordinatorParticipant {
    pub position: u8,
    pub target: CoordinatorParticipantTarget,
}

/// Read unresolved participant targets without loading request images.
pub struct ReadUnresolvedCoordinatorParticipants;

impl Query for ReadUnresolvedCoordinatorParticipants {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 4;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<ReadCrossCellTransactionInput>;
    type Output = Json<Vec<UnresolvedCoordinatorParticipant>>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        if coordinator_target(&input.account_id, &input.routing_key)?.cell_id() != context.cell_id()
        {
            return Err(Error::Identity(
                "participant targets reached the wrong coordinator",
            ));
        }
        let rows = context.sql(&statement(
            "SELECT p.position, p.target FROM ddb_coordinator_participants p \
             JOIN ddb_coordinator_transactions t ON t.transaction_id = p.transaction_id \
             WHERE t.transaction_id = ?1 AND t.account_id = ?2 \
             AND p.resolved_sequence IS NULL ORDER BY p.position",
            vec![
                SqlValue::Blob(input.transaction_id.to_vec()),
                SqlValue::Text(input.account_id),
            ],
        ))?;
        let mut targets = Vec::with_capacity(rows[0].rows.len());
        for row in &rows[0].rows {
            let [SqlValue::Integer(position), SqlValue::Blob(target)] = row.as_slice() else {
                return Err(Error::Command("invalid coordinator participant target"));
            };
            targets.push(UnresolvedCoordinatorParticipant {
                position: u8::try_from(*position)
                    .map_err(|_| Error::Command("invalid participant position"))?,
                target: serde_json::from_slice(target)?,
            });
        }
        Ok(Json(targets))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingTransactionCursor {
    pub created_at_ms: i64,
    pub transaction_id: [u8; 16],
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadPendingCrossCellTransactionsInput {
    pub after: Option<PendingTransactionCursor>,
    pub limit: u8,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingCrossCellTransaction {
    pub account_id: String,
    pub transaction_id: [u8; 16],
    pub routing_key: Vec<u8>,
    pub cursor: PendingTransactionCursor,
    pub state: PendingTransactionState,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PendingTransactionState {
    Begin,
    Commit,
    Abort,
}

/// Page unresolved transactions through one coordinator Cell's pending index.
pub struct ReadPendingCrossCellTransactions;

impl Query for ReadPendingCrossCellTransactions {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 3;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<ReadPendingCrossCellTransactionsInput>;
    type Output = Json<Vec<PendingCrossCellTransaction>>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        if input.limit == 0 || input.limit > 100 {
            return Err(Error::Command("invalid pending transaction page limit"));
        }
        let (after_ms, after_id) = input.after.map_or((i64::MIN, [0; 16]), |cursor| {
            (cursor.created_at_ms, cursor.transaction_id)
        });
        let rows = context.sql(&statement(
            "SELECT transaction_id, account_id, token, state, created_at_ms \
             FROM ddb_coordinator_transactions INDEXED BY ddb_coordinator_pending \
             WHERE unresolved_count > 0 AND (created_at_ms, transaction_id) > (?1, ?2) \
             ORDER BY created_at_ms, transaction_id LIMIT ?3",
            vec![
                SqlValue::Integer(after_ms),
                SqlValue::Blob(after_id.to_vec()),
                SqlValue::Integer(i64::from(input.limit)),
            ],
        ))?;
        let mut pending = Vec::with_capacity(rows[0].rows.len());
        for row in &rows[0].rows {
            let [
                SqlValue::Blob(id),
                SqlValue::Text(account_id),
                token,
                SqlValue::Integer(state),
                SqlValue::Integer(created_at_ms),
            ] = row.as_slice()
            else {
                return Err(Error::Command("invalid pending transaction record"));
            };
            let transaction_id: [u8; 16] = id
                .as_slice()
                .try_into()
                .map_err(|_| Error::Command("invalid pending transaction ID"))?;
            let routing_key = match token {
                SqlValue::Null => transaction_id.to_vec(),
                SqlValue::Text(token) => token.as_bytes().to_vec(),
                _ => return Err(Error::Command("invalid pending transaction token")),
            };
            let state = match state {
                0 => PendingTransactionState::Begin,
                1 => PendingTransactionState::Commit,
                2 => PendingTransactionState::Abort,
                _ => return Err(Error::Command("invalid pending transaction state")),
            };
            pending.push(PendingCrossCellTransaction {
                account_id: account_id.clone(),
                transaction_id,
                routing_key,
                cursor: PendingTransactionCursor {
                    created_at_ms: *created_at_ms,
                    transaction_id,
                },
                state,
            });
        }
        Ok(Json(pending))
    }
}
