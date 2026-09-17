use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::capsule_protocol::{
    CapsuleGitPack, CapsuleGitPackDescriptor, CapsuleSectionKind, CapsuleSectionLocation,
    CapsuleVisibilitySnapshot, PointerCatalog,
};
use crate::error::{MetadataError, Result};
use crate::validation::{validate_content_hash, validate_sha1};

const CHECKPOINT_MAGIC: &[u8; 8] = b"CRBCKP03";
const CHECKPOINT_VERSION: u32 = 3;
const CHECKPOINT_TRAILER_BYTES: usize = 8 + 32 + CHECKPOINT_MAGIC.len();
const MAX_CHECKPOINT_PACKS: usize = 65_535;
const MAX_CHECKPOINT_FOOTER_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointFooter {
    version: u32,
    covered_generation: u64,
    covered_root_digest: String,
    control_offset: u64,
    sections: Vec<CapsuleSectionLocation>,
    git_packs: Vec<CapsuleGitPackDescriptor>,
}

/// One immutable complete Git repository checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    bytes: Bytes,
    hash: String,
    footer_hash: String,
    footer: CheckpointFooter,
}

/// Authenticated control suffix of one immutable Git repository checkpoint.
///
/// The suffix contains the pack indexes, reverse indexes, object locators,
/// pointer catalog, visibility snapshot, and footer. Pack bodies remain
/// outside this value and can be fetched separately by range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointControl {
    bytes: Bytes,
    object_size: u64,
    hash: String,
    footer_hash: String,
    control_offset: u64,
    footer: CheckpointFooter,
}

impl Checkpoint {
    /// Build a checkpoint from independently usable complete Git packs.
    pub fn build(
        covered_generation: u64,
        covered_root_digest: &str,
        git_packs: Vec<CapsuleGitPack>,
    ) -> Result<Self> {
        Self::build_with_pointer_catalog(
            covered_generation,
            covered_root_digest,
            git_packs,
            PointerCatalog::new(),
        )
    }

    /// Build a complete Git checkpoint carrying the compacted pointer catalog.
    pub fn build_with_pointer_catalog(
        covered_generation: u64,
        covered_root_digest: &str,
        git_packs: Vec<CapsuleGitPack>,
        pointer_catalog: PointerCatalog,
    ) -> Result<Self> {
        Self::build_with_catalogs(
            covered_generation,
            covered_root_digest,
            git_packs,
            pointer_catalog,
            None,
        )
    }

