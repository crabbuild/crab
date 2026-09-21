use std::collections::BTreeMap;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::capsule_protocol::{
    Capsule, CapsuleSectionKind, CapsuleTransaction, CapsuleVisibilityDelta, PackMemberDescriptor,
    PackRange, PackSourceDescriptor, PackSourceKind, PointerCatalog,
};
use crate::error::{MetadataError, Result};
use crate::validation::validate_content_hash;

const RUN_MAGIC: &[u8; 8] = b"CRBRUN04";
const RUN_VERSION: u32 = 4;
const RUN_TRAILER_BYTES: usize = 8 + 32 + RUN_MAGIC.len();
const MAX_RUN_FOOTER_BYTES: usize = 8 * 1024 * 1024;
const MAX_INLINE_RUN_CONTROL_SECTION_BYTES: usize = 512 * 1024;
const ADMISSION_MAGIC: &[u8; 8] = b"CRBADM01";
const ADMISSION_VERSION: u32 = 1;
const ADMISSION_HEADER_BYTES: usize = ADMISSION_MAGIC.len() + 4 + 4 + 8;
const MAX_RUN_ADMISSION_BYTES: usize = 128 * 1024 * 1024;
const MAX_RUN_ADMISSION_ENTRIES: usize = 8_000_000;
/// Largest number of push capsules coalesced before a repository checkpoint.
pub const MAX_CAPSULES_PER_RUN: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunCapsuleLocation {
    offset: u64,
    length: u64,
    hash: String,
    transaction_id: String,
    base_root_digest: String,
    transaction: RunSectionLocation,
    #[serde(default)]
    visibility: Option<RunSectionLocation>,
    #[serde(default)]
    catalog: Option<RunSectionLocation>,
    control: RunControlSections,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunSectionLocation {
    offset: u64,
    length: u64,
    hash: String,
}

/// Duplicated control bytes authenticated by the run footer.
///
/// The nested capsule body remains immutable and range-addressable, but warm
/// readers must not issue one object-store range request per control section.
/// Keeping these bounded sections in the run footer makes one suffix read the
/// complete transaction/ref proof while the footer's ranges still bind each
/// byte to its original capsule section. An optional section with a committed
/// range but no bytes is intentionally detached and loaded from that range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunControlSections {
    transaction: Vec<u8>,
    #[serde(default)]
    visibility: Option<Vec<u8>>,
    #[serde(default)]
    catalog: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CapsuleRunFooter {
    version: u32,
    level: u8,
    capsules: Vec<RunCapsuleLocation>,
    git_packs: Vec<PackMemberDescriptor>,
    #[serde(default)]
    admission: Option<RunSectionLocation>,
}

/// Exact object-to-member admission for one immutable capsule run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapsuleRunAdmission {
    members: BTreeMap<[u8; 20], Vec<u32>>,
}

impl CapsuleRunAdmission {
    fn from_member_oids(member_oids: &[Vec<[u8; 20]>], member_count: usize) -> Result<Self> {
        if member_oids.len() != member_count {
            return Err(contract_error(
                "capsule run admission member count does not match the pack directory",
            ));
        }
        let mut members = BTreeMap::new();
        for (member_index, oids) in member_oids.iter().enumerate() {
            let member_index = u32::try_from(member_index)
                .map_err(|_| contract_error("capsule run admission member index overflowed"))?;
            let mut oids = oids.clone();
            oids.sort_unstable();
            oids.dedup();
            for oid in oids {
                members
                    .entry(oid)
                    .or_insert_with(Vec::new)
                    .push(member_index);
            }
        }
        Ok(Self { members })
    }

    fn merge(&self, newer: &Self, member_offset: usize) -> Result<Self> {
        let member_offset = u32::try_from(member_offset)
            .map_err(|_| contract_error("capsule run admission member offset overflowed"))?;
        let mut members = self.members.clone();
        for (oid, newer_members) in &newer.members {
            let entry = members.entry(*oid).or_default();
            for member in newer_members {
                entry.push(member.checked_add(member_offset).ok_or_else(|| {
                    contract_error("capsule run admission member index overflowed")
                })?);
            }
            entry.sort_unstable();
            entry.dedup();
        }
        Ok(Self { members })
    }

    fn encode(&self, member_count: usize) -> Result<Bytes> {
        if self.members.len() > MAX_RUN_ADMISSION_ENTRIES {
            return Err(contract_error("capsule run admission has too many objects"));
        }
        let member_count = u32::try_from(member_count)
            .map_err(|_| contract_error("capsule run admission member count overflowed"))?;
        let entry_count = u64::try_from(self.members.len())
            .map_err(|_| contract_error("capsule run admission object count overflowed"))?;
        let mut bytes = Vec::with_capacity(
            ADMISSION_HEADER_BYTES.saturating_add(self.members.len().saturating_mul(28)),
        );
        bytes.extend_from_slice(ADMISSION_MAGIC);
        bytes.extend_from_slice(&ADMISSION_VERSION.to_be_bytes());
        bytes.extend_from_slice(&member_count.to_be_bytes());
        bytes.extend_from_slice(&entry_count.to_be_bytes());
        let mut previous = None;
        for (oid, members) in &self.members {
            if members.is_empty()
                || members.windows(2).any(|pair| pair[0] >= pair[1])
                || members.iter().any(|member| *member >= member_count)
            {
                return Err(contract_error(
                    "capsule run admission members are not canonical",
                ));
            }
            if previous.is_some_and(|previous| previous >= *oid) {
                return Err(contract_error(
                    "capsule run admission objects are not canonical",
                ));
            }
            previous = Some(*oid);
            bytes.extend_from_slice(oid);
            let count = u32::try_from(members.len())
                .map_err(|_| contract_error("capsule run admission member list overflowed"))?;
            bytes.extend_from_slice(&count.to_be_bytes());
            for member in members {
                bytes.extend_from_slice(&member.to_be_bytes());
            }
        }
        if bytes.len() > MAX_RUN_ADMISSION_BYTES {
            return Err(contract_error(
                "capsule run admission exceeds its size bound",
            ));
        }
        Ok(Bytes::from(bytes))
    }

