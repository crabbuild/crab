use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::error::{MetadataError, Result};
use crate::validation::{validate_content_hash, validate_sha1};

use super::{CapsulePointer, valid_ref_name};

const REF_HEAD_VERSION: u32 = 2;
/// Maximum number of independently mutable ref heads accepted for one repository.
pub const MAX_CAPSULE_REF_HEADS: usize = 1_000_000;
/// Maximum immutable run segments retained by one independently mutable ref.
pub const MAX_CAPSULE_REF_FRONTIER: usize = 64;

/// One visible or prepared ref value and its bounded immutable capsule frontier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapsuleRefState {
    oid: Option<String>,
    peeled_oid: Option<String>,
    transaction_id: Option<String>,
    frontier: Vec<CapsulePointer>,
}

impl CapsuleRefState {
    /// Return the ref object ID, or `None` for an unborn or deleted ref.
    #[must_use]
    pub fn oid(&self) -> Option<&str> {
        self.oid.as_deref()
    }

    /// Return the optional peeled object ID for an annotated tag.
    #[must_use]
    pub fn peeled_oid(&self) -> Option<&str> {
        self.peeled_oid.as_deref()
    }

    /// Return the last transaction committed to this ref after the compacted root.
    #[must_use]
    pub fn transaction_id(&self) -> Option<&str> {
        self.transaction_id.as_deref()
    }

    /// Return the bounded immutable capsule runs needed by this ref.
    #[must_use]
    pub fn frontier(&self) -> &[CapsulePointer] {
        &self.frontier
    }

    pub(crate) fn successor(
        &self,
        oid: Option<String>,
        peeled_oid: Option<String>,
        transaction_id: String,
        frontier: Vec<CapsulePointer>,
    ) -> Result<Self> {
        let state = Self {
            oid,
            peeled_oid,
            transaction_id: Some(transaction_id),
            frontier,
        };
        validate_state(&state)?;
        Ok(state)
    }
}

/// Per-ref mutable authority; prepared state is visible only after its transaction commits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapsuleRefHead {
    version: u32,
    ref_name: String,
    committed: CapsuleRefState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prepared: Option<CapsulePreparedRefState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CapsulePreparedRefState {
    activation_id: String,
    state: CapsuleRefState,
}

impl CapsuleRefHead {
    /// Build an empty journal head over one value already compacted into the root.
    pub fn from_root(
        ref_name: &str,
        oid: Option<String>,
        peeled_oid: Option<String>,
    ) -> Result<Self> {
        let head = Self {
            version: REF_HEAD_VERSION,
            ref_name: ref_name.to_owned(),
            committed: CapsuleRefState {
                oid,
                peeled_oid,
                transaction_id: None,
                frontier: Vec::new(),
            },
            prepared: None,
        };
        validate_head(&head)?;
        Ok(head)
    }