    /// Build a complete Git checkpoint carrying pointer and visibility catalogs.
    pub fn build_with_catalogs(
        covered_generation: u64,
        covered_root_digest: &str,
        git_packs: Vec<CapsuleGitPack>,
        pointer_catalog: PointerCatalog,
        visibility: Option<CapsuleVisibilitySnapshot>,
    ) -> Result<Self> {
        validate_content_hash(
            covered_root_digest,
            "checkpoint covered root digest",
            "capsule-protocol checkpoint",
        )?;
        if git_packs.is_empty() || git_packs.len() > MAX_CHECKPOINT_PACKS {
            return Err(contract_error("checkpoint Git pack count is out of bounds"));
        }
        let mut body = Vec::new();
        let mut sections = Vec::with_capacity(git_packs.len() * 4);
        let mut descriptors = git_packs
            .iter()
            .map(|pack| CapsuleGitPackDescriptor {
                pack_section: 0,
                index_section: 0,
                reverse_index_section: 0,
                locator_section: 0,
                git_checksum: pack.git_checksum.clone(),
                object_count: pack.object_count,
            })
            .collect::<Vec<_>>();
        for (pack_index, pack) in git_packs.iter().enumerate() {
            if pack.pack.is_empty()
                || pack.index.is_empty()
                || pack.reverse_index.is_empty()
                || pack.locator.is_empty()
                || pack.object_count == 0
            {
                return Err(contract_error("checkpoint contains an incomplete Git pack"));
            }
            validate_sha1(
                &pack.git_checksum,
                "checkpoint Git checksum",
                "capsule-protocol checkpoint",
            )?;
            let section = u32::try_from(sections.len())
                .map_err(|_| contract_error("checkpoint section index overflowed"))?;
            append_section(
                &mut body,
                &mut sections,
                CapsuleSectionKind::GitPack,
                pack.pack.clone(),
            )?;
            descriptors[pack_index].pack_section = section;
        }
        let control_offset = u64::try_from(body.len())
            .map_err(|_| contract_error("checkpoint control offset cannot be represented"))?;
        for (pack_index, pack) in git_packs.iter().enumerate() {
            let section = u32::try_from(sections.len())
                .map_err(|_| contract_error("checkpoint section index overflowed"))?;
            append_section(
                &mut body,
                &mut sections,
                CapsuleSectionKind::GitIndex,
                pack.index.clone(),
            )?;
            descriptors[pack_index].index_section = section;
        }
        for (pack_index, pack) in git_packs.iter().enumerate() {
            let section = u32::try_from(sections.len())
                .map_err(|_| contract_error("checkpoint section index overflowed"))?;
            append_section(
                &mut body,
                &mut sections,
                CapsuleSectionKind::GitReverseIndex,
                pack.reverse_index.clone(),
            )?;
            descriptors[pack_index].reverse_index_section = section;
        }
        for (pack_index, pack) in git_packs.iter().enumerate() {
            let section = u32::try_from(sections.len())
                .map_err(|_| contract_error("checkpoint section index overflowed"))?;
            append_section(
                &mut body,
                &mut sections,
                CapsuleSectionKind::GitObjectLocator,
                pack.locator.clone(),
            )?;
            descriptors[pack_index].locator_section = section;
        }
        if !pointer_catalog.is_empty() {
            append_section(
                &mut body,
                &mut sections,
                CapsuleSectionKind::CatalogDelta,
                pointer_catalog.encode()?,
            )?;
        }
        if let Some(visibility) = visibility {
            append_section(
                &mut body,
                &mut sections,
                CapsuleSectionKind::VisibilitySnapshot,
                visibility.encode()?,
            )?;
        }
        let footer = CheckpointFooter {
            version: CHECKPOINT_VERSION,
            covered_generation,
            covered_root_digest: covered_root_digest.to_owned(),
            control_offset,
            sections,
            git_packs: descriptors,
        };
        let footer_bytes = serde_json::to_vec(&footer).map_err(|source| {
            MetadataError::Internal(format!("checkpoint footer serialization failed: {source}"))
        })?;
        if footer_bytes.len() > MAX_CHECKPOINT_FOOTER_BYTES {
            return Err(contract_error("checkpoint footer exceeds its size bound"));
        }
        body.extend_from_slice(&footer_bytes);
        body.extend_from_slice(&(footer_bytes.len() as u64).to_be_bytes());
        body.extend_from_slice(blake3::hash(&footer_bytes).as_bytes());
        body.extend_from_slice(CHECKPOINT_MAGIC);
        Self::decode(Bytes::from(body))
    }

    /// Decode and authenticate a complete checkpoint object.
    pub fn decode(bytes: Bytes) -> Result<Self> {
        if bytes.len() < CHECKPOINT_TRAILER_BYTES {
            return Err(corrupt("checkpoint is shorter than its trailer"));
        }
        let (footer, footer_start, footer_hash) = decode_footer(&bytes)?;
        validate_footer(
            &footer,
            footer_start as u64,
            footer.control_offset,
            Some((&bytes[..footer_start], 0)),
        )?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        Ok(Self {
            bytes,
            hash,
            footer_hash,
            footer,
        })
    }

    /// Return the complete checkpoint bytes.
    #[must_use]
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// Return the BLAKE3 object identity.
    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Return the ref generation covered by this checkpoint.
    #[must_use]
    pub fn covered_generation(&self) -> u64 {
        self.footer.covered_generation
    }

    /// Return the exact root digest covered by this checkpoint.
    #[must_use]
    pub fn covered_root_digest(&self) -> &str {
        &self.footer.covered_root_digest
    }

    /// Return the byte offset at which the authenticated control suffix begins.
    #[must_use]
    pub fn control_offset(&self) -> u64 {
        self.footer.control_offset
    }

    /// Return the byte length of the authenticated control suffix.
    #[must_use]
    pub fn control_size(&self) -> u64 {
        self.bytes.len() as u64 - self.footer.control_offset
    }

    /// Return the BLAKE3 hash of the authenticated footer.
    #[must_use]
    pub fn footer_hash(&self) -> &str {
        &self.footer_hash
    }

