use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::error::{MetadataError, Result};
use crate::validation::validate_content_hash;

const TRANSACTION_RECORD_VERSION: u32 = 2;
/// Largest canonical multi-ref transaction record accepted from storage.
pub const MAX_CAPSULE_TRANSACTION_RECORD_BYTES: u64 = 16 * 1024;

/// Durable state of one uniquely identified multi-ref publication attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapsuleTransactionStatus {
    Preparing,
    Committed,
    Aborted,
}

/// Per-attempt coordinator whose CAS is the multi-ref commit point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapsuleTransactionRecord {
    version: u32,
    activation_id: String,
    transaction_id: String,
    status: CapsuleTransactionStatus,
}

impl CapsuleTransactionRecord {
    /// Create the initial state for one unique publication attempt.
    pub fn preparing(activation_id: String, transaction_id: String) -> Result<Self> {
        let record = Self {
            version: TRANSACTION_RECORD_VERSION,
            activation_id,
            transaction_id,
            status: CapsuleTransactionStatus::Preparing,
        };
        record.validate()?;
        Ok(record)
    }

    /// Decode and verify one canonical transaction record.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let record: Self =
            serde_json::from_slice(bytes).map_err(|source| MetadataError::CorruptObject {
                path: "capsule-protocol transaction record".to_owned(),
                reason: format!("transaction record is invalid JSON: {source}"),
            })?;
        record.validate().map_err(as_corruption)?;
        if record.encode()?.as_ref() != bytes {
            return Err(corrupt("transaction record is not canonically encoded"));
        }
        Ok(record)
    }

    /// Encode this record as deterministic JSON.
    pub fn encode(&self) -> Result<Bytes> {
        self.validate()?;
        serde_json::to_vec(self).map(Bytes::from).map_err(|source| {
            MetadataError::Internal(format!(
                "capsule transaction record serialization failed: {source}"
            ))
        })
    }

    #[must_use]
    pub fn activation_id(&self) -> &str {
        &self.activation_id
    }

    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    #[must_use]
    pub fn status(&self) -> CapsuleTransactionStatus {
        self.status
    }

    /// Build the committed successor of this exact preparing record.
    pub fn commit(&self) -> Result<Self> {
        self.transition(CapsuleTransactionStatus::Committed)
    }

    /// Build the aborted successor of this exact preparing record.
    pub fn abort(&self) -> Result<Self> {
        self.transition(CapsuleTransactionStatus::Aborted)
    }

    fn transition(&self, status: CapsuleTransactionStatus) -> Result<Self> {
        if self.status != CapsuleTransactionStatus::Preparing {
            return Err(contract_error(
                "only a preparing transaction record can reach a terminal state",
            ));
        }
        let record = Self {
            status,
            ..self.clone()
        };
        record.validate()?;
        Ok(record)
    }

    fn validate(&self) -> Result<()> {
        if self.version != TRANSACTION_RECORD_VERSION {
            return Err(contract_error("transaction record version is unsupported"));
        }
        validate_content_hash(
            &self.activation_id,
            "activation id",
            "capsule-protocol transaction record",
        )?;
        validate_content_hash(
            &self.transaction_id,
            "transaction id",
            "capsule-protocol transaction record",
        )
    }
}

fn as_corruption(error: MetadataError) -> MetadataError {
    corrupt(error.to_string())
}

fn corrupt(reason: impl Into<String>) -> MetadataError {
    MetadataError::CorruptObject {
        path: "capsule-protocol transaction record".to_owned(),
        reason: reason.into(),
    }
}

fn contract_error(reason: impl Into<String>) -> MetadataError {
    MetadataError::CapsuleContract {
        record: "transaction record",
        reason: reason.into(),
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn transaction_record_round_trips_canonically() {
        let record = CapsuleTransactionRecord::preparing("1".repeat(64), "2".repeat(64))
            .unwrap()
            .commit()
            .unwrap();

        assert_eq!(
            CapsuleTransactionRecord::decode(&record.encode().unwrap()).unwrap(),
            record
        );
    }

    #[test]
    fn terminal_transaction_record_cannot_transition_again() {
        for record in [
            CapsuleTransactionRecord::preparing("1".repeat(64), "2".repeat(64))
                .unwrap()
                .commit()
                .unwrap(),
            CapsuleTransactionRecord::preparing("3".repeat(64), "4".repeat(64))
                .unwrap()
                .abort()
                .unwrap(),
        ] {
            assert!(record.commit().is_err());
            assert!(record.abort().is_err());
        }
    }
}
