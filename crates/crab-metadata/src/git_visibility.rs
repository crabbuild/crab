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
pub const GIT_VISIBILITY_INDEX_VERSION: u32 = 1;

/// Maximum serialized proof accepted from object storage.
pub const MAX_GIT_VISIBILITY_INDEX_BYTES: u64 = 128 * 1024 * 1024;

const MAX_GIT_VISIBILITY_REFS: usize = 100_000;
/// Maximum number of distinct Git objects in one visibility proof dictionary.
pub const MAX_GIT_VISIBILITY_OBJECTS: u64 = 10_000_000;

/// Maximum number of distinct Git objects built synchronously on a push or repair path.
pub const MAX_SYNCHRONOUS_GIT_VISIBILITY_OBJECTS: u64 = 100_000;

/// Current serialized ref-update evidence format.
pub const GIT_VISIBILITY_EDIT_VERSION: u32 = 1;

/// Immutable visibility delta published before one ref update becomes visible.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitVisibilityEdit {
    /// Schema version of this object.
    pub version: u32,
    /// Ref tip used as the prior closure, if one exists.
    ///
    /// For a new destination ref this may be the tip of an existing visible
    /// ref whose closure can be reused by catalog-bound compaction.
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitCatalogVisibilityTransition {
    from_ordinal: u32,
    to_ordinal: u32,
    objects: GitVisibilityClosure,
}

/// Catalog-bound visibility proof that keeps the large OID dictionary lazy.
///
/// Catalog-bound v1 proofs store ref closures as catalog ordinals. This view validates and
/// queries those closures without reading every ordinal-to-OID row. Callers
/// resolve only the ordinals selected for one operation through the pinned
/// catalog session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCatalogVisibilityIndex {
    /// Schema version of this object.
    pub version: u32,
    /// Manifest generation this proof describes.
    pub generation: u64,
    /// Pack-index hash this proof was built against.
    pub pack_index_hash: String,
    /// Digest binding the complete manifest Git state described by this proof.
    pub git_validation_digest: String,
    /// Digest naming the exact immutable catalog checkpoint.
    pub catalog_digest: String,
    /// Number of dense ordinals in the bound catalog.
    pub object_count: u64,
    refs: BTreeMap<String, GitVisibilityClosure>,
    transitions: BTreeMap<String, Vec<GitCatalogVisibilityTransition>>,
    incremental_history: BTreeMap<String, Vec<GitCatalogVisibilityTransition>>,
}

impl GitCatalogVisibilityIndex {
    #[cfg(feature = "remote-index")]
    fn from_parts(
        generation: u64,
        pack_index_hash: String,
        git_validation_digest: String,
        catalog_digest: String,
        object_count: u64,
        refs: BTreeMap<String, GitVisibilityClosure>,
        transitions: BTreeMap<String, Vec<GitCatalogVisibilityTransition>>,
        incremental_history: BTreeMap<String, Vec<GitCatalogVisibilityTransition>>,
    ) -> Result<Self> {
        let index = Self {
            version: GIT_VISIBILITY_INDEX_VERSION,
            generation,
            pack_index_hash,
            git_validation_digest,
            catalog_digest,
            object_count,
            refs,
            transitions,
            incremental_history,
        };
        index.validate()?;
        Ok(index)
    }

    /// Validate the ordinal proof without opening the catalog dictionary.
    pub fn validate(&self) -> Result<()> {
        if self.version != GIT_VISIBILITY_INDEX_VERSION {
            return Err(corrupt("visibility index version is unsupported"));
        }
        validate_hash(&self.pack_index_hash, "pack index hash")?;
        validate_hash(&self.git_validation_digest, "Git validation digest")?;
        validate_hash(&self.catalog_digest, "Git object catalog digest")?;
        let object_count = usize::try_from(self.object_count)
            .map_err(|_| corrupt("Git object catalog count cannot be represented"))?;
        if self.object_count > MAX_GIT_VISIBILITY_OBJECTS {
            return Err(corrupt(
                "visibility object dictionary contains too many objects",
            ));
        }
        if self.refs.len() > MAX_GIT_VISIBILITY_REFS {
            return Err(corrupt("visibility index contains too many refs"));
        }
        for (name, closure) in &self.refs {
            validate_ref_name(name)?;
            closure.validate(object_count)?;
        }
        validate_catalog_transitions(
            &self.refs,
            &self.transitions,
            object_count,
            MAX_VISIBILITY_TRANSITIONS_PER_REF,
            "visibility transitions",
        )?;
        validate_catalog_transitions(
            &self.refs,
            &self.incremental_history,
            object_count,
            MAX_VISIBILITY_HISTORY_TRANSITIONS_PER_REF,
            "incremental visibility history",
        )?;
        Ok(())
    }

    #[cfg(feature = "remote-index")]
    /// Return the exact catalog checkpoint identity bound to this proof.
    pub fn catalog_identity(&self) -> Result<crate::git_object_locator::GitObjectCatalogIdentity> {
        Ok(crate::git_object_locator::GitObjectCatalogIdentity {
            generation: self.generation,
            pack_index_hash: crab_xet::hash::MerkleHash::from_hex(&self.pack_index_hash)
                .map_err(|_| corrupt("invalid pack index hash"))?,
            object_count: self.object_count,
            catalog_digest: crab_xet::hash::MerkleHash::from_hex(&self.catalog_digest)
                .map_err(|_| corrupt("invalid catalog digest"))?,
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

    /// Return whether one ref closure contains a catalog ordinal.
    #[must_use]
    pub fn contains_ordinal_in_ref(&self, name: &str, ordinal: u32) -> bool {
        self.refs
            .get(name)
            .is_some_and(|closure| closure.contains(ordinal))
    }

    /// Return whether an ordinal is reachable from one visible ref.
    #[must_use]
    pub fn contains_ordinal_for_refs<'a, I>(&self, refs: I, ordinal: u32) -> bool
    where
        I: IntoIterator<Item = &'a str>,
    {
        refs.into_iter()
            .any(|name| self.contains_ordinal_in_ref(name, ordinal))
    }

    /// Return the union of objects rooted at the supplied refs as ordinals.
    #[must_use]
    pub fn ordinals_for_refs<'a, I>(&self, refs: I) -> Vec<u32>
    where
        I: IntoIterator<Item = &'a str>,
    {
        bitmap_positions(&self.union_for_refs(refs))
    }

    /// Return the selected-ref union minus the excluded-ref union.
    #[must_use]
    pub fn ordinals_for_ref_difference<'a, I, J>(&self, selected: I, excluded: J) -> Vec<u32>
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
    }

    /// Return a proven incremental closure for one exact prior ref tip.
    #[must_use]
    pub fn incremental_ordinals(
        &self,
        name: &str,
        to_ordinal: u32,
        haves: &[u32],
    ) -> Option<Vec<u32>> {
        if haves.contains(&to_ordinal) {
            return Some(Vec::new());
        }
        if let Some(transition) = self
            .transitions
            .get(name)
            .into_iter()
            .flat_map(|transitions| transitions.iter().rev())
            .find(|transition| {
                transition.to_ordinal == to_ordinal && haves.contains(&transition.from_ordinal)
            })
        {
            return Some(transition.objects.positions());
        }

        let history = self.incremental_history.get(name)?;
        let by_from = history
            .iter()
            .map(|transition| (transition.from_ordinal, transition))
            .collect::<HashMap<_, _>>();
        for have in haves {
            let mut current = *have;
            let mut selected = vec![0_u8; usize::try_from(self.object_count).ok()?.div_ceil(8)];
            let mut steps = 0usize;
            while current != to_ordinal {
                let Some(transition) = by_from.get(&current) else {
                    break;
                };
                transition.objects.union_into(&mut selected);
                current = transition.to_ordinal;
                steps = steps.saturating_add(1);
                if steps > history.len() {
                    break;
                }
            }
            if current == to_ordinal {
                return Some(bitmap_positions(&selected));
            }
        }
        None
    }

    /// Count distinct objects rooted at the supplied visible refs.
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

    /// Return an authorization digest for the ordinal selection.
    ///
    /// The immutable catalog digest binds ordinals to their OIDs. Hashing the
    /// selected ordinals avoids materializing the full dictionary while still
    /// keeping response-pack cache keys generation- and authorization-bound.
    #[must_use]
    pub fn authorization_digest_for_refs<'a, I>(&self, refs: I) -> [u8; 32]
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut hash = blake3::Hasher::new();
        hash.update(b"crab.git-visibility.authorization.ordinal.v1\0");
        hash.update(self.git_validation_digest.as_bytes());
        hash.update(self.catalog_digest.as_bytes());
        for ordinal in self.ordinals_for_refs(refs) {
            hash.update(&ordinal.to_be_bytes());
        }
        *hash.finalize().as_bytes()
    }

    /// Return total ref memberships without materializing object IDs.
    ///
    /// A shared object is counted once for every ref that contains it. This
    /// metric can exceed [`MAX_GIT_VISIBILITY_OBJECTS`]; the proof limit applies
    /// to the unique object dictionary and the serialized proof size.
    #[must_use]
    pub fn membership_count(&self) -> u64 {
        self.refs.values().map(GitVisibilityClosure::len).sum()
    }

    fn union_for_refs<'a, I>(&self, refs: I) -> Vec<u8>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut union = vec![0u8; usize::try_from(self.object_count).unwrap_or(0).div_ceil(8)];
        for closure in refs.into_iter().filter_map(|name| self.refs.get(name)) {
            closure.union_into(&mut union);
        }
        union
    }

    #[cfg(feature = "remote-index")]
    fn resize_bitmaps(&mut self) -> Result<()> {
        let object_count = usize::try_from(self.object_count)
            .map_err(|_| corrupt("Git object catalog count cannot be represented"))?;
        let bitmap_len = object_count.div_ceil(8);
        for closure in self.refs.values_mut() {
            if let GitVisibilityClosure::Bitmap(bitmap) = closure {
                bitmap.resize(bitmap_len, 0);
            }
        }
        for transitions in self.transitions.values_mut() {
            for transition in transitions {
                if let GitVisibilityClosure::Bitmap(bitmap) = &mut transition.objects {
                    bitmap.resize(bitmap_len, 0);
                }
            }
        }
        for transitions in self.incremental_history.values_mut() {
            for transition in transitions {
                if let GitVisibilityClosure::Bitmap(bitmap) = &mut transition.objects {
                    bitmap.resize(bitmap_len, 0);
                }
            }
        }
        Ok(())
    }

    #[cfg(feature = "remote-index")]
    fn rebind_identity(
        &mut self,
        generation: u64,
        pack_index_hash: &str,
        git_validation_digest: &str,
        catalog_digest: &str,
        object_count: u64,
    ) -> Result<()> {
        if object_count < self.object_count {
            return Err(corrupt(
                "catalog-bound visibility update removed catalog ordinals",
            ));
        }
        self.generation = generation;
        self.pack_index_hash = pack_index_hash.to_owned();
        self.git_validation_digest = git_validation_digest.to_owned();
        self.catalog_digest = catalog_digest.to_owned();
        self.object_count = object_count;
        self.resize_bitmaps()?;
        self.validate()
    }