    /// Decode the control suffix from an already fetched range.
    pub fn decode_control(
        bytes: Bytes,
        object_size: u64,
        expected_hash: &str,
        control_offset: u64,
        control_size: u64,
        expected_footer_hash: &str,
    ) -> Result<CheckpointControl> {
        validate_content_hash(
            expected_hash,
            "checkpoint object hash",
            "capsule-protocol checkpoint",
        )?;
        validate_content_hash(
            expected_footer_hash,
            "checkpoint footer hash",
            "capsule-protocol checkpoint",
        )?;
        if control_size == 0
            || bytes.len() as u64 != control_size
            || control_offset
                .checked_add(control_size)
                .ok_or_else(|| corrupt("checkpoint control range overflowed"))?
                != object_size
        {
            return Err(corrupt(
                "checkpoint control range does not match its pointer",
            ));
        }
        let (footer, footer_start, footer_hash) = decode_footer(&bytes)?;
        if footer_hash != expected_footer_hash {
            return Err(corrupt(
                "checkpoint footer hash does not match its root pointer",
            ));
        }
        let body_len = control_offset
            .checked_add(footer_start as u64)
            .ok_or_else(|| corrupt("checkpoint body length overflowed"))?;
        if footer.control_offset != control_offset || control_offset > body_len {
            return Err(corrupt(
                "checkpoint control offset does not match its footer",
            ));
        }
        validate_footer(
            &footer,
            body_len,
            control_offset,
            Some((&bytes[..footer_start], control_offset)),
        )?;
        Ok(CheckpointControl {
            bytes,
            object_size,
            hash: expected_hash.to_owned(),
            footer_hash,
            control_offset,
            footer,
        })
    }

    /// Return the authenticated control suffix without another allocation.
    pub fn control(&self) -> Result<CheckpointControl> {
        let offset = usize::try_from(self.footer.control_offset)
            .map_err(|_| corrupt("checkpoint control offset cannot be represented"))?;
        let bytes = self.bytes.slice(offset..);
        Self::decode_control(
            bytes,
            self.bytes.len() as u64,
            self.hash(),
            self.control_offset(),
            self.control_size(),
            self.footer_hash(),
        )
    }

    /// Return authenticated Git pack descriptors.
    #[must_use]
    pub fn git_packs(&self) -> &[CapsuleGitPackDescriptor] {
        &self.footer.git_packs
    }

    /// Return authenticated bytes for one checkpoint section.
    pub fn section_bytes(&self, section: u32) -> Result<Bytes> {
        let location = self
            .footer
            .sections
            .get(usize::try_from(section).map_err(|_| corrupt("section index overflowed"))?)
            .ok_or_else(|| corrupt("section index is out of bounds"))?;
        let start = usize::try_from(location.offset)
            .map_err(|_| corrupt("section offset cannot be represented"))?;
        let end = location
            .offset
            .checked_add(location.length)
            .and_then(|end| usize::try_from(end).ok())
            .ok_or_else(|| corrupt("section range cannot be represented"))?;
        Ok(self.bytes.slice(start..end))
    }

    /// Decode the complete pointer catalog compacted by this checkpoint.
    pub fn pointer_catalog(&self) -> Result<PointerCatalog> {
        let mut sections = self
            .footer
            .sections
            .iter()
            .enumerate()
            .filter(|(_, section)| section.kind == CapsuleSectionKind::CatalogDelta);
        let Some((index, _)) = sections.next() else {
            return Ok(PointerCatalog::new());
        };
        if sections.next().is_some() {
            return Err(corrupt("checkpoint contains more than one pointer catalog"));
        }
        let index = u32::try_from(index)
            .map_err(|_| corrupt("pointer catalog section index cannot be represented"))?;
        PointerCatalog::decode(&self.section_bytes(index)?)
    }

    /// Decode the complete Git visibility state compacted by this checkpoint.
    pub fn visibility_snapshot(&self) -> Result<Option<CapsuleVisibilitySnapshot>> {
        let mut sections = self
            .footer
            .sections
            .iter()
            .enumerate()
            .filter(|(_, section)| section.kind == CapsuleSectionKind::VisibilitySnapshot);
        let Some((index, _)) = sections.next() else {
            return Ok(None);
        };
        if sections.next().is_some() {
            return Err(corrupt(
                "checkpoint contains more than one Git visibility snapshot",
            ));
        }
        let index = u32::try_from(index)
            .map_err(|_| corrupt("visibility snapshot section index cannot be represented"))?;
        CapsuleVisibilitySnapshot::decode(&self.section_bytes(index)?).map(Some)
    }
}