    /// Decode and validate one canonical ref-head body.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let head: Self =
            serde_json::from_slice(bytes).map_err(|source| MetadataError::CorruptObject {
                path: "capsule-protocol ref head".to_owned(),
                reason: format!("ref head is invalid JSON: {source}"),
            })?;
        validate_head(&head).map_err(as_corruption)?;
        if head.encode()?.as_ref() != bytes {
            return Err(corrupt("ref head is not canonically encoded"));
        }
        Ok(head)
    }

    /// Encode this head as deterministic JSON.
    pub fn encode(&self) -> Result<Bytes> {
        validate_head(self)?;
        serde_json::to_vec(self).map(Bytes::from).map_err(|source| {
            MetadataError::Internal(format!("capsule ref-head serialization failed: {source}"))
        })
    }

    /// Return the canonical ref name protected by this head.
    #[must_use]
    pub fn ref_name(&self) -> &str {
        &self.ref_name
    }

    /// Resolve the state visible at a committed-transaction snapshot.
    #[must_use]
    pub fn visible<'a>(
        &'a self,
        active_transactions: &std::collections::BTreeSet<String>,
    ) -> &'a CapsuleRefState {
        self.prepared
            .as_ref()
            .filter(|prepared| active_transactions.contains(&prepared.activation_id))
            .map(|prepared| &prepared.state)
            .unwrap_or(&self.committed)
    }

    /// Return the publication attempt coordinating prepared state, if any.
    #[must_use]
    pub fn prepared_activation_id(&self) -> Option<&str> {
        self.prepared
            .as_ref()
            .map(|prepared| prepared.activation_id.as_str())
    }

    /// Return a direct, single-ref successor whose head CAS is the commit point.
    pub fn commit(&self, state: CapsuleRefState) -> Result<Self> {
        let head = Self {
            version: REF_HEAD_VERSION,
            ref_name: self.ref_name.clone(),
            committed: state,
            prepared: None,
        };
        validate_head(&head)?;
        Ok(head)
    }

    /// Prepare a multi-ref successor while retaining the currently visible state.
    pub fn prepare(
        &self,
        visible: CapsuleRefState,
        activation_id: String,
        state: CapsuleRefState,
    ) -> Result<Self> {
        let head = Self {
            version: REF_HEAD_VERSION,
            ref_name: self.ref_name.clone(),
            committed: visible,
            prepared: Some(CapsulePreparedRefState {
                activation_id,
                state,
            }),
        };
        validate_head(&head)?;
        Ok(head)
    }

    /// Build a successor state from the visible value.
    pub fn successor_state(
        &self,
        active_transactions: &std::collections::BTreeSet<String>,
        oid: Option<String>,
        peeled_oid: Option<String>,
        transaction_id: String,
        frontier: Vec<CapsulePointer>,
    ) -> Result<CapsuleRefState> {
        self.visible(active_transactions)
            .successor(oid, peeled_oid, transaction_id, frontier)
    }
}

/// Encode one canonical ref name as a reversible object-key component.
#[must_use]
pub fn capsule_ref_name_key(ref_name: &str) -> String {
    let mut key = String::with_capacity(ref_name.len() * 2);
    for byte in ref_name.bytes() {
        use std::fmt::Write as _;
        let _ = write!(key, "{byte:02x}");
    }
    key
}

/// Decode one ref-head object-key component back to its canonical ref name.
pub fn capsule_ref_name_from_key(key: &str) -> Result<String> {
    if key.is_empty() || !key.len().is_multiple_of(2) {
        return Err(contract_error("ref-head object key is invalid"));
    }
    let bytes = key
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair)
                .map_err(|_| contract_error("ref-head object key is invalid UTF-8"))?;
            u8::from_str_radix(pair, 16)
                .map_err(|_| contract_error("ref-head object key is not hexadecimal"))
        })
        .collect::<Result<Vec<_>>>()?;
    let ref_name = String::from_utf8(bytes)
        .map_err(|_| contract_error("decoded ref-head object key is not UTF-8"))?;
    if !ref_name.starts_with("refs/") || !valid_ref_name(&ref_name) {
        return Err(contract_error(
            "decoded ref-head object key is not a canonical ref",
        ));
    }
    Ok(ref_name)
}

fn validate_head(head: &CapsuleRefHead) -> Result<()> {
    if head.version != REF_HEAD_VERSION
        || !head.ref_name.starts_with("refs/")
        || !valid_ref_name(&head.ref_name)
    {
        return Err(contract_error("ref head identity is invalid"));
    }
    validate_state(&head.committed)?;
    if let Some(prepared) = &head.prepared {
        validate_content_hash(
            &prepared.activation_id,
            "prepared activation id",
            "capsule-protocol ref head",
        )?;
        validate_state(&prepared.state)?;
        if prepared.state.transaction_id.is_none()
            || prepared.state.transaction_id == head.committed.transaction_id
        {
            return Err(contract_error(
                "prepared ref-head state must name a new transaction",
            ));
        }
    }
    Ok(())
}

