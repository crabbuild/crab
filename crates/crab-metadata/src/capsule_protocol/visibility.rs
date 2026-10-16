use std::collections::BTreeMap;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::error::{MetadataError, Result};
use crate::git_visibility::GitVisibilityEdit;

use super::valid_ref_name;

const VISIBILITY_DELTA_VERSION: u32 = 3;

mod paged;
pub(super) use paged::Descriptor as VisibilityPages;
#[cfg(feature = "storage")]
pub use paged::{CapsuleVisibilityReader, VisibilityChange, VisibilityEditHeader};

/// Ref-keyed reachability changes authenticated by one publication capsule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapsuleVisibilityDelta {
    edits: BTreeMap<String, GitVisibilityEdit>,
}

impl CapsuleVisibilityDelta {
    /// Build a canonical delta with exactly one reachability edit per changed live ref.
    pub fn new(edits: BTreeMap<String, GitVisibilityEdit>) -> Result<Self> {
        let delta = Self { edits };
        delta.validate()?;
        Ok(delta)
    }

    /// Encode this delta for an authenticated capsule section.
    pub fn encode(&self) -> Result<Bytes> {
        self.validate()?;
        paged::encode(self)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        paged::decode(bytes)
    }

    /// Return the ref-keyed reachability changes.
    #[must_use]
    pub fn edits(&self) -> &BTreeMap<String, GitVisibilityEdit> {
        &self.edits
    }

    fn validate(&self) -> Result<()> {
        if self.edits.is_empty()
            || self.edits.len() > crate::git_visibility::MAX_GIT_VISIBILITY_REFS
        {
            return Err(contract_error(
                "visibility delta ref count is outside its bound",
            ));
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
}
