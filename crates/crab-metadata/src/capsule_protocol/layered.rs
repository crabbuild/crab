use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::capsule_protocol::{CapsuleGitPack, CapsuleRun, CapsuleSectionKind, PointerCatalog};
use crate::error::{MetadataError, Result};
use crate::git_visibility::{GitVisibilityIndex, GitVisibilityOid, GitVisibilityOrdinalTransition};
use crate::validation::{validate_content_hash, validate_sha1};

const CHECKPOINT_MAGIC: &[u8; 8] = b"CRBCKP05";
const CHECKPOINT_VERSION: u32 = 5;
const LAYER_MAGIC: &[u8; 8] = b"CRBPKL01";
const LAYER_VERSION: u32 = 1;
const TRAILER_BYTES: usize = 8 + 32 + 8;
const MAX_CHECKPOINT_FOOTER_BYTES: usize = 8 * 1024 * 1024;
const MAX_LAYER_FOOTER_BYTES: usize = 8 * 1024 * 1024;
const MAX_PHYSICAL_SOURCES: usize = 64;
const MAX_SOURCE_MEMBERS: usize = 512;
const LAYERED_VISIBILITY_MAGIC: &[u8; 8] = b"CRBVORD1";
const LAYERED_VISIBILITY_VERSION: u32 = 2;
const MAX_LAYERED_VISIBILITY_BYTES: usize = 128 * 1024 * 1024;
const MAX_LAYERED_VISIBILITY_REFS: usize = 100_000;
const MAX_LAYERED_VISIBILITY_TRANSITIONS: usize = 1_000_000;
const VISIBILITY_OBJECT_SET_DIGEST_DOMAIN: &[u8] = b"crab layered visibility object set\0";

/// Compute the stable digest used by the cold-clone object-set proof.
#[must_use]
pub fn visibility_object_set_digest(objects: &[GitVisibilityOid]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(VISIBILITY_OBJECT_SET_DIGEST_DOMAIN);
    hasher.update(&(objects.len() as u64).to_be_bytes());
    for object in objects {
        hasher.update(object);
    }
    hasher.finalize().to_hex().to_string()
}

/// Compact, source-set-bound visibility proof for the layered checkpoint.
///
/// The dictionary is encoded once as raw SHA-1 bytes. Ref closures and
/// incremental transitions use canonical sparse ordinals or bitmaps, so the
/// warm fetch path does not download repeated hexadecimal OIDs for every ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayeredVisibilitySnapshot {
    catalog_digest: String,
    objects: Vec<GitVisibilityOid>,
    member_admission: Option<Vec<LayeredObjectMember>>,
    refs: BTreeMap<String, Vec<u32>>,
    transitions: BTreeMap<String, Vec<GitVisibilityOrdinalTransition>>,
    incremental_history: BTreeMap<String, Vec<GitVisibilityOrdinalTransition>>,
}

/// Authenticated physical source/member admission for one visibility ordinal.
///
/// The pair is bound to the ordered source catalog digest carried by the
/// enclosing checkpoint. It lets readers select the exact immutable pack
/// before loading any unrelated pack index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayeredObjectMember {
    source_index: u16,
    member_index: u16,
}

impl LayeredObjectMember {
    /// Bind one visibility ordinal to a physical source member.
    pub const fn new(source_index: u16, member_index: u16) -> Self {
        Self {
            source_index,
            member_index,
        }
    }

    /// Return the zero-based source ordinal.
    #[must_use]
    pub const fn source_index(self) -> u16 {
        self.source_index
    }

    /// Return the zero-based member ordinal within that source.
    #[must_use]
    pub const fn member_index(self) -> u16 {
        self.member_index
    }
}