    fn decode(bytes: &[u8], expected_member_count: usize) -> Result<Self> {
        if bytes.len() < ADMISSION_HEADER_BYTES || bytes.len() > MAX_RUN_ADMISSION_BYTES {
            return Err(corrupt("capsule run admission has an invalid size"));
        }
        let mut cursor = 0;
        let take = |cursor: &mut usize, length: usize| -> Result<&[u8]> {
            let end = cursor
                .checked_add(length)
                .ok_or_else(|| corrupt("capsule run admission offset overflowed"))?;
            let bytes = bytes
                .get(*cursor..end)
                .ok_or_else(|| corrupt("capsule run admission is truncated"))?;
            *cursor = end;
            Ok(bytes)
        };
        if take(&mut cursor, ADMISSION_MAGIC.len())? != ADMISSION_MAGIC {
            return Err(corrupt("capsule run admission magic is invalid"));
        }
        let version = u32::from_be_bytes(
            take(&mut cursor, 4)?
                .try_into()
                .map_err(|_| corrupt("capsule run admission version is truncated"))?,
        );
        if version != ADMISSION_VERSION {
            return Err(corrupt("capsule run admission version is unsupported"));
        }
        let member_count = u32::from_be_bytes(
            take(&mut cursor, 4)?
                .try_into()
                .map_err(|_| corrupt("capsule run admission member count is truncated"))?,
        );
        if usize::try_from(member_count).ok() != Some(expected_member_count) {
            return Err(corrupt(
                "capsule run admission member count does not match the pack directory",
            ));
        }
        let entry_count = u64::from_be_bytes(
            take(&mut cursor, 8)?
                .try_into()
                .map_err(|_| corrupt("capsule run admission object count is truncated"))?,
        );
        let entry_count = usize::try_from(entry_count)
            .map_err(|_| corrupt("capsule run admission object count overflows"))?;
        if entry_count > MAX_RUN_ADMISSION_ENTRIES {
            return Err(corrupt("capsule run admission has too many objects"));
        }
        let mut members = BTreeMap::new();
        let mut previous = None;
        for _ in 0..entry_count {
            let oid: [u8; 20] = take(&mut cursor, 20)?
                .try_into()
                .map_err(|_| corrupt("capsule run admission object ID is truncated"))?;
            if previous.is_some_and(|previous| previous >= oid) {
                return Err(corrupt("capsule run admission objects are not sorted"));
            }
            previous = Some(oid);
            let member_count = u32::from_be_bytes(
                take(&mut cursor, 4)?
                    .try_into()
                    .map_err(|_| corrupt("capsule run admission member list is truncated"))?,
            );
            if member_count == 0 {
                return Err(corrupt("capsule run admission has an empty member list"));
            }
            let mut oid_members = Vec::with_capacity(
                usize::try_from(member_count)
                    .map_err(|_| corrupt("capsule run admission member list overflows"))?,
            );
            for _ in 0..member_count {
                let member = u32::from_be_bytes(
                    take(&mut cursor, 4)?
                        .try_into()
                        .map_err(|_| corrupt("capsule run admission member is truncated"))?,
                );
                if member >= u32::try_from(expected_member_count).unwrap_or(u32::MAX)
                    || oid_members
                        .last()
                        .is_some_and(|previous| *previous >= member)
                {
                    return Err(corrupt("capsule run admission members are invalid"));
                }
                oid_members.push(member);
            }
            members.insert(oid, oid_members);
        }
        if cursor != bytes.len() {
            return Err(corrupt("capsule run admission has trailing bytes"));
        }
        Ok(Self { members })
    }

    /// Return the member ordinals that contain one Git object.
    #[must_use]
    pub fn object_members(&self, oid: &[u8; 20]) -> Option<&[u32]> {
        self.members.get(oid).map(Vec::as_slice)
    }

    /// Return every admitted object and the run members that contain it.
    #[must_use]
    pub fn entries(&self) -> impl Iterator<Item = (&[u8; 20], &[u32])> {
        self.members
            .iter()
            .map(|(oid, members)| (oid, members.as_slice()))
    }
}

/// Immutable power-of-two run of complete push capsules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapsuleRun {
    bytes: Bytes,
    hash: String,
    footer: CapsuleRunFooter,
    footer_length: u64,
    capsules: Vec<Capsule>,
    git_packs: Vec<PackMemberDescriptor>,
    admission: Option<CapsuleRunAdmission>,
}

/// Authenticated control-only view of one capsule run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapsuleRunControl {
    hash: String,
    object_size: u64,
    control_offset: u64,
    control_size: u64,
    level: u8,
    footer_hash: String,
    capsules: Vec<CapsuleControlLocation>,
    git_packs: Vec<PackMemberDescriptor>,
    admission_range: Option<PackRange>,
    admission: Option<CapsuleRunAdmission>,
}

/// Ranges for the transaction and optional metadata sections of one capsule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapsuleControlLocation {
    hash: String,
    transaction_id: String,
    base_root_digest: String,
    transaction: PackRange,
    visibility: Option<PackRange>,
    catalog: Option<PackRange>,
    transaction_bytes: Bytes,
    visibility_bytes: Option<Bytes>,
    catalog_bytes: Option<Bytes>,
}

/// Transaction and metadata sections needed to validate a run without reading
/// its Git/file payload sections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapsuleControl {
    hash: String,
    transaction_id: String,
    base_root_digest: String,
    transaction: CapsuleTransaction,
    visibility: Option<CapsuleVisibilityDelta>,
    catalog: Option<PointerCatalog>,
    git_packs: Vec<PackMemberDescriptor>,
}

impl CapsuleRunControl {
    /// Return the minimum trailer range needed before loading the footer.
    pub const fn trailer_bytes() -> usize {
        RUN_TRAILER_BYTES
    }

    /// Return the complete control-suffix length encoded by a run trailer.
    #[cfg(feature = "storage")]
    pub(crate) fn suffix_length_from_trailer(trailer: &[u8]) -> Result<u64> {
        if trailer.len() != RUN_TRAILER_BYTES
            || &trailer[trailer.len() - RUN_MAGIC.len()..] != RUN_MAGIC
        {
            return Err(corrupt("capsule run trailer is invalid"));
        }
        let footer_length = u64::from_be_bytes(
            trailer[..8]
                .try_into()
                .map_err(|_| corrupt("capsule run footer length is truncated"))?,
        );
        if footer_length == 0 || footer_length > MAX_RUN_FOOTER_BYTES as u64 {
            return Err(corrupt("capsule run footer length is out of bounds"));
        }
        footer_length
            .checked_add(RUN_TRAILER_BYTES as u64)
            .ok_or_else(|| corrupt("capsule run control suffix length overflowed"))
    }

