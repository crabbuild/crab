use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::error::{MetadataError, Result};
use crate::validation::{validate_content_hash, validate_sha1};

use super::{valid_ref_name, valid_ref_namespace};

const ROOT_MAGIC: &[u8; 8] = b"CRBROOT2";
const ROOT_VERSION: u32 = 2;
const ROOT_HEADER_BYTES: usize = ROOT_MAGIC.len() + 4 + 8;
const ROOT_DIGEST_BYTES: usize = 32;
/// Maximum encoded repository-root size accepted by readers and writers.
pub const MAX_ROOT_BYTES: u64 = 8 * 1024 * 1024;
/// Maximum post-checkpoint capsules kept in one repository root.
pub const MAX_CAPSULE_FRONTIER: usize = 8;
/// Maximum ref transactions admitted before a complete checkpoint is required.
pub const MAX_DELTA_DEPTH: u32 = 500;

/// Root fence that excludes publications during one GC sweep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcFence {
    id: String,
    expires_at_unix: u64,
}

impl GcFence {
    /// Create one bounded maintenance-fence identity.
    pub fn new(id: impl Into<String>, expires_at_unix: u64) -> Result<Self> {
        let fence = Self {
            id: id.into(),
            expires_at_unix,
        };
        validate_gc_fence(&fence)?;
        Ok(fence)
    }

    /// Return the content-hash-shaped owner identity.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Return the wall-clock deadline used to diagnose a stranded fence.
    ///
    /// Expiry never transfers ownership: only the exact fence owner may clear
    /// it, because a paused sweeper could otherwise race a new publication.
    #[must_use]
    pub fn expires_at_unix(&self) -> u64 {
        self.expires_at_unix
    }
}

/// Root reference to one durable immutable capsule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapsulePointer {
    hash: String,
    size: u64,
    level: u8,
    capsule_count: u32,
    transaction_ids: Vec<String>,
    newest_base_root_digest: String,
}

impl CapsulePointer {
    /// Create and validate a root pointer to an immutable capsule.
    pub fn new(
        hash: impl Into<String>,
        size: u64,
        level: u8,
        transaction_ids: Vec<String>,
        newest_base_root_digest: impl Into<String>,
    ) -> Result<Self> {
        let capsule_count = u32::try_from(transaction_ids.len())
            .map_err(|_| contract_error("root capsule run count cannot be represented"))?;
        let pointer = Self {
            hash: hash.into(),
            size,
            level,
            capsule_count,
            transaction_ids,
            newest_base_root_digest: newest_base_root_digest.into(),
        };
        validate_capsule_pointer(&pointer)?;
        Ok(pointer)
    }

    /// Return the capsule's BLAKE3 object identity.
    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Return the complete capsule size.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Return the binary merge level of this capsule run.
    #[must_use]
    pub fn level(&self) -> u8 {
        self.level
    }

    /// Return the number of complete capsules in this run.
    #[must_use]
    pub fn capsule_count(&self) -> u32 {
        self.capsule_count
    }

    /// Return transaction identities in publication order.
    #[must_use]
    pub fn transaction_ids(&self) -> &[String] {
        &self.transaction_ids
    }

    /// Return the parent root digest extended by the newest capsule.
    #[must_use]
    pub fn newest_base_root_digest(&self) -> &str {
        &self.newest_base_root_digest
    }
}

/// Root reference to one complete immutable repository checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointPointer {
    hash: String,
    size: u64,
    covered_generation: u64,
    covered_root_digest: String,
    pack_count: u32,
    object_count: u64,
}

impl CheckpointPointer {
    /// Create a checkpoint pointer whose complete Git pack is range-addressable.
    pub fn new(
        hash: impl Into<String>,
        size: u64,
        covered_generation: u64,
        covered_root_digest: impl Into<String>,
        pack_count: u32,
        object_count: u64,
    ) -> Result<Self> {
        let pointer = Self {
            hash: hash.into(),
            size,
            covered_generation,
            covered_root_digest: covered_root_digest.into(),
            pack_count,
            object_count,
        };
        validate_checkpoint_pointer(&pointer)?;
        Ok(pointer)
    }

    /// Return the immutable checkpoint object identity.
    #[must_use]
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Return the complete checkpoint object size.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Return the repository generation materialized by this checkpoint.
    #[must_use]
    pub fn covered_generation(&self) -> u64 {
        self.covered_generation
    }

