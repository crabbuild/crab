use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::error::{MetadataError, Result};
use crate::validation::{corrupt_object, validate_content_hash};

/// Maximum encoded browse-index record size.
pub const MAX_BROWSE_INDEXES_BYTES: u64 = 4096;

/// Derived Git indexes for one exact capsule state, never ref authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowseIndexes {
    version: u32,
    state_digest: String,
    commit_graph_hash: String,
    path_state_hash: String,
}

impl BrowseIndexes {
    /// Bind complete durable indexes to their captured root and visible ref positions.
    pub fn new(
        state_digest: String,
        commit_graph_hash: String,
        path_state_hash: String,
    ) -> Result<Self> {
        let value = Self {
            version: 1,
            state_digest,
            commit_graph_hash,
            path_state_hash,
        };
        value.validate()?;
        Ok(value)
    }

    /// Return the captured capsule-state digest.
    #[must_use]
    pub fn state_digest(&self) -> &str {
        &self.state_digest
    }

    /// Return the immutable commit-graph descriptor hash.
    #[must_use]
    pub fn commit_graph_hash(&self) -> &str {
        &self.commit_graph_hash
    }

    /// Return the immutable complete path-state descriptor hash.
    #[must_use]
    pub fn path_state_hash(&self) -> &str {
        &self.path_state_hash
    }

    /// Encode a bounded, validated record after both indexes are durable.
    pub fn encode(&self) -> Result<Bytes> {
        self.validate()?;
        serde_json::to_vec(self)
            .map(Bytes::from)
            .map_err(|source| MetadataError::BrowseIndexRecord { source })
    }

    /// Decode only the supported bounded record with valid content identities.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() as u64 > MAX_BROWSE_INDEXES_BYTES {
            return Err(corrupt_object(
                "browse indexes",
                "record exceeds byte limit",
            ));
        }
        let value: Self = serde_json::from_slice(bytes)
            .map_err(|source| MetadataError::BrowseIndexRecord { source })?;
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1 {
            return Err(corrupt_object(
                "browse indexes",
                "unsupported record version",
            ));
        }
        for (field, hash) in [
            ("state digest", &self.state_digest),
            ("commit graph", &self.commit_graph_hash),
            ("path state", &self.path_state_hash),
        ] {
            validate_content_hash(hash, field, "browse indexes")?;
        }
        Ok(())
    }
}

/// Read optional derived metadata without loading any index bodies or ref authority.
#[cfg(feature = "storage")]
pub async fn load_browse_indexes(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
) -> Result<Option<BrowseIndexes>> {
    match layout
        .store()
        .get_with_etag_bounded(
            &layout.capsule_browse_indexes_path(),
            MAX_BROWSE_INDEXES_BYTES,
        )
        .await
    {
        Ok((bytes, _)) => BrowseIndexes::decode(&bytes).map(Some),
        Err(crab_storage::StorageError::NotFound { .. }) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browse_indexes_round_trip_exact_state_and_complete_descriptors() {
        let record = BrowseIndexes::new("1".repeat(64), "2".repeat(64), "3".repeat(64)).unwrap();
        assert_eq!(
            BrowseIndexes::decode(&record.encode().unwrap()).unwrap(),
            record
        );
    }

    #[test]
    fn malformed_or_partial_browse_indexes_are_rejected() {
        let valid = serde_json::to_value(
            BrowseIndexes::new("1".repeat(64), "2".repeat(64), "3".repeat(64)).unwrap(),
        )
        .unwrap();
        for field in ["state_digest", "commit_graph_hash", "path_state_hash"] {
            let mut missing = valid.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(BrowseIndexes::decode(&serde_json::to_vec(&missing).unwrap()).is_err());
            let mut malformed = valid.clone();
            malformed[field] = serde_json::json!("../invalid");
            assert!(BrowseIndexes::decode(&serde_json::to_vec(&malformed).unwrap()).is_err());
        }
        let mut unsupported = valid;
        unsupported["version"] = serde_json::json!(2);
        assert!(BrowseIndexes::decode(&serde_json::to_vec(&unsupported).unwrap()).is_err());
        assert!(BrowseIndexes::decode(&vec![b' '; MAX_BROWSE_INDEXES_BYTES as usize + 1]).is_err());
    }
}
