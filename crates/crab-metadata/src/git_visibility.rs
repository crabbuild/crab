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

/// Current serialized ref-update evidence format.
pub const GIT_VISIBILITY_EDIT_VERSION: u32 = 1;

/// Immutable visibility delta published before one ref update becomes visible.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitVisibilityEdit {
    /// Schema version of this object.
    pub version: u32,
    /// Ref tip expected before the update, if the ref already exists.
    pub old_oid: Option<String>,
    /// Ref tip made visible by the update.
    pub new_oid: String,
    /// Whether `added` is the complete new closure rather than a delta.
    pub replaces: bool,
    /// Objects added to the prior closure, or the complete replacement closure.
    pub added: Vec<String>,
    /// Objects removed from the prior closure. Empty for replacement evidence.
    pub removed: Vec<String>,
}

impl GitVisibilityEdit {
    /// Build normalized evidence from complete old and new closures.
    #[must_use]
    pub fn delta(
        old_oid: Option<String>,
        new_oid: String,
        old: &BTreeSet<String>,
        new: &BTreeSet<String>,
    ) -> Self {
        Self {
            version: GIT_VISIBILITY_EDIT_VERSION,
            old_oid,
            new_oid,
            replaces: false,
            added: new.difference(old).cloned().collect(),
            removed: old.difference(new).cloned().collect(),
        }
    }

    /// Build normalized evidence that replaces an unavailable prior closure.
    #[must_use]
    pub fn replacement(old_oid: Option<String>, new_oid: String, new: &BTreeSet<String>) -> Self {
        Self {
            version: GIT_VISIBILITY_EDIT_VERSION,
            old_oid,
            new_oid,
            replaces: true,
            added: new.iter().cloned().collect(),
            removed: Vec::new(),
        }
    }

    /// Validate the evidence before applying it to authorization state.
    pub fn validate(&self) -> Result<()> {
        if self.version != GIT_VISIBILITY_EDIT_VERSION {
            return Err(corrupt("visibility edit version is unsupported"));
        }
        if let Some(old_oid) = &self.old_oid {
            validate_oid(old_oid)?;
        }
        validate_oid(&self.new_oid)?;
        if self.replaces && !self.removed.is_empty() {
            return Err(corrupt(
                "replacement visibility edit cannot contain removed objects",
            ));
        }
        let object_count = self
            .added
            .len()
            .checked_add(self.removed.len())
            .and_then(|count| u64::try_from(count).ok())
            .ok_or_else(|| corrupt("visibility edit object count overflows"))?;
        if object_count > MAX_GIT_VISIBILITY_OBJECTS {
            return Err(corrupt("visibility edit contains too many objects"));
        }
        validate_sorted_oids(&self.added, "added")?;
        validate_sorted_oids(&self.removed, "removed")?;
        if self
            .added
            .iter()
            .any(|oid| self.removed.binary_search(oid).is_ok())
        {
            return Err(corrupt("visibility edit adds and removes the same object"));
        }
        if self.replaces && !self.added.iter().any(|oid| oid == &self.new_oid) {
            return Err(corrupt("visibility edit does not add its new ref tip"));
        }
        Ok(())
    }

    /// Apply this evidence to an existing ref closure.
    pub fn apply(&self, prior: Option<&[String]>) -> Result<Vec<String>> {
        self.validate()?;
        if !self.replaces && self.old_oid.is_some() && prior.is_none() {
            return Err(corrupt("visibility delta has no prior ref closure"));
        }
        if !self.replaces
            && let Some(old_oid) = &self.old_oid
            && !prior.is_some_and(|objects| objects.binary_search(old_oid).is_ok())
        {
            return Err(corrupt(
                "visibility delta prior closure does not contain its old ref tip",
            ));
        }
        let mut objects = if self.replaces {
            BTreeSet::new()
        } else {
            prior
                .into_iter()
                .flatten()
                .cloned()
                .collect::<BTreeSet<_>>()
        };
        for oid in &self.removed {
            if !objects.remove(oid) {
                return Err(corrupt(
                    "visibility edit removes an object outside the prior closure",
                ));
            }
        }
        objects.extend(self.added.iter().cloned());
        if objects.len() as u64 > MAX_GIT_VISIBILITY_OBJECTS {
            return Err(corrupt("visibility edit result contains too many objects"));
        }
        if !objects.contains(&self.new_oid) {
            return Err(corrupt(
                "visibility edit result does not contain its new ref tip",
            ));
        }
        Ok(objects.into_iter().collect())
    }
}