    /// Return the authenticated root materialized by this checkpoint.
    #[must_use]
    pub fn covered_root_digest(&self) -> &str {
        &self.covered_root_digest
    }

    /// Return the number of independently usable Git packs.
    #[must_use]
    pub fn pack_count(&self) -> u32 {
        self.pack_count
    }

    /// Return the number of Git objects in the complete checkpoint pack.
    #[must_use]
    pub fn object_count(&self) -> u64 {
        self.object_count
    }
}

/// Complete mutable authority for one capsule-protocol repository generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryRoot {
    version: u32,
    repository_id: String,
    generation: u64,
    parent_root_digest: Option<String>,
    latest_transaction_base_digest: Option<String>,
    refs: BTreeMap<String, String>,
    peeled_refs: BTreeMap<String, String>,
    head: String,
    checkpoint: Option<CheckpointPointer>,
    capsule_frontier: Vec<CapsulePointer>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    compacted_ref_transactions: BTreeMap<String, String>,
    delta_depth: u32,
    gc_fence: Option<GcFence>,
    capabilities: BTreeSet<String>,
}

impl RepositoryRoot {
    /// Create an unborn generation-zero repository root.
    pub fn initial(repository_id: &str, head: &str) -> Result<Self> {
        let root = Self {
            version: ROOT_VERSION,
            repository_id: repository_id.to_owned(),
            generation: 0,
            parent_root_digest: None,
            latest_transaction_base_digest: None,
            refs: BTreeMap::new(),
            peeled_refs: BTreeMap::new(),
            head: head.to_owned(),
            checkpoint: None,
            capsule_frontier: Vec::new(),
            compacted_ref_transactions: BTreeMap::new(),
            delta_depth: 0,
            gc_fence: None,
            capabilities: BTreeSet::new(),
        };
        validate_root(&root)?;
        Ok(root)
    }

    /// Build the next generation after applying one already validated transaction.
    pub fn advance(
        &self,
        parent_root_digest: &str,
        refs: BTreeMap<String, String>,
        peeled_refs: BTreeMap<String, String>,
        capsule_frontier: Vec<CapsulePointer>,
        transaction_id: &str,
    ) -> Result<Self> {
        if self.gc_fence.is_some() {
            return Err(contract_error(
                "ref publication is forbidden while the GC fence is active",
            ));
        }
        validate_content_hash(
            transaction_id,
            "new root transaction id",
            "capsule-protocol root",
        )?;
        if capsule_frontier
            .last()
            .is_none_or(|run| run.newest_base_root_digest != parent_root_digest)
        {
            return Err(contract_error(
                "newest capsule run does not extend the parent root",
            ));
        }
        let retained = self
            .capsule_frontier
            .iter()
            .flat_map(|run| run.transaction_ids.iter())
            .cloned()
            .collect::<BTreeSet<_>>();
        let next = capsule_frontier
            .iter()
            .flat_map(|run| run.transaction_ids.iter())
            .cloned()
            .collect::<BTreeSet<_>>();
        if retained.contains(transaction_id)
            || !next.contains(transaction_id)
            || !retained.is_subset(&next)
            || next.len() != retained.len() + 1
        {
            return Err(contract_error(
                "new capsule frontier must retain every transaction and add exactly one",
            ));
        }
        let root = Self {
            version: ROOT_VERSION,
            repository_id: self.repository_id.clone(),
            generation: self
                .generation
                .checked_add(1)
                .ok_or_else(|| contract_error("root generation overflowed"))?,
            parent_root_digest: Some(parent_root_digest.to_owned()),
            latest_transaction_base_digest: Some(parent_root_digest.to_owned()),
            refs,
            peeled_refs,
            head: self.head.clone(),
            checkpoint: self.checkpoint.clone(),
            capsule_frontier,
            compacted_ref_transactions: self.compacted_ref_transactions.clone(),
            delta_depth: self
                .delta_depth
                .checked_add(1)
                .ok_or_else(|| contract_error("root delta depth overflowed"))?,
            gc_fence: None,
            capabilities: self.capabilities.clone(),
        };
        validate_root(&root)?;
        Ok(root)
    }