    /// Decode an authenticated run footer from its control suffix.
    pub fn decode_suffix(
        bytes: Bytes,
        object_size: u64,
        expected_hash: &str,
        expected_level: u8,
        expected_transactions: &[String],
        expected_newest_base: &str,
    ) -> Result<Self> {
        if bytes.len() < RUN_TRAILER_BYTES {
            return Err(corrupt("capsule run control suffix is truncated"));
        }
        let trailer_start = bytes.len() - RUN_TRAILER_BYTES;
        if &bytes[bytes.len() - RUN_MAGIC.len()..] != RUN_MAGIC {
            return Err(corrupt("capsule run control magic is invalid"));
        }
        let footer_length = usize::try_from(u64::from_be_bytes(
            bytes[trailer_start..trailer_start + 8]
                .try_into()
                .map_err(|_| corrupt("capsule run footer length is truncated"))?,
        ))
        .map_err(|_| corrupt("capsule run footer length cannot be represented"))?;
        if footer_length == 0
            || footer_length > MAX_RUN_FOOTER_BYTES
            || footer_length > trailer_start
        {
            return Err(corrupt("capsule run footer length is out of bounds"));
        }
        let footer_start = trailer_start - footer_length;
        let footer_bytes = &bytes[footer_start..trailer_start];
        if blake3::hash(footer_bytes).as_bytes() != &bytes[trailer_start + 8..trailer_start + 40] {
            return Err(corrupt("capsule run footer hash does not match"));
        }
        let footer: CapsuleRunFooter = serde_json::from_slice(footer_bytes)
            .map_err(|source| corrupt(format!("capsule run footer is invalid JSON: {source}")))?;
        if footer.version != RUN_VERSION {
            return Err(corrupt("capsule run footer version is unsupported"));
        }
        validate_level_count(footer.level, footer.capsules.len())
            .map_err(|error| corrupt(error.to_string()))?;
        let control_offset =
            object_size
                .checked_sub(u64::try_from(bytes.len()).map_err(|_| {
                    corrupt("capsule run control suffix length cannot be represented")
                })?)
                .ok_or_else(|| corrupt("capsule run control suffix starts outside its object"))?;
        let mut controls = Vec::with_capacity(footer.capsules.len());
        let mut expected_offset = 0_u64;
        for location in &footer.capsules {
            validate_location(location, expected_offset)?;
            let transaction = &location.transaction;
            validate_control_range(transaction, control_offset, object_size)?;
            if let Some(range) = location.visibility.as_ref() {
                validate_control_range(range, control_offset, object_size)?;
            }
            if let Some(range) = location.catalog.as_ref() {
                validate_control_range(range, control_offset, object_size)?;
            }
            let transaction_bytes = Bytes::from(location.control.transaction.clone());
            let transaction_range = PackRange::from_parts(
                transaction.offset,
                transaction.length,
                transaction.hash.clone(),
            )?;
            verify_control_section(
                Some(&transaction_range),
                Some(&transaction_bytes),
                "transaction",
            )?;
            let visibility_bytes = location.control.visibility.clone().map(Bytes::from);
            let visibility_range = location
                .visibility
                .as_ref()
                .map(|range| PackRange::from_parts(range.offset, range.length, range.hash.clone()))
                .transpose()?;
            verify_embedded_control_section(
                visibility_range.as_ref(),
                visibility_bytes.as_ref(),
                "visibility",
            )?;
            let catalog_bytes = location.control.catalog.clone().map(Bytes::from);
            let catalog_range = location
                .catalog
                .as_ref()
                .map(|range| PackRange::from_parts(range.offset, range.length, range.hash.clone()))
                .transpose()?;
            verify_embedded_control_section(
                catalog_range.as_ref(),
                catalog_bytes.as_ref(),
                "catalog",
            )?;
            controls.push(CapsuleControlLocation {
                hash: location.hash.clone(),
                transaction_id: location.transaction_id.clone(),
                base_root_digest: location.base_root_digest.clone(),
                transaction: transaction_range,
                visibility: visibility_range,
                catalog: catalog_range,
                transaction_bytes,
                visibility_bytes,
                catalog_bytes,
            });
            expected_offset = location
                .offset
                .checked_add(location.length)
                .ok_or_else(|| corrupt("capsule run range overflowed"))?;
        }
        if let Some(admission) = footer.admission.as_ref() {
            validate_body_range(admission, expected_offset, control_offset)?;
            expected_offset = admission
                .offset
                .checked_add(admission.length)
                .ok_or_else(|| corrupt("capsule run admission range overflowed"))?;
        }
        if expected_offset != control_offset {
            return Err(corrupt(
                "capsule run controls do not cover its complete body",
            ));
        }
        let admission_range = footer
            .admission
            .as_ref()
            .map(|range| PackRange::from_parts(range.offset, range.length, range.hash.clone()))
            .transpose()?;
        let footer_hash = blake3::hash(footer_bytes).to_hex().to_string();
        let transactions = controls
            .iter()
            .map(|control| control.transaction_id.clone())
            .collect::<Vec<_>>();
        if transactions != expected_transactions
            || controls
                .last()
                .map(|control| control.base_root_digest.as_str())
                != Some(expected_newest_base)
            || footer.level != expected_level
        {
            return Err(corrupt(
                "capsule run control does not match its authenticated pointer",
            ));
        }
        Ok(Self {
            hash: expected_hash.to_owned(),
            object_size,
            control_offset,
            control_size: u64::try_from(bytes.len())
                .map_err(|_| corrupt("capsule run control suffix length overflows"))?,
            level: footer.level,
            footer_hash,
            capsules: controls,
            git_packs: footer.git_packs,
            admission_range,
            admission: None,
        })
    }

    /// Return the run's authenticated capsule controls.
    #[must_use]
    pub fn capsule_locations(&self) -> &[CapsuleControlLocation] {
        &self.capsules
    }

    /// Return the immutable run identity.
    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Return the run's authenticated pack directory.
    #[must_use]
    pub fn git_packs(&self) -> &[PackMemberDescriptor] {
        &self.git_packs
    }

    /// Return the range of the exact frontier admission sidecar, when present.
    #[must_use]
    pub fn admission_range(&self) -> Option<&PackRange> {
        self.admission_range.as_ref()
    }

    /// Return the decoded exact frontier admission sidecar, when loaded.
    #[must_use]
    pub fn admission(&self) -> Option<&CapsuleRunAdmission> {
        self.admission.as_ref()
    }

    /// Attach and verify the range-addressable admission sidecar.
    pub fn attach_admission(mut self, bytes: Bytes) -> Result<Self> {
        let Some(range) = self.admission_range.as_ref() else {
            return Err(corrupt("capsule run has no admission sidecar"));
        };
        if bytes.len() as u64 != range.length()
            || blake3::hash(&bytes).to_hex().as_str() != range.blake3()
        {
            return Err(corrupt("capsule run admission sidecar hash does not match"));
        }
        self.admission = Some(CapsuleRunAdmission::decode(&bytes, self.git_packs.len())?);
        Ok(self)
    }

    /// Return the BLAKE3 identity of the authenticated run footer.
    #[must_use]
    pub fn footer_hash(&self) -> &str {
        &self.footer_hash
    }

    /// Build the source descriptor without reading any capsule payload bytes.
    pub fn source_descriptor(&self) -> Result<PackSourceDescriptor> {
        PackSourceDescriptor::new(
            PackSourceKind::CapsuleRun,
            self.hash.clone(),
            self.object_size,
            self.control_offset,
            self.control_size,
            self.footer_hash.clone(),
            self.git_packs.clone(),
        )
    }

