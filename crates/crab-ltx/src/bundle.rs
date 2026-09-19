//! Celld-inspired verbatim LTX envelopes with bounded, checksum-verified rows.
//! Apache-2.0; adapted from bundle.rs at the revision in UPSTREAM.md.

use crate::{CrabError, Limits, Result, SegmentInfo};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// One immutable segment and its repository/epoch identity, before bundling.
pub struct BundleEntry {
    pub repository: String,
    pub epoch: String,
    pub info: SegmentInfo,
    pub bytes: Vec<u8>,
}

impl BundleEntry {
    /// Creates an entry using the canonical identity selected by `CellReplica`.
    #[must_use]
    pub fn for_cell(
        cell: [u8; 32],
        incarnation: [u8; 16],
        info: SegmentInfo,
        bytes: Vec<u8>,
    ) -> Self {
        Self {
            repository: encode_identity(&cell),
            epoch: encode_identity(&incarnation),
            info,
            bytes,
        }
    }
}

/// A verified segment's byte extent in a bundle; identity is not authorization.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleRow {
    pub repository: String,
    pub epoch: String,
    pub info: SegmentInfo,
    pub offset: u64,
}

/// An owned, validated envelope containing verbatim checksum-bearing LTX files.
///
/// The format is Crab's `CRB1`, not Celld's `CLB1`: rows retain string epochs,
/// exact ranges and whole-file checksums. A manifest must pin its locations.
pub struct Bundle {
    bytes: Bytes,
    rows: Vec<BundleRow>,
}

impl Bundle {
    /// Encodes bounded segments; duplicate identities and malformed LTX are rejected.
    pub fn encode(entries: Vec<BundleEntry>, limits: Limits) -> Result<Self> {
        let limits = limits.validate()?;
        if entries.is_empty() || entries.len() > limits.max_segments {
            return Err(CrabError::Limit("bundle entries"));
        }
        let mut bytes = Vec::new();
        let mut rows = Vec::new();
        for entry in entries {
            if entry.repository.is_empty()
                || entry.repository.len() > 4096
                || !valid_epoch(&entry.epoch)
            {
                return Err(CrabError::LTXCorrupted);
            }
            crate::recovery::verify_segment(&entry.bytes, &entry.info, limits)?;
            if (bytes.len() as u64).saturating_add(entry.info.size_bytes) > limits.max_plan_bytes {
                return Err(CrabError::Limit("bundle bytes"));
            }
            rows.push(BundleRow {
                repository: entry.repository,
                epoch: entry.epoch,
                info: entry.info,
                offset: bytes.len() as u64,
            });
            bytes.extend(entry.bytes);
        }
        let footer = serde_json::to_vec(&rows)?;
        let len = u32::try_from(footer.len()).map_err(|_| CrabError::Limit("bundle footer"))?;
        bytes.extend(footer);
        bytes.extend(len.to_le_bytes());
        bytes.extend(b"CRB1");
        Self::decode(bytes, limits)
    }

    /// Verifies the complete envelope, every extent, and every segment digest.
    pub fn decode(bytes: Vec<u8>, limits: Limits) -> Result<Self> {
        let limits = limits.validate()?;
        if bytes.len() as u64 > limits.max_plan_bytes {
            return Err(CrabError::Limit("bundle bytes"));
        }
        let trailer = bytes.len().checked_sub(8).ok_or(CrabError::LTXCorrupted)?;
        if &bytes[trailer + 4..] != b"CRB1" {
            return Err(CrabError::LTXCorrupted);
        }
        let len = u32::from_le_bytes(
            bytes[trailer..trailer + 4]
                .try_into()
                .map_err(|_| CrabError::LTXCorrupted)?,
        ) as usize;
        let start = trailer.checked_sub(len).ok_or(CrabError::LTXCorrupted)?;
        let rows: Vec<BundleRow> = serde_json::from_slice(&bytes[start..trailer])?;
        if rows.is_empty() || rows.len() > limits.max_segments {
            return Err(CrabError::Limit("bundle entries"));
        }
        let mut end = 0u64;
        let mut identities = BTreeSet::new();
        for row in &rows {
            if row.repository.is_empty()
                || row.repository.len() > 4096
                || !valid_epoch(&row.epoch)
                || row.offset != end
                || !identities.insert((
                    &row.repository,
                    &row.epoch,
                    row.info.min_txid,
                    row.info.max_txid,
                ))
            {
                return Err(CrabError::LTXCorrupted);
            }
            end = end
                .checked_add(row.info.size_bytes)
                .ok_or(CrabError::LTXCorrupted)?;
            if end > start as u64 {
                return Err(CrabError::LTXCorrupted);
            }
            crate::recovery::verify_segment(
                &bytes[row.offset as usize..end as usize],
                &row.info,
                limits,
            )?;
        }
        if end != start as u64 {
            return Err(CrabError::LTXCorrupted);
        }
        Ok(Self {
            bytes: bytes.into(),
            rows,
        })
    }

    #[must_use]
    pub fn rows(&self) -> &[BundleRow] {
        &self.rows
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn shared_bytes(&self) -> Bytes {
        self.bytes.clone()
    }

    /// Returns the verified bytes for a row index, never a caller-supplied extent.
    pub fn segment(&self, index: usize) -> Result<&[u8]> {
        let row = self.rows.get(index).ok_or(CrabError::TxNotAvailable)?;
        Ok(&self.bytes[row.offset as usize..(row.offset + row.info.size_bytes) as usize])
    }
}

pub(crate) fn cell_identity(cell: &[u8; 32], incarnation: &[u8; 16]) -> (String, String) {
    (encode_identity(cell), encode_identity(incarnation))
}

fn encode_identity(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(TABLE[(byte >> 4) as usize] as char);
        encoded.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn valid_epoch(epoch: &str) -> bool {
    !epoch.is_empty()
        && epoch.len() <= 128
        && epoch
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}