impl LayeredVisibilitySnapshot {
    /// Capture a compact proof bound to the exact ordered pack-source catalog.
    pub fn from_index(index: &GitVisibilityIndex, catalog_digest: &str) -> Result<Self> {
        validate_content_hash(
            catalog_digest,
            "layered visibility catalog digest",
            "capsule-protocol layered checkpoint",
        )?;
        let (objects, _, refs, transitions, incremental_history) = index.ordinal_parts()?;
        let snapshot = Self {
            catalog_digest: catalog_digest.to_owned(),
            objects,
            member_admission: None,
            refs,
            transitions,
            incremental_history,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    /// Capture a compact proof plus an authenticated source/member join.
    pub fn from_index_with_member_admission(
        index: &GitVisibilityIndex,
        catalog_digest: &str,
        member_admission: Vec<LayeredObjectMember>,
    ) -> Result<Self> {
        validate_content_hash(
            catalog_digest,
            "layered visibility catalog digest",
            "capsule-protocol layered checkpoint",
        )?;
        let (objects, remap, refs, transitions, incremental_history) = index.ordinal_parts()?;
        if member_admission.len() != objects.len() {
            return Err(contract_error(
                "layered visibility member admission count does not match its dictionary",
            ));
        }
        let member_admission = remap_member_admission(member_admission, &remap)?;
        let snapshot = Self {
            catalog_digest: catalog_digest.to_owned(),
            objects,
            member_admission: Some(member_admission),
            refs,
            transitions,
            incremental_history,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    /// Return the immutable source-catalog identity bound to this proof.
    #[must_use]
    pub fn catalog_digest(&self) -> &str {
        &self.catalog_digest
    }

    /// Return the compact visibility object dictionary.
    #[must_use]
    pub fn objects(&self) -> &[GitVisibilityOid] {
        &self.objects
    }

    /// Return the authenticated source/member join, when this checkpoint has one.
    #[must_use]
    pub fn member_admission(&self) -> Option<&[LayeredObjectMember]> {
        self.member_admission.as_deref()
    }

    /// Return the stable digest of the complete visibility object dictionary.
    ///
    /// The digest is stored in the checkpoint footer so a cold reader can
    /// compare the downloaded Git index without fetching this potentially
    /// large visibility section.
    #[must_use]
    pub fn object_set_digest(&self) -> String {
        visibility_object_set_digest(&self.objects)
    }

    /// Encode the proof in its canonical bounded binary representation.
    pub fn encode(&self) -> Result<Bytes> {
        self.validate()?;
        let mut writer = BinaryWriter::default();
        writer.bytes(LAYERED_VISIBILITY_MAGIC);
        writer.u32(LAYERED_VISIBILITY_VERSION);
        let digest = blake3::Hash::from_hex(&self.catalog_digest)
            .map_err(|_| contract_error("layered visibility catalog digest is invalid"))?;
        writer.bytes(digest.as_bytes());
        writer.u64(
            u64::try_from(self.objects.len())
                .map_err(|_| contract_error("layered visibility object count overflows"))?,
        );
        for object in &self.objects {
            writer.bytes(object);
        }
        match self.member_admission.as_ref() {
            Some(admission) => {
                writer.bytes(&[1]);
                for member in admission {
                    writer.u16(member.source_index);
                    writer.u16(member.member_index);
                }
            }
            None => writer.bytes(&[0]),
        }
        let object_count = self.objects.len();
        encode_closure_map(&mut writer, &self.refs, object_count)?;
        encode_transition_map(&mut writer, &self.transitions, object_count)?;
        encode_transition_map(&mut writer, &self.incremental_history, object_count)?;
        if writer.bytes.len() > MAX_LAYERED_VISIBILITY_BYTES {
            return Err(contract_error(
                "layered visibility snapshot exceeds its size bound",
            ));
        }
        Ok(Bytes::from(writer.bytes))
    }

    /// Decode and validate one canonical compact proof.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_LAYERED_VISIBILITY_BYTES {
            return Err(corrupt(
                "layered visibility snapshot exceeds its size bound",
            ));
        }
        let mut reader = BinaryReader::new(bytes);
        if reader.bytes(LAYERED_VISIBILITY_MAGIC.len())? != LAYERED_VISIBILITY_MAGIC {
            return Err(corrupt("layered visibility snapshot magic is invalid"));
        }
        if reader.u32()? != LAYERED_VISIBILITY_VERSION {
            return Err(corrupt(
                "layered visibility snapshot version is unsupported",
            ));
        }
        let digest = reader.bytes(32)?;
        let catalog_digest = encode_hex(digest);
        let object_count = reader.u64()?;
        let object_count = usize::try_from(object_count)
            .map_err(|_| corrupt("layered visibility object count cannot be represented"))?;
        if object_count > crate::git_visibility::MAX_GIT_VISIBILITY_OBJECTS as usize {
            return Err(corrupt("layered visibility object dictionary is too large"));
        }
        let mut objects = Vec::with_capacity(object_count);
        for _ in 0..object_count {
            let bytes = reader.bytes(20)?;
            let object = bytes
                .try_into()
                .map_err(|_| corrupt("layered visibility object ID is truncated"))?;
            objects.push(object);
        }
        let member_admission = match reader.bytes(1)?[0] {
            0 => None,
            1 => {
                let mut admission = Vec::with_capacity(object_count);
                for _ in 0..object_count {
                    admission.push(LayeredObjectMember::new(reader.u16()?, reader.u16()?));
                }
                Some(admission)
            }
            _ => {
                return Err(corrupt(
                    "layered visibility member admission flag is invalid",
                ));
            }
        };
        let refs = decode_closure_map(&mut reader, object_count)?;
        let transitions = decode_transition_map(&mut reader, object_count)?;
        let incremental_history = decode_transition_map(&mut reader, object_count)?;
        reader.finish()?;
        let snapshot = Self {
            catalog_digest,
            objects,
            member_admission,
            refs,
            transitions,
            incremental_history,
        };
        snapshot.validate()?;
        if snapshot.encode()?.as_ref() != bytes {
            return Err(corrupt("layered visibility snapshot is not canonical"));
        }
        Ok(snapshot)
    }

    /// Restore the materialized visibility index after checking its catalog binding.
    pub fn to_index(
        &self,
        generation: u64,
        pack_index_hash: &str,
        git_validation_digest: &str,
        expected_catalog_digest: &str,
    ) -> Result<GitVisibilityIndex> {
        if self.catalog_digest != expected_catalog_digest {
            return Err(corrupt(
                "layered visibility proof does not match its pack-source catalog",
            ));
        }
        GitVisibilityIndex::from_ordinal_parts(
            generation,
            pack_index_hash,
            git_validation_digest,
            self.objects.clone(),
            self.refs.clone(),
            self.transitions.clone(),
            self.incremental_history.clone(),
        )
    }

    fn validate(&self) -> Result<()> {
        validate_content_hash(
            &self.catalog_digest,
            "layered visibility catalog digest",
            "capsule-protocol layered checkpoint",
        )?;
        if self.refs.len() > MAX_LAYERED_VISIBILITY_REFS
            || self.objects.len() as u64 > crate::git_visibility::MAX_GIT_VISIBILITY_OBJECTS
        {
            return Err(corrupt("layered visibility proof exceeds its count bound"));
        }
        let object_count = self.objects.len();
        let mut seen = BTreeSet::new();
        for object in &self.objects {
            if !seen.insert(object) {
                return Err(corrupt("layered visibility dictionary repeats an object"));
            }
        }
        if self.objects.windows(2).any(|window| window[0] >= window[1]) {
            return Err(corrupt(
                "layered visibility dictionary is not in canonical order",
            ));
        }
        if let Some(admission) = &self.member_admission {
            if admission.len() != object_count {
                return Err(corrupt(
                    "layered visibility member admission count does not match its dictionary",
                ));
            }
            if admission.iter().any(|member| {
                usize::from(member.source_index) >= MAX_PHYSICAL_SOURCES
                    || usize::from(member.member_index) >= MAX_SOURCE_MEMBERS
            }) {
                return Err(corrupt(
                    "layered visibility member admission is outside its bounds",
                ));
            }
        }
        for (name, closure) in &self.refs {
            validate_visibility_ref(name)?;
            validate_positions(closure, object_count)?;
        }
        validate_transition_map(&self.transitions, &self.refs, object_count, 64)?;
        validate_transition_map(
            &self.incremental_history,
            &self.refs,
            object_count,
            MAX_LAYERED_VISIBILITY_TRANSITIONS,
        )?;
        Ok(())
    }
}

fn remap_member_admission(
    member_admission: Vec<LayeredObjectMember>,
    remap: &[u32],
) -> Result<Vec<LayeredObjectMember>> {
    if member_admission.len() != remap.len() {
        return Err(contract_error(
            "layered visibility member admission count does not match its dictionary",
        ));
    }
    let mut canonical = vec![None; remap.len()];
    for (original, member) in member_admission.into_iter().enumerate() {
        let canonical_position = usize::try_from(remap[original])
            .map_err(|_| contract_error("layered visibility ordinal remap overflows"))?;
        let slot = canonical
            .get_mut(canonical_position)
            .ok_or_else(|| contract_error("layered visibility ordinal remap is invalid"))?;
        if slot.replace(member).is_some() {
            return Err(contract_error(
                "layered visibility ordinal remap repeats a position",
            ));
        }
    }
    canonical
        .into_iter()
        .map(|member| {
            member.ok_or_else(|| contract_error("layered visibility ordinal remap is incomplete"))
        })
        .collect()
}

#[derive(Default)]
struct BinaryWriter {
    bytes: Vec<u8>,
}

impl BinaryWriter {
    fn bytes(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }
}

struct BinaryReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> BinaryReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn bytes(&mut self, length: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| corrupt("layered visibility reader overflowed"))?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| corrupt("layered visibility snapshot is truncated"))?;
        self.offset = end;
        Ok(bytes)
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.bytes(4)?.try_into().map_err(
            |_| corrupt("layered visibility integer is truncated"),
        )?))
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.bytes(2)?.try_into().map_err(
            |_| corrupt("layered visibility integer is truncated"),
        )?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.bytes(8)?.try_into().map_err(
            |_| corrupt("layered visibility integer is truncated"),
        )?))
    }

    fn finish(&self) -> Result<()> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(corrupt("layered visibility snapshot has trailing bytes"))
        }
    }
}

fn encode_closure_map(
    writer: &mut BinaryWriter,
    closures: &BTreeMap<String, Vec<u32>>,
    object_count: usize,
) -> Result<()> {
    writer.u32(
        u32::try_from(closures.len())
            .map_err(|_| contract_error("layered visibility ref count overflows"))?,
    );
    for (name, positions) in closures {
        encode_name(writer, name)?;
        encode_positions(writer, positions, object_count)?;
    }
    Ok(())
}

fn decode_closure_map(
    reader: &mut BinaryReader<'_>,
    object_count: usize,
) -> Result<BTreeMap<String, Vec<u32>>> {
    let count = usize::try_from(reader.u32()?)
        .map_err(|_| corrupt("layered visibility ref count cannot be represented"))?;
    if count > MAX_LAYERED_VISIBILITY_REFS {
        return Err(corrupt("layered visibility ref count exceeds its bound"));
    }
    let mut closures = BTreeMap::new();
    for _ in 0..count {
        let name = decode_name(reader)?;
        let positions = decode_positions(reader, object_count)?;
        if closures.insert(name, positions).is_some() {
            return Err(corrupt("layered visibility proof repeats a ref"));
        }
    }
    Ok(closures)
}

fn encode_transition_map(
    writer: &mut BinaryWriter,
    transitions: &BTreeMap<String, Vec<GitVisibilityOrdinalTransition>>,
    object_count: usize,
) -> Result<()> {
    writer.u32(
        u32::try_from(transitions.len())
            .map_err(|_| contract_error("layered visibility transition ref count overflows"))?,
    );
    for (name, entries) in transitions {
        encode_name(writer, name)?;
        writer.u32(
            u32::try_from(entries.len())
                .map_err(|_| contract_error("layered visibility transition count overflows"))?,
        );
        for entry in entries {
            writer.u32(entry.from_ordinal);
            writer.u32(entry.to_ordinal);
            encode_positions(writer, &entry.objects, object_count)?;
        }
    }
    Ok(())
}

fn decode_transition_map(
    reader: &mut BinaryReader<'_>,
    object_count: usize,
) -> Result<BTreeMap<String, Vec<GitVisibilityOrdinalTransition>>> {
    let ref_count = usize::try_from(reader.u32()?)
        .map_err(|_| corrupt("layered visibility transition ref count cannot be represented"))?;
    if ref_count > MAX_LAYERED_VISIBILITY_REFS {
        return Err(corrupt(
            "layered visibility transition ref count exceeds its bound",
        ));
    }
    let mut output = BTreeMap::new();
    for _ in 0..ref_count {
        let name = decode_name(reader)?;
        let count = usize::try_from(reader.u32()?)
            .map_err(|_| corrupt("layered visibility transition count cannot be represented"))?;
        if count > MAX_LAYERED_VISIBILITY_TRANSITIONS {
            return Err(corrupt(
                "layered visibility transition count exceeds its bound",
            ));
        }
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            entries.push(GitVisibilityOrdinalTransition {
                from_ordinal: reader.u32()?,
                to_ordinal: reader.u32()?,
                objects: decode_positions(reader, object_count)?,
            });
        }
        if output.insert(name, entries).is_some() {
            return Err(corrupt("layered visibility proof repeats transition refs"));
        }
    }
    Ok(output)
}

