//! Account-scoped identity for coordinator transaction replay.

use serde::{Deserialize, Serialize};

pub(crate) const TOKEN_LIFETIME_MS: i64 = 10 * 60 * 1_000;

/// A client request token already validated by ExtendDB.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransactionToken {
    pub account_id: String,
    pub token: String,
    pub fingerprint: String,
}
