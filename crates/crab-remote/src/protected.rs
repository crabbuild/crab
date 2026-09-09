use std::collections::BTreeMap;

use bytes::Bytes;
use crab_auth::PushRefUpdate;
use crab_metadata::{
    manifests::{BulkData, Manifest, append_pack_index, append_shard_index},
    receipts::{
        PushCommitReceipt, RECEIPT_SCHEMA_VERSION, committed_shard_set_digest,
        protected_connectivity_digest, protected_ref_edit_digest,
    },
    segmented::SegmentIndex,
};
use crab_storage::StagedWrite;
use object_store::path::Path as ObjectPath;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::prepare::{Artifacts, Error, Result};

/// Server-verified protected publication plan shared by every Crab client.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ProtectedPushPlan {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror_plan_id: Option<String>,
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

pub(crate) async fn stage_plan(
    artifacts: Artifacts<'_>,
    push_id: &str,
    upload_prefix: &str,
    ref_updates: Vec<PushRefUpdate>,
    cancel: &CancellationToken,
) -> Result<ProtectedPushPlan> {
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let expected = artifacts
        .edits
        .iter()
        .map(|edit| {
            Ok(PushRefUpdate {
                ref_name: edit.ref_name.clone(),
                old_oid: edit.old_oid.clone(),
                new_oid: edit.new_oid.clone().ok_or(Error::Request(
                    "protected publication does not support ref deletion",
                ))?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if expected != ref_updates {
        return Err(Error::Request(
            "protected authorization differs from prepared ref edits",
        ));
    }
    let generation = artifacts
        .snapshot
        .manifest
        .generation
        .checked_add(1)
        .ok_or(Error::Request("candidate generation overflows u64"))?;
    let (shard_index_hash, _, shard_index) =
        append_shard_index(SegmentIndex::default(), generation, &artifacts.shards)?;
    let (pack_index_hash, _, pack_index) =
        append_pack_index(SegmentIndex::default(), generation, &artifacts.packs)?;
    let bulk = BulkData {
        shard_index,
        pack_index,
    };
    crab_metadata::manifest_store::upload_segmented_bulk(
        artifacts.prepared.layout.store(),
        &artifacts.prepared.layout,
        &bulk,
    )
    .await?;

    let mut candidate = Manifest::default_for_repo(&artifacts.snapshot.journal.head);
    candidate.generation = generation;
    candidate.refs = ref_updates
        .iter()
        .map(|update| (update.ref_name.clone(), update.new_oid.clone()))
        .collect::<BTreeMap<_, _>>();
    candidate.shard_index_hash = shard_index_hash;
    candidate.pack_index_hash = pack_index_hash;
    candidate.seal_git_validation();

    let refs = ref_updates
        .iter()
        .map(|update| {
            (
                update.ref_name.clone(),
                update.old_oid.clone(),
                update.new_oid.clone(),
            )
        })
        .collect::<Vec<_>>();
    let mut git_fields = Vec::with_capacity(artifacts.packs.len() * 2);
    for pack in &artifacts.packs {
        git_fields.push(pack.pack_id.as_bytes().to_vec());
        git_fields.push(pack.object_count.to_le_bytes().to_vec());
    }
    let git_object_set_digest = digest_owned(b"crab protected git objects v1\0", &git_fields);
    let mut recipe_fields = Vec::new();
    for file in artifacts
        .prepared
        .content
        .iter()
        .flat_map(|content| content.files.iter())
    {
        recipe_fields.push(file.file_hash.to_vec());
        recipe_fields.push(file.size.to_le_bytes().to_vec());
    }
    let file_recipe_set_digest = digest_owned(b"crab protected file recipes v1\0", &recipe_fields);
    let xorb_proof_digest = digest(
        b"crab protected xorb proofs v1\0",
        artifacts
            .prepared
            .content
            .iter()
            .flat_map(|content| content.xorbs.iter())
            .flat_map(|xorb| [&xorb.protocol_hash[..], &xorb.body_hash[..]]),
    );
    let candidate_pack_index_hash = decode_hash(&candidate.pack_index_hash)?;
    let candidate_shard_index_hash = decode_hash(&candidate.shard_index_hash)?;
    let ref_edit_digest = protected_ref_edit_digest(&refs);
    let connectivity_digest = protected_connectivity_digest(
        &ref_updates
            .iter()
            .map(|update| update.new_oid.clone())
            .collect::<Vec<_>>(),
    );
    let shard_set_digest = committed_shard_set_digest(&artifacts.shards);
    let plan_digest = digest(
        b"crab protected dependency plan v1\0",
        [
            &ref_edit_digest[..],
            &git_object_set_digest[..],
            &file_recipe_set_digest[..],
            &xorb_proof_digest[..],
            &shard_set_digest[..],
            &candidate_pack_index_hash[..],
            &candidate_shard_index_hash[..],
            &connectivity_digest[..],
        ]
        .into_iter(),
    );
    let receipt = PushCommitReceipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        attempt_id: push_id.to_owned(),
        base_generation: artifacts.snapshot.manifest.generation,
        base_etag: Some(artifacts.snapshot.manifest_etag.clone()),
        ref_edit_digest,
        git_object_set_digest,
        file_recipe_set_digest,
        xorb_proof_digest,
        shard_set_digest,
        candidate_pack_index_hash,
        candidate_shard_index_hash,
        gc_registry_generation: 0,
        connectivity_digest,
        plan_digest,
    };
    receipt.validate_base(receipt.base_generation, receipt.base_etag.as_deref())?;
    let staged_objects = artifacts
        .prepared
        .layout
        .store()
        .flush_staged_writes(16)
        .await?;
    let plan = ProtectedPushPlan {
        schema_version: 1,
        mirror_plan_id: None,
        repo_prefix: artifacts.prepared.layout.repo_prefix().to_owned(),
        push_id: push_id.to_owned(),
        upload_prefix: upload_prefix.to_owned(),
        base_manifest_generation: Some(artifacts.snapshot.manifest.generation),
        base_manifest_etag: Some(artifacts.snapshot.manifest_etag.clone()),
        ref_updates,
        candidate_manifest: candidate,
        push_commit_receipt: Some(receipt),
        staged_objects,
    };
    let body = serde_json::to_vec_pretty(&plan).map_err(|source| {
        Error::Io(std::io::Error::other(format!(
            "protected plan serialization failed: {source}"
        )))
    })?;
    let path = ObjectPath::from(format!(
        "{}/push-plan.json",
        upload_prefix.trim_matches('/')
    ));
    artifacts
        .prepared
        .layout
        .store()
        .put_exact(&path, Bytes::from(body.clone()))
        .await?;
    artifacts
        .prepared
        .layout
        .store()
        .flush_staging_object(&path, body.len() as u64)
        .await?;
    Ok(plan)
}

fn digest<'a>(context: &[u8], fields: impl Iterator<Item = &'a [u8]>) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(context);
    for field in fields {
        hasher.update(&(field.len() as u64).to_le_bytes());
        hasher.update(field);
    }
    *hasher.finalize().as_bytes()
}

fn digest_owned(context: &[u8], fields: &[Vec<u8>]) -> [u8; 32] {
    digest(context, fields.iter().map(Vec::as_slice))
}

fn decode_hash(value: &str) -> Result<[u8; 32]> {
    if value.is_empty() {
        return Ok([0; 32]);
    }
    let hash = crab_xet::hash::MerkleHash::from_hex(value)
        .map_err(|_| Error::Request("candidate index hash is invalid"))?;
    Ok(hash.into())
}
