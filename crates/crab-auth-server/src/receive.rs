//! Protected-push receive validation helpers.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::Cursor;
use std::path::Path;
use std::process::Command;

use crab_auth::{PushRefUpdate, validate_push_ref_update, validate_push_ref_updates};
use crab_coordination::active_active::{
    self as coordination_active_active, ActiveActiveReplicationConfig,
};
use crab_git::normalize_repository_prefix;
use crab_metadata::ref_registry::ActiveActiveCoordinatorRegistration;
use crab_storage::{
    StagedWrite, StorageError, StorageProviderKind, Store, canonical_global_content_path,
    content_hash_from_path,
};
use crab_xet::hash::{MerkleHash, compute_data_hash};
use crab_xet::shard::{MDBShardInfo, ShardReader};
use crab_xet::xorb::parser::XorbParser;
use object_store::path::Path as ObjectPath;
use serde::{Deserialize, Serialize};

pub use crab_remote::protected::ProtectedCapsulePushPlan;

use crate::error::{AuthServerError, Result};

mod capsule;
mod capsule_dependencies;
mod git_workspace;
mod session;
mod workflow;

pub use session::ReceiveContext;
pub use workflow::{
    PreparedReceive, VerifiedReceive, commit_receive, prepare_receive, verify_receive,
};

/// Maximum number of protected-push ref updates accepted by receive helpers.
pub const MAX_PUSH_REF_UPDATES: usize = 32;

#[cfg(not(test))]
const MAX_PUSH_STAGED_OBJECTS: usize = 100_000;
#[cfg(test)]
const MAX_PUSH_STAGED_OBJECTS: usize = 16;
const SHARD_V1_MAGIC: &[u8; 4] = b"SH01";
const SHARD_V1_TRAILER_SIZE: usize = 12;
const SHARD_CLOSURE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProtectedShardClosure {
    schema_version: u32,
    shard_hash: String,
    content_hash: String,
    content_size: u64,
    xorb_count: u64,
    file_count: u64,
    xorb_hashes: Vec<String>,
    file_hashes: Vec<String>,
}

/// Chunk metadata that a staged xorb must expose to satisfy staged shard terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedXorbChunk {
    pub index: u32,
    pub hash: MerkleHash,
    pub uncompressed_size: u32,
}

/// Active-active options accepted by protected-push receive commits.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ActiveActiveReceiveConfig {
    pub replication: ActiveActiveReplicationConfig,
    pub writer: String,
}

/// Prepared protected-push session state written after view authorization.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct PushPrepareRecord {
    pub schema_version: u32,
    pub repo_prefix: String,
    pub push_id: String,
    pub source_manifest_generation: u64,
    pub source_manifest_etag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_root_digest: Option<String>,
    pub view_ref_updates: Vec<PushRefUpdate>,
    pub source_ref_updates: Vec<PushRefUpdate>,
    pub view_scope: Option<PreparedViewScope>,
}

/// Prepared protected-view scope carried into a receive operation.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PreparedViewScope {
    pub repo_prefix: String,
    pub global_prefix: String,
    pub source_repo: String,
    pub scope_hash: String,
}

/// Parses and validates active-active receive JSON for a repository URL.
pub fn parse_active_active_receive_config(
    json: Option<&str>,
    repo_url: &str,
) -> Result<Option<ActiveActiveReceiveConfig>> {
    let Some(json) = json.map(str::trim).filter(|json| !json.is_empty()) else {
        return Ok(None);
    };
    let active_active: ActiveActiveReceiveConfig = serde_json::from_str(json)
        .map_err(|e| invalid(format!("invalid active-active JSON: {e}")))?;
    coordination_active_active::validate_active_active_config(&active_active.replication)?;
    if !active_active.replication.is_active_active() {
        return Err(invalid(
            "active-active JSON must configure active-active mode",
        ));
    }
    let expected_writer = coordination_active_active::active_active_writer_name_for_remote(
        &active_active.replication,
        Some(repo_url),
    )?;
    if active_active.writer != expected_writer {
        return Err(invalid(format!(
            "active-active writer {} does not match repo_url writer {expected_writer}",
            active_active.writer
        )));
    }
    Ok(Some(active_active))
}

/// Builds the ref-registry registration for a receive-side active-active config.
pub fn active_active_coordinator_registration(
    replication: &ActiveActiveReplicationConfig,
) -> Result<ActiveActiveCoordinatorRegistration> {
    let coordinator = replication
        .coordinator
        .as_ref()
        .ok_or_else(|| invalid("active-active JSON requires coordinator"))?;
    let resource =
        coordination_active_active::active_active_coordinator_resource(&coordinator.url)?;
    Ok(ActiveActiveCoordinatorRegistration {
        provider: resource.provider.as_str().to_owned(),
        url: coordinator.url.clone(),
        region: coordinator.region.clone(),
        failover_regions: coordinator.failover_regions.clone(),
    })
}

/// Validates the opaque push session token shape.
pub fn validate_push_id(push_id: &str) -> Result<()> {
    let normalized = push_id.trim();
    if normalized.len() != 32
        || !normalized
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        return Err(invalid(
            "push_id must be a 32-character lowercase hex token",
        ));
    }
    Ok(())
}

/// Parses the storage provider accepted by protected-push receive helpers.
pub fn receive_provider(provider: &str) -> Result<StorageProviderKind> {
    StorageProviderKind::parse_cloud_alias(provider)
        .ok_or_else(|| invalid(format!("unsupported receive provider: {}", provider.trim())))
}

/// Validates prepared ref updates stored for a protected push.
pub fn validate_prepared_ref_updates(updates: &[PushRefUpdate]) -> Result<()> {
    if updates.is_empty() {
        return Err(invalid("prepare record requires at least one ref update"));
    }
    if updates.len() > MAX_PUSH_REF_UPDATES {
        return Err(invalid("prepare record contains too many ref updates"));
    }
    validate_push_ref_updates(updates).map_err(|error| invalid(error.to_string()))
}