fn encode_name(writer: &mut BinaryWriter, name: &str) -> Result<()> {
    let bytes = name.as_bytes();
    writer.u32(
        u32::try_from(bytes.len())
            .map_err(|_| contract_error("layered visibility ref name is too long"))?,
    );
    writer.bytes(bytes);
    Ok(())
}

fn decode_name(reader: &mut BinaryReader<'_>) -> Result<String> {
    let length = usize::try_from(reader.u32()?)
        .map_err(|_| corrupt("layered visibility ref name length cannot be represented"))?;
    if length == 0 || length > 4 * 1024 {
        return Err(corrupt("layered visibility ref name length is invalid"));
    }
    let name = std::str::from_utf8(reader.bytes(length)?)
        .map_err(|_| corrupt("layered visibility ref name is not UTF-8"))?;
    validate_visibility_ref(name)?;
    Ok(name.to_owned())
}

fn encode_positions(
    writer: &mut BinaryWriter,
    positions: &[u32],
    object_count: usize,
) -> Result<()> {
    validate_positions_for_encoding(positions)?;
    if positions.iter().any(|position| {
        usize::try_from(*position)
            .ok()
            .is_none_or(|position| position >= object_count)
    }) {
        return Err(contract_error(
            "layered visibility position is outside its dictionary",
        ));
    }
    let bitmap_len = object_count.div_ceil(8);
    let sparse_bytes = positions
        .len()
        .checked_mul(4)
        .ok_or_else(|| contract_error("layered visibility closure size overflows"))?;
    if bitmap_len == 0 || sparse_bytes <= bitmap_len {
        writer.bytes(&[0]);
        writer.u32(
            u32::try_from(positions.len())
                .map_err(|_| contract_error("layered visibility closure count overflows"))?,
        );
        for position in positions {
            writer.u32(*position);
        }
    } else {
        let mut bitmap = vec![0_u8; bitmap_len];
        for position in positions {
            let position = usize::try_from(*position)
                .map_err(|_| contract_error("layered visibility position overflows"))?;
            bitmap[position / 8] |= 1 << (position % 8);
        }
        writer.bytes(&[1]);
        writer.u32(
            u32::try_from(bitmap.len())
                .map_err(|_| contract_error("layered visibility bitmap length overflows"))?,
        );
        writer.bytes(&bitmap);
    }
    Ok(())
}

fn decode_positions(reader: &mut BinaryReader<'_>, object_count: usize) -> Result<Vec<u32>> {
    let kind = reader.bytes(1)?[0];
    let count = usize::try_from(reader.u32()?)
        .map_err(|_| corrupt("layered visibility closure count cannot be represented"))?;
    let positions = match kind {
        0 => {
            if count > object_count {
                return Err(corrupt("layered visibility sparse closure is too large"));
            }
            let mut positions = Vec::with_capacity(count);
            for _ in 0..count {
                positions.push(reader.u32()?);
            }
            positions
        }
        1 => {
            if count != object_count.div_ceil(8) {
                return Err(corrupt(
                    "layered visibility bitmap length does not match its dictionary",
                ));
            }
            let bitmap = reader.bytes(count)?;
            if let Some(last) = bitmap.last()
                && !object_count.is_multiple_of(8)
                && last >> (object_count % 8) != 0
            {
                return Err(corrupt(
                    "layered visibility bitmap sets an out-of-range position",
                ));
            }
            let mut positions = Vec::new();
            for (byte_index, byte) in bitmap.iter().enumerate() {
                for bit_index in 0..8 {
                    if byte & (1 << bit_index) != 0
                        && let Some(position) = byte_index
                            .checked_mul(8)
                            .and_then(|value| value.checked_add(bit_index))
                            .and_then(|value| u32::try_from(value).ok())
                    {
                        positions.push(position);
                    }
                }
            }
            positions
        }
        _ => return Err(corrupt("layered visibility closure encoding is invalid")),
    };
    validate_positions(&positions, object_count)?;
    Ok(positions)
}

fn validate_positions(positions: &[u32], object_count: usize) -> Result<()> {
    validate_positions_for_encoding(positions)?;
    if positions.iter().any(|position| {
        usize::try_from(*position)
            .ok()
            .is_none_or(|position| position >= object_count)
    }) {
        return Err(corrupt(
            "layered visibility position is outside its dictionary",
        ));
    }
    Ok(())
}

fn validate_positions_for_encoding(positions: &[u32]) -> Result<()> {
    if positions.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(corrupt(
            "layered visibility positions must be sorted and deduplicated",
        ));
    }
    Ok(())
}

fn validate_transition_map(
    transitions: &BTreeMap<String, Vec<GitVisibilityOrdinalTransition>>,
    refs: &BTreeMap<String, Vec<u32>>,
    object_count: usize,
    maximum: usize,
) -> Result<()> {
    for (name, entries) in transitions {
        let reference = refs
            .get(name)
            .ok_or_else(|| corrupt("layered visibility transition ref is absent"))?;
        if entries.len() > maximum {
            return Err(corrupt(
                "layered visibility transition count exceeds its bound",
            ));
        }
        for entry in entries {
            if usize::try_from(entry.from_ordinal)
                .ok()
                .is_none_or(|value| value >= object_count)
                || usize::try_from(entry.to_ordinal)
                    .ok()
                    .is_none_or(|value| value >= object_count)
                || reference.binary_search(&entry.from_ordinal).is_err()
                || reference.binary_search(&entry.to_ordinal).is_err()
            {
                return Err(corrupt(
                    "layered visibility transition endpoints are invalid",
                ));
            }
            validate_positions(&entry.objects, object_count)?;
            if entry
                .objects
                .iter()
                .any(|position| reference.binary_search(position).is_err())
            {
                return Err(corrupt(
                    "layered visibility transition escapes its ref closure",
                ));
            }
        }
    }
    Ok(())
}

fn validate_visibility_ref(name: &str) -> Result<()> {
    if name.is_empty() || name.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(corrupt("layered visibility proof contains an invalid ref"));
    }
    Ok(())
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

/// The immutable object containing one bounded pack inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackSourceKind {
    /// A capsule run whose members retain their original capsule bytes.
    CapsuleRun,
    /// A standalone geometric roll-up layer.
    PackLayer,
}

/// One authenticated byte range inside a pack source object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackRange {
    offset: u64,
    length: u64,
    blake3: String,
}

impl PackRange {
    /// Create one non-empty range with its content commitment.
    pub fn new(offset: u64, bytes: &[u8]) -> Result<Self> {
        let length = u64::try_from(bytes.len())
            .map_err(|_| contract_error("pack range length cannot be represented"))?;
        if length == 0 {
            return Err(contract_error("pack ranges must be non-empty"));
        }
        Ok(Self {
            offset,
            length,
            blake3: blake3::hash(bytes).to_hex().to_string(),
        })
    }

    pub(crate) fn from_parts(offset: u64, length: u64, blake3: impl Into<String>) -> Result<Self> {
        if length == 0 {
            return Err(contract_error("pack ranges must be non-empty"));
        }
        let range = Self {
            offset,
            length,
            blake3: blake3.into(),
        };
        validate_content_hash(
            &range.blake3,
            "pack range hash",
            "capsule-protocol layered pack",
        )?;
        Ok(range)
    }

    /// Return the absolute source-object offset.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// Return the range length.
    #[must_use]
    pub const fn length(&self) -> u64 {
        self.length
    }

    /// Return the BLAKE3 commitment of the range.
    #[must_use]
    pub fn blake3(&self) -> &str {
        &self.blake3
    }

    fn end(&self) -> Result<u64> {
        self.offset
            .checked_add(self.length)
            .ok_or_else(|| corrupt("pack range overflowed"))
    }
}

/// Immutable Git pack and sidecar evidence inside one physical source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackMemberDescriptor {
    pack: PackRange,
    index: PackRange,
    reverse_index: PackRange,
    locator: PackRange,
    git_checksum: String,
    object_count: u64,
    external_delta_bases: Vec<String>,
}