impl CheckpointControl {
    /// Return the expected complete checkpoint identity bound by the root.
    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Return the complete checkpoint object size.
    #[must_use]
    pub fn object_size(&self) -> u64 {
        self.object_size
    }

    /// Return the control suffix offset within the complete checkpoint object.
    #[must_use]
    pub fn control_offset(&self) -> u64 {
        self.control_offset
    }

    /// Return the control suffix length.
    #[must_use]
    pub fn control_size(&self) -> u64 {
        self.bytes.len() as u64
    }

    /// Return the BLAKE3 hash of the authenticated footer.
    #[must_use]
    pub fn footer_hash(&self) -> &str {
        &self.footer_hash
    }

    /// Return the ref generation covered by this checkpoint.
    #[must_use]
    pub fn covered_generation(&self) -> u64 {
        self.footer.covered_generation
    }

    /// Return the exact root digest covered by this checkpoint.
    #[must_use]
    pub fn covered_root_digest(&self) -> &str {
        &self.footer.covered_root_digest
    }

    /// Return authenticated Git pack descriptors.
    #[must_use]
    pub fn git_packs(&self) -> &[CapsuleGitPackDescriptor] {
        &self.footer.git_packs
    }

    /// Return the authenticated location for one checkpoint section.
    pub fn section_location(&self, section: u32) -> Result<&CapsuleSectionLocation> {
        self.footer
            .sections
            .get(usize::try_from(section).map_err(|_| corrupt("section index overflowed"))?)
            .ok_or_else(|| corrupt("section index is out of bounds"))
    }

    /// Return authenticated bytes for one control-suffix section.
    pub fn section_bytes(&self, section: u32) -> Result<Bytes> {
        let location = self.section_location(section)?;
        let start = location
            .offset
            .checked_sub(self.control_offset)
            .and_then(|offset| usize::try_from(offset).ok())
            .ok_or_else(|| corrupt("control section is outside the suffix"))?;
        let end = location
            .offset
            .checked_add(location.length)
            .and_then(|end| end.checked_sub(self.control_offset))
            .and_then(|end| usize::try_from(end).ok())
            .ok_or_else(|| corrupt("control section range cannot be represented"))?;
        self.bytes
            .get(start..end)
            .map(Bytes::copy_from_slice)
            .ok_or_else(|| corrupt("control section is out of bounds"))
    }

    /// Decode the complete pointer catalog compacted by this checkpoint.
    pub fn pointer_catalog(&self) -> Result<PointerCatalog> {
        let mut sections = self
            .footer
            .sections
            .iter()
            .enumerate()
            .filter(|(_, section)| section.kind == CapsuleSectionKind::CatalogDelta);
        let Some((index, _)) = sections.next() else {
            return Ok(PointerCatalog::new());
        };
        if sections.next().is_some() {
            return Err(corrupt("checkpoint contains more than one pointer catalog"));
        }
        PointerCatalog::decode(
            &self
                .section_bytes(u32::try_from(index).map_err(|_| {
                    corrupt("pointer catalog section index cannot be represented")
                })?)?,
        )
    }

    /// Decode the complete Git visibility state compacted by this checkpoint.
    pub fn visibility_snapshot(&self) -> Result<Option<CapsuleVisibilitySnapshot>> {
        let mut sections = self
            .footer
            .sections
            .iter()
            .enumerate()
            .filter(|(_, section)| section.kind == CapsuleSectionKind::VisibilitySnapshot);
        let Some((index, _)) = sections.next() else {
            return Ok(None);
        };
        if sections.next().is_some() {
            return Err(corrupt(
                "checkpoint contains more than one Git visibility snapshot",
            ));
        }
        CapsuleVisibilitySnapshot::decode(
            &self.section_bytes(u32::try_from(index).map_err(|_| {
                corrupt("visibility snapshot section index cannot be represented")
            })?)?,
        )
        .map(Some)
    }
}