/// Validates the bounded identity and staged-object shape of a v2 push plan.
pub fn validate_protected_capsule_plan_shape(
    plan: &ProtectedCapsulePushPlan,
    repo_prefix: &str,
    push_id: &str,
) -> Result<()> {
    if plan.schema_version != crab_remote::protected::PROTECTED_CAPSULE_PUSH_PLAN_SCHEMA_VERSION {
        return Err(invalid("unsupported capsule push-plan schema_version"));
    }
    if plan.repo_prefix != repo_prefix {
        return Err(invalid("push-plan repo_prefix does not match repo_url"));
    }
    if plan.push_id != push_id {
        return Err(invalid("push-plan push_id does not match request"));
    }
    let expected_prefix = format!("{repo_prefix}/staging/{push_id}/");
    if plan.upload_prefix.trim_matches('/') != expected_prefix.trim_matches('/') {
        return Err(invalid("push-plan upload_prefix does not match push_id"));
    }
    validate_hash_component(&plan.base_root_digest, "base root digest")?;
    validate_hash_component(&plan.transaction_id, "transaction id")?;
    validate_hash_component(&plan.run_hash, "capsule run hash")?;
    if plan.run_size == 0 {
        return Err(invalid("capsule push-plan declares an empty capsule"));
    }
    if plan.ref_updates.is_empty() {
        return Err(invalid("protected push requires at least one ref update"));
    }
    if plan.ref_updates.len() > MAX_PUSH_REF_UPDATES {
        return Err(invalid("push-plan contains too many ref updates"));
    }
    if plan.staged_objects.len() > MAX_PUSH_STAGED_OBJECTS {
        return Err(invalid("push-plan contains too many staged objects"));
    }
    validate_push_ref_updates(&plan.ref_updates).map_err(|error| invalid(error.to_string()))?;
    validate_staged_write_shapes(&plan.staged_objects, repo_prefix, push_id)?;

    let partition = &plan.run_hash[..2];
    let capsule_key = format!(
        "{}/v2/capsules/{partition}/{}",
        repo_prefix.trim_matches('/'),
        plan.run_hash
    );
    if let Some(capsule) = plan
        .staged_objects
        .iter()
        .find(|object| object.canonical_key == capsule_key)
        && (capsule.blake3 != plan.run_hash || capsule.size != plan.run_size)
    {
        return Err(invalid(
            "staged capsule metadata differs from the push-plan capsule",
        ));
    }
    Ok(())
}

fn validate_staged_write_shapes(
    staged_objects: &[StagedWrite],
    repo_prefix: &str,
    push_id: &str,
) -> Result<()> {
    let mut canonical_keys = BTreeSet::new();
    let mut staged_keys = BTreeSet::new();
    for object in staged_objects {
        if !canonical_keys.insert(object.canonical_key.as_str()) {
            return Err(invalid("push-plan contains duplicate canonical object key"));
        }
        if !staged_keys.insert(object.staged_key.as_str()) {
            return Err(invalid("push-plan contains duplicate staged object key"));
        }
        validate_canonical_key(repo_prefix, &object.canonical_key)?;
        let expected_staged_key = format!(
            "{}/staging/{}/objects/{}",
            repo_prefix,
            push_id,
            object.canonical_key.trim_start_matches('/')
        );
        if object.staged_key != expected_staged_key {
            return Err(invalid("staged object path does not match canonical key"));
        }
    }
    Ok(())
}

/// Validates staged-object bytes against the declared object metadata.
pub fn validate_staged_object_bytes(object: &StagedWrite, bytes: &[u8]) -> Result<()> {
    let hash = blake3::hash(bytes).to_hex().to_string();
    if hash != object.blake3 {
        return Err(invalid(format!(
            "staged object hash mismatch for {}",
            object.staged_key
        )));
    }
    if bytes.len() as u64 != object.size {
        return Err(invalid(format!(
            "staged object size mismatch for {}",
            object.staged_key
        )));
    }
    validate_key_content_hash(&object.canonical_key, &hash)
}

/// Reads a staged object and validates the bytes against its declared metadata.
pub async fn read_verified_staged_object(
    store: &Store,
    object: &StagedWrite,
) -> Result<bytes::Bytes> {
    let path = ObjectPath::from(object.staged_key.clone());
    let (bytes, _) = store.get_with_etag_bounded(&path, object.size).await?;
    validate_staged_object_bytes(object, &bytes)?;
    Ok(bytes)
}

pub(super) async fn promote_staged_writes(
    store: &Store,
    staged_objects: &[StagedWrite],
) -> Result<()> {
    for object in staged_objects {
        let canonical = ObjectPath::from(object.canonical_key.clone());
        let bytes = read_verified_staged_object(store, object).await?;
        let shard_body = content_hash_from_path(&object.canonical_key, "shards")
            .is_some()
            .then(|| bytes.clone());
        store.put(&canonical, bytes).await?;
        if let Some(shard_body) = shard_body {
            publish_protected_shard_closure(store, &object.canonical_key, &shard_body).await?;
        }
    }
    Ok(())
}

async fn publish_protected_shard_closure(
    store: &Store,
    shard_path: &str,
    body: &[u8],
) -> Result<()> {
    let Some(hash_hex) = content_hash_from_path(shard_path, "shards") else {
        return Ok(());
    };
    let expected_hash = MerkleHash::from_hex(hash_hex)
        .map_err(|error| invalid(format!("protected shard hash is invalid: {error}")))?;
    let actual_hash = compute_data_hash(body);
    if actual_hash != expected_hash {
        return Err(invalid(format!(
            "protected shard body hash is {}, expected {}",
            actual_hash.hex(),
            expected_hash.hex()
        )));
    }

    let reader = ShardReader::from_bytes(bytes::Bytes::copy_from_slice(body), expected_hash);
    let shard_info = reader
        .shard_info_public()
        .map_err(|error| invalid(format!("protected shard closure parse failed: {error}")))?;
    let v1_bytes = reader.v1_data();
    let mut xorb_hashes = HashSet::new();
    let mut cursor = Cursor::new(v1_bytes);
    let blocks = shard_info
        .read_all_xorb_blocks_full(&mut cursor)
        .map_err(|error| invalid(format!("protected shard xorb closure failed: {error}")))?;
    for block in &blocks {
        xorb_hashes.insert(block.metadata.xorb_hash.hex());
    }
    let mut file_hashes = HashSet::new();
    let mut cursor = Cursor::new(v1_bytes);
    let files = shard_info
        .read_all_file_info_sections(&mut cursor)
        .map_err(|error| invalid(format!("protected shard file closure failed: {error}")))?;
    for file in &files {
        file_hashes.insert(file.metadata.file_hash.hex());
    }
    let mut xorb_hashes = xorb_hashes.into_iter().collect::<Vec<_>>();
    let mut file_hashes = file_hashes.into_iter().collect::<Vec<_>>();
    xorb_hashes.sort_unstable();
    file_hashes.sort_unstable();
    let content_size = u64::try_from(body.len())
        .map_err(|_| invalid("protected shard size overflows closure metadata"))?;
    let closure = ProtectedShardClosure {
        schema_version: SHARD_CLOSURE_SCHEMA_VERSION,
        shard_hash: expected_hash.hex(),
        content_hash: actual_hash.hex(),
        content_size,
        xorb_count: u64::try_from(xorb_hashes.len())
            .map_err(|_| invalid("protected shard xorb count overflows closure metadata"))?,
        file_count: u64::try_from(file_hashes.len())
            .map_err(|_| invalid("protected shard file count overflows closure metadata"))?,
        xorb_hashes,
        file_hashes,
    };
    let Some(prefix_end) = shard_path.rfind("/shards/") else {
        return Err(invalid("protected shard path has no global prefix"));
    };
    let closure_path = ObjectPath::from(format!(
        "{}/gc/closures/{}.json",
        &shard_path[..prefix_end],
        hash_hex
    ));
    let encoded = serde_json::to_vec(&closure).map_err(|error| {
        invalid(format!(
            "protected shard closure serialization failed: {error}"
        ))
    })?;
    let encoded = bytes::Bytes::from(encoded);
    match store.create_strict(&closure_path, encoded.clone()).await {
        Ok(()) => Ok(()),
        Err(StorageError::StateConflict { .. }) => {
            let (existing, _) = store.get_with_etag(&closure_path).await?;
            if existing == encoded {
                Ok(())
            } else {
                Err(invalid(
                    "existing protected shard closure conflicts with the content-addressed publication",
                ))
            }
        }
        Err(error) => Err(error.into()),
    }
}