fn validate_state(state: &CapsuleRefState) -> Result<()> {
    if state.oid.is_none() && state.peeled_oid.is_some() {
        return Err(contract_error(
            "deleted ref state cannot retain a peeled OID",
        ));
    }
    for oid in [&state.oid, &state.peeled_oid].into_iter().flatten() {
        validate_sha1(oid, "ref-head object id", "capsule-protocol ref head")?;
    }
    if let Some(transaction_id) = &state.transaction_id {
        validate_content_hash(
            transaction_id,
            "ref-head transaction id",
            "capsule-protocol ref head",
        )?;
    }
    if state.frontier.len() > MAX_CAPSULE_REF_FRONTIER {
        return Err(contract_error(
            "ref-head capsule frontier exceeds its bound",
        ));
    }
    if state.transaction_id.is_none() != state.frontier.is_empty() {
        return Err(contract_error(
            "ref-head transaction and capsule frontier must be present together",
        ));
    }
    let mut transactions = std::collections::BTreeSet::new();
    for pointer in &state.frontier {
        CapsulePointer::new(
            pointer.hash(),
            pointer.size(),
            pointer.level(),
            pointer.transaction_ids().to_vec(),
            pointer.newest_base_root_digest(),
        )?;
        for transaction_id in pointer.transaction_ids() {
            if !transactions.insert(transaction_id) {
                return Err(contract_error(
                    "ref-head capsule frontier repeats a transaction",
                ));
            }
        }
    }
    if state.transaction_id.as_deref()
        != state
            .frontier
            .last()
            .and_then(|pointer| pointer.transaction_ids().last())
            .map(String::as_str)
    {
        return Err(contract_error(
            "ref-head position must match its newest capsule transaction",
        ));
    }
    Ok(())
}

fn as_corruption(error: MetadataError) -> MetadataError {
    corrupt(error.to_string())
}

fn corrupt(reason: impl Into<String>) -> MetadataError {
    MetadataError::CorruptObject {
        path: "capsule-protocol ref head".to_owned(),
        reason: reason.into(),
    }
}

fn contract_error(reason: impl Into<String>) -> MetadataError {
    MetadataError::CapsuleContract {
        record: "ref head",
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn pointer(transaction_id: &str) -> CapsulePointer {
        CapsulePointer::new(
            &"3".repeat(64),
            1,
            0,
            vec![transaction_id.to_owned()],
            &"4".repeat(64),
        )
        .unwrap()
    }

    #[test]
    fn committed_transaction_snapshot_selects_prepared_state_atomically() {
        let head = CapsuleRefHead::from_root("refs/heads/main", None, None).unwrap();
        let transaction_id = "1".repeat(64);
        let state = head
            .successor_state(
                &BTreeSet::new(),
                Some("2".repeat(40)),
                None,
                transaction_id.clone(),
                vec![pointer(&transaction_id)],
            )
            .unwrap();
        let prepared = head
            .prepare(
                head.visible(&BTreeSet::new()).clone(),
                "5".repeat(64),
                state,
            )
            .unwrap();
        let expected = "2".repeat(40);

        assert_eq!(prepared.visible(&BTreeSet::new()).oid(), None);
        assert_eq!(
            prepared.visible(&BTreeSet::from(["5".repeat(64)])).oid(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn ref_head_round_trips_canonically() {
        let head =
            CapsuleRefHead::from_root("refs/tags/v1", Some("2".repeat(40)), Some("3".repeat(40)))
                .unwrap();

        assert_eq!(
            CapsuleRefHead::decode(&head.encode().unwrap()).unwrap(),
            head
        );
    }

    #[test]
    fn ref_name_object_key_is_reversible() {
        let name = "refs/heads/agents/a";
        assert_eq!(
            capsule_ref_name_from_key(&capsule_ref_name_key(name)).unwrap(),
            name
        );
    }
}