    /// Publish a checkpoint for this exact root and reset the bounded delta frontier.
    pub fn install_checkpoint(
        &self,
        parent_root_digest: &str,
        checkpoint: CheckpointPointer,
    ) -> Result<Self> {
        if self.gc_fence.is_some() {
            return Err(contract_error(
                "checkpoint publication is forbidden while the GC fence is active",
            ));
        }
        if checkpoint.covered_generation != self.generation
            || checkpoint.covered_root_digest != parent_root_digest
        {
            return Err(contract_error(
                "checkpoint does not cover the exact parent root generation",
            ));
        }
        let root = Self {
            version: ROOT_VERSION,
            repository_id: self.repository_id.clone(),
            generation: self.generation,
            parent_root_digest: Some(parent_root_digest.to_owned()),
            latest_transaction_base_digest: None,
            refs: self.refs.clone(),
            peeled_refs: self.peeled_refs.clone(),
            head: self.head.clone(),
            checkpoint: Some(checkpoint),
            capsule_frontier: Vec::new(),
            compacted_ref_transactions: self.compacted_ref_transactions.clone(),
            delta_depth: 0,
            gc_fence: None,
            capabilities: self.capabilities.clone(),
        };
        validate_root(&root)?;
        Ok(root)
    }

    /// Install a checkpoint that folds the exact visible per-ref head positions.
    pub fn install_ref_checkpoint(
        &self,
        parent_root_digest: &str,
        checkpoint: CheckpointPointer,
        refs: BTreeMap<String, String>,
        peeled_refs: BTreeMap<String, String>,
        compacted_ref_transactions: BTreeMap<String, String>,
    ) -> Result<Self> {
        if self.gc_fence.is_some() {
            return Err(contract_error(
                "checkpoint publication is forbidden while the GC fence is active",
            ));
        }
        if checkpoint.covered_generation != self.generation
            || checkpoint.covered_root_digest != parent_root_digest
        {
            return Err(contract_error(
                "checkpoint does not cover the exact parent root generation",
            ));
        }
        let root = Self {
            version: ROOT_VERSION,
            repository_id: self.repository_id.clone(),
            generation: self
                .generation
                .checked_add(1)
                .ok_or_else(|| contract_error("root generation overflowed"))?,
            parent_root_digest: Some(parent_root_digest.to_owned()),
            latest_transaction_base_digest: None,
            refs,
            peeled_refs,
            head: self.head.clone(),
            checkpoint: Some(checkpoint),
            capsule_frontier: Vec::new(),
            compacted_ref_transactions,
            delta_depth: 0,
            gc_fence: None,
            capabilities: self.capabilities.clone(),
        };
        validate_root(&root)?;
        Ok(root)
    }

    /// Install an exclusive GC fence without changing logical repository state.
    pub fn begin_gc(&self, parent_root_digest: &str, fence: GcFence) -> Result<Self> {
        if self.gc_fence.is_some() {
            return Err(contract_error(
                "GC fencing requires an unfenced repository root",
            ));
        }
        let root = Self {
            version: ROOT_VERSION,
            repository_id: self.repository_id.clone(),
            generation: self.generation,
            parent_root_digest: Some(parent_root_digest.to_owned()),
            latest_transaction_base_digest: self.latest_transaction_base_digest.clone(),
            refs: self.refs.clone(),
            peeled_refs: self.peeled_refs.clone(),
            head: self.head.clone(),
            checkpoint: self.checkpoint.clone(),
            capsule_frontier: self.capsule_frontier.clone(),
            compacted_ref_transactions: self.compacted_ref_transactions.clone(),
            delta_depth: self.delta_depth,
            gc_fence: Some(fence),
            capabilities: self.capabilities.clone(),
        };
        validate_root(&root)?;
        Ok(root)
    }

    /// Remove the exact GC fence after its sweep finishes.
    pub fn end_gc(&self, parent_root_digest: &str, fence_id: &str) -> Result<Self> {
        if self.gc_fence.as_ref().map(GcFence::id) != Some(fence_id) {
            return Err(contract_error("GC fence owner does not match"));
        }
        let mut root = self.clone();
        root.parent_root_digest = Some(parent_root_digest.to_owned());
        root.gc_fence = None;
        validate_root(&root)?;
        Ok(root)
    }

    /// Return the repository identity bound into every generation.
    #[must_use]
    pub fn repository_id(&self) -> &str {
        &self.repository_id
    }