    /// Decode controls after detached sections have been fetched and verified.
    #[cfg(any(feature = "storage", test))]
    pub(crate) fn materialize_capsules_with_external_controls(
        &self,
        external_controls: &std::collections::BTreeMap<(String, CapsuleSectionKind), Bytes>,
    ) -> Result<Vec<CapsuleControl>> {
        self.capsules
            .iter()
            .map(|location| {
                verify_control_section(
                    Some(&location.transaction),
                    Some(&location.transaction_bytes),
                    "transaction",
                )?;
                let visibility_bytes = location.visibility_bytes.clone().or_else(|| {
                    external_controls
                        .get(&(location.hash.clone(), CapsuleSectionKind::VisibilityDelta))
                        .cloned()
                });
                let catalog_bytes = location.catalog_bytes.clone().or_else(|| {
                    external_controls
                        .get(&(location.hash.clone(), CapsuleSectionKind::CatalogDelta))
                        .cloned()
                });
                verify_control_section(
                    location.visibility.as_ref(),
                    visibility_bytes.as_ref(),
                    "visibility",
                )?;
                verify_control_section(
                    location.catalog.as_ref(),
                    catalog_bytes.as_ref(),
                    "catalog",
                )?;
                let transaction = CapsuleTransaction::decode(&location.transaction_bytes)?;
                if transaction.id()? != location.transaction_id
                    || transaction.base_root_digest() != location.base_root_digest
                {
                    return Err(corrupt("capsule run transaction does not match its footer"));
                }
                let visibility = visibility_bytes
                    .as_ref()
                    .map(|bytes| CapsuleVisibilityDelta::decode(bytes))
                    .transpose()?;
                let catalog = catalog_bytes
                    .as_ref()
                    .map(|bytes| PointerCatalog::decode_delta(bytes))
                    .transpose()?;
                Ok(CapsuleControl {
                    hash: location.hash.clone(),
                    transaction_id: location.transaction_id.clone(),
                    base_root_digest: location.base_root_digest.clone(),
                    transaction,
                    visibility,
                    catalog,
                    git_packs: self.git_packs.clone(),
                })
            })
            .collect()
    }
}

impl CapsuleControl {
    /// Return the immutable capsule identity.
    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Return the transaction identity.
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    /// Return the capsule base root identity.
    #[must_use]
    pub fn base_root_digest(&self) -> &str {
        &self.base_root_digest
    }

    /// Return the authenticated transaction.
    pub fn transaction(&self) -> &CapsuleTransaction {
        &self.transaction
    }

    /// Return the visibility delta, when present.
    pub fn visibility_delta(&self) -> Option<&CapsuleVisibilityDelta> {
        self.visibility.as_ref()
    }

    /// Return the pointer catalog delta, when present.
    pub fn pointer_catalog_delta(&self) -> Option<&PointerCatalog> {
        self.catalog.as_ref()
    }

    /// Return the authenticated pack directory for this capsule.
    pub fn git_packs(&self) -> &[PackMemberDescriptor] {
        &self.git_packs
    }
}

impl CapsuleControlLocation {
    /// Return the immutable capsule identity.
    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Return the transaction section range.
    #[must_use]
    pub fn transaction(&self) -> &PackRange {
        &self.transaction
    }

    /// Return the authenticated transaction identity.
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    /// Return the authenticated base-root identity.
    #[must_use]
    pub fn base_root_digest(&self) -> &str {
        &self.base_root_digest
    }

    /// Return the optional visibility section range.
    #[must_use]
    pub fn visibility(&self) -> Option<&PackRange> {
        self.visibility.as_ref()
    }

    /// Return the optional pointer catalog section range.
    #[must_use]
    pub fn catalog(&self) -> Option<&PackRange> {
        self.catalog.as_ref()
    }

    #[cfg(any(feature = "storage", test))]
    pub(crate) fn detached_controls(
        &self,
    ) -> impl Iterator<Item = (String, CapsuleSectionKind, PackRange)> + '_ {
        self.visibility
            .as_ref()
            .filter(|_| self.visibility_bytes.is_none())
            .map(|range| {
                (
                    self.hash.clone(),
                    CapsuleSectionKind::VisibilityDelta,
                    range.clone(),
                )
            })
            .into_iter()
            .chain(
                self.catalog
                    .as_ref()
                    .filter(|_| self.catalog_bytes.is_none())
                    .map(|range| {
                        (
                            self.hash.clone(),
                            CapsuleSectionKind::CatalogDelta,
                            range.clone(),
                        )
                    }),
            )
    }
}

impl CapsuleRun {
    /// Wrap one verified push capsule as a level-zero run.
    pub fn leaf(capsule: Capsule) -> Result<Self> {
        Self::encode(0, vec![capsule], None)
    }

    /// Wrap one verified push capsule with its exact Git object admission.
    pub fn leaf_with_member_oids(
        capsule: Capsule,
        member_oids: Vec<Vec<[u8; 20]>>,
    ) -> Result<Self> {
        let member_count = capsule.git_packs().len();
        let admission = CapsuleRunAdmission::from_member_oids(&member_oids, member_count)?;
        Self::encode(0, vec![capsule], Some(admission))
    }

    /// Merge adjacent equal-level runs without changing any capsule bytes.
    pub fn merge(&self, newer: &Self) -> Result<Self> {
        if self.level() != newer.level() {
            return Err(contract_error("only equal-level capsule runs can merge"));
        }
        let level = self
            .level()
            .checked_add(1)
            .ok_or_else(|| contract_error("capsule run level overflowed"))?;
        let mut capsules = Vec::with_capacity(
            self.capsules
                .len()
                .checked_add(newer.capsules.len())
                .ok_or_else(|| contract_error("capsule run count overflowed"))?,
        );
        capsules.extend(self.capsules.iter().cloned());
        capsules.extend(newer.capsules.iter().cloned());
        let admission = match (&self.admission, &newer.admission) {
            (Some(older), Some(newer)) => Some(older.merge(newer, self.git_packs.len())?),
            _ => None,
        };
        Self::encode(level, capsules, admission)
    }

