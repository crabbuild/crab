//! Token lookup before routing a repeated transaction request.

use crab_cell_runtime::registry::{Query, QueryContext};
use serde::{Deserialize, Serialize};

use super::{CoordinatorDecision, MODULE, TOKEN_LIFETIME_MS, coordinator_target, read_decision};
use crate::table::statement;
use crate::{Error, Json, Result, SqlValue, TransactionToken};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ReadCoordinatorTokenOutcome {
    Missing,
    Mismatch,
    Found {
        transaction_id: [u8; 16],
        decision: CoordinatorDecision,
    },
}

/// Locate a live token without consulting current table routes.
pub struct ReadCoordinatorToken;

impl Query for ReadCoordinatorToken {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 5;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<TransactionToken>;
    type Output = Json<ReadCoordinatorTokenOutcome>;

    fn execute(context: &mut QueryContext<'_>, Json(token): Self::Input) -> Result<Self::Output> {
        if coordinator_target(&token.account_id, token.token.as_bytes())?.cell_id()
            != context.cell_id()
        {
            return Err(Error::Identity(
                "transaction token reached the wrong coordinator",
            ));
        }
        let rows = context.sql(&statement(
            "SELECT transaction_id, fingerprint, state, abort_chunks FROM ddb_coordinator_transactions \
             WHERE token = ?1 AND account_id = ?2 \
             AND (completed_at_ms IS NULL OR completed_at_ms > ?3)",
            vec![SqlValue::Text(token.token), SqlValue::Text(token.account_id), SqlValue::Integer(context.now_ms().saturating_sub(TOKEN_LIFETIME_MS))],
        ))?;
        let Some(row) = rows[0].rows.first() else {
            return Ok(Json(ReadCoordinatorTokenOutcome::Missing));
        };
        let [
            SqlValue::Blob(id),
            SqlValue::Text(fingerprint),
            SqlValue::Integer(state),
            reason,
        ] = row.as_slice()
        else {
            return Err(Error::Command("invalid coordinator token row"));
        };
        if fingerprint != &token.fingerprint {
            return Ok(Json(ReadCoordinatorTokenOutcome::Mismatch));
        }
        let transaction_id = id
            .as_slice()
            .try_into()
            .map_err(|_| Error::Command("invalid coordinator transaction ID"))?;
        Ok(Json(ReadCoordinatorTokenOutcome::Found {
            transaction_id,
            decision: read_decision(|batch| context.sql(batch), transaction_id, *state, reason)?,
        }))
    }
}
