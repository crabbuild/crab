//! Immutable physical placement indexes; callers retain ref visibility and origin verification.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use bytes::Bytes;
use crab_storage::{Store, StoreLayout};
use crab_xet::hash::MerkleHash;
use serde::{Deserialize, Serialize};

use super::{GitObjectLocation, GitObjectLocator, GitPackInventoryEntry, format};
use crate::error::Result;
use crate::manifests::Manifest;
use crate::pages::{Kind, PageCache, PageRef, PageWork, Rows, Writer};
use crate::validation::{corrupt_object, validate_content_hash};

mod codec;
mod reader;
use codec::*;

const MAX_DESCRIPTOR_BYTES: u64 = 16 * 1024;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Descriptor {
    version: u32,
    state_digest: String,
    generation: u64,
    pack_index_hash: String,
    git_validation_digest: String,
    body_hash: String,
    body_bytes: u64,
    objects: PageRef,
    offsets: PageRef,
    packs: PageRef,
    slots: PageRef,
}

/// Canonical construction of OID locations, physical offsets and complete pack coverage.
pub struct PagedGitObjectIndexWrite {
    descriptor: Descriptor,
    body: Bytes,
}

/// Immutable physical lookup data bound to a captured Git inventory and state.
#[derive(Debug)]
pub struct PagedGitObjectIndex {
    descriptor: Descriptor,
    descriptor_bytes: u64,
    path: object_store::path::Path,
    origin: Arc<dyn object_store::ObjectStore>,
    global_prefix: String,
    maximum: u64,
    cache: PageCache,
}

impl PagedGitObjectIndexWrite {
    /// Return encoded physical-page bytes for maintenance admission before upload.
    #[must_use]
    pub fn body_bytes(&self) -> u64 {
        self.descriptor.body_bytes
    }

    /// Build from every verified packed entry in the captured capsule inventory.
    ///
    /// Entries cover each pack without holes; this index does not assign catalog ordinals.
    pub fn build(
        state_digest: &str,
        manifest: &Manifest,
        inventory: &[GitPackInventoryEntry],
        entries: &[([u8; 20], GitObjectLocator)],
    ) -> Result<Self> {
        for hash in [
            state_digest,
            &manifest.pack_index_hash,
            &manifest.git_validation_digest,
        ] {
            validate_content_hash(hash, "object index binding", "Git object index")?;
        }
        let mut packs = BTreeMap::new();
        for pack in inventory {
            if packs.insert(pack.pack_id, *pack).is_some() {
                return bad("duplicate pack inventory");
            }
        }
        let mut manifest_packs = packs
            .values()
            .map(|pack| crate::manifests::PackManifestEntry {
                pack_id: pack.pack_id.to_string(),
                size: pack.pack_size,
                content_hash: pack.pack_id.to_string(),
                ref_tips: Vec::new(),
                object_count: pack.object_count,
            })
            .collect::<Vec<_>>();
        manifest_packs.sort_unstable_by(|left, right| left.pack_id.cmp(&right.pack_id));
        if crate::manifests::compact_pack_index(manifest.generation, &manifest_packs)?.0
            != manifest.pack_index_hash
        {
            return bad("physical inventory does not match captured pack identity");
        }
        let mut offsets = BTreeMap::new();
        let mut objects = BTreeMap::<[u8; 20], GitObjectLocator>::new();
        for (oid, input) in entries {
            let mut locator = *input;
            locator.ordinal = 0;
            let pack = packs
                .get(&locator.pack_id)
                .ok_or_else(|| corruption("location is outside inventory"))?;
            if !format::validate_location_for_pack(locator.location, pack.pack_size) {
                return bad("invalid physical packed-entry range");
            }
            if offsets
                .insert(
                    (locator.pack_id, locator.location.pack_offset),
                    (*oid, locator),
                )
                .is_some()
            {
                return bad("duplicate physical offset");
            }
            // Prefer self-contained encodings, then a stable physical identity.
            // Delta-base facts belong to an encoding and must not cross pack aliases.
            match objects.entry(*oid) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(locator);
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    let previous = entry.get_mut();
                    if previous
                        .metadata
                        .kind
                        .zip(locator.metadata.kind)
                        .is_some_and(|(left, right)| left != right)
                        || previous
                            .metadata
                            .logical_size
                            .zip(locator.metadata.logical_size)
                            .is_some_and(|(left, right)| left != right)
                    {
                        return bad("object aliases disagree about logical facts");
                    }
                    let kind = previous.metadata.kind.or(locator.metadata.kind);
                    let logical_size = previous
                        .metadata
                        .logical_size
                        .or(locator.metadata.logical_size);
                    if placement_rank(locator) < placement_rank(*previous) {
                        *previous = locator;
                    }
                    previous.metadata.kind = kind;
                    previous.metadata.logical_size = logical_size;
                }
            }
        }
        for (pack_id, pack) in &packs {
            let mut next = 12_u64;
            let mut count = 0_u64;
            for ((_, offset), (_, locator)) in offsets.range((*pack_id, 0)..=(*pack_id, u64::MAX)) {
                if *offset != next {
                    return bad("pack entries overlap or leave a gap");
                }
                next = next
                    .checked_add(locator.location.entry_len)
                    .ok_or_else(|| corruption("pack range overflows"))?;
                count += 1;
            }
            if count != pack.object_count || next.checked_add(20) != Some(pack.pack_size) {
                return bad("physical entries do not cover the declared pack");
            }
        }
        // Pack hashes occur once per dictionary, not in every object and offset row.
        // Slot assignment is canonical by encoded hash bytes, independent of input order.
        let mut ordered_packs = packs.values().copied().collect::<Vec<_>>();
        ordered_packs.sort_unstable_by_key(|pack| <[u8; 32]>::from(pack.pack_id));
        let mut slot_by_pack = BTreeMap::new();
        let mut pack_rows = Vec::new();
        let mut slot_rows = Vec::new();
        for (position, pack) in ordered_packs.into_iter().enumerate() {
            let slot = u32::try_from(position).map_err(|_| corruption("pack slots overflow"))?;
            slot_by_pack.insert(pack.pack_id, slot);
            pack_rows.push((
                <[u8; 32]>::from(pack.pack_id).to_vec(),
                encode_binding(slot, pack),
            ));
            slot_rows.push((slot.to_be_bytes().to_vec(), encode_pack(pack)));
        }
        let object_rows = objects
            .into_iter()
            .map(|(oid, locator)| {
                let slot = slot_by_pack
                    .get(&locator.pack_id)
                    .ok_or_else(|| corruption("primary pack has no slot"))?;
                Ok((oid.to_vec(), encode_pointer(*slot, locator)))
            })
            .collect::<Result<Rows>>()?;
        let offset_rows = offsets
            .into_iter()
            .map(|((pack_id, offset), (oid, locator))| {
                let slot = slot_by_pack
                    .get(&pack_id)
                    .ok_or_else(|| corruption("physical pack has no slot"))?;
                let mut value = oid.to_vec();
                value.extend_from_slice(&encode_location(locator.location, locator.metadata));
                Ok((offset_key(*slot, offset), value))
            })
            .collect::<Result<Rows>>()?;
        let (body, [objects, offsets, packs, slots]) =
            encode_trees(object_rows, offset_rows, pack_rows, slot_rows)?;
        let descriptor = Descriptor {
            version: 2,
            state_digest: state_digest.to_owned(),
            generation: manifest.generation,
            pack_index_hash: manifest.pack_index_hash.clone(),
            git_validation_digest: manifest.git_validation_digest.clone(),
            body_hash: blake3::hash(&body).to_hex().to_string(),
            body_bytes: body.len() as u64,
            objects,
            offsets,
            packs,
            slots,
        };
        descriptor.validate()?;
        Ok(Self {
            descriptor,
            body: body.into(),
        })
    }

    /// Upload immutable page bytes before their descriptor and return its content identity.
    pub async fn upload(&self, store: &Store, layout: &StoreLayout<Store>) -> Result<String> {
        crate::derived_index::upload(
            store,
            &body_path(layout, &self.descriptor.body_hash),
            &self.descriptor.body_hash,
            &self.body,
        )
        .await?;
        let bytes = serde_json::to_vec(&self.descriptor)
            .map_err(|source| crate::error::MetadataError::GitObjectIndexRecord { source })?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        crate::derived_index::upload(
            store,
            &layout.bulk_manifest_path("git-object-index", &hash),
            &hash,
            &bytes,
        )
        .await?;
        Ok(hash)
    }
}