    /// Return the monotonically increasing repository generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Return the exact previous root identity for a non-zero generation.
    #[must_use]
    pub fn parent_root_digest(&self) -> Option<&str> {
        self.parent_root_digest.as_deref()
    }

    /// Return the complete advertised ref map.
    #[must_use]
    pub fn refs(&self) -> &BTreeMap<String, String> {
        &self.refs
    }

    /// Return annotated-tag peeled targets.
    #[must_use]
    pub fn peeled_refs(&self) -> &BTreeMap<String, String> {
        &self.peeled_refs
    }

    /// Return the symbolic HEAD branch.
    #[must_use]
    pub fn head(&self) -> &str {
        &self.head
    }

    /// Return the complete checkpoint pinned by this generation, when present.
    #[must_use]
    pub fn checkpoint(&self) -> Option<&CheckpointPointer> {
        self.checkpoint.as_ref()
    }

    /// Return the active exclusive GC fence, when present.
    #[must_use]
    pub fn gc_fence(&self) -> Option<&GcFence> {
        self.gc_fence.as_ref()
    }

    /// Return the bounded post-checkpoint capsule frontier.
    #[must_use]
    pub fn capsule_frontier(&self) -> &[CapsulePointer] {
        &self.capsule_frontier
    }

    /// Return journal positions already folded into the checkpoint and root refs.
    #[must_use]
    pub fn compacted_ref_transactions(&self) -> &BTreeMap<String, String> {
        &self.compacted_ref_transactions
    }

    /// Return whether retained root evidence contains an exact transaction.
    #[must_use]
    pub fn contains_transaction(&self, transaction_id: &str) -> bool {
        self.capsule_frontier
            .iter()
            .any(|run| run.transaction_ids.iter().any(|id| id == transaction_id))
    }
}

/// A verified root plus its exact encoded bytes and content digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootRecord {
    root: RepositoryRoot,
    bytes: Bytes,
    digest: String,
}

impl RootRecord {
    /// Validate and encode one root into a checksummed bounded envelope.
    pub fn encode(root: RepositoryRoot) -> Result<Self> {
        validate_root(&root)?;
        let payload = serde_json::to_vec(&root).map_err(|source| {
            MetadataError::Internal(format!("root serialization failed: {source}"))
        })?;
        let payload_length = u64::try_from(payload.len())
            .map_err(|_| contract_error("root payload length cannot be represented as u64"))?;
        let mut bytes = Vec::with_capacity(ROOT_HEADER_BYTES + payload.len() + ROOT_DIGEST_BYTES);
        bytes.extend_from_slice(ROOT_MAGIC);
        bytes.extend_from_slice(&ROOT_VERSION.to_be_bytes());
        bytes.extend_from_slice(&payload_length.to_be_bytes());
        bytes.extend_from_slice(&payload);
        let digest = blake3::hash(&bytes);
        bytes.extend_from_slice(digest.as_bytes());
        enforce_root_size(bytes.len())?;
        Ok(Self {
            root,
            bytes: Bytes::from(bytes),
            digest: digest.to_hex().to_string(),
        })
    }

    /// Decode and verify one complete bounded repository-root envelope.
    pub fn decode(bytes: Bytes) -> Result<Self> {
        if bytes.len() as u64 > MAX_ROOT_BYTES {
            return Err(corrupt(format!(
                "root exceeds its {MAX_ROOT_BYTES}-byte limit"
            )));
        }
        if bytes.len() < ROOT_HEADER_BYTES + ROOT_DIGEST_BYTES {
            return Err(corrupt("root is shorter than its envelope"));
        }
        if &bytes[..ROOT_MAGIC.len()] != ROOT_MAGIC {
            return Err(corrupt("root magic is invalid"));
        }
        let version = u32::from_be_bytes(
            bytes[8..12]
                .try_into()
                .map_err(|_| corrupt("root version is truncated"))?,
        );
        if version != ROOT_VERSION {
            return Err(corrupt(format!(
                "root envelope must use version {ROOT_VERSION}"
            )));
        }
        let payload_length = u64::from_be_bytes(
            bytes[12..20]
                .try_into()
                .map_err(|_| corrupt("root payload length is truncated"))?,
        );
        let payload_length = usize::try_from(payload_length)
            .map_err(|_| corrupt("root payload length cannot be represented"))?;
        let payload_end = ROOT_HEADER_BYTES
            .checked_add(payload_length)
            .ok_or_else(|| corrupt("root payload length overflowed"))?;
        if payload_end + ROOT_DIGEST_BYTES != bytes.len() {
            return Err(corrupt("root payload length does not match its envelope"));
        }
        let actual_digest = blake3::hash(&bytes[..payload_end]);
        if actual_digest.as_bytes() != &bytes[payload_end..] {
            return Err(corrupt("root digest does not match"));
        }
        let root: RepositoryRoot =
            serde_json::from_slice(&bytes[ROOT_HEADER_BYTES..payload_end])
                .map_err(|source| corrupt(format!("root payload is invalid JSON: {source}")))?;
        validate_root(&root).map_err(|error| corrupt(error.to_string()))?;
        let canonical = serde_json::to_vec(&root).map_err(|source| {
            MetadataError::Internal(format!("root reserialization failed: {source}"))
        })?;
        if canonical.as_slice() != &bytes[ROOT_HEADER_BYTES..payload_end] {
            return Err(corrupt("root payload is not canonically encoded"));
        }
        Ok(Self {
            root,
            bytes,
            digest: actual_digest.to_hex().to_string(),
        })
    }

