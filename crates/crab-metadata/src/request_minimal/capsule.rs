use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::error::{MetadataError, Result};
use crate::request_minimal::CapsuleTransaction;
use crate::validation::{validate_content_hash, validate_sha1};

const CAPSULE_MAGIC: &[u8; 8] = b"CRBCAPS2";
const CAPSULE_VERSION: u32 = 2;
const CAPSULE_TRAILER_BYTES: usize = 8 + 32 + CAPSULE_MAGIC.len();
const MAX_CAPSULE_SECTIONS: usize = 65_535;
const MAX_CAPSULE_FOOTER_BYTES: usize = 8 * 1024 * 1024;

/// Authoritative payload kind stored inside one immutable capsule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapsuleSectionKind {
    /// Canonical expected-old ref transaction; always the first section.
    RefTransaction,
    /// Ordinary Git packfile bytes.
    GitPack,
    /// Git pack index bytes.
    GitIndex,
    /// Git reverse-index bytes.
    GitReverseIndex,
    /// Authenticated object-to-pack location index.
    GitObjectLocator,
    /// Large-file payload frames.
    FileData,
    /// File reconstruction recipes.
    FileRecipes,
    /// Catalog changes introduced by the transaction.
    CatalogDelta,
    /// Authorization visibility changes introduced by the transaction.
    VisibilityDelta,
}

/// One locally prepared capsule payload section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapsuleSection {
    kind: CapsuleSectionKind,
    bytes: Bytes,
}

/// One locally prepared Git pack and all evidence required to read it safely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapsuleGitPack {
    pub(crate) pack: Bytes,
    pub(crate) index: Bytes,
    pub(crate) reverse_index: Bytes,
    pub(crate) locator: Bytes,
    pub(crate) git_checksum: String,
    pub(crate) object_count: u64,
}

impl CapsuleGitPack {
    /// Bind one non-empty pack to its index, reverse index, and object locator.
    pub fn new(
        pack: Bytes,
        index: Bytes,
        reverse_index: Bytes,
        locator: Bytes,
        git_checksum: impl Into<String>,
        object_count: u64,
    ) -> Result<Self> {
        let pack = Self {
            pack,
            index,
            reverse_index,
            locator,
            git_checksum: git_checksum.into(),
            object_count,
        };
        validate_git_pack_input(&pack)?;
        Ok(pack)
    }

    /// Return the complete Git packfile byte length.
    #[must_use]
    pub fn pack_size(&self) -> u64 {
        self.pack.len() as u64
    }
}

/// Authenticated section bindings and Git identity for one capsule pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapsuleGitPackDescriptor {
    pub(crate) pack_section: u32,
    pub(crate) index_section: u32,
    pub(crate) reverse_index_section: u32,
    pub(crate) locator_section: u32,
    pub(crate) git_checksum: String,
    pub(crate) object_count: u64,
}

impl CapsuleGitPackDescriptor {
    /// Return the section containing ordinary Git packfile bytes.
    #[must_use]
    pub fn pack_section(&self) -> u32 {
        self.pack_section
    }

    /// Return the section containing the matching Git pack index.
    #[must_use]
    pub fn index_section(&self) -> u32 {
        self.index_section
    }

    /// Return the section containing the matching Git reverse index.
    #[must_use]
    pub fn reverse_index_section(&self) -> u32 {
        self.reverse_index_section
    }

    /// Return the section containing checksummed object locator metadata.
    #[must_use]
    pub fn locator_section(&self) -> u32 {
        self.locator_section
    }

    /// Return the SHA-1 checksum in the Git pack trailer.
    #[must_use]
    pub fn git_checksum(&self) -> &str {
        &self.git_checksum
    }

    /// Return the number of objects proven by the pack index.
    #[must_use]
    pub fn object_count(&self) -> u64 {
        self.object_count
    }
}

impl CapsuleSection {
    /// Create one non-empty section whose bytes will be authenticated by the capsule footer.
    #[must_use]
    pub fn new(kind: CapsuleSectionKind, bytes: Bytes) -> Self {
        Self { kind, bytes }
    }
}

/// Authenticated location of one section within capsule bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapsuleSectionLocation {
    pub(crate) kind: CapsuleSectionKind,
    pub(crate) offset: u64,
    pub(crate) length: u64,
    pub(crate) blake3: String,
}