    #[cfg(feature = "remote-index")]
    fn remove_ref(&mut self, name: &str) {
        self.refs.remove(name);
        self.transitions.remove(name);
        self.incremental_history.remove(name);
    }

    #[cfg(feature = "remote-index")]
    fn apply_ordinal_edit(
        &mut self,
        name: String,
        edit: &GitVisibilityEdit,
        from_ordinal: Option<u32>,
        to_ordinal: u32,
        base_ref: Option<&str>,
        mut added: Vec<u32>,
        mut removed: Vec<u32>,
    ) -> Result<()> {
        edit.validate()?;
        let object_count = usize::try_from(self.object_count)
            .map_err(|_| corrupt("Git object catalog count cannot be represented"))?;
        if u64::from(to_ordinal) >= self.object_count {
            return Err(corrupt(
                "visibility edit target is outside its Git object catalog",
            ));
        }
        added.sort_unstable();
        added.dedup();
        removed.sort_unstable();
        removed.dedup();
        if added
            .iter()
            .chain(&removed)
            .any(|ordinal| usize::try_from(*ordinal).ok() >= Some(object_count))
        {
            return Err(corrupt(
                "visibility edit object is outside its Git object catalog",
            ));
        }

        let prior = match base_ref {
            Some(base_ref) => self
                .refs
                .get(base_ref)
                .map(GitVisibilityClosure::positions)
                .ok_or_else(|| corrupt("visibility base ref is absent from its prior proof"))?,
            None => self
                .refs
                .get(&name)
                .map(GitVisibilityClosure::positions)
                .unwrap_or_default(),
        };
        if edit.old_oid.is_none() && self.refs.contains_key(&name) {
            return Err(corrupt(
                "visibility add targets a ref already present in its prior catalog",
            ));
        }
        if let Some(from_ordinal) = from_ordinal
            && prior.binary_search(&from_ordinal).is_err()
        {
            return Err(corrupt(
                "visibility delta prior closure does not contain its old ref tip",
            ));
        }
        if edit.old_oid.is_some() && from_ordinal.is_none() {
            return Err(corrupt(
                "visibility delta old ref tip is absent from its prior catalog",
            ));
        }
        let positions = if edit.replaces {
            added.clone()
        } else {
            let mut positions = prior;
            for ordinal in &removed {
                let position = positions.binary_search(ordinal).map_err(|_| {
                    corrupt("visibility edit removes an object outside the prior closure")
                })?;
                positions.remove(position);
            }
            positions.extend(added.iter().copied());
            positions.sort_unstable();
            positions.dedup();
            positions
        };
        if positions.binary_search(&to_ordinal).is_err() {
            return Err(corrupt(
                "visibility edit result does not contain its new ref tip",
            ));
        }
        self.refs.insert(
            name.clone(),
            GitVisibilityClosure::from_positions(positions, object_count)?,
        );
        if edit.replaces || !removed.is_empty() {
            self.transitions.remove(&name);
            self.incremental_history.remove(&name);
            return Ok(());
        }
        let Some(from_ordinal) = from_ordinal else {
            self.transitions.remove(&name);
            self.incremental_history.remove(&name);
            return Ok(());
        };
        let transitions = self.transitions.entry(name.clone()).or_default();
        for transition in transitions.iter_mut() {
            let mut positions = transition.objects.positions();
            positions.extend(added.iter().copied());
            positions.sort_unstable();
            positions.dedup();
            transition.to_ordinal = to_ordinal;
            transition.objects = GitVisibilityClosure::from_positions(positions, object_count)?;
        }
        transitions.retain(|transition| transition.from_ordinal != from_ordinal);
        transitions.push(GitCatalogVisibilityTransition {
            from_ordinal,
            to_ordinal,
            objects: GitVisibilityClosure::from_positions(added.clone(), object_count)?,
        });
        if transitions.len() > MAX_VISIBILITY_TRANSITIONS_PER_REF {
            transitions.remove(0);
        }
        let history = self.incremental_history.entry(name).or_default();
        history.push(GitCatalogVisibilityTransition {
            from_ordinal,
            to_ordinal,
            objects: GitVisibilityClosure::from_positions(added, object_count)?,
        });
        if history.len() > MAX_VISIBILITY_HISTORY_TRANSITIONS_PER_REF {
            history.remove(0);
        }
        Ok(())
    }
}