    fn encode(
        level: u8,
        capsules: Vec<Capsule>,
        admission: Option<CapsuleRunAdmission>,
    ) -> Result<Self> {
        validate_level_count(level, capsules.len())?;
        let mut body = Vec::new();
        let mut locations = Vec::with_capacity(capsules.len());
        for capsule in &capsules {
            let offset = u64::try_from(body.len())
                .map_err(|_| contract_error("capsule run offset cannot be represented"))?;
            let length = u64::try_from(capsule.bytes().len())
                .map_err(|_| contract_error("capsule run length cannot be represented"))?;
            body.extend_from_slice(capsule.bytes());
            let transaction =
                section_location(capsule, offset, CapsuleSectionKind::RefTransaction)?
                    .ok_or_else(|| contract_error("capsule run transaction section is missing"))?;
            let visibility =
                section_location(capsule, offset, CapsuleSectionKind::VisibilityDelta)?;
            let catalog = section_location(capsule, offset, CapsuleSectionKind::CatalogDelta)?;
            let inline_visibility =
                section_bytes_for_kind(capsule, CapsuleSectionKind::VisibilityDelta)?
                    .filter(|bytes| bytes.len() <= MAX_INLINE_RUN_CONTROL_SECTION_BYTES)
                    .map(|bytes| bytes.to_vec());
            let inline_catalog = section_bytes_for_kind(capsule, CapsuleSectionKind::CatalogDelta)?
                .filter(|bytes| bytes.len() <= MAX_INLINE_RUN_CONTROL_SECTION_BYTES)
                .map(|bytes| bytes.to_vec());
            locations.push(RunCapsuleLocation {
                offset,
                length,
                hash: capsule.hash().to_owned(),
                transaction_id: capsule.transaction_id().to_owned(),
                base_root_digest: capsule.base_root_digest().to_owned(),
                transaction,
                visibility,
                catalog,
                control: RunControlSections {
                    transaction: capsule.section_bytes(0)?.to_vec(),
                    visibility: inline_visibility,
                    catalog: inline_catalog,
                },
            });
        }
        let git_packs = pack_members(&capsules, &locations)?;
        let admission = admission
            .filter(|admission| !admission.members.is_empty())
            .map(|admission| {
                if admission.members.values().flatten().any(|member| {
                    usize::try_from(*member)
                        .ok()
                        .is_none_or(|member| member >= git_packs.len())
                }) {
                    return Err(contract_error(
                        "capsule run admission references an absent pack member",
                    ));
                }
                let bytes = admission.encode(git_packs.len())?;
                let offset = u64::try_from(body.len())
                    .map_err(|_| contract_error("capsule run admission offset overflowed"))?;
                let length = u64::try_from(bytes.len())
                    .map_err(|_| contract_error("capsule run admission length overflowed"))?;
                body.extend_from_slice(&bytes);
                Ok((
                    admission,
                    RunSectionLocation {
                        offset,
                        length,
                        hash: blake3::hash(&bytes).to_hex().to_string(),
                    },
                ))
            })
            .transpose()?;
        let footer_bytes = loop {
            let footer = CapsuleRunFooter {
                version: RUN_VERSION,
                level,
                capsules: locations.clone(),
                git_packs: git_packs.clone(),
                admission: admission.as_ref().map(|(_, range)| range.clone()),
            };
            let footer_bytes = serde_json::to_vec(&footer).map_err(|source| {
                MetadataError::Internal(format!("capsule run serialization failed: {source}"))
            })?;
            if footer_bytes.len() <= MAX_RUN_FOOTER_BYTES {
                break footer_bytes;
            }

            let mut largest = None;
            for (index, location) in locations.iter().enumerate() {
                for (kind, bytes) in [
                    (
                        CapsuleSectionKind::VisibilityDelta,
                        location.control.visibility.as_ref(),
                    ),
                    (
                        CapsuleSectionKind::CatalogDelta,
                        location.control.catalog.as_ref(),
                    ),
                ] {
                    if let Some(bytes) = bytes
                        && largest
                            .as_ref()
                            .is_none_or(|(_, _, length)| *length < bytes.len())
                    {
                        largest = Some((index, kind, bytes.len()));
                    }
                }
            }
            let Some((index, kind, _)) = largest else {
                return Err(contract_error(format!(
                    "capsule run footer exceeds {MAX_RUN_FOOTER_BYTES} bytes"
                )));
            };
            let location = locations
                .get_mut(index)
                .ok_or_else(|| contract_error("capsule run control disappeared"))?;
            match kind {
                CapsuleSectionKind::VisibilityDelta => location.control.visibility = None,
                CapsuleSectionKind::CatalogDelta => location.control.catalog = None,
                _ => {
                    return Err(contract_error(
                        "capsule run selected a non-detachable control section",
                    ));
                }
            }
        };
        let footer_length = u64::try_from(footer_bytes.len())
            .map_err(|_| contract_error("capsule run footer length cannot be represented"))?;
        body.extend_from_slice(&footer_bytes);
        body.extend_from_slice(&footer_length.to_be_bytes());
        body.extend_from_slice(blake3::hash(&footer_bytes).as_bytes());
        body.extend_from_slice(RUN_MAGIC);
        Self::decode(Bytes::from(body))
    }

    /// Decode and verify a run plus every complete capsule it contains.
    pub fn decode(bytes: Bytes) -> Result<Self> {
        if bytes.len() < RUN_TRAILER_BYTES {
            return Err(corrupt("capsule run is shorter than its trailer"));
        }
        let trailer_start = bytes.len() - RUN_TRAILER_BYTES;
        if &bytes[bytes.len() - RUN_MAGIC.len()..] != RUN_MAGIC {
            return Err(corrupt("capsule run magic is invalid"));
        }
        let footer_length = u64::from_be_bytes(
            bytes[trailer_start..trailer_start + 8]
                .try_into()
                .map_err(|_| corrupt("capsule run footer length is truncated"))?,
        );
        let footer_length = usize::try_from(footer_length)
            .map_err(|_| corrupt("capsule run footer length cannot be represented"))?;
        if footer_length > MAX_RUN_FOOTER_BYTES || footer_length > trailer_start {
            return Err(corrupt("capsule run footer length is out of bounds"));
        }
        let footer_start = trailer_start - footer_length;
        let footer_bytes = &bytes[footer_start..trailer_start];
        if blake3::hash(footer_bytes).as_bytes() != &bytes[trailer_start + 8..trailer_start + 40] {
            return Err(corrupt("capsule run footer hash does not match"));
        }
        let footer: CapsuleRunFooter = serde_json::from_slice(footer_bytes)
            .map_err(|source| corrupt(format!("capsule run footer is invalid JSON: {source}")))?;
        if footer.version != RUN_VERSION {
            return Err(corrupt("capsule run footer version is unsupported"));
        }
        validate_level_count(footer.level, footer.capsules.len())
            .map_err(|error| corrupt(error.to_string()))?;
        let mut expected_offset = 0_u64;
        let mut capsules = Vec::with_capacity(footer.capsules.len());
        for location in &footer.capsules {
            validate_location(location, expected_offset)?;
            let end = location
                .offset
                .checked_add(location.length)
                .ok_or_else(|| corrupt("capsule run range overflowed"))?;
            let start = usize::try_from(location.offset)
                .map_err(|_| corrupt("capsule run offset cannot be represented"))?;
            let end_usize = usize::try_from(end)
                .map_err(|_| corrupt("capsule run end cannot be represented"))?;
            if bytes.get(start..end_usize).is_none() {
                return Err(corrupt("capsule run range is out of bounds"));
            }
            let capsule = Capsule::decode(bytes.slice(start..end_usize))?;
            if capsule.hash() != location.hash
                || capsule.transaction_id() != location.transaction_id
                || capsule.base_root_digest() != location.base_root_digest
            {
                return Err(corrupt("capsule does not match its run descriptor"));
            }
            validate_control_descriptors(&capsule, location)?;
            validate_control_bundle(&capsule, location)?;
            capsules.push(capsule);
            expected_offset = end;
        }
        let expected_packs = pack_members(&capsules, &footer.capsules)?;
        if expected_packs != footer.git_packs {
            return Err(corrupt(
                "capsule run Git pack directory does not match its capsules",
            ));
        }
        let admission = if let Some(admission) = footer.admission.as_ref() {
            validate_body_range(admission, expected_offset, footer_start as u64)?;
            let end = admission
                .offset
                .checked_add(admission.length)
                .ok_or_else(|| corrupt("capsule run admission range overflowed"))?;
            let start = usize::try_from(admission.offset)
                .map_err(|_| corrupt("capsule run admission offset cannot be represented"))?;
            let end = usize::try_from(end)
                .map_err(|_| corrupt("capsule run admission end cannot be represented"))?;
            let bytes = bytes
                .get(start..end)
                .ok_or_else(|| corrupt("capsule run admission range is out of bounds"))?;
            if blake3::hash(bytes).to_hex().as_str() != admission.hash {
                return Err(corrupt("capsule run admission hash does not match"));
            }
            expected_offset = end as u64;
            Some(CapsuleRunAdmission::decode(bytes, expected_packs.len())?)
        } else {
            None
        };
        if expected_offset != footer_start as u64 {
            return Err(corrupt("capsule runs do not cover the complete body"));
        }
        let hash = blake3::hash(&bytes).to_hex().to_string();
        Ok(Self {
            bytes,
            hash,
            footer,
            footer_length: u64::try_from(footer_length)
                .map_err(|_| corrupt("capsule run footer length cannot be represented"))?,
            capsules,
            git_packs: expected_packs,
            admission,
        })
    }