/// Returns strict xorb references declared by a staged shard object.
pub fn strict_xorb_references_from_shard(
    bytes: &[u8],
) -> Result<BTreeMap<String, Vec<ExpectedXorbChunk>>> {
    let shard_bytes = strip_shard_bloom_trailer(bytes);
    let mut cursor = Cursor::new(shard_bytes);
    let shard = MDBShardInfo::load_from_reader(&mut cursor)
        .map_err(|e| invalid(format!("invalid staged shard object: {e}")))?;
    let xorbs = shard
        .read_all_xorb_blocks_full(&mut cursor)
        .map_err(|e| invalid(format!("invalid staged shard xorb section: {e}")))?;
    let mut refs = BTreeMap::new();
    for xorb in xorbs {
        let hash = xorb.metadata.xorb_hash.hex();
        validate_hash_component(&hash, "staged shard xorb hash")?;
        let declared_chunks = usize::try_from(xorb.metadata.num_entries)
            .map_err(|_| invalid("staged shard xorb chunk count overflows usize"))?;
        let mut chunks = Vec::with_capacity(xorb.chunks.len());
        for (index, chunk) in xorb.chunks.iter().enumerate() {
            chunks.push(ExpectedXorbChunk {
                index: u32::try_from(index)
                    .map_err(|_| invalid("staged shard xorb chunk index overflows u32"))?,
                hash: chunk.chunk_hash,
                uncompressed_size: chunk.unpacked_segment_bytes,
            });
        }
        if chunks.len() != declared_chunks {
            return Err(invalid("staged shard xorb chunk count mismatch"));
        }
        let key = canonical_global_content_path("xorbs", &hash).to_string();
        if refs.insert(key, chunks).is_some() {
            return Err(invalid("staged shard contains duplicate xorb metadata"));
        }
    }
    Ok(refs)
}

/// Removes the optional shard bloom trailer before Xet MDB parsing.
#[must_use]
pub fn strip_shard_bloom_trailer(data: &[u8]) -> &[u8] {
    if data.len() >= SHARD_V1_TRAILER_SIZE && &data[data.len() - 4..] == SHARD_V1_MAGIC {
        let offset_start = data.len() - SHARD_V1_TRAILER_SIZE;
        if let Ok(bytes) = data[offset_start..offset_start + 8].try_into() {
            let bloom_offset = u64::from_le_bytes(bytes) as usize;
            if bloom_offset <= data.len() {
                return &data[..bloom_offset];
            }
        }
    }
    data
}

/// Builds a prepared session record from one authenticated capsule view.
pub fn build_capsule_prepare_record(
    repo_prefix: &str,
    push_id: &str,
    source_generation: u64,
    source_root_digest: &str,
    source_refs: &BTreeMap<String, String>,
    view_ref_updates: Vec<PushRefUpdate>,
    view_scope: Option<PreparedViewScope>,
) -> Result<PushPrepareRecord> {
    validate_prepared_ref_updates(&view_ref_updates)?;
    validate_hash_component(source_root_digest, "source root digest")?;
    if let Some(scope) = view_scope.as_ref() {
        validate_prepared_view_scope(scope, repo_prefix)?;
    }
    let source_ref_updates = source_ref_updates_for_refs(source_refs, &view_ref_updates)?;
    Ok(PushPrepareRecord {
        schema_version: 2,
        repo_prefix: repo_prefix.to_owned(),
        push_id: push_id.to_owned(),
        // Keep the shipped response field stable while schema v2 identifies
        // this value as a capsule-root generation, not a v1 manifest.
        source_manifest_generation: source_generation,
        source_manifest_etag: String::new(),
        source_root_digest: Some(source_root_digest.to_owned()),
        view_ref_updates,
        source_ref_updates,
        view_scope,
    })
}

/// Validates a captured Capsule prepare record before verification or recovery.
pub fn validate_prepare_record_shape(
    record: &PushPrepareRecord,
    repo_prefix: &str,
    push_id: &str,
) -> Result<()> {
    if record.schema_version != 2 {
        return Err(invalid("unsupported prepare record schema_version"));
    }
    if !record.source_manifest_etag.is_empty() {
        return Err(invalid(
            "capsule prepare record cannot contain a manifest etag",
        ));
    }
    validate_hash_component(
        record
            .source_root_digest
            .as_deref()
            .ok_or_else(|| invalid("capsule prepare record is missing its root digest"))?,
        "source root digest",
    )?;
    if record.repo_prefix != repo_prefix {
        return Err(invalid(
            "prepare record repo_prefix does not match repo_url",
        ));
    }
    if record.push_id != push_id {
        return Err(invalid("prepare record push_id does not match request"));
    }
    validate_prepared_ref_updates(&record.view_ref_updates)?;
    validate_prepared_ref_updates(&record.source_ref_updates)?;
    if ref_names(&record.view_ref_updates) != ref_names(&record.source_ref_updates) {
        return Err(invalid("prepare record source refs do not match view refs"));
    }
    if let Some(scope) = record.view_scope.as_ref() {
        validate_prepared_view_scope(scope, repo_prefix)?;
    }
    Ok(())
}

