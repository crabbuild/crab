//! Protected-push receive validation helpers.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Cursor;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use crab_auth::{
    PushRefUpdate, normalize_optional_oid, validate_push_ref_update, validate_push_ref_updates,
};
use crab_coordination::active_active::{
    self as coordination_active_active, ActiveActiveReplicationConfig,
};
use crab_git::normalize_repository_prefix;
use crab_metadata::{
    manifests::{
        Manifest, PackManifestEntry, parse_pack_segment_entries, validate_manifest_payload,
    },
    pack_metadata::{
        parse_pack_metadata, serialize_pack_metadata_bounded, validate_pack_metadata_for_entry,
    },
    receipts::{
        CommittedChunkReceipt, GenerationIndexReceipt, OriginReceipt, PushCommitReceipt,
        RECEIPT_SCHEMA_VERSION, generation_file_index_digest, generation_git_object_locator_digest,
    },
    ref_registry::ActiveActiveCoordinatorRegistration,
    remote_index::{RemoteIndexConfig, RemoteIndexWriter},
    segmented::{self, SegmentIndex, SegmentKind},
    segmented_store,
    value_codec::CommittedFileRecord,
};
use crab_staging::shard_replay::{REPLAY_BATCH_ENTRIES, ShardReplaySpool};
use crab_storage::{
    StagedWrite, StorageError, StorageProviderKind, Store, StoreLayout,
    canonical_global_content_path, content_hash_from_path,
};
use crab_types::time::now_rfc3339_millis;
use crab_xet::hash::{MerkleHash, compute_data_hash};
use crab_xet::shard::{MDBShardInfo, ShardReader};
use crab_xet::xorb::format::FOOTER_SIZE;
use crab_xet::xorb::parser::{XorbParser, xorb_payload_digest_from_footer};
use object_store::path::Path as ObjectPath;
use serde::{Deserialize, Serialize};

use crate::error::{AuthServerError, Result};

mod finalize;
mod git_workspace;
mod session;
mod workflow;

pub use finalize::{ReceiveManifestCommit, commit_receive_manifest};
pub use session::{BaseState, ReceiveContext};
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

/// Protected-push plan uploaded by the client before receive verification.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ProtectedPushPlan {
    pub schema_version: u32,
    pub repo_prefix: String,
    pub push_id: String,
    pub upload_prefix: String,
    pub base_manifest_generation: Option<u64>,
    pub base_manifest_etag: Option<String>,
    pub ref_updates: Vec<PushRefUpdate>,
    pub candidate_manifest: Manifest,
    pub push_commit_receipt: Option<PushCommitReceipt>,
    pub staged_objects: Vec<StagedWrite>,
}

/// Source-repository materialization produced from a protected view push.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MaterializedSourcePush {
    pub ref_updates: Vec<PushRefUpdate>,
    pub packs: Vec<PackManifestEntry>,
    pub peeled_refs: BTreeMap<String, String>,
    pub(crate) git_visibility: MaterializedGitVisibility,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum MaterializedGitVisibility {
    Exact(BTreeMap<String, Vec<String>>),
    CompletePackOnly { observed: usize, maximum: usize },
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
    pub view_ref_updates: Vec<PushRefUpdate>,
    pub source_ref_updates: Vec<PushRefUpdate>,
    pub view_scope: Option<PreparedViewScope>,
}

pub fn validate_protected_dependency_receipt(plan: &ProtectedPushPlan) -> Result<()> {
    let receipt = plan.push_commit_receipt.as_ref().ok_or_else(|| {
        invalid("protected push plan is missing its base-bound dependency receipt")
    })?;
    let base_generation = plan.base_manifest_generation.unwrap_or(0);
    receipt
        .validate_base(base_generation, plan.base_manifest_etag.as_deref())
        .map_err(AuthServerError::from)?;
    let shard_index_hash = if plan.candidate_manifest.shard_index_hash.is_empty() {
        [0; 32]
    } else {
        merkle_hash_from_hex(
            &plan.candidate_manifest.shard_index_hash,
            "protected candidate shard-index hash",
        )?
        .into()
    };
    let pack_index_hash = if plan.candidate_manifest.pack_index_hash.is_empty() {
        [0; 32]
    } else {
        merkle_hash_from_hex(
            &plan.candidate_manifest.pack_index_hash,
            "protected candidate pack-index hash",
        )?
        .into()
    };
    let protected_updates = plan
        .ref_updates
        .iter()
        .map(|update| {
            (
                update.ref_name.clone(),
                update.old_oid.clone(),
                update.new_oid.clone(),
            )
        })
        .collect::<Vec<_>>();
    let ref_edit_digest = crab_metadata::receipts::protected_ref_edit_digest(&protected_updates);
    let connectivity_digest = crab_metadata::receipts::protected_connectivity_digest(
        &plan
            .ref_updates
            .iter()
            .map(|update| update.new_oid.clone())
            .collect::<Vec<_>>(),
    );
    if receipt.attempt_id != plan.push_id
        || receipt.candidate_shard_index_hash != shard_index_hash
        || receipt.candidate_pack_index_hash != pack_index_hash
        || receipt.ref_edit_digest != ref_edit_digest
        || receipt.connectivity_digest != connectivity_digest
        || receipt.gc_registry_generation != 0
    {
        return Err(invalid(
            "protected push dependency receipt does not bind the plan base and candidate indexes",
        ));
    }
    Ok(())
}

