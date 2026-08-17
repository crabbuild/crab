//! Immutable Git object visibility proofs for upload-pack admission.
//!
//! The manifest names refs and the pack index names storage. Neither one is
//! sufficient to authorize an arbitrary object ID: upload-pack also needs the
//! complete object closure of the refs visible to the caller. This module
//! stores that closure in one generation-bound, immutable object.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

use serde::{Deserialize, Serialize};

use crate::error::{MetadataError, Result};

/// Current serialized visibility-index format.
pub const GIT_VISIBILITY_INDEX_VERSION: u32 = 1;

/// Maximum serialized proof accepted from object storage.
pub const MAX_GIT_VISIBILITY_INDEX_BYTES: u64 = 128 * 1024 * 1024;

const MAX_GIT_VISIBILITY_REFS: usize = 100_000;
/// Maximum number of ref-rooted object entries accepted in one proof.
pub const MAX_GIT_VISIBILITY_OBJECTS: u64 = 10_000_000;

/// Complete ref-rooted Git object visibility proof for one repository snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitVisibilityIndex {
    /// Schema version of this object.
    pub version: u32,
    /// Manifest generation this proof describes.
    pub generation: u64,
    /// Pack-index hash this proof was built against.
    pub pack_index_hash: String,
    /// Complete object closure for each manifest ref, including the ref tip.
    pub refs: BTreeMap<String, Vec<String>>,
}

impl GitVisibilityIndex {
    /// Build a normalized proof from ref-rooted object sets.
    #[must_use]
    pub fn new(
        generation: u64,
        pack_index_hash: impl Into<String>,
        refs: BTreeMap<String, Vec<String>>,
    ) -> Self {
        let refs = refs
            .into_iter()
            .map(|(name, objects)| {
                let mut objects = objects;
                objects.sort_unstable();
                objects.dedup();
                (name, objects)
            })
            .collect();
        Self {
            version: GIT_VISIBILITY_INDEX_VERSION,
            generation,
            pack_index_hash: pack_index_hash.into(),
            refs,
        }
    }

    /// Validate the object before it is used for authorization.
    pub fn validate(&self) -> Result<()> {
        if self.version != GIT_VISIBILITY_INDEX_VERSION {
            return Err(corrupt("visibility index version is unsupported"));
        }
        if self.refs.len() > MAX_GIT_VISIBILITY_REFS {
            return Err(corrupt("visibility index contains too many refs"));
        }
        if !(self.pack_index_hash.is_empty() && self.refs.is_empty()) {
            validate_hash(&self.pack_index_hash, "pack index hash")?;
        }
        let mut object_count = 0u64;
        for (name, objects) in &self.refs {
            if name.is_empty() || name.bytes().any(|byte| byte.is_ascii_control()) {
                return Err(corrupt("visibility index contains an invalid ref name"));
            }
            object_count =
                object_count
                    .checked_add(u64::try_from(objects.len()).map_err(|_| {
                        corrupt("visibility index object count cannot be represented")
                    })?)
                    .ok_or_else(|| corrupt("visibility index object count overflows"))?;
            if object_count > MAX_GIT_VISIBILITY_OBJECTS {
                return Err(corrupt("visibility index contains too many objects"));
            }
            let mut previous: Option<&str> = None;
            for object in objects {
                validate_oid(object)?;
                if previous.is_some_and(|value| value >= object.as_str()) {
                    return Err(corrupt(
                        "visibility index object lists must be sorted and deduplicated",
                    ));
                }
                previous = Some(object.as_str());
            }
        }
        Ok(())
    }

    /// Return the union of objects rooted at the supplied visible refs.
    #[must_use]
    pub fn objects_for_refs<'a, I>(&self, refs: I) -> BTreeSet<String>
    where
        I: IntoIterator<Item = &'a str>,
    {
        refs.into_iter()
            .filter_map(|name| self.refs.get(name))
            .flat_map(|objects| objects.iter().cloned())
            .collect()
    }

