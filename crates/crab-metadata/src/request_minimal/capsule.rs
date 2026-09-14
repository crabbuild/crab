use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::error::{MetadataError, Result};
use crate::request_minimal::CapsuleTransaction;
use crate::validation::validate_content_hash;

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
    kind: CapsuleSectionKind,
    offset: u64,
    length: u64,
    blake3: String,
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
    pub fn build(transaction: &CapsuleTransaction, sections: Vec<CapsuleSection>) -> Result<Self> {
        if sections.len() >= MAX_CAPSULE_SECTIONS {
            return Err(contract_error(format!(
                "capsule has too many payload sections (maximum {})",
                MAX_CAPSULE_SECTIONS - 1
            )));
        }
        if sections
            .iter()
            .any(|section| section.kind == CapsuleSectionKind::RefTransaction)
        {
            return Err(contract_error(
                "caller payload cannot contain a ref transaction section",
            ));
        }
        let transaction_bytes = transaction.encode()?;
        let mut encoded_sections = Vec::with_capacity(sections.len() + 1);
        encoded_sections.push((CapsuleSectionKind::RefTransaction, transaction_bytes));
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

    #[test]
    fn capsule_round_trip_authenticates_every_section() {
        let capsule = Capsule::build(
            &transaction(),
            vec![CapsuleSection::new(
                CapsuleSectionKind::GitPack,
                Bytes::from_static(b"PACK payload"),
            )],
        )
        .unwrap();

        let decoded = Capsule::decode(capsule.bytes().clone()).unwrap();

        assert_eq!(decoded.hash(), capsule.hash());
        assert_eq!(decoded.transaction_id(), transaction().id().unwrap());
        assert_eq!(decoded.sections().len(), 2);
    }

    #[test]
    fn capsule_rejects_corrupt_payload() {
        let capsule = Capsule::build(
            &transaction(),
            vec![CapsuleSection::new(
                CapsuleSectionKind::GitPack,
                Bytes::from_static(b"PACK payload"),
            )],
        )
        .unwrap();
        let mut bytes = capsule.bytes().to_vec();
        bytes[0] ^= 1;

        let error = Capsule::decode(Bytes::from(bytes)).expect_err("corruption must fail");

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
