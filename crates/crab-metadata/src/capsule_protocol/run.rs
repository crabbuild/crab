use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::capsule_protocol::Capsule;
use crate::error::{MetadataError, Result};
use crate::validation::validate_content_hash;

const RUN_MAGIC: &[u8; 8] = b"CRBRUN02";
const RUN_VERSION: u32 = 2;
const RUN_TRAILER_BYTES: usize = 8 + 32 + RUN_MAGIC.len();
const MAX_RUN_FOOTER_BYTES: usize = 8 * 1024 * 1024;
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CapsuleRunFooter {
    version: u32,
    level: u8,
    capsules: Vec<RunCapsuleLocation>,
}

/// Immutable power-of-two run of complete push capsules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapsuleRun {
    bytes: Bytes,
    hash: String,
    footer: CapsuleRunFooter,
    capsules: Vec<Capsule>,
}

impl CapsuleRun {
    /// Wrap one verified push capsule as a level-zero run.
    pub fn leaf(capsule: Capsule) -> Result<Self> {
        Self::encode(0, vec![capsule])
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
        Self::encode(level, capsules)
    }

    fn encode(level: u8, capsules: Vec<Capsule>) -> Result<Self> {
        validate_level_count(level, capsules.len())?;
        let mut body = Vec::new();
        let mut locations = Vec::with_capacity(capsules.len());
        for capsule in &capsules {
            let offset = u64::try_from(body.len())
                .map_err(|_| contract_error("capsule run offset cannot be represented"))?;
            let length = u64::try_from(capsule.bytes().len())
                .map_err(|_| contract_error("capsule run length cannot be represented"))?;
            body.extend_from_slice(capsule.bytes());
            locations.push(RunCapsuleLocation {
                offset,
                length,
                hash: capsule.hash().to_owned(),
                transaction_id: capsule.transaction_id().to_owned(),
                base_root_digest: capsule.base_root_digest().to_owned(),
            });
        }
        let footer = CapsuleRunFooter {
            version: RUN_VERSION,
            level,
            capsules: locations,
        };
        let footer_bytes = serde_json::to_vec(&footer).map_err(|source| {
            MetadataError::Internal(format!("capsule run serialization failed: {source}"))
        })?;
        if footer_bytes.len() > MAX_RUN_FOOTER_BYTES {
            return Err(contract_error(format!(
                "capsule run footer exceeds {MAX_RUN_FOOTER_BYTES} bytes"
            )));
        }
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
            capsules.push(capsule);
            expected_offset = end;
        }
        if expected_offset != footer_start as u64 {
            return Err(corrupt("capsule runs do not cover the complete body"));
        }
        let hash = blake3::hash(&bytes).to_hex().to_string();
        Ok(Self {
            bytes,
            hash,
            footer,
            capsules,
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
    use crate::capsule_protocol::{CapsuleGitPack, CapsuleRefEdit, CapsuleTransaction};

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
}