impl CapsuleSectionLocation {
    /// Return the section's wire kind.
    #[must_use]
    pub fn kind(&self) -> CapsuleSectionKind {
        self.kind
    }

    /// Return the section offset from the start of capsule payload bytes.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Return the section length.
    #[must_use]
    pub fn length(&self) -> u64 {
        self.length
    }

    /// Return the section's BLAKE3 digest.
    #[must_use]
    pub fn blake3(&self) -> &str {
        &self.blake3
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CapsuleFooter {
    version: u32,
    base_root_digest: String,
    transaction_id: String,
    sections: Vec<CapsuleSectionLocation>,
    git_packs: Vec<CapsuleGitPackDescriptor>,
}

/// An immutable, locally verified request-minimal publication capsule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capsule {
    bytes: Bytes,
    hash: String,
    footer: CapsuleFooter,
}

impl Capsule {
    /// Build and verify a capsule containing one canonical ref transaction and payload sections.
    pub fn build(
        transaction: &CapsuleTransaction,
        git_packs: Vec<CapsuleGitPack>,
        sections: Vec<CapsuleSection>,
    ) -> Result<Self> {
        let git_section_count = git_packs
            .len()
            .checked_mul(4)
            .ok_or_else(|| contract_error("capsule Git pack section count overflowed"))?;
        let payload_section_count = git_section_count
            .checked_add(sections.len())
            .ok_or_else(|| contract_error("capsule payload section count overflowed"))?;
        if payload_section_count >= MAX_CAPSULE_SECTIONS {
            return Err(contract_error(format!(
                "capsule has too many payload sections (maximum {})",
                MAX_CAPSULE_SECTIONS - 1
            )));
        }
        if sections
            .iter()
            .any(|section| is_reserved_git_section(section.kind))
        {
            return Err(contract_error(
                "caller payload cannot contain transaction or unbound Git sections",
            ));
        }
        let transaction_bytes = transaction.encode()?;
        let mut encoded_sections = Vec::with_capacity(payload_section_count + 1);
        encoded_sections.push((CapsuleSectionKind::RefTransaction, transaction_bytes));
        let mut descriptors = Vec::with_capacity(git_packs.len());
        for pack in git_packs {
            validate_git_pack_input(&pack)?;
            let first = u32::try_from(encoded_sections.len())
                .map_err(|_| contract_error("capsule section index cannot be represented"))?;
            encoded_sections.push((CapsuleSectionKind::GitPack, pack.pack));
            encoded_sections.push((CapsuleSectionKind::GitIndex, pack.index));
            encoded_sections.push((CapsuleSectionKind::GitReverseIndex, pack.reverse_index));
            encoded_sections.push((CapsuleSectionKind::GitObjectLocator, pack.locator));
            descriptors.push(CapsuleGitPackDescriptor {
                pack_section: first,
                index_section: first + 1,
                reverse_index_section: first + 2,
                locator_section: first + 3,
                git_checksum: pack.git_checksum,
                object_count: pack.object_count,
            });
        }
        encoded_sections.extend(
            sections
                .into_iter()
                .map(|section| (section.kind, section.bytes)),
        );

        let mut body = Vec::new();
        let mut locations = Vec::with_capacity(encoded_sections.len());
        for (kind, bytes) in encoded_sections {
            if bytes.is_empty() {
                return Err(contract_error("capsule sections must not be empty"));
            }
            let offset = u64::try_from(body.len()).map_err(|_| {
                contract_error("capsule section offset cannot be represented as u64")
            })?;
            let length = u64::try_from(bytes.len()).map_err(|_| {
                contract_error("capsule section length cannot be represented as u64")
            })?;
            body.extend_from_slice(&bytes);
            locations.push(CapsuleSectionLocation {
                kind,
                offset,
                length,
                blake3: blake3::hash(&bytes).to_hex().to_string(),
            });
        }

        let footer = CapsuleFooter {
            version: CAPSULE_VERSION,
            base_root_digest: transaction.base_root_digest().to_owned(),
            transaction_id: transaction.id()?,
            sections: locations,
            git_packs: descriptors,
        };
        let footer_bytes = serde_json::to_vec(&footer).map_err(|source| {
            MetadataError::Internal(format!("capsule footer serialization failed: {source}"))
        })?;
        if footer_bytes.len() > MAX_CAPSULE_FOOTER_BYTES {
            return Err(contract_error(format!(
                "capsule footer exceeds {MAX_CAPSULE_FOOTER_BYTES} bytes"
            )));
        }
        let footer_length = u64::try_from(footer_bytes.len())
            .map_err(|_| contract_error("capsule footer length cannot be represented as u64"))?;
        body.extend_from_slice(&footer_bytes);
        body.extend_from_slice(&footer_length.to_be_bytes());
        body.extend_from_slice(blake3::hash(&footer_bytes).as_bytes());
        body.extend_from_slice(CAPSULE_MAGIC);
        Self::decode(Bytes::from(body))
    }

