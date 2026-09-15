use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::error::{MetadataError, Result};
use crate::validation::{validate_content_hash, validate_sha1};

const TRANSACTION_VERSION: u32 = 2;

/// One exact ref edit authenticated by a capsule transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapsuleRefEdit {
    ref_name: String,
    expected_old: Option<String>,
    new_oid: Option<String>,
    peeled_oid: Option<String>,
}

impl CapsuleRefEdit {
    /// Describe one expected-old ref replacement, creation, or deletion.
    #[must_use]
    pub fn new(
        ref_name: impl Into<String>,
        expected_old: Option<String>,
        new_oid: Option<String>,
        peeled_oid: Option<String>,
    ) -> Self {
        Self {
            ref_name: ref_name.into(),
            expected_old,
            new_oid,
            peeled_oid,
        }
    }

    /// Return the canonical ref name.
    #[must_use]
    pub fn ref_name(&self) -> &str {
        &self.ref_name
    }

    /// Return the object ID that must be visible before publication.
    #[must_use]
    pub fn expected_old(&self) -> Option<&str> {
        self.expected_old.as_deref()
    }

    /// Return the object ID made visible by publication.
    #[must_use]
    pub fn new_oid(&self) -> Option<&str> {
        self.new_oid.as_deref()
    }

    /// Return the optional peeled target for an annotated tag.
    #[must_use]
    pub fn peeled_oid(&self) -> Option<&str> {
        self.peeled_oid.as_deref()
    }
}

/// Canonical ref transaction embedded as the first capsule section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapsuleTransaction {
    version: u32,
    base_root_digest: String,
    edits: Vec<CapsuleRefEdit>,
}

impl CapsuleTransaction {
    /// Create a canonical transaction, sorting edits and rejecting duplicate refs.
    pub fn new(base_root_digest: &str, mut edits: Vec<CapsuleRefEdit>) -> Result<Self> {
        validate_content_hash(
            base_root_digest,
            "transaction base root digest",
            "capsule-protocol transaction",
        )?;
        if edits.is_empty() {
            return Err(contract_error("transaction must edit at least one ref"));
        }
        edits.sort_unstable_by(|left, right| left.ref_name.cmp(&right.ref_name));
        for pair in edits.windows(2) {
            if pair[0].ref_name == pair[1].ref_name {
                return Err(contract_error("transaction contains duplicate ref edits"));
            }
        }
        for edit in &edits {
            validate_edit(edit)?;
        }
        Ok(Self {
            version: TRANSACTION_VERSION,
            base_root_digest: base_root_digest.to_owned(),
            edits,
        })
    }

    /// Encode the transaction into its deterministic capsule section.
    pub fn encode(&self) -> Result<Bytes> {
        validate_transaction(self)?;
        serde_json::to_vec(self).map(Bytes::from).map_err(|source| {
            MetadataError::Internal(format!(
                "capsule transaction serialization failed: {source}"
            ))
        })
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        let transaction: Self =
            serde_json::from_slice(bytes).map_err(|source| MetadataError::CorruptObject {
                path: "capsule-protocol capsule transaction".to_owned(),
                reason: format!("transaction is invalid JSON: {source}"),
            })?;
        validate_transaction(&transaction).map_err(|error| MetadataError::CorruptObject {
            path: "capsule-protocol capsule transaction".to_owned(),
            reason: error.to_string(),
        })?;
        if transaction.encode()?.as_ref() != bytes {
            return Err(MetadataError::CorruptObject {
                path: "capsule-protocol capsule transaction".to_owned(),
                reason: "transaction is not canonically encoded".to_owned(),
            });
        }
        Ok(transaction)
    }

    /// Return the BLAKE3 identity of the canonical transaction bytes.
    pub fn id(&self) -> Result<String> {
        Ok(blake3::hash(&self.encode()?).to_hex().to_string())
    }

    /// Return the exact repository-root digest this transaction extends.
    #[must_use]
    pub fn base_root_digest(&self) -> &str {
        &self.base_root_digest
    }

    /// Return the canonically ordered ref edits.
    #[must_use]
    pub fn edits(&self) -> &[CapsuleRefEdit] {
        &self.edits
    }
}

fn validate_transaction(transaction: &CapsuleTransaction) -> Result<()> {
    if transaction.version != TRANSACTION_VERSION {
        return Err(contract_error(format!(
            "transaction must use version {TRANSACTION_VERSION}"
        )));
    }
    validate_content_hash(
        &transaction.base_root_digest,
        "transaction base root digest",
        "capsule-protocol transaction",
    )?;
    if transaction.edits.is_empty() {
        return Err(contract_error("transaction must edit at least one ref"));
    }
    if transaction
        .edits
        .windows(2)
        .any(|pair| pair[0].ref_name >= pair[1].ref_name)
    {
        return Err(contract_error(
            "transaction ref edits must be strictly ordered and unique",
        ));
    }
    for edit in &transaction.edits {
        validate_edit(edit)?;
    }
    Ok(())
}

fn validate_edit(edit: &CapsuleRefEdit) -> Result<()> {
    if !edit.ref_name.starts_with("refs/")
        || crab_git::refname::validate_push_refname(&edit.ref_name).is_err()
    {
        return Err(contract_error("transaction contains an invalid ref name"));
    }
    if edit.expected_old.is_none() && edit.new_oid.is_none() {
        return Err(contract_error(
            "transaction edit has neither old nor new object id",
        ));
    }
    if edit.expected_old == edit.new_oid {
        return Err(contract_error("transaction contains a no-op ref edit"));
    }
    for oid in [&edit.expected_old, &edit.new_oid, &edit.peeled_oid]
        .into_iter()
        .flatten()
    {
        validate_sha1(oid, "transaction object id", "capsule-protocol transaction")?;
    }
    if edit.new_oid.is_none() && edit.peeled_oid.is_some() {
        return Err(contract_error(
            "deleted ref cannot retain a peeled object id",
        ));
    }
    Ok(())
}

fn contract_error(reason: impl Into<String>) -> MetadataError {
    MetadataError::CapsuleContract {
        record: "transaction",
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transaction_reuses_canonical_git_ref_validation() {
        let error = CapsuleTransaction::new(
            &"1".repeat(64),
            vec![CapsuleRefEdit::new(
                "refs/heads/main.lock",
                None,
                Some("2".repeat(40)),
                None,
            )],
        )
        .expect_err("Git-reserved ref suffix must fail");

        assert!(matches!(
            error,
            MetadataError::CapsuleContract {
                record: "transaction",
                ..
            }
        ));
    }
}