fn validate_sorted_oids(objects: &[String], field: &str) -> Result<()> {
    let mut previous: Option<&str> = None;
    for object in objects {
        validate_oid(object)?;
        if previous.is_some_and(|value| value >= object.as_str()) {
            return Err(corrupt(format!(
                "visibility edit {field} objects must be sorted and deduplicated"
            )));
        }
        previous = Some(object.as_str());
    }
    Ok(())
}

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
    use std::collections::BTreeMap;

    use bytes::Bytes;
    use crab_storage::{StorageError, Store, StoreLayout};

    use super::{
        GitVisibilityEdit, GitVisibilityIndex, MAX_GIT_VISIBILITY_INDEX_BYTES, validate_hash,
    };
    use crate::error::Result;
    use crate::manifests::Manifest;
    use crate::ref_journal::RefJournalEdit;

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

    /// Upload content-addressed ref-update visibility evidence.
    pub async fn upload_edit(
        store: &Store,
        router: &StoreLayout<Store>,
        edit: &GitVisibilityEdit,
    ) -> Result<String> {
        edit.validate()?;
        let body = serde_json::to_vec(edit).map_err(|error| {
            crate::error::MetadataError::Internal(format!("visibility edit serialize: {error}"))
        })?;
        if body.len() as u64 > MAX_GIT_VISIBILITY_INDEX_BYTES {
            return Err(crate::error::MetadataError::CorruptObject {
                path: "git-visibility-edit".to_owned(),
                reason: format!(
                    "visibility edit exceeds {} bytes",
                    MAX_GIT_VISIBILITY_INDEX_BYTES
                ),
            });
        }
        let hash = blake3::hash(&body).to_hex().to_string();
        store
            .put(&router.git_visibility_edit_path(&hash), Bytes::from(body))
            .await?;
        Ok(hash)
    }

    /// Read and verify content-addressed ref-update visibility evidence.
    pub async fn read_edit(
        store: &Store,
        router: &StoreLayout<Store>,
        evidence_hash: &str,
    ) -> Result<GitVisibilityEdit> {
        validate_hash(evidence_hash, "visibility edit hash")?;
        let path = router.git_visibility_edit_path(evidence_hash);
        let metadata = store.head(&path).await?;
        if metadata.size > MAX_GIT_VISIBILITY_INDEX_BYTES {
            return Err(crate::error::MetadataError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!(
                    "visibility edit is {} bytes; maximum is {}",
                    metadata.size, MAX_GIT_VISIBILITY_INDEX_BYTES
                ),
            });
        }
        let (body, _) = store.get_with_etag(&path).await?;
        if blake3::hash(&body).to_hex().as_str() != evidence_hash {
            return Err(crate::error::MetadataError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: "visibility edit body does not match its identity".to_owned(),
            });
        }
        let edit: GitVisibilityEdit = serde_json::from_slice(&body).map_err(|error| {
            crate::error::MetadataError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!("invalid visibility edit JSON: {error}"),
            }
        })?;
        edit.validate()?;
        Ok(edit)
    }

    /// Build a generation proof by applying journal-owned immutable evidence.
    ///
    /// Returns `None` when the base proof or any transaction evidence is absent,
    /// so callers can retain the evidence-less repair path.
    pub async fn compact_journal_edits(
        store: &Store,
        router: &StoreLayout<Store>,
        base: &Manifest,
        edits: &[RefJournalEdit],
        generation: u64,
        pack_index_hash: &str,
        final_refs: &BTreeMap<String, String>,
    ) -> Result<Option<GitVisibilityIndex>> {
        let mut refs = if base.refs.is_empty() {
            BTreeMap::new()
        } else {
            match read(store, router, base.generation, &base.pack_index_hash).await {
                Ok(index) => {
                    if index.refs.keys().ne(base.refs.keys())
                        || base.refs.iter().any(|(name, tip)| {
                            !index
                                .refs
                                .get(name)
                                .is_some_and(|objects| objects.binary_search(tip).is_ok())
                        })
                    {
                        return Err(crate::error::MetadataError::CorruptObject {
                            path: router
                                .git_visibility_path(base.generation, &base.pack_index_hash)
                                .as_ref()
                                .to_owned(),
                            reason: "base visibility proof does not match its manifest refs"
                                .to_owned(),
                        });
                    }
                    index.refs
                }
                Err(crate::error::MetadataError::Storage {
                    source: StorageError::NotFound { .. },
                }) => return Ok(None),
                Err(error) => return Err(error),
            }
        };

        for edit in edits {
            match &edit.new_oid {
                None => {
                    refs.remove(&edit.ref_name);
                }
                Some(new_oid) => {
                    let Some(evidence_hash) = edit.visibility_evidence_hash.as_deref() else {
                        return Ok(None);
                    };
                    let evidence = read_edit(store, router, evidence_hash).await?;
                    if evidence.old_oid != edit.old_oid || evidence.new_oid != *new_oid {
                        return Err(crate::error::MetadataError::CorruptObject {
                            path: router
                                .git_visibility_edit_path(evidence_hash)
                                .as_ref()
                                .to_owned(),
                            reason: "visibility edit does not match its ref journal edit"
                                .to_owned(),
                        });
                    }
                    let closure = evidence.apply(refs.get(&edit.ref_name).map(Vec::as_slice))?;
                    refs.insert(edit.ref_name.clone(), closure);
                }
            }
        }

        if refs.keys().ne(final_refs.keys()) {
            return Err(crate::error::MetadataError::CorruptObject {
                path: "git-visibility-index".to_owned(),
                reason: "compacted visibility refs do not match the manifest".to_owned(),
            });
        }
        for (name, tip) in final_refs {
            if !refs
                .get(name)
                .is_some_and(|objects| objects.binary_search(tip).is_ok())
            {
                return Err(crate::error::MetadataError::CorruptObject {
                    path: "git-visibility-index".to_owned(),
                    reason: format!("compacted visibility proof does not contain ref tip {name}"),
                });
            }
        }
        Ok(Some(GitVisibilityIndex::new(
            generation,
            pack_index_hash,
            refs,
        )))
    }
}