fn append_section(
    body: &mut Vec<u8>,
    sections: &mut Vec<CapsuleSectionLocation>,
    kind: CapsuleSectionKind,
    bytes: Bytes,
) -> Result<()> {
    let offset = u64::try_from(body.len())
        .map_err(|_| contract_error("checkpoint section offset cannot be represented"))?;
    let length = u64::try_from(bytes.len())
        .map_err(|_| contract_error("checkpoint section length cannot be represented"))?;
    body.extend_from_slice(&bytes);
    sections.push(CapsuleSectionLocation {
        kind,
        offset,
        length,
        blake3: blake3::hash(&bytes).to_hex().to_string(),
    });
    Ok(())
}

fn decode_footer(bytes: &[u8]) -> Result<(CheckpointFooter, usize, String)> {
    if bytes.len() < CHECKPOINT_TRAILER_BYTES {
        return Err(corrupt("checkpoint is shorter than its trailer"));
    }
    let trailer = bytes.len() - CHECKPOINT_TRAILER_BYTES;
    if &bytes[bytes.len() - CHECKPOINT_MAGIC.len()..] != CHECKPOINT_MAGIC {
        return Err(corrupt("checkpoint magic is invalid"));
    }
    let footer_length = u64::from_be_bytes(
        bytes[trailer..trailer + 8]
            .try_into()
            .map_err(|_| corrupt("checkpoint footer length is truncated"))?,
    );
    let footer_length = usize::try_from(footer_length)
        .map_err(|_| corrupt("checkpoint footer length cannot be represented"))?;
    if footer_length > MAX_CHECKPOINT_FOOTER_BYTES || footer_length > trailer {
        return Err(corrupt("checkpoint footer length is out of bounds"));
    }
    let footer_start = trailer - footer_length;
    let footer_bytes = &bytes[footer_start..trailer];
    let actual_hash = blake3::hash(footer_bytes).to_hex().to_string();
    let expected_hash = blake3::Hash::from_bytes(
        bytes[trailer + 8..trailer + 40]
            .try_into()
            .map_err(|_| corrupt("checkpoint footer hash is truncated"))?,
    )
    .to_hex()
    .to_string();
    if actual_hash != expected_hash {
        return Err(corrupt("checkpoint footer hash does not match"));
    }
    let footer: CheckpointFooter = serde_json::from_slice(footer_bytes)
        .map_err(|source| corrupt(format!("checkpoint footer is invalid JSON: {source}")))?;
    if serde_json::to_vec(&footer).map_err(|source| {
        MetadataError::Internal(format!("checkpoint footer serialization failed: {source}"))
    })? != footer_bytes
    {
        return Err(corrupt("checkpoint footer is not canonically encoded"));
    }
    Ok((footer, footer_start, actual_hash))
}