    /// Decode and verify a complete capsule and every authenticated section.
    pub fn decode(bytes: Bytes) -> Result<Self> {
        if bytes.len() < CAPSULE_TRAILER_BYTES {
            return Err(corrupt("capsule is shorter than its trailer"));
        }
        let trailer_start = bytes.len() - CAPSULE_TRAILER_BYTES;
        let footer_length = u64::from_be_bytes(
            bytes[trailer_start..trailer_start + 8]
                .try_into()
                .map_err(|_| corrupt("capsule footer length is truncated"))?,
        );
        if &bytes[bytes.len() - CAPSULE_MAGIC.len()..] != CAPSULE_MAGIC {
            return Err(corrupt("capsule magic is invalid"));
        }
        let footer_length = usize::try_from(footer_length)
            .map_err(|_| corrupt("capsule footer length cannot be represented"))?;
        if footer_length > MAX_CAPSULE_FOOTER_BYTES || footer_length > trailer_start {
            return Err(corrupt("capsule footer length is out of bounds"));
        }
        let footer_start = trailer_start - footer_length;
        let footer_bytes = &bytes[footer_start..trailer_start];
        let expected_footer_hash = &bytes[trailer_start + 8..trailer_start + 40];
        if blake3::hash(footer_bytes).as_bytes() != expected_footer_hash {
            return Err(corrupt("capsule footer hash does not match"));
        }
        let footer: CapsuleFooter = serde_json::from_slice(footer_bytes)
            .map_err(|source| corrupt(format!("capsule footer is invalid JSON: {source}")))?;
        validate_footer(&footer, &bytes[..footer_start])?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        Ok(Self {
            bytes,
            hash,
            footer,
        })
    }

    /// Return the complete encoded capsule bytes.
    #[must_use]
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// Return the capsule's BLAKE3 object identity.
    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Return the exact repository-root digest this capsule extends.
    #[must_use]
    pub fn base_root_digest(&self) -> &str {
        &self.footer.base_root_digest
    }

    /// Return the canonical embedded ref transaction identity.
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.footer.transaction_id
    }

    /// Return authenticated locations for all capsule sections.
    #[must_use]
    pub fn sections(&self) -> &[CapsuleSectionLocation] {
        &self.footer.sections
    }

    /// Return every authenticated Git pack descriptor in publication order.
    #[must_use]
    pub fn git_packs(&self) -> &[CapsuleGitPackDescriptor] {
        &self.footer.git_packs
    }

    /// Return the authenticated bytes for one section owned by this capsule.
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
}