    /// Return the complete encoded run bytes.
    #[must_use]
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// Return the BLAKE3 object identity of the run.
    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Return the binary merge level, where level zero contains one capsule.
    #[must_use]
    pub fn level(&self) -> u8 {
        self.footer.level
    }

    /// Return complete verified capsules in publication order.
    #[must_use]
    pub fn capsules(&self) -> &[Capsule] {
        &self.capsules
    }

    /// Return the authenticated pack-member directory for this run.
    #[must_use]
    pub fn git_packs(&self) -> &[PackMemberDescriptor] {
        &self.git_packs
    }

    /// Return the exact object-to-member admission sidecar, when present.
    #[must_use]
    pub fn admission(&self) -> Option<&CapsuleRunAdmission> {
        self.admission.as_ref()
    }

    /// Return the absolute byte range of one nested capsule.
    pub fn capsule_range(&self, index: usize) -> Result<(u64, u64)> {
        let location = self
            .footer
            .capsules
            .get(index)
            .ok_or_else(|| corrupt("capsule run index is out of bounds"))?;
        Ok((location.offset, location.length))
    }

    /// Return the offset at which the authenticated run control suffix starts.
    #[must_use]
    pub fn control_offset(&self) -> u64 {
        self.bytes.len() as u64 - RUN_TRAILER_BYTES as u64 - self.footer_length
    }

    /// Return the authenticated control suffix length.
    #[must_use]
    pub fn control_size(&self) -> u64 {
        self.bytes.len() as u64 - self.control_offset()
    }

    /// Return the BLAKE3 hash of the authenticated run footer.
    #[must_use]
    pub fn footer_hash(&self) -> String {
        let offset = self.control_offset() as usize;
        let footer_end = self.bytes.len() - RUN_TRAILER_BYTES;
        blake3::hash(&self.bytes[offset..footer_end])
            .to_hex()
            .to_string()
    }

    /// Return transaction identities in publication order.
    #[must_use]
    pub fn transaction_ids(&self) -> Vec<String> {
        self.footer
            .capsules
            .iter()
            .map(|capsule| capsule.transaction_id.clone())
            .collect()
    }

    /// Return the base root digest of the newest capsule in the run.
    #[must_use]
    pub fn newest_base_root_digest(&self) -> &str {
        self.footer
            .capsules
            .last()
            .map_or("", |capsule| capsule.base_root_digest.as_str())
    }
}

fn validate_level_count(level: u8, count: usize) -> Result<()> {
    let expected = 1_usize
        .checked_shl(u32::from(level))
        .ok_or_else(|| contract_error("capsule run level is too large"))?;
    if expected != count || count > MAX_CAPSULES_PER_RUN {
        return Err(contract_error(
            "capsule run count must equal its power-of-two level",
        ));
    }
    Ok(())
}

fn pack_members(
    capsules: &[Capsule],
    locations: &[RunCapsuleLocation],
) -> Result<Vec<PackMemberDescriptor>> {
    if capsules.len() != locations.len() {
        return Err(corrupt(
            "capsule run pack directory has a capsule count mismatch",
        ));
    }
    let mut members = Vec::new();
    for (capsule, location) in capsules.iter().zip(locations) {
        for descriptor in capsule.git_packs() {
            let range = |section: u32, bytes: &Bytes| -> Result<PackRange> {
                let section_index = usize::try_from(section)
                    .map_err(|_| corrupt("capsule section index cannot be represented"))?;
                let section_location = capsule
                    .sections()
                    .get(section_index)
                    .ok_or_else(|| corrupt("capsule section index is out of bounds"))?;
                let offset = location
                    .offset
                    .checked_add(section_location.offset())
                    .ok_or_else(|| corrupt("capsule run pack range overflowed"))?;
                let range = PackRange::new(offset, bytes)?;
                if range.blake3() != section_location.blake3() {
                    return Err(corrupt("capsule run pack section hash does not match"));
                }
                Ok(range)
            };
            let pack = capsule.section_bytes(descriptor.pack_section())?;
            let index = capsule.section_bytes(descriptor.index_section())?;
            let reverse = capsule.section_bytes(descriptor.reverse_index_section())?;
            let locator = capsule.section_bytes(descriptor.locator_section())?;
            members.push(PackMemberDescriptor::new(
                range(descriptor.pack_section(), &pack)?,
                range(descriptor.index_section(), &index)?,
                range(descriptor.reverse_index_section(), &reverse)?,
                range(descriptor.locator_section(), &locator)?,
                descriptor.git_checksum(),
                descriptor.object_count(),
                descriptor.external_delta_bases().to_vec(),
            )?);
        }
    }
    Ok(members)
}

fn validate_location(location: &RunCapsuleLocation, expected_offset: u64) -> Result<()> {
    validate_content_hash(
        &location.hash,
        "capsule run capsule hash",
        "capsule-protocol capsule run",
    )?;
    validate_content_hash(
        &location.transaction_id,
        "capsule run transaction id",
        "capsule-protocol capsule run",
    )?;
    validate_content_hash(
        &location.base_root_digest,
        "capsule run base root digest",
        "capsule-protocol capsule run",
    )?;
    if location.length == 0 || location.offset != expected_offset {
        return Err(corrupt(
            "capsule run entries must be non-empty and contiguous",
        ));
    }
    Ok(())
}

fn section_location(
    capsule: &Capsule,
    run_offset: u64,
    kind: CapsuleSectionKind,
) -> Result<Option<RunSectionLocation>> {
    let mut found = None;
    for section in capsule
        .sections()
        .iter()
        .filter(|section| section.kind() == kind)
    {
        if found.is_some() {
            return Err(corrupt(format!("capsule has duplicate {kind:?} sections")));
        }
        found = Some(RunSectionLocation {
            offset: run_offset
                .checked_add(section.offset())
                .ok_or_else(|| corrupt("capsule run section offset overflowed"))?,
            length: section.length(),
            hash: section.blake3().to_owned(),
        });
    }
    Ok(found)
}