    /// Return the validated repository-root payload.
    #[must_use]
    pub fn root(&self) -> &RepositoryRoot {
        &self.root
    }

    /// Return the exact encoded root bytes.
    #[must_use]
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// Return the BLAKE3 identity of the encoded root envelope.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }
}

fn validate_capsule_pointer(pointer: &CapsulePointer) -> Result<()> {
    validate_content_hash(&pointer.hash, "root capsule hash", "capsule-protocol root")?;
    validate_content_hash(
        &pointer.newest_base_root_digest,
        "root capsule base digest",
        "capsule-protocol root",
    )?;
    let expected_count = 1_u32
        .checked_shl(u32::from(pointer.level))
        .ok_or_else(|| contract_error("root capsule run level is too large"))?;
    if pointer.size == 0
        || pointer.capsule_count != expected_count
        || usize::try_from(pointer.capsule_count).ok() != Some(pointer.transaction_ids.len())
    {
        return Err(contract_error("root capsule run descriptor is invalid"));
    }
    let mut transactions = BTreeSet::new();
    for transaction_id in &pointer.transaction_ids {
        validate_content_hash(
            transaction_id,
            "root transaction id",
            "capsule-protocol root",
        )?;
        if !transactions.insert(transaction_id) {
            return Err(contract_error(
                "root capsule run repeats a transaction identity",
            ));
        }
    }
    Ok(())
}

fn validate_checkpoint_pointer(pointer: &CheckpointPointer) -> Result<()> {
    validate_content_hash(
        &pointer.hash,
        "root checkpoint hash",
        "capsule-protocol root",
    )?;
    validate_content_hash(
        &pointer.covered_root_digest,
        "root checkpoint covered digest",
        "capsule-protocol root",
    )?;
    if pointer.size == 0 || pointer.pack_count == 0 || pointer.object_count == 0 {
        return Err(contract_error("checkpoint descriptor is out of bounds"));
    }
    Ok(())
}

fn validate_gc_fence(fence: &GcFence) -> Result<()> {
    validate_content_hash(&fence.id, "root GC fence id", "capsule-protocol root")?;
    if fence.expires_at_unix == 0 {
        return Err(contract_error("root GC fence expiry must be non-zero"));
    }
    Ok(())
}