fn validate_footer(
    footer: &CheckpointFooter,
    body_len: u64,
    control_offset: u64,
    control_bytes: Option<(&[u8], u64)>,
) -> Result<()> {
    if footer.version != CHECKPOINT_VERSION
        || footer.git_packs.is_empty()
        || footer.git_packs.len() > MAX_CHECKPOINT_PACKS
        || !matches!(
            footer
                .sections
                .len()
                .checked_sub(footer.git_packs.len() * 4),
            Some(0..=2)
        )
    {
        return Err(corrupt("checkpoint footer shape is invalid"));
    }
    if footer.control_offset != control_offset || control_offset > body_len {
        return Err(corrupt("checkpoint control offset is invalid"));
    }
    validate_content_hash(
        &footer.covered_root_digest,
        "checkpoint covered root digest",
        "capsule-protocol checkpoint",
    )?;
    let mut expected_offset = 0_u64;
    for location in &footer.sections {
        validate_content_hash(
            &location.blake3,
            "checkpoint section hash",
            "capsule-protocol checkpoint",
        )?;
        if location.length == 0 || location.offset != expected_offset {
            return Err(corrupt(
                "checkpoint sections are not non-empty and contiguous",
            ));
        }
        let end = location
            .offset
            .checked_add(location.length)
            .ok_or_else(|| corrupt("checkpoint section range overflowed"))?;
        if end > body_len {
            return Err(corrupt("checkpoint section is out of bounds"));
        }
        if location.offset < control_offset && end > control_offset {
            return Err(corrupt("checkpoint control range splits a section"));
        }
        if let Some((control_bytes, bytes_offset)) = control_bytes {
            let complete_body = bytes_offset == 0 && control_bytes.len() as u64 == body_len;
            if !complete_body && location.offset < control_offset {
                expected_offset = end;
                continue;
            }
            let start = usize::try_from(location.offset - bytes_offset)
                .map_err(|_| corrupt("checkpoint control section offset cannot be represented"))?;
            let end = usize::try_from(end - bytes_offset)
                .map_err(|_| corrupt("checkpoint control section end cannot be represented"))?;
            let section = control_bytes
                .get(start..end)
                .ok_or_else(|| corrupt("checkpoint control section is out of bounds"))?;
            if blake3::hash(section).to_hex().as_str() != location.blake3 {
                return Err(corrupt("checkpoint section hash does not match"));
            }
        }
        expected_offset = end;
    }
    if expected_offset != body_len {
        return Err(corrupt(
            "checkpoint sections do not cover the complete body",
        ));
    }
    for (pack_index, descriptor) in footer.git_packs.iter().enumerate() {
        validate_sha1(
            &descriptor.git_checksum,
            "checkpoint Git checksum",
            "capsule-protocol checkpoint",
        )?;
        if descriptor.object_count == 0 {
            return Err(corrupt("checkpoint Git pack has zero objects"));
        }
        let pack_count = footer.git_packs.len();
        let bindings = [
            (descriptor.pack_section, CapsuleSectionKind::GitPack),
            (descriptor.index_section, CapsuleSectionKind::GitIndex),
            (
                descriptor.reverse_index_section,
                CapsuleSectionKind::GitReverseIndex,
            ),
            (
                descriptor.locator_section,
                CapsuleSectionKind::GitObjectLocator,
            ),
        ];
        let expected_indices = [
            pack_index,
            pack_count + pack_index,
            pack_count * 2 + pack_index,
            pack_count * 3 + pack_index,
        ];
        for ((index, kind), expected_index) in bindings.into_iter().zip(expected_indices) {
            let index = usize::try_from(index)
                .map_err(|_| corrupt("checkpoint Git section index overflowed"))?;
            let location = footer
                .sections
                .get(index)
                .ok_or_else(|| corrupt("checkpoint Git section index is out of bounds"))?;
            if index != expected_index || location.kind != kind {
                return Err(corrupt("checkpoint Git pack bindings are not canonical"));
            }
        }
    }
    let pack_count = footer.git_packs.len();
    for (index, expected_kind) in [
        CapsuleSectionKind::GitPack,
        CapsuleSectionKind::GitIndex,
        CapsuleSectionKind::GitReverseIndex,
        CapsuleSectionKind::GitObjectLocator,
    ]
    .into_iter()
    .enumerate()
    {
        let start = index * pack_count;
        if footer.sections[start..start + pack_count]
            .iter()
            .any(|section| section.kind != expected_kind)
        {
            return Err(corrupt(
                "checkpoint Git sections are not grouped canonically",
            ));
        }
    }
    let trailing = &footer.sections[pack_count * 4..];
    match trailing {
        []
        | [
            CapsuleSectionLocation {
                kind: CapsuleSectionKind::CatalogDelta,
                ..
            },
        ]
        | [
            CapsuleSectionLocation {
                kind: CapsuleSectionKind::VisibilitySnapshot,
                ..
            },
        ]
        | [
            CapsuleSectionLocation {
                kind: CapsuleSectionKind::CatalogDelta,
                ..
            },
            CapsuleSectionLocation {
                kind: CapsuleSectionKind::VisibilitySnapshot,
                ..
            },
        ] => {}
        _ => {
            return Err(corrupt(
                "checkpoint trailing catalogs are not canonically ordered",
            ));
        }
    }
    Ok(())
}

fn contract_error(reason: impl Into<String>) -> MetadataError {
    MetadataError::CapsuleContract {
        record: "checkpoint",
        reason: reason.into(),
    }
}