impl Descriptor {
    fn validate(&self) -> Result<()> {
        if self.version != 2 {
            return bad("unsupported object index descriptor");
        }
        for hash in [
            &self.state_digest,
            &self.pack_index_hash,
            &self.git_validation_digest,
            &self.body_hash,
        ] {
            validate_content_hash(hash, "object index identity", "Git object index")?;
        }
        for (root, kind) in [
            (&self.objects, Kind::ObjectLocators),
            (&self.offsets, Kind::PackOffsets),
            (&self.packs, Kind::PackInventory),
            (&self.slots, Kind::PackSlots),
        ] {
            root.validate(kind, self.body_bytes)?;
        }
        if self.objects.count > self.offsets.count
            || self.packs.count > self.offsets.count
            || self.packs.count != self.slots.count
            || self.slots.first != 0u32.to_be_bytes()
            || self.slots.last != (self.slots.count - 1).to_be_bytes()
        {
            return bad("object index counts are inconsistent");
        }
        Ok(())
    }
}

fn encode_trees(
    mut objects: Rows,
    mut offsets: Rows,
    mut packs: Rows,
    mut slots: Rows,
) -> Result<(Vec<u8>, [PageRef; 4])> {
    for rows in [&mut objects, &mut offsets, &mut packs, &mut slots] {
        rows.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    }
    let mut builder = Writer::default();
    let roots = [
        builder.tree(Kind::ObjectLocators, objects)?,
        builder.tree(Kind::PackOffsets, offsets)?,
        builder.tree(Kind::PackInventory, packs)?,
        builder.tree(Kind::PackSlots, slots)?,
    ];
    Ok((builder.bytes, roots))
}
fn logical_conflict(left: super::GitObjectMetadata, right: super::GitObjectMetadata) -> bool {
    left.kind.zip(right.kind).is_some_and(|(a, b)| a != b)
        || left
            .logical_size
            .zip(right.logical_size)
            .is_some_and(|(a, b)| a != b)
}
fn placement_rank(locator: GitObjectLocator) -> (bool, MerkleHash, u64) {
    (
        locator.metadata.delta_base_oid.is_some(),
        locator.pack_id,
        locator.location.pack_offset,
    )
}
fn hash_bytes(hash: &str) -> Result<[u8; 32]> {
    blake3::Hash::from_hex(hash)
        .map(|hash| *hash.as_bytes())
        .map_err(|_| corruption("invalid object index hash"))
}
fn body_path(layout: &StoreLayout<Store>, hash: &str) -> object_store::path::Path {
    layout.repo_path(&format!("metadata/git-object-index/layers/{hash}.bin"))
}
fn corruption(reason: &str) -> crate::error::MetadataError {
    corrupt_object("Git object index", reason)
}
fn bad<T>(reason: &str) -> Result<T> {
    Err(corruption(reason))
}

#[cfg(test)]
mod tests;