    /// Count the distinct objects rooted at the supplied visible refs.
    #[must_use]
    pub fn object_count_for_refs<'a, I>(&self, refs: I) -> usize
    where
        I: IntoIterator<Item = &'a str>,
    {
        let lists = refs
            .into_iter()
            .filter_map(|name| self.refs.get(name).map(Vec::as_slice))
            .collect::<Vec<_>>();
        let mut pending = BinaryHeap::new();
        for (list_index, objects) in lists.iter().enumerate() {
            if let Some(value) = objects.first() {
                pending.push(Reverse((value.as_str(), list_index, 0usize)));
            }
        }

        let mut count = 0usize;
        let mut previous: Option<&str> = None;
        while let Some(Reverse((value, list_index, object_index))) = pending.pop() {
            if previous != Some(value) {
                count = count.saturating_add(1);
                previous = Some(value);
            }
            let next_index = object_index.saturating_add(1);
            if let Some(next) = lists[list_index].get(next_index) {
                pending.push(Reverse((next.as_str(), list_index, next_index)));
            }
        }
        count
    }

    /// Return whether an object is proven reachable from one of the supplied refs.
    #[must_use]
    pub fn contains_for_refs<'a, I>(&self, refs: I, oid: &str) -> bool
    where
        I: IntoIterator<Item = &'a str>,
    {
        refs.into_iter().any(|name| {
            self.refs.get(name).is_some_and(|objects| {
                objects
                    .binary_search_by(|value| value.as_str().cmp(oid))
                    .is_ok()
            })
        })
    }
}

fn validate_oid(value: &str) -> Result<()> {
    if value.len() != 40
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(corrupt("visibility index contains an invalid object ID"));
    }
    Ok(())
}

fn validate_hash(value: &str, field: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(corrupt(format!("{field} is not lowercase hexadecimal")));
    }
    Ok(())
}

fn corrupt(reason: impl Into<String>) -> MetadataError {
    MetadataError::CorruptObject {
        path: "git-visibility-index".to_owned(),
        reason: reason.into(),
    }
}

#[cfg(feature = "storage")]
mod storage {
    use bytes::Bytes;
    use crab_storage::{StorageError, Store, StoreLayout};

    use super::{GitVisibilityIndex, MAX_GIT_VISIBILITY_INDEX_BYTES};
    use crate::error::Result;