fn corrupt(reason: impl Into<String>) -> MetadataError {
    MetadataError::CorruptObject {
        path: "capsule-protocol checkpoint".to_owned(),
        reason: reason.into(),
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test assertions")]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::git_visibility::GitVisibilityIndex;

    #[test]
    fn checkpoint_round_trip_preserves_visibility_snapshot() {
        let tip = "1".repeat(40);
        let refs = BTreeMap::from([("refs/heads/main".to_owned(), vec![tip])]);
        let visibility = CapsuleVisibilitySnapshot::from_index(
            &GitVisibilityIndex::new(7, "2".repeat(64), "3".repeat(64), refs.clone())
                .expect("visibility index"),
        )
        .expect("visibility snapshot");
        let git_pack = CapsuleGitPack::new(
            Bytes::from_static(b"pack"),
            Bytes::from_static(b"index"),
            Bytes::from_static(b"reverse"),
            Bytes::from_static(b"locator"),
            "4".repeat(40),
            1,
        )
        .expect("Git pack");
        let checkpoint = Checkpoint::build_with_catalogs(
            7,
            &"5".repeat(64),
            vec![git_pack],
            PointerCatalog::new(),
            Some(visibility),
        )
        .expect("checkpoint");

        let decoded = Checkpoint::decode(checkpoint.bytes().clone()).expect("decoded checkpoint");

        assert_eq!(
            decoded
                .visibility_snapshot()
                .expect("decoded visibility")
                .expect("visibility section")
                .refs(),
            &refs
        );
    }

    #[test]
    fn checkpoint_control_round_trip_excludes_pack_bodies() {
        let git_pack = CapsuleGitPack::new(
            Bytes::from_static(b"pack"),
            Bytes::from_static(b"index"),
            Bytes::from_static(b"reverse"),
            Bytes::from_static(b"locator"),
            "4".repeat(40),
            1,
        )
        .expect("Git pack");
        let checkpoint = Checkpoint::build_with_pointer_catalog(
            7,
            &"5".repeat(64),
            vec![git_pack],
            PointerCatalog::new(),
        )
        .expect("checkpoint");

        let control = checkpoint.control().expect("control suffix");

        assert_eq!(control.hash(), checkpoint.hash());
        assert_eq!(control.control_offset(), checkpoint.control_offset());
        assert_eq!(control.control_size(), checkpoint.control_size());
        assert_eq!(control.footer_hash(), checkpoint.footer_hash());
        assert_eq!(control.git_packs(), checkpoint.git_packs());
        assert!(
            control
                .section_bytes(checkpoint.git_packs()[0].pack_section())
                .is_err()
        );
        assert_eq!(
            control
                .section_bytes(checkpoint.git_packs()[0].index_section())
                .expect("control index"),
            Bytes::from_static(b"index")
        );
    }

    #[test]
    fn complete_checkpoint_decode_authenticates_pack_bodies() {
        let git_pack = CapsuleGitPack::new(
            Bytes::from_static(b"pack"),
            Bytes::from_static(b"index"),
            Bytes::from_static(b"reverse"),
            Bytes::from_static(b"locator"),
            "4".repeat(40),
            1,
        )
        .expect("Git pack");
        let checkpoint = Checkpoint::build(7, &"5".repeat(64), vec![git_pack]).expect("checkpoint");
        let mut bytes = checkpoint.bytes().to_vec();
        bytes[0] ^= 1;

        assert!(Checkpoint::decode(Bytes::from(bytes)).is_err());
    }

    #[test]
    fn checkpoint_control_rejects_truncated_or_corrupt_suffix() {
        let git_pack = CapsuleGitPack::new(
            Bytes::from_static(b"pack"),
            Bytes::from_static(b"index"),
            Bytes::from_static(b"reverse"),
            Bytes::from_static(b"locator"),
            "4".repeat(40),
            1,
        )
        .expect("Git pack");
        let checkpoint = Checkpoint::build(7, &"5".repeat(64), vec![git_pack]).expect("checkpoint");
        let control = checkpoint.control().expect("control suffix");

        let truncated = control.bytes.slice(..control.bytes.len() - 1);
        assert!(
            Checkpoint::decode_control(
                truncated,
                checkpoint.bytes().len() as u64,
                checkpoint.hash(),
                checkpoint.control_offset(),
                checkpoint.control_size(),
                checkpoint.footer_hash(),
            )
            .is_err()
        );

        let mut corrupt = control.bytes.to_vec();
        corrupt[0] ^= 1;
        assert!(
            Checkpoint::decode_control(
                Bytes::from(corrupt),
                checkpoint.bytes().len() as u64,
                checkpoint.hash(),
                checkpoint.control_offset(),
                checkpoint.control_size(),
                checkpoint.footer_hash(),
            )
            .is_err()
        );
    }
}