fn validate_root(root: &RepositoryRoot) -> Result<()> {
    if root.version != ROOT_VERSION {
        return Err(corrupt(format!("root must use version {ROOT_VERSION}")));
    }
    validate_content_hash(
        &root.repository_id,
        "root repository id",
        "capsule-protocol root",
    )?;
    if !root.head.starts_with("refs/heads/") || !valid_ref_name(&root.head) {
        return Err(contract_error("root HEAD must name a branch"));
    }
    for (name, oid) in root.refs.iter().chain(root.peeled_refs.iter()) {
        if !name.starts_with("refs/") || !valid_ref_name(name) {
            return Err(contract_error("root contains an invalid ref name"));
        }
        validate_sha1(oid, "root ref object id", "capsule-protocol root")?;
    }
    if !valid_ref_namespace(root.refs.keys().map(String::as_str)) {
        return Err(contract_error("root contains conflicting ref names"));
    }
    if root
        .peeled_refs
        .keys()
        .any(|name| !root.refs.contains_key(name))
    {
        return Err(contract_error(
            "root contains a peeled target without its ref",
        ));
    }
    for (name, transaction_id) in &root.compacted_ref_transactions {
        if !name.starts_with("refs/") || !valid_ref_name(name) {
            return Err(contract_error(
                "root contains an invalid compacted ref position",
            ));
        }
        validate_content_hash(
            transaction_id,
            "compacted ref transaction id",
            "capsule-protocol root",
        )?;
    }
    if root.capsule_frontier.len() > MAX_CAPSULE_FRONTIER || root.delta_depth > MAX_DELTA_DEPTH {
        return Err(contract_error(
            "root capsule frontier is not bounded by delta depth",
        ));
    }
    if root.generation == 0 {
        if root.latest_transaction_base_digest.is_some()
            || root.checkpoint.is_some()
            || !root.capsule_frontier.is_empty()
            || !root.compacted_ref_transactions.is_empty()
        {
            return Err(contract_error(
                "generation-zero root cannot have publication state",
            ));
        }
        if let Some(parent) = root.parent_root_digest.as_deref() {
            validate_content_hash(parent, "root parent digest", "capsule-protocol root")?;
        }
    } else {
        let parent = root
            .parent_root_digest
            .as_deref()
            .ok_or_else(|| contract_error("non-zero root generation requires a parent digest"))?;
        validate_content_hash(parent, "root parent digest", "capsule-protocol root")?;
    }
    if let Some(fence) = &root.gc_fence {
        validate_gc_fence(fence)?;
    }
    if let Some(checkpoint) = &root.checkpoint {
        validate_checkpoint_pointer(checkpoint)?;
        if checkpoint.covered_generation > root.generation {
            return Err(contract_error(
                "checkpoint must cover a generation before its publishing root",
            ));
        }
    }
    let mut capsules = BTreeSet::new();
    let mut transactions = BTreeSet::new();
    let mut previous_level = None;
    let mut capsule_count = 0_u32;
    for run in &root.capsule_frontier {
        validate_capsule_pointer(run)?;
        if previous_level.is_some_and(|level| level <= run.level) {
            return Err(contract_error(
                "root capsule run levels must be strictly descending",
            ));
        }
        previous_level = Some(run.level);
        capsule_count = capsule_count
            .checked_add(run.capsule_count)
            .ok_or_else(|| contract_error("root capsule count overflowed"))?;
        if !capsules.insert(run.hash.as_str()) {
            return Err(contract_error("root capsule frontier repeats a run"));
        }
        for transaction_id in &run.transaction_ids {
            if !transactions.insert(transaction_id.as_str()) {
                return Err(contract_error(
                    "root capsule frontier repeats a transaction",
                ));
            }
        }
    }
    if capsule_count != root.delta_depth {
        return Err(contract_error(
            "root delta depth does not equal its capsule run inventory",
        ));
    }
    match (
        root.capsule_frontier.last(),
        root.latest_transaction_base_digest.as_deref(),
    ) {
        (None, None) => {}
        (Some(run), Some(base)) if run.newest_base_root_digest == base => {
            validate_content_hash(
                base,
                "latest transaction base digest",
                "capsule-protocol root",
            )?;
        }
        _ => {
            return Err(contract_error(
                "latest transaction base does not match the capsule frontier",
            ));
        }
    }
    Ok(())
}

fn enforce_root_size(size: usize) -> Result<()> {
    if size as u64 > MAX_ROOT_BYTES {
        return Err(contract_error(format!(
            "root exceeds its {MAX_ROOT_BYTES}-byte limit"
        )));
    }
    Ok(())
}

fn contract_error(reason: impl Into<String>) -> MetadataError {
    MetadataError::CapsuleContract {
        record: "root",
        reason: reason.into(),
    }
}

