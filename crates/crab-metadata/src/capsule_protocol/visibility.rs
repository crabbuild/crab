use std::collections::BTreeMap;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::error::{MetadataError, Result};
use crate::git_visibility::{
    GitVisibilityCheckpointTransition, GitVisibilityEdit, GitVisibilityIndex,
};

use super::valid_ref_name;

const VISIBILITY_DELTA_VERSION: u32 = 2;
const VISIBILITY_SNAPSHOT_VERSION: u32 = 3;

/// Ref-keyed reachability changes authenticated by one publication capsule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapsuleVisibilityDelta {
    version: u32,
    edits: BTreeMap<String, GitVisibilityEdit>,
}

impl CapsuleVisibilityDelta {
    /// Build a canonical delta with exactly one reachability edit per changed live ref.
    pub fn new(edits: BTreeMap<String, GitVisibilityEdit>) -> Result<Self> {
        let delta = Self {
            version: VISIBILITY_DELTA_VERSION,
            edits,
        };
        delta.validate()?;
        Ok(delta)
    }

    /// Encode this delta for an authenticated capsule section.
    pub fn encode(&self) -> Result<Bytes> {
        self.validate()?;
        serde_json::to_vec(self).map(Bytes::from).map_err(|source| {
            MetadataError::Internal(format!(
                "capsule visibility delta serialization failed: {source}"
            ))
        })
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        let delta: Self =
            serde_json::from_slice(bytes).map_err(|source| MetadataError::CorruptObject {
                path: "capsule-protocol visibility delta".to_owned(),
                reason: format!("visibility delta is invalid JSON: {source}"),
            })?;
        delta.validate().map_err(as_corruption)?;
        if delta.encode()?.as_ref() != bytes {
            return Err(corrupt("visibility delta is not canonically encoded"));
        }
        Ok(delta)
    }

    /// Return the ref-keyed reachability changes.
    #[must_use]
    pub fn edits(&self) -> &BTreeMap<String, GitVisibilityEdit> {
        &self.edits
    }

    fn validate(&self) -> Result<()> {
        if self.version != VISIBILITY_DELTA_VERSION {
            return Err(contract_error("visibility delta version is unsupported"));
        }
        if self.edits.is_empty() {
            return Err(contract_error("visibility delta must contain an edit"));
        }
        for (name, edit) in &self.edits {
            if !name.starts_with("refs/") || !valid_ref_name(name) {
                return Err(contract_error(
                    "visibility delta contains an invalid ref name",
                ));
            }
            edit.validate()?;
        }
        Ok(())
    }
}

/// Complete ref reachability state compacted into a checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapsuleVisibilitySnapshot {
    version: u32,
    refs: BTreeMap<String, Vec<String>>,
    incremental_history: BTreeMap<String, Vec<GitVisibilityCheckpointTransition>>,
}

