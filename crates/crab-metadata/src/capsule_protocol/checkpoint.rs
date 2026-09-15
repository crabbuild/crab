use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::capsule_protocol::{
    CapsuleGitPack, CapsuleGitPackDescriptor, CapsuleSectionKind, CapsuleSectionLocation,
    PointerCatalog,
};
use crate::error::{MetadataError, Result};
use crate::validation::{validate_content_hash, validate_sha1};

const CHECKPOINT_MAGIC: &[u8; 8] = b"CRBCKP02";
const CHECKPOINT_VERSION: u32 = 2;
const CHECKPOINT_TRAILER_BYTES: usize = 8 + 32 + CHECKPOINT_MAGIC.len();
const MAX_CHECKPOINT_PACKS: usize = 65_535;
const MAX_CHECKPOINT_FOOTER_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointFooter {
    version: u32,
    covered_generation: u64,
    covered_root_digest: String,
    sections: Vec<CapsuleSectionLocation>,
    git_packs: Vec<CapsuleGitPackDescriptor>,
}

/// One immutable complete Git repository checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    bytes: Bytes,
    hash: String,
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
        let mut descriptors = Vec::with_capacity(git_packs.len());
        for pack in git_packs {
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
            let first = u32::try_from(sections.len())
                .map_err(|_| contract_error("checkpoint section index overflowed"))?;
            append_section(
                &mut body,
                &mut sections,
                CapsuleSectionKind::GitPack,
                pack.pack,
            )?;
            append_section(
                &mut body,
                &mut sections,
                CapsuleSectionKind::GitIndex,
                pack.index,
            )?;
            append_section(
                &mut body,
                &mut sections,
                CapsuleSectionKind::GitReverseIndex,
                pack.reverse_index,
            )?;
            append_section(
                &mut body,
                &mut sections,
                CapsuleSectionKind::GitObjectLocator,
                pack.locator,
            )?;
            descriptors.push(CapsuleGitPackDescriptor {
                pack_section: first,
                index_section: first + 1,
                reverse_index_section: first + 2,
                locator_section: first + 3,
                git_checksum: pack.git_checksum,
                object_count: pack.object_count,
            });
        }
        if !pointer_catalog.is_empty() {
            append_section(
                &mut body,
                &mut sections,
                CapsuleSectionKind::CatalogDelta,
                pointer_catalog.encode()?,
            )?;
        }
        let footer = CheckpointFooter {
            version: CHECKPOINT_VERSION,
            covered_generation,
            covered_root_digest: covered_root_digest.to_owned(),
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
        if blake3::hash(footer_bytes).as_bytes() != &bytes[trailer + 8..trailer + 40] {
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
        validate_footer(&footer, &bytes[..footer_start])?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        Ok(Self {
            bytes,
            hash,
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

fn validate_footer(footer: &CheckpointFooter, body: &[u8]) -> Result<()> {
    if footer.version != CHECKPOINT_VERSION
        || footer.git_packs.is_empty()
        || footer.git_packs.len() > MAX_CHECKPOINT_PACKS
        || !matches!(
            footer
                .sections
                .len()
                .checked_sub(footer.git_packs.len() * 4),
            Some(0 | 1)
        )
    {
        return Err(corrupt("checkpoint footer shape is invalid"));
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
        let start = usize::try_from(location.offset)
            .map_err(|_| corrupt("checkpoint section offset cannot be represented"))?;
        let end_usize = usize::try_from(end)
            .map_err(|_| corrupt("checkpoint section end cannot be represented"))?;
        let section = body
            .get(start..end_usize)
            .ok_or_else(|| corrupt("checkpoint section is out of bounds"))?;
        if blake3::hash(section).to_hex().as_str() != location.blake3 {
            return Err(corrupt("checkpoint section hash does not match"));
        }
        expected_offset = end;
    }
    if expected_offset != body.len() as u64 {
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
        let first = pack_index * 4;
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
        for (offset, (index, kind)) in bindings.into_iter().enumerate() {
            let index = usize::try_from(index)
                .map_err(|_| corrupt("checkpoint Git section index overflowed"))?;
            let location = footer
                .sections
                .get(index)
                .ok_or_else(|| corrupt("checkpoint Git section index is out of bounds"))?;
            if index != first + offset || location.kind != kind {
                return Err(corrupt("checkpoint Git pack bindings are not canonical"));
            }
        }
    }
    if footer.sections.len() == footer.git_packs.len() * 4 + 1
        && footer.sections.last().map(|section| section.kind)
            != Some(CapsuleSectionKind::CatalogDelta)
    {
        return Err(corrupt(
            "checkpoint trailing section must be the pointer catalog",
        ));
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