fn section_bytes_for_kind(capsule: &Capsule, kind: CapsuleSectionKind) -> Result<Option<Bytes>> {
    let mut sections = capsule
        .sections()
        .iter()
        .enumerate()
        .filter(|(_, section)| section.kind() == kind);
    let Some((index, _)) = sections.next() else {
        return Ok(None);
    };
    if sections.next().is_some() {
        return Err(corrupt(format!("capsule has duplicate {kind:?} sections")));
    }
    let index =
        u32::try_from(index).map_err(|_| corrupt("capsule section index cannot be represented"))?;
    capsule.section_bytes(index).map(Some)
}

fn validate_control_descriptors(capsule: &Capsule, location: &RunCapsuleLocation) -> Result<()> {
    let expected = section_location(capsule, location.offset, CapsuleSectionKind::RefTransaction)?
        .ok_or_else(|| corrupt("capsule run transaction section is missing"))?;
    if location.transaction != expected {
        return Err(corrupt(
            "capsule run transaction control range does not match capsule",
        ));
    }
    for (kind, declared) in [
        (
            CapsuleSectionKind::VisibilityDelta,
            location.visibility.as_ref(),
        ),
        (CapsuleSectionKind::CatalogDelta, location.catalog.as_ref()),
    ] {
        if let Some(declared) = declared {
            let expected = section_location(capsule, location.offset, kind)?
                .ok_or_else(|| corrupt("capsule run optional control section is missing"))?;
            if declared != &expected {
                return Err(corrupt(
                    "capsule run optional control range does not match capsule",
                ));
            }
        }
    }
    Ok(())
}

fn validate_control_bundle(capsule: &Capsule, location: &RunCapsuleLocation) -> Result<()> {
    let transaction = capsule.section_bytes(0)?;
    let transaction_range = PackRange::from_parts(
        location.transaction.offset,
        location.transaction.length,
        location.transaction.hash.clone(),
    )?;
    verify_control_section(Some(&transaction_range), Some(&transaction), "transaction")?;
    let embedded_transaction = Bytes::from(location.control.transaction.clone());
    verify_control_section(
        Some(&transaction_range),
        Some(&embedded_transaction),
        "transaction",
    )?;
    let visibility = section_bytes_for_kind(capsule, CapsuleSectionKind::VisibilityDelta)?;
    let catalog = section_bytes_for_kind(capsule, CapsuleSectionKind::CatalogDelta)?;
    let visibility_range = location
        .visibility
        .as_ref()
        .map(|range| PackRange::from_parts(range.offset, range.length, range.hash.clone()))
        .transpose()?;
    let catalog_range = location
        .catalog
        .as_ref()
        .map(|range| PackRange::from_parts(range.offset, range.length, range.hash.clone()))
        .transpose()?;
    verify_control_section(visibility_range.as_ref(), visibility.as_ref(), "visibility")?;
    verify_control_section(catalog_range.as_ref(), catalog.as_ref(), "catalog")?;
    let embedded_visibility = location.control.visibility.clone().map(Bytes::from);
    verify_embedded_control_section(
        visibility_range.as_ref(),
        embedded_visibility.as_ref(),
        "visibility",
    )?;
    let embedded_catalog = location.control.catalog.clone().map(Bytes::from);
    verify_embedded_control_section(catalog_range.as_ref(), embedded_catalog.as_ref(), "catalog")?;
    Ok(())
}

fn validate_control_range(
    range: &RunSectionLocation,
    control_offset: u64,
    object_size: u64,
) -> Result<()> {
    validate_content_hash(
        &range.hash,
        "capsule run control section hash",
        "capsule-protocol capsule run",
    )?;
    let end = range
        .offset
        .checked_add(range.length)
        .ok_or_else(|| corrupt("capsule run control range overflowed"))?;
    if range.offset >= control_offset || end > control_offset || end > object_size {
        return Err(corrupt("capsule run control range is outside the body"));
    }
    Ok(())
}

fn validate_body_range(
    range: &RunSectionLocation,
    expected_offset: u64,
    body_end: u64,
) -> Result<()> {
    validate_content_hash(
        &range.hash,
        "capsule run admission sidecar hash",
        "capsule-protocol capsule run",
    )?;
    if range.offset != expected_offset {
        return Err(corrupt(
            "capsule run admission sidecar is not contiguous with its capsules",
        ));
    }
    let end = range
        .offset
        .checked_add(range.length)
        .ok_or_else(|| corrupt("capsule run admission sidecar range overflowed"))?;
    if range.length == 0 || end != body_end {
        return Err(corrupt(
            "capsule run admission sidecar does not cover the body suffix",
        ));
    }
    Ok(())
}

fn verify_control_section(
    expected: Option<&PackRange>,
    actual: Option<&Bytes>,
    label: &str,
) -> Result<()> {
    match (expected, actual) {
        (Some(expected), Some(actual))
            if blake3::hash(actual).to_hex().as_str() == expected.blake3() =>
        {
            Ok(())
        }
        (None, None) => Ok(()),
        (Some(_), Some(_)) => Err(corrupt(format!(
            "capsule run {label} control section hash does not match"
        ))),
        (Some(_), None) | (None, Some(_)) => Err(corrupt(format!(
            "capsule run {label} control section presence does not match"
        ))),
    }
}

fn verify_embedded_control_section(
    expected: Option<&PackRange>,
    actual: Option<&Bytes>,
    label: &str,
) -> Result<()> {
    match (expected, actual) {
        (Some(_), None) => Ok(()),
        _ => verify_control_section(expected, actual, label),
    }
}

fn contract_error(reason: impl Into<String>) -> MetadataError {
    MetadataError::CapsuleContract {
        record: "capsule run",
        reason: reason.into(),
    }
}

