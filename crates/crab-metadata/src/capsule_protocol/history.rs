use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::error::{MetadataError, Result};
use crate::validation::{validate_content_hash, validate_sha1};

use super::{CapsulePointer, CheckpointPointer, valid_ref_name, valid_ref_namespace};

const HISTORY_SEGMENT_VERSION: u32 = 2;
/// Maximum encoded history-segment size accepted by readers and writers.
pub const MAX_HISTORY_SEGMENT_BYTES: u64 = 32 * 1024 * 1024;
/// Maximum authenticated segments retained by one repository root.
pub const MAX_HISTORY_CHAIN_SEGMENTS: usize = 100_000;
/// Maximum encoded history bytes retained by one repository root.
pub const MAX_HISTORY_CHAIN_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Authenticated pointer to one immutable checkpoint-history segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistorySegmentPointer {
    hash: String,
    size: u64,
    covered_generation: u64,
    covered_root_digest: String,
    previous_segment_hash: Option<String>,
    transaction_count: u32,
}

impl HistorySegmentPointer {
    /// Return the segment's BLAKE3 object identity.
    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Return the complete encoded segment size.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Return the root generation captured by the segment checkpoint.
    #[must_use]
    pub fn covered_generation(&self) -> u64 {
        self.covered_generation
    }

    /// Return the exact root digest captured by the segment checkpoint.
    #[must_use]
    pub fn covered_root_digest(&self) -> &str {
        &self.covered_root_digest
    }

    /// Return the preceding segment identity, when this is not the first segment.
    #[must_use]
    pub fn previous_segment_hash(&self) -> Option<&str> {
        self.previous_segment_hash.as_deref()
    }

    /// Return the number of transactions retained by this segment.
    #[must_use]
    pub fn transaction_count(&self) -> u32 {
        self.transaction_count
    }
}

/// Exact ref state and capsule runs folded into one checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistorySegmentState {
    refs: BTreeMap<String, String>,
    peeled_refs: BTreeMap<String, String>,
    head: String,
    compacted_ref_transactions: BTreeMap<String, String>,
    capsule_runs: Vec<CapsulePointer>,
}