impl PackMemberDescriptor {
    /// Bind one complete Git pack and its authenticated sidecars.
    #[expect(
        clippy::too_many_arguments,
        reason = "one descriptor carries each independently authenticated pack section"
    )]
    pub fn new(
        pack: PackRange,
        index: PackRange,
        reverse_index: PackRange,
        locator: PackRange,
        git_checksum: impl Into<String>,
        object_count: u64,
        external_delta_bases: Vec<String>,
    ) -> Result<Self> {
        let descriptor = Self {
            pack,
            index,
            reverse_index,
            locator,
            git_checksum: git_checksum.into(),
            object_count,
            external_delta_bases,
        };
        descriptor.validate(u64::MAX, 0)?;
        Ok(descriptor)
    }

    /// Construct a member whose sections are laid out in a standalone layer.
    pub fn from_pack(pack: &CapsuleGitPack) -> Result<(Self, Vec<u8>)> {
        let mut body = Vec::new();
        let pack_range = append_range(&mut body, pack.pack_bytes())?;
        let index_range = append_range(&mut body, pack.index_bytes())?;
        let reverse_range = append_range(&mut body, pack.reverse_index_bytes())?;
        let locator_range = append_range(&mut body, pack.locator_bytes())?;
        let descriptor = Self::new(
            pack_range,
            index_range,
            reverse_range,
            locator_range,
            pack.git_checksum(),
            pack.object_count(),
            pack.external_delta_bases().to_vec(),
        )?;
        Ok((descriptor, body))
    }

    /// Return the pack body range.
    #[must_use]
    pub const fn pack(&self) -> &PackRange {
        &self.pack
    }

    /// Return the index range.
    #[must_use]
    pub const fn index(&self) -> &PackRange {
        &self.index
    }

    /// Return the reverse-index range.
    #[must_use]
    pub const fn reverse_index(&self) -> &PackRange {
        &self.reverse_index
    }

    /// Return the object-locator range.
    #[must_use]
    pub const fn locator(&self) -> &PackRange {
        &self.locator
    }

    /// Return the Git pack checksum.
    #[must_use]
    pub fn git_checksum(&self) -> &str {
        &self.git_checksum
    }

    /// Return the number of objects proven by the index.
    #[must_use]
    pub const fn object_count(&self) -> u64 {
        self.object_count
    }

    /// Return external `REF_DELTA` bases required by this member.
    #[must_use]
    pub fn external_delta_bases(&self) -> &[String] {
        &self.external_delta_bases
    }

    fn validate(&self, source_size: u64, control_offset: u64) -> Result<()> {
        validate_sha1(
            &self.git_checksum,
            "pack member Git checksum",
            "capsule-protocol layered pack",
        )?;
        let dependency_count = u64::try_from(self.external_delta_bases.len())
            .map_err(|_| corrupt("pack member dependency count cannot be represented"))?;
        if self.object_count == 0 || dependency_count > self.object_count {
            return Err(corrupt("pack member object or dependency count is invalid"));
        }
        let ranges = [&self.pack, &self.index, &self.reverse_index, &self.locator];
        let mut previous_end = 0_u64;
        for range in ranges {
            validate_content_hash(
                range.blake3(),
                "pack member range hash",
                "capsule-protocol layered pack",
            )?;
            let end = range.end()?;
            if range.offset() < control_offset || range.offset() < previous_end || end > source_size
            {
                return Err(corrupt("pack member range is outside its source"));
            }
            previous_end = end;
        }
        for base in &self.external_delta_bases {
            validate_sha1(
                base,
                "pack member external delta base",
                "capsule-protocol layered pack",
            )?;
        }
        Ok(())
    }
}

/// One physical immutable source in a layered checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackSourceDescriptor {
    kind: PackSourceKind,
    object_hash: String,
    object_size: u64,
    control_offset: u64,
    control_size: u64,
    control_hash: String,
    members: Vec<PackMemberDescriptor>,
}

impl PackSourceDescriptor {
    /// Bind one immutable source and its member directory.
    #[expect(
        clippy::too_many_arguments,
        reason = "the descriptor authenticates the complete immutable source boundary"
    )]
    pub fn new(
        kind: PackSourceKind,
        object_hash: impl Into<String>,
        object_size: u64,
        control_offset: u64,
        control_size: u64,
        control_hash: impl Into<String>,
        members: Vec<PackMemberDescriptor>,
    ) -> Result<Self> {
        let descriptor = Self {
            kind,
            object_hash: object_hash.into(),
            object_size,
            control_offset,
            control_size,
            control_hash: control_hash.into(),
            members,
        };
        descriptor.validate()?;
        Ok(descriptor)
    }

    /// Build a source descriptor over the pack members nested in one capsule run.
    pub fn from_capsule_run(run: &CapsuleRun) -> Result<Self> {
        PackSourceDescriptor::new(
            PackSourceKind::CapsuleRun,
            run.hash(),
            run.bytes().len() as u64,
            run.control_offset(),
            run.control_size(),
            run.footer_hash(),
            run.git_packs().to_vec(),
        )
    }

    /// Return the source kind.
    #[must_use]
    pub const fn kind(&self) -> PackSourceKind {
        self.kind
    }

    /// Return the immutable source object hash.
    #[must_use]
    pub fn object_hash(&self) -> &str {
        &self.object_hash
    }

    /// Return the immutable source object size.
    #[must_use]
    pub const fn object_size(&self) -> u64 {
        self.object_size
    }

    /// Return the source control offset.
    #[must_use]
    pub const fn control_offset(&self) -> u64 {
        self.control_offset
    }

    /// Return the source control size.
    #[must_use]
    pub const fn control_size(&self) -> u64 {
        self.control_size
    }

    /// Return the control suffix commitment.
    #[must_use]
    pub fn control_hash(&self) -> &str {
        &self.control_hash
    }

    /// Return every authenticated pack member in source order.
    #[must_use]
    pub fn members(&self) -> &[PackMemberDescriptor] {
        &self.members
    }

    /// Return the aggregate compressed member bytes.
    pub fn compressed_bytes(&self) -> Result<u64> {
        self.members.iter().try_fold(0_u64, |total, member| {
            total
                .checked_add(member.pack().length())
                .ok_or_else(|| corrupt("pack source byte count overflowed"))
        })
    }

    /// Return the aggregate object count.
    pub fn object_count(&self) -> Result<u64> {
        self.members.iter().try_fold(0_u64, |total, member| {
            total
                .checked_add(member.object_count())
                .ok_or_else(|| corrupt("pack source object count overflowed"))
        })
    }

    fn validate(&self) -> Result<()> {
        validate_content_hash(
            &self.object_hash,
            "pack source object hash",
            "capsule-protocol layered pack",
        )?;
        validate_content_hash(
            &self.control_hash,
            "pack source control hash",
            "capsule-protocol layered pack",
        )?;
        if self.object_size == 0
            || self.control_size == 0
            || self.members.is_empty()
            || self.members.len() > MAX_SOURCE_MEMBERS
            || self.control_offset >= self.object_size
            || self.control_offset.checked_add(self.control_size) != Some(self.object_size)
        {
            return Err(corrupt("pack source bounds or member count are invalid"));
        }
        let mut previous_member_end = 0_u64;
        let mut member_pack_hashes = std::collections::BTreeSet::new();
        for member in &self.members {
            member.validate(self.object_size, 0)?;
            let ranges = [
                member.pack(),
                member.index(),
                member.reverse_index(),
                member.locator(),
            ];
            if member.pack().offset() < previous_member_end {
                return Err(corrupt("pack source members overlap or are out of order"));
            }
            if ranges
                .iter()
                .any(|range| range.end().is_ok_and(|end| end > self.control_offset))
            {
                return Err(corrupt("pack member overlaps its control suffix"));
            }
            if !member_pack_hashes.insert(member.pack().blake3()) {
                return Err(corrupt("pack source repeats a pack member identity"));
            }
            previous_member_end = member
                .locator()
                .end()
                .map_err(|_| corrupt("pack source member range overflowed"))?;
        }
        Ok(())
    }
}