    /// Read and validate a generation-bound visibility proof.
    pub async fn read(
        store: &Store,
        router: &StoreLayout<Store>,
        generation: u64,
        pack_index_hash: &str,
    ) -> Result<GitVisibilityIndex> {
        let path = router.git_visibility_path(generation, pack_index_hash);
        let metadata = store.head(&path).await?;
        if metadata.size > MAX_GIT_VISIBILITY_INDEX_BYTES {
            return Err(crate::error::MetadataError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!(
                    "visibility index is {} bytes; maximum is {}",
                    metadata.size, MAX_GIT_VISIBILITY_INDEX_BYTES
                ),
            });
        }
        let (body, _) = store.get_with_etag(&path).await?;
        if body.len() as u64 > MAX_GIT_VISIBILITY_INDEX_BYTES {
            return Err(crate::error::MetadataError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!(
                    "visibility index body exceeds {} bytes",
                    MAX_GIT_VISIBILITY_INDEX_BYTES
                ),
            });
        }
        let index: GitVisibilityIndex = serde_json::from_slice(&body).map_err(|error| {
            crate::error::MetadataError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!("invalid visibility index JSON: {error}"),
            }
        })?;
        if index.generation != generation || index.pack_index_hash != pack_index_hash {
            return Err(crate::error::MetadataError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: "visibility index does not match its immutable path".to_owned(),
            });
        }
        index.validate()?;
        Ok(index)
    }

    /// Upload a visibility proof once and verify an existing immutable value.
    pub async fn upload_if_absent(
        store: &Store,
        router: &StoreLayout<Store>,
        index: &GitVisibilityIndex,
    ) -> Result<()> {
        index.validate()?;
        let body = serde_json::to_vec(index).map_err(|error| {
            crate::error::MetadataError::Internal(format!("visibility index serialize: {error}"))
        })?;
        if body.len() as u64 > MAX_GIT_VISIBILITY_INDEX_BYTES {
            return Err(crate::error::MetadataError::CorruptObject {
                path: router
                    .git_visibility_path(index.generation, &index.pack_index_hash)
                    .as_ref()
                    .to_owned(),
                reason: format!(
                    "visibility index exceeds {} bytes",
                    MAX_GIT_VISIBILITY_INDEX_BYTES
                ),
            });
        }
        let path = router.git_visibility_path(index.generation, &index.pack_index_hash);
        match store.put(&path, Bytes::from(body)).await {
            Ok(()) => Ok(()),
            Err(StorageError::StateConflict { .. }) => {
                let metadata = store.head(&path).await?;
                if metadata.size > MAX_GIT_VISIBILITY_INDEX_BYTES {
                    return Err(crate::error::MetadataError::CorruptObject {
                        path: path.as_ref().to_owned(),
                        reason: format!(
                            "existing visibility index is {} bytes; maximum is {}",
                            metadata.size, MAX_GIT_VISIBILITY_INDEX_BYTES
                        ),
                    });
                }
                let (existing, _) = store.get_with_etag(&path).await?;
                if existing.len() as u64 > MAX_GIT_VISIBILITY_INDEX_BYTES {
                    return Err(crate::error::MetadataError::CorruptObject {
                        path: path.as_ref().to_owned(),
                        reason: format!(
                            "existing visibility index exceeds {} bytes",
                            MAX_GIT_VISIBILITY_INDEX_BYTES
                        ),
                    });
                }
                let existing: GitVisibilityIndex =
                    serde_json::from_slice(&existing).map_err(|error| {
                        crate::error::MetadataError::CorruptObject {
                            path: path.as_ref().to_owned(),
                            reason: format!("invalid existing visibility index JSON: {error}"),
                        }
                    })?;
                existing.validate()?;
                if existing == *index {
                    Ok(())
                } else {
                    Err(crate::error::MetadataError::CorruptObject {
                        path: path.as_ref().to_owned(),
                        reason: "immutable visibility index conflicts with the requested proof"
                            .to_owned(),
                    })
                }
            }
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(feature = "storage")]
pub use storage::{read, upload_if_absent};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_and_authorizes_ref_closure() {
        let index = GitVisibilityIndex::new(
            4,
            "a".repeat(64),
            BTreeMap::from([(
                "refs/heads/main".to_owned(),
                vec!["b".repeat(40), "a".repeat(40), "b".repeat(40)],
            )]),
        );

        assert!(index.validate().is_ok());
        assert!(index.contains_for_refs(["refs/heads/main"], &"a".repeat(40)));
        assert_eq!(index.objects_for_refs(["refs/heads/main"]).len(), 2);
        assert_eq!(
            index.object_count_for_refs(["refs/heads/main", "refs/heads/main"]),
            2
        );
    }

    #[test]
    fn rejects_stale_or_malformed_proof() {
        let mut index = GitVisibilityIndex::new(4, "a".repeat(64), BTreeMap::new());
        index
            .refs
            .insert("refs/heads/main".to_owned(), vec!["A".repeat(40)]);
        assert!(index.validate().is_err());
    }

    #[test]
    fn rejects_proofs_with_too_many_refs_before_authorization() {
        let refs = (0..=MAX_GIT_VISIBILITY_REFS)
            .map(|index| (format!("refs/heads/{index}"), Vec::new()))
            .collect();
        let index = GitVisibilityIndex::new(4, "a".repeat(64), refs);
        assert!(index.validate().is_err());
    }
}
