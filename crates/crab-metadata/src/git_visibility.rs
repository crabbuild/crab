//! Immutable Git object visibility proofs for upload-pack admission.
//!
//! The manifest names refs and the pack index names storage. Neither one is
//! sufficient to authorize an arbitrary object ID: upload-pack also needs the
//! complete object closure of the refs visible to the caller. This module
//! stores that closure in one generation-bound, immutable object.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use crate::error::{MetadataError, Result};
use crate::manifests::Manifest;

/// Current serialized visibility-index format.
pub const GIT_VISIBILITY_INDEX_VERSION: u32 = 5;

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

    /// Build normalized evidence from exact added and removed object sets.
    #[must_use]
    pub fn from_delta_objects(
        old_oid: Option<String>,
        new_oid: String,
        mut added: Vec<String>,
        mut removed: Vec<String>,
    ) -> Self {
        added.sort_unstable();
        added.dedup();
        removed.sort_unstable();
        removed.dedup();
        Self {
            version: GIT_VISIBILITY_EDIT_VERSION,
            old_oid,
            new_oid,
            replaces: false,
            added,
            removed,
        }
    }

    /// Build normalized evidence that replaces an unavailable prior closure.
    #[must_use]
    pub fn replacement(old_oid: Option<String>, new_oid: String, new: &BTreeSet<String>) -> Self {
        Self::from_replacement_objects(old_oid, new_oid, new.iter().cloned().collect())
    }

    /// Build normalized replacement evidence from a complete object list.
    #[must_use]
    pub fn from_replacement_objects(
        old_oid: Option<String>,
        new_oid: String,
        mut added: Vec<String>,
    ) -> Self {
        added.sort_unstable();
        added.dedup();
        Self {
            version: GIT_VISIBILITY_EDIT_VERSION,
            old_oid,
            new_oid,
            replaces: true,
            added,
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

/// Binary SHA-1 object identity used by a visibility proof.
pub type GitVisibilityOid = [u8; 20];

#[derive(Debug, Clone, PartialEq, Eq)]
enum GitVisibilityClosure {
    Sparse(Vec<u32>),
    Bitmap(Vec<u8>),
}

const MAX_VISIBILITY_TRANSITIONS_PER_REF: usize = 64;
const MAX_VISIBILITY_HISTORY_TRANSITIONS_PER_REF: usize = 1_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitVisibilityTransition {
    from_oid: GitVisibilityOid,
    to_oid: GitVisibilityOid,
    objects: GitVisibilityClosure,
}

impl GitVisibilityClosure {
    fn from_positions(positions: Vec<u32>, object_count: usize) -> Result<Self> {
        let bitmap_len = object_count.div_ceil(8);
        if bitmap_len >= positions.len().saturating_mul(std::mem::size_of::<u32>()) {
            return Ok(Self::Sparse(positions));
        }
        let mut bitmap = vec![0u8; bitmap_len];
        for position in positions {
            let position = usize::try_from(position)
                .map_err(|_| corrupt("visibility closure position cannot be represented"))?;
            let byte = bitmap
                .get_mut(position / 8)
                .ok_or_else(|| corrupt("visibility closure position is outside its dictionary"))?;
            *byte |= 1 << (position % 8);
        }
        Ok(Self::Bitmap(bitmap))
    }

    fn validate(&self, object_count: usize) -> Result<u64> {
        match self {
            Self::Sparse(positions) => {
                let mut previous = None;
                for position in positions {
                    let raw_position = *position;
                    if previous.is_some_and(|previous| previous >= raw_position) {
                        return Err(corrupt(
                            "visibility closure positions must be sorted and deduplicated",
                        ));
                    }
                    let position = usize::try_from(raw_position).map_err(|_| {
                        corrupt("visibility closure position cannot be represented")
                    })?;
                    if position >= object_count {
                        return Err(corrupt(
                            "visibility closure position is outside its dictionary",
                        ));
                    }
                    previous = Some(raw_position);
                }
                u64::try_from(positions.len())
                    .map_err(|_| corrupt("visibility closure object count cannot be represented"))
            }
            Self::Bitmap(bitmap) => {
                if bitmap.len() != object_count.div_ceil(8) {
                    return Err(corrupt(
                        "visibility closure bitmap length does not match its dictionary",
                    ));
                }
                if let Some(last) = bitmap.last()
                    && !object_count.is_multiple_of(8)
                    && last >> (object_count % 8) != 0
                {
                    return Err(corrupt(
                        "visibility closure bitmap sets a position outside its dictionary",
                    ));
                }
                bitmap.iter().try_fold(0u64, |count, byte| {
                    count
                        .checked_add(u64::from(byte.count_ones()))
                        .ok_or_else(|| corrupt("visibility closure object count overflows"))
                })
            }
        }
    }

    fn contains(&self, position: u32) -> bool {
        match self {
            Self::Sparse(positions) => positions.binary_search(&position).is_ok(),
            Self::Bitmap(bitmap) => usize::try_from(position)
                .ok()
                .and_then(|position| bitmap.get(position / 8).map(|byte| (*byte, position)))
                .is_some_and(|(byte, position)| byte & (1 << (position % 8)) != 0),
        }
    }

    fn len(&self) -> u64 {
        match self {
            Self::Sparse(positions) => u64::try_from(positions.len()).unwrap_or(u64::MAX),
            Self::Bitmap(bitmap) => bitmap.iter().map(|byte| u64::from(byte.count_ones())).sum(),
        }
    }

    fn union_into(&self, union: &mut [u8]) {
        match self {
            Self::Sparse(positions) => {
                for position in positions {
                    if let Ok(position) = usize::try_from(*position)
                        && let Some(byte) = union.get_mut(position / 8)
                    {
                        *byte |= 1 << (position % 8);
                    }
                }
            }
            Self::Bitmap(bitmap) => {
                for (output, input) in union.iter_mut().zip(bitmap) {
                    *output |= input;
                }
            }
        }
    }

    fn positions(&self) -> Vec<u32> {
        match self {
            Self::Sparse(positions) => positions.clone(),
            Self::Bitmap(bitmap) => bitmap_positions(bitmap),
        }
    }
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
    objects: Vec<GitVisibilityOid>,
    positions: HashMap<GitVisibilityOid, u32>,
    refs: BTreeMap<String, GitVisibilityClosure>,
    transitions: BTreeMap<String, Vec<GitVisibilityTransition>>,
    incremental_history: BTreeMap<String, Vec<GitVisibilityTransition>>,
}

impl GitVisibilityIndex {
    /// Build a normalized proof from ref-rooted object sets.
    pub fn new(
        generation: u64,
        pack_index_hash: impl Into<String>,
        git_validation_digest: impl Into<String>,
        refs: BTreeMap<String, Vec<String>>,
    ) -> Result<Self> {
        if refs.len() > MAX_GIT_VISIBILITY_REFS {
            return Err(corrupt("visibility index contains too many refs"));
        }
        let mut decoded_refs = BTreeMap::new();
        let mut dictionary = BTreeSet::new();
        let mut membership_count = 0u64;
        for (name, objects) in refs {
            validate_ref_name(&name)?;
            let mut decoded = objects
                .into_iter()
                .map(|oid| decode_oid(&oid))
                .collect::<Result<Vec<_>>>()?;
            decoded.sort_unstable();
            decoded.dedup();
            membership_count =
                membership_count
                    .checked_add(u64::try_from(decoded.len()).map_err(|_| {
                        corrupt("visibility index object count cannot be represented")
                    })?)
                    .ok_or_else(|| corrupt("visibility index object count overflows"))?;
            if membership_count > MAX_GIT_VISIBILITY_OBJECTS {
                return Err(corrupt("visibility index contains too many objects"));
            }
            dictionary.extend(decoded.iter().copied());
            decoded_refs.insert(name, decoded);
        }
        let objects = dictionary.into_iter().collect::<Vec<_>>();
        let positions = build_positions(&objects)?;
        let refs = decoded_refs
            .into_iter()
            .map(|(name, objects)| {
                let closure = objects
                    .into_iter()
                    .map(|oid| {
                        positions.get(&oid).copied().ok_or_else(|| {
                            corrupt("visibility closure object is absent from its dictionary")
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok((
                    name,
                    GitVisibilityClosure::from_positions(closure, positions.len())?,
                ))
            })
            .collect::<Result<_>>()?;
        let index = Self {
            version: GIT_VISIBILITY_INDEX_VERSION,
            generation,
            pack_index_hash: pack_index_hash.into(),
            git_validation_digest: git_validation_digest.into(),
            objects,
            positions,
            refs,
            transitions: BTreeMap::new(),
            incremental_history: BTreeMap::new(),
        };
        index.validate()?;
        Ok(index)
    }

    fn from_parts(
        version: u32,
        generation: u64,
        pack_index_hash: String,
        git_validation_digest: String,
        objects: Vec<GitVisibilityOid>,
        refs: BTreeMap<String, GitVisibilityClosure>,
        transitions: BTreeMap<String, Vec<GitVisibilityTransition>>,
    ) -> Result<Self> {
        let positions = build_positions(&objects)?;
        let index = Self {
            version,
            generation,
            pack_index_hash,
            git_validation_digest,
            objects,
            positions,
            refs,
            transitions,
            incremental_history: BTreeMap::new(),
        };
        index.validate()?;
        Ok(index)
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
        if self.objects.len() as u64 > MAX_GIT_VISIBILITY_OBJECTS {
            return Err(corrupt(
                "visibility object dictionary contains too many objects",
            ));
        }
        if self.positions.len() != self.objects.len()
            || self.objects.iter().enumerate().any(|(position, oid)| {
                self.positions
                    .get(oid)
                    .and_then(|position| usize::try_from(*position).ok())
                    != Some(position)
            })
        {
            return Err(corrupt(
                "visibility object lookup does not match its dictionary",
            ));
        }
        if self.refs.len() > MAX_GIT_VISIBILITY_REFS {
            return Err(corrupt("visibility index contains too many refs"));
        }
        let mut membership_count = 0u64;
        for (name, closure) in &self.refs {
            validate_ref_name(name)?;
            membership_count = membership_count
                .checked_add(closure.validate(self.objects.len())?)
                .ok_or_else(|| corrupt("visibility index object count overflows"))?;
            if membership_count > MAX_GIT_VISIBILITY_OBJECTS {
                return Err(corrupt("visibility index contains too many objects"));
            }
        }
        for (name, transitions) in &self.transitions {
            if !self.refs.contains_key(name)
                || transitions.len() > MAX_VISIBILITY_TRANSITIONS_PER_REF
            {
                return Err(corrupt("visibility transitions do not match their ref"));
            }
            for transition in transitions {
                let ref_closure = self
                    .refs
                    .get(name)
                    .ok_or_else(|| corrupt("visibility transition ref closure is absent"))?;
                if !self.contains_in_ref(name, &transition.from_oid)
                    || !self.contains_in_ref(name, &transition.to_oid)
                    || transition.objects.validate(self.objects.len())? > MAX_GIT_VISIBILITY_OBJECTS
                    || transition
                        .objects
                        .positions()
                        .into_iter()
                        .any(|position| !ref_closure.contains(position))
                {
                    return Err(corrupt("visibility transition is invalid"));
                }
            }
        }
        for (name, transitions) in &self.incremental_history {
            if !self.refs.contains_key(name)
                || transitions.len() > MAX_VISIBILITY_HISTORY_TRANSITIONS_PER_REF
            {
                return Err(corrupt(
                    "incremental visibility history does not match its ref",
                ));
            }
            let ref_closure = self
                .refs
                .get(name)
                .ok_or_else(|| corrupt("incremental visibility history ref closure is absent"))?;
            for transition in transitions {
                if !self.contains_in_ref(name, &transition.from_oid)
                    || !self.contains_in_ref(name, &transition.to_oid)
                    || transition.objects.validate(self.objects.len())? > MAX_GIT_VISIBILITY_OBJECTS
                    || transition
                        .objects
                        .positions()
                        .into_iter()
                        .any(|position| !ref_closure.contains(position))
                {
                    return Err(corrupt("incremental visibility history is invalid"));
                }
            }
        }
        Ok(())
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
                decode_oid(tip)
                    .ok()
                    .is_some_and(|oid| self.contains_in_ref(name, &oid))
            })
            && manifest.peeled_refs.iter().all(|(name, peeled)| {
                decode_oid(peeled)
                    .ok()
                    .is_some_and(|oid| self.contains_in_ref(name, &oid))
            })
    }

    /// Return the number of refs in this proof.
    #[must_use]
    pub fn ref_count(&self) -> usize {
        self.refs.len()
    }

    /// Return whether this proof contains the named ref.
    #[must_use]
    pub fn contains_ref(&self, name: &str) -> bool {
        self.refs.contains_key(name)
    }

    /// Return whether one ref closure contains an object.
    #[must_use]
    pub fn contains_in_ref(&self, name: &str, oid: &GitVisibilityOid) -> bool {
        self.positions.get(oid).is_some_and(|position| {
            self.refs
                .get(name)
                .is_some_and(|closure| closure.contains(*position))
        })
    }

    /// Return whether one ref closure contains a canonical hexadecimal object ID.
    #[must_use]
    pub fn contains_hex_in_ref(&self, name: &str, oid: &str) -> bool {
        decode_oid(oid)
            .ok()
            .is_some_and(|oid| self.contains_in_ref(name, &oid))
    }

    /// Return one ref closure as canonical hexadecimal IDs.
    #[must_use]
    pub fn objects_for_ref(&self, name: &str) -> Option<Vec<String>> {
        self.refs.get(name).map(|closure| {
            let mut objects = closure
                .positions()
                .into_iter()
                .filter_map(|position| usize::try_from(position).ok())
                .filter_map(|position| self.objects.get(position))
                .map(encode_oid)
                .collect::<Vec<_>>();
            // Catalog ordinals follow physical pack publication order. Keep the
            // public closure contract sorted so set operations remain correct.
            objects.sort_unstable();
            objects
        })
    }

    /// Materialize all ref closures for migration and rebuild boundaries.
    #[must_use]
    pub fn ref_closures(&self) -> BTreeMap<String, Vec<String>> {
        self.refs
            .keys()
            .filter_map(|name| {
                self.objects_for_ref(name)
                    .map(|objects| (name.clone(), objects))
            })
            .collect()
    }

    /// Return the union of objects rooted at the supplied visible refs.
    #[must_use]
    pub fn objects_for_refs<'a, I>(&self, refs: I) -> Vec<GitVisibilityOid>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let union = self.union_for_refs(refs);
        bitmap_positions(&union)
            .into_iter()
            .filter_map(|position| usize::try_from(position).ok())
            .filter_map(|position| self.objects.get(position).copied())
            .collect()
    }

    /// Return the selected-ref union minus the excluded-ref union.
    #[must_use]
    pub fn objects_for_ref_difference<'a, I, J>(
        &self,
        selected: I,
        excluded: J,
    ) -> Vec<GitVisibilityOid>
    where
        I: IntoIterator<Item = &'a str>,
        J: IntoIterator<Item = &'a str>,
    {
        let mut selected = self.union_for_refs(selected);
        let excluded = self.union_for_refs(excluded);
        for (selected, excluded) in selected.iter_mut().zip(excluded) {
            *selected &= !excluded;
        }
        bitmap_positions(&selected)
            .into_iter()
            .filter_map(|position| usize::try_from(position).ok())
            .filter_map(|position| self.objects.get(position).copied())
            .collect()
    }

    /// Return a proven incremental closure for one exact prior ref tip.
    #[must_use]
    pub fn incremental_objects(
        &self,
        name: &str,
        to_oid: &GitVisibilityOid,
        haves: &[GitVisibilityOid],
    ) -> Option<Vec<GitVisibilityOid>> {
        if haves.contains(to_oid) {
            return Some(Vec::new());
        }
        if let Some(transition) = self
            .transitions
            .get(name)
            .into_iter()
            .flat_map(|transitions| transitions.iter().rev())
            .find(|transition| transition.to_oid == *to_oid && haves.contains(&transition.from_oid))
        {
            return Some(
                transition
                    .objects
                    .positions()
                    .into_iter()
                    .filter_map(|position| usize::try_from(position).ok())
                    .filter_map(|position| self.objects.get(position).copied())
                    .collect(),
            );
        }

        let history = self.incremental_history.get(name)?;
        let by_from = history
            .iter()
            .map(|transition| (transition.from_oid, transition))
            .collect::<HashMap<_, _>>();
        for have in haves {
            let mut current = *have;
            let mut selected = vec![0_u8; self.objects.len().div_ceil(8)];
            let mut steps = 0usize;
            while current != *to_oid {
                let Some(transition) = by_from.get(&current) else {
                    break;
                };
                transition.objects.union_into(&mut selected);
                current = transition.to_oid;
                steps = steps.saturating_add(1);
                if steps > history.len() {
                    break;
                }
            }
            if current == *to_oid {
                return Some(
                    bitmap_positions(&selected)
                        .into_iter()
                        .filter_map(|position| usize::try_from(position).ok())
                        .filter_map(|position| self.objects.get(position).copied())
                        .collect(),
                );
            }
        }
        None
    }

    /// Count the distinct objects rooted at the supplied visible refs.
    #[must_use]
    pub fn object_count_for_refs<'a, I>(&self, refs: I) -> usize
    where
        I: IntoIterator<Item = &'a str>,
    {
        self.union_for_refs(refs)
            .iter()
            .map(|byte| byte.count_ones() as usize)
            .sum()
    }

    /// Return a stable digest of the object authorization union for selected refs.
    ///
    /// Ref names are deliberately excluded so reusable artifacts reveal only
    /// the immutable object authorization set, not mutable repository labels.
    #[must_use]
    pub fn authorization_digest_for_refs<'a, I>(&self, refs: I) -> [u8; 32]
    where
        I: IntoIterator<Item = &'a str>,
    {
        let union = self.union_for_refs(refs);
        let mut hash = blake3::Hasher::new();
        hash.update(b"crab.git-visibility.authorization.v1\0");
        hash.update(self.git_validation_digest.as_bytes());
        for position in bitmap_positions(&union) {
            if let Ok(position) = usize::try_from(position)
                && let Some(oid) = self.objects.get(position)
            {
                hash.update(oid);
            }
        }
        *hash.finalize().as_bytes()
    }

    /// Return whether an object is proven reachable from one of the supplied refs.
    #[must_use]
    pub fn contains_for_refs<'a, I>(&self, refs: I, oid: &GitVisibilityOid) -> bool
    where
        I: IntoIterator<Item = &'a str>,
    {
        self.positions.get(oid).is_some_and(|position| {
            refs.into_iter().any(|name| {
                self.refs
                    .get(name)
                    .is_some_and(|closure| closure.contains(*position))
            })
        })
    }

    /// Return total ref memberships without materializing object IDs.
    #[must_use]
    pub fn membership_count(&self) -> u64 {
        self.refs.values().map(GitVisibilityClosure::len).sum()
    }

    fn remove_ref(&mut self, name: &str) {
        self.refs.remove(name);
        self.transitions.remove(name);
        self.incremental_history.remove(name);
    }

    fn apply_edit(&mut self, name: String, edit: &GitVisibilityEdit) -> Result<()> {
        let prior = self.objects_for_ref(&name);
        let closure = edit.apply(prior.as_deref())?;
        let mut positions = Vec::with_capacity(closure.len());
        for oid in closure {
            let oid = decode_oid(&oid)?;
            let position = match self.positions.get(&oid).copied() {
                Some(position) => position,
                None => {
                    let position = u32::try_from(self.objects.len())
                        .map_err(|_| corrupt("visibility object dictionary is too large"))?;
                    self.objects.push(oid);
                    self.positions.insert(oid, position);
                    position
                }
            };
            positions.push(position);
        }
        positions.sort_unstable();
        let bitmap_len = self.objects.len().div_ceil(8);
        for closure in self.refs.values_mut() {
            if let GitVisibilityClosure::Bitmap(bitmap) = closure {
                bitmap.resize(bitmap_len, 0);
            }
        }
        for transitions in self.incremental_history.values_mut() {
            for transition in transitions {
                if let GitVisibilityClosure::Bitmap(bitmap) = &mut transition.objects {
                    bitmap.resize(bitmap_len, 0);
                }
            }
        }
        self.refs.insert(
            name.clone(),
            GitVisibilityClosure::from_positions(positions, self.objects.len())?,
        );
        let from_oid = edit.old_oid.as_deref().map(decode_oid).transpose()?;
        let to_oid = decode_oid(&edit.new_oid)?;
        if edit.replaces || !edit.removed.is_empty() {
            self.transitions.remove(&name);
            self.incremental_history.remove(&name);
            return Ok(());
        }
        let Some(from_oid) = from_oid else {
            self.transitions.remove(&name);
            self.incremental_history.remove(&name);
            return Ok(());
        };
        let added = edit
            .added
            .iter()
            .map(|oid| {
                let oid = decode_oid(oid)?;
                self.positions
                    .get(&oid)
                    .copied()
                    .ok_or_else(|| corrupt("visibility transition object is absent"))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut added = added;
        added.sort_unstable();
        added.dedup();
        let transitions = self.transitions.entry(name.clone()).or_default();
        for transition in transitions.iter_mut() {
            let mut positions = transition.objects.positions();
            positions.extend(added.iter().copied());
            positions.sort_unstable();
            positions.dedup();
            transition.to_oid = to_oid;
            transition.objects =
                GitVisibilityClosure::from_positions(positions, self.objects.len())?;
        }
        transitions.retain(|transition| transition.from_oid != from_oid);
        transitions.push(GitVisibilityTransition {
            from_oid,
            to_oid,
            objects: GitVisibilityClosure::from_positions(added.clone(), self.objects.len())?,
        });
        if transitions.len() > MAX_VISIBILITY_TRANSITIONS_PER_REF {
            transitions.remove(0);
        }
        let history = self.incremental_history.entry(name).or_default();
        history.push(GitVisibilityTransition {
            from_oid,
            to_oid,
            objects: GitVisibilityClosure::from_positions(added, self.objects.len())?,
        });
        if history.len() > MAX_VISIBILITY_HISTORY_TRANSITIONS_PER_REF {
            history.remove(0);
        }
        Ok(())
    }

    fn bind_identity(
        &mut self,
        generation: u64,
        pack_index_hash: &str,
        git_validation_digest: &str,
    ) -> Result<()> {
        self.version = GIT_VISIBILITY_INDEX_VERSION;
        self.generation = generation;
        self.pack_index_hash = pack_index_hash.to_owned();
        self.git_validation_digest = git_validation_digest.to_owned();
        self.validate()
    }

    fn union_for_refs<'a, I>(&self, refs: I) -> Vec<u8>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut union = vec![0u8; self.objects.len().div_ceil(8)];
        for closure in refs.into_iter().filter_map(|name| self.refs.get(name)) {
            closure.union_into(&mut union);
        }
        union
    }
}

fn build_positions(objects: &[GitVisibilityOid]) -> Result<HashMap<GitVisibilityOid, u32>> {
    let mut positions = HashMap::with_capacity(objects.len());
    for (position, oid) in objects.iter().copied().enumerate() {
        let position = u32::try_from(position)
            .map_err(|_| corrupt("visibility object dictionary is too large"))?;
        if positions.insert(oid, position).is_some() {
            return Err(corrupt("visibility object dictionary must be deduplicated"));
        }
    }
    Ok(positions)
}

fn bitmap_positions(bitmap: &[u8]) -> Vec<u32> {
    let mut positions = Vec::new();
    for (byte_index, byte) in bitmap.iter().enumerate() {
        for bit_index in 0..8 {
            if byte & (1 << bit_index) == 0 {
                continue;
            }
            if let Some(position) = byte_index
                .checked_mul(8)
                .and_then(|position| position.checked_add(bit_index))
                .and_then(|position| u32::try_from(position).ok())
            {
                positions.push(position);
            }
        }
    }
    positions
}

fn validate_ref_name(name: &str) -> Result<()> {
    if name.is_empty() || name.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(corrupt("visibility index contains an invalid ref name"));
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

fn decode_oid(value: &str) -> Result<GitVisibilityOid> {
    validate_oid(value)?;
    let mut decoded = [0u8; 20];
    let (pairs, remainder) = value.as_bytes().as_chunks::<2>();
    if !remainder.is_empty() {
        return Err(corrupt("visibility object ID has an incomplete byte"));
    }
    for (output, pair) in decoded.iter_mut().zip(pairs) {
        *output = (hex_value(pair[0]) << 4) | hex_value(pair[1]);
    }
    Ok(decoded)
}

fn hex_value(value: u8) -> u8 {
    match value {
        b'0'..=b'9' => value - b'0',
        b'a'..=b'f' => value - b'a' + 10,
        _ => 0,
    }
}

fn encode_oid(oid: &GitVisibilityOid) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(40);
    for byte in oid {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn validate_ref_closures(refs: &BTreeMap<String, Vec<String>>) -> Result<()> {
    if refs.len() > MAX_GIT_VISIBILITY_REFS {
        return Err(corrupt("visibility index contains too many refs"));
    }
    let mut object_count = 0u64;
    for (name, objects) in refs {
        validate_ref_name(name)?;
        object_count = object_count
            .checked_add(
                u64::try_from(objects.len())
                    .map_err(|_| corrupt("visibility index object count cannot be represented"))?,
            )
            .ok_or_else(|| corrupt("visibility index object count overflows"))?;
        if object_count > MAX_GIT_VISIBILITY_OBJECTS {
            return Err(corrupt("visibility index contains too many objects"));
        }
        validate_sorted_oids(objects, "closure")?;
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
    #[cfg(feature = "remote-index")]
    use std::sync::Arc;
    #[cfg(feature = "remote-index")]
    use std::time::Duration;

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD_NO_PAD;
    use bytes::Bytes;
    use crab_storage::{StorageError, Store, StoreLayout};
    #[cfg(feature = "remote-index")]
    use crab_xet::hash::MerkleHash;
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
    const GIT_VISIBILITY_INDEX_V3_VERSION: u32 = 3;
    const GIT_VISIBILITY_INDEX_V4_VERSION: u32 = 4;

    /// Stored format used to satisfy a visibility read.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum GitVisibilityFormat {
        /// Catalog-ordinal proof with no independent OID dictionary.
        V5,
        /// Binary-runtime proof with retained incremental ref transitions.
        V4,
        /// Dictionary-compressed, digest-bound proof written by earlier Crab versions.
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

    // Version 3 stores a sorted hexadecimal dictionary. Reads normalize it to
    // the binary runtime dictionary without expanding per-ref OID ownership.
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
    #[serde(deny_unknown_fields)]
    struct GitVisibilityIndexV4 {
        version: u32,
        generation: u64,
        pack_index_hash: String,
        git_validation_digest: String,
        objects: Vec<String>,
        refs: BTreeMap<String, GitVisibilityClosureV3>,
        transitions: BTreeMap<String, Vec<GitVisibilityTransitionV4>>,
        #[serde(default)]
        incremental_history: BTreeMap<String, Vec<GitVisibilityTransitionV4>>,
    }

    #[cfg(feature = "remote-index")]
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct GitVisibilityIndexV5 {
        version: u32,
        generation: u64,
        pack_index_hash: String,
        git_validation_digest: String,
        catalog_digest: String,
        object_count: u64,
        refs: BTreeMap<String, GitVisibilityClosureV3>,
        transitions: BTreeMap<String, Vec<GitVisibilityTransitionV5>>,
        #[serde(default)]
        incremental_history: BTreeMap<String, Vec<GitVisibilityTransitionV5>>,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct GitVisibilityTransitionV4 {
        from_oid: String,
        to_oid: String,
        objects: GitVisibilityClosureV3,
    }

    #[cfg(feature = "remote-index")]
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct GitVisibilityTransitionV5 {
        from_ordinal: u32,
        to_ordinal: u32,
        objects: GitVisibilityClosureV3,
    }

    #[derive(Deserialize)]
    struct GitVisibilityVersion {
        version: u32,
    }

    #[derive(Clone, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum GitVisibilityClosureV3 {
        Sparse(Vec<u32>),
        Bitmap(String),
    }

    impl GitVisibilityIndexV3 {
        fn from_index(index: &GitVisibilityIndex) -> Result<Self> {
            index.validate()?;
            let mut used = vec![false; index.objects.len()];
            for closure in index.refs.values() {
                for position in closure.positions() {
                    let position = usize::try_from(position).map_err(|_| {
                        super::corrupt("visibility closure position cannot be represented")
                    })?;
                    let covered = used.get_mut(position).ok_or_else(|| {
                        super::corrupt("visibility closure position is outside its dictionary")
                    })?;
                    *covered = true;
                }
            }
            let mut ordered = index
                .objects
                .iter()
                .copied()
                .enumerate()
                .filter(|(position, _)| used[*position])
                .collect::<Vec<_>>();
            ordered.sort_unstable_by_key(|(_, oid)| *oid);
            let mut remapped = vec![None; index.objects.len()];
            let mut objects = Vec::with_capacity(ordered.len());
            for (new_position, (old_position, oid)) in ordered.into_iter().enumerate() {
                let new_position = u32::try_from(new_position)
                    .map_err(|_| super::corrupt("visibility object dictionary is too large"))?;
                remapped[old_position] = Some(new_position);
                objects.push(super::encode_oid(&oid));
            }
            let refs = index
                .refs
                .iter()
                .map(|(name, closure)| {
                    let positions = closure
                        .positions()
                        .into_iter()
                        .map(|position| {
                            usize::try_from(position)
                                .ok()
                                .and_then(|position| remapped.get(position).copied().flatten())
                                .ok_or_else(|| {
                                    super::corrupt(
                                        "visibility closure position is outside its dictionary",
                                    )
                                })
                        })
                        .collect::<Result<Vec<_>>>()?;
                    let mut positions = positions;
                    positions.sort_unstable();
                    Ok((
                        name.clone(),
                        GitVisibilityClosureV3::from_positions(positions, objects.len())?,
                    ))
                })
                .collect::<Result<BTreeMap<_, _>>>()?;
            Ok(Self {
                version: GIT_VISIBILITY_INDEX_V3_VERSION,
                generation: index.generation,
                pack_index_hash: index.pack_index_hash.clone(),
                git_validation_digest: index.git_validation_digest.clone(),
                objects,
                refs,
            })
        }

        fn into_index(self) -> Result<GitVisibilityIndex> {
            if self.version != GIT_VISIBILITY_INDEX_V3_VERSION {
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
            let objects = self
                .objects
                .iter()
                .map(|oid| super::decode_oid(oid))
                .collect::<Result<Vec<_>>>()?;
            let mut membership_count = 0u64;
            let mut covered = vec![false; objects.len()];
            let refs = self
                .refs
                .into_iter()
                .map(|(name, closure)| {
                    if name.is_empty() || name.bytes().any(|byte| byte.is_ascii_control()) {
                        return Err(super::corrupt(
                            "visibility index contains an invalid ref name",
                        ));
                    }
                    let positions = closure.into_positions(objects.len())?;
                    membership_count = membership_count
                        .checked_add(u64::try_from(positions.len()).map_err(|_| {
                            super::corrupt("visibility index object count cannot be represented")
                        })?)
                        .ok_or_else(|| super::corrupt("visibility index object count overflows"))?;
                    if membership_count > MAX_GIT_VISIBILITY_OBJECTS {
                        return Err(super::corrupt("visibility index contains too many objects"));
                    }
                    let mut prior_position = None;
                    let positions = positions
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
                            objects.get(position).ok_or_else(|| {
                                super::corrupt(
                                    "visibility closure position is outside its dictionary",
                                )
                            })?;
                            covered[position] = true;
                            u32::try_from(position).map_err(|_| {
                                super::corrupt("visibility closure position cannot be represented")
                            })
                        })
                        .collect::<Result<Vec<_>>>()?;
                    Ok((
                        name,
                        super::GitVisibilityClosure::from_positions(positions, objects.len())?,
                    ))
                })
                .collect::<Result<BTreeMap<_, _>>>()?;
            if covered.iter().any(|covered| !covered) {
                return Err(super::corrupt(
                    "visibility object dictionary contains an unreferenced object",
                ));
            }
            GitVisibilityIndex::from_parts(
                GIT_VISIBILITY_INDEX_VERSION,
                self.generation,
                self.pack_index_hash,
                self.git_validation_digest,
                objects,
                refs,
                BTreeMap::new(),
            )
        }
    }

    impl GitVisibilityIndexV4 {
        fn from_index(index: &GitVisibilityIndex) -> Result<Self> {
            let encoded = GitVisibilityIndexV3::from_index(index)?;
            let positions = encoded
                .objects
                .iter()
                .enumerate()
                .map(|(position, oid)| {
                    u32::try_from(position)
                        .map(|position| (oid.as_str(), position))
                        .map_err(|_| super::corrupt("visibility object dictionary is too large"))
                })
                .collect::<Result<BTreeMap<_, _>>>()?;
            let encode_transitions =
                |source: &BTreeMap<String, Vec<super::GitVisibilityTransition>>| {
                    source
                        .iter()
                        .map(|(name, transitions)| {
                            let transitions = transitions
                                .iter()
                                .map(|transition| {
                                    let mut objects =
                                        transition
                                            .objects
                                            .positions()
                                            .into_iter()
                                            .map(|position| {
                                                usize::try_from(position)
                                            .ok()
                                            .and_then(|position| index.objects.get(position))
                                            .map(super::encode_oid)
                                            .and_then(|oid| positions.get(oid.as_str()).copied())
                                            .ok_or_else(|| {
                                                super::corrupt(
                                                    "visibility transition object is absent",
                                                )
                                            })
                                            })
                                            .collect::<Result<Vec<_>>>()?;
                                    objects.sort_unstable();
                                    Ok(GitVisibilityTransitionV4 {
                                        from_oid: super::encode_oid(&transition.from_oid),
                                        to_oid: super::encode_oid(&transition.to_oid),
                                        objects: GitVisibilityClosureV3::from_positions(
                                            objects,
                                            encoded.objects.len(),
                                        )?,
                                    })
                                })
                                .collect::<Result<Vec<_>>>()?;
                            Ok((name.clone(), transitions))
                        })
                        .collect::<Result<_>>()
                };
            let transitions = encode_transitions(&index.transitions)?;
            let incremental_history = encode_transitions(&index.incremental_history)?;
            Ok(Self {
                version: GIT_VISIBILITY_INDEX_V4_VERSION,
                generation: encoded.generation,
                pack_index_hash: encoded.pack_index_hash,
                git_validation_digest: encoded.git_validation_digest,
                objects: encoded.objects,
                refs: encoded.refs,
                transitions,
                incremental_history,
            })
        }

        fn into_index(self) -> Result<GitVisibilityIndex> {
            if self.version != GIT_VISIBILITY_INDEX_V4_VERSION {
                return Err(super::corrupt(
                    "visibility index storage version is unsupported",
                ));
            }
            let object_count = self.objects.len();
            let decode_transitions = |source: BTreeMap<String, Vec<GitVisibilityTransitionV4>>| {
                source
                    .into_iter()
                    .map(|(name, transitions)| {
                        let transitions = transitions
                            .into_iter()
                            .map(|transition| {
                                Ok(super::GitVisibilityTransition {
                                    from_oid: super::decode_oid(&transition.from_oid)?,
                                    to_oid: super::decode_oid(&transition.to_oid)?,
                                    objects: super::GitVisibilityClosure::from_positions(
                                        transition.objects.into_positions(object_count)?,
                                        object_count,
                                    )?,
                                })
                            })
                            .collect::<Result<Vec<_>>>()?;
                        Ok((name, transitions))
                    })
                    .collect::<Result<_>>()
            };
            let transitions = decode_transitions(self.transitions)?;
            let incremental_history = decode_transitions(self.incremental_history)?;
            let mut index = GitVisibilityIndexV3 {
                version: GIT_VISIBILITY_INDEX_V3_VERSION,
                generation: self.generation,
                pack_index_hash: self.pack_index_hash,
                git_validation_digest: self.git_validation_digest,
                objects: self.objects,
                refs: self.refs,
            }
            .into_index()?;
            index.transitions = transitions;
            index.incremental_history = incremental_history;
            index.validate()?;
            Ok(index)
        }
    }

    #[cfg(feature = "remote-index")]
    impl GitVisibilityIndexV5 {
        fn validate_binding(
            &self,
            generation: u64,
            pack_index_hash: &str,
            git_validation_digest: &str,
        ) -> Result<crate::git_object_locator::GitObjectCatalogIdentity> {
            if self.version != GIT_VISIBILITY_INDEX_VERSION {
                return Err(super::corrupt(
                    "visibility index storage version is unsupported",
                ));
            }
            validate_hash(&self.pack_index_hash, "pack index hash")?;
            validate_hash(&self.git_validation_digest, "Git validation digest")?;
            validate_hash(&self.catalog_digest, "Git object catalog digest")?;
            if self.generation != generation
                || self.pack_index_hash != pack_index_hash
                || self.git_validation_digest != git_validation_digest
            {
                return Err(super::corrupt(
                    "visibility index does not match its immutable identity",
                ));
            }
            let object_count = usize::try_from(self.object_count)
                .map_err(|_| super::corrupt("Git object catalog count cannot be represented"))?;
            if self.object_count > MAX_GIT_VISIBILITY_OBJECTS
                || self.object_count > u64::from(u32::MAX)
            {
                return Err(super::corrupt("Git object catalog is too large"));
            }
            if self.refs.len() > MAX_GIT_VISIBILITY_REFS {
                return Err(super::corrupt("visibility index contains too many refs"));
            }
            let decode_closure = |closure: &GitVisibilityClosureV3| {
                let positions = closure.clone().into_positions(object_count)?;
                let normalized =
                    super::GitVisibilityClosure::from_positions(positions, object_count)?;
                normalized.validate(object_count)?;
                Ok::<_, crate::error::MetadataError>(normalized)
            };
            let refs = self
                .refs
                .iter()
                .map(|(name, closure)| {
                    super::validate_ref_name(name)?;
                    Ok((name, decode_closure(closure)?))
                })
                .collect::<Result<BTreeMap<_, _>>>()?;
            if self.transitions.len() > self.refs.len()
                || self
                    .transitions
                    .keys()
                    .any(|name| !self.refs.contains_key(name))
            {
                return Err(super::corrupt(
                    "visibility transitions do not match their refs",
                ));
            }
            for (name, transitions) in &self.transitions {
                if transitions.len() > super::MAX_VISIBILITY_TRANSITIONS_PER_REF {
                    return Err(super::corrupt(
                        "visibility transitions exceed the per-ref limit",
                    ));
                }
                let reference = refs
                    .get(name)
                    .ok_or_else(|| super::corrupt("visibility transition ref closure is absent"))?;
                for transition in transitions {
                    let from = transition.from_ordinal;
                    let to = transition.to_ordinal;
                    if u64::from(from) >= self.object_count
                        || u64::from(to) >= self.object_count
                        || !reference.contains(from)
                        || !reference.contains(to)
                    {
                        return Err(super::corrupt(
                            "visibility transition endpoint is outside its ref closure",
                        ));
                    }
                    let objects = decode_closure(&transition.objects)?;
                    if objects
                        .positions()
                        .into_iter()
                        .any(|position| !reference.contains(position))
                    {
                        return Err(super::corrupt(
                            "visibility transition objects are outside their ref closure",
                        ));
                    }
                }
            }
            if self.incremental_history.len() > self.refs.len()
                || self
                    .incremental_history
                    .keys()
                    .any(|name| !self.refs.contains_key(name))
            {
                return Err(super::corrupt(
                    "incremental visibility history does not match its refs",
                ));
            }
            for (name, transitions) in &self.incremental_history {
                if transitions.len() > super::MAX_VISIBILITY_HISTORY_TRANSITIONS_PER_REF {
                    return Err(super::corrupt(
                        "incremental visibility history exceeds the per-ref limit",
                    ));
                }
                let reference = refs.get(name).ok_or_else(|| {
                    super::corrupt("incremental visibility history ref closure is absent")
                })?;
                for transition in transitions {
                    let from = transition.from_ordinal;
                    let to = transition.to_ordinal;
                    if u64::from(from) >= self.object_count
                        || u64::from(to) >= self.object_count
                        || !reference.contains(from)
                        || !reference.contains(to)
                    {
                        return Err(super::corrupt(
                            "incremental visibility history endpoint is outside its ref closure",
                        ));
                    }
                    let objects = decode_closure(&transition.objects)?;
                    if objects
                        .positions()
                        .into_iter()
                        .any(|position| !reference.contains(position))
                    {
                        return Err(super::corrupt(
                            "incremental visibility history objects are outside its ref closure",
                        ));
                    }
                }
            }
            Ok(crate::git_object_locator::GitObjectCatalogIdentity {
                generation: self.generation,
                pack_index_hash: MerkleHash::from_hex(&self.pack_index_hash)
                    .map_err(|_| super::corrupt("invalid pack index hash"))?,
                object_count: self.object_count,
                catalog_digest: MerkleHash::from_hex(&self.catalog_digest)
                    .map_err(|_| super::corrupt("invalid catalog digest"))?,
            })
        }

        fn from_index(
            index: &GitVisibilityIndex,
            catalog: &[[u8; 20]],
            identity: crate::git_object_locator::GitObjectCatalogIdentity,
        ) -> Result<Self> {
            index.validate()?;
            if identity.generation != index.generation
                || identity.pack_index_hash.to_string() != index.pack_index_hash
                || identity.object_count != catalog.len() as u64
            {
                return Err(super::corrupt(
                    "visibility index does not match its Git object catalog",
                ));
            }
            let positions = catalog
                .iter()
                .copied()
                .enumerate()
                .map(|(position, oid)| {
                    u32::try_from(position)
                        .map(|position| (oid, position))
                        .map_err(|_| super::corrupt("Git object catalog is too large"))
                })
                .collect::<Result<std::collections::HashMap<_, _>>>()?;
            if positions.len() != catalog.len() {
                return Err(super::corrupt(
                    "Git object catalog contains duplicate object IDs",
                ));
            }
            let remap = |closure: &super::GitVisibilityClosure| {
                let mut ordinals = closure
                    .positions()
                    .into_iter()
                    .map(|position| {
                        usize::try_from(position)
                            .ok()
                            .and_then(|position| index.objects.get(position))
                            .and_then(|oid| positions.get(oid))
                            .copied()
                            .ok_or_else(|| {
                                super::corrupt(
                                    "visibility object is absent from its Git object catalog",
                                )
                            })
                    })
                    .collect::<Result<Vec<_>>>()?;
                ordinals.sort_unstable();
                GitVisibilityClosureV3::from_positions(ordinals, catalog.len())
            };
            let refs = index
                .refs
                .iter()
                .map(|(name, closure)| Ok((name.clone(), remap(closure)?)))
                .collect::<Result<_>>()?;
            let encode_transitions = |source: &BTreeMap<
                String,
                Vec<super::GitVisibilityTransition>,
            >| {
                source
                        .iter()
                        .map(|(name, transitions)| {
                            let transitions = transitions
                                .iter()
                                .map(|transition| {
                                    Ok(GitVisibilityTransitionV5 {
                                        from_ordinal: positions
                                            .get(&transition.from_oid)
                                            .copied()
                                            .ok_or_else(|| {
                                                super::corrupt(
                                                    "visibility transition base is absent from its catalog",
                                                )
                                            })?,
                                        to_ordinal: positions
                                            .get(&transition.to_oid)
                                            .copied()
                                            .ok_or_else(|| {
                                                super::corrupt(
                                                    "visibility transition target is absent from its catalog",
                                                )
                                            })?,
                                        objects: remap(&transition.objects)?,
                                    })
                                })
                                .collect::<Result<Vec<_>>>()?;
                            Ok((name.clone(), transitions))
                        })
                        .collect::<Result<_>>()
            };
            let transitions = encode_transitions(&index.transitions)?;
            let incremental_history = encode_transitions(&index.incremental_history)?;
            Ok(Self {
                version: GIT_VISIBILITY_INDEX_VERSION,
                generation: index.generation,
                pack_index_hash: index.pack_index_hash.clone(),
                git_validation_digest: index.git_validation_digest.clone(),
                catalog_digest: identity.catalog_digest.to_string(),
                object_count: identity.object_count,
                refs,
                transitions,
                incremental_history,
            })
        }

        fn into_index(self, catalog: Vec<[u8; 20]>) -> Result<GitVisibilityIndex> {
            if self.version != GIT_VISIBILITY_INDEX_VERSION {
                return Err(super::corrupt(
                    "visibility index storage version is unsupported",
                ));
            }
            validate_hash(&self.pack_index_hash, "pack index hash")?;
            validate_hash(&self.git_validation_digest, "Git validation digest")?;
            validate_hash(&self.catalog_digest, "Git object catalog digest")?;
            let object_count = usize::try_from(self.object_count)
                .map_err(|_| super::corrupt("Git object catalog count cannot be represented"))?;
            if object_count != catalog.len() || self.object_count > MAX_GIT_VISIBILITY_OBJECTS {
                return Err(super::corrupt(
                    "visibility index object count does not match its Git object catalog",
                ));
            }
            let refs = self
                .refs
                .into_iter()
                .map(|(name, closure)| {
                    Ok((
                        name,
                        super::GitVisibilityClosure::from_positions(
                            closure.into_positions(object_count)?,
                            object_count,
                        )?,
                    ))
                })
                .collect::<Result<_>>()?;
            let decode_transitions = |source: BTreeMap<String, Vec<GitVisibilityTransitionV5>>| {
                source
                    .into_iter()
                    .map(|(name, transitions)| {
                        let transitions = transitions
                            .into_iter()
                            .map(|transition| {
                                let from_oid = catalog
                                    .get(transition.from_ordinal as usize)
                                    .copied()
                                    .ok_or_else(|| {
                                        super::corrupt(
                                            "visibility transition base is outside its catalog",
                                        )
                                    })?;
                                let to_oid = catalog
                                    .get(transition.to_ordinal as usize)
                                    .copied()
                                    .ok_or_else(|| {
                                    super::corrupt(
                                        "visibility transition target is outside its catalog",
                                    )
                                })?;
                                Ok(super::GitVisibilityTransition {
                                    from_oid,
                                    to_oid,
                                    objects: super::GitVisibilityClosure::from_positions(
                                        transition.objects.into_positions(object_count)?,
                                        object_count,
                                    )?,
                                })
                            })
                            .collect::<Result<Vec<_>>>()?;
                        Ok((name, transitions))
                    })
                    .collect::<Result<_>>()
            };
            let transitions = decode_transitions(self.transitions)?;
            let incremental_history = decode_transitions(self.incremental_history)?;
            let mut index = GitVisibilityIndex::from_parts(
                GIT_VISIBILITY_INDEX_VERSION,
                self.generation,
                self.pack_index_hash,
                self.git_validation_digest,
                catalog,
                refs,
                transitions,
            )?;
            index.incremental_history = incremental_history;
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
        let (body, _) = store
            .get_with_etag_bounded(path, MAX_GIT_VISIBILITY_INDEX_BYTES)
            .await?;
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

    fn decode_digest_bound(
        body: &[u8],
        path: &ObjectPath,
    ) -> Result<(GitVisibilityIndex, GitVisibilityFormat)> {
        let version: GitVisibilityVersion = serde_json::from_slice(body).map_err(|error| {
            crate::error::MetadataError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!("invalid visibility index JSON: {error}"),
            }
        })?;
        match version.version {
            GIT_VISIBILITY_INDEX_V4_VERSION => {
                let index: GitVisibilityIndexV4 =
                    serde_json::from_slice(body).map_err(|error| {
                        crate::error::MetadataError::CorruptObject {
                            path: path.as_ref().to_owned(),
                            reason: format!("invalid visibility index JSON: {error}"),
                        }
                    })?;
                index
                    .into_index()
                    .map(|index| (index, GitVisibilityFormat::V4))
            }
            GIT_VISIBILITY_INDEX_V3_VERSION => {
                let index: GitVisibilityIndexV3 =
                    serde_json::from_slice(body).map_err(|error| {
                        crate::error::MetadataError::CorruptObject {
                            path: path.as_ref().to_owned(),
                            reason: format!("invalid visibility index JSON: {error}"),
                        }
                    })?;
                index
                    .into_index()
                    .map(|index| (index, GitVisibilityFormat::V3))
            }
            _ => Err(super::corrupt(
                "visibility index storage version is unsupported",
            )),
        }
    }

    #[cfg(feature = "remote-index")]
    async fn read_catalog_bound(
        store: &Store,
        router: &StoreLayout<Store>,
        generation: u64,
        pack_index_hash: &str,
        git_validation_digest: &str,
    ) -> Result<(GitVisibilityIndex, GitVisibilityFormat)> {
        let path = router.git_visibility_catalog_path(git_validation_digest);
        let body = read_bounded(store, &path).await?;
        let stored: GitVisibilityIndexV5 = serde_json::from_slice(&body).map_err(|error| {
            crate::error::MetadataError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!("invalid catalog visibility index JSON: {error}"),
            }
        })?;
        let identity =
            catalog_identity(&stored, generation, pack_index_hash, git_validation_digest)?;
        let session = crate::git_object_locator::GitObjectLocatorSession::open_for_catalog(
            Arc::clone(store.inner()),
            router.repo_prefix(),
            identity,
            Duration::from_secs(60 * 60),
        )
        .await?;
        let catalog = session.all_object_ids_and_close().await?;
        let index = stored.into_index(catalog)?;
        index.validate_identity(
            generation,
            &pack_index_hash.to_string(),
            git_validation_digest,
        )?;
        Ok((index, GitVisibilityFormat::V5))
    }

    #[cfg(feature = "remote-index")]
    fn catalog_identity(
        stored: &GitVisibilityIndexV5,
        generation: u64,
        pack_index_hash: &str,
        git_validation_digest: &str,
    ) -> Result<crate::git_object_locator::GitObjectCatalogIdentity> {
        stored.validate_binding(generation, pack_index_hash, git_validation_digest)
    }

    /// Check a catalog proof without materializing the complete ordinal list.
    ///
    /// Push and owner admission only need to know whether an immutable V5
    /// proof is already bound to the published catalog. Full ordinal
    /// materialization remains in `read_catalog_bound` for authorization and
    /// compaction paths that actually need object IDs.
    #[cfg(feature = "remote-index")]
    async fn catalog_bound_exists_for_identity(
        store: &Store,
        router: &StoreLayout<Store>,
        generation: u64,
        pack_index_hash: &str,
        git_validation_digest: &str,
    ) -> Result<bool> {
        let path = router.git_visibility_catalog_path(git_validation_digest);
        let body = match read_bounded(store, &path).await {
            Ok(body) => body,
            Err(crate::error::MetadataError::Storage {
                source: StorageError::NotFound { .. },
            }) => return Ok(false),
            Err(error) => return Err(error),
        };
        let stored: GitVisibilityIndexV5 = serde_json::from_slice(&body).map_err(|error| {
            crate::error::MetadataError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!("invalid catalog visibility index JSON: {error}"),
            }
        })?;
        let identity =
            catalog_identity(&stored, generation, pack_index_hash, git_validation_digest)?;
        let session = crate::git_object_locator::GitObjectLocatorSession::open_for_catalog(
            Arc::clone(store.inner()),
            router.repo_prefix(),
            identity,
            Duration::from_secs(60 * 60),
        )
        .await?;
        let matches = session.catalog_identity() == Some(identity);
        session.close().await?;
        Ok(matches)
    }

    #[cfg(feature = "remote-index")]
    async fn catalog_bound_exists(
        store: &Store,
        router: &StoreLayout<Store>,
        manifest: &Manifest,
    ) -> Result<bool> {
        catalog_bound_exists_for_identity(
            store,
            router,
            manifest.generation,
            &manifest.pack_index_hash,
            &manifest.git_validation_digest,
        )
        .await
    }

    /// Check whether the V5 visibility proof is bound to the published catalog.
    ///
    /// This validates the immutable proof and catalog checkpoint identity
    /// without materializing the catalog's complete object-ID dictionary.
    #[cfg(feature = "remote-index")]
    pub async fn catalog_bound_available(
        store: &Store,
        router: &StoreLayout<Store>,
        generation: u64,
        pack_index_hash: &str,
        git_validation_digest: &str,
    ) -> Result<bool> {
        catalog_bound_exists_for_identity(
            store,
            router,
            generation,
            pack_index_hash,
            git_validation_digest,
        )
        .await
    }

    async fn read_digest_bound(
        store: &Store,
        router: &StoreLayout<Store>,
        generation: u64,
        pack_index_hash: &str,
        git_validation_digest: &str,
    ) -> Result<(GitVisibilityIndex, GitVisibilityFormat)> {
        let path = router.git_visibility_path(git_validation_digest);
        let body = read_bounded(store, &path).await?;
        let (index, format) = decode_digest_bound(&body, &path)?;
        index.validate_identity(generation, pack_index_hash, git_validation_digest)?;
        Ok((index, format))
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
        )?;
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
        #[cfg(feature = "remote-index")]
        match read_catalog_bound(
            store,
            router,
            generation,
            pack_index_hash,
            git_validation_digest,
        )
        .await
        {
            Ok((index, format)) => return Ok(GitVisibilityRead { index, format }),
            Err(crate::error::MetadataError::Storage {
                source: StorageError::NotFound { .. },
            }) => {}
            Err(error) => return Err(error),
        }
        match read_digest_bound(
            store,
            router,
            generation,
            pack_index_hash,
            git_validation_digest,
        )
        .await
        {
            Ok((index, format)) => Ok(GitVisibilityRead { index, format }),
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
            Ok(read) => {
                let path = match read.format {
                    GitVisibilityFormat::V5 => {
                        router.git_visibility_catalog_path(&manifest.git_validation_digest)
                    }
                    GitVisibilityFormat::V4 | GitVisibilityFormat::V3 | GitVisibilityFormat::V1 => {
                        router.git_visibility_path(&manifest.git_validation_digest)
                    }
                };
                Err(crate::error::MetadataError::CorruptObject {
                    path: path.as_ref().to_owned(),
                    reason: "digest-bound visibility proof does not match its manifest".to_owned(),
                })
            }
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
        #[cfg(feature = "remote-index")]
        {
            let session = crate::git_object_locator::GitObjectLocatorSession::open(
                Arc::clone(store.inner()),
                router.repo_prefix(),
            )
            .await?;
            let identity = session.catalog_identity();
            let catalog_matches = identity.is_some_and(|identity| {
                identity.generation == index.generation
                    && identity.pack_index_hash.to_string() == index.pack_index_hash
            });
            if catalog_matches {
                let identity = identity.ok_or_else(|| {
                    crate::error::MetadataError::Internal(
                        "matching Git object catalog identity disappeared".to_owned(),
                    )
                })?;
                let catalog = session.all_object_ids_and_close().await?;
                let stored = GitVisibilityIndexV5::from_index(index, &catalog, identity)?;
                let body = serde_json::to_vec(&stored).map_err(|error| {
                    crate::error::MetadataError::Internal(format!(
                        "catalog visibility index serialize: {error}"
                    ))
                })?;
                let path = router.git_visibility_catalog_path(&index.git_validation_digest);
                if body.len() as u64 > MAX_GIT_VISIBILITY_INDEX_BYTES {
                    return Err(crate::error::MetadataError::CorruptObject {
                        path: path.as_ref().to_owned(),
                        reason: format!(
                            "visibility index exceeds {} bytes",
                            MAX_GIT_VISIBILITY_INDEX_BYTES
                        ),
                    });
                }
                return upload_visibility_body(store, &path, Bytes::from(body)).await;
            }
            session.close().await?;
        }
        let stored = GitVisibilityIndexV4::from_index(index)?;
        let body = serde_json::to_vec(&stored).map_err(|error| {
            crate::error::MetadataError::Internal(format!("visibility index serialize: {error}"))
        })?;
        let path = router.git_visibility_path(&index.git_validation_digest);
        if body.len() as u64 > MAX_GIT_VISIBILITY_INDEX_BYTES {
            return Err(crate::error::MetadataError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!(
                    "visibility index exceeds {} bytes",
                    MAX_GIT_VISIBILITY_INDEX_BYTES
                ),
            });
        }
        upload_visibility_body(store, &path, Bytes::from(body)).await
    }

    /// Ensure a manifest's proof is bound to its exact published object catalog.
    pub async fn ensure_catalog_bound(
        store: &Store,
        router: &StoreLayout<Store>,
        manifest: &Manifest,
    ) -> Result<bool> {
        #[cfg(feature = "remote-index")]
        {
            if catalog_bound_exists(store, router, manifest).await? {
                return Ok(true);
            }
            let Some(read) = read_for_manifest(store, router, manifest).await? else {
                return Ok(false);
            };
            if read.format == GitVisibilityFormat::V5 {
                return Ok(true);
            }
            upload_if_absent(store, router, &read.index).await?;
            return Ok(read_for_manifest(store, router, manifest)
                .await?
                .is_some_and(|read| read.format == GitVisibilityFormat::V5));
        }
        #[cfg(not(feature = "remote-index"))]
        {
            let _ = (store, router, manifest);
            Ok(false)
        }
    }

    async fn upload_visibility_body(store: &Store, path: &ObjectPath, body: Bytes) -> Result<()> {
        match store.put(path, body.clone()).await {
            Ok(()) => Ok(()),
            Err(StorageError::StateConflict { .. }) => {
                let metadata = store.head(path).await?;
                if metadata.size > MAX_GIT_VISIBILITY_INDEX_BYTES {
                    return Err(crate::error::MetadataError::CorruptObject {
                        path: path.as_ref().to_owned(),
                        reason: format!(
                            "existing visibility index is {} bytes; maximum is {}",
                            metadata.size, MAX_GIT_VISIBILITY_INDEX_BYTES
                        ),
                    });
                }
                let (existing, _) = store
                    .get_with_etag_bounded(&path, MAX_GIT_VISIBILITY_INDEX_BYTES)
                    .await?;
                if existing.len() as u64 > MAX_GIT_VISIBILITY_INDEX_BYTES {
                    return Err(crate::error::MetadataError::CorruptObject {
                        path: path.as_ref().to_owned(),
                        reason: format!(
                            "existing visibility index exceeds {} bytes",
                            MAX_GIT_VISIBILITY_INDEX_BYTES
                        ),
                    });
                }
                if existing == body {
                    return Ok(());
                }
                Err(crate::error::MetadataError::CorruptObject {
                    path: path.as_ref().to_owned(),
                    reason: "immutable visibility index conflicts with the requested proof"
                        .to_owned(),
                })
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
        let (body, _) = store
            .get_with_etag_bounded(&path, MAX_GIT_VISIBILITY_INDEX_BYTES)
            .await?;
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
        let mut index = if base.refs.is_empty() {
            GitVisibilityIndex::new(
                base.generation,
                base.pack_index_hash.clone(),
                base.git_validation_digest.clone(),
                BTreeMap::new(),
            )?
        } else {
            match read_for_manifest(store, router, base).await? {
                Some(read) => {
                    let index = read.index;
                    if read.format == GitVisibilityFormat::V1 {
                        upload_if_absent(store, router, &index).await?;
                    }
                    index
                }
                None => return Ok(None),
            }
        };

        for edit in edits {
            match &edit.new_oid {
                None => {
                    index.remove_ref(&edit.ref_name);
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
                    index.apply_edit(edit.ref_name.clone(), &evidence)?;
                }
            }
        }

        if index.refs.keys().ne(final_refs.keys()) {
            return Err(crate::error::MetadataError::CorruptObject {
                path: "git-visibility-index".to_owned(),
                reason: "compacted visibility refs do not match the manifest".to_owned(),
            });
        }
        for (name, tip) in final_refs {
            let tip = super::decode_oid(tip)?;
            if !index.contains_in_ref(name, &tip) {
                return Err(crate::error::MetadataError::CorruptObject {
                    path: "git-visibility-index".to_owned(),
                    reason: format!("compacted visibility proof does not contain ref tip {name}"),
                });
            }
        }
        index.bind_identity(generation, pack_index_hash, git_validation_digest)?;
        Ok(Some(index))
    }
}

#[cfg(feature = "storage")]
pub use storage::{
    GitVisibilityFormat, GitVisibilityRead, compact_journal_edits, ensure_catalog_bound, read,
    read_edit, read_for_manifest, read_with_format, upload_edit, upload_if_absent,
};

#[cfg(all(feature = "remote-index", feature = "storage"))]
pub use storage::catalog_bound_available;

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn bitmap_queries_match_sorted_closure_sets(
            closures in prop::collection::vec(prop::collection::vec(0_u8..64, 0..64), 1..8),
            selected in prop::collection::vec(0_usize..16, 0..16),
            query in 0_u8..64,
        ) {
            let refs = closures
                .iter()
                .enumerate()
                .map(|(index, objects)| {
                    (
                        format!("refs/heads/{index}"),
                        objects.iter().map(|object| format!("{object:040x}")).collect(),
                    )
                })
                .collect::<BTreeMap<_, _>>();
            let index = GitVisibilityIndex::new(
                4,
                "a".repeat(64),
                "b".repeat(64),
                refs,
            )
            .expect("generated visibility index is valid");
            let selected = selected
                .into_iter()
                .map(|value| format!("refs/heads/{}", value % closures.len()))
                .collect::<BTreeSet<_>>();
            let expected = selected
                .iter()
                .filter_map(|name| name.rsplit('/').next())
                .filter_map(|index| index.parse::<usize>().ok())
                .flat_map(|index| closures[index].iter().copied())
                .collect::<BTreeSet<_>>();
            let selected_names = selected.iter().map(String::as_str).collect::<Vec<_>>();
            let actual = index
                .objects_for_refs(selected_names.iter().copied())
                .into_iter()
                .map(|oid| oid[19])
                .collect::<BTreeSet<_>>();
            let query_oid = decode_oid(&format!("{query:040x}"))
                .expect("generated OID is valid");

            prop_assert_eq!(actual, expected.clone());
            prop_assert_eq!(
                index.object_count_for_refs(selected_names.iter().copied()),
                expected.len(),
            );
            prop_assert_eq!(
                index.contains_for_refs(selected_names.iter().copied(), &query_oid),
                expected.contains(&query),
            );
            prop_assert_eq!(
                index.membership_count(),
                closures
                    .iter()
                    .map(|closure| closure.iter().collect::<BTreeSet<_>>().len() as u64)
                    .sum::<u64>(),
            );
        }

        #[test]
        fn visibility_edits_match_set_difference(
            old_tail in prop::collection::vec(2_u8..64, 0..64),
            new_tail in prop::collection::vec(2_u8..64, 0..64),
        ) {
            let old = old_tail
                .into_iter()
                .chain([0])
                .map(|object| format!("{object:040x}"))
                .collect::<BTreeSet<_>>();
            let new = new_tail
                .into_iter()
                .chain([1])
                .map(|object| format!("{object:040x}"))
                .collect::<BTreeSet<_>>();
            let edit = GitVisibilityEdit::delta(
                Some(format!("{:040x}", 0)),
                format!("{:040x}", 1),
                &old,
                &new,
            );

            prop_assert_eq!(
                edit.apply(Some(&old.into_iter().collect::<Vec<_>>()))
                    .expect("generated edit is valid"),
                new.into_iter().collect::<Vec<_>>(),
            );
        }
    }

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
        )
        .expect("valid visibility index");

        assert!(index.validate().is_ok());
        assert!(index.contains_hex_in_ref("refs/heads/main", &"a".repeat(40)));
        assert_eq!(index.objects_for_refs(["refs/heads/main"]).len(), 2);
        assert_eq!(
            index.object_count_for_refs(["refs/heads/main", "refs/heads/main"]),
            2
        );
    }

    #[test]
    fn authorization_digest_tracks_object_union_without_ref_names() {
        let index = GitVisibilityIndex::new(
            4,
            "a".repeat(64),
            "c".repeat(64),
            BTreeMap::from([
                ("refs/heads/main".to_owned(), vec!["1".repeat(40)]),
                ("refs/heads/alias".to_owned(), vec!["1".repeat(40)]),
                ("refs/heads/private".to_owned(), vec!["2".repeat(40)]),
            ]),
        )
        .expect("valid visibility index");

        assert_eq!(
            index.authorization_digest_for_refs(["refs/heads/main"]),
            index.authorization_digest_for_refs(["refs/heads/alias"])
        );
        assert_ne!(
            index.authorization_digest_for_refs(["refs/heads/main"]),
            index.authorization_digest_for_refs(["refs/heads/main", "refs/heads/private"])
        );
    }

    #[test]
    fn rejects_stale_or_malformed_proof() {
        let index = GitVisibilityIndex::new(
            4,
            "a".repeat(64),
            "c".repeat(64),
            BTreeMap::from([("refs/heads/main".to_owned(), vec!["A".repeat(40)])]),
        );
        assert!(index.is_err());
    }

    #[test]
    fn rejects_proofs_with_too_many_refs_before_authorization() {
        let refs = (0..=MAX_GIT_VISIBILITY_REFS)
            .map(|index| (format!("refs/heads/{index}"), Vec::new()))
            .collect();
        assert!(GitVisibilityIndex::new(4, "a".repeat(64), "c".repeat(64), refs).is_err());
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
        let index = GitVisibilityIndex::new(
            manifest.generation,
            &manifest.pack_index_hash,
            &manifest.git_validation_digest,
            BTreeMap::from([(
                "refs/tags/release".to_owned(),
                vec!["1".repeat(40), "2".repeat(40)],
            )]),
        )
        .expect("valid visibility index");

        assert!(index.matches_manifest(&manifest));
        let missing_peeled = GitVisibilityIndex::new(
            manifest.generation,
            &manifest.pack_index_hash,
            &manifest.git_validation_digest,
            BTreeMap::from([("refs/tags/release".to_owned(), vec!["1".repeat(40)])]),
        )
        .expect("valid incomplete proof");
        assert!(!missing_peeled.matches_manifest(&manifest));
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

    #[test]
    fn ref_edit_normalizes_exact_delta_objects() {
        let edit = GitVisibilityEdit::from_delta_objects(
            Some("1".repeat(40)),
            "2".repeat(40),
            vec!["4".repeat(40), "3".repeat(40), "4".repeat(40)],
            vec!["1".repeat(40), "1".repeat(40)],
        );

        assert_eq!(edit.added, ["3".repeat(40), "4".repeat(40)]);
        assert_eq!(edit.removed, ["1".repeat(40)]);
        assert!(edit.validate().is_ok());
    }

    #[test]
    fn fast_forward_transitions_compose_and_rewrites_invalidate_them() {
        let mut index = GitVisibilityIndex::new(
            4,
            "a".repeat(64),
            "b".repeat(64),
            BTreeMap::from([(
                "refs/heads/main".to_owned(),
                vec!["1".repeat(40), "2".repeat(40)],
            )]),
        )
        .expect("valid visibility index");
        let first = GitVisibilityEdit::delta(
            Some("1".repeat(40)),
            "3".repeat(40),
            &BTreeSet::from(["1".repeat(40), "2".repeat(40)]),
            &BTreeSet::from([
                "1".repeat(40),
                "2".repeat(40),
                "3".repeat(40),
                "4".repeat(40),
            ]),
        );
        index
            .apply_edit("refs/heads/main".to_owned(), &first)
            .expect("first fast-forward");
        let second = GitVisibilityEdit::delta(
            Some("3".repeat(40)),
            "5".repeat(40),
            &BTreeSet::from([
                "1".repeat(40),
                "2".repeat(40),
                "3".repeat(40),
                "4".repeat(40),
            ]),
            &BTreeSet::from([
                "1".repeat(40),
                "2".repeat(40),
                "3".repeat(40),
                "4".repeat(40),
                "5".repeat(40),
            ]),
        );
        index
            .apply_edit("refs/heads/main".to_owned(), &second)
            .expect("second fast-forward");
        let first_tip = decode_oid(&"1".repeat(40)).expect("valid OID");
        let second_tip = decode_oid(&"3".repeat(40)).expect("valid OID");
        let current_tip = decode_oid(&"5".repeat(40)).expect("valid OID");

        assert_eq!(
            index.incremental_objects("refs/heads/main", &current_tip, &[first_tip]),
            Some(vec![
                decode_oid(&"3".repeat(40)).expect("valid OID"),
                decode_oid(&"4".repeat(40)).expect("valid OID"),
                current_tip,
            ])
        );
        assert_eq!(
            index.incremental_objects("refs/heads/main", &current_tip, &[second_tip]),
            Some(vec![current_tip])
        );

        let rewrite = GitVisibilityEdit::replacement(
            Some("5".repeat(40)),
            "6".repeat(40),
            &BTreeSet::from(["6".repeat(40)]),
        );
        index
            .apply_edit("refs/heads/main".to_owned(), &rewrite)
            .expect("replacement update");
        let rewritten_tip = decode_oid(&"6".repeat(40)).expect("valid OID");

        assert_eq!(
            index.incremental_objects("refs/heads/main", &rewritten_tip, &[first_tip]),
            None
        );
    }

    #[test]
    fn long_fast_forward_history_keeps_exact_incremental_closures() {
        let oid = |value: usize| format!("{value:040x}");
        let mut index = GitVisibilityIndex::new(
            4,
            "a".repeat(64),
            "b".repeat(64),
            BTreeMap::from([("refs/heads/main".to_owned(), vec![oid(0)])]),
        )
        .expect("valid visibility index");
        let mut prior = BTreeSet::from([oid(0)]);

        for value in 1..=100 {
            let new_oid = oid(value);
            let mut next = prior.clone();
            next.insert(new_oid.clone());
            let edit = GitVisibilityEdit::delta(Some(oid(value - 1)), new_oid, &prior, &next);
            index
                .apply_edit("refs/heads/main".to_owned(), &edit)
                .expect("fast-forward visibility edit");
            prior = next;
        }

        let tip = decode_oid(&oid(100)).expect("valid tip OID");
        let from_start = decode_oid(&oid(0)).expect("valid base OID");
        let from_checkpoint = decode_oid(&oid(10)).expect("valid checkpoint OID");
        let expected = (1..=100)
            .map(|value| decode_oid(&oid(value)).expect("valid expected OID"))
            .collect::<Vec<_>>();
        let expected_after_checkpoint = (11..=100)
            .map(|value| decode_oid(&oid(value)).expect("valid expected OID"))
            .collect::<Vec<_>>();

        assert_eq!(
            index.incremental_objects("refs/heads/main", &tip, &[from_start]),
            Some(expected)
        );
        assert_eq!(
            index.incremental_objects("refs/heads/main", &tip, &[from_checkpoint]),
            Some(expected_after_checkpoint)
        );
        index
            .validate()
            .expect("long visibility history remains valid");
    }

    #[test]
    fn fast_forward_delta_positions_are_catalog_sorted() {
        let oid = |value: usize| format!("{value:040x}");
        let mut index = GitVisibilityIndex::new(
            4,
            "a".repeat(64),
            "b".repeat(64),
            BTreeMap::from([
                ("refs/heads/main".to_owned(), vec![oid(0)]),
                ("refs/heads/other".to_owned(), vec![oid(2), oid(4)]),
            ]),
        )
        .expect("valid visibility index");
        let old = BTreeSet::from([oid(0)]);
        let new = BTreeSet::from([oid(0), oid(1), oid(2), oid(3)]);
        let edit = GitVisibilityEdit::delta(Some(oid(0)), oid(3), &old, &new);

        index
            .apply_edit("refs/heads/main".to_owned(), &edit)
            .expect("fast-forward visibility edit");

        index
            .validate()
            .expect("catalog-ordered transition positions remain valid");
        let from = decode_oid(&oid(0)).expect("valid base OID");
        let tip = decode_oid(&oid(3)).expect("valid tip OID");
        let mut actual = index
            .incremental_objects("refs/heads/main", &tip, &[from])
            .expect("fast-forward transition");
        actual.sort_unstable();
        let expected = [oid(1), oid(2), oid(3)]
            .into_iter()
            .map(|value| decode_oid(&value).expect("valid expected OID"))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    #[cfg(feature = "storage")]
    #[tokio::test]
    async fn stored_long_fast_forward_history_keeps_incremental_fetch_exact() {
        use std::sync::Arc;

        use crab_storage::{Store, StoreLayout};
        use object_store::memory::InMemory;

        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        let oid = |value: usize| format!("{value:040x}");
        let mut index = GitVisibilityIndex::new(
            4,
            "a".repeat(64),
            "b".repeat(64),
            BTreeMap::from([("refs/heads/main".to_owned(), vec![oid(0)])]),
        )
        .expect("valid visibility index");
        let mut prior = BTreeSet::from([oid(0)]);

        for value in 1..=100 {
            let new_oid = oid(value);
            let mut next = prior.clone();
            next.insert(new_oid.clone());
            let edit = GitVisibilityEdit::delta(Some(oid(value - 1)), new_oid, &prior, &next);
            index
                .apply_edit("refs/heads/main".to_owned(), &edit)
                .expect("fast-forward visibility edit");
            prior = next;
        }
        upload_if_absent(&store, &router, &index)
            .await
            .expect("upload visibility history");

        let stored = read_with_format(
            &store,
            &router,
            index.generation,
            &index.pack_index_hash,
            &index.git_validation_digest,
        )
        .await
        .expect("read visibility history");
        assert_eq!(stored.format, GitVisibilityFormat::V4);
        let tip = decode_oid(&oid(100)).expect("valid tip OID");
        let from = decode_oid(&oid(10)).expect("valid checkpoint OID");
        assert_eq!(
            stored
                .index
                .incremental_objects("refs/heads/main", &tip, &[from])
                .map(|mut objects| {
                    objects.sort_unstable();
                    objects
                }),
            Some(
                (11..=100)
                    .map(|value| decode_oid(&oid(value)).expect("valid expected OID"))
                    .collect()
            )
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
        )
        .expect("valid left visibility index");
        let right = GitVisibilityIndex::new(
            7,
            &pack_hash,
            "c".repeat(64),
            BTreeMap::from([("refs/heads/right".to_owned(), vec!["2".repeat(40)])]),
        )
        .expect("valid right visibility index");

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

    #[cfg(all(feature = "storage", feature = "remote-index"))]
    #[tokio::test]
    async fn legacy_dictionary_proof_migrates_to_exact_catalog_ordinals() {
        use std::sync::Arc;

        use crab_storage::{Store, StoreLayout};
        use crab_xet::hash::MerkleHash;
        use object_store::memory::InMemory;

        use crate::git_object_locator::{
            GitLocatorCoverage, GitObjectLocation, GitObjectLocatorEntry, GitObjectLocatorWriter,
            GitPackLocatorRecord,
        };

        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        let pack_hash = "a".repeat(64);
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 7;
        manifest.pack_index_hash.clone_from(&pack_hash);
        manifest
            .refs
            .insert("refs/heads/main".to_owned(), "1".repeat(40));
        manifest.seal_git_validation();
        let digest = manifest.git_validation_digest.clone();
        let index = GitVisibilityIndex::new(
            7,
            &pack_hash,
            &digest,
            BTreeMap::from([(
                "refs/heads/main".to_owned(),
                vec!["1".repeat(40), "2".repeat(40)],
            )]),
        )
        .expect("visibility index");

        upload_if_absent(&store, &router, &index)
            .await
            .expect("upload dictionary proof");
        assert_eq!(
            read_with_format(&store, &router, 7, &pack_hash, &digest)
                .await
                .expect("read dictionary proof")
                .format,
            GitVisibilityFormat::V4
        );

        let pack_index_hash = MerkleHash::from_hex(&pack_hash).expect("pack hash");
        let mut writer =
            GitObjectLocatorWriter::open(Arc::clone(store.inner()), router.repo_prefix())
                .await
                .expect("open catalog writer");
        let binding = writer
            .bind_packs(&[GitPackLocatorRecord {
                pack_id: MerkleHash::from([3; 32]),
                committed_generation: 7,
                pack_index_hash,
                object_count: 2,
                pack_size: 256,
            }])
            .await
            .expect("bind pack")[0];
        writer
            .write_locations(
                binding,
                &[
                    GitObjectLocatorEntry {
                        oid: [0x22; 20],
                        location: GitObjectLocation {
                            pack_offset: 12,
                            entry_len: 64,
                            crc32: 1,
                        },
                        metadata: Default::default(),
                    },
                    GitObjectLocatorEntry {
                        oid: [0x11; 20],
                        location: GitObjectLocation {
                            pack_offset: 76,
                            entry_len: 64,
                            crc32: 2,
                        },
                        metadata: Default::default(),
                    },
                ],
            )
            .await
            .expect("write catalog objects");
        writer
            .set_coverage(GitLocatorCoverage {
                generation: 7,
                pack_index_hash,
            })
            .await
            .expect("publish catalog coverage");
        writer.close().await.expect("close catalog writer");

        assert!(
            ensure_catalog_bound(&store, &router, &manifest)
                .await
                .expect("migrate catalog proof")
        );
        assert!(
            catalog_bound_available(
                &store,
                &router,
                manifest.generation,
                &manifest.pack_index_hash,
                &manifest.git_validation_digest,
            )
            .await
            .expect("check catalog proof")
        );
        assert!(
            catalog_bound_available(
                &store,
                &router,
                manifest.generation.saturating_add(1),
                &manifest.pack_index_hash,
                &manifest.git_validation_digest,
            )
            .await
            .is_err()
        );
        let migrated = read_with_format(&store, &router, 7, &pack_hash, &digest)
            .await
            .expect("read catalog proof");
        assert_eq!(migrated.format, GitVisibilityFormat::V5);
        assert_eq!(
            migrated.index.objects_for_ref("refs/heads/main"),
            index.objects_for_ref("refs/heads/main")
        );
        assert!(migrated.index.matches_manifest(&manifest));

        let mut history_index = index.clone();
        let closure = BTreeSet::from(["1".repeat(40), "2".repeat(40)]);
        let edit =
            GitVisibilityEdit::delta(Some("1".repeat(40)), "2".repeat(40), &closure, &closure);
        history_index
            .apply_edit("refs/heads/main".to_owned(), &edit)
            .expect("append catalog-bound visibility history");
        let mut history_manifest = manifest.clone();
        history_manifest
            .refs
            .insert("refs/heads/main".to_owned(), "2".repeat(40));
        history_manifest.seal_git_validation();
        history_index
            .bind_identity(
                history_manifest.generation,
                &history_manifest.pack_index_hash,
                &history_manifest.git_validation_digest,
            )
            .expect("bind catalog-bound visibility history");
        upload_if_absent(&store, &router, &history_index)
            .await
            .expect("upload catalog-bound visibility history");
        let stored_history = read_with_format(
            &store,
            &router,
            history_manifest.generation,
            &history_manifest.pack_index_hash,
            &history_manifest.git_validation_digest,
        )
        .await
        .expect("read catalog-bound visibility history");
        assert_eq!(stored_history.format, GitVisibilityFormat::V5);
        let from = decode_oid(&"1".repeat(40)).expect("valid history base OID");
        let to = decode_oid(&"2".repeat(40)).expect("valid history target OID");
        assert_eq!(
            stored_history
                .index
                .incremental_objects("refs/heads/main", &to, &[from]),
            Some(Vec::new())
        );
    }

    #[cfg(feature = "storage")]
    #[tokio::test]
    async fn shipped_v1_proof_is_read_and_can_be_backfilled_to_current_format() {
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
        assert_eq!(current.format, GitVisibilityFormat::V4);
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
        )
        .expect("valid current visibility index");
        upload_if_absent(&store, &router, &current).await.unwrap();
        let read = read_for_manifest(&store, &router, &manifest)
            .await
            .unwrap()
            .expect("digest-bound proof should supersede the orphaned v1 candidate");
        assert_eq!(read.format, GitVisibilityFormat::V4);
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
        let expanded_refs = refs.clone();
        let index = GitVisibilityIndex::new(7, "a".repeat(64), "b".repeat(64), refs)
            .expect("valid visibility index");
        let expanded = serde_json::to_vec(&serde_json::json!({
            "version": index.version,
            "generation": index.generation,
            "pack_index_hash": &index.pack_index_hash,
            "git_validation_digest": &index.git_validation_digest,
            "refs": expanded_refs,
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
    async fn stored_v3_proof_reads_into_the_binary_runtime_model() {
        use std::sync::Arc;

        use bytes::Bytes;
        use crab_storage::{Store, StoreLayout};
        use object_store::memory::InMemory;

        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        let pack_hash = "a".repeat(64);
        let digest = "b".repeat(64);
        let stored = serde_json::json!({
            "version": 3,
            "generation": 7,
            "pack_index_hash": pack_hash.clone(),
            "git_validation_digest": digest.clone(),
            "objects": ["1".repeat(40), "2".repeat(40)],
            "refs": {"refs/heads/main": {"sparse": [0, 1]}},
        });
        store
            .put(
                &router.git_visibility_path(&digest),
                Bytes::from(serde_json::to_vec(&stored).unwrap()),
            )
            .await
            .unwrap();

        let read = read_with_format(&store, &router, 7, &pack_hash, &digest)
            .await
            .unwrap();

        assert_eq!(read.format, GitVisibilityFormat::V3);
        assert_eq!(read.index.object_count_for_refs(["refs/heads/main"]), 2);
        assert!(
            read.index
                .contains_hex_in_ref("refs/heads/main", &"2".repeat(40))
        );
    }

    #[cfg(feature = "storage")]
    #[tokio::test]
    async fn stored_proofs_reject_dictionary_order_and_identity_mismatches() {
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
            "objects": ["2".repeat(40), "1".repeat(40)],
            "refs": {"refs/heads/main": {"sparse": [0, 1]}},
        });
        store
            .put(
                &router.git_visibility_path(&digest),
                Bytes::from(serde_json::to_vec(&malformed).unwrap()),
            )
            .await
            .unwrap();

        assert!(read(&store, &router, 7, &pack_hash, &digest).await.is_err());

        let valid_digest = "c".repeat(64);
        let valid = GitVisibilityIndex::new(
            7,
            &pack_hash,
            &valid_digest,
            BTreeMap::from([("refs/heads/main".to_owned(), vec!["1".repeat(40)])]),
        )
        .expect("valid visibility index");
        upload_if_absent(&store, &router, &valid).await.unwrap();

        assert!(
            read(&store, &router, 8, &pack_hash, &valid_digest)
                .await
                .is_err()
        );
        assert!(
            read(&store, &router, 7, &"d".repeat(64), &valid_digest)
                .await
                .is_err()
        );
    }

    #[cfg(feature = "storage")]
    #[tokio::test]
    async fn current_storage_rejects_invalid_transitions() {
        use std::sync::Arc;

        use bytes::Bytes;
        use crab_storage::{Store, StoreLayout};
        use object_store::memory::InMemory;

        let cases = [
            serde_json::json!({
                "version": 4,
                "generation": 7,
                "pack_index_hash": "a".repeat(64),
                "git_validation_digest": "b".repeat(64),
                "objects": ["1".repeat(40)],
                "refs": {"refs/heads/main": {"sparse": [0]}},
                "transitions": {
                    "refs/heads/absent": [{
                        "from_oid": "1".repeat(40),
                        "to_oid": "1".repeat(40),
                        "objects": {"sparse": [0]},
                    }],
                },
            }),
            serde_json::json!({
                "version": 4,
                "generation": 7,
                "pack_index_hash": "a".repeat(64),
                "git_validation_digest": "b".repeat(64),
                "objects": ["1".repeat(40), "2".repeat(40)],
                "refs": {
                    "refs/heads/main": {"sparse": [0]},
                    "refs/heads/hidden": {"sparse": [1]},
                },
                "transitions": {
                    "refs/heads/main": [{
                        "from_oid": "1".repeat(40),
                        "to_oid": "1".repeat(40),
                        "objects": {"sparse": [1]},
                    }],
                },
            }),
            serde_json::json!({
                "version": 4,
                "generation": 7,
                "pack_index_hash": "a".repeat(64),
                "git_validation_digest": "b".repeat(64),
                "objects": ["1".repeat(40)],
                "refs": {"refs/heads/main": {"sparse": [0]}},
                "transitions": {
                    "refs/heads/main": [{
                        "from_oid": "1".repeat(40),
                        "to_oid": "1".repeat(40),
                        "objects": {"bitmap": "AAA"},
                    }],
                },
            }),
        ];

        for body in cases {
            let store = Store::new(Arc::new(InMemory::new()));
            let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
            let digest = "b".repeat(64);
            store
                .put(
                    &router.git_visibility_path(&digest),
                    Bytes::from(serde_json::to_vec(&body).unwrap()),
                )
                .await
                .unwrap();

            assert!(
                read(&store, &router, 7, &"a".repeat(64), &digest)
                    .await
                    .is_err()
            );
        }
    }

    #[cfg(feature = "storage")]
    #[tokio::test]
    async fn journal_fast_forward_retains_the_exact_incremental_closure() {
        use std::sync::Arc;

        use crab_storage::{Store, StoreLayout};
        use object_store::memory::InMemory;

        use crate::ref_journal::RefJournalEdit;

        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        let mut base = Manifest::default_for_repo("refs/heads/main");
        base.generation = 4;
        base.pack_index_hash = "a".repeat(64);
        base.refs
            .insert("refs/heads/main".to_owned(), "1".repeat(40));
        base.seal_git_validation();
        let base_index = GitVisibilityIndex::new(
            base.generation,
            &base.pack_index_hash,
            &base.git_validation_digest,
            BTreeMap::from([(
                "refs/heads/main".to_owned(),
                vec!["1".repeat(40), "2".repeat(40)],
            )]),
        )
        .unwrap();
        upload_if_absent(&store, &router, &base_index)
            .await
            .unwrap();

        let old = BTreeSet::from(["1".repeat(40), "2".repeat(40)]);
        let new = BTreeSet::from([
            "1".repeat(40),
            "2".repeat(40),
            "3".repeat(40),
            "4".repeat(40),
        ]);
        let evidence = GitVisibilityEdit::delta(Some("1".repeat(40)), "3".repeat(40), &old, &new);
        let evidence_hash = upload_edit(&store, &router, &evidence).await.unwrap();
        let edits = [RefJournalEdit {
            ref_name: "refs/heads/main".to_owned(),
            old_oid: Some("1".repeat(40)),
            new_oid: Some("3".repeat(40)),
            peeled_oid: None,
            lock_holder: None,
            visibility_evidence_hash: Some(evidence_hash),
        }];
        let final_refs = BTreeMap::from([("refs/heads/main".to_owned(), "3".repeat(40))]);
        let mut final_manifest = base.clone();
        final_manifest.generation = 5;
        final_manifest.pack_index_hash = "c".repeat(64);
        final_manifest.refs.clone_from(&final_refs);
        final_manifest.seal_git_validation();

        let compacted = compact_journal_edits(
            &store,
            &router,
            &base,
            &edits,
            final_manifest.generation,
            &final_manifest.pack_index_hash,
            &final_manifest.git_validation_digest,
            &final_refs,
        )
        .await
        .unwrap()
        .expect("complete edit evidence");
        upload_if_absent(&store, &router, &compacted).await.unwrap();
        let current = read_with_format(
            &store,
            &router,
            final_manifest.generation,
            &final_manifest.pack_index_hash,
            &final_manifest.git_validation_digest,
        )
        .await
        .unwrap();
        let from = decode_oid(&"1".repeat(40)).unwrap();
        let to = decode_oid(&"3".repeat(40)).unwrap();

        assert_eq!(current.format, GitVisibilityFormat::V4);
        assert_eq!(
            current
                .index
                .incremental_objects("refs/heads/main", &to, &[from]),
            Some(vec![
                decode_oid(&"3".repeat(40)).unwrap(),
                decode_oid(&"4".repeat(40)).unwrap(),
            ])
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
        )
        .expect("valid base visibility index");
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
            compacted.objects_for_ref("refs/heads/left"),
            Some(vec!["1".repeat(40), "3".repeat(40), "c".repeat(40)])
        );
        assert_eq!(
            compacted.objects_for_ref("refs/heads/right"),
            Some(vec!["2".repeat(40), "4".repeat(40), "d".repeat(40)])
        );
    }
}