/// Compute the immutable catalog identity for an ordered layered source set.
///
/// The digest covers every source/member identity and range commitment. It is
/// intentionally independent of checkpoint generation so unchanged members
/// retain the same ordinal namespace across later checkpoints.
pub fn source_catalog_digest(sources: &[PackSourceDescriptor]) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.v2.layered-source-catalog.v1\0");
    for source in sources {
        hasher.update(match source.kind {
            PackSourceKind::CapsuleRun => b"run\0" as &[u8],
            PackSourceKind::PackLayer => b"layer\0",
        });
        hash_text(&mut hasher, source.object_hash());
        hasher.update(&source.object_size.to_be_bytes());
        hasher.update(&source.control_offset.to_be_bytes());
        hasher.update(&source.control_size.to_be_bytes());
        hash_text(&mut hasher, source.control_hash());
        for member in &source.members {
            hash_range(&mut hasher, member.pack());
            hash_range(&mut hasher, member.index());
            hash_range(&mut hasher, member.reverse_index());
            hash_range(&mut hasher, member.locator());
            hash_text(&mut hasher, member.git_checksum());
            hasher.update(&member.object_count.to_be_bytes());
            for base in &member.external_delta_bases {
                hash_text(&mut hasher, base);
            }
            hasher.update(&[0xff]);
        }
        hasher.update(&[0xfe]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn hash_text(hasher: &mut blake3::Hasher, value: &str) {
    hasher.update(&(value.len() as u64).to_be_bytes());
    hasher.update(value.as_bytes());
}

fn hash_range(hasher: &mut blake3::Hasher, range: &PackRange) {
    hasher.update(&range.offset.to_be_bytes());
    hasher.update(&range.length.to_be_bytes());
    hash_text(hasher, &range.blake3);
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LayerFooter {
    version: u32,
    control_offset: u64,
    member: PackMemberDescriptor,
}

/// One standalone immutable geometric pack layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackLayer {
    bytes: Bytes,
    hash: String,
    footer_hash: String,
    footer: LayerFooter,
}

impl PackLayer {
    /// Build and authenticate one standalone layer from a complete Git pack.
    pub fn build(pack: &CapsuleGitPack) -> Result<Self> {
        let (member, mut body) = PackMemberDescriptor::from_pack(pack)?;
        let control_offset = u64::try_from(body.len())
            .map_err(|_| contract_error("pack layer control offset cannot be represented"))?;
        let footer = LayerFooter {
            version: LAYER_VERSION,
            control_offset,
            member,
        };
        let footer_bytes = serde_json::to_vec(&footer).map_err(|source| {
            MetadataError::Internal(format!("pack layer footer serialization failed: {source}"))
        })?;
        if footer_bytes.len() > MAX_LAYER_FOOTER_BYTES {
            return Err(contract_error("pack layer footer exceeds its size bound"));
        }
        body.extend_from_slice(&footer_bytes);
        body.extend_from_slice(&(footer_bytes.len() as u64).to_be_bytes());
        body.extend_from_slice(blake3::hash(&footer_bytes).as_bytes());
        body.extend_from_slice(LAYER_MAGIC);
        Self::decode(Bytes::from(body))
    }

    /// Decode and authenticate one complete layer object.
    pub fn decode(bytes: Bytes) -> Result<Self> {
        let (footer, footer_start, footer_hash) = decode_layer_footer(&bytes)?;
        validate_layer_footer(&footer, footer_start as u64, bytes.len() as u64)?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        Ok(Self {
            bytes,
            hash,
            footer_hash,
            footer,
        })
    }

    /// Return the complete encoded layer object.
    #[must_use]
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// Return the immutable layer object hash.
    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Return the authenticated footer hash.
    #[must_use]
    pub fn footer_hash(&self) -> &str {
        &self.footer_hash
    }

    /// Return the authenticated pack member descriptor.
    #[must_use]
    pub fn member(&self) -> &PackMemberDescriptor {
        &self.footer.member
    }

    /// Build the source descriptor used by a layered checkpoint.
    pub fn source_descriptor(&self) -> Result<PackSourceDescriptor> {
        PackSourceDescriptor::new(
            PackSourceKind::PackLayer,
            self.hash.clone(),
            self.bytes.len() as u64,
            self.footer.control_offset,
            self.bytes.len() as u64 - self.footer.control_offset,
            self.footer_hash.clone(),
            vec![self.footer.member.clone()],
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointFooter {
    version: u32,
    covered_generation: u64,
    covered_root_digest: String,
    control_offset: u64,
    sections: Vec<LayerSectionLocation>,
    sources: Vec<PackSourceDescriptor>,
    #[serde(default)]
    cold_clone_object_set_digest: Option<String>,
    #[serde(default)]
    cold_clone_object_count: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LayerSectionLocation {
    kind: CapsuleSectionKind,
    offset: u64,
    length: u64,
    blake3: String,
}

/// Metadata-only authenticated v2 checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayeredCheckpoint {
    bytes: Bytes,
    hash: String,
    footer_hash: String,
    footer: CheckpointFooter,
    object_size: u64,
    control_offset: u64,
    control_only: bool,
}

impl LayeredCheckpoint {
    /// Build a metadata-only checkpoint from immutable pack sources.
    pub fn build(
        covered_generation: u64,
        covered_root_digest: &str,
        sources: Vec<PackSourceDescriptor>,
        pointer_catalog: PointerCatalog,
        visibility: Option<crate::capsule_protocol::CapsuleVisibilitySnapshot>,
    ) -> Result<Self> {
        let visibility = match visibility {
            Some(value) => Some((CapsuleSectionKind::VisibilitySnapshot, value.encode()?)),
            None => None,
        };
        Self::build_inner(
            covered_generation,
            covered_root_digest,
            sources,
            pointer_catalog,
            visibility,
            None,
            None,
        )
    }

    /// Build a metadata-only checkpoint with the compact ordinal proof.
    pub fn build_with_ordinal_visibility(
        covered_generation: u64,
        covered_root_digest: &str,
        sources: Vec<PackSourceDescriptor>,
        pointer_catalog: PointerCatalog,
        visibility: Option<LayeredVisibilitySnapshot>,
    ) -> Result<Self> {
        let cold_clone_object_set_digest = visibility
            .as_ref()
            .map(LayeredVisibilitySnapshot::object_set_digest);
        let cold_clone_object_count = visibility
            .as_ref()
            .map(|snapshot| {
                u64::try_from(snapshot.objects().len())
                    .map_err(|_| contract_error("layered visibility object count overflows"))
            })
            .transpose()?;
        let visibility = match visibility {
            Some(value) => Some((
                CapsuleSectionKind::VisibilityOrdinalSnapshot,
                value.encode()?,
            )),
            None => None,
        };
        Self::build_inner(
            covered_generation,
            covered_root_digest,
            sources,
            pointer_catalog,
            visibility,
            cold_clone_object_set_digest,
            cold_clone_object_count,
        )
    }

    fn build_inner(
        covered_generation: u64,
        covered_root_digest: &str,
        sources: Vec<PackSourceDescriptor>,
        pointer_catalog: PointerCatalog,
        visibility: Option<(CapsuleSectionKind, Bytes)>,
        cold_clone_object_set_digest: Option<String>,
        cold_clone_object_count: Option<u64>,
    ) -> Result<Self> {
        validate_content_hash(
            covered_root_digest,
            "layered checkpoint covered root digest",
            "capsule-protocol layered checkpoint",
        )?;
        if sources.is_empty() || sources.len() > MAX_PHYSICAL_SOURCES {
            return Err(contract_error(
                "layered checkpoint source count is out of bounds",
            ));
        }
        let mut body = Vec::new();
        let mut sections = Vec::new();
        if !pointer_catalog.is_empty() {
            append_section(
                &mut body,
                &mut sections,
                CapsuleSectionKind::CatalogDelta,
                pointer_catalog.encode()?,
            )?;
        }
        if let Some((kind, visibility)) = visibility {
            append_section(&mut body, &mut sections, kind, visibility)?;
        }
        let control_offset = 0_u64;
        let footer = CheckpointFooter {
            version: CHECKPOINT_VERSION,
            covered_generation,
            covered_root_digest: covered_root_digest.to_owned(),
            control_offset,
            sections,
            sources,
            cold_clone_object_set_digest,
            cold_clone_object_count,
        };
        validate_source_inventory(&footer.sources)?;
        for source in &footer.sources {
            source.validate()?;
        }
        let footer_bytes = serde_json::to_vec(&footer).map_err(|source| {
            MetadataError::Internal(format!(
                "layered checkpoint footer serialization failed: {source}"
            ))
        })?;
        if footer_bytes.len() > MAX_CHECKPOINT_FOOTER_BYTES {
            return Err(contract_error(
                "layered checkpoint footer exceeds its size bound",
            ));
        }
        body.extend_from_slice(&footer_bytes);
        body.extend_from_slice(&(footer_bytes.len() as u64).to_be_bytes());
        body.extend_from_slice(blake3::hash(&footer_bytes).as_bytes());
        body.extend_from_slice(CHECKPOINT_MAGIC);
        Self::decode(Bytes::from(body))
    }

    /// Decode and authenticate one layered checkpoint object.
    pub fn decode(bytes: Bytes) -> Result<Self> {
        let (footer, footer_start, footer_hash) = decode_checkpoint_footer(&bytes)?;
        validate_checkpoint_footer(&footer, footer_start as u64, bytes.len() as u64)?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let object_size = u64::try_from(bytes.len())
            .map_err(|_| corrupt("layered checkpoint size cannot be represented"))?;
        let control_offset = u64::try_from(footer_start)
            .map_err(|_| corrupt("layered checkpoint control offset cannot be represented"))?;
        Ok(Self {
            bytes,
            hash,
            footer_hash,
            footer,
            object_size,
            control_offset,
            control_only: false,
        })
    }

    /// Decode only the authenticated footer from a range-addressable object.
    ///
    /// The caller supplies the complete object identity and the absolute byte
    /// offset of the fetched footer range. This verifies the footer hash and
    /// every source descriptor without reading the visibility/catalog body.
    pub fn decode_control(
        bytes: Bytes,
        object_size: u64,
        control_offset: u64,
        expected_hash: &str,
        expected_footer_hash: &str,
    ) -> Result<Self> {
        if bytes.is_empty() {
            return Err(corrupt("layered checkpoint control range is empty"));
        }
        validate_content_hash(
            expected_hash,
            "layered checkpoint hash",
            "capsule-protocol layered checkpoint",
        )?;
        validate_content_hash(
            expected_footer_hash,
            "layered checkpoint footer hash",
            "capsule-protocol layered checkpoint",
        )?;
        let (footer, relative_start, footer_hash) = decode_checkpoint_footer(&bytes)?;
        if footer_hash != expected_footer_hash {
            return Err(corrupt(
                "layered checkpoint control footer hash does not match its pointer",
            ));
        }
        let footer_start = control_offset
            .checked_add(
                u64::try_from(relative_start)
                    .map_err(|_| corrupt("layered checkpoint footer offset overflows"))?,
            )
            .ok_or_else(|| corrupt("layered checkpoint footer offset overflows"))?;
        let control_end = control_offset
            .checked_add(
                u64::try_from(bytes.len())
                    .map_err(|_| corrupt("layered checkpoint control size overflows"))?,
            )
            .ok_or_else(|| corrupt("layered checkpoint control range overflows"))?;
        if control_end != object_size || footer_start < control_offset {
            return Err(corrupt(
                "layered checkpoint control range does not end at the object",
            ));
        }
        validate_checkpoint_footer(&footer, footer_start, object_size)?;
        Ok(Self {
            bytes: Bytes::new(),
            hash: expected_hash.to_owned(),
            footer_hash,
            footer,
            object_size,
            control_offset: footer_start,
            control_only: true,
        })
    }

    /// Return the complete checkpoint bytes.
    #[must_use]
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// Return the absolute offset of the authenticated footer/control range.
    #[must_use]
    pub const fn control_offset(&self) -> u64 {
        self.control_offset
    }

    /// Return the length of the authenticated footer/control range.
    #[must_use]
    pub const fn control_size(&self) -> u64 {
        self.object_size.saturating_sub(self.control_offset)
    }

    /// Return whether this value contains only the authenticated footer.
    #[must_use]
    pub const fn is_control_only(&self) -> bool {
        self.control_only
    }

    /// Return the immutable checkpoint hash.
    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Return the authenticated footer hash.
    #[must_use]
    pub fn footer_hash(&self) -> &str {
        &self.footer_hash
    }

    /// Return the covered root generation.
    #[must_use]
    pub const fn covered_generation(&self) -> u64 {
        self.footer.covered_generation
    }

    /// Return the exact root digest covered by this checkpoint.
    #[must_use]
    pub fn covered_root_digest(&self) -> &str {
        &self.footer.covered_root_digest
    }

    /// Return every physical pack source in stable order.
    #[must_use]
    pub fn sources(&self) -> &[PackSourceDescriptor] {
        &self.footer.sources
    }

    /// Return the number of physical sources.
    #[must_use]
    pub fn source_count(&self) -> usize {
        self.footer.sources.len()
    }

    /// Return the number of pack members.
    pub fn pack_count(&self) -> Result<u32> {
        self.footer.sources.iter().try_fold(0_u32, |total, source| {
            total
                .checked_add(
                    u32::try_from(source.members.len())
                        .map_err(|_| corrupt("layered checkpoint pack member count overflowed"))?,
                )
                .ok_or_else(|| corrupt("layered checkpoint pack member count overflowed"))
        })
    }

    /// Return the number of declared Git objects.
    pub fn object_count(&self) -> Result<u64> {
        self.footer.sources.iter().try_fold(0_u64, |total, source| {
            total
                .checked_add(source.object_count()?)
                .ok_or_else(|| corrupt("layered checkpoint object count overflowed"))
        })
    }

    /// Return the compact cold-clone proof for the complete visibility set.
    #[must_use]
    pub fn cold_clone_object_set_digest(&self) -> Option<&str> {
        self.footer.cold_clone_object_set_digest.as_deref()
    }

    /// Return the object count covered by the compact cold-clone proof.
    #[must_use]
    pub const fn cold_clone_object_count(&self) -> Option<u64> {
        self.footer.cold_clone_object_count
    }

    /// Decode the complete pointer catalog compacted by this checkpoint.
    pub fn pointer_catalog(&self) -> Result<PointerCatalog> {
        let Some(section) = self.section(CapsuleSectionKind::CatalogDelta)? else {
            return Ok(PointerCatalog::new());
        };
        PointerCatalog::decode(&section)
    }

    /// Decode the complete visibility snapshot compacted by this checkpoint.
    pub fn visibility_snapshot(
        &self,
    ) -> Result<Option<crate::capsule_protocol::CapsuleVisibilitySnapshot>> {
        let Some(section) = self.section(CapsuleSectionKind::VisibilitySnapshot)? else {
            return Ok(None);
        };
        crate::capsule_protocol::CapsuleVisibilitySnapshot::decode(&section).map(Some)
    }

    /// Decode the compact ordinal visibility proof, when present.
    pub fn visibility_ordinal_snapshot(&self) -> Result<Option<LayeredVisibilitySnapshot>> {
        let Some(section) = self.section(CapsuleSectionKind::VisibilityOrdinalSnapshot)? else {
            return Ok(None);
        };
        LayeredVisibilitySnapshot::decode(&section).map(Some)
    }

    fn section(&self, kind: CapsuleSectionKind) -> Result<Option<Bytes>> {
        if self.control_only {
            return Err(corrupt(
                "layered checkpoint section is unavailable in a control-only view",
            ));
        }
        let matches = self
            .footer
            .sections
            .iter()
            .filter(|section| section.kind == kind);
        let mut matches = matches.peekable();
        let Some(section) = matches.next() else {
            return Ok(None);
        };
        if matches.next().is_some() {
            return Err(corrupt("layered checkpoint repeats a control section"));
        }
        let start = usize::try_from(section.offset)
            .map_err(|_| corrupt("layered checkpoint section offset overflowed"))?;
        let end = section
            .offset
            .checked_add(section.length)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| corrupt("layered checkpoint section end overflowed"))?;
        self.bytes
            .get(start..end)
            .filter(|bytes| blake3::hash(bytes).to_hex().as_str() == section.blake3)
            .map(Bytes::copy_from_slice)
            .ok_or_else(|| corrupt("layered checkpoint control section hash does not match"))
            .map(Some)
    }
}

fn append_range(body: &mut Vec<u8>, bytes: &Bytes) -> Result<PackRange> {
    let offset = u64::try_from(body.len())
        .map_err(|_| contract_error("pack layer offset cannot be represented"))?;
    body.extend_from_slice(bytes);
    PackRange::new(offset, bytes)
}

fn append_section(
    body: &mut Vec<u8>,
    sections: &mut Vec<LayerSectionLocation>,
    kind: CapsuleSectionKind,
    bytes: Bytes,
) -> Result<()> {
    if bytes.is_empty() {
        return Err(contract_error(
            "layered checkpoint sections must not be empty",
        ));
    }
    let offset = u64::try_from(body.len())
        .map_err(|_| contract_error("layered checkpoint section offset overflowed"))?;
    let length = u64::try_from(bytes.len())
        .map_err(|_| contract_error("layered checkpoint section length overflowed"))?;
    body.extend_from_slice(&bytes);
    sections.push(LayerSectionLocation {
        kind,
        offset,
        length,
        blake3: blake3::hash(&bytes).to_hex().to_string(),
    });
    Ok(())
}

fn decode_layer_footer(bytes: &[u8]) -> Result<(LayerFooter, usize, String)> {
    let (footer_bytes, footer_start, footer_hash) =
        decode_trailer(bytes, LAYER_MAGIC, MAX_LAYER_FOOTER_BYTES)?;
    let footer: LayerFooter = serde_json::from_slice(footer_bytes)
        .map_err(|source| corrupt(format!("pack layer footer is invalid JSON: {source}")))?;
    Ok((footer, footer_start, footer_hash))
}

fn decode_checkpoint_footer(bytes: &[u8]) -> Result<(CheckpointFooter, usize, String)> {
    let (footer_bytes, footer_start, footer_hash) =
        decode_trailer(bytes, CHECKPOINT_MAGIC, MAX_CHECKPOINT_FOOTER_BYTES)?;
    let footer: CheckpointFooter = serde_json::from_slice(footer_bytes).map_err(|source| {
        corrupt(format!(
            "layered checkpoint footer is invalid JSON: {source}"
        ))
    })?;
    Ok((footer, footer_start, footer_hash))
}

fn decode_trailer<'a>(
    bytes: &'a [u8],
    magic: &[u8; 8],
    max_footer: usize,
) -> Result<(&'a [u8], usize, String)> {
    if bytes.len() < TRAILER_BYTES || &bytes[bytes.len() - magic.len()..] != magic {
        return Err(corrupt(
            "layered object is shorter than or has an invalid trailer",
        ));
    }
    let trailer = bytes.len() - TRAILER_BYTES;
    let footer_length = u64::from_be_bytes(
        bytes[trailer..trailer + 8]
            .try_into()
            .map_err(|_| corrupt("layered footer length is truncated"))?,
    );
    let footer_length = usize::try_from(footer_length)
        .map_err(|_| corrupt("layered footer length cannot be represented"))?;
    if footer_length == 0 || footer_length > max_footer || footer_length > trailer {
        return Err(corrupt("layered footer length is out of bounds"));
    }
    let footer_start = trailer - footer_length;
    let footer_bytes = &bytes[footer_start..trailer];
    let footer_hash = blake3::hash(footer_bytes).to_hex().to_string();
    let expected = blake3::Hash::from_bytes(
        bytes[trailer + 8..trailer + 40]
            .try_into()
            .map_err(|_| corrupt("layered footer hash is truncated"))?,
    )
    .to_hex()
    .to_string();
    if footer_hash != expected {
        return Err(corrupt("layered footer hash does not match"));
    }
    Ok((footer_bytes, footer_start, footer_hash))
}

fn validate_layer_footer(footer: &LayerFooter, footer_start: u64, object_size: u64) -> Result<()> {
    if footer.version != LAYER_VERSION || footer.control_offset >= object_size {
        return Err(corrupt("pack layer footer shape is invalid"));
    }
    if footer.control_offset > footer_start {
        return Err(corrupt("pack layer control offset is invalid"));
    }
    footer.member.validate(object_size, 0)?;
    if footer.member.pack().end()? > footer.control_offset
        || footer.member.index().end()? > footer_start
        || footer.member.reverse_index().end()? > footer_start
        || footer.member.locator().end()? > footer_start
    {
        return Err(corrupt("pack layer member overlaps its control suffix"));
    }
    Ok(())
}

fn validate_checkpoint_footer(
    footer: &CheckpointFooter,
    footer_start: u64,
    object_size: u64,
) -> Result<()> {
    if footer.version != CHECKPOINT_VERSION
        || footer.sources.is_empty()
        || footer.sources.len() > MAX_PHYSICAL_SOURCES
        || footer.control_offset != 0
    {
        return Err(corrupt("layered checkpoint footer shape is invalid"));
    }
    validate_content_hash(
        &footer.covered_root_digest,
        "layered checkpoint covered root digest",
        "capsule-protocol layered checkpoint",
    )?;
    if let Some(digest) = &footer.cold_clone_object_set_digest {
        validate_content_hash(
            digest,
            "layered checkpoint cold-clone object-set digest",
            "capsule-protocol layered checkpoint",
        )?;
    }
    if matches!(
        (
            footer.cold_clone_object_set_digest.as_ref(),
            footer.cold_clone_object_count,
        ),
        (Some(_), None) | (None, Some(_))
    ) {
        return Err(corrupt(
            "layered checkpoint cold-clone proof has an incomplete object-set commitment",
        ));
    }
    let mut expected_offset = 0_u64;
    for section in &footer.sections {
        validate_content_hash(
            &section.blake3,
            "layered checkpoint section hash",
            "capsule-protocol layered checkpoint",
        )?;
        let end = section
            .offset
            .checked_add(section.length)
            .ok_or_else(|| corrupt("layered checkpoint section range overflowed"))?;
        if section.length == 0 || section.offset != expected_offset || end > footer_start {
            return Err(corrupt("layered checkpoint sections are not canonical"));
        }
        expected_offset = end;
    }
    if expected_offset != footer_start {
        return Err(corrupt("layered checkpoint sections do not cover its body"));
    }
    validate_source_inventory(&footer.sources)?;
    for source in &footer.sources {
        source.validate()?;
        if source.object_size == 0 {
            return Err(corrupt("layered checkpoint source size is invalid"));
        }
    }
    if object_size <= footer_start {
        return Err(corrupt("layered checkpoint object is truncated"));
    }
    Ok(())
}

fn validate_source_inventory(sources: &[PackSourceDescriptor]) -> Result<()> {
    let mut identities = std::collections::BTreeSet::new();
    for source in sources {
        if !identities.insert(source.object_hash()) {
            return Err(corrupt("layered checkpoint repeats a source identity"));
        }
    }
    Ok(())
}

fn contract_error(reason: impl Into<String>) -> MetadataError {
    MetadataError::CapsuleContract {
        record: "capsule-protocol layered pack",
        reason: reason.into(),
    }
}

fn corrupt(reason: impl Into<String>) -> MetadataError {
    MetadataError::CorruptObject {
        path: "capsule-protocol layered pack".to_owned(),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::git_visibility::GitVisibilityIndex;

    fn pack() -> CapsuleGitPack {
        CapsuleGitPack::new(
            Bytes::from_static(b"PACK\0\0\0\0\0\0\0\0"),
            Bytes::from_static(b"index"),
            Bytes::from_static(b"reverse"),
            Bytes::from_static(b"locator"),
            "0123456789012345678901234567890123456789",
            1,
        )
        .unwrap()
    }

    #[test]
    fn layer_round_trip_preserves_source_identity() {
        let layer = PackLayer::build(&pack()).unwrap();
        let decoded = PackLayer::decode(layer.bytes().clone()).unwrap();
        assert_eq!(decoded.hash(), layer.hash());
        let source = decoded.source_descriptor().unwrap();
        assert_eq!(source.kind(), PackSourceKind::PackLayer);
        assert_eq!(source.members().len(), 1);
    }

    #[test]
    fn layered_checkpoint_round_trip_is_metadata_only() {
        let layer = PackLayer::build(&pack()).unwrap();
        let checkpoint = LayeredCheckpoint::build(
            3,
            &"a".repeat(64),
            vec![layer.source_descriptor().unwrap()],
            PointerCatalog::new(),
            None,
        )
        .unwrap();
        let decoded = LayeredCheckpoint::decode(checkpoint.bytes().clone()).unwrap();
        assert_eq!(decoded.hash(), checkpoint.hash());
        assert_eq!(decoded.pack_count().unwrap(), 1);
        assert!(decoded.bytes().len() < 16 * 1024);
    }

    #[test]
    fn layered_checkpoint_control_round_trip_skips_body() {
        let layer = PackLayer::build(&pack()).unwrap();
        let checkpoint = LayeredCheckpoint::build(
            3,
            &"a".repeat(64),
            vec![layer.source_descriptor().unwrap()],
            PointerCatalog::new(),
            None,
        )
        .unwrap();
        let start = usize::try_from(checkpoint.control_offset()).unwrap();
        let control = checkpoint.bytes().slice(start..);
        let decoded = LayeredCheckpoint::decode_control(
            control,
            checkpoint.bytes().len() as u64,
            checkpoint.control_offset(),
            checkpoint.hash(),
            checkpoint.footer_hash(),
        )
        .unwrap();
        assert!(decoded.is_control_only());
        assert!(decoded.bytes().is_empty());
        assert_eq!(decoded.sources(), checkpoint.sources());
        assert_eq!(decoded.control_size(), checkpoint.control_size());
        assert!(
            LayeredCheckpoint::decode_control(
                checkpoint.bytes().slice(start..),
                checkpoint.bytes().len() as u64,
                checkpoint.control_offset(),
                checkpoint.hash(),
                &"b".repeat(64),
            )
            .is_err()
        );
    }

    #[test]
    fn source_rejects_overlapping_members() {
        let first = PackMemberDescriptor::new(
            PackRange::new(0, b"pack").unwrap(),
            PackRange::new(4, b"index").unwrap(),
            PackRange::new(9, b"reverse").unwrap(),
            PackRange::new(16, b"locator").unwrap(),
            "0".repeat(40),
            1,
            Vec::new(),
        )
        .unwrap();
        let second = PackMemberDescriptor::new(
            PackRange::new(24, b"pack2").unwrap(),
            PackRange::new(29, b"index").unwrap(),
            PackRange::new(34, b"reverse").unwrap(),
            PackRange::new(41, b"locator").unwrap(),
            "1".repeat(40),
            1,
            Vec::new(),
        )
        .unwrap();
        let overlapping = PackMemberDescriptor::new(
            PackRange::new(18, b"pack").unwrap(),
            PackRange::new(22, b"index").unwrap(),
            PackRange::new(27, b"reverse").unwrap(),
            PackRange::new(34, b"locator").unwrap(),
            "1".repeat(40),
            1,
            Vec::new(),
        )
        .unwrap();
        let source = PackSourceDescriptor::new(
            PackSourceKind::PackLayer,
            "a".repeat(64),
            128,
            64,
            64,
            "b".repeat(64),
            vec![first.clone(), overlapping],
        );
        assert!(source.is_err());
        let source = PackSourceDescriptor::new(
            PackSourceKind::PackLayer,
            "a".repeat(64),
            128,
            64,
            64,
            "b".repeat(64),
            vec![first, second],
        );
        assert!(source.is_ok());
    }

    #[test]
    fn checkpoint_rejects_duplicate_source_identity() {
        let layer = PackLayer::build(&pack()).unwrap();
        let source = layer.source_descriptor().unwrap();
        let error = LayeredCheckpoint::build(
            3,
            &"a".repeat(64),
            vec![source.clone(), source],
            PointerCatalog::new(),
            None,
        )
        .expect_err("duplicate source identities must fail closed");
        assert!(matches!(error, MetadataError::CorruptObject { .. }));
    }

    #[test]
    fn ordinal_visibility_round_trips_and_binds_source_catalog() {
        let layer = PackLayer::build(&pack()).unwrap();
        let source = layer.source_descriptor().unwrap();
        let catalog_digest = source_catalog_digest(std::slice::from_ref(&source)).unwrap();
        let refs = BTreeMap::from([(
            "refs/heads/main".to_owned(),
            vec!["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned()],
        )]);
        let index = GitVisibilityIndex::new(1, "1".repeat(64), "2".repeat(64), refs).unwrap();
        let snapshot = LayeredVisibilitySnapshot::from_index(&index, &catalog_digest).unwrap();
        let encoded = snapshot.encode().unwrap();
        let decoded = LayeredVisibilitySnapshot::decode(&encoded).unwrap();
        let restored = decoded
            .to_index(1, &"1".repeat(64), &"2".repeat(64), &catalog_digest)
            .unwrap();
        assert_eq!(restored.ref_closures(), index.ref_closures());
        assert!(
            decoded
                .to_index(1, &"1".repeat(64), &"2".repeat(64), &"f".repeat(64))
                .is_err()
        );
    }

    #[test]
    fn ordinal_visibility_member_admission_round_trips() {
        let refs = BTreeMap::from([(
            "refs/heads/main".to_owned(),
            vec!["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned()],
        )]);
        let index = GitVisibilityIndex::new(1, "1".repeat(64), "2".repeat(64), refs).unwrap();
        let snapshot = LayeredVisibilitySnapshot::from_index_with_member_admission(
            &index,
            &"3".repeat(64),
            vec![LayeredObjectMember::new(0, 0)],
        )
        .unwrap();
        let decoded = LayeredVisibilitySnapshot::decode(&snapshot.encode().unwrap()).unwrap();
        assert_eq!(
            decoded.member_admission(),
            Some([LayeredObjectMember::new(0, 0)].as_slice())
        );
        assert!(
            LayeredVisibilitySnapshot::from_index_with_member_admission(
                &index,
                &"3".repeat(64),
                Vec::new(),
            )
            .is_err()
        );

        let layer = PackLayer::build(&pack()).unwrap();
        let source = layer.source_descriptor().unwrap();
        let catalog_digest = source_catalog_digest(std::slice::from_ref(&source)).unwrap();
        let snapshot = LayeredVisibilitySnapshot::from_index_with_member_admission(
            &index,
            &catalog_digest,
            vec![LayeredObjectMember::new(0, 0)],
        )
        .unwrap();
        let expected_digest = snapshot.object_set_digest();
        let checkpoint = LayeredCheckpoint::build_with_ordinal_visibility(
            1,
            &"4".repeat(64),
            vec![source],
            PointerCatalog::new(),
            Some(snapshot),
        )
        .unwrap();
        assert_eq!(
            checkpoint.cold_clone_object_set_digest(),
            Some(expected_digest.as_str())
        );
        assert_eq!(checkpoint.cold_clone_object_count(), Some(1));
        let control_start = usize::try_from(checkpoint.control_offset()).unwrap();
        let control = LayeredCheckpoint::decode_control(
            checkpoint.bytes().slice(control_start..),
            checkpoint.bytes().len() as u64,
            checkpoint.control_offset(),
            checkpoint.hash(),
            checkpoint.footer_hash(),
        )
        .unwrap();
        assert_eq!(
            control.cold_clone_object_set_digest(),
            Some(expected_digest.as_str())
        );
        assert_eq!(control.cold_clone_object_count(), Some(1));
    }

    #[test]
    fn ordinal_visibility_member_admission_follows_dictionary_remap() {
        let object_b = "b".repeat(40);
        let object_a = "a".repeat(40);
        let object_c = "c".repeat(40);
        let mut index = GitVisibilityIndex::new(
            1,
            "1".repeat(64),
            "2".repeat(64),
            BTreeMap::from([("refs/heads/main".to_owned(), vec![object_b.clone()])]),
        )
        .unwrap();
        index
            .apply_ref_edit(
                "refs/heads/main".to_owned(),
                &crate::git_visibility::GitVisibilityEdit::from_delta_objects(
                    Some(object_b),
                    object_c.clone(),
                    vec![object_a, object_c],
                    Vec::new(),
                ),
            )
            .unwrap();

        let first = LayeredObjectMember::new(0, 0);
        let second = LayeredObjectMember::new(0, 1);
        let third = LayeredObjectMember::new(0, 2);
        let snapshot = LayeredVisibilitySnapshot::from_index_with_member_admission(
            &index,
            &"3".repeat(64),
            vec![first, second, third],
        )
        .unwrap();
        assert!(
            snapshot
                .objects()
                .windows(2)
                .all(|window| window[0] < window[1])
        );
        assert_eq!(
            snapshot.member_admission(),
            Some([second, first, third].as_slice())
        );
        LayeredVisibilitySnapshot::decode(&snapshot.encode().unwrap()).unwrap();
    }

    #[test]
    fn ordinal_visibility_is_smaller_than_hex_snapshot_for_shared_history() {
        let mut refs = BTreeMap::new();
        refs.insert(
            "refs/heads/main".to_owned(),
            (0..128)
                .map(|value| format!("{value:040x}"))
                .collect::<Vec<_>>(),
        );
        let index = GitVisibilityIndex::new(1, "1".repeat(64), "2".repeat(64), refs).unwrap();
        let full = crate::capsule_protocol::CapsuleVisibilitySnapshot::from_index(&index)
            .unwrap()
            .encode()
            .unwrap();
        let compact = LayeredVisibilitySnapshot::from_index(&index, &"3".repeat(64))
            .unwrap()
            .encode()
            .unwrap();
        assert!(compact.len() < full.len());
    }
}