fn validate_footer(footer: &CapsuleFooter, body: &[u8]) -> Result<()> {
    if footer.version != CAPSULE_VERSION {
        return Err(corrupt(format!(
            "capsule must use version {CAPSULE_VERSION}"
        )));
    }
    validate_content_hash(
        &footer.base_root_digest,
        "capsule base root digest",
        "request-minimal capsule",
    )?;
    validate_content_hash(
        &footer.transaction_id,
        "capsule transaction id",
        "request-minimal capsule",
    )?;
    if footer.sections.is_empty() || footer.sections.len() > MAX_CAPSULE_SECTIONS {
        return Err(corrupt("capsule section count is out of bounds"));
    }
    if footer.sections[0].kind != CapsuleSectionKind::RefTransaction
        || footer.sections[1..]
            .iter()
            .any(|section| section.kind == CapsuleSectionKind::RefTransaction)
    {
        return Err(corrupt(
            "capsule must contain exactly one leading ref transaction section",
        ));
    }
    validate_git_pack_descriptors(footer)?;
    let mut expected_offset = 0u64;
    for location in &footer.sections {
        validate_content_hash(
            &location.blake3,
            "capsule section hash",
            "request-minimal capsule",
        )?;
        if location.length == 0 || location.offset != expected_offset {
            return Err(corrupt("capsule sections must be non-empty and contiguous"));
        }
        let end = location
            .offset
            .checked_add(location.length)
            .ok_or_else(|| corrupt("capsule section range overflowed"))?;
        let start = usize::try_from(location.offset)
            .map_err(|_| corrupt("capsule section offset cannot be represented"))?;
        let end_usize = usize::try_from(end)
            .map_err(|_| corrupt("capsule section end cannot be represented"))?;
        let section = body
            .get(start..end_usize)
            .ok_or_else(|| corrupt("capsule section range is out of bounds"))?;
        if blake3::hash(section).to_hex().as_str() != location.blake3 {
            return Err(corrupt("capsule section hash does not match"));
        }
        expected_offset = end;
    }
    if expected_offset != body.len() as u64 {
        return Err(corrupt("capsule sections do not cover the complete body"));
    }
    let transaction_location = &footer.sections[0];
    let transaction_end = transaction_location
        .offset
        .checked_add(transaction_location.length)
        .ok_or_else(|| corrupt("capsule transaction range overflowed"))?;
    let transaction_end = usize::try_from(transaction_end)
        .map_err(|_| corrupt("capsule transaction range cannot be represented"))?;
    let transaction_bytes = &body[..transaction_end];
    if blake3::hash(transaction_bytes).to_hex().as_str() != footer.transaction_id {
        return Err(corrupt(
            "capsule transaction identity does not match its section",
        ));
    }
    let transaction = CapsuleTransaction::decode(transaction_bytes)?;
    if transaction.base_root_digest() != footer.base_root_digest {
        return Err(corrupt(
            "capsule transaction base does not match the footer base",
        ));
    }
    Ok(())
}

fn validate_git_pack_input(pack: &CapsuleGitPack) -> Result<()> {
    if pack.pack.is_empty()
        || pack.index.is_empty()
        || pack.reverse_index.is_empty()
        || pack.locator.is_empty()
    {
        return Err(contract_error(
            "Git pack, index, reverse index, and locator must all be non-empty",
        ));
    }
    validate_sha1(
        &pack.git_checksum,
        "capsule Git checksum",
        "request-minimal capsule",
    )?;
    if pack.object_count == 0 {
        return Err(contract_error("capsule Git pack must contain an object"));
    }
    Ok(())
}

fn validate_git_pack_descriptors(footer: &CapsuleFooter) -> Result<()> {
    let mut claimed = vec![false; footer.sections.len()];
    claimed[0] = true;
    for descriptor in &footer.git_packs {
        validate_sha1(
            &descriptor.git_checksum,
            "capsule Git checksum",
            "request-minimal capsule",
        )?;
        if descriptor.object_count == 0 {
            return Err(corrupt("capsule Git pack has zero objects"));
        }
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
        for (index, expected_kind) in bindings {
            let index = usize::try_from(index)
                .map_err(|_| corrupt("capsule Git section index cannot be represented"))?;
            let location = footer
                .sections
                .get(index)
                .ok_or_else(|| corrupt("capsule Git section index is out of bounds"))?;
            if location.kind != expected_kind {
                return Err(corrupt(
                    "capsule Git descriptor section kind does not match",
                ));
            }
            if claimed[index] {
                return Err(corrupt("capsule Git section is claimed more than once"));
            }
            claimed[index] = true;
        }
    }
    for (index, location) in footer.sections.iter().enumerate().skip(1) {
        if is_git_section(location.kind) != claimed[index] {
            return Err(corrupt(
                "capsule Git sections must belong to exactly one pack descriptor",
            ));
        }
    }
    Ok(())
}

fn is_reserved_git_section(kind: CapsuleSectionKind) -> bool {
    kind == CapsuleSectionKind::RefTransaction || is_git_section(kind)
}