fn validate_catalog_transitions(
    refs: &BTreeMap<String, GitVisibilityClosure>,
    transitions: &BTreeMap<String, Vec<GitCatalogVisibilityTransition>>,
    object_count: usize,
    maximum: usize,
    field: &str,
) -> Result<()> {
    for (name, transitions) in transitions {
        if !refs.contains_key(name) || transitions.len() > maximum {
            return Err(corrupt(format!("{field} do not match their ref")));
        }
        let reference = refs
            .get(name)
            .ok_or_else(|| corrupt(format!("{field} ref closure is absent")))?;
        for transition in transitions {
            if usize::try_from(transition.from_ordinal).ok() >= Some(object_count)
                || usize::try_from(transition.to_ordinal).ok() >= Some(object_count)
                || !reference.contains(transition.from_ordinal)
                || !reference.contains(transition.to_ordinal)
                || transition.objects.validate(object_count)? > MAX_GIT_VISIBILITY_OBJECTS
                || transition
                    .objects
                    .positions()
                    .into_iter()
                    .any(|position| !reference.contains(position))
            {
                return Err(corrupt(format!("{field} transition is invalid")));
            }
        }
    }
    Ok(())
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
        for (name, objects) in refs {
            validate_ref_name(&name)?;
            let mut decoded = objects
                .into_iter()
                .map(|oid| decode_oid(&oid))
                .collect::<Result<Vec<_>>>()?;
            decoded.sort_unstable();
            decoded.dedup();
            dictionary.extend(decoded.iter().copied());
            if dictionary.len() as u64 > MAX_GIT_VISIBILITY_OBJECTS {
                return Err(corrupt(
                    "visibility object dictionary contains too many objects",
                ));
            }
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

    #[cfg(any(feature = "storage", test))]
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
        for (name, closure) in &self.refs {
            validate_ref_name(name)?;
            closure.validate(self.objects.len())?;
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

    /// Materialize all ref closures for rebuild boundaries.
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

    #[cfg(feature = "storage")]
    fn remove_ref(&mut self, name: &str) {
        self.refs.remove(name);
        self.transitions.remove(name);
        self.incremental_history.remove(name);
    }

    #[cfg(any(feature = "storage", test))]
    fn apply_edit(&mut self, name: String, edit: &GitVisibilityEdit) -> Result<()> {
        self.apply_edit_with_base(name, edit, None)
    }

    #[cfg(any(feature = "storage", test))]
    fn apply_edit_with_base(
        &mut self,
        name: String,
        edit: &GitVisibilityEdit,
        base_ref: Option<&str>,
    ) -> Result<()> {
        let prior = match base_ref {
            Some(base_ref) => Some(
                self.objects_for_ref(base_ref)
                    .ok_or_else(|| corrupt("visibility base ref is absent from its prior proof"))?,
            ),
            None => self.objects_for_ref(&name),
        };
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
        for transitions in self.transitions.values_mut() {
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

    #[cfg(any(feature = "storage", test))]
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
    use std::collections::HashMap;
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
        GIT_VISIBILITY_INDEX_VERSION, GitCatalogVisibilityIndex, GitVisibilityEdit,
        GitVisibilityIndex, MAX_GIT_VISIBILITY_INDEX_BYTES, MAX_GIT_VISIBILITY_OBJECTS,
        MAX_GIT_VISIBILITY_REFS, validate_hash, validate_oid,
    };
    use crate::error::Result;
    use crate::manifests::Manifest;
    use crate::ref_journal::RefJournalEdit;

    /// Stored format used to satisfy a visibility read.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum GitVisibilityFormat {
        /// Catalog-ordinal v1 proof with no independent OID dictionary.
        CatalogV1,
        /// Digest-bound v1 proof with an embedded OID dictionary.
        DigestV1,
    }

    /// Validated visibility proof and its stored format.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct GitVisibilityRead {
        /// Normalized current-format proof.
        pub index: GitVisibilityIndex,
        /// Stored format that supplied the proof.
        pub format: GitVisibilityFormat,
    }

    /// Lazy catalog-bound visibility proof and its storage format.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct GitCatalogVisibilityRead {
        /// Ordinal proof that has not materialized the catalog OID dictionary.
        pub index: GitCatalogVisibilityIndex,
        /// Stored format that supplied the proof.
        pub format: GitVisibilityFormat,
    }

    // The digest-bound v1 shape stores a sorted hexadecimal dictionary. Reads
    // normalize it without expanding per-ref OID ownership.
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct GitVisibilityDictionaryV1 {
        version: u32,
        generation: u64,
        pack_index_hash: String,
        git_validation_digest: String,
        objects: Vec<String>,
        refs: BTreeMap<String, GitVisibilityStoredClosureV1>,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct GitVisibilityDigestV1 {
        version: u32,
        generation: u64,
        pack_index_hash: String,
        git_validation_digest: String,
        objects: Vec<String>,
        refs: BTreeMap<String, GitVisibilityStoredClosureV1>,
        transitions: BTreeMap<String, Vec<GitVisibilityTransitionOidV1>>,
        incremental_history: BTreeMap<String, Vec<GitVisibilityTransitionOidV1>>,
    }

    #[cfg(feature = "remote-index")]
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct GitVisibilityCatalogV1 {
        version: u32,
        generation: u64,
        pack_index_hash: String,
        git_validation_digest: String,
        catalog_digest: String,
        object_count: u64,
        refs: BTreeMap<String, GitVisibilityStoredClosureV1>,
        transitions: BTreeMap<String, Vec<GitVisibilityTransitionOrdinalV1>>,
        incremental_history: BTreeMap<String, Vec<GitVisibilityTransitionOrdinalV1>>,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct GitVisibilityTransitionOidV1 {
        from_oid: String,
        to_oid: String,
        objects: GitVisibilityStoredClosureV1,
    }

    #[cfg(feature = "remote-index")]
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct GitVisibilityTransitionOrdinalV1 {
        from_ordinal: u32,
        to_ordinal: u32,
        objects: GitVisibilityStoredClosureV1,
    }

    #[cfg(feature = "remote-index")]
    const GIT_VISIBILITY_PENDING_VERSION: u32 = 1;

    #[cfg(feature = "remote-index")]
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct GitVisibilityPending {
        version: u32,
        target_generation: u64,
        target_pack_index_hash: String,
        target_git_validation_digest: String,
        base_generation: u64,
        base_pack_index_hash: String,
        base_git_validation_digest: String,
        base_catalog_digest: String,
        base_object_count: u64,
        edits: Vec<GitVisibilityPendingEdit>,
    }

    #[cfg(feature = "remote-index")]
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct GitVisibilityPendingEdit {
        ref_name: String,
        old_oid: Option<String>,
        new_oid: Option<String>,
        visibility_evidence_hash: Option<String>,
    }

    #[cfg(feature = "remote-index")]
    struct ResolvedGitVisibilityPendingEdit {
        ref_name: String,
        old_oid: Option<[u8; 20]>,
        source_oid: Option<[u8; 20]>,
        new_oid: Option<[u8; 20]>,
        base_ref_name: Option<String>,
        evidence: Option<GitVisibilityEdit>,
    }

    #[derive(Clone, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum GitVisibilityStoredClosureV1 {
        Sparse(Vec<u32>),
        Bitmap(String),
    }

    impl GitVisibilityDictionaryV1 {
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
                        GitVisibilityStoredClosureV1::from_positions(positions, objects.len())?,
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
            let objects = self
                .objects
                .iter()
                .map(|oid| super::decode_oid(oid))
                .collect::<Result<Vec<_>>>()?;
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

    impl GitVisibilityDigestV1 {
        fn from_index(index: &GitVisibilityIndex) -> Result<Self> {
            let encoded = GitVisibilityDictionaryV1::from_index(index)?;
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
                                    Ok(GitVisibilityTransitionOidV1 {
                                        from_oid: super::encode_oid(&transition.from_oid),
                                        to_oid: super::encode_oid(&transition.to_oid),
                                        objects: GitVisibilityStoredClosureV1::from_positions(
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
                version: GIT_VISIBILITY_INDEX_VERSION,
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
            if self.version != GIT_VISIBILITY_INDEX_VERSION {
                return Err(super::corrupt(
                    "visibility index storage version is unsupported",
                ));
            }
            let object_count = self.objects.len();
            let decode_transitions =
                |source: BTreeMap<String, Vec<GitVisibilityTransitionOidV1>>| {
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
            let mut index = GitVisibilityDictionaryV1 {
                version: GIT_VISIBILITY_INDEX_VERSION,
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
    impl GitVisibilityCatalogV1 {
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
            let decode_closure = |closure: &GitVisibilityStoredClosureV1| {
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
                GitVisibilityStoredClosureV1::from_positions(ordinals, catalog.len())
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
                                    Ok(GitVisibilityTransitionOrdinalV1 {
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

        fn into_catalog_index(self) -> Result<super::GitCatalogVisibilityIndex> {
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
            if self.object_count > MAX_GIT_VISIBILITY_OBJECTS {
                return Err(super::corrupt(
                    "visibility object dictionary contains too many objects",
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
            let decode_transitions =
                |source: BTreeMap<String, Vec<GitVisibilityTransitionOrdinalV1>>| {
                    source
                        .into_iter()
                        .map(|(name, transitions)| {
                            let transitions = transitions
                                .into_iter()
                                .map(|transition| {
                                    if usize::try_from(transition.from_ordinal).ok()
                                        >= Some(object_count)
                                        || usize::try_from(transition.to_ordinal).ok()
                                            >= Some(object_count)
                                    {
                                        return Err(super::corrupt(
                                            "visibility transition endpoint is outside its catalog",
                                        ));
                                    }
                                    Ok(super::GitCatalogVisibilityTransition {
                                        from_ordinal: transition.from_ordinal,
                                        to_ordinal: transition.to_ordinal,
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
            super::GitCatalogVisibilityIndex::from_parts(
                self.generation,
                self.pack_index_hash,
                self.git_validation_digest,
                self.catalog_digest,
                self.object_count,
                refs,
                transitions,
                incremental_history,
            )
        }

        fn from_catalog_index(index: &super::GitCatalogVisibilityIndex) -> Result<Self> {
            index.validate()?;
            let object_count = usize::try_from(index.object_count)
                .map_err(|_| super::corrupt("Git object catalog count cannot be represented"))?;
            let encode_closure = |closure: &super::GitVisibilityClosure| {
                GitVisibilityStoredClosureV1::from_positions(closure.positions(), object_count)
            };
            let encode_transitions =
                |source: &BTreeMap<String, Vec<super::GitCatalogVisibilityTransition>>| {
                    source
                        .iter()
                        .map(|(name, transitions)| {
                            let transitions = transitions
                                .iter()
                                .map(|transition| {
                                    Ok(GitVisibilityTransitionOrdinalV1 {
                                        from_ordinal: transition.from_ordinal,
                                        to_ordinal: transition.to_ordinal,
                                        objects: encode_closure(&transition.objects)?,
                                    })
                                })
                                .collect::<Result<Vec<_>>>()?;
                            Ok((name.clone(), transitions))
                        })
                        .collect::<Result<BTreeMap<_, _>>>()
                };
            let refs = index
                .refs
                .iter()
                .map(|(name, closure)| Ok((name.clone(), encode_closure(closure)?)))
                .collect::<Result<BTreeMap<_, _>>>()?;
            Ok(Self {
                version: GIT_VISIBILITY_INDEX_VERSION,
                generation: index.generation,
                pack_index_hash: index.pack_index_hash.clone(),
                git_validation_digest: index.git_validation_digest.clone(),
                catalog_digest: index.catalog_digest.clone(),
                object_count: index.object_count,
                refs,
                transitions: encode_transitions(&index.transitions)?,
                incremental_history: encode_transitions(&index.incremental_history)?,
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
            let decode_transitions =
                |source: BTreeMap<String, Vec<GitVisibilityTransitionOrdinalV1>>| {
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

    impl GitVisibilityStoredClosureV1 {
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
        let index: GitVisibilityDigestV1 = serde_json::from_slice(body).map_err(|error| {
            crate::error::MetadataError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!("invalid visibility index JSON: {error}"),
            }
        })?;
        index
            .into_index()
            .map(|index| (index, GitVisibilityFormat::DigestV1))
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
        let stored: GitVisibilityCatalogV1 = serde_json::from_slice(&body).map_err(|error| {
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
        index.validate_identity(generation, pack_index_hash, git_validation_digest)?;
        Ok((index, GitVisibilityFormat::CatalogV1))
    }

    #[cfg(feature = "remote-index")]
    async fn read_catalog_bound_lazy(
        store: &Store,
        router: &StoreLayout<Store>,
        generation: u64,
        pack_index_hash: &str,
        git_validation_digest: &str,
    ) -> Result<(GitCatalogVisibilityIndex, GitVisibilityFormat)> {
        let path = router.git_visibility_catalog_path(git_validation_digest);
        let body = read_bounded(store, &path).await?;
        let stored: GitVisibilityCatalogV1 = serde_json::from_slice(&body).map_err(|error| {
            crate::error::MetadataError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!("invalid catalog visibility index JSON: {error}"),
            }
        })?;
        stored.validate_binding(generation, pack_index_hash, git_validation_digest)?;
        Ok((stored.into_catalog_index()?, GitVisibilityFormat::CatalogV1))
    }

    #[cfg(feature = "remote-index")]
    fn catalog_identity(
        stored: &GitVisibilityCatalogV1,
        generation: u64,
        pack_index_hash: &str,
        git_validation_digest: &str,
    ) -> Result<crate::git_object_locator::GitObjectCatalogIdentity> {
        stored.validate_binding(generation, pack_index_hash, git_validation_digest)
    }

    /// Check a catalog proof without materializing the complete ordinal list.
    ///
    /// Push and owner admission only need to know whether an immutable catalog v1
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
        let Some(identity) = read_catalog_identity(
            store,
            router,
            generation,
            pack_index_hash,
            git_validation_digest,
        )
        .await?
        else {
            return Ok(false);
        };
        if catalog_checkpoint_marker_matches(store, router, identity).await? {
            return Ok(true);
        }
        let session = crate::git_object_locator::GitObjectLocatorSession::open_for_catalog(
            Arc::clone(store.inner()),
            router.repo_prefix(),
            identity,
            Duration::from_secs(60 * 60),
        )
        .await?;
        let matches = session.catalog_identity() == Some(identity);
        session.close().await?;
        if matches {
            write_catalog_checkpoint_marker(store, router, identity).await?;
        }
        Ok(matches)
    }

    #[cfg(feature = "remote-index")]
    async fn read_catalog_identity(
        store: &Store,
        router: &StoreLayout<Store>,
        generation: u64,
        pack_index_hash: &str,
        git_validation_digest: &str,
    ) -> Result<Option<crate::git_object_locator::GitObjectCatalogIdentity>> {
        let path = router.git_visibility_catalog_path(git_validation_digest);
        let body = match read_bounded(store, &path).await {
            Ok(body) => body,
            Err(crate::error::MetadataError::Storage {
                source: StorageError::NotFound { .. },
            }) => return Ok(None),
            Err(error) => return Err(error),
        };
        let stored: GitVisibilityCatalogV1 = serde_json::from_slice(&body).map_err(|error| {
            crate::error::MetadataError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!("invalid catalog visibility index JSON: {error}"),
            }
        })?;
        catalog_identity(&stored, generation, pack_index_hash, git_validation_digest).map(Some)
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

    /// Check whether the catalog-bound v1 visibility proof is published.
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
        let Some(identity) = read_catalog_identity(
            store,
            router,
            generation,
            pack_index_hash,
            git_validation_digest,
        )
        .await?
        else {
            return Ok(false);
        };
        catalog_checkpoint_marker_matches(store, router, identity).await
    }

    /// Check whether a digest-bound visibility proof is present without reading its body.
    ///
    /// Callers must still decode and validate the proof before using its contents.
    pub async fn digest_bound_available(
        store: &Store,
        router: &StoreLayout<Store>,
        git_validation_digest: &str,
    ) -> Result<bool> {
        validate_hash(git_validation_digest, "Git validation digest")?;
        let path = router.git_visibility_path(git_validation_digest);
        match store.head(&path).await {
            Ok(metadata) if metadata.size <= MAX_GIT_VISIBILITY_INDEX_BYTES => Ok(true),
            Ok(metadata) => Err(crate::error::MetadataError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!(
                    "visibility index is {} bytes; maximum is {}",
                    metadata.size, MAX_GIT_VISIBILITY_INDEX_BYTES
                ),
            }),
            Err(StorageError::NotFound { .. }) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Check the post-checkpoint marker without opening the large SlateDB catalog.
    #[cfg(feature = "remote-index")]
    async fn catalog_checkpoint_marker_matches(
        store: &Store,
        router: &StoreLayout<Store>,
        identity: crate::git_object_locator::GitObjectCatalogIdentity,
    ) -> Result<bool> {
        let path = ObjectPath::from(crate::git_object_locator::catalog_checkpoint_marker_path(
            router.repo_prefix(),
            identity.catalog_digest,
        ));
        let body = match store.get_with_etag(&path).await {
            Ok((body, _)) => body,
            Err(StorageError::NotFound { .. }) => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let marker: crate::git_object_locator::CatalogCheckpointMarker =
            serde_json::from_slice(&body).map_err(|error| {
                crate::error::MetadataError::CorruptObject {
                    path: path.as_ref().to_owned(),
                    reason: format!("invalid catalog checkpoint marker JSON: {error}"),
                }
            })?;
        Ok(marker.matches_identity(identity))
    }

    #[cfg(feature = "remote-index")]
    async fn write_catalog_checkpoint_marker(
        store: &Store,
        router: &StoreLayout<Store>,
        identity: crate::git_object_locator::GitObjectCatalogIdentity,
    ) -> Result<()> {
        let path = ObjectPath::from(crate::git_object_locator::catalog_checkpoint_marker_path(
            router.repo_prefix(),
            identity.catalog_digest,
        ));
        let body = serde_json::to_vec(
            &crate::git_object_locator::CatalogCheckpointMarker::for_identity(identity),
        )
        .map_err(|error| {
            crate::error::MetadataError::Internal(format!(
                "catalog checkpoint marker serialize: {error}"
            ))
        })?;
        store
            .put(&path, Bytes::from(body))
            .await
            .map_err(Into::into)
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

    /// Read the canonical catalog-bound or digest-bound v1 proof.
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
        let (index, format) = read_digest_bound(
            store,
            router,
            generation,
            pack_index_hash,
            git_validation_digest,
        )
        .await?;
        Ok(GitVisibilityRead { index, format })
    }

    /// Read a catalog-bound v1 proof without materializing its OID dictionary.
    #[cfg(feature = "remote-index")]
    pub async fn read_catalog_with_format(
        store: &Store,
        router: &StoreLayout<Store>,
        generation: u64,
        pack_index_hash: &str,
        git_validation_digest: &str,
    ) -> Result<GitCatalogVisibilityRead> {
        validate_hash(git_validation_digest, "Git validation digest")?;
        let (index, format) = read_catalog_bound_lazy(
            store,
            router,
            generation,
            pack_index_hash,
            git_validation_digest,
        )
        .await?;
        Ok(GitCatalogVisibilityRead { index, format })
    }

    /// Read and validate a canonical v1 visibility proof.
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

    /// Read the canonical v1 proof applicable to one manifest.
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
            Ok(read) => {
                let path = match read.format {
                    GitVisibilityFormat::CatalogV1 => {
                        router.git_visibility_catalog_path(&manifest.git_validation_digest)
                    }
                    GitVisibilityFormat::DigestV1 => {
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
                let stored = GitVisibilityCatalogV1::from_index(index, &catalog, identity)?;
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
        upload_digest_bound_if_absent(store, router, index).await
    }

    /// Upload a digest-bound visibility proof without opening the remote catalog.
    ///
    /// This preserves the immutable digest-bound v1 contract for acknowledgement paths
    /// that defer repository-wide catalog maintenance to the generation owner.
    pub async fn upload_digest_bound_if_absent(
        store: &Store,
        router: &StoreLayout<Store>,
        index: &GitVisibilityIndex,
    ) -> Result<()> {
        index.validate()?;
        let stored = GitVisibilityDigestV1::from_index(index)?;
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
            if let Some(index) = apply_catalog_journal_edits(store, router, manifest).await? {
                upload_catalog_index(store, router, &index).await?;
                return Ok(true);
            }
            let Some(read) = read_for_manifest(store, router, manifest).await? else {
                return Ok(false);
            };
            if read.format == GitVisibilityFormat::CatalogV1 {
                return Ok(true);
            }
            upload_if_absent(store, router, &read.index).await?;
            return Ok(read_for_manifest(store, router, manifest)
                .await?
                .is_some_and(|read| read.format == GitVisibilityFormat::CatalogV1));
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
                    .get_with_etag_bounded(path, MAX_GIT_VISIBILITY_INDEX_BYTES)
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

    /// Stage a lazy ordinal proof for a ref-journal compaction.
    ///
    /// A catalog-bound v1 proof can be carried forward without materializing its OID
    /// dictionary. The pending object is immutable and keyed by the target
    /// validation digest; the owner completes it after publishing the target
    /// catalog, or the normal full rebuild remains available.
    pub async fn prepare_catalog_journal_edits(
        store: &Store,
        router: &StoreLayout<Store>,
        base: &Manifest,
        edits: &[RefJournalEdit],
        final_refs: &BTreeMap<String, String>,
        generation: u64,
        pack_index_hash: &str,
        git_validation_digest: &str,
    ) -> Result<bool> {
        #[cfg(feature = "remote-index")]
        {
            if base.refs.is_empty() {
                return Ok(false);
            }
            let path = router.git_visibility_catalog_path(&base.git_validation_digest);
            let body = match read_bounded(store, &path).await {
                Ok(body) => body,
                Err(crate::error::MetadataError::Storage {
                    source: StorageError::NotFound { .. },
                }) => return Ok(false),
                Err(error) => return Err(error),
            };
            let stored: GitVisibilityCatalogV1 =
                serde_json::from_slice(&body).map_err(|error| {
                    crate::error::MetadataError::CorruptObject {
                        path: path.as_ref().to_owned(),
                        reason: format!("invalid catalog visibility index JSON: {error}"),
                    }
                })?;
            let base_identity = stored.validate_binding(
                base.generation,
                &base.pack_index_hash,
                &base.git_validation_digest,
            )?;
            validate_hash(pack_index_hash, "pack index hash")?;
            validate_hash(git_validation_digest, "Git validation digest")?;
            let mut ref_tips = base.refs.clone();
            let edits = edits
                .iter()
                .map(|edit| {
                    super::validate_ref_name(&edit.ref_name)?;
                    if edit.old_oid.is_none() && edit.new_oid.is_none() {
                        return Err(super::corrupt(
                            "catalog visibility pending edit cannot omit both ref tips",
                        ));
                    }
                    if ref_tips.get(&edit.ref_name) != edit.old_oid.as_ref() {
                        return Err(super::corrupt(
                            "catalog visibility pending edit does not match its prior ref tip",
                        ));
                    }
                    if let Some(old_oid) = &edit.old_oid {
                        super::validate_oid(old_oid)?;
                    }
                    if let Some(new_oid) = &edit.new_oid {
                        super::validate_oid(new_oid)?;
                        let Some(evidence_hash) = edit.visibility_evidence_hash.as_deref() else {
                            return Ok(None);
                        };
                        validate_hash(evidence_hash, "visibility edit hash")?;
                    } else if edit.visibility_evidence_hash.is_some() {
                        return Err(super::corrupt(
                            "deleted ref visibility edit cannot carry evidence",
                        ));
                    }
                    match &edit.new_oid {
                        Some(new_oid) => {
                            ref_tips.insert(edit.ref_name.clone(), new_oid.clone());
                        }
                        None => {
                            ref_tips.remove(&edit.ref_name);
                        }
                    }
                    Ok(Some(GitVisibilityPendingEdit {
                        ref_name: edit.ref_name.clone(),
                        old_oid: edit.old_oid.clone(),
                        new_oid: edit.new_oid.clone(),
                        visibility_evidence_hash: edit.visibility_evidence_hash.clone(),
                    }))
                })
                .collect::<Result<Option<Vec<_>>>>()?;
            let Some(edits) = edits else {
                return Ok(false);
            };
            if ref_tips != *final_refs {
                return Err(super::corrupt(
                    "catalog visibility pending edits do not match their target refs",
                ));
            }
            let pending = GitVisibilityPending {
                version: GIT_VISIBILITY_PENDING_VERSION,
                target_generation: generation,
                target_pack_index_hash: pack_index_hash.to_owned(),
                target_git_validation_digest: git_validation_digest.to_owned(),
                base_generation: base_identity.generation,
                base_pack_index_hash: base_identity.pack_index_hash.to_string(),
                base_git_validation_digest: base.git_validation_digest.clone(),
                base_catalog_digest: base_identity.catalog_digest.to_string(),
                base_object_count: base_identity.object_count,
                edits,
            };
            let body = serde_json::to_vec(&pending).map_err(|error| {
                crate::error::MetadataError::Internal(format!(
                    "catalog visibility pending serialize: {error}"
                ))
            })?;
            if body.len() as u64 > MAX_GIT_VISIBILITY_INDEX_BYTES {
                return Err(crate::error::MetadataError::CorruptObject {
                    path: router
                        .git_visibility_pending_path(git_validation_digest)
                        .as_ref()
                        .to_owned(),
                    reason: format!(
                        "catalog visibility pending object exceeds {} bytes",
                        MAX_GIT_VISIBILITY_INDEX_BYTES
                    ),
                });
            }
            store
                .put_exact(
                    &router.git_visibility_pending_path(git_validation_digest),
                    Bytes::from(body),
                )
                .await?;
            Ok(true)
        }
        #[cfg(not(feature = "remote-index"))]
        {
            let _ = (
                store,
                router,
                base,
                edits,
                final_refs,
                generation,
                pack_index_hash,
                git_validation_digest,
            );
            Ok(false)
        }
    }

    #[cfg(feature = "remote-index")]
    async fn apply_catalog_journal_edits(
        store: &Store,
        router: &StoreLayout<Store>,
        manifest: &Manifest,
    ) -> Result<Option<GitCatalogVisibilityIndex>> {
        let pending_path = router.git_visibility_pending_path(&manifest.git_validation_digest);
        let body = match read_bounded(store, &pending_path).await {
            Ok(body) => body,
            Err(crate::error::MetadataError::Storage {
                source: StorageError::NotFound { .. },
            }) => return Ok(None),
            Err(error) => return Err(error),
        };
        let pending: GitVisibilityPending = serde_json::from_slice(&body).map_err(|error| {
            crate::error::MetadataError::CorruptObject {
                path: pending_path.as_ref().to_owned(),
                reason: format!("invalid catalog visibility pending JSON: {error}"),
            }
        })?;
        if pending.version != GIT_VISIBILITY_PENDING_VERSION
            || pending.target_generation != manifest.generation
            || pending.target_pack_index_hash != manifest.pack_index_hash
            || pending.target_git_validation_digest != manifest.git_validation_digest
        {
            return Err(super::corrupt(
                "catalog visibility pending object does not match its target manifest",
            ));
        }
        validate_hash(&pending.base_pack_index_hash, "base pack index hash")?;
        validate_hash(
            &pending.base_git_validation_digest,
            "base Git validation digest",
        )?;
        validate_hash(&pending.base_catalog_digest, "base catalog digest")?;
        let base_path = router.git_visibility_catalog_path(&pending.base_git_validation_digest);
        let base_body = match read_bounded(store, &base_path).await {
            Ok(body) => body,
            Err(crate::error::MetadataError::Storage {
                source: StorageError::NotFound { .. },
            }) => return Ok(None),
            Err(error) => return Err(error),
        };
        let base_stored: GitVisibilityCatalogV1 =
            serde_json::from_slice(&base_body).map_err(|error| {
                crate::error::MetadataError::CorruptObject {
                    path: base_path.as_ref().to_owned(),
                    reason: format!("invalid base catalog visibility index JSON: {error}"),
                }
            })?;
        let base_identity = base_stored.validate_binding(
            pending.base_generation,
            &pending.base_pack_index_hash,
            &pending.base_git_validation_digest,
        )?;
        if base_identity.catalog_digest.to_string() != pending.base_catalog_digest
            || base_identity.object_count != pending.base_object_count
        {
            return Ok(None);
        }
        let base_session =
            match crate::git_object_locator::GitObjectLocatorSession::open_for_catalog(
                Arc::clone(store.inner()),
                router.repo_prefix(),
                base_identity,
                Duration::from_secs(60 * 60),
            )
            .await
            {
                Ok(session) => session,
                Err(crate::error::MetadataError::CorruptObject { .. }) => return Ok(None),
                Err(error) => return Err(error),
            };
        base_session.close().await?;
        let mut index = base_stored.into_catalog_index()?;
        let packs =
            crate::manifest_store::read_bulk_pack_list(store, router, &manifest.pack_index_hash)
                .await?;
        let inventory = packs
            .into_iter()
            .map(|pack| {
                let pack_id = MerkleHash::from_hex(&pack.pack_id).map_err(|error| {
                    super::corrupt(format!(
                        "invalid manifest pack ID in catalog handoff: {error}"
                    ))
                })?;
                Ok((
                    pack_id,
                    crate::git_object_locator::GitPackInventoryEntry {
                        pack_id,
                        object_count: pack.object_count,
                        pack_size: pack.size,
                    },
                ))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        let session = crate::git_object_locator::GitObjectLocatorSession::open(
            Arc::clone(store.inner()),
            router.repo_prefix(),
        )
        .await?;
        let result: Result<Option<GitCatalogVisibilityIndex>> = async {
            let Some(identity) = session.catalog_identity() else {
                return Ok(None);
            };
            if identity.generation != manifest.generation
                || identity.pack_index_hash.to_string() != manifest.pack_index_hash
                || identity.object_count < pending.base_object_count
                || !catalog_checkpoint_marker_matches(store, router, identity).await?
            {
                return Ok(None);
            }

            let mut required = HashMap::<[u8; 20], ()>::new();
            for oid in manifest.refs.values().chain(manifest.peeled_refs.values()) {
                required.insert(super::decode_oid(oid)?, ());
            }
            let mut base_refs = manifest.refs.clone();
            for edit in pending.edits.iter().rev() {
                match &edit.old_oid {
                    Some(old_oid) => {
                        base_refs.insert(edit.ref_name.clone(), old_oid.clone());
                    }
                    None => {
                        base_refs.remove(&edit.ref_name);
                    }
                }
            }
            let mut resolved = Vec::with_capacity(pending.edits.len());
            for pending_edit in pending.edits {
                super::validate_ref_name(&pending_edit.ref_name)?;
                let old_oid = pending_edit
                    .old_oid
                    .as_deref()
                    .map(super::decode_oid)
                    .transpose()?;
                // A deleted tip may occur in neither the target refs nor an
                // update delta. Resolve it explicitly so deletion can prove
                // membership in the old closure before removing that ref.
                if let Some(oid) = old_oid {
                    required.insert(oid, ());
                }
                let new_oid = pending_edit
                    .new_oid
                    .as_deref()
                    .map(super::decode_oid)
                    .transpose()?;
                if let Some(new_oid) = new_oid {
                    required.insert(new_oid, ());
                    let Some(evidence_hash) = pending_edit.visibility_evidence_hash.as_deref()
                    else {
                        return Err(super::corrupt(
                            "catalog visibility pending update is missing evidence",
                        ));
                    };
                    let evidence = read_edit(store, router, evidence_hash).await?;
                    if evidence.new_oid
                        != pending_edit.new_oid.as_deref().ok_or_else(|| {
                            super::corrupt(
                                "catalog visibility pending update is missing its new ref tip",
                            )
                        })?
                    {
                        return Err(super::corrupt(
                            "catalog visibility evidence does not match its pending ref edit",
                        ));
                    }
                    let base_ref_name = match (&pending_edit.old_oid, &evidence.old_oid) {
                        (expected, actual) if expected == actual => None,
                        (None, Some(base_oid)) => {
                            // Resolve against the prior ref map because the same
                            // journal may also move or delete the source ref.
                            base_refs
                                .iter()
                                .find(|(_, tip)| *tip == base_oid)
                                .map(|(name, _)| name.clone())
                        }
                        _ => {
                            return Err(super::corrupt(
                                "catalog visibility evidence does not match its pending ref edit",
                            ));
                        }
                    };
                    let source_oid = evidence
                        .old_oid
                        .as_deref()
                        .map(super::decode_oid)
                        .transpose()?;
                    if let Some(oid) = source_oid {
                        required.insert(oid, ());
                    }
                    if evidence.old_oid != pending_edit.old_oid && base_ref_name.is_none() {
                        return Ok(None);
                    }
                    for oid in evidence.added.iter().chain(evidence.removed.iter()) {
                        required.insert(super::decode_oid(oid)?, ());
                    }
                    resolved.push(ResolvedGitVisibilityPendingEdit {
                        ref_name: pending_edit.ref_name,
                        old_oid,
                        source_oid,
                        new_oid: Some(new_oid),
                        base_ref_name,
                        evidence: Some(evidence),
                    });
                } else {
                    if pending_edit.visibility_evidence_hash.is_some() {
                        return Err(super::corrupt(
                            "deleted ref pending edit cannot carry visibility evidence",
                        ));
                    }
                    // Deleted tips need their own lookup: no surviving ref or
                    // update evidence necessarily names the old object.
                    if let Some(oid) = old_oid {
                        required.insert(oid, ());
                    }
                    resolved.push(ResolvedGitVisibilityPendingEdit {
                        ref_name: pending_edit.ref_name,
                        old_oid,
                        source_oid: old_oid,
                        new_oid: None,
                        base_ref_name: None,
                        evidence: None,
                    });
                }
            }
            let mut object_ids = required.keys().copied().collect::<Vec<_>>();
            object_ids.sort_unstable();
            let lookups = session.lookup_batch(&object_ids, &inventory).await?;
            let mut ordinals = HashMap::with_capacity(object_ids.len());
            for (oid, lookup) in object_ids.into_iter().zip(lookups) {
                match lookup {
                    crate::git_object_locator::GitObjectLookup::Hit(locator) => {
                        ordinals.insert(oid, locator.ordinal);
                    }
                    crate::git_object_locator::GitObjectLookup::Miss => return Ok(None),
                    crate::git_object_locator::GitObjectLookup::Corrupt => {
                        return Err(super::corrupt(
                            "catalog visibility handoff encountered a corrupt locator row",
                        ));
                    }
                }
            }
            index.rebind_identity(
                manifest.generation,
                &manifest.pack_index_hash,
                &manifest.git_validation_digest,
                &identity.catalog_digest.to_string(),
                identity.object_count,
            )?;
            for edit in resolved {
                let Some(evidence) = edit.evidence else {
                    let old_oid = edit.old_oid.ok_or_else(|| {
                        super::corrupt("catalog visibility deletion has no old ref tip")
                    })?;
                    let old_ordinal = ordinals.get(&old_oid).copied().ok_or_else(|| {
                        super::corrupt("catalog visibility deletion old ref tip is absent")
                    })?;
                    if !index.contains_ordinal_in_ref(&edit.ref_name, old_ordinal) {
                        return Err(super::corrupt(
                            "catalog visibility deletion old ref tip is outside its prior closure",
                        ));
                    }
                    index.remove_ref(&edit.ref_name);
                    continue;
                };
                let source_ordinal = edit
                    .source_oid
                    .map(|oid| {
                        ordinals.get(&oid).copied().ok_or_else(|| {
                            super::corrupt("catalog visibility source ref tip is absent")
                        })
                    })
                    .transpose()?;
                let new_ordinal = ordinals
                    .get(&edit.new_oid.ok_or_else(|| {
                        super::corrupt("catalog visibility new ref tip is absent")
                    })?)
                    .copied()
                    .ok_or_else(|| super::corrupt("catalog visibility new ref tip is absent"))?;
                let added = evidence
                    .added
                    .iter()
                    .map(|oid| {
                        let oid = super::decode_oid(oid)?;
                        ordinals.get(&oid).copied().ok_or_else(|| {
                            super::corrupt("catalog visibility added object is absent")
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let removed = evidence
                    .removed
                    .iter()
                    .map(|oid| {
                        let oid = super::decode_oid(oid)?;
                        ordinals.get(&oid).copied().ok_or_else(|| {
                            super::corrupt("catalog visibility removed object is absent")
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                if let Some(base_ref_name) = edit.base_ref_name.as_deref()
                    && !index.contains_ref(base_ref_name)
                {
                    return Ok(None);
                }
                index.apply_ordinal_edit(
                    edit.ref_name,
                    &evidence,
                    source_ordinal,
                    new_ordinal,
                    edit.base_ref_name.as_deref(),
                    added,
                    removed,
                )?;
            }
            if index.ref_count() != manifest.refs.len()
                || manifest.refs.keys().any(|name| !index.contains_ref(name))
            {
                return Err(super::corrupt(
                    "catalog visibility handoff refs do not match its manifest",
                ));
            }
            for (name, oid) in manifest.refs.iter().chain(manifest.peeled_refs.iter()) {
                let oid = super::decode_oid(oid)?;
                let ordinal = ordinals
                    .get(&oid)
                    .copied()
                    .ok_or_else(|| super::corrupt("catalog visibility manifest tip is absent"))?;
                if !index.contains_ordinal_in_ref(name, ordinal) {
                    return Err(super::corrupt(
                        "catalog visibility handoff does not contain a manifest tip",
                    ));
                }
            }
            index.validate()?;
            Ok(Some(index))
        }
        .await;
        let close = session.close().await;
        match (result, close) {
            (Ok(result), Ok(())) => Ok(result),
            (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(close_error)) => {
                tracing::warn!(
                    error = %close_error,
                    "Git catalog handoff reader close also failed after operation error"
                );
                Err(error)
            }
        }
    }

    #[cfg(feature = "remote-index")]
    async fn upload_catalog_index(
        store: &Store,
        router: &StoreLayout<Store>,
        index: &GitCatalogVisibilityIndex,
    ) -> Result<()> {
        let stored = GitVisibilityCatalogV1::from_catalog_index(index)?;
        let body = serde_json::to_vec(&stored).map_err(|error| {
            crate::error::MetadataError::Internal(format!(
                "catalog visibility index serialize: {error}"
            ))
        })?;
        if body.len() as u64 > MAX_GIT_VISIBILITY_INDEX_BYTES {
            return Err(crate::error::MetadataError::CorruptObject {
                path: router
                    .git_visibility_catalog_path(&index.git_validation_digest)
                    .as_ref()
                    .to_owned(),
                reason: format!(
                    "visibility index exceeds {} bytes",
                    MAX_GIT_VISIBILITY_INDEX_BYTES
                ),
            });
        }
        upload_visibility_body(
            store,
            &router.git_visibility_catalog_path(&index.git_validation_digest),
            Bytes::from(body),
        )
        .await
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
                Some(read) => read.index,
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
                    if evidence.new_oid != *new_oid {
                        return Err(crate::error::MetadataError::CorruptObject {
                            path: router
                                .git_visibility_edit_path(evidence_hash)
                                .as_ref()
                                .to_owned(),
                            reason: "visibility edit does not match its ref journal edit"
                                .to_owned(),
                        });
                    }
                    let base_ref = match (&edit.old_oid, &evidence.old_oid) {
                        (expected, actual) if expected == actual => None,
                        (None, Some(base_oid)) => {
                            // New-ref evidence may be a delta from an existing
                            // visible ref. An unbound local-only base stays on
                            // the owner's full repair path.
                            let Some(base_ref) = base
                                .refs
                                .iter()
                                .find(|(_, tip)| *tip == base_oid)
                                .map(|(name, _)| name.as_str())
                            else {
                                return Ok(None);
                            };
                            Some(base_ref)
                        }
                        _ => {
                            return Err(crate::error::MetadataError::CorruptObject {
                                path: router
                                    .git_visibility_edit_path(evidence_hash)
                                    .as_ref()
                                    .to_owned(),
                                reason: "visibility edit does not match its ref journal edit"
                                    .to_owned(),
                            });
                        }
                    };
                    match base_ref {
                        Some(base_ref) => index.apply_edit_with_base(
                            edit.ref_name.clone(),
                            &evidence,
                            Some(base_ref),
                        )?,
                        None => index.apply_edit(edit.ref_name.clone(), &evidence)?,
                    }
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
    GitVisibilityFormat, GitVisibilityRead, compact_journal_edits, digest_bound_available,
    ensure_catalog_bound, prepare_catalog_journal_edits, read, read_edit, read_for_manifest,
    read_with_format, upload_digest_bound_if_absent, upload_edit, upload_if_absent,
};

#[cfg(all(feature = "remote-index", feature = "storage"))]
pub use storage::{GitCatalogVisibilityRead, catalog_bound_available, read_catalog_with_format};

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
    fn shared_ref_history_uses_unique_dictionary_limit() {
        let objects = (0..101)
            .map(|value| {
                let mut oid = [0; 20];
                oid[..4].copy_from_slice(&(value as u32).to_be_bytes());
                oid
            })
            .collect::<Vec<_>>();
        let closure = GitVisibilityClosure::from_positions(
            (0..objects.len() as u32).collect(),
            objects.len(),
        )
        .expect("shared closure is valid");
        let refs: BTreeMap<String, GitVisibilityClosure> = (0..MAX_GIT_VISIBILITY_REFS)
            .map(|index| (format!("refs/heads/{index}"), closure.clone()))
            .collect();
        #[cfg(feature = "remote-index")]
        let catalog_refs = refs.clone();

        let index = GitVisibilityIndex::from_parts(
            GIT_VISIBILITY_INDEX_VERSION,
            4,
            "a".repeat(64),
            "c".repeat(64),
            objects,
            refs,
            BTreeMap::new(),
        )
        .expect("shared ref history fits the unique dictionary limit");

        assert_eq!(
            index.membership_count(),
            101 * MAX_GIT_VISIBILITY_REFS as u64
        );

        #[cfg(feature = "remote-index")]
        let catalog = GitCatalogVisibilityIndex::from_parts(
            4,
            "a".repeat(64),
            "c".repeat(64),
            "d".repeat(64),
            101,
            catalog_refs,
            BTreeMap::new(),
            BTreeMap::new(),
        )
        .expect("shared catalog history fits the unique dictionary limit");
        #[cfg(feature = "remote-index")]
        assert_eq!(
            catalog.membership_count(),
            101 * MAX_GIT_VISIBILITY_REFS as u64
        );
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
    fn new_ref_delta_can_reuse_an_existing_base_closure() {
        let base_oid = "a".repeat(40);
        let new_oid = "c".repeat(40);
        let mut index = GitVisibilityIndex::new(
            4,
            "b".repeat(64),
            "d".repeat(64),
            BTreeMap::from([(
                "refs/heads/main".to_owned(),
                vec![base_oid.clone(), "b".repeat(40)],
            )]),
        )
        .expect("valid base visibility index");
        let edit = GitVisibilityEdit::from_delta_objects(
            Some(base_oid),
            new_oid.clone(),
            vec![new_oid.clone()],
            Vec::new(),
        );

        index
            .apply_edit_with_base(
                "refs/heads/feature".to_owned(),
                &edit,
                Some("refs/heads/main"),
            )
            .expect("base closure delta applies");

        assert_eq!(
            index
                .objects_for_ref("refs/heads/feature")
                .expect("feature closure"),
            vec!["a".repeat(40), "b".repeat(40), "c".repeat(40)]
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

    #[cfg(feature = "remote-index")]
    #[test]
    fn catalog_fast_forward_rejects_missing_old_tip() {
        let mut index = GitCatalogVisibilityIndex::from_parts(
            4,
            "a".repeat(64),
            "b".repeat(64),
            "c".repeat(64),
            2,
            BTreeMap::from([(
                "refs/heads/main".to_owned(),
                GitVisibilityClosure::from_positions(vec![0], 2).expect("valid catalog closure"),
            )]),
            BTreeMap::new(),
            BTreeMap::new(),
        )
        .expect("valid catalog visibility index");
        let edit = GitVisibilityEdit::delta(
            Some("1".repeat(40)),
            "2".repeat(40),
            &BTreeSet::from(["1".repeat(40)]),
            &BTreeSet::from(["2".repeat(40)]),
        );

        assert!(
            index
                .apply_ordinal_edit(
                    "refs/heads/main".to_owned(),
                    &edit,
                    None,
                    1,
                    None,
                    vec![1],
                    vec![0]
                )
                .is_err()
        );
    }

    #[cfg(feature = "remote-index")]
    #[test]
    fn catalog_new_ref_delta_reuses_existing_base_closure() {
        let base_oid = "1".repeat(40);
        let new_oid = "2".repeat(40);
        let mut index = GitCatalogVisibilityIndex::from_parts(
            4,
            "a".repeat(64),
            "b".repeat(64),
            "c".repeat(64),
            2,
            BTreeMap::from([(
                "refs/heads/main".to_owned(),
                GitVisibilityClosure::from_positions(vec![0], 2).expect("valid catalog closure"),
            )]),
            BTreeMap::new(),
            BTreeMap::new(),
        )
        .expect("valid catalog visibility index");
        let edit = GitVisibilityEdit::from_delta_objects(
            Some(base_oid),
            new_oid,
            vec!["2".repeat(40)],
            Vec::new(),
        );

        index
            .apply_ordinal_edit(
                "refs/heads/feature".to_owned(),
                &edit,
                Some(0),
                1,
                Some("refs/heads/main"),
                vec![1],
                Vec::new(),
            )
            .expect("base closure delta applies");

        assert!(index.contains_ordinal_in_ref("refs/heads/feature", 0));
        assert!(index.contains_ordinal_in_ref("refs/heads/feature", 1));
    }

    #[cfg(feature = "remote-index")]
    #[test]
    fn catalog_add_rejects_an_existing_ref_closure() {
        let mut index = GitCatalogVisibilityIndex::from_parts(
            4,
            "a".repeat(64),
            "b".repeat(64),
            "c".repeat(64),
            2,
            BTreeMap::from([(
                "refs/heads/main".to_owned(),
                GitVisibilityClosure::from_positions(vec![0], 2).expect("valid catalog closure"),
            )]),
            BTreeMap::new(),
            BTreeMap::new(),
        )
        .expect("valid catalog visibility index");
        let edit =
            GitVisibilityEdit::from_replacement_objects(None, "2".repeat(40), vec!["2".repeat(40)]);

        assert!(
            index
                .apply_ordinal_edit(
                    "refs/heads/main".to_owned(),
                    &edit,
                    None,
                    1,
                    None,
                    vec![1],
                    Vec::new(),
                )
                .is_err()
        );
    }

    #[test]
    fn unrelated_ref_transition_bitmaps_follow_dictionary_growth() {
        let oid = |value: usize| format!("{value:040x}");
        let mut index = GitVisibilityIndex::new(
            4,
            "a".repeat(64),
            "b".repeat(64),
            BTreeMap::from([
                (
                    "refs/heads/main".to_owned(),
                    (0..64).map(oid).collect::<Vec<_>>(),
                ),
                (
                    "refs/heads/other".to_owned(),
                    (100..164).map(oid).collect::<Vec<_>>(),
                ),
            ]),
        )
        .expect("valid visibility index");

        let other_old = (100..164).map(oid).collect::<BTreeSet<_>>();
        let other_new = (100..196).map(oid).collect::<BTreeSet<_>>();
        index
            .apply_edit(
                "refs/heads/other".to_owned(),
                &GitVisibilityEdit::delta(Some(oid(163)), oid(195), &other_old, &other_new),
            )
            .expect("create a dense transition bitmap");

        let main_old = (0..64).map(oid).collect::<BTreeSet<_>>();
        let mut main_new = main_old.clone();
        main_new.insert(oid(196));
        index
            .apply_edit(
                "refs/heads/main".to_owned(),
                &GitVisibilityEdit::delta(Some(oid(63)), oid(196), &main_old, &main_new),
            )
            .expect("grow the shared object dictionary");

        index
            .bind_identity(5, &"c".repeat(64), &"d".repeat(64))
            .expect("all transition bitmaps must match the grown dictionary");
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
        assert_eq!(stored.format, GitVisibilityFormat::DigestV1);
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

    #[cfg(feature = "storage")]
    #[tokio::test]
    async fn digest_bound_availability_tracks_bounded_immutable_proof_presence() {
        use std::sync::Arc;

        use crab_storage::{Store, StoreLayout};
        use object_store::memory::InMemory;

        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        let index = GitVisibilityIndex::new(
            7,
            "a".repeat(64),
            "b".repeat(64),
            BTreeMap::from([("refs/heads/main".to_owned(), vec!["1".repeat(40)])]),
        )
        .expect("valid visibility index");

        assert!(
            !digest_bound_available(&store, &router, &index.git_validation_digest)
                .await
                .expect("missing proof is unavailable")
        );
        upload_digest_bound_if_absent(&store, &router, &index)
            .await
            .expect("upload proof");
        assert!(
            digest_bound_available(&store, &router, &index.git_validation_digest)
                .await
                .expect("uploaded proof is available")
        );
    }

    #[cfg(all(feature = "storage", feature = "remote-index"))]
    #[tokio::test]
    async fn digest_v1_proof_promotes_to_exact_catalog_ordinals() {
        use std::sync::Arc;

        use crab_storage::{Store, StoreLayout};
        use crab_xet::hash::MerkleHash;
        use object_store::memory::InMemory;
        use object_store::path::Path as ObjectPath;

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
            GitVisibilityFormat::DigestV1
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
        let catalog_identity = writer.catalog_identity().expect("catalog identity");
        writer.close().await.expect("close catalog writer");

        assert!(
            ensure_catalog_bound(&store, &router, &manifest)
                .await
                .expect("promote catalog proof")
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
            .expect("checkpoint marker is available")
        );
        let marker_path =
            ObjectPath::from(crate::git_object_locator::catalog_checkpoint_marker_path(
                router.repo_prefix(),
                catalog_identity.catalog_digest,
            ));
        store.delete(&marker_path).await.expect("remove marker");
        assert!(
            !catalog_bound_available(
                &store,
                &router,
                manifest.generation,
                &manifest.pack_index_hash,
                &manifest.git_validation_digest,
            )
            .await
            .expect("missing marker is a failed readiness check")
        );
        assert!(
            ensure_catalog_bound(&store, &router, &manifest)
                .await
                .expect("repair missing checkpoint marker")
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
        let promoted = read_with_format(&store, &router, 7, &pack_hash, &digest)
            .await
            .expect("read catalog proof");
        assert_eq!(promoted.format, GitVisibilityFormat::CatalogV1);
        assert_eq!(
            promoted.index.objects_for_ref("refs/heads/main"),
            index.objects_for_ref("refs/heads/main")
        );
        assert!(promoted.index.matches_manifest(&manifest));

        let lazy = read_catalog_with_format(&store, &router, 7, &pack_hash, &digest)
            .await
            .expect("read lazy catalog proof");
        assert_eq!(lazy.format, GitVisibilityFormat::CatalogV1);
        assert_eq!(lazy.index.object_count, 2);
        assert_eq!(
            lazy.index.catalog_identity().expect("catalog identity"),
            catalog_identity
        );
        assert_eq!(lazy.index.ordinals_for_refs(["refs/heads/main"]), [0, 1]);
        assert!(lazy.index.contains_ordinal_for_refs(["refs/heads/main"], 0));
        assert_eq!(
            lazy.index.object_count_for_refs(["refs/heads/main"]),
            promoted.index.object_count_for_refs(["refs/heads/main"])
        );

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
        assert_eq!(stored_history.format, GitVisibilityFormat::CatalogV1);
        let from = decode_oid(&"1".repeat(40)).expect("valid history base OID");
        let to = decode_oid(&"2".repeat(40)).expect("valid history target OID");
        assert_eq!(
            stored_history
                .index
                .incremental_objects("refs/heads/main", &to, &[from]),
            Some(Vec::new())
        );
        let lazy_history = read_catalog_with_format(
            &store,
            &router,
            history_manifest.generation,
            &history_manifest.pack_index_hash,
            &history_manifest.git_validation_digest,
        )
        .await
        .expect("read lazy catalog-bound visibility history");
        assert_eq!(
            lazy_history
                .index
                .incremental_ordinals("refs/heads/main", 0, &[1]),
            Some(Vec::new())
        );
    }

    #[cfg(all(feature = "storage", feature = "remote-index"))]
    #[tokio::test]
    async fn catalog_journal_handoff_applies_updates_and_new_ref_from_prior_tip() {
        use std::sync::Arc;

        use crab_storage::{Store, StoreLayout};
        use crab_xet::hash::MerkleHash;
        use object_store::memory::InMemory;

        use crate::git_object_locator::{
            GitLocatorCoverage, GitObjectLocation, GitObjectLocatorEntry, GitObjectLocatorWriter,
            GitPackLocatorRecord,
        };
        use crate::manifests::{BulkData, PackManifestEntry, compact_pack_index};
        use crate::ref_journal::RefJournalEdit;

        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        let base_pack_id = MerkleHash::from([3; 32]).to_string();
        let new_pack_id = MerkleHash::from([4; 32]).to_string();
        let base_packs = vec![PackManifestEntry {
            pack_id: base_pack_id.clone(),
            size: 256,
            content_hash: base_pack_id.clone(),
            ref_tips: vec!["1".repeat(40)],
            object_count: 2,
        }];
        let target_packs = [
            base_packs[0].clone(),
            PackManifestEntry {
                pack_id: new_pack_id.clone(),
                size: 512,
                content_hash: new_pack_id.clone(),
                ref_tips: vec!["5".repeat(40)],
                object_count: 4,
            },
        ];
        let (base_pack_index_hash, _, base_pack_index) =
            compact_pack_index(4, &base_packs).expect("base pack index");
        let (target_pack_index_hash, _, target_pack_index) =
            compact_pack_index(5, &target_packs).expect("target pack index");
        crate::manifest_store::upload_segmented_bulk(
            &store,
            &router,
            &BulkData {
                shard_index: crate::segmented::SegmentWrite::default(),
                pack_index: base_pack_index,
            },
        )
        .await
        .expect("upload base pack index");
        crate::manifest_store::upload_segmented_bulk(
            &store,
            &router,
            &BulkData {
                shard_index: crate::segmented::SegmentWrite::default(),
                pack_index: target_pack_index,
            },
        )
        .await
        .expect("upload target pack index");

        let mut base = Manifest::default_for_repo("refs/heads/main");
        base.generation = 4;
        base.pack_index_hash = base_pack_index_hash.clone();
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
        .expect("base visibility index");
        let base_pack_index_hash =
            MerkleHash::from_hex(&base_pack_index_hash).expect("base pack index hash");
        let mut writer =
            GitObjectLocatorWriter::open(Arc::clone(store.inner()), router.repo_prefix())
                .await
                .expect("open base catalog writer");
        let base_binding = writer
            .bind_packs(&[GitPackLocatorRecord {
                pack_id: MerkleHash::from_hex(&base_pack_id).expect("base pack ID"),
                committed_generation: 4,
                pack_index_hash: base_pack_index_hash,
                object_count: 2,
                pack_size: 256,
            }])
            .await
            .expect("bind base pack")[0];
        writer
            .write_locations(
                base_binding,
                &[
                    GitObjectLocatorEntry {
                        oid: [0x11; 20],
                        location: GitObjectLocation {
                            pack_offset: 12,
                            entry_len: 64,
                            crc32: 1,
                        },
                        metadata: Default::default(),
                    },
                    GitObjectLocatorEntry {
                        oid: [0x22; 20],
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
            .expect("write base catalog rows");
        writer
            .set_coverage(GitLocatorCoverage {
                generation: 4,
                pack_index_hash: base_pack_index_hash,
            })
            .await
            .expect("publish base catalog");
        writer.close().await.expect("close base catalog writer");
        upload_if_absent(&store, &router, &base_index)
            .await
            .expect("upload base catalog-bound proof");

        let mut target = base.clone();
        target.generation = 5;
        target.pack_index_hash = target_pack_index_hash.clone();
        target
            .refs
            .insert("refs/heads/main".to_owned(), "5".repeat(40));
        target
            .refs
            .insert("refs/heads/feature".to_owned(), "1".repeat(40));
        target.seal_git_validation();
        let target_pack_index_hash =
            MerkleHash::from_hex(&target_pack_index_hash).expect("target pack index hash");
        let mut writer =
            GitObjectLocatorWriter::open(Arc::clone(store.inner()), router.repo_prefix())
                .await
                .expect("open target catalog writer");
        let bindings = writer
            .bind_packs(&[
                GitPackLocatorRecord {
                    pack_id: MerkleHash::from_hex(&base_pack_id).expect("base pack ID"),
                    committed_generation: 4,
                    pack_index_hash: base_pack_index_hash,
                    object_count: 2,
                    pack_size: 256,
                },
                GitPackLocatorRecord {
                    pack_id: MerkleHash::from_hex(&new_pack_id).expect("new pack ID"),
                    committed_generation: 5,
                    pack_index_hash: target_pack_index_hash,
                    object_count: 4,
                    pack_size: 512,
                },
            ])
            .await
            .expect("bind target packs");
        writer
            .write_locations(
                bindings[1],
                &[
                    GitObjectLocatorEntry {
                        oid: [0x33; 20],
                        location: GitObjectLocation {
                            pack_offset: 12,
                            entry_len: 64,
                            crc32: 3,
                        },
                        metadata: Default::default(),
                    },
                    GitObjectLocatorEntry {
                        oid: [0x44; 20],
                        location: GitObjectLocation {
                            pack_offset: 76,
                            entry_len: 64,
                            crc32: 4,
                        },
                        metadata: Default::default(),
                    },
                    GitObjectLocatorEntry {
                        oid: [0x55; 20],
                        location: GitObjectLocation {
                            pack_offset: 140,
                            entry_len: 64,
                            crc32: 5,
                        },
                        metadata: Default::default(),
                    },
                    GitObjectLocatorEntry {
                        oid: [0x66; 20],
                        location: GitObjectLocation {
                            pack_offset: 204,
                            entry_len: 64,
                            crc32: 6,
                        },
                        metadata: Default::default(),
                    },
                ],
            )
            .await
            .expect("write target catalog rows");
        writer
            .set_coverage(GitLocatorCoverage {
                generation: 5,
                pack_index_hash: target_pack_index_hash,
            })
            .await
            .expect("publish target catalog");
        writer.close().await.expect("close target catalog writer");

        let first_evidence = GitVisibilityEdit::delta(
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
        let first_evidence_hash = upload_edit(&store, &router, &first_evidence)
            .await
            .expect("upload visibility evidence");
        let second_evidence = GitVisibilityEdit::delta(
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
                "6".repeat(40),
            ]),
        );
        let second_evidence_hash = upload_edit(&store, &router, &second_evidence)
            .await
            .expect("upload second visibility evidence");
        let branch_evidence = GitVisibilityEdit::from_delta_objects(
            Some("1".repeat(40)),
            "1".repeat(40),
            Vec::new(),
            Vec::new(),
        );
        let branch_evidence_hash = upload_edit(&store, &router, &branch_evidence)
            .await
            .expect("upload branch visibility evidence");
        let edits = vec![
            RefJournalEdit {
                ref_name: "refs/heads/feature".to_owned(),
                old_oid: None,
                new_oid: Some("1".repeat(40)),
                peeled_oid: None,
                lock_holder: None,
                visibility_evidence_hash: Some(branch_evidence_hash),
            },
            RefJournalEdit {
                ref_name: "refs/heads/main".to_owned(),
                old_oid: Some("1".repeat(40)),
                new_oid: Some("3".repeat(40)),
                peeled_oid: None,
                lock_holder: None,
                visibility_evidence_hash: Some(first_evidence_hash),
            },
            RefJournalEdit {
                ref_name: "refs/heads/main".to_owned(),
                old_oid: Some("3".repeat(40)),
                new_oid: Some("5".repeat(40)),
                peeled_oid: None,
                lock_holder: None,
                visibility_evidence_hash: Some(second_evidence_hash),
            },
        ];
        assert!(
            prepare_catalog_journal_edits(
                &store,
                &router,
                &base,
                &edits,
                &target.refs,
                target.generation,
                &target.pack_index_hash,
                &target.git_validation_digest,
            )
            .await
            .expect("prepare catalog visibility handoff")
        );
        assert!(
            ensure_catalog_bound(&store, &router, &target)
                .await
                .expect("apply catalog visibility handoff")
        );
        let lazy = read_catalog_with_format(
            &store,
            &router,
            target.generation,
            &target.pack_index_hash,
            &target.git_validation_digest,
        )
        .await
        .expect("read target catalog proof");
        assert_eq!(
            lazy.index.ordinals_for_refs(["refs/heads/main"]),
            vec![0, 1, 2, 3, 4, 5]
        );
        assert_eq!(
            lazy.index.incremental_ordinals("refs/heads/main", 4, &[0]),
            Some(vec![2, 3, 4, 5])
        );
        assert_eq!(
            lazy.index.ordinals_for_refs(["refs/heads/feature"]),
            vec![0, 1]
        );

        // The removed tip is absent from both the new ref and its replacement
        // evidence. Deletion must explicitly resolve it before checking closure.
        let mut after_delete = target.clone();
        after_delete.generation += 1;
        after_delete.refs = BTreeMap::from([
            ("refs/heads/feature".into(), "1".repeat(40)),
            ("refs/heads/kept".into(), "1".repeat(40)),
        ]);
        after_delete.seal_git_validation();
        let replacement = GitVisibilityEdit::from_replacement_objects(
            None,
            "1".repeat(40),
            vec!["1".repeat(40), "2".repeat(40)],
        );
        let evidence = upload_edit(&store, &router, &replacement).await.unwrap();
        let deletion = [
            RefJournalEdit {
                ref_name: "refs/heads/main".into(),
                old_oid: Some("5".repeat(40)),
                new_oid: None,
                peeled_oid: None,
                lock_holder: None,
                visibility_evidence_hash: None,
            },
            RefJournalEdit {
                ref_name: "refs/heads/kept".into(),
                old_oid: None,
                new_oid: Some("1".repeat(40)),
                peeled_oid: None,
                lock_holder: None,
                visibility_evidence_hash: Some(evidence),
            },
        ];
        assert!(
            prepare_catalog_journal_edits(
                &store,
                &router,
                &target,
                &deletion,
                &after_delete.refs,
                after_delete.generation,
                &after_delete.pack_index_hash,
                &after_delete.git_validation_digest,
            )
            .await
            .unwrap()
        );
        let mut writer =
            GitObjectLocatorWriter::open(Arc::clone(store.inner()), router.repo_prefix())
                .await
                .unwrap();
        writer
            .set_coverage(GitLocatorCoverage {
                generation: after_delete.generation,
                pack_index_hash: target_pack_index_hash,
            })
            .await
            .unwrap();
        writer.close().await.unwrap();
        assert!(
            ensure_catalog_bound(&store, &router, &after_delete)
                .await
                .unwrap()
        );
        let final_index = read_catalog_with_format(
            &store,
            &router,
            after_delete.generation,
            &after_delete.pack_index_hash,
            &after_delete.git_validation_digest,
        )
        .await
        .unwrap();
        assert_eq!(
            final_index.index.ordinals_for_refs(["refs/heads/kept"]),
            vec![0, 1]
        );
        assert_eq!(
            final_index.index.ordinals_for_refs(["refs/heads/feature"]),
            vec![0, 1]
        );
        assert!(!final_index.index.contains_ref("refs/heads/main"));
    }

    #[cfg(all(feature = "storage", feature = "remote-index"))]
    #[tokio::test]
    async fn catalog_deletion_resolves_tip_absent_from_surviving_refs() {
        use std::sync::Arc;

        use crab_storage::{Store, StoreLayout};
        use crab_xet::hash::MerkleHash;
        use object_store::memory::InMemory;

        use crate::git_object_locator::{
            GitLocatorCoverage, GitObjectLocation, GitObjectLocatorEntry, GitObjectLocatorWriter,
            GitPackLocatorRecord,
        };
        use crate::manifests::{BulkData, PackManifestEntry, compact_pack_index};
        use crate::ref_journal::RefJournalEdit;

        for deleted_ref in ["refs/heads/retired", "refs/tags/released"] {
            let store = Store::new(Arc::new(InMemory::new()));
            let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
            let pack_id = MerkleHash::from([3; 32]);
            let packs = [PackManifestEntry {
                pack_id: pack_id.to_string(),
                size: 256,
                content_hash: pack_id.to_string(),
                ref_tips: vec!["1".repeat(40), "2".repeat(40)],
                object_count: 2,
            }];
            let (pack_hash, _, pack_index) = compact_pack_index(4, &packs).unwrap();
            crate::manifest_store::upload_segmented_bulk(
                &store,
                &router,
                &BulkData {
                    shard_index: Default::default(),
                    pack_index,
                },
            )
            .await
            .unwrap();
            let mut base = Manifest::default_for_repo("refs/heads/main");
            base.generation = 4;
            base.pack_index_hash = pack_hash.clone();
            base.refs = BTreeMap::from([
                ("refs/heads/main".to_owned(), "1".repeat(40)),
                (deleted_ref.to_owned(), "2".repeat(40)),
            ]);
            base.seal_git_validation();
            let base_index = GitVisibilityIndex::new(
                base.generation,
                &base.pack_index_hash,
                &base.git_validation_digest,
                BTreeMap::from([
                    ("refs/heads/main".to_owned(), vec!["1".repeat(40)]),
                    (deleted_ref.to_owned(), vec!["1".repeat(40), "2".repeat(40)]),
                ]),
            )
            .unwrap();
            let pack_index_hash = MerkleHash::from_hex(&pack_hash).unwrap();
            let mut writer =
                GitObjectLocatorWriter::open(Arc::clone(store.inner()), router.repo_prefix())
                    .await
                    .unwrap();
            let binding = writer
                .bind_packs(&[GitPackLocatorRecord {
                    pack_id,
                    committed_generation: 4,
                    pack_index_hash,
                    object_count: 2,
                    pack_size: 256,
                }])
                .await
                .unwrap()[0];
            let entries = [0x11, 0x22]
                .into_iter()
                .enumerate()
                .map(|(index, byte)| GitObjectLocatorEntry {
                    oid: [byte; 20],
                    location: GitObjectLocation {
                        pack_offset: 12 + index as u64 * 64,
                        entry_len: 64,
                        crc32: index as u32 + 1,
                    },
                    metadata: Default::default(),
                })
                .collect::<Vec<_>>();
            writer.write_locations(binding, &entries).await.unwrap();
            writer
                .set_coverage(GitLocatorCoverage {
                    generation: 4,
                    pack_index_hash,
                })
                .await
                .unwrap();
            writer.close().await.unwrap();
            upload_if_absent(&store, &router, &base_index)
                .await
                .unwrap();

            let mut target = base.clone();
            target.generation = 5;
            target.refs.remove(deleted_ref);
            target.seal_git_validation();
            let mut writer =
                GitObjectLocatorWriter::open(Arc::clone(store.inner()), router.repo_prefix())
                    .await
                    .unwrap();
            writer
                .set_coverage(GitLocatorCoverage {
                    generation: 5,
                    pack_index_hash,
                })
                .await
                .unwrap();
            writer.close().await.unwrap();
            let edits = [RefJournalEdit {
                ref_name: deleted_ref.to_owned(),
                old_oid: Some("2".repeat(40)),
                new_oid: None,
                peeled_oid: None,
                lock_holder: None,
                visibility_evidence_hash: None,
            }];
            assert!(
                prepare_catalog_journal_edits(
                    &store,
                    &router,
                    &base,
                    &edits,
                    &target.refs,
                    target.generation,
                    &target.pack_index_hash,
                    &target.git_validation_digest,
                )
                .await
                .unwrap()
            );
            assert!(
                ensure_catalog_bound(&store, &router, &target)
                    .await
                    .unwrap()
            );
            let proof = read_catalog_with_format(
                &store,
                &router,
                target.generation,
                &target.pack_index_hash,
                &target.git_validation_digest,
            )
            .await
            .unwrap();
            assert_eq!(
                proof
                    .index
                    .ordinals_for_refs(["refs/heads/main", deleted_ref]),
                vec![0]
            );
        }
    }

    #[cfg(feature = "storage")]
    #[tokio::test]
    async fn digest_v1_deduplicates_shared_ref_history() {
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
    async fn digest_v1_rejects_closure_positions_outside_dictionary() {
        use std::sync::Arc;

        use bytes::Bytes;
        use crab_storage::{Store, StoreLayout};
        use object_store::memory::InMemory;

        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        let pack_hash = "a".repeat(64);
        let digest = "b".repeat(64);
        let malformed = serde_json::json!({
            "version": 1,
            "generation": 7,
            "pack_index_hash": pack_hash.clone(),
            "git_validation_digest": digest.clone(),
            "objects": ["1".repeat(40)],
            "refs": {"refs/heads/main": {"sparse": [1]}},
            "transitions": {},
            "incremental_history": {},
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
    async fn digest_v1_rejects_bitmap_bits_outside_dictionary() {
        use std::sync::Arc;

        use bytes::Bytes;
        use crab_storage::{Store, StoreLayout};
        use object_store::memory::InMemory;

        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        let pack_hash = "a".repeat(64);
        let digest = "b".repeat(64);
        let malformed = serde_json::json!({
            "version": 1,
            "generation": 7,
            "pack_index_hash": pack_hash.clone(),
            "git_validation_digest": digest.clone(),
            "objects": ["1".repeat(40)],
            "refs": {"refs/heads/main": {"bitmap": "gA"}},
            "transitions": {},
            "incremental_history": {},
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
    async fn non_v1_digest_proof_is_rejected() {
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
            "transitions": {},
            "incremental_history": {},
        });
        store
            .put(
                &router.git_visibility_path(&digest),
                Bytes::from(serde_json::to_vec(&stored).unwrap()),
            )
            .await
            .unwrap();

        assert!(
            read_with_format(&store, &router, 7, &pack_hash, &digest)
                .await
                .is_err()
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
            "version": 1,
            "generation": 7,
            "pack_index_hash": pack_hash.clone(),
            "git_validation_digest": digest.clone(),
            "objects": ["2".repeat(40), "1".repeat(40)],
            "refs": {"refs/heads/main": {"sparse": [0, 1]}},
            "transitions": {},
            "incremental_history": {},
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

        assert_eq!(current.format, GitVisibilityFormat::DigestV1);
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
    async fn new_ref_journal_delta_reuses_the_base_visibility_closure() {
        use std::sync::Arc;

        use crab_storage::{Store, StoreLayout};
        use object_store::memory::InMemory;

        use crate::ref_journal::RefJournalEdit;

        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        let base_oid = "1".repeat(40);
        let new_oid = "3".repeat(40);
        let mut base = Manifest::default_for_repo("refs/heads/main");
        base.generation = 4;
        base.pack_index_hash = "a".repeat(64);
        base.refs
            .insert("refs/heads/main".to_owned(), base_oid.clone());
        base.seal_git_validation();
        let base_index = GitVisibilityIndex::new(
            base.generation,
            &base.pack_index_hash,
            &base.git_validation_digest,
            BTreeMap::from([(
                "refs/heads/main".to_owned(),
                vec![base_oid.clone(), "2".repeat(40)],
            )]),
        )
        .expect("valid base visibility index");
        upload_if_absent(&store, &router, &base_index)
            .await
            .expect("upload base visibility index");

        let evidence = GitVisibilityEdit::from_delta_objects(
            Some(base_oid.clone()),
            new_oid.clone(),
            vec![new_oid.clone()],
            Vec::new(),
        );
        let evidence_hash = upload_edit(&store, &router, &evidence)
            .await
            .expect("upload new-ref visibility evidence");
        let edits = [RefJournalEdit {
            ref_name: "refs/heads/feature".to_owned(),
            old_oid: None,
            new_oid: Some(new_oid.clone()),
            peeled_oid: None,
            lock_holder: None,
            visibility_evidence_hash: Some(evidence_hash),
        }];
        let final_refs = BTreeMap::from([
            ("refs/heads/feature".to_owned(), new_oid.clone()),
            ("refs/heads/main".to_owned(), base_oid),
        ]);
        let mut target = base.clone();
        target.generation = 5;
        target.pack_index_hash = "b".repeat(64);
        target.refs.clone_from(&final_refs);
        target.seal_git_validation();

        let compacted = compact_journal_edits(
            &store,
            &router,
            &base,
            &edits,
            target.generation,
            &target.pack_index_hash,
            &target.git_validation_digest,
            &final_refs,
        )
        .await
        .expect("compact new-ref visibility evidence")
        .expect("base-anchored evidence should avoid a full rebuild");

        assert!(compacted.contains_hex_in_ref("refs/heads/main", &"1".repeat(40)));
        assert!(compacted.contains_hex_in_ref("refs/heads/feature", &new_oid));
        assert!(compacted.contains_hex_in_ref("refs/heads/feature", &"2".repeat(40)));
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
