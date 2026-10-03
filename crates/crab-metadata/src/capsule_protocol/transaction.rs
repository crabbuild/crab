use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::error::{MetadataError, Result};
use crate::validation::{validate_content_hash, validate_sha1};

use super::valid_ref_name;

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
    publication_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plan_id: Option<String>,
    edits: Vec<CapsuleRefEdit>,
}

impl CapsuleTransaction {
    /// Create a canonical transaction, sorting edits and rejecting duplicate refs.
    pub fn new(base_root_digest: &str, edits: Vec<CapsuleRefEdit>) -> Result<Self> {
        let publication_id = blake3::hash(uuid::Uuid::now_v7().as_bytes())
            .to_hex()
            .to_string();
        Self::new_inner(base_root_digest, publication_id, None, edits)
    }

    /// Create a deterministic source transaction from one authorized view transaction.
    pub fn for_protected_source(
        base_root_digest: &str,
        candidate_transaction_id: &str,
        edits: Vec<CapsuleRefEdit>,
    ) -> Result<Self> {
        validate_content_hash(
            candidate_transaction_id,
            "candidate transaction id",
            "capsule-protocol transaction",
        )?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"crab protected source publication v1\0");
        hasher.update(base_root_digest.as_bytes());
        hasher.update(candidate_transaction_id.as_bytes());
        Self::new_inner(
            base_root_digest,
            hasher.finalize().to_hex().to_string(),
            None,
            edits,
        )
    }

    /// Create a transaction whose identity is bound to one reviewed mirror plan.
    pub fn for_plan(
        base_root_digest: &str,
        plan_id: &str,
        edits: Vec<CapsuleRefEdit>,
    ) -> Result<Self> {
        validate_content_hash(plan_id, "mirror plan id", "capsule-protocol transaction")?;
        Self::new_inner(
            base_root_digest,
            plan_id.to_owned(),
            Some(plan_id.to_owned()),
            edits,
        )
    }

    fn new_inner(
        base_root_digest: &str,
        publication_id: String,
        plan_id: Option<String>,
        mut edits: Vec<CapsuleRefEdit>,
    ) -> Result<Self> {
        validate_content_hash(
            base_root_digest,
            "transaction base root digest",
            "capsule-protocol transaction",
        )?;
        validate_content_hash(
            &publication_id,
            "transaction publication id",
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
            publication_id,
            plan_id,
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

    /// Return the unique publication attempt bound into this transaction.
    #[must_use]
    pub fn publication_id(&self) -> &str {
        &self.publication_id
    }

    /// Return the reviewed mirror plan committed by this transaction, if any.
    #[must_use]
    pub fn plan_id(&self) -> Option<&str> {
        self.plan_id.as_deref()
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
    validate_content_hash(
        &transaction.publication_id,
        "transaction publication id",
        "capsule-protocol transaction",
    )?;
    if let Some(plan_id) = &transaction.plan_id {
        validate_content_hash(plan_id, "mirror plan id", "capsule-protocol transaction")?;
    }
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
    if !edit.ref_name.starts_with("refs/") || !valid_ref_name(&edit.ref_name) {
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

    #[test]
    fn unplanned_repeated_edits_have_distinct_transaction_identities() {
        let edit = CapsuleRefEdit::new("refs/tags/v1", None, Some("2".repeat(40)), None);

        let first = CapsuleTransaction::new(&"1".repeat(64), vec![edit.clone()]).unwrap();
        let second = CapsuleTransaction::new(&"1".repeat(64), vec![edit]).unwrap();

        assert_ne!(first.publication_id(), second.publication_id());
        assert_ne!(first.id().unwrap(), second.id().unwrap());
    }

    #[test]
    fn planned_retries_have_the_same_transaction_identity() {
        let plan_id = "3".repeat(64);
        let edit = CapsuleRefEdit::new("refs/heads/main", None, Some("2".repeat(40)), None);

        let first =
            CapsuleTransaction::for_plan(&"1".repeat(64), &plan_id, vec![edit.clone()]).unwrap();
        let second = CapsuleTransaction::for_plan(&"1".repeat(64), &plan_id, vec![edit]).unwrap();

        assert_eq!(first.publication_id(), plan_id);
        assert_eq!(first.id().unwrap(), second.id().unwrap());
    }

    #[test]
    fn protected_source_retries_bind_candidate_and_source_base() {
        let candidate = "3".repeat(64);
        let edit = CapsuleRefEdit::new("refs/heads/main", None, Some("2".repeat(40)), None);
        let first = CapsuleTransaction::for_protected_source(
            &"1".repeat(64),
            &candidate,
            vec![edit.clone()],
        )
        .unwrap();
        let second = CapsuleTransaction::for_protected_source(
            &"1".repeat(64),
            &candidate,
            vec![edit.clone()],
        )
        .unwrap();
        let different_base =
            CapsuleTransaction::for_protected_source(&"4".repeat(64), &candidate, vec![edit])
                .unwrap();

        assert_eq!(first.id().unwrap(), second.id().unwrap());
        assert_ne!(first.id().unwrap(), different_base.id().unwrap());
        assert!(first.plan_id().is_none());
    }
}