fn is_git_section(kind: CapsuleSectionKind) -> bool {
    matches!(
        kind,
        CapsuleSectionKind::GitPack
            | CapsuleSectionKind::GitIndex
            | CapsuleSectionKind::GitReverseIndex
            | CapsuleSectionKind::GitObjectLocator
    )
}

fn contract_error(reason: impl Into<String>) -> MetadataError {
    MetadataError::RequestMinimalContract {
        record: "capsule",
        reason: reason.into(),
    }
}

fn corrupt(reason: impl Into<String>) -> MetadataError {
    MetadataError::CorruptObject {
        path: "request-minimal capsule".to_owned(),
        reason: reason.into(),
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::request_minimal::{CapsuleRefEdit, CapsuleTransaction};

    fn transaction() -> CapsuleTransaction {
        CapsuleTransaction::new(
            &"1".repeat(64),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                Some("2".repeat(40)),
                Some("3".repeat(40)),
                None,
            )],
        )
        .unwrap()
    }

    fn git_pack() -> CapsuleGitPack {
        CapsuleGitPack::new(
            Bytes::from_static(b"PACK payload"),
            Bytes::from_static(b"index payload"),
            Bytes::from_static(b"reverse index payload"),
            Bytes::from_static(b"locator payload"),
            "4".repeat(40),
            1,
        )
        .unwrap()
    }

    #[test]
    fn capsule_round_trip_authenticates_every_section() {
        let capsule = Capsule::build(&transaction(), vec![git_pack()], Vec::new()).unwrap();

        let decoded = Capsule::decode(capsule.bytes().clone()).unwrap();

        assert_eq!(decoded.hash(), capsule.hash());
        assert_eq!(decoded.transaction_id(), transaction().id().unwrap());
        assert_eq!(decoded.sections().len(), 5);
        assert_eq!(decoded.git_packs().len(), 1);
        assert_eq!(
            decoded
                .section_bytes(decoded.git_packs()[0].pack_section())
                .unwrap(),
            Bytes::from_static(b"PACK payload")
        );
    }

    #[test]
    fn capsule_rejects_corrupt_payload() {
        let capsule = Capsule::build(&transaction(), vec![git_pack()], Vec::new()).unwrap();
        let mut bytes = capsule.bytes().to_vec();
        bytes[0] ^= 1;

        let error = Capsule::decode(Bytes::from(bytes)).expect_err("corruption must fail");

        assert!(matches!(error, MetadataError::CorruptObject { .. }));
    }

    #[test]
    fn capsule_rejects_unbound_git_sections() {
        let error = Capsule::build(
            &transaction(),
            Vec::new(),
            vec![CapsuleSection::new(
                CapsuleSectionKind::GitIndex,
                Bytes::from_static(b"orphan index"),
            )],
        )
        .expect_err("Git evidence must be bound to one descriptor");

        assert!(matches!(
            error,
            MetadataError::RequestMinimalContract { .. }
        ));
    }

    #[test]
    fn capsule_rejects_cross_pack_section_binding() {
        let capsule =
            Capsule::build(&transaction(), vec![git_pack(), git_pack()], Vec::new()).unwrap();
        let mut footer = capsule.footer.clone();
        footer.git_packs[1].index_section = footer.git_packs[0].index_section;

        let error = validate_git_pack_descriptors(&footer)
            .expect_err("one section cannot authenticate evidence for two packs");

        assert!(matches!(error, MetadataError::CorruptObject { .. }));
    }

    #[test]
    fn transaction_encoding_is_independent_of_input_edit_order() {
        let edits = BTreeMap::from([
            ("refs/heads/a", "2".repeat(40)),
            ("refs/heads/b", "3".repeat(40)),
        ]);
        let forward = CapsuleTransaction::new(
            &"1".repeat(64),
            edits
                .iter()
                .map(|(name, oid)| CapsuleRefEdit::new(*name, None, Some(oid.clone()), None))
                .collect(),
        )
        .unwrap();
        let reverse = CapsuleTransaction::new(
            &"1".repeat(64),
            edits
                .iter()
                .rev()
                .map(|(name, oid)| CapsuleRefEdit::new(*name, None, Some(oid.clone()), None))
                .collect(),
        )
        .unwrap();

        assert_eq!(forward.id().unwrap(), reverse.id().unwrap());
    }
}