fn corrupt(reason: impl Into<String>) -> MetadataError {
    MetadataError::CorruptObject {
        path: "capsule-protocol root".to_owned(),
        reason: reason.into(),
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn root_round_trip_preserves_digest_and_generation() {
        let root = RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap();
        let encoded = RootRecord::encode(root).unwrap();

        let decoded = RootRecord::decode(encoded.bytes().clone()).unwrap();

        assert_eq!(decoded.digest(), encoded.digest());
        assert_eq!(decoded.root().generation(), 0);
    }

    #[test]
    fn root_rejects_corrupt_digest() {
        let root = RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap();
        let encoded = RootRecord::encode(root).unwrap();
        let mut bytes = encoded.bytes().to_vec();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;

        let error = RootRecord::decode(Bytes::from(bytes)).expect_err("corruption must fail");

        assert!(matches!(error, MetadataError::CorruptObject { .. }));
    }

    fn advance_with_synthetic_run(record: &RootRecord, sequence: u64) -> Result<RootRecord> {
        let transaction_id = format!("{:064x}", sequence + 10);
        let mut frontier = record.root().capsule_frontier().to_vec();
        let mut level = 0_u8;
        let mut transaction_ids = vec![transaction_id.clone()];
        while frontier
            .last()
            .is_some_and(|pointer| pointer.level() == level)
        {
            let older = frontier
                .pop()
                .ok_or_else(|| contract_error("synthetic frontier became empty"))?;
            let mut merged = older.transaction_ids().to_vec();
            merged.extend(transaction_ids);
            transaction_ids = merged;
            level += 1;
        }
        frontier.push(CapsulePointer::new(
            format!(
                "{:064x}",
                sequence.saturating_mul(16) + u64::from(level) + 1
            ),
            1,
            level,
            transaction_ids,
            record.digest(),
        )?);
        RootRecord::encode(record.root().advance(
            record.digest(),
            BTreeMap::new(),
            BTreeMap::new(),
            frontier,
            &transaction_id,
        )?)
    }

    #[test]
    fn delta_limit_requires_checkpoint_before_transaction_501() {
        let mut record = RootRecord::encode(
            RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap(),
        )
        .unwrap();
        for generation in 0..MAX_DELTA_DEPTH {
            record = advance_with_synthetic_run(&record, u64::from(generation)).unwrap();
        }

        let error = advance_with_synthetic_run(&record, u64::from(MAX_DELTA_DEPTH))
            .expect_err("checkpoint must bound the transaction window");

        assert!(matches!(error, MetadataError::CapsuleContract { .. }));
    }

    #[test]
    fn checkpoint_resets_frontier_before_the_next_push() {
        let mut record = RootRecord::encode(
            RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap(),
        )
        .unwrap();
        for generation in 0..10 {
            record = advance_with_synthetic_run(&record, generation).unwrap();
        }
        let pointer = CheckpointPointer::new(
            "b".repeat(64),
            100,
            record.root().generation(),
            record.digest(),
            1,
            1,
        )
        .unwrap();
        let checkpoint_root = record
            .root()
            .install_checkpoint(record.digest(), pointer)
            .unwrap();
        let checkpoint_record = RootRecord::encode(checkpoint_root).unwrap();

        let next = advance_with_synthetic_run(&checkpoint_record, 100)
            .unwrap()
            .root()
            .clone();

        assert_eq!(checkpoint_record.root().capsule_frontier().len(), 0);
        assert_eq!(next.capsule_frontier().len(), 1);
        assert_eq!(checkpoint_record.root().generation(), 10);
        assert_eq!(next.generation(), 11);
    }

    #[test]
    fn gc_fence_preserves_logical_state_and_blocks_publication() {
        let initial = RootRecord::encode(
            RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap(),
        )
        .unwrap();
        let published = advance_with_synthetic_run(&initial, 1).unwrap();
        let fence = GcFence::new("f".repeat(64), 1).unwrap();
        let fenced = RootRecord::encode(
            published
                .root()
                .begin_gc(published.digest(), fence)
                .unwrap(),
        )
        .unwrap();

        assert_eq!(fenced.root().generation(), published.root().generation());
        assert_eq!(fenced.root().refs(), published.root().refs());
        assert_eq!(
            fenced.root().capsule_frontier(),
            published.root().capsule_frontier()
        );
        assert!(advance_with_synthetic_run(&fenced, 2).is_err());

        let released = RootRecord::encode(
            fenced
                .root()
                .end_gc(fenced.digest(), &"f".repeat(64))
                .unwrap(),
        )
        .unwrap();
        assert!(released.root().gc_fence().is_none());
        assert!(advance_with_synthetic_run(&released, 2).is_ok());
    }
}
