//! Account-scoped transaction token claims and Cell-local applied receipts.

use crab_cell_runtime::registry::{Command, CommandContext, CommandResult, Query, QueryContext};
use serde::{Deserialize, Serialize};

use crate::table::statement;
use crate::{Error, Json, MODULE, Result, SqlValue, account_target};

pub(crate) const TOKEN_LIFETIME_MS: i64 = 10 * 60 * 1_000;

/// A client request token already validated by ExtendDB.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransactionToken {
    pub account_id: String,
    pub token: String,
    pub fingerprint: String,
}

/// The Cell first selected for a transaction token.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TransactionDestination {
    Account,
    Data { partition: Vec<u8>, epoch: u64 },
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ClaimTransactionTokenInput {
    pub token: TransactionToken,
    pub destination: TransactionDestination,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ClaimTransactionTokenOutcome {
    Claimed(TransactionDestination),
    Mismatch,
}

pub struct ClaimTransactionToken;

impl Command for ClaimTransactionToken {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 16;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<ClaimTransactionTokenInput>;
    type Output = Json<ClaimTransactionTokenOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        if account_target(&input.token.account_id)? != *context.target() {
            return Err(Error::Identity(
                "transaction token reached the wrong account",
            ));
        }
        let cutoff = context.now_ms().saturating_sub(TOKEN_LIFETIME_MS);
        let rows = context.sql(&statement(
            "SELECT fingerprint, destination FROM ddb_transaction_claims \
             WHERE token = ?1 AND created_at_ms > ?2",
            vec![
                SqlValue::Text(input.token.token.clone()),
                SqlValue::Integer(cutoff),
            ],
        ))?;
        if let Some(row) = rows[0].rows.first() {
            let [SqlValue::Text(fingerprint), SqlValue::Blob(destination)] = row.as_slice() else {
                return Err(Error::Command("invalid transaction claim row"));
            };
            if fingerprint != &input.token.fingerprint {
                return Ok(CommandResult::Rejected(Json(
                    ClaimTransactionTokenOutcome::Mismatch,
                )));
            }
            return Ok(CommandResult::Success(Json(
                ClaimTransactionTokenOutcome::Claimed(serde_json::from_slice(destination)?),
            )));
        }
        context.sql(&statement(
            "INSERT INTO ddb_transaction_claims (token, fingerprint, destination, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4) ON CONFLICT(token) DO UPDATE SET \
             fingerprint = excluded.fingerprint, destination = excluded.destination, \
             created_at_ms = excluded.created_at_ms",
            vec![
                SqlValue::Text(input.token.token),
                SqlValue::Text(input.token.fingerprint),
                SqlValue::Blob(serde_json::to_vec(&input.destination)?),
                SqlValue::Integer(context.now_ms()),
            ],
        ))?;
        prune_expired(context, TokenStore::Claims, cutoff)?;
        Ok(CommandResult::Success(Json(
            ClaimTransactionTokenOutcome::Claimed(input.destination),
        )))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ReadTransactionClaimOutcome {
    Missing,
    Claimed(TransactionDestination),
    Mismatch,
}

pub struct ReadTransactionClaim;

impl Query for ReadTransactionClaim {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 19;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<TransactionToken>;
    type Output = Json<ReadTransactionClaimOutcome>;

    fn execute(context: &mut QueryContext<'_>, Json(token): Self::Input) -> Result<Self::Output> {
        if account_target(&token.account_id)?.cell_id() != context.cell_id() {
            return Err(Error::Identity(
                "transaction token reached the wrong account",
            ));
        }
        let rows = context.sql(&statement(
            "SELECT fingerprint, destination FROM ddb_transaction_claims \
             WHERE token = ?1 AND created_at_ms > ?2",
            vec![
                SqlValue::Text(token.token),
                SqlValue::Integer(context.now_ms().saturating_sub(TOKEN_LIFETIME_MS)),
            ],
        ))?;
        let Some(row) = rows[0].rows.first() else {
            return Ok(Json(ReadTransactionClaimOutcome::Missing));
        };
        let [SqlValue::Text(fingerprint), SqlValue::Blob(destination)] = row.as_slice() else {
            return Err(Error::Command("invalid transaction claim row"));
        };
        if fingerprint != &token.fingerprint {
            return Ok(Json(ReadTransactionClaimOutcome::Mismatch));
        }
        Ok(Json(ReadTransactionClaimOutcome::Claimed(
            serde_json::from_slice(destination)?,
        )))
    }
}

pub enum AppliedToken {
    Fresh,
    Replay,
    Mismatch,
}

pub fn applied_token(
    context: &mut CommandContext<'_, '_>,
    token: &TransactionToken,
) -> Result<AppliedToken> {
    let cutoff = context.now_ms().saturating_sub(TOKEN_LIFETIME_MS);
    let rows = context.sql(&statement(
        "SELECT fingerprint FROM ddb_transaction_applied \
         WHERE account_id = ?1 AND token = ?2 AND created_at_ms > ?3",
        vec![
            SqlValue::Text(token.account_id.clone()),
            SqlValue::Text(token.token.clone()),
            SqlValue::Integer(cutoff),
        ],
    ))?;
    let Some(row) = rows[0].rows.first() else {
        return Ok(AppliedToken::Fresh);
    };
    let [SqlValue::Text(fingerprint)] = row.as_slice() else {
        return Err(Error::Command("invalid applied transaction token row"));
    };
    Ok(if fingerprint == &token.fingerprint {
        AppliedToken::Replay
    } else {
        AppliedToken::Mismatch
    })
}

pub fn record_applied_token(
    context: &mut CommandContext<'_, '_>,
    token: &TransactionToken,
) -> Result<()> {
    context.sql(&statement(
        "INSERT INTO ddb_transaction_applied (account_id, token, fingerprint, created_at_ms) \
         VALUES (?1, ?2, ?3, ?4) ON CONFLICT(account_id, token) DO UPDATE SET \
         fingerprint = excluded.fingerprint, created_at_ms = excluded.created_at_ms",
        vec![
            SqlValue::Text(token.account_id.clone()),
            SqlValue::Text(token.token.clone()),
            SqlValue::Text(token.fingerprint.clone()),
            SqlValue::Integer(context.now_ms()),
        ],
    ))?;
    prune_expired(
        context,
        TokenStore::Applied,
        context.now_ms().saturating_sub(TOKEN_LIFETIME_MS),
    )
}

enum TokenStore {
    Claims,
    Applied,
}

fn prune_expired(
    context: &mut CommandContext<'_, '_>,
    table: TokenStore,
    cutoff: i64,
) -> Result<()> {
    let sql = match table {
        TokenStore::Claims => {
            "DELETE FROM ddb_transaction_claims WHERE rowid IN \
             (SELECT rowid FROM ddb_transaction_claims WHERE created_at_ms <= ?1 LIMIT 64)"
        }
        TokenStore::Applied => {
            "DELETE FROM ddb_transaction_applied WHERE rowid IN \
             (SELECT rowid FROM ddb_transaction_applied WHERE created_at_ms <= ?1 LIMIT 64)"
        }
    };
    context.sql(&statement(sql, vec![SqlValue::Integer(cutoff)]))?;
    Ok(())
}