/// Validates a prepared protected-view scope for a source repository.
pub fn validate_prepared_view_scope(
    scope: &PreparedViewScope,
    source_repo_prefix: &str,
) -> Result<()> {
    let repo_prefix =
        normalize_repository_prefix(&scope.repo_prefix).map_err(AuthServerError::from)?;
    if repo_prefix != scope.repo_prefix {
        return Err(invalid("prepared view repo_prefix is not normalized"));
    }
    let source_repo =
        normalize_repository_prefix(&scope.source_repo).map_err(AuthServerError::from)?;
    if source_repo != source_repo_prefix {
        return Err(invalid("prepared view source_repo does not match repo_url"));
    }
    if source_repo != scope.source_repo {
        return Err(invalid("prepared view source_repo is not normalized"));
    }
    let expected_global_prefix = format!("{repo_prefix}/.crab");
    if scope.global_prefix != expected_global_prefix {
        return Err(invalid(
            "prepared view global_prefix does not match view repo_prefix",
        ));
    }
    validate_hash_component(&scope.scope_hash, "prepared view scope_hash")
}

fn source_ref_updates_for_refs(
    refs: &BTreeMap<String, String>,
    ref_updates: &[PushRefUpdate],
) -> Result<Vec<PushRefUpdate>> {
    let mut updates = Vec::with_capacity(ref_updates.len());
    for update in ref_updates {
        let current = refs.get(&update.ref_name);
        if let Some(current) = current {
            validate_sha1(current, "source ref oid")?;
        }
        let current = current.map(|oid| oid.to_ascii_lowercase());
        let new_oid = update.new_oid.to_ascii_lowercase();
        if current.as_deref() == Some(new_oid.as_str()) {
            return Err(invalid("protected push does not allow no-op ref updates"));
        }
        updates.push(PushRefUpdate {
            ref_name: update.ref_name.clone(),
            old_oid: current,
            new_oid,
        });
    }
    Ok(updates)
}

/// Returns the ref names from validated ref updates in order.
#[must_use]
pub fn ref_names(updates: &[PushRefUpdate]) -> Vec<&str> {
    updates
        .iter()
        .map(|update| update.ref_name.as_str())
        .collect()
}

/// Validates a single protected-push ref update.
pub fn validate_ref_update(update: &PushRefUpdate) -> Result<()> {
    validate_push_ref_update(update).map_err(|error| invalid(error.to_string()))
}

/// Validates a Git SHA-1 hex string.
pub fn validate_sha1(value: &str, field: &str) -> Result<()> {
    if value.len() != 40 || !value.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(invalid(format!("{field} must be 40 hex characters")));
    }
    Ok(())
}

/// Validates a 64-character content hash component.
pub fn validate_hash_component(value: &str, field: &str) -> Result<()> {
    if value.len() != 64 || !value.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(invalid(format!("{field} must be 64 hex characters")));
    }
    Ok(())
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| invalid("visibility Git path is not valid UTF-8"))
}