pub async fn validate_protected_shard_set_receipt(
    store: &Store,
    router: &StoreLayout<Store>,
    plan: &ProtectedPushPlan,
) -> Result<()> {
    let receipt = plan.push_commit_receipt.as_ref().ok_or_else(|| {
        invalid("protected push plan is missing its base-bound dependency receipt")
    })?;
    let shards = if plan.candidate_manifest.shard_index_hash.is_empty() {
        Vec::new()
    } else {
        crab_metadata::manifest_store::read_bulk_shard_list(
            store,
            router,
            &plan.candidate_manifest.shard_index_hash,
        )
        .await?
    };
    let digest = crab_metadata::receipts::committed_shard_set_digest(&shards);
    if receipt.shard_set_digest != digest {
        return Err(invalid(
            "protected push dependency receipt does not bind the candidate shard set",
        ));
    }
    Ok(())
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

/// Validates the protected-push plan shape before expensive object checks.
pub fn validate_push_plan_shape(
    plan: &ProtectedPushPlan,
    repo_prefix: &str,
    push_id: &str,
) -> Result<()> {
    if plan.schema_version != 1 {
        return Err(invalid("unsupported push-plan schema_version"));
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
    if plan.ref_updates.is_empty() {
        return Err(invalid("protected push requires at least one ref update"));
    }
    if plan.ref_updates.len() > MAX_PUSH_REF_UPDATES {
        return Err(invalid("push-plan contains too many ref updates"));
    }
    if plan.staged_objects.len() > MAX_PUSH_STAGED_OBJECTS {
        return Err(invalid("push-plan contains too many staged objects"));
    }
    validate_push_ref_updates(&plan.ref_updates).map_err(|error| invalid(error.to_string()))
}

/// Validates the candidate manifest delta described by a protected-push plan.
pub fn validate_candidate_manifest_shape(
    plan: &ProtectedPushPlan,
    repo_prefix: &str,
) -> Result<()> {
    let candidate = &plan.candidate_manifest;
    validate_manifest_payload(candidate)?;
    if candidate.generation == 0 {
        return Err(invalid("candidate manifest generation must be non-zero"));
    }

    let mut expected_refs = BTreeMap::new();
    for update in &plan.ref_updates {
        expected_refs.insert(update.ref_name.clone(), update.new_oid.clone());
    }
    if candidate.refs != expected_refs {
        return Err(invalid(
            "candidate manifest refs differ from staged ref updates",
        ));
    }

    if candidate.pusher.is_some()
        || candidate.commit_graph_hash.is_some()
        || candidate.ref_registry_hash.is_some()
    {
        return Err(invalid(
            "candidate delta manifest contains unsupported metadata",
        ));
    }

    if !candidate.peeled_refs.is_empty() {
        return Err(invalid(
            "candidate delta manifest cannot contain peeled refs",
        ));
    }

    let staged_keys: BTreeSet<&str> = plan
        .staged_objects
        .iter()
        .map(|object| object.canonical_key.as_str())
        .collect();
    validate_candidate_index_hash(
        repo_prefix,
        "shard",
        &candidate.shard_index_hash,
        &staged_keys,
    )?;
    validate_candidate_index_hash(
        repo_prefix,
        "pack",
        &candidate.pack_index_hash,
        &staged_keys,
    )
}

/// Validates staged-object paths and duplicate protection for a protected-push plan.
pub fn validate_staged_object_shapes(
    plan: &ProtectedPushPlan,
    repo_prefix: &str,
    push_id: &str,
) -> Result<()> {
    let mut canonical_keys = BTreeSet::new();
    let mut staged_keys = BTreeSet::new();
    for object in &plan.staged_objects {
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
    let maximum = if is_pack_metadata_key(&object.canonical_key) {
        object
            .size
            .min(crab_metadata::pack_metadata::MAX_PACK_METADATA_BYTES)
    } else {
        object.size
    };
    let (bytes, _) = store.get_with_etag_bounded(&path, maximum).await?;
    validate_staged_object_bytes(object, &bytes)?;
    Ok(bytes)
}

fn is_pack_metadata_key(key: &str) -> bool {
    key.rsplit('/')
        .next()
        .is_some_and(|name| name.starts_with("pack-") && name.ends_with(".meta"))
        && key.contains("/packs/")
}

async fn existing_pack_metadata_is_oversized(store: &Store, path: &ObjectPath) -> bool {
    match store.head(path).await {
        Ok(meta) => meta.size > crab_metadata::pack_metadata::MAX_PACK_METADATA_BYTES,
        Err(error) => {
            tracing::debug!(
                path = %path,
                error = %error,
                "could not confirm oversized pack metadata during receive fallback"
            );
            false
        }
    }
}

async fn promote_pack_metadata_union(
    store: &Store,
    canonical: &ObjectPath,
    staged_bytes: bytes::Bytes,
) -> Result<()> {
    let staged = parse_pack_metadata(&staged_bytes, canonical.as_ref())?;
    let mut requested = staged.ref_tips.iter().cloned().collect::<BTreeSet<_>>();
    for _ in 0..64 {
        match store
            .get_with_etag_bounded(
                canonical,
                crab_metadata::pack_metadata::MAX_PACK_METADATA_BYTES,
            )
            .await
        {
            Ok((body, etag)) => {
                let mut existing = parse_pack_metadata(&body, canonical.as_ref())?;
                if existing.pack_id != staged.pack_id
                    || existing.object_count != staged.object_count
                {
                    return Err(invalid(
                        "canonical pack metadata conflicts with staged pack identity",
                    ));
                }
                let before = existing.ref_tips.iter().cloned().collect::<BTreeSet<_>>();
                if before.is_empty() && !staged.ref_tips.is_empty() {
                    // An empty hint may be a deliberate legacy fallback after
                    // a previous union exceeded the bound. It cannot be
                    // enriched without proving the complete ref-tip set again.
                    return Ok(());
                }
                requested.extend(before.iter().cloned());
                if requested == before {
                    return Ok(());
                }
                existing.ref_tips = requested.iter().cloned().collect();
                let Some(body) = serialize_pack_metadata_bounded(&existing)? else {
                    tracing::warn!(
                        path = %canonical,
                        maximum_bytes = crab_metadata::pack_metadata::MAX_PACK_METADATA_BYTES,
                        "pack metadata ref-tip union exceeded its bound; retaining legacy hint"
                    );
                    return Ok(());
                };
                match store
                    .update(canonical, bytes::Bytes::from(body), etag)
                    .await
                {
                    Ok(_) => return Ok(()),
                    Err(StorageError::StateConflict { .. }) => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            Err(StorageError::NotFound { .. }) => {
                let mut metadata = staged.clone();
                metadata.ref_tips = requested.iter().cloned().collect();
                let body = serialize_pack_metadata_bounded(&metadata)?
                    .ok_or_else(|| invalid("pack metadata hint exceeded its size bound"))?;
                match store
                    .create_strict(canonical, bytes::Bytes::from(body))
                    .await
                {
                    Ok(()) => return Ok(()),
                    Err(StorageError::StateConflict { .. }) => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => {
                if matches!(&error, StorageError::CorruptObject { .. })
                    && existing_pack_metadata_is_oversized(store, canonical).await
                {
                    tracing::warn!(
                        path = %canonical,
                        maximum_bytes = crab_metadata::pack_metadata::MAX_PACK_METADATA_BYTES,
                        "existing oversized pack metadata is treated as a legacy hint"
                    );
                    return Ok(());
                }
                return Err(error.into());
            }
        }
    }
    Err(AuthServerError::CasConflict {
        path: canonical.to_string(),
        expected_etag: None,
    })
}

/// Promotes verified staged objects to their immutable canonical locations.
///
/// Callers must first run [`validate_staged_object_shapes`]. Staged bytes are
/// re-read and revalidated immediately before the canonical write.
pub async fn promote_staged_objects(store: &Store, plan: &ProtectedPushPlan) -> Result<()> {
    for object in &plan.staged_objects {
        let canonical = ObjectPath::from(object.canonical_key.clone());
        let bytes = read_verified_staged_object(store, object).await?;
        if is_pack_metadata_key(&object.canonical_key) {
            promote_pack_metadata_union(store, &canonical, bytes).await?;
        } else {
            let shard_body = content_hash_from_path(&object.canonical_key, "shards")
                .is_some()
                .then(|| bytes.clone());
            store.put(&canonical, bytes).await?;
            if let Some(shard_body) = shard_body {
                publish_protected_shard_closure(store, &object.canonical_key, &shard_body).await?;
            }
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

/// Validates candidate segmented metadata and every staged object reference.
pub async fn validate_candidate_metadata(
    store: &Store,
    router: &StoreLayout<Store>,
    plan: &ProtectedPushPlan,
) -> Result<()> {
    let mut referenced_keys = BTreeSet::new();
    validate_segmented_index(
        store,
        router,
        SegmentKind::Shard,
        &plan.candidate_manifest.shard_index_hash,
        plan,
        &mut referenced_keys,
    )
    .await?;
    validate_segmented_index(
        store,
        router,
        SegmentKind::Pack,
        &plan.candidate_manifest.pack_index_hash,
        plan,
        &mut referenced_keys,
    )
    .await?;
    validate_staged_objects_are_referenced(plan, &referenced_keys)
}

/// Reads a staged segmented metadata index from the plan.
pub async fn read_staged_segment_index(
    store: &Store,
    router: &StoreLayout<Store>,
    kind: SegmentKind,
    hash: &str,
    plan: &ProtectedPushPlan,
) -> Result<SegmentIndex> {
    let index_path = router.repo_path(&segmented::index_relative_path(kind, hash));
    let canonical_key = index_path.as_ref().to_owned();
    let bytes = read_staged_object_bytes(store, plan, &canonical_key).await?;
    Ok(segmented::parse_segment_index(&bytes, &canonical_key)?)
}

/// Reads required staged object bytes for a canonical key in the plan.
pub async fn read_staged_object_bytes(
    store: &Store,
    plan: &ProtectedPushPlan,
    canonical_key: &str,
) -> Result<Vec<u8>> {
    read_optional_staged_object_bytes(store, plan, canonical_key)
        .await?
        .ok_or_else(|| invalid(format!("required staged object missing: {canonical_key}")))
}

/// Reads optional staged object bytes for a canonical key in the plan.
pub async fn read_optional_staged_object_bytes(
    store: &Store,
    plan: &ProtectedPushPlan,
    canonical_key: &str,
) -> Result<Option<Vec<u8>>> {
    let Some(object) = plan
        .staged_objects
        .iter()
        .find(|object| object.canonical_key == canonical_key)
    else {
        return Ok(None);
    };
    let bytes = read_verified_staged_object(store, object).await?;
    Ok(Some(bytes.to_vec()))
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

/// Removes the optional canonical v1 bloom trailer before xet MDB parsing.
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

/// Builds a prepared protected-push session record from the current source manifest.
pub fn build_prepare_record(
    repo_prefix: &str,
    push_id: &str,
    base: (&Manifest, &str),
    view_ref_updates: Vec<PushRefUpdate>,
    view_scope: Option<PreparedViewScope>,
) -> Result<PushPrepareRecord> {
    validate_prepared_ref_updates(&view_ref_updates)?;
    if let Some(scope) = view_scope.as_ref() {
        validate_prepared_view_scope(scope, repo_prefix)?;
    }
    let source_ref_updates = source_ref_updates_for(base.0, &view_ref_updates)?;
    Ok(PushPrepareRecord {
        schema_version: 1,
        repo_prefix: repo_prefix.to_owned(),
        push_id: push_id.to_owned(),
        source_manifest_generation: base.0.generation,
        source_manifest_etag: base.1.to_owned(),
        view_ref_updates,
        source_ref_updates,
        view_scope,
    })
}

/// Validates a prepared protected-push session record loaded from storage.
pub fn validate_prepare_record_shape(
    record: &PushPrepareRecord,
    repo_prefix: &str,
    push_id: &str,
) -> Result<()> {
    if record.schema_version != 1 {
        return Err(invalid("unsupported prepare record schema_version"));
    }
    if record.repo_prefix != repo_prefix {
        return Err(invalid(
            "prepare record repo_prefix does not match repo_url",
        ));
    }
    if record.push_id != push_id {
        return Err(invalid("prepare record push_id does not match request"));
    }
    if record.source_manifest_etag.trim().is_empty() {
        return Err(invalid("prepare record source_manifest_etag is empty"));
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

/// Maps authorized view ref updates onto source-repository ref updates.
pub fn source_ref_updates_for(
    base: &Manifest,
    ref_updates: &[PushRefUpdate],
) -> Result<Vec<PushRefUpdate>> {
    let mut updates = Vec::with_capacity(ref_updates.len());
    for update in ref_updates {
        let current = base.refs.get(&update.ref_name);
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

/// Replays a prepared source-ref mapping against a candidate push plan.
pub fn source_ref_updates_from_prepare(
    record: &PushPrepareRecord,
    base: &Manifest,
    plan_ref_updates: &[PushRefUpdate],
) -> Result<Vec<PushRefUpdate>> {
    if record.view_ref_updates != plan_ref_updates {
        return Err(conflict("staged ref updates do not match prepare record"));
    }
    for update in &record.source_ref_updates {
        let current = base.refs.get(&update.ref_name);
        if let Some(current) = current {
            validate_sha1(current, "source ref oid")?;
        }
        let current = current.map(|oid| oid.to_ascii_lowercase());
        if current != normalize_optional_oid(update.old_oid.as_deref()) {
            return Err(conflict(format!(
                "source ref changed since prepare: {}",
                update.ref_name
            )));
        }
    }
    Ok(record.source_ref_updates.clone())
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

/// Returns `None` for empty strings without trimming meaningful hash values.
#[must_use]
pub fn non_empty(value: &str) -> Option<&str> {
    if value.is_empty() { None } else { Some(value) }
}

/// Computes path evidence for protected-push authorization.
pub async fn compute_changed_paths(
    store: &Store,
    router: &StoreLayout<Store>,
    repo_prefix: &str,
    plan: &ProtectedPushPlan,
    ref_updates: &[PushRefUpdate],
    prepare: Option<&PushPrepareRecord>,
) -> Result<Vec<String>> {
    git_workspace::compute_changed_paths(store, router, repo_prefix, plan, ref_updates, prepare)
        .await
}

/// Materializes authorized view ref updates into source-repository commits.
pub async fn materialize_source_push(
    store: &Store,
    router: &StoreLayout<Store>,
    repo_prefix: &str,
    base: Option<&Manifest>,
    plan: &ProtectedPushPlan,
    source_updates: &[PushRefUpdate],
    prepare: &PushPrepareRecord,
) -> Result<MaterializedSourcePush> {
    git_workspace::materialize_source_push(
        store,
        router,
        repo_prefix,
        base,
        plan,
        source_updates,
        prepare,
    )
    .await
}

/// Installs the current repository manifest's packs into an initialized Git dir.
pub async fn install_base_packs(
    store: &Store,
    router: &StoreLayout<Store>,
    git_dir: &Path,
) -> Result<()> {
    git_workspace::install_base_packs(store, router, git_dir).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GitVisibilityPublication {
    Published,
    CompletePackOnly { observed: usize, maximum: usize },
}

enum GitVisibilityBuildError {
    Walk(crab_git::walk::WalkError),
}

impl GitVisibilityPublication {
    #[must_use]
    pub(crate) const fn is_published(self) -> bool {
        matches!(self, Self::Published)
    }
}

pub(crate) async fn publish_materialized_git_visibility(
    store: &Store,
    router: &StoreLayout<Store>,
    manifest: &Manifest,
    visibility: &MaterializedGitVisibility,
) -> Result<GitVisibilityPublication> {
    let refs = match visibility {
        MaterializedGitVisibility::Exact(refs) => refs,
        MaterializedGitVisibility::CompletePackOnly { observed, maximum } => {
            return Ok(GitVisibilityPublication::CompletePackOnly {
                observed: *observed,
                maximum: *maximum,
            });
        }
    };
    let index = crab_metadata::git_visibility::GitVisibilityIndex::new(
        manifest.generation,
        manifest.pack_index_hash.clone(),
        manifest.git_validation_digest.clone(),
        refs.clone(),
    )
    .map_err(AuthServerError::from)?;
    if !index.matches_manifest(manifest) {
        return Err(invalid(
            "materialized Git visibility does not match the candidate manifest",
        ));
    }
    crab_metadata::git_visibility::upload_if_absent(store, router, &index)
        .await
        .map_err(AuthServerError::from)?;
    Ok(GitVisibilityPublication::Published)
}

pub(crate) async fn publish_git_visibility_index_from_git_dir(
    store: &Store,
    router: &StoreLayout<Store>,
    manifest: &Manifest,
    git_dir: &Path,
) -> Result<GitVisibilityPublication> {
    let refs = manifest
        .refs
        .iter()
        .map(|(name, oid)| (name.clone(), oid.clone()))
        .collect::<Vec<_>>();
    let visibility =
        build_git_visibility_from_git_dir(git_dir, &refs, &manifest.peeled_refs).await?;
    publish_materialized_git_visibility(store, router, manifest, &visibility).await
}

pub(crate) async fn build_git_visibility_from_git_dir(
    git_dir: &Path,
    refs: &[(String, String)],
    peeled_refs: &BTreeMap<String, String>,
) -> Result<MaterializedGitVisibility> {
    if refs.is_empty() {
        return Ok(MaterializedGitVisibility::Exact(BTreeMap::new()));
    }

    let refs = refs.to_vec();
    let peeled_refs = peeled_refs.clone();
    let git_dir_for_walk = git_dir.to_owned();
    let walk = tokio::task::spawn_blocking(move || {
        let closures = crab_git::walk::walk_reachable_by_ref_bounded(
            &git_dir_for_walk,
            &refs,
            &peeled_refs,
            crab_metadata::git_visibility::MAX_SYNCHRONOUS_GIT_VISIBILITY_OBJECTS as usize,
        )
        .map_err(GitVisibilityBuildError::Walk)?;
        Ok::<_, GitVisibilityBuildError>(closures)
    })
    .await
    .map_err(|source| AuthServerError::GitVisibilityJoin { source })?;
    let closures = match walk {
        Ok(closures) => closures,
        Err(GitVisibilityBuildError::Walk(crab_git::walk::WalkError::LimitExceeded {
            actual,
            maximum,
        })) => {
            return Ok(MaterializedGitVisibility::CompletePackOnly {
                observed: actual,
                maximum,
            });
        }
        Err(GitVisibilityBuildError::Walk(source)) => {
            return Err(AuthServerError::GitVisibilityWalk { source });
        }
    };

    let refs = closures
        .into_iter()
        .map(|(name, closure)| {
            let mut objects = BTreeSet::new();
            objects.extend(closure.commits.iter().map(sha1_hex));
            objects.extend(closure.trees.iter().map(sha1_hex));
            objects.extend(closure.blobs.iter().map(sha1_hex));
            objects.extend(closure.tags.iter().map(sha1_hex));
            (name, objects.into_iter().collect::<Vec<_>>())
        })
        .collect();
    Ok(MaterializedGitVisibility::Exact(refs))
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

fn sha1_hex(oid: &[u8; 20]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(40);
    for byte in oid {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

/// Publishes service metadata indexes for staged shard terms.
pub async fn commit_service_metadata(
    store: &Store,
    router: &StoreLayout<Store>,
    plan: &ProtectedPushPlan,
    manifest: &Manifest,
    gc_registry_generation: u64,
) -> Result<[u8; 32]> {
    let shard_index_hash = if manifest.shard_index_hash.is_empty() {
        MerkleHash::default()
    } else {
        merkle_hash_from_hex(&manifest.shard_index_hash, "committed shard-index hash")?
    };
    let digest = generation_file_index_digest(shard_index_hash.into());
    let Some(candidate_hash) = non_empty(&plan.candidate_manifest.shard_index_hash) else {
        return Ok(digest);
    };
    let index =
        read_staged_segment_index(store, router, SegmentKind::Shard, candidate_hash, plan).await?;

    let config = RemoteIndexConfig::for_repo_with_global_prefix(
        router.repo_prefix(),
        router.global_prefix(),
    );
    let writer = RemoteIndexWriter::open(Arc::clone(store.inner()), &config, true, true).await?;
    let workspace = tempfile::tempdir()?;
    let operation = async {
        for segment in index.segments {
            let segment_path = router.repo_path(&segment.path);
            let segment_key = segment_path.as_ref().to_owned();
            let bytes = read_staged_object_bytes(store, plan, &segment_key).await?;
            let entries = segmented::parse_shard_segment_entries(&segment, &bytes, &segment_key)?;
            for entry in entries {
                let shard_hash = merkle_hash_from_hex(&entry.shard_hash, "shard segment entry")?;
                let shard_bytes = read_shard_bytes(store, router, plan, &shard_hash).await?;
                let workspace_path = workspace.path().to_owned();
                let spool = tokio::task::spawn_blocking(move || {
                    ShardReplaySpool::from_reader_in(
                        Cursor::new(shard_bytes),
                        &workspace_path,
                        shard_hash,
                        true,
                        true,
                    )
                })
                .await
                .map_err(|error| {
                    AuthServerError::Internal(format!("shard replay worker failed: {error}"))
                })??;

                let mut after_id = 0_i64;
                loop {
                    let rows = spool.file_batch(after_id, REPLAY_BATCH_ENTRIES)?;
                    if rows.is_empty() {
                        break;
                    }
                    let entries = rows
                        .into_iter()
                        .map(|row| {
                            after_id = row.id;
                            (
                                row.file_hash,
                                CommittedFileRecord {
                                    recipe_hash: row.recipe_hash,
                                    shard_hash,
                                    committed_generation: manifest.generation,
                                    shard_index_hash,
                                },
                            )
                        })
                        .collect::<Vec<_>>();
                    writer.write_entries(&entries, &[]).await?;
                }

                let mut origins: HashMap<MerkleHash, OriginReceipt> = HashMap::new();
                let mut after_id = 0_i64;
                loop {
                    let rows = spool.chunk_batch(after_id, REPLAY_BATCH_ENTRIES)?;
                    if rows.is_empty() {
                        break;
                    }
                    let mut entries = Vec::with_capacity(rows.len());
                    for row in rows {
                        after_id = row.id;
                        let origin = if let Some(origin) = origins.get(&row.xorb_hash) {
                            origin.clone()
                        } else {
                            let xorb_key = router.xorb_path(&row.xorb_hash).as_ref().to_owned();
                            let xorb_path = ObjectPath::from(xorb_key.as_str());
                            let meta = store.head(&xorb_path).await?;
                            let payload_digest =
                                read_xorb_payload_digest(store, &xorb_path, meta.size).await?;
                            let origin = OriginReceipt::new(
                                "canonical-origin".to_owned(),
                                xorb_key,
                                row.xorb_hash.into(),
                                payload_digest,
                                meta.size,
                                meta.e_tag,
                                meta.version,
                            );
                            origins.insert(row.xorb_hash, origin.clone());
                            origin
                        };
                        entries.push((
                            row.chunk_hash,
                            CommittedChunkReceipt {
                                schema_version: RECEIPT_SCHEMA_VERSION,
                                chunk_hash: row.chunk_hash.into(),
                                xorb_hash: row.xorb_hash.into(),
                                chunk_index: row.chunk_index,
                                uncompressed_size: row.uncompressed_size,
                                origin,
                                source_repo_prefix: router.repo_prefix().to_owned(),
                                source_shard_hash: shard_hash.into(),
                                committed_generation: manifest.generation,
                                shard_index_hash: shard_index_hash.into(),
                                gc_registry_generation,
                            },
                        ));
                    }
                    writer.write_entries(&[], &entries).await?;
                }
            }
        }
        Ok::<_, AuthServerError>(())
    }
    .await;
    let close_result = writer.close().await;
    operation?;
    close_result?;
    Ok(digest)
}

async fn while_renewing_git_locator_lock<T>(
    lock: &mut crab_coordination::PushLock,
    operation: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    let renewal_interval = (lock.ttl() / 3).max(std::time::Duration::from_secs(1));
    let mut ticker = tokio::time::interval(renewal_interval);
    ticker.tick().await;
    tokio::pin!(operation);
    let mut renewal_error = None;
    loop {
        tokio::select! {
            result = &mut operation => {
                return match result {
                    Err(error) => Err(error),
                    Ok(value) => match renewal_error {
                        Some(error) => Err(AuthServerError::from(error)),
                        None => Ok(value),
                    },
                };
            }
            _ = ticker.tick(), if renewal_error.is_none() => {
                if let Err(error) = lock.renew().await {
                    renewal_error = Some(error);
                }
            }
        }
    }
}

#[derive(Debug)]
struct ServiceLocatorEvidence {
    pack_id: MerkleHash,
    _temp: tempfile::TempDir,
    idx_path: std::path::PathBuf,
    rev_path: std::path::PathBuf,
    git_sha1: String,
    kind_by_oid: Option<Arc<HashMap<[u8; 20], crab_metadata::git_object_locator::GitObjectKind>>>,
}

fn validate_service_locator_evidence(
    pack: &PackManifestEntry,
    idx_path: &Path,
    rev_path: &Path,
    expected_git_sha1: &str,
) -> Result<()> {
    let locations = crab_git::pack_locator::PackLocationIter::open(idx_path, rev_path, pack.size)
        .map_err(crab_git::pack::PackError::from)?;
    if locations.object_count() != pack.object_count {
        return Err(invalid(format!(
            "committed pack index has {} objects, expected {}",
            locations.object_count(),
            pack.object_count
        )));
    }
    if locations.pack_checksum().to_string() != expected_git_sha1 {
        return Err(invalid(
            "committed pack index checksum disagrees with pack trailer",
        ));
    }
    Ok(())
}

async fn download_service_locator_evidence(
    store: &Store,
    router: &StoreLayout<Store>,
    pack: &PackManifestEntry,
) -> Result<ServiceLocatorEvidence> {
    if pack.size < 20 {
        return Err(invalid("committed Git pack is too short for its trailer"));
    }
    let trailer = store
        .range_get(&router.pack_path(&pack.pack_id), pack.size - 20..pack.size)
        .await?;
    let git_sha1 = gix_hash::ObjectId::from(
        <[u8; 20]>::try_from(trailer.as_ref())
            .map_err(|_| invalid("committed Git pack trailer is not 20 bytes"))?,
    )
    .to_string();
    let temp = tempfile::tempdir()?;
    let idx_path = temp.path().join("pack.idx");
    let rev_path = temp.path().join("pack.rev");
    let index_maximum = crab_git::pack_locator::max_pack_index_size(pack.object_count)
        .ok_or_else(|| invalid("committed Git pack index size overflows its bound"))?;
    let reverse_maximum = crab_git::pack_locator::pack_reverse_index_size(pack.object_count)
        .ok_or_else(|| invalid("committed Git reverse index size overflows its bound"))?;
    store
        .download_to_path_bounded(
            &router.pack_index_path(&pack.pack_id),
            &idx_path,
            index_maximum,
        )
        .await?;
    store
        .download_to_path_bounded(
            &router.pack_reverse_index_path(&pack.pack_id),
            &rev_path,
            reverse_maximum,
        )
        .await?;
    validate_service_locator_evidence(pack, &idx_path, &rev_path, &git_sha1)?;
    let kind_by_oid =
        load_service_pack_kind_metadata(store, router, pack, &idx_path, &rev_path).await?;
    Ok(ServiceLocatorEvidence {
        pack_id: merkle_hash_from_hex(&pack.pack_id, "committed pack id")?,
        _temp: temp,
        idx_path,
        rev_path,
        git_sha1,
        kind_by_oid,
    })
}

async fn load_service_pack_kind_metadata(
    store: &Store,
    router: &StoreLayout<Store>,
    pack: &PackManifestEntry,
    idx_path: &Path,
    rev_path: &Path,
) -> Result<Option<Arc<HashMap<[u8; 20], crab_metadata::git_object_locator::GitObjectKind>>>> {
    let path = router.pack_kind_metadata_path(&pack.pack_id);
    let maximum = crab_git::pack_locator::pack_kind_metadata_size(pack.object_count)
        .ok_or_else(|| invalid("Git kind metadata size overflows its bound"))?;
    let bytes = match store.get_with_etag_bounded(&path, maximum).await {
        Ok((bytes, _)) => bytes,
        Err(StorageError::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let idx_path = idx_path.to_owned();
    let rev_path = rev_path.to_owned();
    let pack_size = pack.size;
    let object_count = usize::try_from(pack.object_count)
        .map_err(|_| invalid("Git kind metadata object count does not fit in memory"))?;
    let map = tokio::task::spawn_blocking(move || -> Result<_> {
        let locations =
            crab_git::pack_locator::PackLocationIter::open(&idx_path, &rev_path, pack_size)
                .map_err(crab_git::pack::PackError::from)?;
        let entries = crab_git::pack_locator::decode_pack_kind_metadata_iter(&bytes, locations)
            .map_err(crab_git::pack::PackError::from)?;
        let mut kinds = HashMap::with_capacity(entries.len());
        for entry in entries {
            let (oid, kind) = entry.map_err(crab_git::pack::PackError::from)?;
            let oid: [u8; 20] = oid
                .as_bytes()
                .try_into()
                .map_err(|_| invalid("Git kind metadata contains a non-SHA1 object"))?;
            let kind = match kind {
                gix_object::Kind::Commit => {
                    crab_metadata::git_object_locator::GitObjectKind::Commit
                }
                gix_object::Kind::Tree => crab_metadata::git_object_locator::GitObjectKind::Tree,
                gix_object::Kind::Blob => crab_metadata::git_object_locator::GitObjectKind::Blob,
                gix_object::Kind::Tag => crab_metadata::git_object_locator::GitObjectKind::Tag,
            };
            if kinds.insert(oid, kind).is_some() {
                return Err(invalid("Git kind metadata contains a duplicate object"));
            }
        }
        if kinds.len() != object_count {
            return Err(invalid(format!(
                "Git kind metadata contains {} objects, expected {object_count}",
                kinds.len()
            )));
        }
        Ok(Arc::new(kinds))
    })
    .await
    .map_err(|error| {
        AuthServerError::Internal(format!("Git object-kind metadata worker failed: {error}"))
    })??;
    Ok(Some(map))
}

#[derive(Clone, Copy)]
enum NewPackLocatorSource {
    PackBody,
    VerifiedIndexes,
}

async fn derive_service_locator_evidence(
    store: &Store,
    router: &StoreLayout<Store>,
    pack: &PackManifestEntry,
) -> Result<ServiceLocatorEvidence> {
    let pack_id = merkle_hash_from_hex(&pack.pack_id, "committed pack id")?;
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source.pack");
    let downloaded = store
        .download_to_path_bounded(&router.pack_path(&pack.pack_id), &source, pack.size)
        .await?;
    if downloaded != pack.size {
        return Err(invalid(format!(
            "committed pack {} has size {downloaded}, expected {}",
            pack.pack_id, pack.size
        )));
    }
    let canonical_name = pack.pack_id.clone();
    let expected_object_count = pack.object_count;
    let (
        actual_pack_id,
        temp,
        idx_path,
        rev_path,
        git_sha1,
        idx_size,
        idx_hash,
        rev_size,
        rev_hash,
        kind_by_oid,
        kind_metadata,
    ) = tokio::task::spawn_blocking(move || {
        crab_git::initialize_bare_git_dir(temp.path()).map_err(AuthServerError::from)?;
        let pack_dir = temp.path().join("objects/pack");
        std::fs::create_dir_all(&pack_dir)?;
        let installed = crab_git::pack::install_pack_file_from_path(
            &pack_dir,
            &source,
            &canonical_name,
            0,
            // Receive validation already fscked the complete repository. This
            // isolated acceleration directory lacks objects retained in older
            // packs, so object fsck would reject valid incremental packs.
            false,
        )?;
        let mut locations = crab_git::pack_locator::PackLocationIter::open(
            &installed.idx_path,
            &installed.rev_path,
            downloaded,
        )
        .map_err(crab_git::pack::PackError::from)?;
        if locations.object_count() != expected_object_count {
            return Err(invalid(format!(
                "committed pack index has {} objects, expected {}",
                locations.object_count(),
                expected_object_count
            )));
        }
        if locations.pack_checksum().to_string() != installed.git_sha1 {
            return Err(invalid(
                "committed pack index checksum disagrees with pack trailer",
            ));
        }
        let object_ids = locations
            .by_ref()
            .map(|location| {
                location
                    .map(|location| location.oid)
                    .map_err(crab_git::pack::PackError::from)
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(AuthServerError::from)?;
        let kinds = crab_git::object_kinds_from_git_dir(temp.path(), &object_ids)
            .map_err(AuthServerError::from)?;
        if kinds.len() != object_ids.len() {
            return Err(AuthServerError::Internal(
                "Git object-kind catalog returned an incomplete pack result".to_owned(),
            ));
        }
        let ordered_kinds = object_ids
            .iter()
            .map(|oid| {
                kinds.get(oid).copied().ok_or_else(|| {
                    AuthServerError::Internal(
                        "Git object-kind catalog omitted a pack object".to_owned(),
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let kind_metadata = crab_git::pack_locator::encode_pack_kind_metadata(
            locations.pack_checksum(),
            &ordered_kinds,
        )
        .map_err(crab_git::pack::PackError::from)
        .map_err(AuthServerError::from)?;
        let kind_by_oid = kinds
            .into_iter()
            .map(|(oid, kind)| {
                let oid = oid.as_bytes().try_into().map_err(|_| {
                    AuthServerError::Internal(
                        "Git object-kind catalog returned a non-SHA1 object".to_owned(),
                    )
                })?;
                let kind = match kind {
                    gix_object::Kind::Commit => {
                        crab_metadata::git_object_locator::GitObjectKind::Commit
                    }
                    gix_object::Kind::Tree => {
                        crab_metadata::git_object_locator::GitObjectKind::Tree
                    }
                    gix_object::Kind::Blob => {
                        crab_metadata::git_object_locator::GitObjectKind::Blob
                    }
                    gix_object::Kind::Tag => crab_metadata::git_object_locator::GitObjectKind::Tag,
                };
                Ok((oid, kind))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        let mut file = std::fs::File::open(&source)?;
        let mut hasher = blake3::Hasher::new();
        std::io::copy(&mut file, &mut hasher)?;
        let mut idx_file = std::fs::File::open(&installed.idx_path)?;
        let idx_size = idx_file.metadata()?.len();
        let mut idx_hasher = blake3::Hasher::new();
        std::io::copy(&mut idx_file, &mut idx_hasher)?;
        let mut rev_file = std::fs::File::open(&installed.rev_path)?;
        let rev_size = rev_file.metadata()?.len();
        let mut rev_hasher = blake3::Hasher::new();
        std::io::copy(&mut rev_file, &mut rev_hasher)?;
        Ok::<_, AuthServerError>((
            hasher.finalize().to_hex().to_string(),
            temp,
            installed.idx_path,
            installed.rev_path,
            installed.git_sha1,
            idx_size,
            *idx_hasher.finalize().as_bytes(),
            rev_size,
            *rev_hasher.finalize().as_bytes(),
            kind_by_oid,
            kind_metadata,
        ))
    })
    .await
    .map_err(|error| AuthServerError::Internal(format!("pack indexing join failed: {error}")))??;
    if actual_pack_id != pack.pack_id {
        return Err(invalid(format!(
            "committed pack {} body hash mismatch",
            pack.pack_id
        )));
    }
    store
        .put_multipart_file_retry(
            &router.pack_index_path(&pack.pack_id),
            &idx_path,
            idx_size,
            idx_hash,
            8 * 1024 * 1024,
            &tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await?;
    store
        .put_multipart_file_retry(
            &router.pack_reverse_index_path(&pack.pack_id),
            &rev_path,
            rev_size,
            rev_hash,
            8 * 1024 * 1024,
            &tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await?;
    store
        .put(
            &router.pack_kind_metadata_path(&pack.pack_id),
            bytes::Bytes::from(kind_metadata),
        )
        .await?;
    Ok(ServiceLocatorEvidence {
        pack_id,
        _temp: temp,
        idx_path,
        rev_path,
        git_sha1,
        kind_by_oid: Some(Arc::new(kind_by_oid)),
    })
}

async fn ensure_service_locator_evidence(
    store: &Store,
    router: &StoreLayout<Store>,
    packs: &[PackManifestEntry],
    derived: &mut Vec<ServiceLocatorEvidence>,
    pending_ids: &HashSet<MerkleHash>,
) -> Result<()> {
    for pack in packs {
        let pack_id = merkle_hash_from_hex(&pack.pack_id, "committed pack id")?;
        if !pending_ids.contains(&pack_id) || derived.iter().any(|item| item.pack_id == pack_id) {
            continue;
        }
        derived.push(download_service_locator_evidence(store, router, pack).await?);
    }
    Ok(())
}

async fn write_service_locator_evidence(
    writer: &mut crab_metadata::git_object_locator::GitObjectLocatorWriter,
    bindings: &HashMap<MerkleHash, crab_metadata::git_object_locator::GitPackLocatorBinding>,
    derived: &[ServiceLocatorEvidence],
    pending_ids: &HashSet<MerkleHash>,
) -> Result<()> {
    for evidence in derived {
        if !pending_ids.contains(&evidence.pack_id) {
            continue;
        }
        let Some(binding) = bindings.get(&evidence.pack_id).copied() else {
            continue;
        };
        let mut locations = crab_git::pack_locator::PackLocationIter::open(
            &evidence.idx_path,
            &evidence.rev_path,
            binding.record.pack_size,
        )
        .map_err(crab_git::pack::PackError::from)?;
        if locations.pack_checksum().to_string() != evidence.git_sha1 {
            return Err(invalid(
                "committed pack index checksum changed before locator publication",
            ));
        }
        let mut entries = Vec::with_capacity(25_000);
        for location in &mut locations {
            let location = location.map_err(crab_git::pack::PackError::from)?;
            let oid = location
                .oid
                .as_bytes()
                .try_into()
                .map_err(|_| invalid("committed pack index contains non-SHA1 object"))?;
            entries.push(crab_metadata::git_object_locator::GitObjectLocatorEntry {
                oid,
                location: crab_metadata::git_object_locator::GitObjectLocation {
                    pack_offset: location.pack_offset,
                    entry_len: location.entry_len,
                    crc32: location.crc32,
                },
                metadata: crab_metadata::git_object_locator::GitObjectMetadata {
                    kind: evidence
                        .kind_by_oid
                        .as_ref()
                        .and_then(|kinds| kinds.get(&oid).copied()),
                    ..Default::default()
                },
            });
            if entries.len() == 25_000 {
                writer.write_locations(binding, &entries).await?;
                entries.clear();
            }
        }
        if !entries.is_empty() {
            writer.write_locations(binding, &entries).await?;
        }
    }
    Ok(())
}

async fn commit_service_git_locators_with_source(
    store: &Store,
    router: &StoreLayout<Store>,
    manifest: &Manifest,
    packs: &[PackManifestEntry],
    source: NewPackLocatorSource,
) -> Result<[u8; 32]> {
    let pack_index_hash = if manifest.pack_index_hash.is_empty() {
        MerkleHash::default()
    } else {
        merkle_hash_from_hex(&manifest.pack_index_hash, "committed pack-index hash")?
    };
    let mut derived = Vec::with_capacity(packs.len());
    for pack in packs {
        derived.push(match source {
            NewPackLocatorSource::PackBody => {
                derive_service_locator_evidence(store, router, pack).await?
            }
            NewPackLocatorSource::VerifiedIndexes => {
                download_service_locator_evidence(store, router, pack).await?
            }
        });
    }

    let mut lock = crab_coordination::PushLock::acquire_internal_default(
        store.inner(),
        router.repo_prefix(),
        crab_coordination::GIT_OBJECT_LOCATOR_RESOURCE,
    )
    .await?;
    let write_result = while_renewing_git_locator_lock(&mut lock, async {
        let (current, _) = crab_metadata::manifest_store::read_manifest(store, router).await?;
        if current.generation != manifest.generation
            || current.pack_index_hash != manifest.pack_index_hash
        {
            return Err(invalid(
                "committed manifest changed before Git locator publication",
            ));
        }
        let current_packs = if current.pack_index_hash.is_empty() {
            Vec::new()
        } else {
            crab_metadata::manifest_store::read_bulk_pack_list(
                store,
                router,
                &current.pack_index_hash,
            )
            .await?
        };
        let planned_object_rows = current_packs
            .iter()
            .fold(0_u64, |total, pack| total.saturating_add(pack.object_count));
        let mut writer =
            crab_metadata::git_object_locator::GitObjectLocatorWriter::open_for_publication(
                Arc::clone(store.inner()),
                router.repo_prefix(),
                planned_object_rows,
            )
            .await?;
        let operation = async {
            let records = current_packs
                .iter()
                .map(|pack| {
                    Ok(crab_metadata::git_object_locator::GitPackLocatorRecord {
                        pack_id: merkle_hash_from_hex(&pack.pack_id, "committed pack id")?,
                        committed_generation: manifest.generation,
                        pack_index_hash,
                        object_count: pack.object_count,
                        pack_size: pack.size,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let bindings = writer.bind_packs(&records).await?;
            let retained_slots = bindings
                .iter()
                .map(|binding| binding.pack_slot)
                .collect::<HashSet<_>>();
            // Publish current-pack rows before sweeping stale slots. A repack
            // can move every OID to a new slot without changing the object
            // universe; sweeping first would force a full catalog rebuild.
            let covered = bindings
                .iter()
                .filter(|binding| writer.binding_has_covered_objects(**binding))
                .map(|binding| binding.record.pack_id)
                .collect::<HashSet<_>>();
            let mut pending_ids = HashSet::new();
            for pack in &current_packs {
                let pack_id = merkle_hash_from_hex(&pack.pack_id, "committed pack id")?;
                if covered.contains(&pack_id) {
                    continue;
                }
                pending_ids.insert(pack_id);
                if let Some(local) = derived.iter().find(|item| item.pack_id == pack_id) {
                    validate_service_locator_evidence(
                        pack,
                        &local.idx_path,
                        &local.rev_path,
                        &local.git_sha1,
                    )?;
                    continue;
                }
                derived.push(download_service_locator_evidence(store, router, pack).await?);
            }
            let bindings = bindings
                .into_iter()
                .map(|binding| (binding.record.pack_id, binding))
                .collect::<HashMap<_, _>>();
            write_service_locator_evidence(&mut writer, &bindings, &derived, &pending_ids).await?;

            let sweep = writer.sweep_unreferenced(&retained_slots).await?;
            if sweep.object_rows_deleted != 0 {
                // Only an actual object deletion changes the dense ordinal
                // universe. Rebuild then replay every current pack.
                writer.replace_object_catalog(&retained_slots).await?;
                pending_ids = current_packs
                    .iter()
                    .map(|pack| merkle_hash_from_hex(&pack.pack_id, "committed pack id"))
                    .collect::<Result<HashSet<_>>>()?;
                ensure_service_locator_evidence(
                    store,
                    router,
                    &current_packs,
                    &mut derived,
                    &pending_ids,
                )
                .await?;
                write_service_locator_evidence(&mut writer, &bindings, &derived, &pending_ids)
                    .await?;
                writer.complete_object_catalog_rebuild().await?;
            }
            tracing::debug!(
                object_rows_deleted = sweep.object_rows_deleted,
                pack_rows_deleted = sweep.pack_rows_deleted,
                catalog_rebuilt = sweep.object_rows_deleted != 0,
                "swept stale Git locator rows"
            );
            writer.flush_objects().await?;
            let (after, _) = crab_metadata::manifest_store::read_manifest(store, router).await?;
            if after.generation != manifest.generation
                || after.pack_index_hash != manifest.pack_index_hash
            {
                return Err(invalid(
                    "committed manifest changed during Git locator publication",
                ));
            }
            writer
                .set_coverage(crab_metadata::git_object_locator::GitLocatorCoverage {
                    generation: manifest.generation,
                    pack_index_hash,
                })
                .await?;
            Ok::<_, AuthServerError>(())
        }
        .await;
        let close_result = writer.close().await.map_err(AuthServerError::from);
        match (operation, close_result) {
            (Ok(()), Ok(stats)) if stats.coverage_updated => Ok(stats),
            (Ok(()), Ok(_)) => Err(invalid("Git locator coverage was not advanced")),
            (Err(error), Ok(_)) | (Ok(()), Err(error)) => Err(error),
            (Err(error), Err(close_error)) => {
                tracing::warn!(
                    error = %close_error,
                    "Git locator close also failed after publication error"
                );
                Err(error)
            }
        }
    })
    .await;
    let release_result = lock.release().await.map_err(AuthServerError::from);
    let _stats = write_result?;
    release_result?;
    Ok(generation_git_object_locator_digest(pack_index_hash.into()))
}

/// Publish exact Git object locators by indexing each new pack body.
pub async fn commit_service_git_locators(
    store: &Store,
    router: &StoreLayout<Store>,
    manifest: &Manifest,
    packs: &[PackManifestEntry],
) -> Result<[u8; 32]> {
    commit_service_git_locators_with_source(
        store,
        router,
        manifest,
        packs,
        NewPackLocatorSource::PackBody,
    )
    .await
}

pub(crate) async fn commit_service_git_locators_from_verified_indexes(
    store: &Store,
    router: &StoreLayout<Store>,
    manifest: &Manifest,
    packs: &[PackManifestEntry],
) -> Result<[u8; 32]> {
    commit_service_git_locators_with_source(
        store,
        router,
        manifest,
        packs,
        NewPackLocatorSource::VerifiedIndexes,
    )
    .await
}

/// Write the post-CAS receipt only after both acceleration indexes commit.
pub async fn write_service_generation_index_receipt(
    store: &Store,
    router: &StoreLayout<Store>,
    manifest: &Manifest,
    file_index_digest: [u8; 32],
    git_object_locator_digest: [u8; 32],
) -> Result<()> {
    let shard_index_hash = if manifest.shard_index_hash.is_empty() {
        MerkleHash::default()
    } else {
        merkle_hash_from_hex(&manifest.shard_index_hash, "committed shard-index hash")?
    };
    let pack_index_hash = if manifest.pack_index_hash.is_empty() {
        MerkleHash::default()
    } else {
        merkle_hash_from_hex(&manifest.pack_index_hash, "committed pack-index hash")?
    };
    let receipt = GenerationIndexReceipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        generation: manifest.generation,
        shard_index_hash: shard_index_hash.into(),
        pack_index_hash: pack_index_hash.into(),
        file_index_digest,
        git_object_locator_digest,
    };
    receipt.validate(
        manifest.generation,
        shard_index_hash.into(),
        pack_index_hash.into(),
    )?;
    let path = router.repo_path(&format!(
        "metadata/generation-receipts/{:020}.json",
        manifest.generation
    ));
    let body = serde_json::to_vec(&receipt)
        .map_err(|error| AuthServerError::Internal(format!("receipt serialize: {error}")))?;
    match store.put(&path, bytes::Bytes::from(body)).await {
        Ok(()) => Ok(()),
        Err(StorageError::StateConflict { .. }) => {
            let (existing, _) = store.get_with_etag(&path).await?;
            let existing: GenerationIndexReceipt = serde_json::from_slice(&existing)
                .map_err(|error| invalid(format!("generation receipt decode failed: {error}")))?;
            existing.validate(
                manifest.generation,
                shard_index_hash.into(),
                pack_index_hash.into(),
            )?;
            if existing != receipt {
                return Err(invalid(
                    "generation receipt conflicts with the committed index digest",
                ));
            }
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// Builds the source-repository manifest for a verified protected push.
pub async fn build_service_candidate_manifest(
    store: &Store,
    router: &StoreLayout<Store>,
    base: Option<&Manifest>,
    plan: &ProtectedPushPlan,
    materialized: &MaterializedSourcePush,
) -> Result<Manifest> {
    let generation = base.map_or(1, |manifest| manifest.generation.saturating_add(1));
    let mut manifest = base.map_or_else(
        || Manifest::default_for_repo(&plan.candidate_manifest.head),
        Clone::clone,
    );

    manifest.generation = generation;
    manifest.created_at = now_rfc3339_millis();
    manifest.session_id = uuid::Uuid::now_v7().to_string();
    for update in &materialized.ref_updates {
        manifest
            .refs
            .insert(update.ref_name.clone(), update.new_oid.clone());
    }
    if !manifest.refs.contains_key(&manifest.head) {
        let candidate = &plan.candidate_manifest.head;
        let branch =
            if candidate.starts_with("refs/heads/") && manifest.refs.contains_key(candidate) {
                Some(candidate)
            } else {
                manifest
                    .refs
                    .keys()
                    .find(|name| name.starts_with("refs/heads/"))
            };
        if let Some(branch) = branch {
            manifest.head = branch.clone();
        }
    }
    manifest.peeled_refs = materialized.peeled_refs.clone();
    manifest.shard_index_hash = build_service_segment_index(
        store,
        router,
        SegmentKind::Shard,
        base.and_then(|manifest| non_empty(&manifest.shard_index_hash)),
        non_empty(&plan.candidate_manifest.shard_index_hash),
        generation,
        plan,
    )
    .await?;
    manifest.pack_index_hash = build_service_pack_index(
        store,
        router,
        base.and_then(|manifest| non_empty(&manifest.pack_index_hash)),
        non_empty(&plan.candidate_manifest.pack_index_hash),
        generation,
        plan,
        &materialized.packs,
    )
    .await?;
    // Protected receive validates the staged workspace and dependency receipt
    // before this candidate reaches the service-owned commit workflow.
    manifest.seal_git_validation();
    Ok(manifest)
}

async fn read_xorb_payload_digest(
    store: &Store,
    path: &ObjectPath,
    object_size: u64,
) -> Result<[u8; 32]> {
    let footer_size =
        u64::try_from(FOOTER_SIZE).map_err(|_| invalid("xorb footer size overflows u64"))?;
    if object_size < footer_size {
        return Err(AuthServerError::CorruptObject {
            path: path.to_string(),
            reason: "xorb is too small for its footer".to_owned(),
        });
    }
    let footer = store
        .range_get(path, object_size - footer_size..object_size)
        .await?;
    let data_len = usize::try_from(object_size).map_err(|_| AuthServerError::CorruptObject {
        path: path.to_string(),
        reason: "xorb size does not fit this platform".to_owned(),
    })?;
    xorb_payload_digest_from_footer(data_len, &footer).map_err(|error| {
        AuthServerError::CorruptObject {
            path: path.to_string(),
            reason: format!("invalid xorb footer: {error}"),
        }
    })
}

async fn read_shard_bytes(
    store: &Store,
    router: &StoreLayout<Store>,
    plan: &ProtectedPushPlan,
    shard_hash: &MerkleHash,
) -> Result<Vec<u8>> {
    let key = router.shard_path(shard_hash).as_ref().to_owned();
    if let Some(bytes) = read_optional_staged_object_bytes(store, plan, &key).await? {
        return Ok(bytes);
    }
    let (bytes, _) = store.get_with_etag(&ObjectPath::from(key)).await?;
    Ok(bytes.to_vec())
}

async fn build_service_segment_index(
    store: &Store,
    router: &StoreLayout<Store>,
    kind: SegmentKind,
    base_hash: Option<&str>,
    delta_hash: Option<&str>,
    generation: u64,
    plan: &ProtectedPushPlan,
) -> Result<String> {
    let base = match base_hash {
        Some(hash) => segmented_store::read_index(store, router, kind, hash)
            .await
            .map_err(AuthServerError::from)?,
        None => SegmentIndex::default(),
    };
    let Some(delta_hash) = delta_hash else {
        return Ok(base_hash.unwrap_or_default().to_owned());
    };
    let delta = read_staged_segment_index(store, router, kind, delta_hash, plan).await?;
    if delta.segments.is_empty() {
        return Ok(base_hash.unwrap_or_default().to_owned());
    }

    let mut combined = base;
    for segment in delta.segments {
        if segment.snapshot {
            return Err(invalid(format!(
                "protected push does not allow {} metadata compaction",
                kind.as_str()
            )));
        }
        combined = segmented::append_segment(
            combined,
            segmented::SegmentRef {
                generation,
                snapshot: false,
                ..segment
            },
        );
    }
    combined.generation = generation;
    let index = segmented::build_index_object(kind, combined)?;
    segmented_store::upload_if_absent(
        store,
        router,
        &segmented::index_relative_path(kind, &index.hash),
        &index.bytes,
    )
    .await
    .map_err(AuthServerError::from)?;
    Ok(index.hash)
}

async fn build_service_pack_index(
    store: &Store,
    router: &StoreLayout<Store>,
    base_hash: Option<&str>,
    delta_hash: Option<&str>,
    generation: u64,
    plan: &ProtectedPushPlan,
    extra_packs: &[PackManifestEntry],
) -> Result<String> {
    let mut combined = match base_hash {
        Some(hash) => segmented_store::read_index(store, router, SegmentKind::Pack, hash)
            .await
            .map_err(AuthServerError::from)?,
        None => SegmentIndex::default(),
    };
    if let Some(delta_hash) = delta_hash {
        let delta =
            read_staged_segment_index(store, router, SegmentKind::Pack, delta_hash, plan).await?;
        for segment in delta.segments {
            if segment.snapshot {
                return Err(invalid(
                    "protected push does not allow pack metadata compaction",
                ));
            }
            combined = segmented::append_segment(
                combined,
                segmented::SegmentRef {
                    generation,
                    snapshot: false,
                    ..segment
                },
            );
        }
    }
    if !extra_packs.is_empty() {
        let segment = segmented::build_segment(SegmentKind::Pack, generation, false, extra_packs)?
            .ok_or_else(|| invalid("pack metadata segment builder returned empty"))?;
        segmented_store::upload_if_absent(store, router, &segment.reference.path, &segment.bytes)
            .await
            .map_err(AuthServerError::from)?;
        combined = segmented::append_segment(combined, segment.reference);
    }
    if combined.segments.is_empty() {
        return Ok(base_hash.unwrap_or_default().to_owned());
    }
    combined.generation = generation;
    let index = segmented::build_index_object(SegmentKind::Pack, combined)?;
    segmented_store::upload_if_absent(
        store,
        router,
        &segmented::index_relative_path(SegmentKind::Pack, &index.hash),
        &index.bytes,
    )
    .await
    .map_err(AuthServerError::from)?;
    Ok(index.hash)
}

fn xorb_hash_from_key(key: &str) -> Result<MerkleHash> {
    let hash = content_hash_from_path(key, "xorbs")
        .ok_or_else(|| invalid(format!("unsupported xorb canonical key: {key}")))?;
    merkle_hash_from_hex(hash, "xorb key hash")
}

fn merkle_hash_from_hex(value: &str, label: &str) -> Result<MerkleHash> {
    MerkleHash::from_hex(value).map_err(|e| invalid(format!("invalid {label}: {e}")))
}

fn validate_candidate_index_hash(
    repo_prefix: &str,
    kind: &str,
    hash: &str,
    staged_keys: &BTreeSet<&str>,
) -> Result<()> {
    if hash.is_empty() {
        return Ok(());
    }
    let key = format!("{repo_prefix}/metadata/{kind}/indexes/{hash}.json");
    if staged_keys.contains(key.as_str()) {
        return Ok(());
    }
    Err(invalid(format!(
        "candidate manifest {kind} index hash is neither current nor staged"
    )))
}

async fn validate_segmented_index(
    store: &Store,
    router: &StoreLayout<Store>,
    kind: SegmentKind,
    candidate_hash: &str,
    plan: &ProtectedPushPlan,
    referenced_keys: &mut BTreeSet<String>,
) -> Result<()> {
    if candidate_hash.is_empty() {
        return Ok(());
    }
    validate_hash_component(candidate_hash, "candidate segment index hash")?;

    let index_path = router.repo_path(&segmented::index_relative_path(kind, candidate_hash));
    let index_key = index_path.as_ref().to_owned();
    referenced_keys.insert(index_key.clone());
    let candidate = read_staged_segment_index(store, router, kind, candidate_hash, plan).await?;
    segmented::validate_segment_index_shape(kind, &candidate)?;

    let base = SegmentIndex::default();
    segmented::validate_segment_index_shape(kind, &base)?;
    segmented::validate_append_only_index(kind, &base, &candidate)?;

    let base_len = base.segments.len();
    let new_segments = &candidate.segments[base_len..];
    if !new_segments.is_empty() && candidate.generation != plan.candidate_manifest.generation {
        return Err(invalid(format!(
            "{} metadata index generation does not match candidate manifest",
            kind.as_str()
        )));
    }
    for segment in new_segments {
        if segment.generation != plan.candidate_manifest.generation {
            return Err(invalid(format!(
                "{} metadata segment generation does not match candidate manifest",
                kind.as_str()
            )));
        }
        if segment.snapshot {
            return Err(invalid(format!(
                "protected push does not allow {} metadata compaction",
                kind.as_str()
            )));
        }
        let segment_path = router.repo_path(&segment.path);
        let canonical_key = segment_path.as_ref().to_owned();
        referenced_keys.insert(canonical_key.clone());
        let bytes = read_staged_object_bytes(store, plan, &canonical_key).await?;
        if bytes.len() as u64 != segment.bytes {
            return Err(invalid(format!(
                "{} metadata segment byte count mismatch",
                kind.as_str()
            )));
        }
        match kind {
            SegmentKind::Shard => {
                let entries =
                    segmented::parse_shard_segment_entries(segment, &bytes, &canonical_key)?;
                for entry in entries {
                    let key = router.shard_path(&entry.shard_hash).as_ref().to_owned();
                    referenced_keys.insert(key.clone());
                    if let Some(bytes) =
                        read_optional_staged_object_bytes(store, plan, &key).await?
                    {
                        for (relative_xorb_key, expected_chunks) in
                            strict_xorb_references_from_shard(&bytes)?
                        {
                            let xorb_hash = xorb_hash_from_key(&relative_xorb_key)?;
                            let xorb_key = router.xorb_path(&xorb_hash).as_ref().to_owned();
                            referenced_keys.insert(xorb_key.clone());
                            if let Some(xorb_bytes) =
                                read_optional_staged_object_bytes(store, plan, &xorb_key).await?
                            {
                                validate_staged_xorb(&xorb_key, &xorb_bytes, &expected_chunks)?;
                            } else {
                                let (xorb_bytes, _) = store
                                    .get_with_etag(&ObjectPath::from(xorb_key.clone()))
                                    .await?;
                                validate_staged_xorb(&xorb_key, &xorb_bytes, &expected_chunks)?;
                            }
                        }
                    } else {
                        require_existing_object(store, &key).await?;
                    }
                }
            }
            SegmentKind::Pack => {
                let entries = parse_pack_segment_entries(segment, &bytes, &canonical_key)?;
                for entry in entries {
                    let pack_key = router.pack_path(&entry.pack_id).as_ref().to_owned();
                    referenced_keys.insert(pack_key.clone());
                    require_staged_or_existing_object(store, plan, &pack_key).await?;
                    let meta_key = router
                        .pack_metadata_path(&entry.pack_id)
                        .as_ref()
                        .to_owned();
                    referenced_keys.insert(meta_key.clone());
                    validate_optional_pack_metadata(store, plan, &meta_key, &entry).await?;
                    let kind_key = router
                        .pack_kind_metadata_path(&entry.pack_id)
                        .as_ref()
                        .to_owned();
                    if let Some(bytes) =
                        read_optional_staged_object_bytes(store, plan, &kind_key).await?
                    {
                        referenced_keys.insert(kind_key.clone());
                        validate_optional_pack_kind_metadata(
                            store, router, plan, &kind_key, &bytes, &entry,
                        )
                        .await?;
                    }
                }
            }
        }
    }

    Ok(())
}

fn validate_staged_objects_are_referenced(
    plan: &ProtectedPushPlan,
    referenced_keys: &BTreeSet<String>,
) -> Result<()> {
    for object in &plan.staged_objects {
        if !referenced_keys.contains(&object.canonical_key) {
            return Err(invalid(format!(
                "staged object is not referenced by candidate metadata: {}",
                object.canonical_key
            )));
        }
    }
    Ok(())
}

async fn validate_optional_pack_metadata(
    store: &Store,
    plan: &ProtectedPushPlan,
    canonical_key: &str,
    pack: &PackManifestEntry,
) -> Result<()> {
    let Some(bytes) = read_optional_staged_object_bytes(store, plan, canonical_key).await? else {
        return Ok(());
    };
    let metadata = parse_pack_metadata(&bytes, canonical_key)?;
    validate_pack_metadata_for_entry(&metadata, pack)?;
    Ok(())
}

async fn validate_optional_pack_kind_metadata(
    store: &Store,
    router: &StoreLayout<Store>,
    plan: &ProtectedPushPlan,
    canonical_key: &str,
    bytes: &[u8],
    pack: &PackManifestEntry,
) -> Result<()> {
    let canonical_pack_key = router.pack_path(&pack.pack_id).as_ref().to_owned();
    let pack_path = if let Some(object) = plan
        .staged_objects
        .iter()
        .find(|object| object.canonical_key == canonical_pack_key)
    {
        if object.size != pack.size {
            return Err(invalid("staged pack size does not match pack metadata"));
        }
        object.staged_key.clone()
    } else {
        canonical_pack_key
    };
    let pack_checksum = if pack.size < 20 {
        return Err(invalid(format!(
            "pack is too short for kind metadata validation: {canonical_key}"
        )));
    } else {
        let trailer = store
            .range_get(&ObjectPath::from(pack_path), pack.size - 20..pack.size)
            .await?;
        gix_hash::ObjectId::from(
            <[u8; 20]>::try_from(trailer.as_ref())
                .map_err(|_| invalid("pack trailer is not 20 bytes"))?,
        )
    };
    crab_git::pack_locator::validate_pack_kind_metadata(bytes, pack_checksum, pack.object_count)
        .map_err(crab_git::pack::PackError::from)?;
    Ok(())
}

async fn require_staged_or_existing_object(
    store: &Store,
    plan: &ProtectedPushPlan,
    canonical_key: &str,
) -> Result<()> {
    if plan
        .staged_objects
        .iter()
        .any(|object| object.canonical_key == canonical_key)
    {
        return Ok(());
    }
    require_existing_object(store, canonical_key).await
}

async fn require_existing_object(store: &Store, canonical_key: &str) -> Result<()> {
    match store
        .head(&ObjectPath::from(canonical_key.to_owned()))
        .await
    {
        Ok(_) => Ok(()),
        Err(StorageError::NotFound { .. }) => Err(invalid(format!(
            "metadata references missing canonical object: {canonical_key}"
        ))),
        Err(e) => Err(e.into()),
    }
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
        || is_allowed_pack_key(relative)
        || is_allowed_metadata_key(relative)
}

fn is_allowed_pack_key(relative: &str) -> bool {
    let Some(rest) = relative.strip_prefix("packs/pack-") else {
        return false;
    };
    [".pack", ".meta", ".kinds"].iter().any(|suffix| {
        rest.strip_suffix(suffix)
            .is_some_and(|hash| validate_hash_component(hash, "pack hash").is_ok())
    })
}

fn is_allowed_metadata_key(relative: &str) -> bool {
    for kind in ["pack", "shard"] {
        let segment_prefix = format!("metadata/{kind}/segments/");
        if let Some(name) = relative.strip_prefix(&segment_prefix) {
            return name.strip_suffix(".jsonl").is_some_and(|hash| {
                validate_hash_component(hash, "metadata segment hash").is_ok()
            });
        }
        let index_prefix = format!("metadata/{kind}/indexes/");
        if let Some(name) = relative.strip_prefix(&index_prefix) {
            return name
                .strip_suffix(".json")
                .is_some_and(|hash| validate_hash_component(hash, "metadata index hash").is_ok());
        }
    }
    false
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
    if let Some(rest) = key.rsplit_once("/pack-").map(|(_, rest)| rest) {
        return rest.strip_suffix(".pack");
    }
    for marker in [
        "/metadata/pack/segments/",
        "/metadata/pack/indexes/",
        "/metadata/shard/segments/",
        "/metadata/shard/indexes/",
    ] {
        if let Some(hash) = key.split_once(marker).map(|(_, rest)| rest) {
            return hash
                .strip_suffix(".jsonl")
                .or_else(|| hash.strip_suffix(".json"));
        }
    }
    None
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
    use crab_metadata::file_index_lookup::resolve_file_hash_to_shard;
    use crab_metadata::pack_metadata::PackMetadata;
    use crab_metadata::remote_index::read_chunk_index_entry;
    use crab_metadata::segmented::ShardSegmentEntry;
    use crab_storage::{StorageError, repo_pack_metadata_path};
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

    fn candidate_manifest() -> Manifest {
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 1;
        manifest.refs.insert("refs/heads/main".to_owned(), oid('2'));
        manifest.shard_index_hash = hash('c');
        manifest.pack_index_hash = hash('d');
        manifest.seal_git_validation();
        manifest
    }

    fn push_plan() -> ProtectedPushPlan {
        ProtectedPushPlan {
            schema_version: 1,
            repo_prefix: "org/repo".to_owned(),
            push_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
            upload_prefix: "org/repo/staging/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/".to_owned(),
            base_manifest_generation: Some(4),
            base_manifest_etag: Some("etag-1".to_owned()),
            ref_updates: vec![ref_update(Some(oid('1')), oid('2'))],
            candidate_manifest: candidate_manifest(),
            push_commit_receipt: None,
            staged_objects: vec![
                staged_object(format!(
                    "org/repo/metadata/shard/indexes/{}.json",
                    hash('c')
                )),
                staged_object(format!("org/repo/metadata/pack/indexes/{}.json", hash('d'))),
            ],
        }
    }

    #[tokio::test]
    async fn service_candidate_preserves_unborn_head_for_tag_only_updates() -> Result<()> {
        let store = store();
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        let mut plan = push_plan();
        plan.candidate_manifest = Manifest::default_for_repo("refs/heads/unborn");
        let materialized = MaterializedSourcePush {
            ref_updates: vec![PushRefUpdate {
                ref_name: "refs/tags/v1".into(),
                old_oid: None,
                new_oid: oid('2'),
            }],
            packs: vec![],
            peeled_refs: BTreeMap::new(),
            git_visibility: MaterializedGitVisibility::Exact(BTreeMap::new()),
        };
        let manifest =
            build_service_candidate_manifest(&store, &router, None, &plan, &materialized).await?;
        assert_eq!(manifest.head, "refs/heads/unborn");
        Ok(())
    }

    #[tokio::test]
    async fn materialized_visibility_must_match_candidate_manifest() {
        let store = store();
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        let manifest = candidate_manifest();
        let visibility = MaterializedGitVisibility::Exact(BTreeMap::from([(
            "refs/heads/main".to_owned(),
            vec![oid('3')],
        )]));

        assert!(
            publish_materialized_git_visibility(&store, &router, &manifest, &visibility)
                .await
                .is_err()
        );
        assert!(matches!(
            store
                .head(&router.git_visibility_path(&manifest.git_validation_digest))
                .await,
            Err(StorageError::NotFound { .. })
        ));
    }

    #[test]
    fn protected_dependency_receipt_is_required_and_candidate_bound() {
        let mut plan = push_plan();
        assert!(validate_protected_dependency_receipt(&plan).is_err());
        let parse = |value: &str| {
            <[u8; 32]>::from(MerkleHash::from_hex(value).expect("candidate index hash"))
        };
        let protected_updates = plan
            .ref_updates
            .iter()
            .map(|update| {
                (
                    update.ref_name.clone(),
                    update.old_oid.clone(),
                    update.new_oid.clone(),
                )
            })
            .collect::<Vec<_>>();
        plan.push_commit_receipt = Some(PushCommitReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            attempt_id: plan.push_id.clone(),
            base_generation: 4,
            base_etag: Some("etag-1".to_owned()),
            ref_edit_digest: crab_metadata::receipts::protected_ref_edit_digest(&protected_updates),
            git_object_set_digest: [2; 32],
            file_recipe_set_digest: [3; 32],
            xorb_proof_digest: [4; 32],
            shard_set_digest: [5; 32],
            candidate_pack_index_hash: parse(&plan.candidate_manifest.pack_index_hash),
            candidate_shard_index_hash: parse(&plan.candidate_manifest.shard_index_hash),
            gc_registry_generation: 0,
            connectivity_digest: crab_metadata::receipts::protected_connectivity_digest(&[plan
                .ref_updates[0]
                .new_oid
                .clone()]),
            plan_digest: [7; 32],
        });

        validate_protected_dependency_receipt(&plan).expect("valid protected receipt");
        let original_new_oid = plan.ref_updates[0].new_oid.clone();
        plan.ref_updates[0].new_oid = oid('3');
        assert!(
            validate_protected_dependency_receipt(&plan).is_err(),
            "receipt must bind the exact protected ref edits"
        );
        plan.ref_updates[0].new_oid = original_new_oid;
        plan.push_commit_receipt
            .as_mut()
            .expect("receipt")
            .candidate_shard_index_hash = [9; 32];
        assert!(validate_protected_dependency_receipt(&plan).is_err());
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
        let mut plan = push_plan();
        plan.ref_updates = (0..=MAX_PUSH_REF_UPDATES)
            .map(|idx| PushRefUpdate {
                ref_name: format!("refs/heads/branch-{idx}"),
                old_oid: Some(oid('1')),
                new_oid: oid('2'),
            })
            .collect();

        let err = validate_push_plan_shape(&plan, "org/repo", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
            .expect_err("oversized ref update list must be rejected");

        assert!(
            err.to_string().contains("too many ref updates"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn push_plan_shape_rejects_excessive_staged_object_list() {
        let mut plan = push_plan();
        plan.staged_objects = (0..=MAX_PUSH_STAGED_OBJECTS)
            .map(|idx| staged_object(format!("org/repo/packs/pack-{}-{idx}.pack", hash('a'))))
            .collect();

        let err = validate_push_plan_shape(&plan, "org/repo", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
            .expect_err("oversized staged object list must be rejected");

        assert!(
            err.to_string().contains("too many staged objects"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn push_plan_rejects_unknown_top_level_fields() {
        let mut value = serde_json::to_value(push_plan()).expect("push plan should serialize");
        value
            .as_object_mut()
            .expect("push plan should serialize as object")
            .insert("legacy_mode".to_owned(), serde_json::json!(true));

        let err = serde_json::from_value::<ProtectedPushPlan>(value)
            .expect_err("unknown push-plan fields must be rejected");

        assert!(
            err.to_string().contains("unknown field"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn push_plan_rejects_unknown_ref_update_fields() {
        let mut value = serde_json::to_value(push_plan()).expect("push plan should serialize");
        value["ref_updates"][0]
            .as_object_mut()
            .expect("ref update should serialize as object")
            .insert("force".to_owned(), serde_json::json!(true));

        let err = serde_json::from_value::<ProtectedPushPlan>(value)
            .expect_err("unknown ref update fields must be rejected");

        assert!(
            err.to_string().contains("unknown field"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn push_plan_rejects_unknown_staged_object_fields() {
        let mut value = serde_json::to_value(push_plan()).expect("push plan should serialize");
        value["staged_objects"][0]
            .as_object_mut()
            .expect("staged object should serialize as object")
            .insert("canonical_write".to_owned(), serde_json::json!(true));

        let err = serde_json::from_value::<ProtectedPushPlan>(value)
            .expect_err("unknown staged object fields must be rejected");

        assert!(
            err.to_string().contains("unknown field"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn candidate_manifest_must_match_ref_updates() {
        let mut plan = push_plan();
        plan.candidate_manifest
            .refs
            .insert("refs/heads/other".to_owned(), oid('9'));
        plan.candidate_manifest.seal_git_validation();

        let err = validate_candidate_manifest_shape(&plan, "org/repo")
            .expect_err("candidate manifest must match ref updates");

        assert!(
            err.to_string().contains("refs differ"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn candidate_manifest_rejects_unstaged_index_hashes() {
        let mut plan = push_plan();
        plan.candidate_manifest.pack_index_hash = hash('9');
        plan.candidate_manifest.seal_git_validation();
        plan.staged_objects
            .retain(|object| !object.canonical_key.contains("/metadata/pack/indexes/"));

        let err = validate_candidate_manifest_shape(&plan, "org/repo")
            .expect_err("unstaged index hash must be rejected");

        assert!(
            err.to_string().contains("neither current nor staged"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn candidate_delta_manifest_allows_empty_index_hash() -> Result<()> {
        let mut plan = push_plan();
        plan.candidate_manifest.pack_index_hash.clear();
        plan.candidate_manifest.seal_git_validation();

        validate_candidate_manifest_shape(&plan, "org/repo")
    }

    #[test]
    fn candidate_manifest_rejects_service_owned_metadata_changes() {
        let mut plan = push_plan();
        plan.candidate_manifest.commit_graph_hash = Some(hash('9'));

        let err = validate_candidate_manifest_shape(&plan, "org/repo")
            .expect_err("service-owned metadata changes must be rejected");

        assert!(
            err.to_string().contains("unsupported metadata"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn candidate_manifest_accepts_staged_updates() -> Result<()> {
        validate_candidate_manifest_shape(&push_plan(), "org/repo")
    }

    #[test]
    fn staged_object_shapes_reject_duplicate_canonical_keys() {
        let mut plan = push_plan();
        let duplicate = plan
            .staged_objects
            .first()
            .expect("push plan should have a staged object")
            .clone();
        plan.staged_objects.push(duplicate);

        let err =
            validate_staged_object_shapes(&plan, "org/repo", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
                .expect_err("duplicate canonical key must be rejected");

        assert!(
            err.to_string().contains("duplicate canonical"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn staged_object_shapes_reject_wrong_staging_prefix() {
        let mut plan = push_plan();
        plan.staged_objects[0].staged_key =
            "org/repo/staging/other/objects/org/repo/packs/pack-bad.pack".to_owned();

        let err =
            validate_staged_object_shapes(&plan, "org/repo", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
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
            format!("{repo}/packs/pack-{}.pack", hash('c')),
            format!("{repo}/packs/pack-{}.meta", hash('c')),
            format!("{repo}/packs/pack-{}.kinds", hash('c')),
            format!("{repo}/metadata/pack/segments/{}.jsonl", hash('d')),
            format!("{repo}/metadata/pack/indexes/{}.json", hash('e')),
            format!("{repo}/metadata/shard/segments/{}.jsonl", hash('f')),
            format!("{repo}/metadata/shard/indexes/{}.json", hash('1')),
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
        let object = staged_object_for_bytes(format!("org/repo/packs/pack-{actual}.pack"), bytes);

        validate_staged_object_bytes(&object, bytes)
    }

    #[test]
    fn staged_object_bytes_reject_content_addressed_mismatch() {
        let bytes = b"pack bytes";
        let object =
            staged_object_for_bytes(format!("org/repo/packs/pack-{}.pack", hash('a')), bytes);

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

    type FileShardReference = (
        MerkleHash,
        MerkleHash,
        Vec<u8>,
        MerkleHash,
        Bytes,
        Vec<ExpectedXorbChunk>,
    );

    fn test_file_shard_reference() -> Result<FileShardReference> {
        use crab_xet::shard::{
            FileDataSequenceEntry, FileDataSequenceHeader, MDBFileInfo, MDBXorbInfo, ShardWriter,
            XorbChunkSequenceEntry, XorbChunkSequenceHeader,
        };
        use crab_xet::xorb::builder::{RunId, XorbBuilder};
        use crab_xet::xorb::format::Chunk;

        let chunk = Chunk::new(Bytes::from_static(b"enterprise service metadata payload"));
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

        let file_hash = MerkleHash::from([42, 42, 42, 42]);
        let mut writer = ShardWriter::new();
        writer.add_xorb(Arc::new(MDBXorbInfo {
            metadata: XorbChunkSequenceHeader::new(xorb.hash, entries.len(), byte_offset),
            chunks: entries.clone(),
        }))?;
        writer.add_file(MDBFileInfo {
            metadata: FileDataSequenceHeader::new(file_hash, 1u32, false, false),
            segments: vec![FileDataSequenceEntry::new(
                xorb.hash,
                byte_offset,
                0u32,
                u32::try_from(entries.len())
                    .map_err(|_| invalid("test xorb entry count overflows u32"))?,
            )],
            verification: vec![],
            metadata_ext: None,
        })?;
        let (shard_bytes, shard_hash) = writer.finalize()?;

        let xorb_key = canonical_global_content_path("xorbs", &xorb.hash.hex()).to_string();
        let refs = strict_xorb_references_from_shard(&shard_bytes)?;
        let expected_chunks = refs
            .get(&xorb_key)
            .cloned()
            .ok_or_else(|| invalid("test shard did not reference test xorb"))?;

        Ok((
            file_hash,
            shard_hash,
            shard_bytes,
            xorb.hash,
            xorb.bytes,
            expected_chunks,
        ))
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

    #[test]
    fn staged_object_reference_check_rejects_unreferenced_object() {
        let plan = push_plan();
        let referenced_keys = BTreeSet::from([plan.staged_objects[0].canonical_key.clone()]);

        let err = validate_staged_objects_are_referenced(&plan, &referenced_keys)
            .expect_err("unreferenced staged object must be rejected before promotion");

        assert!(
            err.to_string().contains("not referenced"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn candidate_metadata_accepts_appended_pack_index() -> Result<()> {
        let store = store();
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        let base_index = SegmentIndex::default();
        let base_index_object =
            segmented::build_index_object(SegmentKind::Pack, base_index.clone())?;
        store
            .put_exact(
                &router.repo_path(&segmented::index_relative_path(
                    SegmentKind::Pack,
                    &base_index_object.hash,
                )),
                Bytes::from(base_index_object.bytes.clone()),
            )
            .await?;

        let pack_bytes = b"pack".to_vec();
        let pack_id = blake3_hex(&pack_bytes);
        let pack = PackManifestEntry {
            pack_id: pack_id.clone(),
            size: pack_bytes.len() as u64,
            content_hash: pack_id.clone(),
            ref_tips: vec![oid('2')],
            object_count: 1,
        };
        let segment = segmented::build_segment(SegmentKind::Pack, 1, false, &[pack])?
            .ok_or_else(|| invalid("test segment missing"))?;
        let candidate_index = segmented::append_segment(base_index, segment.reference.clone());
        let candidate_index_object =
            segmented::build_index_object(SegmentKind::Pack, candidate_index)?;

        let segment_key = router
            .repo_path(&segment.reference.path)
            .as_ref()
            .to_owned();
        let index_key = router
            .repo_path(&segmented::index_relative_path(
                SegmentKind::Pack,
                &candidate_index_object.hash,
            ))
            .as_ref()
            .to_owned();
        let pack_key = format!("org/repo/packs/pack-{pack_id}.pack");
        let pack_metadata = PackMetadata {
            pack_id: pack_id.clone(),
            ref_tips: vec![oid('2')],
            object_count: 1,
        };
        let pack_metadata_bytes = serde_json::to_vec(&pack_metadata)
            .map_err(|e| AuthServerError::Internal(format!("test pack metadata: {e}")))?;
        let pack_metadata_key = format!("org/repo/packs/pack-{pack_id}.meta");

        let segment_object = staged_object_for_bytes(segment_key, &segment.bytes);
        let index_object = staged_object_for_bytes(index_key, &candidate_index_object.bytes);
        let pack_object = staged_object_for_bytes(pack_key, &pack_bytes);
        let pack_metadata_object = staged_object_for_bytes(pack_metadata_key, &pack_metadata_bytes);
        put_staged(&store, &segment_object, Bytes::from(segment.bytes)).await?;
        put_staged(
            &store,
            &index_object,
            Bytes::from(candidate_index_object.bytes),
        )
        .await?;
        put_staged(&store, &pack_object, Bytes::from(pack_bytes)).await?;
        put_staged(
            &store,
            &pack_metadata_object,
            Bytes::from(pack_metadata_bytes),
        )
        .await?;

        let mut candidate = candidate_manifest();
        candidate.shard_index_hash.clear();
        candidate.pack_index_hash = candidate_index_object.hash;

        let plan = ProtectedPushPlan {
            schema_version: 1,
            repo_prefix: "org/repo".to_owned(),
            push_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
            upload_prefix: "org/repo/staging/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/".to_owned(),
            base_manifest_generation: None,
            base_manifest_etag: None,
            ref_updates: vec![ref_update(Some(oid('1')), oid('2'))],
            candidate_manifest: candidate,
            push_commit_receipt: None,
            staged_objects: vec![
                segment_object,
                index_object,
                pack_object,
                pack_metadata_object,
            ],
        };

        validate_staged_object_shapes(&plan, "org/repo", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")?;
        validate_candidate_metadata(&store, &router, &plan).await
    }

    #[tokio::test]
    async fn commit_service_metadata_publishes_file_and_chunk_indexes() -> Result<()> {
        let store = store();
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        let (file_hash, shard_hash, shard_bytes, xorb_hash, xorb_bytes, expected_chunks) =
            test_file_shard_reference()?;
        let shard_entry = ShardSegmentEntry {
            shard_hash: shard_hash.hex(),
            size: shard_bytes.len() as u64,
        };
        let segment = segmented::build_segment(SegmentKind::Shard, 1, false, &[shard_entry])?
            .ok_or_else(|| invalid("test shard segment missing"))?;
        let index = segmented::append_segment(SegmentIndex::default(), segment.reference.clone());
        let index_object = segmented::build_index_object(SegmentKind::Shard, index)?;

        let segment_key = router
            .repo_path(&segment.reference.path)
            .as_ref()
            .to_owned();
        let index_key = router
            .repo_path(&segmented::index_relative_path(
                SegmentKind::Shard,
                &index_object.hash,
            ))
            .as_ref()
            .to_owned();
        let shard_key = router.shard_path(&shard_hash).as_ref().to_owned();
        let xorb_key = router.xorb_path(&xorb_hash).as_ref().to_owned();

        let segment_object = staged_object_for_bytes(segment_key, &segment.bytes);
        let index_object_staged = staged_object_for_bytes(index_key, &index_object.bytes);
        let shard_object = staged_object_for_bytes(shard_key, &shard_bytes);
        let xorb_object = staged_object_for_bytes(xorb_key, &xorb_bytes);
        put_staged(&store, &segment_object, Bytes::from(segment.bytes)).await?;
        put_staged(
            &store,
            &index_object_staged,
            Bytes::from(index_object.bytes),
        )
        .await?;
        put_staged(&store, &shard_object, Bytes::from(shard_bytes)).await?;
        put_staged(&store, &xorb_object, xorb_bytes).await?;

        let mut candidate = candidate_manifest();
        candidate.shard_index_hash = index_object.hash;
        candidate.pack_index_hash.clear();
        candidate.seal_git_validation();
        let plan = ProtectedPushPlan {
            schema_version: 1,
            repo_prefix: "org/repo".to_owned(),
            push_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
            upload_prefix: "org/repo/staging/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/".to_owned(),
            base_manifest_generation: None,
            base_manifest_etag: None,
            ref_updates: vec![ref_update(Some(oid('1')), oid('2'))],
            candidate_manifest: candidate,
            push_commit_receipt: None,
            staged_objects: vec![
                segment_object,
                index_object_staged,
                shard_object,
                xorb_object,
            ],
        };

        validate_staged_object_shapes(&plan, "org/repo", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")?;
        for object in &plan.staged_objects {
            read_verified_staged_object(&store, object).await?;
        }
        validate_candidate_metadata(&store, &router, &plan).await?;
        promote_staged_objects(&store, &plan).await?;
        let mut committed_manifest = plan.candidate_manifest.clone();
        committed_manifest.generation = 1;
        crab_metadata::manifest_store::create_manifest(&store, &router, &committed_manifest)
            .await?;
        commit_service_metadata(&store, &router, &plan, &committed_manifest, 1).await?;

        assert_eq!(
            resolve_file_hash_to_shard(Arc::clone(store.inner()), "org/repo", &file_hash).await?,
            Some(shard_hash)
        );
        let first_chunk = expected_chunks
            .first()
            .ok_or_else(|| invalid("test shard had no chunk entries"))?;
        let config = RemoteIndexConfig::for_repo("org/repo");
        let stored = read_chunk_index_entry(Arc::clone(store.inner()), &config, &first_chunk.hash)
            .await?
            .ok_or_else(|| invalid("chunk index entry missing"))?;
        assert_eq!(stored.xorb_hash, xorb_hash);
        assert_eq!(stored.chunk_index, first_chunk.index);
        assert_eq!(stored.uncompressed_size, first_chunk.uncompressed_size);
        Ok(())
    }

    #[tokio::test]
    async fn pack_metadata_sidecar_must_match_pack_entry() -> Result<()> {
        let store = store();
        let pack = PackManifestEntry {
            pack_id: hash('a'),
            size: 4,
            content_hash: hash('a'),
            ref_tips: vec![oid('2')],
            object_count: 1,
        };
        let metadata = PackMetadata {
            pack_id: hash('b'),
            ref_tips: vec![oid('2')],
            object_count: 1,
        };
        let bytes = serde_json::to_vec(&metadata)
            .map_err(|e| AuthServerError::Internal(format!("test pack metadata: {e}")))?;
        let key = repo_pack_metadata_path("org/repo", &pack.pack_id)
            .as_ref()
            .to_owned();
        let object = staged_object_for_bytes(key.clone(), &bytes);
        put_staged(&store, &object, Bytes::from(bytes)).await?;
        let mut plan = push_plan();
        plan.staged_objects = vec![object];

        let err = validate_optional_pack_metadata(&store, &plan, &key, &pack)
            .await
            .expect_err("pack metadata sidecar must match the referenced pack entry");

        assert!(
            err.to_string().contains("pack_id does not match"),
            "unexpected error: {err}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn promote_pack_metadata_keeps_oversized_existing_hint_legacy() -> Result<()> {
        let store = store();
        let pack_id = hash('a');
        let path = repo_pack_metadata_path("org/repo", &pack_id);
        let existing = Bytes::from(vec![
            b' ';
            usize::try_from(
                crab_metadata::pack_metadata::MAX_PACK_METADATA_BYTES + 1
            )
            .map_err(|_| invalid(
                "test metadata size does not fit usize"
            ))?
        ]);
        store.put(&path, existing.clone()).await?;

        let staged = PackMetadata {
            pack_id,
            ref_tips: vec![oid('2')],
            object_count: 1,
        };
        let staged_bytes = serde_json::to_vec(&staged)
            .map_err(|error| AuthServerError::Internal(format!("test pack metadata: {error}")))?;

        promote_pack_metadata_union(&store, &path, Bytes::from(staged_bytes)).await?;

        let (body, _) = store.get_with_etag(&path).await?;
        assert_eq!(body, existing);
        Ok(())
    }

    #[tokio::test]
    async fn promote_pack_metadata_does_not_enrich_empty_legacy_hint() -> Result<()> {
        let store = store();
        let pack_id = hash('a');
        let path = repo_pack_metadata_path("org/repo", &pack_id);
        let existing = PackMetadata {
            pack_id: pack_id.clone(),
            ref_tips: Vec::new(),
            object_count: 1,
        };
        let existing_bytes = serde_json::to_vec(&existing)
            .map_err(|error| AuthServerError::Internal(format!("test pack metadata: {error}")))?;
        store
            .put(&path, Bytes::from(existing_bytes.clone()))
            .await?;

        let staged = PackMetadata {
            pack_id,
            ref_tips: vec![oid('2')],
            object_count: 1,
        };
        let staged_bytes = serde_json::to_vec(&staged)
            .map_err(|error| AuthServerError::Internal(format!("test pack metadata: {error}")))?;

        promote_pack_metadata_union(&store, &path, Bytes::from(staged_bytes)).await?;

        let (body, _) = store.get_with_etag(&path).await?;
        assert_eq!(body, existing_bytes);
        Ok(())
    }

    #[tokio::test]
    async fn promote_staged_objects_revalidates_staged_bytes_before_canonical_write() -> Result<()>
    {
        let store = store();
        let original = Bytes::from_static(b"pack");
        let pack_id = blake3_hex(&original);
        let canonical_key = format!("org/repo/packs/pack-{pack_id}.pack");
        let object = staged_object_for_bytes(canonical_key.clone(), &original);
        let mut plan = push_plan();
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

        let err = promote_staged_objects(&store, &plan)
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
            "org/repo/packs/pack-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.pack"
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
        let canonical_key = format!("org/repo/packs/pack-{pack_id}.pack");
        let canonical_path = ObjectPath::from(canonical_key.clone());
        let object = staged_object_for_bytes(canonical_key, &original);
        let mut plan = push_plan();
        plan.staged_objects = vec![object.clone()];

        let existing = Bytes::from_static(b"different-pack");
        store.put(&canonical_path, existing.clone()).await?;
        put_staged(&store, &object, original).await?;

        let err = promote_staged_objects(&store, &plan)
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

    #[tokio::test]
    async fn promote_staged_pack_metadata_unions_existing_ref_tips() -> Result<()> {
        let store = store();
        let pack_id = hash('a');
        let canonical_key = format!("org/repo/packs/pack-{pack_id}.meta");
        let canonical_path = ObjectPath::from(canonical_key.clone());
        let existing = PackMetadata {
            pack_id: pack_id.clone(),
            ref_tips: vec![oid('1')],
            object_count: 3,
        };
        store
            .put(
                &canonical_path,
                Bytes::from(serde_json::to_vec(&existing).map_err(|error| {
                    AuthServerError::Internal(format!("existing metadata serialize: {error}"))
                })?),
            )
            .await?;
        let staged = PackMetadata {
            pack_id,
            ref_tips: vec![oid('2')],
            object_count: 3,
        };
        let staged_bytes = Bytes::from(serde_json::to_vec(&staged).map_err(|error| {
            AuthServerError::Internal(format!("staged metadata serialize: {error}"))
        })?);
        let object = staged_object_for_bytes(canonical_key, &staged_bytes);
        put_staged(&store, &object, staged_bytes).await?;
        let mut plan = push_plan();
        plan.staged_objects = vec![object];

        promote_staged_objects(&store, &plan).await?;

        let (body, _) = store.get_with_etag(&canonical_path).await?;
        let merged = parse_pack_metadata(&body, canonical_path.as_ref())?;
        assert_eq!(merged.ref_tips, vec![oid('1'), oid('2')]);
        Ok(())
    }

    #[test]
    fn prepare_record_maps_view_refs_to_source_manifest_refs() -> Result<()> {
        let mut base = Manifest::default_for_repo("refs/heads/main");
        base.generation = 12;
        base.refs.insert("refs/heads/main".to_owned(), oid('A'));
        let view_updates = vec![ref_update(Some(oid('1')), oid('B'))];

        let record = build_prepare_record(
            "org/repo",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            (&base, "etag-12"),
            view_updates.clone(),
            None,
        )?;

        assert_eq!(record.source_manifest_generation, 12);
        assert_eq!(record.source_manifest_etag, "etag-12");
        assert_eq!(record.view_ref_updates, view_updates);
        assert_eq!(
            record.source_ref_updates,
            vec![ref_update(Some(oid('a')), oid('b'))]
        );
        validate_prepare_record_shape(&record, "org/repo", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")?;
        Ok(())
    }

    #[test]
    fn source_ref_updates_accept_case_insensitive_old_oid() -> Result<()> {
        let mut base = Manifest::default_for_repo("refs/heads/main");
        base.refs.insert("refs/heads/main".to_owned(), oid('1'));
        let updates = vec![ref_update(Some(oid('1').to_ascii_uppercase()), oid('2'))];

        let source_updates = source_ref_updates_for(&base, &updates)?;

        assert_eq!(
            source_updates[0].old_oid.as_deref(),
            Some(oid('1').as_str())
        );
        Ok(())
    }

    #[test]
    fn prepare_record_replay_rejects_moved_source_ref() -> Result<()> {
        let mut base = Manifest::default_for_repo("refs/heads/main");
        base.refs.insert("refs/heads/main".to_owned(), oid('1'));
        let view_updates = vec![ref_update(Some(oid('a')), oid('2'))];
        let record = build_prepare_record(
            "org/repo",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            (&base, "etag-1"),
            view_updates.clone(),
            None,
        )?;

        let mut moved = base;
        moved.refs.insert("refs/heads/main".to_owned(), oid('3'));
        let err = source_ref_updates_from_prepare(&record, &moved, &view_updates)
            .expect_err("moved source ref should reject prepare replay");

        assert!(
            matches!(err, AuthServerError::CasConflict { .. }),
            "expected CAS conflict, got {err:?}"
        );
        Ok(())
    }

    #[test]
    fn prepared_view_scope_requires_normalized_source_repo() {
        let scope = PreparedViewScope {
            repo_prefix: "org/repo/acl-views/v1/aaaaaaaa/1-deadbeef".to_owned(),
            global_prefix: "org/repo/acl-views/v1/aaaaaaaa/1-deadbeef/.crab".to_owned(),
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
            repo_prefix: "org/repo/acl-views/v1/aaaaaaaa/1-deadbeef".to_owned(),
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
