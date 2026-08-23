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
use crate::manifests::Manifest;

/// Current serialized visibility-index format.
pub const GIT_VISIBILITY_INDEX_VERSION: u32 = 3;

/// Maximum serialized proof accepted from object storage.
pub const MAX_GIT_VISIBILITY_INDEX_BYTES: u64 = 128 * 1024 * 1024;

const MAX_GIT_VISIBILITY_REFS: usize = 100_000;
/// Maximum number of ref-rooted object entries accepted in one proof.
pub const MAX_GIT_VISIBILITY_OBJECTS: u64 = 10_000_000;

/// Maximum ref-rooted object entries built synchronously on a push or repair path.
pub const MAX_SYNCHRONOUS_GIT_VISIBILITY_OBJECTS: u64 = 100_000;

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitVisibilityIndex {
    /// Schema version of this object.
    pub version: u32,
    /// Manifest generation this proof describes.
    pub generation: u64,
    /// Pack-index hash this proof was built against.
    pub pack_index_hash: String,
    /// Digest binding the complete manifest Git state described by this proof.
    pub git_validation_digest: String,
    /// Complete object closure for each manifest ref, including the ref tip.
    pub refs: BTreeMap<String, Vec<String>>,
}

impl GitVisibilityIndex {
    /// Build a normalized proof from ref-rooted object sets.
    #[must_use]
    pub fn new(
        generation: u64,
        pack_index_hash: impl Into<String>,
        git_validation_digest: impl Into<String>,
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
            git_validation_digest: git_validation_digest.into(),
            refs,
        }
    }

    /// Validate the object before it is used for authorization.
    pub fn validate(&self) -> Result<()> {
        if self.version != GIT_VISIBILITY_INDEX_VERSION {
            return Err(corrupt("visibility index version is unsupported"));
        }
        if !(self.pack_index_hash.is_empty() && self.refs.is_empty()) {
            validate_hash(&self.pack_index_hash, "pack index hash")?;
        }
        validate_hash(&self.git_validation_digest, "Git validation digest")?;
        validate_ref_closures(&self.refs)
    }

    #[cfg(feature = "storage")]
    fn validate_identity(
        &self,
        generation: u64,
        pack_index_hash: &str,
        git_validation_digest: &str,
    ) -> Result<()> {
        if self.generation != generation
            || self.pack_index_hash != pack_index_hash
            || self.git_validation_digest != git_validation_digest
        {
            return Err(corrupt(
                "visibility index does not match its immutable identity",
            ));
        }
        Ok(())
    }

    /// Return whether this proof covers the complete Git state of a manifest.
    #[must_use]
    pub fn matches_manifest(&self, manifest: &Manifest) -> bool {
        self.generation == manifest.generation
            && self.pack_index_hash == manifest.pack_index_hash
            && self.git_validation_digest == manifest.git_validation_digest
            && self.refs.keys().eq(manifest.refs.keys())
            && manifest.refs.iter().all(|(name, tip)| {
                self.refs
                    .get(name)
                    .is_some_and(|objects| objects.binary_search(tip).is_ok())
            })
            && manifest.peeled_refs.iter().all(|(name, peeled)| {
                self.refs
                    .get(name)
                    .is_some_and(|objects| objects.binary_search(peeled).is_ok())
            })
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

fn validate_ref_closures(refs: &BTreeMap<String, Vec<String>>) -> Result<()> {
    if refs.len() > MAX_GIT_VISIBILITY_REFS {
        return Err(corrupt("visibility index contains too many refs"));
    }
    let mut object_count = 0u64;
    for (name, objects) in refs {
        if name.is_empty() || name.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(corrupt("visibility index contains an invalid ref name"));
        }
        object_count = object_count
            .checked_add(
                u64::try_from(objects.len())
                    .map_err(|_| corrupt("visibility index object count cannot be represented"))?,
            )
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
    use std::collections::{BTreeMap, BTreeSet};

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD_NO_PAD;
    use bytes::Bytes;
    use crab_storage::{StorageError, Store, StoreLayout};
    use object_store::path::Path as ObjectPath;
    use serde::{Deserialize, Serialize};

    use super::{
        GIT_VISIBILITY_INDEX_VERSION, GitVisibilityEdit, GitVisibilityIndex,
        MAX_GIT_VISIBILITY_INDEX_BYTES, MAX_GIT_VISIBILITY_OBJECTS, MAX_GIT_VISIBILITY_REFS,
        validate_hash, validate_oid, validate_ref_closures,
    };
    use crate::error::Result;
    use crate::manifests::Manifest;
    use crate::ref_journal::RefJournalEdit;

    const LEGACY_GIT_VISIBILITY_INDEX_VERSION: u32 = 1;

    /// Stored format used to satisfy a visibility read.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum GitVisibilityFormat {
        /// Dictionary-compressed, digest-bound proof written by current Crab versions.
        V3,
        /// Generation-and-pack-bound proof shipped by Crab 1.0.15.
        V1,
    }

    /// Validated visibility proof and its stored format.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct GitVisibilityRead {
        /// Normalized current-format proof.
        pub index: GitVisibilityIndex,
        /// Stored format that supplied the proof.
        pub format: GitVisibilityFormat,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct GitVisibilityIndexV1 {
        version: u32,
        generation: u64,
        pack_index_hash: String,
        refs: BTreeMap<String, Vec<String>>,
    }

    // Runtime authorization uses per-ref OID slices. Persistence keeps that API
    // out of the storage contract by deduplicating shared OIDs here.
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct GitVisibilityIndexV3 {
        version: u32,
        generation: u64,
        pack_index_hash: String,
        git_validation_digest: String,
        objects: Vec<String>,
        refs: BTreeMap<String, GitVisibilityClosureV3>,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum GitVisibilityClosureV3 {
        Sparse(Vec<u32>),
        Bitmap(String),
    }

    impl GitVisibilityIndexV3 {
        fn from_index(index: &GitVisibilityIndex) -> Result<Self> {
            index.validate()?;
            let objects = index
                .refs
                .values()
                .flatten()
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let positions = objects
                .iter()
                .enumerate()
                .map(|(position, oid)| {
                    u32::try_from(position)
                        .map(|position| (oid.as_str(), position))
                        .map_err(|_| super::corrupt("visibility object dictionary is too large"))
                })
                .collect::<Result<BTreeMap<_, _>>>()?;
            let refs = index
                .refs
                .iter()
                .map(|(name, closure)| {
                    let positions = closure
                        .iter()
                        .map(|oid| {
                            positions.get(oid.as_str()).copied().ok_or_else(|| {
                                super::corrupt(
                                    "visibility closure object is absent from its dictionary",
                                )
                            })
                        })
                        .collect::<Result<Vec<_>>>()?;
                    Ok((
                        name.clone(),
                        GitVisibilityClosureV3::from_positions(positions, objects.len())?,
                    ))
                })
                .collect::<Result<BTreeMap<_, _>>>()?;
            Ok(Self {
                version: GIT_VISIBILITY_INDEX_VERSION,
                generation: index.generation,
                pack_index_hash: index.pack_index_hash.clone(),
                git_validation_digest: index.git_validation_digest.clone(),
                objects,
                refs,
            })
        }

        fn into_index(self) -> Result<GitVisibilityIndex> {
            if self.version != GIT_VISIBILITY_INDEX_VERSION {
                return Err(super::corrupt(
                    "visibility index storage version is unsupported",
                ));
            }
            if !(self.pack_index_hash.is_empty() && self.refs.is_empty()) {
                validate_hash(&self.pack_index_hash, "pack index hash")?;
            }
            validate_hash(&self.git_validation_digest, "Git validation digest")?;
            if self.objects.len() as u64 > MAX_GIT_VISIBILITY_OBJECTS {
                return Err(super::corrupt(
                    "visibility object dictionary contains too many objects",
                ));
            }
            let mut previous: Option<&str> = None;
            for oid in &self.objects {
                validate_oid(oid)?;
                if previous.is_some_and(|value| value >= oid.as_str()) {
                    return Err(super::corrupt(
                        "visibility object dictionary must be sorted and deduplicated",
                    ));
                }
                previous = Some(oid.as_str());
            }
            if self.refs.len() > MAX_GIT_VISIBILITY_REFS {
                return Err(super::corrupt("visibility index contains too many refs"));
            }
            let mut membership_count = 0u64;
            let mut covered = vec![false; self.objects.len()];
            let refs = self
                .refs
                .into_iter()
                .map(|(name, closure)| {
                    if name.is_empty() || name.bytes().any(|byte| byte.is_ascii_control()) {
                        return Err(super::corrupt(
                            "visibility index contains an invalid ref name",
                        ));
                    }
                    let positions = closure.into_positions(self.objects.len())?;
                    membership_count = membership_count
                        .checked_add(u64::try_from(positions.len()).map_err(|_| {
                            super::corrupt("visibility index object count cannot be represented")
                        })?)
                        .ok_or_else(|| super::corrupt("visibility index object count overflows"))?;
                    if membership_count > MAX_GIT_VISIBILITY_OBJECTS {
                        return Err(super::corrupt("visibility index contains too many objects"));
                    }
                    let mut prior_position = None;
                    let objects = positions
                        .into_iter()
                        .map(|position| {
                            if prior_position.is_some_and(|prior| prior >= position) {
                                return Err(super::corrupt(
                                    "visibility closure positions must be sorted and deduplicated",
                                ));
                            }
                            prior_position = Some(position);
                            let position = usize::try_from(position).map_err(|_| {
                                super::corrupt("visibility closure position cannot be represented")
                            })?;
                            let oid = self.objects.get(position).ok_or_else(|| {
                                super::corrupt(
                                    "visibility closure position is outside its dictionary",
                                )
                            })?;
                            covered[position] = true;
                            Ok(oid.clone())
                        })
                        .collect::<Result<Vec<_>>>()?;
                    Ok((name, objects))
                })
                .collect::<Result<BTreeMap<_, _>>>()?;
            if covered.iter().any(|covered| !covered) {
                return Err(super::corrupt(
                    "visibility object dictionary contains an unreferenced object",
                ));
            }
            let index = GitVisibilityIndex {
                version: self.version,
                generation: self.generation,
                pack_index_hash: self.pack_index_hash,
                git_validation_digest: self.git_validation_digest,
                refs,
            };
            index.validate()?;
            Ok(index)
        }
    }

    impl GitVisibilityClosureV3 {
        fn from_positions(positions: Vec<u32>, object_count: usize) -> Result<Self> {
            let bitmap_len = object_count.div_ceil(8);
            let bitmap_encoded_len = bitmap_len
                .checked_div(3)
                .and_then(|groups| groups.checked_mul(4))
                .and_then(|size| {
                    size.checked_add(match bitmap_len % 3 {
                        0 => 0,
                        1 => 2,
                        _ => 3,
                    })
                })
                .ok_or_else(|| super::corrupt("visibility closure bitmap size overflows"))?;
            let sparse_encoded_len = positions
                .iter()
                .try_fold(positions.len().saturating_sub(1), |size, position| {
                    size.checked_add(decimal_digits(*position))
                });
            let sparse_encoded_len = sparse_encoded_len
                .ok_or_else(|| super::corrupt("visibility sparse closure size overflows"))?;
            if bitmap_encoded_len >= sparse_encoded_len {
                return Ok(Self::Sparse(positions));
            }

            let mut bitmap = vec![0_u8; bitmap_len];
            for position in positions {
                let position = usize::try_from(position).map_err(|_| {
                    super::corrupt("visibility closure position cannot be represented")
                })?;
                let byte = bitmap.get_mut(position / 8).ok_or_else(|| {
                    super::corrupt("visibility closure position is outside its dictionary")
                })?;
                *byte |= 1 << (position % 8);
            }
            Ok(Self::Bitmap(STANDARD_NO_PAD.encode(bitmap)))
        }

        fn into_positions(self, object_count: usize) -> Result<Vec<u32>> {
            match self {
                Self::Sparse(positions) => Ok(positions),
                Self::Bitmap(encoded) => {
                    let bitmap = STANDARD_NO_PAD.decode(&encoded).map_err(|_| {
                        super::corrupt("visibility closure bitmap is not valid base64")
                    })?;
                    if STANDARD_NO_PAD.encode(&bitmap) != encoded {
                        return Err(super::corrupt(
                            "visibility closure bitmap is not canonical base64",
                        ));
                    }
                    let expected_len = object_count.div_ceil(8);
                    if bitmap.len() != expected_len {
                        return Err(super::corrupt(
                            "visibility closure bitmap length does not match its dictionary",
                        ));
                    }
                    if let Some(last) = bitmap.last()
                        && !object_count.is_multiple_of(8)
                        && last >> (object_count % 8) != 0
                    {
                        return Err(super::corrupt(
                            "visibility closure bitmap sets a position outside its dictionary",
                        ));
                    }
                    let position_count = bitmap.iter().try_fold(0_u64, |count, byte| {
                        count.checked_add(u64::from(byte.count_ones()))
                    });
                    let position_count = position_count
                        .and_then(|count| usize::try_from(count).ok())
                        .ok_or_else(|| {
                            super::corrupt("visibility closure object count cannot be represented")
                        })?;
                    let mut positions = Vec::with_capacity(position_count);
                    for (byte_index, byte) in bitmap.iter().enumerate() {
                        for bit_index in 0..8 {
                            if byte & (1 << bit_index) == 0 {
                                continue;
                            }
                            let position = byte_index
                                .checked_mul(8)
                                .and_then(|position| position.checked_add(bit_index))
                                .and_then(|position| u32::try_from(position).ok())
                                .ok_or_else(|| {
                                    super::corrupt(
                                        "visibility closure position cannot be represented",
                                    )
                                })?;
                            positions.push(position);
                        }
                    }
                    Ok(positions)
                }
            }
        }
    }

    fn decimal_digits(mut value: u32) -> usize {
        let mut digits = 1;
        while value >= 10 {
            value /= 10;
            digits += 1;
        }
        digits
    }

    impl GitVisibilityIndexV1 {
        fn validate(&self) -> Result<()> {
            if self.version != LEGACY_GIT_VISIBILITY_INDEX_VERSION {
                return Err(super::corrupt(
                    "legacy visibility index version is unsupported",
                ));
            }
            if !(self.pack_index_hash.is_empty() && self.refs.is_empty()) {
                validate_hash(&self.pack_index_hash, "pack index hash")?;
            }
            validate_ref_closures(&self.refs)
        }
    }

    async fn read_bounded(store: &Store, path: &ObjectPath) -> Result<Bytes> {
        let metadata = store.head(path).await?;
        if metadata.size > MAX_GIT_VISIBILITY_INDEX_BYTES {
            return Err(crate::error::MetadataError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!(
                    "visibility index is {} bytes; maximum is {}",
                    metadata.size, MAX_GIT_VISIBILITY_INDEX_BYTES
                ),
            });
        }
        let (body, _) = store.get_with_etag(path).await?;
        if body.len() as u64 > MAX_GIT_VISIBILITY_INDEX_BYTES {
            return Err(crate::error::MetadataError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!(
                    "visibility index body exceeds {} bytes",
                    MAX_GIT_VISIBILITY_INDEX_BYTES
                ),
            });
        }
        Ok(body)
    }

    async fn read_v3(
        store: &Store,
        router: &StoreLayout<Store>,
        generation: u64,
        pack_index_hash: &str,
        git_validation_digest: &str,
    ) -> Result<GitVisibilityIndex> {
        let path = router.git_visibility_path(git_validation_digest);
        let body = read_bounded(store, &path).await?;
        let index: GitVisibilityIndexV3 = serde_json::from_slice(&body).map_err(|error| {
            crate::error::MetadataError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!("invalid visibility index JSON: {error}"),
            }
        })?;
        let index = index.into_index()?;
        index.validate_identity(generation, pack_index_hash, git_validation_digest)?;
        Ok(index)
    }

    async fn read_v1(
        store: &Store,
        router: &StoreLayout<Store>,
        generation: u64,
        pack_index_hash: &str,
        git_validation_digest: &str,
    ) -> Result<GitVisibilityIndex> {
        let path = router.git_visibility_v1_path(generation, pack_index_hash);
        let body = read_bounded(store, &path).await?;
        let index: GitVisibilityIndexV1 = serde_json::from_slice(&body).map_err(|error| {
            crate::error::MetadataError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!("invalid legacy visibility index JSON: {error}"),
            }
        })?;
        index.validate()?;
        if index.generation != generation || index.pack_index_hash != pack_index_hash {
            return Err(crate::error::MetadataError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: "legacy visibility index does not match its immutable path".to_owned(),
            });
        }
        let index = GitVisibilityIndex::new(
            generation,
            pack_index_hash,
            git_validation_digest,
            index.refs,
        );
        index.validate()?;
        Ok(index)
    }

    /// Read a digest-bound proof, falling back to the shipped v1 location.
    pub async fn read_with_format(
        store: &Store,
        router: &StoreLayout<Store>,
        generation: u64,
        pack_index_hash: &str,
        git_validation_digest: &str,
    ) -> Result<GitVisibilityRead> {
        validate_hash(git_validation_digest, "Git validation digest")?;
        match read_v3(
            store,
            router,
            generation,
            pack_index_hash,
            git_validation_digest,
        )
        .await
        {
            Ok(index) => Ok(GitVisibilityRead {
                index,
                format: GitVisibilityFormat::V3,
            }),
            Err(crate::error::MetadataError::Storage {
                source: StorageError::NotFound { .. },
            }) => read_v1(
                store,
                router,
                generation,
                pack_index_hash,
                git_validation_digest,
            )
            .await
            .map(|index| GitVisibilityRead {
                index,
                format: GitVisibilityFormat::V1,
            }),
            Err(error) => Err(error),
        }
    }

    /// Read and validate a visibility proof, including the shipped v1 migration path.
    pub async fn read(
        store: &Store,
        router: &StoreLayout<Store>,
        generation: u64,
        pack_index_hash: &str,
        git_validation_digest: &str,
    ) -> Result<GitVisibilityIndex> {
        read_with_format(
            store,
            router,
            generation,
            pack_index_hash,
            git_validation_digest,
        )
        .await
        .map(|read| read.index)
    }

    /// Read the proof applicable to one manifest, including v1 migration.
    ///
    /// A valid shipped v1 object can share its generation-and-pack key with an
    /// abandoned ref-only candidate. Such an object is not damage to the
    /// current manifest: it is simply inapplicable, so callers may publish the
    /// digest-bound proof without being blocked by the legacy collision.
    pub async fn read_for_manifest(
        store: &Store,
        router: &StoreLayout<Store>,
        manifest: &Manifest,
    ) -> Result<Option<GitVisibilityRead>> {
        match read_with_format(
            store,
            router,
            manifest.generation,
            &manifest.pack_index_hash,
            &manifest.git_validation_digest,
        )
        .await
        {
            Ok(read) if read.index.matches_manifest(manifest) => Ok(Some(read)),
            Ok(read) if read.format == GitVisibilityFormat::V1 => Ok(None),
            Ok(_) => Err(crate::error::MetadataError::CorruptObject {
                path: router
                    .git_visibility_path(&manifest.git_validation_digest)
                    .as_ref()
                    .to_owned(),
                reason: "digest-bound visibility proof does not match its manifest".to_owned(),
            }),
            Err(crate::error::MetadataError::Storage {
                source: StorageError::NotFound { .. },
            }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Upload a visibility proof once and verify an existing immutable value.
    pub async fn upload_if_absent(
        store: &Store,
        router: &StoreLayout<Store>,
        index: &GitVisibilityIndex,
    ) -> Result<()> {
        index.validate()?;
        let stored = GitVisibilityIndexV3::from_index(index)?;
        let body = serde_json::to_vec(&stored).map_err(|error| {
            crate::error::MetadataError::Internal(format!("visibility index serialize: {error}"))
        })?;
        if body.len() as u64 > MAX_GIT_VISIBILITY_INDEX_BYTES {
            return Err(crate::error::MetadataError::CorruptObject {
                path: router
                    .git_visibility_path(&index.git_validation_digest)
                    .as_ref()
                    .to_owned(),
                reason: format!(
                    "visibility index exceeds {} bytes",
                    MAX_GIT_VISIBILITY_INDEX_BYTES
                ),
            });
        }
        let path = router.git_visibility_path(&index.git_validation_digest);
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
                let existing: GitVisibilityIndexV3 =
                    serde_json::from_slice(&existing).map_err(|error| {
                        crate::error::MetadataError::CorruptObject {
                            path: path.as_ref().to_owned(),
                            reason: format!("invalid existing visibility index JSON: {error}"),
                        }
                    })?;
                let existing = existing.into_index()?;
                existing.validate_identity(
                    index.generation,
                    &index.pack_index_hash,
                    &index.git_validation_digest,
                )?;
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
        git_validation_digest: &str,
        final_refs: &BTreeMap<String, String>,
    ) -> Result<Option<GitVisibilityIndex>> {
        let mut refs = if base.refs.is_empty() {
            BTreeMap::new()
        } else {
            match read_for_manifest(store, router, base).await? {
                Some(read) => {
                    let index = read.index;
                    if read.format == GitVisibilityFormat::V1 {
                        upload_if_absent(store, router, &index).await?;
                    }
                    index.refs
                }
                None => return Ok(None),
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
            git_validation_digest,
            refs,
        )))
    }
}

#[cfg(feature = "storage")]
pub use storage::{
    GitVisibilityFormat, GitVisibilityRead, compact_journal_edits, read, read_edit,
    read_for_manifest, read_with_format, upload_edit, upload_if_absent,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_and_authorizes_ref_closure() {
        let index = GitVisibilityIndex::new(
            4,
            "a".repeat(64),
            "c".repeat(64),
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
        let mut index = GitVisibilityIndex::new(4, "a".repeat(64), "c".repeat(64), BTreeMap::new());
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
        let index = GitVisibilityIndex::new(4, "a".repeat(64), "c".repeat(64), refs);
        assert!(index.validate().is_err());
    }

    #[test]
    fn manifest_match_requires_ref_and_peeled_tip_coverage() {
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 4;
        manifest.pack_index_hash = "a".repeat(64);
        manifest
            .refs
            .insert("refs/tags/release".to_owned(), "1".repeat(40));
        manifest
            .peeled_refs
            .insert("refs/tags/release".to_owned(), "2".repeat(40));
        manifest.seal_git_validation();
        let mut index = GitVisibilityIndex::new(
            manifest.generation,
            &manifest.pack_index_hash,
            &manifest.git_validation_digest,
            BTreeMap::from([(
                "refs/tags/release".to_owned(),
                vec!["1".repeat(40), "2".repeat(40)],
            )]),
        );

        assert!(index.matches_manifest(&manifest));
        index.refs.get_mut("refs/tags/release").unwrap().pop();
        assert!(!index.matches_manifest(&manifest));
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
    async fn digest_bound_keys_keep_ref_only_candidates_independent() {
        use std::sync::Arc;

        use crab_storage::{Store, StoreLayout};
        use object_store::memory::InMemory;

        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        let pack_hash = "a".repeat(64);
        let left = GitVisibilityIndex::new(
            7,
            &pack_hash,
            "b".repeat(64),
            BTreeMap::from([("refs/heads/left".to_owned(), vec!["1".repeat(40)])]),
        );
        let right = GitVisibilityIndex::new(
            7,
            &pack_hash,
            "c".repeat(64),
            BTreeMap::from([("refs/heads/right".to_owned(), vec!["2".repeat(40)])]),
        );

        upload_if_absent(&store, &router, &left).await.unwrap();
        upload_if_absent(&store, &router, &right).await.unwrap();

        let left_read = read(&store, &router, 7, &pack_hash, &left.git_validation_digest)
            .await
            .unwrap();
        let right_read = read(&store, &router, 7, &pack_hash, &right.git_validation_digest)
            .await
            .unwrap();
        assert_eq!(left_read, left);
        assert_eq!(right_read, right);
    }

    #[cfg(feature = "storage")]
    #[tokio::test]
    async fn shipped_v1_proof_is_read_and_can_be_backfilled_to_v3() {
        use std::sync::Arc;

        use bytes::Bytes;
        use crab_storage::{Store, StoreLayout};
        use object_store::memory::InMemory;

        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        let generation = 7;
        let pack_hash = "a".repeat(64);
        let digest = "b".repeat(64);
        let legacy = serde_json::json!({
            "version": 1,
            "generation": generation,
            "pack_index_hash": pack_hash.clone(),
            "refs": {"refs/heads/main": ["1".repeat(40)]},
        });
        store
            .put(
                &router.git_visibility_v1_path(generation, &pack_hash),
                Bytes::from(serde_json::to_vec(&legacy).unwrap()),
            )
            .await
            .unwrap();

        let migrated = read_with_format(&store, &router, generation, &pack_hash, &digest)
            .await
            .unwrap();
        assert_eq!(migrated.format, GitVisibilityFormat::V1);
        assert_eq!(migrated.index.git_validation_digest, digest);

        upload_if_absent(&store, &router, &migrated.index)
            .await
            .unwrap();
        let current = read_with_format(&store, &router, generation, &pack_hash, &digest)
            .await
            .unwrap();
        assert_eq!(current.format, GitVisibilityFormat::V3);
        assert_eq!(current.index, migrated.index);
    }

    #[cfg(feature = "storage")]
    #[tokio::test]
    async fn orphaned_v1_candidate_does_not_block_digest_bound_proof() {
        use std::sync::Arc;

        use bytes::Bytes;
        use crab_storage::{Store, StoreLayout};
        use object_store::memory::InMemory;

        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        let generation = 7;
        let pack_hash = "a".repeat(64);
        let legacy = serde_json::json!({
            "version": 1,
            "generation": generation,
            "pack_index_hash": pack_hash.clone(),
            "refs": {"refs/heads/main": ["1".repeat(40)]},
        });
        store
            .put(
                &router.git_visibility_v1_path(generation, &pack_hash),
                Bytes::from(serde_json::to_vec(&legacy).unwrap()),
            )
            .await
            .unwrap();
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = generation;
        manifest.pack_index_hash.clone_from(&pack_hash);
        manifest
            .refs
            .insert("refs/heads/main".to_owned(), "2".repeat(40));
        manifest.seal_git_validation();

        assert!(
            read_for_manifest(&store, &router, &manifest)
                .await
                .unwrap()
                .is_none()
        );

        let current = GitVisibilityIndex::new(
            generation,
            &pack_hash,
            &manifest.git_validation_digest,
            BTreeMap::from([("refs/heads/main".to_owned(), vec!["2".repeat(40)])]),
        );
        upload_if_absent(&store, &router, &current).await.unwrap();
        let read = read_for_manifest(&store, &router, &manifest)
            .await
            .unwrap()
            .expect("digest-bound proof should supersede the orphaned v1 candidate");
        assert_eq!(read.format, GitVisibilityFormat::V3);
        assert_eq!(read.index, current);
    }

    #[cfg(feature = "storage")]
    #[tokio::test]
    async fn v3_storage_deduplicates_shared_ref_history() {
        use std::sync::Arc;

        use crab_storage::{Store, StoreLayout};
        use object_store::memory::InMemory;

        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        let objects = (0..256)
            .map(|position| format!("{position:040x}"))
            .collect::<Vec<_>>();
        let mut refs = (0..8)
            .map(|position| (format!("refs/heads/branch-{position}"), objects.clone()))
            .collect::<BTreeMap<_, _>>();
        refs.insert("refs/heads/sparse".to_owned(), vec![objects[0].clone()]);
        let index = GitVisibilityIndex::new(7, "a".repeat(64), "b".repeat(64), refs);
        let expanded = serde_json::to_vec(&serde_json::json!({
            "version": index.version,
            "generation": index.generation,
            "pack_index_hash": &index.pack_index_hash,
            "git_validation_digest": &index.git_validation_digest,
            "refs": &index.refs,
        }))
        .unwrap();

        upload_if_absent(&store, &router, &index).await.unwrap();

        let (body, _) = store
            .get_with_etag(&router.git_visibility_path(&index.git_validation_digest))
            .await
            .unwrap();
        let stored: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(stored["objects"].as_array().unwrap().len(), 256);
        assert!(stored["refs"]["refs/heads/branch-0"]["bitmap"].is_string());
        assert!(stored["refs"]["refs/heads/sparse"]["sparse"].is_array());
        assert!(body.len() < expanded.len() / 4);
        assert_eq!(
            read(
                &store,
                &router,
                index.generation,
                &index.pack_index_hash,
                &index.git_validation_digest,
            )
            .await
            .unwrap(),
            index
        );
    }

    #[cfg(feature = "storage")]
    #[tokio::test]
    async fn v3_storage_rejects_closure_positions_outside_dictionary() {
        use std::sync::Arc;

        use bytes::Bytes;
        use crab_storage::{Store, StoreLayout};
        use object_store::memory::InMemory;

        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        let pack_hash = "a".repeat(64);
        let digest = "b".repeat(64);
        let malformed = serde_json::json!({
            "version": 3,
            "generation": 7,
            "pack_index_hash": pack_hash.clone(),
            "git_validation_digest": digest.clone(),
            "objects": ["1".repeat(40)],
            "refs": {"refs/heads/main": {"sparse": [1]}},
        });
        store
            .put(
                &router.git_visibility_path(&digest),
                Bytes::from(serde_json::to_vec(&malformed).unwrap()),
            )
            .await
            .unwrap();

        assert!(read(&store, &router, 7, &pack_hash, &digest).await.is_err());
    }

    #[cfg(feature = "storage")]
    #[tokio::test]
    async fn v3_storage_rejects_bitmap_bits_outside_dictionary() {
        use std::sync::Arc;

        use bytes::Bytes;
        use crab_storage::{Store, StoreLayout};
        use object_store::memory::InMemory;

        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        let pack_hash = "a".repeat(64);
        let digest = "b".repeat(64);
        let malformed = serde_json::json!({
            "version": 3,
            "generation": 7,
            "pack_index_hash": pack_hash.clone(),
            "git_validation_digest": digest.clone(),
            "objects": ["1".repeat(40)],
            "refs": {"refs/heads/main": {"bitmap": "gA"}},
        });
        store
            .put(
                &router.git_visibility_path(&digest),
                Bytes::from(serde_json::to_vec(&malformed).unwrap()),
            )
            .await
            .unwrap();

        assert!(read(&store, &router, 7, &pack_hash, &digest).await.is_err());
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
            &base.git_validation_digest,
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
        let mut final_manifest = base.clone();
        final_manifest.generation = 5;
        final_manifest.pack_index_hash = "e".repeat(64);
        final_manifest.refs.clone_from(&final_refs);
        final_manifest.seal_git_validation();

        let compacted = compact_journal_edits(
            &store,
            &router,
            &base,
            &edits,
            5,
            &"e".repeat(64),
            &final_manifest.git_validation_digest,
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