pub(crate) fn derive_peeled_refs(
    git_dir: &Path,
    refs: &[(String, String)],
) -> Result<BTreeMap<String, String>> {
    let mut peeled = BTreeMap::new();
    for (name, oid) in refs {
        let expression = format!("{oid}^{{}}");
        let output = Command::new("git")
            .args([
                "--git-dir",
                path_str(git_dir)?,
                "rev-parse",
                "--verify",
                &expression,
            ])
            .output()?;
        if !output.status.success() {
            return Err(invalid(format!(
                "failed to peel Git reference {name}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let target = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if target != *oid {
            peeled.insert(name.clone(), target);
        }
    }
    Ok(peeled)
}

fn xorb_hash_from_key(key: &str) -> Result<MerkleHash> {
    let hash = content_hash_from_path(key, "xorbs")
        .ok_or_else(|| invalid(format!("unsupported xorb canonical key: {key}")))?;
    merkle_hash_from_hex(hash, "xorb key hash")
}

fn merkle_hash_from_hex(value: &str, label: &str) -> Result<MerkleHash> {
    MerkleHash::from_hex(value).map_err(|e| invalid(format!("invalid {label}: {e}")))
}

/// Validates staged xorb bytes against shard-declared chunk metadata.
pub fn validate_staged_xorb(
    canonical_key: &str,
    bytes: &[u8],
    expected_chunks: &[ExpectedXorbChunk],
) -> Result<()> {
    let expected_hash = xorb_hash_from_key(canonical_key)?.hex();
    let parser = XorbParser::parse(bytes::Bytes::copy_from_slice(bytes)).map_err(|e| {
        AuthServerError::CorruptObject {
            path: canonical_key.to_owned(),
            reason: format!("invalid staged xorb: {e}"),
        }
    })?;
    let actual_hash = parser.hash().hex();
    if actual_hash != expected_hash {
        return Err(invalid(format!(
            "staged xorb hash does not match canonical key: {canonical_key}"
        )));
    }
    let actual_chunks = usize::try_from(parser.num_chunks())
        .map_err(|_| invalid("staged xorb chunk count overflows usize"))?;
    if actual_chunks != expected_chunks.len() {
        return Err(invalid(format!(
            "staged xorb chunk count does not match shard metadata: {canonical_key}"
        )));
    }
    for expected in expected_chunks {
        let meta =
            parser
                .chunk_meta(expected.index)
                .map_err(|e| AuthServerError::CorruptObject {
                    path: canonical_key.to_owned(),
                    reason: format!("staged xorb chunk metadata missing: {e}"),
                })?;
        if meta.hash != expected.hash || meta.uncompressed_len != expected.uncompressed_size {
            return Err(invalid(format!(
                "staged xorb chunk metadata does not match shard metadata: {canonical_key}"
            )));
        }
    }
    parser
        .verify_payload_digest()
        .map_err(|error| AuthServerError::CorruptObject {
            path: canonical_key.to_owned(),
            reason: format!("staged xorb payload digest is invalid: {error}"),
        })?;
    Ok(())
}

fn validate_canonical_key(repo_prefix: &str, key: &str) -> Result<()> {
    if has_unsafe_key_shape(key) {
        return Err(invalid(format!("unsafe canonical write key: {key}")));
    }
    if is_allowed_global_key(key) {
        return Ok(());
    }

    let repo_prefix = repo_prefix.trim_matches('/');
    let repo_scoped = format!("{repo_prefix}/");
    let Some(relative) = key.strip_prefix(&repo_scoped) else {
        return Err(invalid(format!(
            "canonical write escaped repo prefix: {key}"
        )));
    };

    if relative == "manifest"
        || relative.starts_with("refs/")
        || relative.starts_with("locks/")
        || relative.starts_with("staging/")
    {
        return Err(invalid(format!("forbidden canonical write key: {key}")));
    }
    if is_allowed_repo_key(relative) {
        return Ok(());
    }
    Err(invalid(format!("unsupported canonical write key: {key}")))
}

fn has_unsafe_key_shape(key: &str) -> bool {
    key.is_empty()
        || key.trim() != key
        || key.starts_with('/')
        || key.ends_with('/')
        || key.contains("//")
        || key.split('/').any(|part| part == "." || part == "..")
}

fn is_allowed_global_key(key: &str) -> bool {
    key.starts_with(".crab/")
        && (content_hash_from_path(key, "xorbs").is_some()
            || content_hash_from_path(key, "shards").is_some())
}

fn is_allowed_repo_key(relative: &str) -> bool {
    is_allowed_global_key(relative)
        || is_allowed_capsule_key(relative)
        || is_allowed_lfs_key(relative)
}

fn is_allowed_capsule_key(relative: &str) -> bool {
    let Some(rest) = relative.strip_prefix("v2/capsules/") else {
        return false;
    };
    let Some((partition, hash)) = rest.split_once('/') else {
        return false;
    };
    partition.len() == 2
        && hash.starts_with(partition)
        && validate_hash_component(hash, "capsule hash").is_ok()
}

fn is_allowed_lfs_key(relative: &str) -> bool {
    let Some(rest) = relative.strip_prefix("lfs/objects/") else {
        return false;
    };
    let mut parts = rest.split('/');
    let (Some(first), Some(second), Some(oid), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    first.len() == 2
        && second.len() == 2
        && oid.starts_with(first)
        && oid.get(2..4) == Some(second)
        && validate_hash_component(oid, "LFS object id").is_ok()
}

fn validate_key_content_hash(key: &str, actual_blake3: &str) -> Result<()> {
    if let Some(expected) = expected_blake3_from_key(key)
        && expected != actual_blake3
    {
        return Err(invalid(format!(
            "content-addressed staged object hash mismatch for {key}"
        )));
    }
    Ok(())
}

fn expected_blake3_from_key(key: &str) -> Option<&str> {
    let rest = key.split_once("/v2/capsules/")?.1;
    let (partition, hash) = rest.split_once('/')?;
    hash.starts_with(partition).then_some(hash)
}

fn invalid(message: impl Into<String>) -> AuthServerError {
    AuthServerError::invalid(message)
}

fn conflict(message: impl Into<String>) -> AuthServerError {
    AuthServerError::CasConflict {
        path: message.into(),
        expected_etag: None,
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;
    use bytes::Bytes;
    use crab_storage::StorageError;
    use object_store::memory::InMemory;
    use std::sync::Arc;

    fn oid(ch: char) -> String {
        std::iter::repeat_n(ch, 40).collect()
    }

    fn hash(ch: char) -> String {
        std::iter::repeat_n(ch, 64).collect()
    }

    fn blake3_hex(bytes: &[u8]) -> String {
        blake3::hash(bytes).to_hex().to_string()
    }

    fn ref_update(old_oid: Option<String>, new_oid: String) -> PushRefUpdate {
        PushRefUpdate {
            ref_name: "refs/heads/main".to_owned(),
            old_oid,
            new_oid,
        }
    }

    fn staged_object_for_bytes(canonical_key: String, bytes: &[u8]) -> StagedWrite {
        StagedWrite {
            staged_key: format!(
                "org/repo/staging/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/objects/{canonical_key}"
            ),
            canonical_key,
            blake3: blake3_hex(bytes),
            size: bytes.len() as u64,
        }
    }

    fn staged_object(canonical_key: String) -> StagedWrite {
        StagedWrite {
            staged_key: format!(
                "org/repo/staging/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/objects/{canonical_key}"
            ),
            canonical_key,
            blake3: hash('b'),
            size: 7,
        }
    }

    fn store() -> Store {
        Store::new(Arc::new(InMemory::new()))
    }

    async fn put_staged(store: &Store, object: &StagedWrite, bytes: Bytes) -> Result<()> {
        store
            .put_exact(&ObjectPath::from(object.staged_key.clone()), bytes)
            .await?;
        Ok(())
    }

    fn capsule_push_plan() -> ProtectedCapsulePushPlan {
        let run_hash = hash('a');
        ProtectedCapsulePushPlan {
            schema_version: crab_remote::protected::PROTECTED_CAPSULE_PUSH_PLAN_SCHEMA_VERSION,
            repo_prefix: "org/repo".to_owned(),
            push_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
            upload_prefix: "org/repo/staging/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/".to_owned(),
            base_root_digest: hash('c'),
            transaction_id: hash('d'),
            run_hash: run_hash.clone(),
            run_size: 7,
            ref_updates: vec![ref_update(Some(oid('1')), oid('2'))],
            staged_objects: vec![StagedWrite {
                canonical_key: format!("org/repo/v2/capsules/aa/{run_hash}"),
                staged_key: format!(
                    "org/repo/staging/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/objects/org/repo/v2/capsules/aa/{run_hash}"
                ),
                blake3: run_hash,
                size: 7,
            }],
        }
    }

    fn active_active_replication() -> ActiveActiveReplicationConfig {
        ActiveActiveReplicationConfig {
            mode: coordination_active_active::ActiveActiveMode::ActiveActive,
            coordinator: Some(coordination_active_active::ActiveActiveCoordinatorConfig {
                kind: coordination_active_active::ActiveActiveCoordinatorKind::Managed,
                url: "dynamodb://crab-coordinator".to_owned(),
                region: "us-east-1".to_owned(),
                failover_regions: vec!["us-west-2".to_owned()],
                consistency:
                    coordination_active_active::ActiveActiveCoordinatorConsistency::Linearizable,
            }),
            writers: vec![coordination_active_active::ActiveActiveWriterConfig {
                name: "east".to_owned(),
                url: "crab://bucket/org/repo".to_owned(),
                region: "us-east-1".to_owned(),
                enabled: true,
            }],
        }
    }

    #[test]
    fn active_active_receive_config_requires_matching_writer() {
        let config = ActiveActiveReceiveConfig {
            replication: active_active_replication(),
            writer: "east".to_owned(),
        };
        let json = serde_json::to_string(&config).expect("active-active config should serialize");

        assert!(
            parse_active_active_receive_config(Some(&json), "crab://bucket/org/repo")
                .expect("active-active config should parse")
                .is_some()
        );

        let wrong_writer = ActiveActiveReceiveConfig {
            replication: active_active_replication(),
            writer: "west".to_owned(),
        };
        let wrong_writer_json =
            serde_json::to_string(&wrong_writer).expect("active-active config should serialize");

        let wrong_repo =
            parse_active_active_receive_config(Some(&wrong_writer_json), "crab://bucket/org/repo")
                .expect_err("wrong writer should be rejected");
        assert!(
            wrong_repo
                .to_string()
                .contains("does not match repo_url writer")
        );
    }

    #[test]
    fn active_active_registration_uses_validated_coordinator_url_scheme() {
        let registration = active_active_coordinator_registration(&active_active_replication())
            .expect("valid active-active config should build registration");

        assert_eq!(registration.provider, "dynamodb");
        assert_eq!(registration.url, "dynamodb://crab-coordinator");
        assert_eq!(registration.region, "us-east-1");
        assert_eq!(registration.failover_regions, vec!["us-west-2"]);
    }

    #[test]
    fn push_id_accepts_only_lowercase_hex_session_tokens() {
        assert!(validate_push_id("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").is_ok());

        for push_id in [
            "push-123",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "../aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(
                validate_push_id(push_id).is_err(),
                "expected {push_id} to be rejected"
            );
        }
    }

    #[test]
    fn receive_provider_accepts_cloud_aliases_only() {
        assert_eq!(
            receive_provider("s3").expect("valid s3 alias"),
            StorageProviderKind::S3
        );
        assert_eq!(
            receive_provider("gcs").expect("valid gcs alias"),
            StorageProviderKind::Gcs
        );
        assert_eq!(
            receive_provider("azure").expect("valid azure alias"),
            StorageProviderKind::Azure
        );

        assert!(receive_provider("local").is_err());
        assert!(receive_provider("auto").is_err());
    }

    #[test]
    fn push_plan_shape_rejects_excessive_ref_updates() {
        let mut plan = capsule_push_plan();
        plan.ref_updates = (0..=MAX_PUSH_REF_UPDATES)
            .map(|idx| PushRefUpdate {
                ref_name: format!("refs/heads/branch-{idx}"),
                old_oid: Some(oid('1')),
                new_oid: oid('2'),
            })
            .collect();

        let err = validate_protected_capsule_plan_shape(
            &plan,
            "org/repo",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )
        .expect_err("oversized ref update list must be rejected");

        assert!(
            err.to_string().contains("too many ref updates"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn capsule_push_plan_shape_binds_staged_capsule() {
        validate_protected_capsule_plan_shape(
            &capsule_push_plan(),
            "org/repo",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )
        .expect("valid capsule plan");
    }

    #[test]
    fn capsule_push_plan_shape_rejects_mismatched_staged_capsule() {
        let mut plan = capsule_push_plan();
        plan.staged_objects[0].size += 1;

        let error = validate_protected_capsule_plan_shape(
            &plan,
            "org/repo",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )
        .expect_err("capsule metadata mismatch must be rejected");

        assert!(error.to_string().contains("staged capsule metadata"));
    }

    #[test]
    fn push_plan_shape_rejects_excessive_staged_object_list() {
        let mut plan = capsule_push_plan();
        plan.staged_objects = (0..=MAX_PUSH_STAGED_OBJECTS)
            .map(|idx| staged_object(format!("org/repo/packs/pack-{}-{idx}.pack", hash('a'))))
            .collect();

        let err = validate_protected_capsule_plan_shape(
            &plan,
            "org/repo",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )
        .expect_err("oversized staged object list must be rejected");

        assert!(
            err.to_string().contains("too many staged objects"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn push_plan_rejects_unknown_top_level_fields() {
        let mut value =
            serde_json::to_value(capsule_push_plan()).expect("push plan should serialize");
        value
            .as_object_mut()
            .expect("push plan should serialize as object")
            .insert("legacy_mode".to_owned(), serde_json::json!(true));

        let err = serde_json::from_value::<ProtectedCapsulePushPlan>(value)
            .expect_err("unknown push-plan fields must be rejected");

        assert!(
            err.to_string().contains("unknown field"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn push_plan_rejects_unknown_ref_update_fields() {
        let mut value =
            serde_json::to_value(capsule_push_plan()).expect("push plan should serialize");
        value["ref_updates"][0]
            .as_object_mut()
            .expect("ref update should serialize as object")
            .insert("force".to_owned(), serde_json::json!(true));

        let err = serde_json::from_value::<ProtectedCapsulePushPlan>(value)
            .expect_err("unknown ref update fields must be rejected");

        assert!(
            err.to_string().contains("unknown field"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn push_plan_rejects_unknown_staged_object_fields() {
        let mut value =
            serde_json::to_value(capsule_push_plan()).expect("push plan should serialize");
        value["staged_objects"][0]
            .as_object_mut()
            .expect("staged object should serialize as object")
            .insert("canonical_write".to_owned(), serde_json::json!(true));

        let err = serde_json::from_value::<ProtectedCapsulePushPlan>(value)
            .expect_err("unknown staged object fields must be rejected");

        assert!(
            err.to_string().contains("unknown field"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn staged_object_shapes_reject_duplicate_canonical_keys() {
        let mut plan = capsule_push_plan();
        let duplicate = plan
            .staged_objects
            .first()
            .expect("push plan should have a staged object")
            .clone();
        plan.staged_objects.push(duplicate);

        let err = validate_staged_write_shapes(
            &plan.staged_objects,
            "org/repo",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )
        .expect_err("duplicate canonical key must be rejected");

        assert!(
            err.to_string().contains("duplicate canonical"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn staged_object_shapes_reject_wrong_staging_prefix() {
        let mut plan = capsule_push_plan();
        plan.staged_objects[0].staged_key =
            "org/repo/staging/other/objects/org/repo/v2/capsules/aa/bad".to_owned();

        let err = validate_staged_write_shapes(
            &plan.staged_objects,
            "org/repo",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )
        .expect_err("staged object path must match push id");

        assert!(
            err.to_string().contains("path does not match"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn canonical_key_allowlist_accepts_push_immutable_objects() {
        let repo = "org/repo";
        for key in [
            format!(".crab/xorbs/aa/{}", hash('a')),
            format!(".crab/shards/bb/{}", hash('b')),
            format!("{repo}/.crab/xorbs/aa/{}", hash('a')),
            format!("{repo}/.crab/shards/bb/{}", hash('b')),
            format!("{repo}/v2/capsules/aa/{}", hash('a')),
        ] {
            assert!(
                validate_canonical_key(repo, &key).is_ok(),
                "expected {key} to be allowed"
            );
        }
    }

    #[test]
    fn canonical_key_allowlist_rejects_mutable_and_escaped_objects() {
        let repo = "org/repo";
        for key in [
            "other/repo/packs/pack-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.pack".to_owned(),
            format!("{repo}/packs/pack-{}.pack", hash('c')),
            format!("{repo}/packs/pack-{}.meta", hash('c')),
            format!("{repo}/packs/pack-{}.kinds", hash('c')),
            format!("{repo}/metadata/pack/segments/{}.jsonl", hash('d')),
            format!("{repo}/metadata/pack/indexes/{}.json", hash('e')),
            format!("{repo}/metadata/shard/segments/{}.jsonl", hash('f')),
            format!("{repo}/metadata/shard/indexes/{}.json", hash('1')),
            format!("{repo}/manifest"),
            format!("{repo}/refs/heads/main"),
            format!("{repo}/locks/main"),
            format!("{repo}/staging/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/object"),
            format!("{repo}/.crab/ref-registry"),
            format!("{repo}/.crab/chunk_index_db/{}", hash('a')),
            format!("evil/.crab/xorbs/aa/{}", hash('a')),
            ".crab/ref-registry".to_owned(),
            format!(".crab/chunk_index_db/{}", hash('a')),
            format!("{repo}/packs/pack-{}.idx", hash('a')),
            format!("{repo}/metadata/pack/indexes/not-a-hash.json"),
            format!("{repo}/manifests/shard-list"),
            format!("{repo}/manifests/shard-list-{}", hash('a')),
            format!("{repo}/manifests/pack-list-{}", hash('b')),
            format!("{repo}/packs/../manifest"),
        ] {
            assert!(
                validate_canonical_key(repo, &key).is_err(),
                "expected {key} to be rejected"
            );
        }
    }

    #[test]
    fn staged_object_bytes_bind_content_addressed_keys() -> Result<()> {
        let bytes = b"pack bytes";
        let actual = blake3_hex(bytes);
        let object = staged_object_for_bytes(
            format!("org/repo/v2/capsules/{}/{actual}", &actual[..2]),
            bytes,
        );

        validate_staged_object_bytes(&object, bytes)
    }

    #[test]
    fn staged_object_bytes_reject_content_addressed_mismatch() {
        let bytes = b"pack bytes";
        let object =
            staged_object_for_bytes(format!("org/repo/v2/capsules/aa/{}", hash('a')), bytes);

        let err = validate_staged_object_bytes(&object, bytes)
            .expect_err("pack key must match staged content hash");

        assert!(
            err.to_string()
                .contains("content-addressed staged object hash mismatch"),
            "unexpected error: {err}"
        );
    }

    fn test_xorb_reference() -> Result<(String, Bytes, Vec<ExpectedXorbChunk>, Vec<u8>)> {
        use crab_xet::shard::{
            MDBXorbInfo, ShardWriter, XorbChunkSequenceEntry, XorbChunkSequenceHeader,
        };
        use crab_xet::xorb::builder::{RunId, XorbBuilder};
        use crab_xet::xorb::format::Chunk;

        let chunk = Chunk::new(Bytes::from_static(b"enterprise xorb payload"));
        let mut builder = XorbBuilder::new();
        builder.push(&chunk, RunId(0))?;
        let xorb = builder
            .finalize()?
            .into_iter()
            .next()
            .ok_or_else(|| invalid("test xorb was not finalized"))?;

        let mut byte_offset = 0u32;
        let mut entries = Vec::with_capacity(xorb.placements.len());
        for placement in &xorb.placements {
            entries.push(XorbChunkSequenceEntry::new(
                placement.chunk_hash,
                placement.uncompressed_size,
                byte_offset,
            ));
            byte_offset = byte_offset
                .checked_add(placement.uncompressed_size)
                .ok_or_else(|| invalid("test xorb size overflow"))?;
        }

        let mut writer = ShardWriter::new();
        writer.add_xorb(Arc::new(MDBXorbInfo {
            metadata: XorbChunkSequenceHeader::new(xorb.hash, entries.len(), byte_offset),
            chunks: entries,
        }))?;
        let (shard_bytes, _) = writer.finalize()?;

        let key = canonical_global_content_path("xorbs", &xorb.hash.hex()).to_string();
        let refs = strict_xorb_references_from_shard(&shard_bytes)?;
        let expected_chunks = refs
            .get(&key)
            .cloned()
            .ok_or_else(|| invalid("test shard did not reference test xorb"))?;

        Ok((key, xorb.bytes, expected_chunks, shard_bytes))
    }

    #[test]
    fn strict_shard_parse_reports_referenced_xorbs() -> Result<()> {
        let (key, xorb_bytes, expected_chunks, shard_bytes) = test_xorb_reference()?;
        let refs = strict_xorb_references_from_shard(&shard_bytes)?;

        assert_eq!(
            refs,
            BTreeMap::from([(key.clone(), expected_chunks.clone())])
        );
        validate_staged_xorb(&key, &xorb_bytes, &expected_chunks)?;
        validate_staged_xorb(&format!("org/repo/{key}"), &xorb_bytes, &expected_chunks)?;
        assert!(strict_xorb_references_from_shard(b"not a shard").is_err());
        Ok(())
    }

    #[test]
    fn staged_xorb_validation_rejects_malformed_or_mismatched_bytes() -> Result<()> {
        let (key, xorb_bytes, mut expected_chunks, _) = test_xorb_reference()?;

        let malformed = validate_staged_xorb(&key, b"not a xorb", &expected_chunks)
            .expect_err("malformed staged xorb bytes must be rejected");
        assert!(
            malformed.to_string().contains("invalid staged xorb"),
            "unexpected error: {malformed}"
        );

        let first = expected_chunks
            .first_mut()
            .ok_or_else(|| invalid("test xorb had no chunks"))?;
        first.uncompressed_size = first.uncompressed_size.saturating_add(1);
        let mismatch = validate_staged_xorb(&key, &xorb_bytes, &expected_chunks)
            .expect_err("xorb metadata must match shard-declared chunk metadata");
        assert!(
            mismatch
                .to_string()
                .contains("chunk metadata does not match"),
            "unexpected error: {mismatch}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn promote_staged_objects_revalidates_staged_bytes_before_canonical_write() -> Result<()>
    {
        let store = store();
        let original = Bytes::from_static(b"pack");
        let pack_id = blake3_hex(&original);
        let canonical_key = format!("org/repo/v2/capsules/{}/{pack_id}", &pack_id[..2]);
        let object = staged_object_for_bytes(canonical_key.clone(), &original);
        let mut plan = capsule_push_plan();
        plan.staged_objects = vec![object.clone()];

        put_staged(&store, &object, original).await?;
        store
            .delete(&ObjectPath::from(object.staged_key.clone()))
            .await?;
        store
            .put(
                &ObjectPath::from(object.staged_key.clone()),
                Bytes::from_static(b"bad!"),
            )
            .await?;

        let err = promote_staged_writes(&store, &plan.staged_objects)
            .await
            .expect_err("tampered staged object must not be promoted");
        assert!(
            err.to_string().contains("staged object hash mismatch"),
            "unexpected error: {err}"
        );
        let canonical_err = store
            .get_with_etag(&ObjectPath::from(canonical_key))
            .await
            .expect_err("canonical object must not be written after promotion failure");
        assert!(matches!(canonical_err, StorageError::NotFound { .. }));
        Ok(())
    }

    #[tokio::test]
    async fn read_verified_staged_object_rejects_an_oversized_provider_body() -> Result<()> {
        let store = store();
        let object = staged_object_for_bytes(
            "org/repo/v2/capsules/aa/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_owned(),
            b"pack",
        );
        put_staged(&store, &object, Bytes::from_static(b"oversized")).await?;

        let error = read_verified_staged_object(&store, &object)
            .await
            .expect_err("staged reads must stop at their declared size");
        assert!(matches!(error, AuthServerError::CorruptObject { .. }));
        assert!(error.to_string().contains("bounded read"));
        Ok(())
    }

    #[tokio::test]
    async fn promote_staged_objects_does_not_overwrite_existing_canonical_content() -> Result<()> {
        let store = store();
        let original = Bytes::from_static(b"pack");
        let pack_id = blake3_hex(&original);
        let canonical_key = format!("org/repo/v2/capsules/{}/{pack_id}", &pack_id[..2]);
        let canonical_path = ObjectPath::from(canonical_key.clone());
        let object = staged_object_for_bytes(canonical_key, &original);
        let mut plan = capsule_push_plan();
        plan.staged_objects = vec![object.clone()];

        let existing = Bytes::from_static(b"different-pack");
        store.put(&canonical_path, existing.clone()).await?;
        put_staged(&store, &object, original).await?;

        let err = promote_staged_writes(&store, &plan.staged_objects)
            .await
            .expect_err("promotion must not overwrite immutable canonical content");
        assert!(
            matches!(err, AuthServerError::CasConflict { .. }),
            "expected CasConflict, got {err:?}"
        );
        let (got, _etag) = store.get_with_etag(&canonical_path).await?;
        assert_eq!(got, existing);
        Ok(())
    }

    #[test]
    fn prepared_view_scope_requires_normalized_source_repo() {
        let scope = PreparedViewScope {
            repo_prefix: "org/repo/acl-views/v2/aaaaaaaa/1-deadbeef".to_owned(),
            global_prefix: "org/repo/acl-views/v2/aaaaaaaa/1-deadbeef/.crab".to_owned(),
            source_repo: "org/repo/".to_owned(),
            scope_hash: hash('a'),
        };

        let err = validate_prepared_view_scope(&scope, "org/repo")
            .expect_err("source repo with trailing slash should be rejected");

        assert!(err.to_string().contains("source_repo is not normalized"));
    }

    #[test]
    fn prepared_view_scope_requires_view_global_prefix() {
        let scope = PreparedViewScope {
            repo_prefix: "org/repo/acl-views/v2/aaaaaaaa/1-deadbeef".to_owned(),
            global_prefix: "org/repo/.crab".to_owned(),
            source_repo: "org/repo".to_owned(),
            scope_hash: hash('a'),
        };

        let err = validate_prepared_view_scope(&scope, "org/repo")
            .expect_err("view global prefix must be scoped to the prepared view");

        assert!(err.to_string().contains("global_prefix"));
    }

    #[test]
    fn ref_update_requires_valid_full_branch_ref() {
        let valid = PushRefUpdate {
            ref_name: "refs/heads/feature".to_owned(),
            old_oid: Some(oid('1')),
            new_oid: oid('2'),
        };

        assert!(validate_ref_update(&valid).is_ok());

        for ref_name in [
            "heads/feature",
            "refs/tags/v1.0",
            "refs/notes/review",
            "refs/pull/1/head",
            "refs/heads/",
            "refs/heads/.hidden",
            "refs/heads/main.lock",
            "refs/heads/main/",
            "refs/heads/main@{1}",
            "refs/heads/main~1",
            "refs/heads/main:other",
            "refs//heads/main",
        ] {
            let update = PushRefUpdate {
                ref_name: ref_name.to_owned(),
                old_oid: Some(oid('1')),
                new_oid: oid('2'),
            };
            assert!(
                validate_ref_update(&update).is_err(),
                "expected {ref_name} to be rejected"
            );
        }
    }

    #[test]
    fn ref_update_rejects_noop_ref_mutation() {
        let update = PushRefUpdate {
            ref_name: "refs/heads/main".to_owned(),
            old_oid: Some(oid('2')),
            new_oid: oid('2'),
        };

        let err = validate_ref_update(&update).expect_err("no-op ref update must be rejected");
        assert!(
            err.to_string().contains("no-op ref updates"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn ref_update_rejects_case_insensitive_noop_ref_mutation() {
        let update = PushRefUpdate {
            ref_name: "refs/heads/main".to_owned(),
            old_oid: Some(oid('a').to_ascii_uppercase()),
            new_oid: oid('a'),
        };

        let err = validate_ref_update(&update).expect_err("no-op ref update must be rejected");
        assert!(
            err.to_string().contains("no-op ref updates"),
            "unexpected error: {err}"
        );
    }
}