fn corrupt(reason: impl Into<String>) -> MetadataError {
    MetadataError::CorruptObject {
        path: "capsule-protocol capsule run".to_owned(),
        reason: reason.into(),
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;
    use crate::capsule_protocol::{
        CapsuleGitPack, CapsuleRefEdit, CapsuleSection, CapsuleTransaction,
    };

    fn capsule(base: char, transaction: char) -> Capsule {
        let transaction = CapsuleTransaction::new(
            &base.to_string().repeat(64),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                Some(transaction.to_string().repeat(40)),
                None,
            )],
        )
        .unwrap();
        Capsule::build(
            &transaction,
            vec![
                CapsuleGitPack::new(
                    Bytes::from_static(b"PACK"),
                    Bytes::from_static(b"index"),
                    Bytes::from_static(b"reverse"),
                    Bytes::from_static(b"locator"),
                    "4".repeat(40),
                    1,
                )
                .unwrap(),
            ],
            Vec::new(),
        )
        .unwrap()
    }

    #[test]
    fn equal_level_runs_merge_without_changing_capsules() {
        let older = CapsuleRun::leaf(capsule('1', '2')).unwrap();
        let newer = CapsuleRun::leaf(capsule('3', '4')).unwrap();

        let merged = older.merge(&newer).unwrap();
        let decoded = CapsuleRun::decode(merged.bytes().clone()).unwrap();

        assert_eq!(decoded.level(), 1);
        assert_eq!(decoded.capsules().len(), 2);
        assert_eq!(decoded.capsules()[0].hash(), older.capsules()[0].hash());
        assert_eq!(decoded.capsules()[1].hash(), newer.capsules()[0].hash());
    }

    #[test]
    fn unequal_level_runs_cannot_merge() {
        let leaf = CapsuleRun::leaf(capsule('1', '2')).unwrap();
        let level_one = leaf.merge(&leaf).unwrap();

        let error = level_one
            .merge(&leaf)
            .expect_err("binary runs merge only at equal levels");

        assert!(matches!(error, MetadataError::CapsuleContract { .. }));
    }

    #[test]
    fn corrupt_embedded_capsule_fails_closed() {
        let run = CapsuleRun::leaf(capsule('1', '2')).unwrap();
        let mut bytes = run.bytes().to_vec();
        bytes[0] ^= 1;

        let error = CapsuleRun::decode(Bytes::from(bytes)).expect_err("corruption must fail");

        assert!(matches!(error, MetadataError::CorruptObject { .. }));
    }

    #[test]
    fn control_suffix_round_trips_without_capsule_payload() {
        let run = CapsuleRun::leaf(capsule('1', '2')).unwrap();
        let control = CapsuleRunControl::decode_suffix(
            run.bytes().slice(run.control_offset() as usize..),
            run.bytes().len() as u64,
            run.hash(),
            run.level(),
            &run.transaction_ids(),
            run.newest_base_root_digest(),
        )
        .unwrap();
        let capsules = control
            .materialize_capsules_with_external_controls(&std::collections::BTreeMap::new())
            .unwrap();

        assert_eq!(capsules[0].transaction_id(), run.transaction_ids()[0]);
        assert_eq!(
            capsules[0].base_root_digest(),
            run.newest_base_root_digest()
        );
    }

    #[test]
    fn exact_admission_round_trips_and_merges_member_ordinals() {
        let older_oid = [1_u8; 20];
        let shared_oid = [2_u8; 20];
        let newer_oid = [3_u8; 20];
        let older =
            CapsuleRun::leaf_with_member_oids(capsule('1', '2'), vec![vec![shared_oid, older_oid]])
                .unwrap();
        let newer =
            CapsuleRun::leaf_with_member_oids(capsule('3', '4'), vec![vec![newer_oid, shared_oid]])
                .unwrap();

        let decoded = CapsuleRun::decode(older.bytes().clone()).unwrap();
        let admission = decoded.admission().unwrap();
        assert_eq!(admission.object_members(&older_oid), Some(&[0][..]));
        assert_eq!(admission.object_members(&shared_oid), Some(&[0][..]));

        let merged = older.merge(&newer).unwrap();
        let decoded = CapsuleRun::decode(merged.bytes().clone()).unwrap();
        let admission = decoded.admission().unwrap();
        assert_eq!(admission.object_members(&older_oid), Some(&[0][..]));
        assert_eq!(admission.object_members(&shared_oid), Some(&[0, 1][..]));
        assert_eq!(admission.object_members(&newer_oid), Some(&[1][..]));
    }

    #[test]
    fn admission_control_range_loads_and_authenticates_sidecar() {
        let oid = [7_u8; 20];
        let run = CapsuleRun::leaf_with_member_oids(capsule('1', '2'), vec![vec![oid]]).unwrap();
        let control = CapsuleRunControl::decode_suffix(
            run.bytes().slice(run.control_offset() as usize..),
            run.bytes().len() as u64,
            run.hash(),
            run.level(),
            &run.transaction_ids(),
            run.newest_base_root_digest(),
        )
        .unwrap();
        let range = control.admission_range().unwrap();
        let bytes = run
            .bytes()
            .slice(range.offset() as usize..(range.offset() + range.length()) as usize);
        let control = control.attach_admission(bytes).unwrap();
        assert_eq!(
            control.admission().unwrap().object_members(&oid),
            Some(&[0][..])
        );
    }

    #[test]
    fn admission_decode_rejects_trailing_bytes() {
        let oid = [9_u8; 20];
        let admission = CapsuleRunAdmission::from_member_oids(&[vec![oid]], 1).unwrap();
        let mut bytes = admission.encode(1).unwrap().to_vec();
        bytes.push(0);

        let error = CapsuleRunAdmission::decode(&bytes, 1)
            .expect_err("trailing admission bytes must be rejected");
        assert!(matches!(error, MetadataError::CorruptObject { .. }));
    }

    #[test]
    fn large_visibility_control_is_detached_from_bounded_footer() {
        let transaction = CapsuleTransaction::new(
            &"1".repeat(64),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                Some("2".repeat(40)),
                None,
            )],
        )
        .unwrap();
        let objects = (0..20_000)
            .map(|index| format!("{index:040x}"))
            .collect::<Vec<_>>();
        let visibility = crate::git_visibility::GitVisibilityEdit::from_replacement_objects(
            None,
            objects[2_000].clone(),
            objects,
        );
        let visibility = crate::capsule_protocol::CapsuleVisibilityDelta::new(
            std::collections::BTreeMap::from([("refs/heads/main".to_owned(), visibility)]),
        )
        .unwrap()
        .encode()
        .unwrap();
        let capsule = Capsule::build(
            &transaction,
            Vec::new(),
            vec![CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                visibility.clone(),
            )],
        )
        .unwrap();
        let run = CapsuleRun::leaf(capsule.clone()).unwrap();
        let control = CapsuleRunControl::decode_suffix(
            run.bytes().slice(run.control_offset() as usize..),
            run.bytes().len() as u64,
            run.hash(),
            run.level(),
            &run.transaction_ids(),
            run.newest_base_root_digest(),
        )
        .unwrap();
        let detached = control
            .capsule_locations()
            .iter()
            .flat_map(CapsuleControlLocation::detached_controls)
            .collect::<Vec<_>>();
        assert_eq!(detached.len(), 1);
        let section = capsule
            .sections()
            .iter()
            .position(|section| section.kind() == CapsuleSectionKind::VisibilityDelta)
            .unwrap();
        let section = capsule.section_bytes(section as u32).unwrap();
        let controls = control
            .materialize_capsules_with_external_controls(&std::collections::BTreeMap::from([(
                (
                    capsule.hash().to_owned(),
                    CapsuleSectionKind::VisibilityDelta,
                ),
                section,
            )]))
            .unwrap();
        assert_eq!(controls[0].visibility_delta().unwrap().edits().len(), 1);
    }
}
