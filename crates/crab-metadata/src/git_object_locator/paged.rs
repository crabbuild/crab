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
        let object_rows = objects
            .into_iter()
            .map(|(oid, locator)| (oid.to_vec(), encode_locator(locator)))
            .collect();
        let offset_rows = offsets
            .into_iter()
            .map(|((pack_id, offset), (oid, locator))| {
                let mut value = oid.to_vec();
                value.extend_from_slice(&encode_location(locator.location, locator.metadata));
                (offset_key(pack_id, offset), value)
            })
            .collect();
        let pack_rows = packs
            .into_iter()
            .map(|(pack_id, pack)| {
                let mut value = pack.object_count.to_be_bytes().to_vec();
                value.extend_from_slice(&pack.pack_size.to_be_bytes());
                (<[u8; 32]>::from(pack_id).to_vec(), value)
            })
            .collect();
        let (body, objects, offsets, packs) = encode_trees(object_rows, offset_rows, pack_rows)?;
        let descriptor = Descriptor {
            version: 1,
            state_digest: state_digest.to_owned(),
            generation: manifest.generation,
            pack_index_hash: manifest.pack_index_hash.clone(),
            git_validation_digest: manifest.git_validation_digest.clone(),
            body_hash: blake3::hash(&body).to_hex().to_string(),
            body_bytes: body.len() as u64,
            objects,
            offsets,
            packs,
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

impl PagedGitObjectIndex {
    /// Capture an authenticated descriptor without loading its physical inventory or locations.
    ///
    /// Callers accept readiness only after complete verification; placement never authorizes refs.
    pub async fn load(
        store: &Store,
        layout: &StoreLayout<Store>,
        hash: &str,
        state_digest: &str,
        manifest: &Manifest,
        maximum: u64,
    ) -> Result<Self> {
        validate_content_hash(hash, "object index descriptor", "Git object index")?;
        let expected = hash_bytes(hash)?;
        let bytes = store
            .verify_bounded(
                &layout.bulk_manifest_path("git-object-index", hash),
                &expected,
                maximum.min(MAX_DESCRIPTOR_BYTES),
            )
            .await?;
        let descriptor: Descriptor = serde_json::from_slice(&bytes)
            .map_err(|source| crate::error::MetadataError::GitObjectIndexRecord { source })?;
        descriptor.validate()?;
        if descriptor.state_digest != state_digest
            || descriptor.generation != manifest.generation
            || descriptor.pack_index_hash != manifest.pack_index_hash
            || descriptor.git_validation_digest != manifest.git_validation_digest
        {
            return bad("object index does not match captured state");
        }
        let path = body_path(layout, &descriptor.body_hash);
        Ok(Self {
            descriptor,
            descriptor_bytes: bytes.len() as u64,
            path,
            origin: Arc::clone(layout.store().inner()),
            global_prefix: layout.global_prefix().to_owned(),
            maximum,
            cache: PageCache::default(),
        })
    }

    /// Check captured state and in-process origin before attaching lookup to a repository.
    #[must_use]
    pub fn matches_binding(
        &self,
        layout: &StoreLayout<Store>,
        state_digest: &str,
        manifest: &Manifest,
    ) -> bool {
        Arc::ptr_eq(&self.origin, layout.store().inner())
            && self.global_prefix == layout.global_prefix()
            && self.path == body_path(layout, &self.descriptor.body_hash)
            && self.descriptor.state_digest == state_digest
            && self.descriptor.generation == manifest.generation
            && self.descriptor.pack_index_hash == manifest.pack_index_hash
            && self.descriptor.git_validation_digest == manifest.git_validation_digest
    }

    /// Return the immutable body containing every page for retained-root GC.
    #[must_use]
    pub fn body_path(&self) -> &object_store::path::Path {
        &self.path
    }

    /// Resolve OIDs against the captured physical inventory, preserving duplicates and misses.
    ///
    /// The caller admits request/output counts and supplies an admitted store for the captured origin.
    pub async fn lookup(
        &self,
        store: &Store,
        oids: &[[u8; 20]],
        inventory: &HashMap<MerkleHash, GitPackInventoryEntry>,
    ) -> Result<Vec<Option<GitObjectLocator>>> {
        let mut work = self.work();
        let keys = oids.iter().map(|oid| oid.to_vec()).collect::<Vec<_>>();
        let values = self
            .lookup_rows(
                store,
                &self.descriptor.objects,
                Kind::ObjectLocators,
                &keys,
                &mut work,
            )
            .await?;
        let output = values
            .into_iter()
            .map(|value| value.map(|value| decode_locator(&value)).transpose())
            .collect::<Result<Vec<_>>>()?;
        let locators = output.iter().flatten().copied().collect::<Vec<_>>();
        let offset_keys = locators
            .iter()
            .map(|locator| offset_key(locator.pack_id, locator.location.pack_offset))
            .collect::<Vec<_>>();
        let physical = self
            .lookup_rows(
                store,
                &self.descriptor.offsets,
                Kind::PackOffsets,
                &offset_keys,
                &mut work,
            )
            .await?;
        let mut physical = physical.into_iter();
        for (oid, locator) in oids.iter().zip(&output) {
            let Some(locator) = locator else {
                continue;
            };
            let row = physical
                .next()
                .flatten()
                .ok_or_else(|| corruption("object location has no physical offset"))?;
            if row.len() != 64 || array::<20>(&row, 0)? != *oid {
                return bad("object and physical offset identities differ");
            }
            let (location, metadata) = decode_location(locator.location.pack_offset, &row[20..])?;
            if location != locator.location
                || metadata.delta_base_oid != locator.metadata.delta_base_oid
                || metadata
                    .kind
                    .zip(locator.metadata.kind)
                    .is_some_and(|(left, right)| left != right)
                || metadata
                    .logical_size
                    .zip(locator.metadata.logical_size)
                    .is_some_and(|(left, right)| left != right)
            {
                return bad("object and physical offset facts differ");
            }
        }
        self.validate_inventory(store, &locators, inventory, &mut work)
            .await?;
        Ok(output)
    }

    /// Resolve physical delta-base offsets without reading an entire reverse pack index.
    ///
    /// The caller admits request/output counts and supplies an admitted store for the captured origin.
    pub async fn oids_at_offsets(
        &self,
        store: &Store,
        offsets: &[(MerkleHash, u64)],
        inventory: &HashMap<MerkleHash, GitPackInventoryEntry>,
    ) -> Result<Vec<Option<[u8; 20]>>> {
        let mut work = self.work();
        let keys = offsets
            .iter()
            .map(|(pack, offset)| offset_key(*pack, *offset))
            .collect::<Vec<_>>();
        let values = self
            .lookup_rows(
                store,
                &self.descriptor.offsets,
                Kind::PackOffsets,
                &keys,
                &mut work,
            )
            .await?;
        let mut locators = Vec::new();
        let mut output = Vec::with_capacity(values.len());
        for ((pack_id, offset), value) in offsets.iter().zip(values) {
            let Some(value) = value else {
                output.push(None);
                continue;
            };
            if value.len() != 64 {
                return bad("invalid offset row length");
            }
            let oid = array(&value, 0)?;
            let (location, metadata) = decode_location(*offset, &value[20..])?;
            locators.push(GitObjectLocator {
                ordinal: 0,
                pack_id: *pack_id,
                location,
                metadata,
            });
            output.push(Some(oid));
        }
        self.validate_inventory(store, &locators, inventory, &mut work)
            .await?;
        let oid_keys = output
            .iter()
            .flatten()
            .map(|oid| oid.to_vec())
            .collect::<Vec<_>>();
        let primary = self
            .lookup_rows(
                store,
                &self.descriptor.objects,
                Kind::ObjectLocators,
                &oid_keys,
                &mut work,
            )
            .await?;
        if primary.iter().any(Option::is_none) {
            return bad("physical offset has no object identity");
        }
        let primary = primary
            .into_iter()
            .flatten()
            .map(|value| decode_locator(&value))
            .collect::<Result<Vec<_>>>()?;
        self.validate_inventory(store, &primary, inventory, &mut work)
            .await?;
        Ok(output)
    }

    /// Verify complete coverage and canonical bytes before publishing readiness.
    pub async fn verify_complete(
        &self,
        store: &Store,
        manifest: &Manifest,
        maximum: u64,
    ) -> Result<()> {
        let bytes = store
            .verify_bounded(
                &self.path,
                &hash_bytes(&self.descriptor.body_hash)?,
                maximum,
            )
            .await?;
        if bytes.len() as u64 != self.descriptor.body_bytes {
            return bad("object index body length mismatch");
        }
        let mut audit = BTreeMap::new();
        crate::pages::scan(
            &bytes,
            &self.descriptor.objects,
            Kind::ObjectLocators,
            self.path.as_ref(),
            &mut audit,
        )?;
        let packs = crate::pages::scan(
            &bytes,
            &self.descriptor.packs,
            Kind::PackInventory,
            self.path.as_ref(),
            &mut audit,
        )?;
        let inventory = packs
            .into_iter()
            .map(|(key, value)| {
                if value.len() != 16 {
                    return bad("invalid inventory row length");
                }
                Ok(GitPackInventoryEntry {
                    pack_id: MerkleHash::from(array(&key, 0)?),
                    object_count: u64::from_be_bytes(array(&value, 0)?),
                    pack_size: u64::from_be_bytes(array(&value, 8)?),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let offsets = crate::pages::scan(
            &bytes,
            &self.descriptor.offsets,
            Kind::PackOffsets,
            self.path.as_ref(),
            &mut audit,
        )?;
        let entries = offsets
            .into_iter()
            .map(|(key, value)| {
                if value.len() != 64 {
                    return bad("invalid offset row length");
                }
                let offset = u64::from_be_bytes(array(&key, 32)?);
                let (location, metadata) = decode_location(offset, &value[20..])?;
                Ok((
                    array(&value, 0)?,
                    GitObjectLocator {
                        ordinal: 0,
                        pack_id: MerkleHash::from(array(&key, 0)?),
                        location,
                        metadata,
                    },
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let rebuilt = PagedGitObjectIndexWrite::build(
            &self.descriptor.state_digest,
            manifest,
            &inventory,
            &entries,
        )?;
        if rebuilt.descriptor != self.descriptor || rebuilt.body.as_ref() != bytes.as_ref() {
            return bad("object index coverage or canonical placement mismatch");
        }
        Ok(())
    }

    fn work(&self) -> PageWork {
        PageWork {
            maximum: self.maximum,
            bytes: self.descriptor_bytes,
            seen: BTreeSet::new(),
        }
    }

    async fn lookup_rows(
        &self,
        store: &Store,
        root: &PageRef,
        kind: Kind,
        keys: &[Vec<u8>],
        work: &mut PageWork,
    ) -> Result<Vec<Option<Vec<u8>>>> {
        self.cache
            .lookup(
                store,
                &self.path,
                0,
                self.descriptor.body_bytes,
                root,
                kind,
                keys,
                work,
            )
            .await
    }

    async fn validate_inventory(
        &self,
        store: &Store,
        locators: &[GitObjectLocator],
        inventory: &HashMap<MerkleHash, GitPackInventoryEntry>,
        work: &mut PageWork,
    ) -> Result<()> {
        let pack_ids = locators
            .iter()
            .map(|locator| locator.pack_id)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let keys = pack_ids
            .iter()
            .map(|pack| <[u8; 32]>::from(*pack).to_vec())
            .collect::<Vec<_>>();
        let values = self
            .lookup_rows(
                store,
                &self.descriptor.packs,
                Kind::PackInventory,
                &keys,
                work,
            )
            .await?;
        for (pack_id, value) in pack_ids.into_iter().zip(values) {
            let value = value.ok_or_else(|| corruption("required pack binding is absent"))?;
            if value.len() != 16 {
                return bad("invalid inventory row length");
            }
            let expected = inventory
                .get(&pack_id)
                .ok_or_else(|| corruption("pack is outside captured inventory"))?;
            if u64::from_be_bytes(array(&value, 0)?) != expected.object_count
                || u64::from_be_bytes(array(&value, 8)?) != expected.pack_size
            {
                return bad("pack binding differs from captured inventory");
            }
        }
        for locator in locators {
            if !inventory.get(&locator.pack_id).is_some_and(|pack| {
                format::validate_location_for_pack(locator.location, pack.pack_size)
            }) {
                return bad("location escapes captured pack");
            }
        }
        Ok(())
    }
}

impl Descriptor {
    fn validate(&self) -> Result<()> {
        if self.version != 1 {
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
        ] {
            root.validate(kind, self.body_bytes)?;
        }
        if self.objects.count > self.offsets.count || self.packs.count > self.offsets.count {
            return bad("object index counts are inconsistent");
        }
        Ok(())
    }
}

fn encode_trees(
    mut objects: Rows,
    mut offsets: Rows,
    mut packs: Rows,
) -> Result<(Vec<u8>, PageRef, PageRef, PageRef)> {
    for rows in [&mut objects, &mut offsets, &mut packs] {
        rows.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    }
    let mut builder = Writer::default();
    let objects = builder.tree(Kind::ObjectLocators, objects)?;
    let offsets = builder.tree(Kind::PackOffsets, offsets)?;
    let packs = builder.tree(Kind::PackInventory, packs)?;
    Ok((builder.bytes, objects, offsets, packs))
}
fn placement_rank(locator: GitObjectLocator) -> (bool, MerkleHash, u64) {
    (
        locator.metadata.delta_base_oid.is_some(),
        locator.pack_id,
        locator.location.pack_offset,
    )
}
fn offset_key(pack: MerkleHash, offset: u64) -> Vec<u8> {
    let mut key = <[u8; 32]>::from(pack).to_vec();
    key.extend_from_slice(&offset.to_be_bytes());
    key
}
fn encode_location(location: GitObjectLocation, metadata: super::GitObjectMetadata) -> Vec<u8> {
    let mut value = location.entry_len.to_be_bytes().to_vec();
    value.extend_from_slice(&location.crc32.to_be_bytes());
    value.extend_from_slice(&format::encode_object_metadata(metadata));
    value
}
fn encode_locator(locator: GitObjectLocator) -> Vec<u8> {
    let mut value = <[u8; 32]>::from(locator.pack_id).to_vec();
    value.extend_from_slice(&locator.location.pack_offset.to_be_bytes());
    value.extend_from_slice(&encode_location(locator.location, locator.metadata));
    value
}
fn decode_location(
    offset: u64,
    bytes: &[u8],
) -> Result<(GitObjectLocation, super::GitObjectMetadata)> {
    if bytes.len() != 44 {
        return bad("invalid location row length");
    }
    let metadata = format::decode_object_metadata(&bytes[12..])
        .ok_or_else(|| corruption("invalid object metadata"))?;
    Ok((
        GitObjectLocation {
            pack_offset: offset,
            entry_len: u64::from_be_bytes(array(bytes, 0)?),
            crc32: u32::from_be_bytes(array(bytes, 8)?),
        },
        metadata,
    ))
}
fn decode_locator(bytes: &[u8]) -> Result<GitObjectLocator> {
    if bytes.len() != 84 {
        return bad("invalid locator row length");
    }
    let (location, metadata) =
        decode_location(u64::from_be_bytes(array(bytes, 32)?), &bytes[40..])?;
    Ok(GitObjectLocator {
        ordinal: 0,
        pack_id: MerkleHash::from(array(bytes, 0)?),
        location,
        metadata,
    })
}
fn array<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N]> {
    bytes
        .get(offset..offset + N)
        .and_then(|value| value.try_into().ok())
        .ok_or_else(|| corruption("truncated object index value"))
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
