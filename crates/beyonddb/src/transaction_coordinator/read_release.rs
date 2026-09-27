//! Durable acknowledgement and recovery of consumed transactional read images.

use crab_cell_runtime::registry::{Command, CommandContext, CommandResult};

use super::phase::phase_identity;
use super::{
    CoordinatorPhaseInput, CoordinatorPhaseOutcome, MODULE, ReadCrossCellTransactionInput,
};
use crate::table::statement;
use crate::{Error, Json, Result, SqlValue};

/// Record that the initiating reader has assembled every saved result image.
pub struct BeginReadResultRelease;

impl Command for BeginReadResultRelease {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 6;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<ReadCrossCellTransactionInput>;
    type Output = Json<bool>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        phase_identity(context, &input.account_id, &input.routing_key)?;
        let rows = context.sql(&statement(
            "SELECT t.state, t.unresolved_count, COUNT(p.position), SUM(p.retain_operations) \
             FROM ddb_coordinator_transactions t JOIN ddb_coordinator_participants p \
             ON p.transaction_id = t.transaction_id WHERE t.transaction_id = ?1 AND t.account_id = ?2 \
             GROUP BY t.transaction_id",
            vec![SqlValue::Blob(input.transaction_id.to_vec()), SqlValue::Text(input.account_id)],
        ))?;
        let Some(
            [
                SqlValue::Integer(1),
                SqlValue::Integer(0),
                SqlValue::Integer(count),
                SqlValue::Integer(reads),
            ],
        ) = rows[0].rows.first().map(Vec::as_slice)
        else {
            return Ok(CommandResult::Rejected(Json(false)));
        };
        if *count == 0 || count != reads {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        // Keep the immutable targets discoverable until every participant has
        // acknowledged cleanup. Replayed acknowledgements count only work left.
        context.sql(&statement(
            "UPDATE ddb_coordinator_transactions SET read_release_count = \
             (SELECT COUNT(operation_chunks) FROM ddb_coordinator_participants WHERE transaction_id = ?1) \
             WHERE transaction_id = ?1",
            vec![SqlValue::Blob(input.transaction_id.to_vec())],
        ))?;
        Ok(CommandResult::Success(Json(true)))
    }
}

/// Compact a read participant's operation mapping after its images are released.
pub struct RecordReadResultRelease;

impl Command for RecordReadResultRelease {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 7;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<CoordinatorPhaseInput>;
    type Output = Json<CoordinatorPhaseOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        phase_identity(context, &input.account_id, &input.routing_key)?;
        if input.sequence == 0 || input.sequence > i64::MAX as u64 {
            return Err(Error::Command("invalid read release sequence"));
        }
        let rows = context.sql(&statement(
            "SELECT p.cell_id, p.operation_chunks, p.retain_operations, p.resolved_sequence, \
             t.state, t.unresolved_count, t.read_release_count FROM ddb_coordinator_participants p \
             JOIN ddb_coordinator_transactions t ON t.transaction_id = p.transaction_id \
             WHERE p.transaction_id = ?1 AND p.position = ?2 AND t.account_id = ?3",
            vec![
                SqlValue::Blob(input.transaction_id.to_vec()),
                SqlValue::Integer(i64::from(input.position)),
                SqlValue::Text(input.account_id),
            ],
        ))?;
        let Some(row) = rows[0].rows.first() else {
            return Ok(CommandResult::Rejected(Json(
                CoordinatorPhaseOutcome::Missing,
            )));
        };
        let [
            SqlValue::Blob(cell),
            chunks,
            SqlValue::Integer(retain),
            resolved,
            SqlValue::Integer(state),
            SqlValue::Integer(unresolved),
            SqlValue::Integer(releases),
        ] = row.as_slice()
        else {
            return Err(Error::Command("invalid read release record"));
        };
        if cell.as_slice() != input.participant_cell {
            return Ok(CommandResult::Rejected(Json(
                CoordinatorPhaseOutcome::WrongParticipant,
            )));
        }
        if *state != 1 || *unresolved != 0 || *retain != 1 || *resolved == SqlValue::Null {
            return Ok(CommandResult::Rejected(Json(
                CoordinatorPhaseOutcome::WrongDecision,
            )));
        }
        if *chunks == SqlValue::Null {
            return Ok(CommandResult::Success(Json(
                CoordinatorPhaseOutcome::Replay,
            )));
        }
        if *releases == 0 {
            return Ok(CommandResult::Rejected(Json(
                CoordinatorPhaseOutcome::WrongDecision,
            )));
        }
        context.sql(&statement(
            "DELETE FROM ddb_transaction_payloads WHERE transaction_id = ?1 AND position = ?2",
            vec![
                SqlValue::Blob(input.transaction_id.to_vec()),
                SqlValue::Integer(i64::from(input.position)),
            ],
        ))?;
        context.sql(&statement(
            "UPDATE ddb_coordinator_participants SET operation_chunks = NULL WHERE transaction_id = ?1 AND position = ?2",
            vec![SqlValue::Blob(input.transaction_id.to_vec()), SqlValue::Integer(i64::from(input.position))],
        ))?;
        context.sql(&statement(
            "UPDATE ddb_coordinator_transactions SET read_release_count = read_release_count - 1 WHERE transaction_id = ?1",
            vec![SqlValue::Blob(input.transaction_id.to_vec())],
        ))?;
        Ok(CommandResult::Success(Json(
            CoordinatorPhaseOutcome::Recorded,
        )))
    }
}
