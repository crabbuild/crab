use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::error::{MetadataError, Result};
use crate::request_minimal::CapsuleSectionLocation;
use crate::validation::{validate_content_hash, validate_sha1};

const ROOT_MAGIC: &[u8; 8] = b"CRBROOT2";
const ROOT_VERSION: u32 = 2;
const ROOT_HEADER_BYTES: usize = ROOT_MAGIC.len() + 4 + 8;
const ROOT_DIGEST_BYTES: usize = 32;
/// Maximum encoded repository-root size accepted by readers and writers.
pub const MAX_ROOT_BYTES: u64 = 8 * 1024 * 1024;
/// Maximum post-checkpoint capsules kept in one repository root.
pub const MAX_CAPSULE_FRONTIER: usize = 7;

/// Root reference to one durable immutable capsule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapsulePointer {
    hash: String,
    size: u64,
    transaction_id: String,
    base_root_digest: String,
}

impl CapsulePointer {
    /// Create and validate a root pointer to an immutable capsule.
    pub fn new(
        hash: impl Into<String>,
        size: u64,
        transaction_id: impl Into<String>,
        base_root_digest: impl Into<String>,
    ) -> Result<Self> {
        let pointer = Self {
            hash: hash.into(),
            size,
            transaction_id: transaction_id.into(),
            base_root_digest: base_root_digest.into(),
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

    /// Return the embedded ref transaction identity.
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    /// Return the repository-root digest on which the capsule depends.
    #[must_use]
    pub fn base_root_digest(&self) -> &str {
        &self.base_root_digest
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
    git_pack: CapsuleSectionLocation,
    git_checksum: String,
    object_count: u64,
}

impl CheckpointPointer {
    /// Create a checkpoint pointer whose complete Git pack is range-addressable.
    pub fn new(
        hash: impl Into<String>,
        size: u64,
        covered_generation: u64,
        covered_root_digest: impl Into<String>,
        git_pack: CapsuleSectionLocation,
        git_checksum: impl Into<String>,
        object_count: u64,
    ) -> Result<Self> {
        let pointer = Self {
            hash: hash.into(),
            size,
            covered_generation,
            covered_root_digest: covered_root_digest.into(),
            git_pack,
            git_checksum: git_checksum.into(),
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

    /// Return the range containing the checkpoint's complete Git pack.
    #[must_use]
    pub fn git_pack(&self) -> &CapsuleSectionLocation {
        &self.git_pack
    }

    /// Return the Git pack trailer checksum.
    #[must_use]
    pub fn git_checksum(&self) -> &str {
        &self.git_checksum
    }

    /// Return the number of Git objects in the complete checkpoint pack.
    #[must_use]
    pub fn object_count(&self) -> u64 {
        self.object_count
    }
}

/// Complete mutable authority for one request-minimal repository generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryRoot {
    version: u32,
    repository_id: String,
    generation: u64,
    parent_root_digest: Option<String>,
    refs: BTreeMap<String, String>,
    peeled_refs: BTreeMap<String, String>,
    head: String,
    checkpoint: Option<CheckpointPointer>,
    capsule_frontier: Vec<CapsulePointer>,
    delta_depth: u32,
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
            refs: BTreeMap::new(),
            peeled_refs: BTreeMap::new(),
            head: head.to_owned(),
            checkpoint: None,
            capsule_frontier: Vec::new(),
            delta_depth: 0,
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
        capsule: CapsulePointer,
    ) -> Result<Self> {
        if capsule.base_root_digest != parent_root_digest {
            return Err(contract_error(
                "capsule base does not match the parent root",
            ));
        }
        if self.capsule_frontier.len() >= MAX_CAPSULE_FRONTIER {
            return Err(contract_error(format!(
                "capsule frontier reached its {MAX_CAPSULE_FRONTIER}-generation checkpoint limit"
            )));
        }
        let mut frontier = self.capsule_frontier.clone();
        frontier.push(capsule);
        let root = Self {
            version: ROOT_VERSION,
            repository_id: self.repository_id.clone(),
            generation: self
                .generation
                .checked_add(1)
                .ok_or_else(|| contract_error("root generation overflowed"))?,
            parent_root_digest: Some(parent_root_digest.to_owned()),
            refs,
            peeled_refs,
            head: self.head.clone(),
            checkpoint: self.checkpoint.clone(),
            capsule_frontier: frontier,
            delta_depth: self.delta_depth + 1,
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
            refs: self.refs.clone(),
            peeled_refs: self.peeled_refs.clone(),
            head: self.head.clone(),
            checkpoint: Some(checkpoint),
            capsule_frontier: Vec::new(),
            delta_depth: 0,
            capabilities: self.capabilities.clone(),
        };
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

    /// Return the bounded post-checkpoint capsule frontier.
    #[must_use]
    pub fn capsule_frontier(&self) -> &[CapsulePointer] {
        &self.capsule_frontier
    }

    /// Return whether retained root evidence contains an exact transaction.
    #[must_use]
    pub fn contains_transaction(&self, transaction_id: &str) -> bool {
        self.capsule_frontier
            .iter()
            .any(|capsule| capsule.transaction_id == transaction_id)
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
    validate_content_hash(&pointer.hash, "root capsule hash", "request-minimal root")?;
    validate_content_hash(
        &pointer.transaction_id,
        "root transaction id",
        "request-minimal root",
    )?;
    validate_content_hash(
        &pointer.base_root_digest,
        "root capsule base digest",
        "request-minimal root",
    )?;
    if pointer.size == 0 {
        return Err(contract_error("root capsule size must be non-zero"));
    }
    Ok(())
}

fn validate_checkpoint_pointer(pointer: &CheckpointPointer) -> Result<()> {
    validate_content_hash(
        &pointer.hash,
        "root checkpoint hash",
        "request-minimal root",
    )?;
    validate_content_hash(
        &pointer.covered_root_digest,
        "root checkpoint covered digest",
        "request-minimal root",
    )?;
    validate_content_hash(
        pointer.git_pack.blake3(),
        "root checkpoint pack hash",
        "request-minimal root",
    )?;
    validate_sha1(
        &pointer.git_checksum,
        "root checkpoint Git checksum",
        "request-minimal root",
    )?;
    let pack_end = pointer
        .git_pack
        .offset()
        .checked_add(pointer.git_pack.length())
        .ok_or_else(|| contract_error("checkpoint Git pack range overflowed"))?;
    if pointer.size == 0
        || pointer.object_count == 0
        || pointer.git_pack.kind() != crate::request_minimal::CapsuleSectionKind::GitPack
        || pointer.git_pack.length() == 0
        || pack_end > pointer.size
    {
        return Err(contract_error("checkpoint descriptor is out of bounds"));
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
        "request-minimal root",
    )?;
    if !root.head.starts_with("refs/heads/")
        || crab_git::refname::validate_push_refname(&root.head).is_err()
    {
        return Err(contract_error("root HEAD must name a branch"));
    }
    for (name, oid) in root.refs.iter().chain(root.peeled_refs.iter()) {
        if !name.starts_with("refs/") || crab_git::refname::validate_push_refname(name).is_err() {
            return Err(contract_error("root contains an invalid ref name"));
        }
        validate_sha1(oid, "root ref object id", "request-minimal root")?;
    }
    crab_git::refname::validate_ref_namespace(root.refs.keys().map(String::as_str))
        .map_err(|error| contract_error(error.to_string()))?;
    if root
        .peeled_refs
        .keys()
        .any(|name| !root.refs.contains_key(name))
    {
        return Err(contract_error(
            "root contains a peeled target without its ref",
        ));
    }
    if root.capsule_frontier.len() > MAX_CAPSULE_FRONTIER
        || root.delta_depth as usize != root.capsule_frontier.len()
    {
        return Err(contract_error(
            "root capsule frontier is not bounded by delta depth",
        ));
    }
    if root.generation == 0 {
        if root.parent_root_digest.is_some()
            || root.checkpoint.is_some()
            || !root.capsule_frontier.is_empty()
        {
            return Err(contract_error(
                "generation-zero root cannot have a parent or frontier",
            ));
        }
    } else {
        let parent = root
            .parent_root_digest
            .as_deref()
            .ok_or_else(|| contract_error("non-zero root generation requires a parent digest"))?;
        validate_content_hash(parent, "root parent digest", "request-minimal root")?;
        if root
            .capsule_frontier
            .last()
            .is_some_and(|capsule| capsule.base_root_digest != parent)
        {
            return Err(contract_error(
                "newest capsule does not extend the root parent generation",
            ));
        }
    }
    if let Some(checkpoint) = &root.checkpoint {
        validate_checkpoint_pointer(checkpoint)?;
        if checkpoint.covered_generation >= root.generation {
            return Err(contract_error(
                "checkpoint must cover a generation before its publishing root",
            ));
        }
    }
    let mut capsules = BTreeSet::new();
    let mut transactions = BTreeSet::new();
    for capsule in &root.capsule_frontier {
        validate_capsule_pointer(capsule)?;
        if !capsules.insert(capsule.hash.as_str())
            || !transactions.insert(capsule.transaction_id.as_str())
        {
            return Err(contract_error(
                "root capsule frontier repeats a capsule or transaction",
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
    MetadataError::RequestMinimalContract {
        record: "root",
        reason: reason.into(),
    }
}

fn corrupt(reason: impl Into<String>) -> MetadataError {
    MetadataError::CorruptObject {
        path: "request-minimal root".to_owned(),
        reason: reason.into(),
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;
    use crate::request_minimal::{
        Capsule, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind, CapsuleTransaction,
    };

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

    #[test]
    fn frontier_limit_requires_checkpoint_before_an_eighth_delta() {
        let mut record = RootRecord::encode(
            RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap(),
        )
        .unwrap();
        for generation in 0..MAX_CAPSULE_FRONTIER {
            let pointer = CapsulePointer::new(
                format!("{generation:064x}"),
                1,
                format!("{:064x}", generation + 10),
                record.digest(),
            )
            .unwrap();
            let root = record
                .root()
                .advance(record.digest(), BTreeMap::new(), BTreeMap::new(), pointer)
                .unwrap();
            record = RootRecord::encode(root).unwrap();
        }
        let pointer =
            CapsulePointer::new("f".repeat(64), 1, "e".repeat(64), record.digest()).unwrap();

        let error = record
            .root()
            .advance(record.digest(), BTreeMap::new(), BTreeMap::new(), pointer)
            .expect_err("frontier must stay bounded");

        assert!(matches!(
            error,
            MetadataError::RequestMinimalContract { .. }
        ));
    }

    #[test]
    fn checkpoint_resets_frontier_before_the_next_push() {
        let mut record = RootRecord::encode(
            RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap(),
        )
        .unwrap();
        for generation in 0..MAX_CAPSULE_FRONTIER {
            let pointer = CapsulePointer::new(
                format!("{generation:064x}"),
                1,
                format!("{:064x}", generation + 10),
                record.digest(),
            )
            .unwrap();
            record = RootRecord::encode(
                record
                    .root()
                    .advance(record.digest(), BTreeMap::new(), BTreeMap::new(), pointer)
                    .unwrap(),
            )
            .unwrap();
        }
        let checkpoint_transaction = CapsuleTransaction::new(
            record.digest(),
            vec![CapsuleRefEdit::new(
                "refs/heads/checkpoint-evidence",
                None,
                Some("a".repeat(40)),
                None,
            )],
        )
        .unwrap();
        let checkpoint = Capsule::build(
            &checkpoint_transaction,
            vec![CapsuleSection::new(
                CapsuleSectionKind::GitPack,
                Bytes::from_static(b"PACK checkpoint"),
            )],
        )
        .unwrap();
        let pack = checkpoint
            .sections()
            .iter()
            .find(|section| section.kind() == CapsuleSectionKind::GitPack)
            .unwrap()
            .clone();
        let pointer = CheckpointPointer::new(
            checkpoint.hash(),
            checkpoint.bytes().len() as u64,
            record.root().generation(),
            record.digest(),
            pack,
            "b".repeat(40),
            1,
        )
        .unwrap();
        let checkpoint_root = record
            .root()
            .install_checkpoint(record.digest(), pointer)
            .unwrap();
        let checkpoint_record = RootRecord::encode(checkpoint_root).unwrap();

        let next_pointer = CapsulePointer::new(
            "c".repeat(64),
            1,
            "d".repeat(64),
            checkpoint_record.digest(),
        )
        .unwrap();
        let next = checkpoint_record
            .root()
            .advance(
                checkpoint_record.digest(),
                BTreeMap::new(),
                BTreeMap::new(),
                next_pointer,
            )
            .unwrap();

        assert_eq!(checkpoint_record.root().capsule_frontier().len(), 0);
        assert_eq!(next.capsule_frontier().len(), 1);
    }
}