impl CapsuleVisibilitySnapshot {
    /// Capture complete ref closures independently of a particular pack layout.
    pub fn from_index(index: &GitVisibilityIndex) -> Result<Self> {
        index.validate()?;
        let snapshot = Self {
            version: VISIBILITY_SNAPSHOT_VERSION,
            refs: index.ref_closures(),
            incremental_history: index.checkpoint_history(),
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    /// Encode this complete checkpoint state.
    pub fn encode(&self) -> Result<Bytes> {
        self.validate()?;
        serde_json::to_vec(self).map(Bytes::from).map_err(|source| {
            MetadataError::Internal(format!(
                "capsule visibility snapshot serialization failed: {source}"
            ))
        })
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        let snapshot: Self =
            serde_json::from_slice(bytes).map_err(|source| MetadataError::CorruptObject {
                path: "capsule-protocol visibility snapshot".to_owned(),
                reason: format!("visibility snapshot is invalid JSON: {source}"),
            })?;
        snapshot.validate().map_err(as_corruption)?;
        if snapshot.encode()?.as_ref() != bytes {
            return Err(corrupt("visibility snapshot is not canonically encoded"));
        }
        Ok(snapshot)
    }

    /// Return the complete ref-keyed object closures.
    #[must_use]
    pub fn refs(&self) -> &BTreeMap<String, Vec<String>> {
        &self.refs
    }

    /// Return the bounded recent transition suffix for control-only fetches.
    ///
    /// The complete history remains in the snapshot body. This suffix is a
    /// performance hint authenticated by the enclosing checkpoint; readers
    /// fall back to the complete visibility proof when a requested have is
    /// older than the retained links.
    pub fn recent_transitions(
        &self,
    ) -> Result<BTreeMap<String, Vec<GitVisibilityCheckpointTransition>>> {
        self.to_index(0, &"0".repeat(64), &"0".repeat(64))
            .map(|index| index.recent_checkpoint_history())
    }

    /// Restore the checkpoint proof under its current pack identity.
    pub fn to_index(
        &self,
        generation: u64,
        pack_index_hash: &str,
        git_validation_digest: &str,
    ) -> Result<GitVisibilityIndex> {
        let mut index = GitVisibilityIndex::new(
            generation,
            pack_index_hash,
            git_validation_digest,
            self.refs.clone(),
        )?;
        index.restore_checkpoint_history(&self.incremental_history)?;
        Ok(index)
    }

    fn validate(&self) -> Result<()> {
        if self.version != VISIBILITY_SNAPSHOT_VERSION {
            return Err(contract_error("visibility snapshot version is unsupported"));
        }
        self.to_index(0, &"0".repeat(64), &"0".repeat(64))?;
        Ok(())
    }
}

fn as_corruption(error: MetadataError) -> MetadataError {
    corrupt(error.to_string())
}

fn corrupt(reason: impl Into<String>) -> MetadataError {
    MetadataError::CorruptObject {
        path: "capsule-protocol visibility".to_owned(),
        reason: reason.into(),
    }
}

fn contract_error(reason: impl Into<String>) -> MetadataError {
    MetadataError::CapsuleContract {
        record: "visibility",
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    fn oid(character: char) -> String {
        std::iter::repeat_n(character, 40).collect()
    }

    #[test]
    fn visibility_delta_round_trips_canonically() {
        let tip = oid('a');
        let closure = BTreeSet::from([tip.clone(), oid('b')]);
        let edit = GitVisibilityEdit::replacement(None, tip, &closure);
        let delta =
            CapsuleVisibilityDelta::new(BTreeMap::from([("refs/heads/main".to_owned(), edit)]))
                .expect("valid visibility delta");

        let encoded = delta.encode().expect("encode visibility delta");

        assert_eq!(
            CapsuleVisibilityDelta::decode(&encoded).expect("decode visibility delta"),
            delta
        );
    }

    #[test]
    fn visibility_delta_rejects_non_ref_keys() {
        let tip = oid('a');
        let closure = BTreeSet::from([tip.clone()]);
        let edit = GitVisibilityEdit::replacement(None, tip, &closure);

        let error = CapsuleVisibilityDelta::new(BTreeMap::from([("HEAD".to_owned(), edit)]))
            .expect_err("non-ref visibility key must fail");

        assert!(error.to_string().contains("invalid ref name"));
    }

    #[test]
    fn visibility_snapshot_preserves_complete_ref_closures() {
        let tip = oid('a');
        let refs = BTreeMap::from([("refs/heads/main".to_owned(), vec![tip.clone(), oid('b')])]);
        let index = GitVisibilityIndex::new(7, "1".repeat(64), "2".repeat(64), refs.clone())
            .expect("valid visibility index");
        let snapshot = CapsuleVisibilitySnapshot::from_index(&index).expect("visibility snapshot");

        let encoded = snapshot.encode().expect("encode visibility snapshot");
        let decoded = CapsuleVisibilitySnapshot::decode(&encoded).expect("decode snapshot");

        assert_eq!(decoded.refs(), &refs);
    }

    #[test]
    fn visibility_snapshot_preserves_incremental_fetch_history() {
        let old_tip = oid('a');
        let new_tip = oid('b');
        let added = oid('c');
        let mut index = GitVisibilityIndex::new(
            7,
            "1".repeat(64),
            "2".repeat(64),
            BTreeMap::from([("refs/heads/main".to_owned(), vec![old_tip.clone()])]),
        )
        .expect("valid visibility index");
        index
            .apply_ref_edit(
                "refs/heads/main".to_owned(),
                &GitVisibilityEdit::from_delta_objects(
                    Some(old_tip.clone()),
                    new_tip.clone(),
                    vec![added.clone(), new_tip.clone()],
                    Vec::new(),
                ),
            )
            .expect("apply visibility edit");
        let snapshot = CapsuleVisibilitySnapshot::from_index(&index).expect("visibility snapshot");
        let decoded = CapsuleVisibilitySnapshot::decode(&snapshot.encode().unwrap()).unwrap();
        let restored = decoded
            .to_index(8, &"3".repeat(64), &"4".repeat(64))
            .expect("restore visibility snapshot");
        let old_tip = [0xaa; 20];
        let new_tip = [0xbb; 20];

        assert_eq!(
            restored.incremental_objects("refs/heads/main", &new_tip, &[old_tip]),
            Some(vec![[0xbb; 20], [0xcc; 20]])
        );
    }
}