#[cfg(feature = "storage")]
pub use storage::{compact_journal_edits, read, read_edit, upload_edit, upload_if_absent};

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

    #[test]
    fn ref_edit_delta_reconstructs_exact_new_closure() {
        let old = BTreeSet::from(["a".repeat(40), "b".repeat(40)]);
        let new = BTreeSet::from(["b".repeat(40), "c".repeat(40)]);
        let edit = GitVisibilityEdit::delta(Some("a".repeat(40)), "c".repeat(40), &old, &new);

        assert_eq!(
            edit.apply(Some(&old.into_iter().collect::<Vec<_>>()))
                .unwrap(),
            new.into_iter().collect::<Vec<_>>()
        );
    }

    #[cfg(feature = "storage")]
    #[tokio::test]
    async fn journal_compaction_combines_independent_writer_evidence() {
        use std::sync::Arc;

        use crab_storage::{Store, StoreLayout};
        use object_store::memory::InMemory;

        use crate::manifests::Manifest;
        use crate::ref_journal::RefJournalEdit;

        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        let pack_hash = "f".repeat(64);
        let mut base = Manifest::default_for_repo("refs/heads/left");
        base.generation = 4;
        base.pack_index_hash.clone_from(&pack_hash);
        base.refs
            .insert("refs/heads/left".to_owned(), "a".repeat(40));
        base.refs
            .insert("refs/heads/right".to_owned(), "b".repeat(40));
        base.seal_git_validation();
        let base_index = GitVisibilityIndex::new(
            4,
            &pack_hash,
            BTreeMap::from([
                (
                    "refs/heads/left".to_owned(),
                    vec!["1".repeat(40), "a".repeat(40)],
                ),
                (
                    "refs/heads/right".to_owned(),
                    vec!["2".repeat(40), "b".repeat(40)],
                ),
            ]),
        );
        upload_if_absent(&store, &router, &base_index)
            .await
            .unwrap();

        let left_evidence = GitVisibilityEdit::delta(
            Some("a".repeat(40)),
            "c".repeat(40),
            &BTreeSet::from(["1".repeat(40), "a".repeat(40)]),
            &BTreeSet::from(["1".repeat(40), "3".repeat(40), "c".repeat(40)]),
        );
        let right_evidence = GitVisibilityEdit::delta(
            Some("b".repeat(40)),
            "d".repeat(40),
            &BTreeSet::from(["2".repeat(40), "b".repeat(40)]),
            &BTreeSet::from(["2".repeat(40), "4".repeat(40), "d".repeat(40)]),
        );
        let left_hash = upload_edit(&store, &router, &left_evidence).await.unwrap();
        let right_hash = upload_edit(&store, &router, &right_evidence).await.unwrap();
        let edits = vec![
            RefJournalEdit {
                ref_name: "refs/heads/left".to_owned(),
                old_oid: Some("a".repeat(40)),
                new_oid: Some("c".repeat(40)),
                peeled_oid: None,
                lock_holder: None,
                visibility_evidence_hash: Some(left_hash),
            },
            RefJournalEdit {
                ref_name: "refs/heads/right".to_owned(),
                old_oid: Some("b".repeat(40)),
                new_oid: Some("d".repeat(40)),
                peeled_oid: None,
                lock_holder: None,
                visibility_evidence_hash: Some(right_hash),
            },
        ];
        let final_refs = BTreeMap::from([
            ("refs/heads/left".to_owned(), "c".repeat(40)),
            ("refs/heads/right".to_owned(), "d".repeat(40)),
        ]);

        let compacted = compact_journal_edits(
            &store,
            &router,
            &base,
            &edits,
            5,
            &"e".repeat(64),
            &final_refs,
        )
        .await
        .unwrap()
        .expect("every writer published visibility evidence");

        assert_eq!(
            compacted.refs["refs/heads/left"],
            vec!["1".repeat(40), "3".repeat(40), "c".repeat(40)]
        );
        assert_eq!(
            compacted.refs["refs/heads/right"],
            vec!["2".repeat(40), "4".repeat(40), "d".repeat(40)]
        );
    }
}