impl HistorySegmentState {
    /// Capture the complete logical state folded by checkpoint maintenance.
    #[must_use]
    pub fn new(
        refs: BTreeMap<String, String>,
        peeled_refs: BTreeMap<String, String>,
        head: String,
        compacted_ref_transactions: BTreeMap<String, String>,
        capsule_runs: Vec<CapsulePointer>,
    ) -> Self {
        Self {
            refs,
            peeled_refs,
            head,
            compacted_ref_transactions,
            capsule_runs,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistorySegmentPayload {
    version: u32,
    checkpoint: CheckpointPointer,
    previous: Option<HistorySegmentPointer>,
    refs: BTreeMap<String, String>,
    peeled_refs: BTreeMap<String, String>,
    head: String,
    compacted_ref_transactions: BTreeMap<String, String>,
    capsule_runs: Vec<CapsulePointer>,
}

/// Immutable checkpoint recovery point plus the capsule runs it compacted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistorySegment {
    bytes: Bytes,
    hash: String,
    payload: HistorySegmentPayload,
}

impl HistorySegment {
    /// Build one canonical segment before checkpoint/root publication.
    pub fn build(
        checkpoint: CheckpointPointer,
        previous: Option<HistorySegmentPointer>,
        state: HistorySegmentState,
    ) -> Result<Self> {
        let payload = HistorySegmentPayload {
            version: HISTORY_SEGMENT_VERSION,
            checkpoint,
            previous,
            refs: state.refs,
            peeled_refs: state.peeled_refs,
            head: state.head,
            compacted_ref_transactions: state.compacted_ref_transactions,
            capsule_runs: state.capsule_runs,
        };
        validate_payload(&payload)?;
        let bytes = serde_json::to_vec(&payload).map_err(|source| {
            MetadataError::Internal(format!("history segment serialization failed: {source}"))
        })?;
        enforce_size(bytes.len())?;
        let bytes = Bytes::from(bytes);
        let hash = blake3::hash(&bytes).to_hex().to_string();
        Ok(Self {
            bytes,
            hash,
            payload,
        })
    }

    /// Decode and authenticate one canonical history segment.
    pub fn decode(bytes: Bytes) -> Result<Self> {
        enforce_size(bytes.len()).map_err(as_corruption)?;
        let payload: HistorySegmentPayload = serde_json::from_slice(&bytes)
            .map_err(|source| corrupt(format!("history segment is invalid JSON: {source}")))?;
        validate_payload(&payload).map_err(as_corruption)?;
        let canonical = serde_json::to_vec(&payload).map_err(|source| {
            MetadataError::Internal(format!("history segment serialization failed: {source}"))
        })?;
        if canonical.as_slice() != bytes.as_ref() {
            return Err(corrupt("history segment is not canonically encoded"));
        }
        let hash = blake3::hash(&bytes).to_hex().to_string();
        Ok(Self {
            bytes,
            hash,
            payload,
        })
    }

    /// Return the complete canonical segment bytes.
    #[must_use]
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// Return the segment's BLAKE3 object identity.
    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Build the exact pointer installed in the repository root.
    pub fn pointer(&self) -> Result<HistorySegmentPointer> {
        let transaction_count = self
            .payload
            .capsule_runs
            .iter()
            .try_fold(0_u32, |total, run| total.checked_add(run.capsule_count()))
            .ok_or_else(|| contract_error("history transaction count overflowed"))?;
        Ok(HistorySegmentPointer {
            hash: self.hash.clone(),
            size: self.bytes.len() as u64,
            covered_generation: self.payload.checkpoint.covered_generation(),
            covered_root_digest: self.payload.checkpoint.covered_root_digest().to_owned(),
            previous_segment_hash: self
                .payload
                .previous
                .as_ref()
                .map(|previous| previous.hash.clone()),
            transaction_count,
        })
    }

    /// Return the checkpoint recovery point authenticated by this segment.
    #[must_use]
    pub fn checkpoint(&self) -> &CheckpointPointer {
        &self.payload.checkpoint
    }

    /// Return the preceding history segment, when present.
    #[must_use]
    pub fn previous(&self) -> Option<&HistorySegmentPointer> {
        self.payload.previous.as_ref()
    }

    /// Return the exact refs captured by checkpoint maintenance.
    #[must_use]
    pub fn refs(&self) -> &BTreeMap<String, String> {
        &self.payload.refs
    }

    /// Return the exact peeled refs captured by checkpoint maintenance.
    #[must_use]
    pub fn peeled_refs(&self) -> &BTreeMap<String, String> {
        &self.payload.peeled_refs
    }

    /// Return the symbolic HEAD captured by checkpoint maintenance.
    #[must_use]
    pub fn head(&self) -> &str {
        &self.payload.head
    }

    /// Return per-ref transaction positions captured by checkpoint maintenance.
    #[must_use]
    pub fn compacted_ref_transactions(&self) -> &BTreeMap<String, String> {
        &self.payload.compacted_ref_transactions
    }

    /// Return immutable capsule runs retained by this segment.
    #[must_use]
    pub fn capsule_runs(&self) -> &[CapsulePointer] {
        &self.payload.capsule_runs
    }
}

fn validate_payload(payload: &HistorySegmentPayload) -> Result<()> {
    if payload.version != HISTORY_SEGMENT_VERSION {
        return Err(contract_error(format!(
            "history segment must use version {HISTORY_SEGMENT_VERSION}"
        )));
    }
    validate_checkpoint_pointer(&payload.checkpoint)?;
    if let Some(previous) = &payload.previous {
        validate_history_pointer(previous)?;
        if previous.covered_generation >= payload.checkpoint.covered_generation() {
            return Err(contract_error(
                "history predecessor generation must be older than its checkpoint",
            ));
        }
    }
    if !payload.head.starts_with("refs/heads/") || !valid_ref_name(&payload.head) {
        return Err(contract_error("history HEAD must name a branch"));
    }
    for (name, oid) in payload.refs.iter().chain(payload.peeled_refs.iter()) {
        if !name.starts_with("refs/") || !valid_ref_name(name) {
            return Err(contract_error(
                "history segment contains an invalid ref name",
            ));
        }
        validate_sha1(oid, "history ref object id", "capsule-protocol history")?;
    }
    if !valid_ref_namespace(payload.refs.keys().map(String::as_str)) {
        return Err(contract_error(
            "history segment contains conflicting ref names",
        ));
    }
    if payload
        .peeled_refs
        .keys()
        .any(|name| !payload.refs.contains_key(name))
    {
        return Err(contract_error(
            "history segment has a peeled target without its ref",
        ));
    }
    for (name, transaction_id) in &payload.compacted_ref_transactions {
        if !name.starts_with("refs/") || !valid_ref_name(name) {
            return Err(contract_error(
                "history segment contains an invalid compacted ref position",
            ));
        }
        validate_content_hash(
            transaction_id,
            "history compacted transaction id",
            "capsule-protocol history",
        )?;
    }
    if payload.capsule_runs.is_empty() {
        return Err(contract_error(
            "history segment must retain at least one capsule run",
        ));
    }
    let mut run_hashes = BTreeSet::new();
    let mut transaction_ids = BTreeSet::new();
    for run in &payload.capsule_runs {
        validate_capsule_pointer(run)?;
        if !run_hashes.insert(run.hash()) {
            return Err(contract_error("history segment repeats a capsule run"));
        }
        for transaction_id in run.transaction_ids() {
            if !transaction_ids.insert(transaction_id) {
                return Err(contract_error(
                    "history segment repeats a transaction identity",
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn validate_history_pointer(pointer: &HistorySegmentPointer) -> Result<()> {
    validate_content_hash(
        &pointer.hash,
        "history segment hash",
        "capsule-protocol history pointer",
    )?;
    validate_content_hash(
        &pointer.covered_root_digest,
        "history covered root digest",
        "capsule-protocol history pointer",
    )?;
    if let Some(previous) = &pointer.previous_segment_hash {
        validate_content_hash(
            previous,
            "history predecessor hash",
            "capsule-protocol history pointer",
        )?;
    }
    if pointer.size == 0
        || pointer.size > MAX_HISTORY_SEGMENT_BYTES
        || pointer.transaction_count == 0
    {
        return Err(contract_error("history segment pointer is out of bounds"));
    }
    Ok(())
}

fn validate_checkpoint_pointer(pointer: &CheckpointPointer) -> Result<()> {
    CheckpointPointer::new(
        pointer.hash(),
        pointer.size(),
        pointer.control_offset(),
        pointer.control_size(),
        pointer.footer_hash(),
        pointer.covered_generation(),
        pointer.covered_root_digest(),
        pointer.pack_count(),
        pointer.object_count(),
    )?;
    Ok(())
}

fn validate_capsule_pointer(pointer: &CapsulePointer) -> Result<()> {
    CapsulePointer::new(
        pointer.hash(),
        pointer.size(),
        pointer.level(),
        pointer.transaction_ids().to_vec(),
        pointer.newest_base_root_digest(),
    )?;
    Ok(())
}

fn enforce_size(size: usize) -> Result<()> {
    if size as u64 > MAX_HISTORY_SEGMENT_BYTES {
        return Err(contract_error(format!(
            "history segment exceeds its {MAX_HISTORY_SEGMENT_BYTES}-byte limit"
        )));
    }
    Ok(())
}

fn as_corruption(error: MetadataError) -> MetadataError {
    corrupt(error.to_string())
}

fn contract_error(reason: impl Into<String>) -> MetadataError {
    MetadataError::CapsuleContract {
        record: "history segment",
        reason: reason.into(),
    }
}

fn corrupt(reason: impl Into<String>) -> MetadataError {
    MetadataError::CorruptObject {
        path: "capsule-protocol history segment".to_owned(),
        reason: reason.into(),
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    fn checkpoint() -> CheckpointPointer {
        CheckpointPointer::new(
            "1".repeat(64),
            100,
            0,
            100,
            "0".repeat(64),
            7,
            "2".repeat(64),
            1,
            3,
        )
        .unwrap()
    }

    fn run() -> CapsulePointer {
        CapsulePointer::new("3".repeat(64), 200, 0, vec!["4".repeat(64)], "2".repeat(64)).unwrap()
    }

    fn state() -> HistorySegmentState {
        HistorySegmentState::new(
            BTreeMap::from([("refs/heads/main".to_owned(), "5".repeat(40))]),
            BTreeMap::new(),
            "refs/heads/main".to_owned(),
            BTreeMap::from([("refs/heads/main".to_owned(), "4".repeat(64))]),
            vec![run()],
        )
    }

    #[test]
    fn history_segment_round_trips_with_exact_pointer() {
        let segment = HistorySegment::build(checkpoint(), None, state()).unwrap();
        let decoded = HistorySegment::decode(segment.bytes().clone()).unwrap();

        assert_eq!(decoded, segment);
        assert_eq!(decoded.pointer().unwrap().hash(), segment.hash());
        assert_eq!(decoded.pointer().unwrap().transaction_count(), 1);
        assert_eq!(decoded.refs()["refs/heads/main"], "5".repeat(40));
    }

    #[test]
    fn history_segment_retry_has_stable_identity() {
        let first = HistorySegment::build(checkpoint(), None, state()).unwrap();
        let retry = HistorySegment::build(checkpoint(), None, state()).unwrap();

        assert_eq!(first.bytes(), retry.bytes());
        assert_eq!(first.hash(), retry.hash());
    }

    #[test]
    fn history_segment_rejects_duplicate_transaction_identity() {
        let mut state = state();
        state.capsule_runs.push(run());

        let error = HistorySegment::build(checkpoint(), None, state)
            .expect_err("duplicate transaction identity must fail");

        assert!(matches!(error, MetadataError::CapsuleContract { .. }));
    }

    #[test]
    fn history_segment_detects_noncanonical_or_corrupt_bytes() {
        let segment = HistorySegment::build(checkpoint(), None, state()).unwrap();
        let mut bytes = segment.bytes().to_vec();
        bytes.push(b' ');

        let error = HistorySegment::decode(Bytes::from(bytes))
            .expect_err("noncanonical history bytes must fail");

        assert!(matches!(error, MetadataError::CorruptObject { .. }));
    }
}
