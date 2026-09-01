use std::collections::HashSet;
use std::time::Duration;

use crab_auth::{PushFinalizeResponse, PushRefUpdate};
use crab_coordination::{GcFenceHeartbeat, GcFenceLease};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::GitVisibilityPublication;
use super::git_workspace::verify_source_push;
use super::{
    MaterializedSourcePush, PreparedViewScope, ReceiveContext, ReceiveManifestCommit,
    build_service_candidate_manifest, commit_receive_manifest,
    commit_service_git_locators_from_verified_indexes, commit_service_metadata, conflict, invalid,
    parse_active_active_receive_config, promote_staged_objects,
    publish_materialized_git_visibility, source_ref_updates_from_prepare,
    validate_candidate_manifest_shape, validate_candidate_metadata,
    validate_protected_dependency_receipt, validate_protected_shard_set_receipt,
    validate_push_plan_shape, validate_staged_object_shapes,
    write_service_generation_index_receipt,
};
use crate::error::{AuthServerError, Result};

const VERIFIED_RECEIVE_SCHEMA_VERSION: u32 = 1;
const GC_FENCE_TTL: Duration = crab_coordination::DEFAULT_GC_FENCE_TTL;

struct ReceiveFenceLease {
    lease: GcFenceLease,
    heartbeat: GcFenceHeartbeat,
}

impl ReceiveFenceLease {
    async fn release(self) -> Result<()> {
        self.heartbeat.stop().await;
        self.lease.release().await.map_err(Into::into)
    }
}

struct ReceiveWriterFences {
    global: ReceiveFenceLease,
    repo: ReceiveFenceLease,
}

impl ReceiveWriterFences {
    async fn acquire(ctx: &ReceiveContext, cancel: &CancellationToken) -> Result<Self> {
        let global = GcFenceLease::acquire_writer(
            ctx.store().inner(),
            ctx.router().global_prefix(),
            GC_FENCE_TTL,
        )
        .await
        .map_err(AuthServerError::from)?;
        let global_heartbeat = GcFenceHeartbeat::spawn(&global, cancel.clone(), GC_FENCE_TTL / 3);
        let repo = match GcFenceLease::acquire_writer(
            ctx.store().inner(),
            ctx.repo_prefix(),
            GC_FENCE_TTL,
        )
        .await
        .map_err(AuthServerError::from)
        {
            Ok(repo) => repo,
            Err(error) => {
                global_heartbeat.stop().await;
                let _ = global.release().await;
                return Err(error);
            }
        };
        let repo_heartbeat = GcFenceHeartbeat::spawn(&repo, cancel.clone(), GC_FENCE_TTL / 3);
        Ok(Self {
            global: ReceiveFenceLease {
                lease: global,
                heartbeat: global_heartbeat,
            },
            repo: ReceiveFenceLease {
                lease: repo,
                heartbeat: repo_heartbeat,
            },
        })
    }

    async fn release(self) -> Result<()> {
        let repo_result = self.repo.release().await;
        let global_result = self.global.release().await;
        match repo_result {
            Err(error) => Err(error),
            Ok(()) => global_result,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct VerifiedReceiveEvidence {
    schema_version: u32,
    repo_prefix: String,
    push_id: String,
    source_plan_digest: String,
    prepare_digest: String,
    materialized: MaterializedSourcePush,
}

/// Prepared protected-push receive state returned to the helper.
pub struct PreparedReceive {
    pub source_generation: u64,
}

/// Verified protected-push receive state returned to the helper.
pub struct VerifiedReceive {
    pub ref_updates: Vec<PushRefUpdate>,
    pub verified_changed_paths: Vec<String>,
    pub plan_digest: String,
    pub verified_staged_bytes: u64,
}

fn prepare_digest(prepare: &super::PushPrepareRecord) -> Result<String> {
    let bytes = serde_json::to_vec(prepare)
        .map_err(|error| invalid(format!("prepare record serialize failed: {error}")))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

async fn write_verified_receive_evidence(
    ctx: &ReceiveContext,
    source_plan_digest: String,
    prepare: &super::PushPrepareRecord,
    materialized: MaterializedSourcePush,
) -> Result<String> {
    let evidence = VerifiedReceiveEvidence {
        schema_version: VERIFIED_RECEIVE_SCHEMA_VERSION,
        repo_prefix: ctx.repo_prefix().to_owned(),
        push_id: ctx.push_id().to_owned(),
        source_plan_digest,
        prepare_digest: prepare_digest(prepare)?,
        materialized,
    };
    let body = serde_json::to_vec(&evidence)
        .map_err(|error| invalid(format!("verified receive serialize failed: {error}")))?;
    let digest = blake3::hash(&body).to_hex().to_string();
    ctx.write_verified_receive(bytes::Bytes::from(body)).await?;
    Ok(digest)
}

async fn read_verified_receive_evidence(
    ctx: &ReceiveContext,
    expected_digest: &str,
    source_plan_digest: &str,
    prepare: &super::PushPrepareRecord,
) -> Result<MaterializedSourcePush> {
    super::validate_hash_component(expected_digest, "verified receive digest")?;
    let body = ctx.read_verified_receive().await?;
    if blake3::hash(&body).to_hex().as_str() != expected_digest {
        return Err(conflict(
            "verified receive evidence changed after authorization",
        ));
    }
    let evidence: VerifiedReceiveEvidence = serde_json::from_slice(&body)
        .map_err(|error| invalid(format!("invalid verified receive evidence: {error}")))?;
    if evidence.schema_version != VERIFIED_RECEIVE_SCHEMA_VERSION
        || evidence.repo_prefix != ctx.repo_prefix()
        || evidence.push_id != ctx.push_id()
    {
        return Err(conflict(
            "verified receive evidence belongs to another session",
        ));
    }
    if evidence.source_plan_digest != source_plan_digest
        || evidence.prepare_digest != prepare_digest(prepare)?
    {
        return Err(conflict(
            "source state changed after protected verification",
        ));
    }
    Ok(evidence.materialized)
}

/// Prepares a protected-push receive session after view authorization.
pub async fn prepare_receive(
    ctx: &ReceiveContext,
    ref_updates: Vec<PushRefUpdate>,
    view_scope: Option<PreparedViewScope>,
) -> Result<PreparedReceive> {
    ctx.validate_layout().await?;
    let record = ctx.write_prepare_record(ref_updates, view_scope).await?;
    Ok(PreparedReceive {
        source_generation: record.source_manifest_generation,
    })
}

/// Verifies a staged protected-push receive plan.
pub async fn verify_receive(ctx: &ReceiveContext) -> Result<VerifiedReceive> {
    ctx.validate_layout().await?;
    let plan = ctx.read_plan().await?;
    validate_push_plan_shape(&plan, ctx.repo_prefix(), ctx.push_id())?;
    validate_protected_dependency_receipt(&plan)?;
    let base = ctx.read_base_state().await?;
    let source_plan_digest = ctx.verified_plan_digest(&plan, &base)?;
    let prepare = ctx.read_prepare_record().await?;
    let source_ref_updates =
        source_ref_updates_from_prepare(&prepare, base.manifest(), &plan.ref_updates)?;
    validate_candidate_manifest_shape(&plan, ctx.repo_prefix())?;
    ctx.validate_staged_objects(&plan).await?;
    validate_candidate_metadata(ctx.store(), ctx.router(), &plan).await?;
    let (paths, materialized) = verify_source_push(
        ctx.store(),
        ctx.router(),
        ctx.repo_prefix(),
        Some(base.manifest()),
        &plan,
        &source_ref_updates,
        &prepare,
    )
    .await?;
    let verified_staged_bytes = plan
        .staged_objects
        .iter()
        .map(|object| object.size)
        .chain(materialized.packs.iter().map(|pack| pack.size))
        .try_fold(0u64, |total, size| total.checked_add(size))
        .ok_or_else(|| invalid("verified staged object bytes exceed the supported range"))?;
    let digest =
        write_verified_receive_evidence(ctx, source_plan_digest, &prepare, materialized).await?;
    Ok(VerifiedReceive {
        ref_updates: plan.ref_updates,
        verified_changed_paths: paths,
        plan_digest: digest,
        verified_staged_bytes,
    })
}

/// Commits a verified protected-push receive plan.
pub async fn commit_receive(
    ctx: &ReceiveContext,
    repo_url: &str,
    plan_digest: &str,
    active_active_json: Option<&str>,
) -> Result<PushFinalizeResponse> {
    ctx.validate_layout().await?;
    let operation_cancel = CancellationToken::new();
    let fences = ReceiveWriterFences::acquire(ctx, &operation_cancel).await?;
    let operation = tokio::select! {
        result = commit_receive_inner(ctx, repo_url, plan_digest, active_active_json) => result,
        _ = operation_cancel.cancelled() => {
            Err(conflict("GC writer fence lease lost during protected receive"))
        }
    };
    let release = fences.release().await;
    match (operation, release) {
        (Ok(response), Ok(())) => Ok(response),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

async fn commit_receive_inner(
    ctx: &ReceiveContext,
    repo_url: &str,
    plan_digest: &str,
    active_active_json: Option<&str>,
) -> Result<PushFinalizeResponse> {
    let active_active = parse_active_active_receive_config(active_active_json, repo_url)?;
    let plan = ctx.read_plan().await?;
    validate_push_plan_shape(&plan, ctx.repo_prefix(), ctx.push_id())?;
    validate_protected_dependency_receipt(&plan)?;
    let base = ctx.read_base_state().await?;
    let source_plan_digest = ctx.verified_plan_digest(&plan, &base)?;
    if super::validate_hash_component(plan_digest, "verified receive digest").is_err() {
        return Err(conflict("source manifest changed after verification"));
    }
    let prepare = ctx.read_prepare_record().await?;
    source_ref_updates_from_prepare(&prepare, base.manifest(), &plan.ref_updates)?;
    validate_candidate_manifest_shape(&plan, ctx.repo_prefix())?;
    // Promotion re-reads and hashes every staged body immediately before its
    // canonical write. Validate paths here, but do not download all candidate
    // packs once more before that integrity boundary.
    validate_staged_object_shapes(&plan, ctx.repo_prefix(), ctx.push_id())?;
    validate_candidate_metadata(ctx.store(), ctx.router(), &plan).await?;
    let materialized =
        read_verified_receive_evidence(ctx, plan_digest, &source_plan_digest, &prepare).await?;
    promote_staged_objects(ctx.store(), &plan).await?;
    validate_protected_shard_set_receipt(ctx.store(), ctx.router(), &plan).await?;
    for pack in &materialized.packs {
        crab_metadata::pack_origin::verify_pack_origin(
            ctx.store(),
            ctx.router().repo_prefix(),
            pack,
        )
        .await?;
    }
    let manifest = build_service_candidate_manifest(
        ctx.store(),
        ctx.router(),
        Some(base.manifest()),
        &plan,
        &materialized,
    )
    .await?;
    let visibility_publication = publish_materialized_git_visibility(
        ctx.store(),
        ctx.router(),
        &manifest,
        &materialized.git_visibility,
    )
    .await?;
    if let GitVisibilityPublication::CompletePackOnly { observed, maximum } = visibility_publication
    {
        tracing::warn!(
            generation = manifest.generation,
            proof_objects = observed,
            maximum,
            "protected push exceeds the Git visibility proof profile; complete-pack fetch remains available"
        );
    }
    let committed_shards = if manifest.shard_index_hash.is_empty() {
        Vec::new()
    } else {
        crab_metadata::manifest_store::read_bulk_shard_list(
            ctx.store(),
            ctx.router(),
            &manifest.shard_index_hash,
        )
        .await?
    };
    let gc_registry_generation = crab_metadata::ref_registry::union_register_repo_shards(
        ctx.store(),
        ctx.router(),
        committed_shards,
    )
    .await?;
    // Hydration metadata belongs to the candidate generation. Publish it
    // before refs so a successful receive can never expose an unreadable file.
    let committed_file_index_digest = commit_service_metadata(
        ctx.store(),
        ctx.router(),
        &plan,
        &manifest,
        gc_registry_generation,
    )
    .await?;
    let active_active_commit = commit_receive_manifest(
        ctx.store(),
        ctx.router(),
        ReceiveManifestCommit {
            repo_prefix: ctx.repo_prefix(),
            active_active: active_active.as_ref(),
            plan: &plan,
            materialized: &materialized,
            manifest: &manifest,
            base_etag: Some(base.etag()),
            visibility_proof_published: visibility_publication.is_published(),
        },
    )
    .await?;
    let file_index_digest = match crab_metadata::git_visibility::ensure_catalog_bound(
        ctx.store(),
        ctx.router(),
        &manifest,
    )
    .await
    {
        Ok(true) => Some(committed_file_index_digest),
        Ok(false) if !visibility_publication.is_published() => Some(committed_file_index_digest),
        Ok(false) => {
            tracing::warn!(
                generation = manifest.generation,
                "protected push committed; catalog visibility requires repair"
            );
            None
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                generation = manifest.generation,
                "protected push committed; catalog visibility publication failed"
            );
            None
        }
    };
    let git_object_locator_digest = match async {
        let committed_packs = if manifest.pack_index_hash.is_empty() {
            Vec::new()
        } else {
            crab_metadata::manifest_store::read_bulk_pack_list(
                ctx.store(),
                ctx.router(),
                &manifest.pack_index_hash,
            )
            .await?
        };
        let base_pack_ids = if !base.manifest().pack_index_hash.is_empty() {
            crab_metadata::manifest_store::read_bulk_pack_list(
                ctx.store(),
                ctx.router(),
                &base.manifest().pack_index_hash,
            )
            .await?
            .into_iter()
            .map(|pack| pack.pack_id)
            .collect::<HashSet<_>>()
        } else {
            HashSet::new()
        };
        let new_packs = committed_packs
            .into_iter()
            .filter(|pack| !base_pack_ids.contains(&pack.pack_id))
            .collect::<Vec<_>>();
        commit_service_git_locators_from_verified_indexes(
            ctx.store(),
            ctx.router(),
            &manifest,
            &new_packs,
        )
        .await
    }
    .await
    {
        Ok(digest) => Some(digest),
        Err(error) => {
            tracing::warn!(
                error = %error,
                generation = manifest.generation,
                "protected push committed; Git locator acceleration requires repair"
            );
            None
        }
    };
    if let (Some(file_index_digest), Some(git_object_locator_digest)) =
        (file_index_digest, git_object_locator_digest)
        && let Err(error) = write_service_generation_index_receipt(
            ctx.store(),
            ctx.router(),
            &manifest,
            file_index_digest,
            git_object_locator_digest,
        )
        .await
    {
        tracing::warn!(
            error = %error,
            generation = manifest.generation,
            "protected push committed; generation receipt requires repair"
        );
    }
    Ok(PushFinalizeResponse::updated_with_commit_outcome(
        materialized.ref_updates,
        active_active_commit.as_ref(),
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    use std::io::Write;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use bytes::Bytes;
    use crab_metadata::{
        manifest_store,
        manifests::{Manifest, PackManifestEntry},
        pack_metadata::PackMetadata,
        receipts::{PushCommitReceipt, RECEIPT_SCHEMA_VERSION},
        segmented::{self, SegmentIndex, SegmentKind},
    };
    use crab_storage::{StagedWrite, Store};
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;

    use super::*;
    use crate::error::AuthServerError;
    use crate::receive::{ProtectedPushPlan, install_base_packs, invalid, merkle_hash_from_hex};

    const PUSH_ID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    async fn context() -> Result<ReceiveContext> {
        let context = ReceiveContext::from_store(
            Store::new(Arc::new(InMemory::new())),
            "org/repo".to_owned(),
            PUSH_ID.to_owned(),
        );
        crab_metadata::layout_descriptor::ensure_canonical_layout(
            context.store(),
            context.router(),
        )
        .await?;
        manifest_store::create_manifest(
            context.store(),
            context.router(),
            &Manifest::default_for_repo("refs/heads/main"),
        )
        .await?;
        Ok(context)
    }

    fn oid(ch: char) -> String {
        std::iter::repeat_n(ch, 40).collect()
    }

    fn blake3_hex(bytes: &[u8]) -> String {
        blake3::hash(bytes).to_hex().to_string()
    }

    fn ref_update() -> PushRefUpdate {
        PushRefUpdate {
            ref_name: "refs/heads/main".to_owned(),
            old_oid: Some(oid('1')),
            new_oid: oid('2'),
        }
    }

    fn dependency_receipt(
        base_generation: u64,
        base_etag: Option<String>,
        candidate: &Manifest,
        updates: &[PushRefUpdate],
    ) -> PushCommitReceipt {
        let parse = |value: &str| {
            if value.is_empty() {
                [0; 32]
            } else {
                <[u8; 32]>::from(
                    crab_xet::hash::MerkleHash::from_hex(value).expect("candidate index hash"),
                )
            }
        };
        let protected_updates = updates
            .iter()
            .map(|update| {
                (
                    update.ref_name.clone(),
                    update.old_oid.clone(),
                    update.new_oid.clone(),
                )
            })
            .collect::<Vec<_>>();
        PushCommitReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            attempt_id: PUSH_ID.to_owned(),
            base_generation,
            base_etag,
            ref_edit_digest: crab_metadata::receipts::protected_ref_edit_digest(&protected_updates),
            git_object_set_digest: [2; 32],
            file_recipe_set_digest: [3; 32],
            xorb_proof_digest: [4; 32],
            shard_set_digest: crab_metadata::receipts::committed_shard_set_digest(&[]),
            candidate_pack_index_hash: parse(&candidate.pack_index_hash),
            candidate_shard_index_hash: parse(&candidate.shard_index_hash),
            gc_registry_generation: 0,
            connectivity_digest: crab_metadata::receipts::protected_connectivity_digest(
                &updates
                    .iter()
                    .map(|update| update.new_oid.clone())
                    .collect::<Vec<_>>(),
            ),
            plan_digest: [7; 32],
        }
    }

    fn push_plan() -> ProtectedPushPlan {
        let update = ref_update();
        let mut refs = BTreeMap::new();
        refs.insert(update.ref_name.clone(), update.new_oid.clone());
        let mut candidate_manifest = Manifest::default_for_repo(&update.ref_name);
        candidate_manifest.generation = 1;
        candidate_manifest.refs = refs;

        let push_commit_receipt =
            dependency_receipt(0, None, &candidate_manifest, std::slice::from_ref(&update));
        ProtectedPushPlan {
            schema_version: 1,
            repo_prefix: "org/repo".to_owned(),
            push_id: PUSH_ID.to_owned(),
            upload_prefix: format!("org/repo/staging/{PUSH_ID}/"),
            base_manifest_generation: None,
            base_manifest_etag: None,
            ref_updates: vec![update],
            candidate_manifest,
            push_commit_receipt: Some(push_commit_receipt),
            staged_objects: Vec::new(),
        }
    }

    async fn write_plan(ctx: &ReceiveContext, plan: &ProtectedPushPlan) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(plan)
            .map_err(|e| AuthServerError::Internal(format!("push-plan serialize: {e}")))?;
        ctx.store()
            .put_exact(
                &ObjectPath::from(format!(
                    "{}/staging/{}/push-plan.json",
                    ctx.repo_prefix(),
                    ctx.push_id()
                )),
                Bytes::from(bytes),
            )
            .await?;
        Ok(())
    }

    fn staged_object(canonical_key: String, bytes: &[u8]) -> StagedWrite {
        StagedWrite {
            staged_key: format!("org/repo/staging/{PUSH_ID}/objects/{canonical_key}"),
            canonical_key,
            blake3: blake3_hex(bytes),
            size: bytes.len() as u64,
        }
    }

    async fn put_staged(ctx: &ReceiveContext, object: &StagedWrite, bytes: Vec<u8>) -> Result<()> {
        ctx.store()
            .put_exact(
                &ObjectPath::from(object.staged_key.clone()),
                Bytes::from(bytes),
            )
            .await?;
        Ok(())
    }

    fn pack_entry_for_bytes(
        pack_bytes: &[u8],
        ref_tips: Vec<String>,
        object_count: u64,
    ) -> PackManifestEntry {
        let pack_id = blake3_hex(pack_bytes);
        PackManifestEntry {
            pack_id: pack_id.clone(),
            size: pack_bytes.len() as u64,
            content_hash: pack_id,
            ref_tips,
            object_count,
        }
    }

    fn run_git<const N: usize>(args: [&str; N], cwd: Option<&Path>) -> Result<()> {
        let mut command = Command::new("git");
        command
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let output = command.output()?;
        if output.status.success() {
            return Ok(());
        }
        Err(invalid(String::from_utf8_lossy(&output.stderr).trim()))
    }

    fn run_git_capture<const N: usize>(args: [&str; N]) -> Result<String> {
        let output = Command::new("git")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()?;
        if !output.status.success() {
            return Err(invalid(String::from_utf8_lossy(&output.stderr).trim()));
        }
        String::from_utf8(output.stdout).map_err(|_| invalid("git output was not valid UTF-8"))
    }

    fn git_capture_with_input<const N: usize>(
        args: [&str; N],
        cwd: &Path,
        input: &[u8],
    ) -> Result<Vec<u8>> {
        let mut child = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| AuthServerError::Internal(format!("spawn git failed: {e}")))?;
        child
            .stdin
            .as_mut()
            .ok_or_else(|| AuthServerError::Internal("git stdin missing".to_owned()))?
            .write_all(input)
            .map_err(|e| AuthServerError::Internal(format!("write git stdin failed: {e}")))?;
        let output = child
            .wait_with_output()
            .map_err(|e| AuthServerError::Internal(format!("git wait failed: {e}")))?;
        if !output.status.success() {
            return Err(AuthServerError::Internal(format!(
                "git failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        Ok(output.stdout)
    }

    fn git_object_count(repo: &Path, revs: &str) -> Result<u64> {
        let output =
            git_capture_with_input(["rev-list", "--objects", "--stdin"], repo, revs.as_bytes())?;
        let mut objects = BTreeSet::new();
        for line in String::from_utf8(output)
            .map_err(|_| invalid("test git output was not UTF-8"))?
            .lines()
        {
            if let Some(oid) = line.split_whitespace().next() {
                objects.insert(oid.to_owned());
            }
        }
        Ok(objects.len() as u64)
    }

    async fn put_pack_index(
        ctx: &ReceiveContext,
        generation: u64,
        pack: PackManifestEntry,
    ) -> Result<String> {
        let segment = segmented::build_segment(SegmentKind::Pack, generation, false, &[pack])?
            .ok_or_else(|| invalid("test pack segment missing"))?;
        ctx.store()
            .put_exact(
                &ctx.router().repo_path(&segment.reference.path),
                Bytes::from(segment.bytes),
            )
            .await?;
        let index = segmented::append_segment(SegmentIndex::default(), segment.reference);
        let index_object = segmented::build_index_object(SegmentKind::Pack, index)?;
        ctx.store()
            .put_exact(
                &ctx.router().repo_path(&segmented::index_relative_path(
                    SegmentKind::Pack,
                    &index_object.hash,
                )),
                Bytes::from(index_object.bytes),
            )
            .await?;
        Ok(index_object.hash)
    }

    async fn put_canonical_pack(
        ctx: &ReceiveContext,
        generation: u64,
        pack_bytes: Vec<u8>,
        ref_tip: String,
        object_count: u64,
    ) -> Result<String> {
        let pack = pack_entry_for_bytes(&pack_bytes, vec![ref_tip.clone()], object_count);
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source.pack");
        std::fs::write(&source, &pack_bytes)?;
        let installed = crab_git::pack::install_pack_file_from_path(
            &temp.path().join("objects/pack"),
            &source,
            &pack.pack_id,
            0,
            true,
        )?;
        ctx.store()
            .put_exact(
                &ctx.router().pack_path(&pack.pack_id),
                Bytes::from(pack_bytes),
            )
            .await?;
        ctx.store()
            .put_exact(
                &ctx.router().pack_index_path(&pack.pack_id),
                Bytes::from(std::fs::read(&installed.idx_path)?),
            )
            .await?;
        ctx.store()
            .put_exact(
                &ctx.router().pack_reverse_index_path(&pack.pack_id),
                Bytes::from(std::fs::read(&installed.rev_path)?),
            )
            .await?;
        let metadata = PackMetadata {
            pack_id: pack.pack_id.clone(),
            ref_tips: vec![ref_tip],
            object_count: pack.object_count,
        };
        let metadata_bytes = serde_json::to_vec(&metadata)
            .map_err(|e| AuthServerError::Internal(format!("test pack metadata: {e}")))?;
        ctx.store()
            .put_exact(
                &ctx.router().pack_metadata_path(&pack.pack_id),
                Bytes::from(metadata_bytes),
            )
            .await?;
        put_pack_index(ctx, generation, pack).await
    }

    async fn put_staged_pack_delta(
        ctx: &ReceiveContext,
        generation: u64,
        pack_bytes: Vec<u8>,
        ref_tip: String,
        object_count: u64,
        source_git_dir: &Path,
    ) -> Result<(String, Vec<StagedWrite>)> {
        let pack = pack_entry_for_bytes(&pack_bytes, vec![ref_tip.clone()], object_count);
        let segment =
            segmented::build_segment(SegmentKind::Pack, generation, false, &[pack.clone()])?
                .ok_or_else(|| invalid("test pack segment missing"))?;
        let index = segmented::append_segment(SegmentIndex::default(), segment.reference.clone());
        let index_object = segmented::build_index_object(SegmentKind::Pack, index)?;

        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source.pack");
        std::fs::write(&source, &pack_bytes)?;
        let installed = crab_git::pack::install_pack_file_from_path(
            &temp.path().join("objects/pack"),
            &source,
            &pack.pack_id,
            0,
            true,
        )?;
        let mut locations = crab_git::pack_locator::PackLocationIter::open(
            &installed.idx_path,
            &installed.rev_path,
            pack.size,
        )
        .map_err(crab_git::pack::PackError::from)?;
        let object_ids = locations
            .by_ref()
            .map(|location| {
                location
                    .map(|location| location.oid)
                    .map_err(crab_git::pack::PackError::from)
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(AuthServerError::from)?;
        let kinds = crab_git::object_kinds_from_git_dir(source_git_dir, &object_ids)
            .map_err(AuthServerError::from)?;
        let ordered_kinds = object_ids
            .iter()
            .map(|oid| {
                kinds
                    .get(oid)
                    .copied()
                    .ok_or_else(|| invalid("test pack kind metadata omitted an object"))
            })
            .collect::<Result<Vec<_>>>()?;
        let kind_metadata = crab_git::pack_locator::encode_pack_kind_metadata(
            locations.pack_checksum(),
            &ordered_kinds,
        )
        .map_err(crab_git::pack::PackError::from)?;

        let pack_key = format!("org/repo/packs/pack-{}.pack", pack.pack_id);
        let pack_object = staged_object(pack_key, &pack_bytes);
        put_staged(ctx, &pack_object, pack_bytes).await?;

        let metadata = PackMetadata {
            pack_id: pack.pack_id.clone(),
            ref_tips: vec![ref_tip],
            object_count: pack.object_count,
        };
        let metadata_bytes = serde_json::to_vec(&metadata)
            .map_err(|e| AuthServerError::Internal(format!("test pack metadata: {e}")))?;
        let metadata_key = format!("org/repo/packs/pack-{}.meta", pack.pack_id);
        let metadata_object = staged_object(metadata_key, &metadata_bytes);
        put_staged(ctx, &metadata_object, metadata_bytes).await?;

        let kind_metadata_key = format!("org/repo/packs/pack-{}.kinds", pack.pack_id);
        let kind_metadata_object = staged_object(kind_metadata_key, &kind_metadata);
        put_staged(ctx, &kind_metadata_object, kind_metadata).await?;

        let segment_key = ctx
            .router()
            .repo_path(&segment.reference.path)
            .as_ref()
            .to_owned();
        let segment_object = staged_object(segment_key, &segment.bytes);
        put_staged(ctx, &segment_object, segment.bytes).await?;

        let index_key = ctx
            .router()
            .repo_path(&segmented::index_relative_path(
                SegmentKind::Pack,
                &index_object.hash,
            ))
            .as_ref()
            .to_owned();
        let index_object_staged = staged_object(index_key, &index_object.bytes);
        put_staged(ctx, &index_object_staged, index_object.bytes).await?;

        Ok((
            index_object.hash,
            vec![
                segment_object,
                index_object_staged,
                pack_object,
                metadata_object,
                kind_metadata_object,
            ],
        ))
    }

    fn path_str(path: &Path) -> Result<&str> {
        path.to_str()
            .ok_or_else(|| invalid("temporary path is not valid UTF-8"))
    }

    #[tokio::test]
    async fn prepare_receive_writes_prepare_record() -> Result<()> {
        let ctx = context().await?;

        let prepared = prepare_receive(&ctx, vec![ref_update()], None).await?;

        assert_eq!(prepared.source_generation, 0);
        let record = ctx.read_prepare_record().await?;
        assert_eq!(record.source_manifest_generation, 0);
        assert_eq!(record.source_ref_updates[0].old_oid, None);
        Ok(())
    }

    #[tokio::test]
    async fn prepare_receive_rejects_layout_without_manifest() -> Result<()> {
        let ctx = ReceiveContext::from_store(
            Store::new(Arc::new(InMemory::new())),
            "org/repo".to_owned(),
            PUSH_ID.to_owned(),
        );
        crab_metadata::layout_descriptor::ensure_canonical_layout(ctx.store(), ctx.router())
            .await?;

        let error = match prepare_receive(&ctx, vec![ref_update()], None).await {
            Ok(_) => {
                return Err(invalid(
                    "protected push synthesized missing repository state",
                ));
            }
            Err(error) => error,
        };

        assert!(matches!(error, AuthServerError::CorruptObject { .. }));
        assert!(error.to_string().contains("crab init"));
        Ok(())
    }

    #[tokio::test]
    async fn commit_receive_rejects_stale_digest_before_prepare_record() -> Result<()> {
        let ctx = context().await?;
        write_plan(&ctx, &push_plan()).await?;

        let err = commit_receive(&ctx, "crab://bucket/org/repo", "stale-digest", None)
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            AuthServerError::CasConflict { path, expected_etag: None }
                if path == "source manifest changed after verification"
        ));
        Ok(())
    }

    #[tokio::test]
    async fn verified_receive_evidence_rejects_replacement() -> Result<()> {
        let ctx = context().await?;
        let prepare = ctx.write_prepare_record(vec![ref_update()], None).await?;
        let materialized = MaterializedSourcePush {
            ref_updates: vec![ref_update()],
            packs: Vec::new(),
            peeled_refs: BTreeMap::new(),
            git_visibility: super::super::MaterializedGitVisibility::Exact(BTreeMap::new()),
        };
        let source_plan_digest = blake3_hex(b"source plan");
        let digest = write_verified_receive_evidence(
            &ctx,
            source_plan_digest.clone(),
            &prepare,
            materialized,
        )
        .await?;
        ctx.store()
            .put_overwrite(
                &ObjectPath::from(format!(
                    "{}/protected-push-sessions/{PUSH_ID}.verified.json",
                    ctx.repo_prefix()
                )),
                Bytes::from_static(b"replaced"),
            )
            .await?;

        let error = read_verified_receive_evidence(&ctx, &digest, &source_plan_digest, &prepare)
            .await
            .expect_err("replacement evidence must not authorize commit");

        assert!(matches!(error, AuthServerError::CasConflict { .. }));
        Ok(())
    }

    #[tokio::test]
    async fn commit_rejects_unreferenced_staged_object_before_body_download() -> Result<()> {
        let ctx = context().await?;
        let mut plan = push_plan();
        plan.candidate_manifest.seal_git_validation();
        let missing_body = b"candidate pack body";
        let pack_id = blake3_hex(missing_body);
        plan.staged_objects = vec![staged_object(
            format!("org/repo/packs/pack-{pack_id}.pack"),
            missing_body,
        )];
        write_plan(&ctx, &plan).await?;
        prepare_receive(&ctx, plan.ref_updates.clone(), None).await?;
        let base = ctx.read_base_state().await?;
        let digest = ctx.verified_plan_digest(&plan, &base)?;

        let error = commit_receive(&ctx, "crab://bucket/org/repo", &digest, None)
            .await
            .expect_err("unreferenced staged object must be rejected");

        assert!(
            error
                .to_string()
                .contains("staged object is not referenced by candidate metadata"),
            "finalization downloaded the missing body before rejecting its metadata: {error}",
        );
        Ok(())
    }

    #[tokio::test]
    async fn git_object_locator_filtered_view_push_preserves_hidden_paths() -> Result<()> {
        let ctx = context().await?;
        let temp = tempfile::tempdir()?;

        let source_repo = temp.path().join("source");
        run_git(["init", path_str(&source_repo)?], None)?;
        run_git(
            ["config", "user.email", "alice@example.com"],
            Some(&source_repo),
        )?;
        run_git(["config", "user.name", "Alice"], Some(&source_repo))?;
        std::fs::create_dir_all(source_repo.join("src"))?;
        std::fs::create_dir_all(source_repo.join("secret"))?;
        std::fs::write(source_repo.join(".gitattributes"), b"*.txt text\n")?;
        std::fs::write(source_repo.join("src/app.txt"), b"allowed v1\n")?;
        std::fs::write(source_repo.join("secret/key.txt"), b"classified\n")?;
        run_git(
            ["add", ".gitattributes", "src/app.txt", "secret/key.txt"],
            Some(&source_repo),
        )?;
        run_git(["commit", "-m", "source base"], Some(&source_repo))?;
        let source_old = run_git_capture(["-C", path_str(&source_repo)?, "rev-parse", "HEAD"])?
            .trim()
            .to_owned();
        let source_pack_bytes = git_capture_with_input(
            ["pack-objects", "--stdout", "--revs"],
            &source_repo,
            format!("{source_old}\n").as_bytes(),
        )?;
        let source_pack_id = blake3_hex(&source_pack_bytes);
        let source_object_count = git_object_count(&source_repo, &format!("{source_old}\n"))?;
        let source_pack_index = put_canonical_pack(
            &ctx,
            4,
            source_pack_bytes,
            source_old.clone(),
            source_object_count,
        )
        .await?;

        let mut base = Manifest::default_for_repo("refs/heads/main");
        base.generation = 4;
        base.refs
            .insert("refs/heads/main".to_owned(), source_old.clone());
        base.pack_index_hash = source_pack_index;
        base.seal_git_validation();
        let (_, initial_etag) = manifest_store::read_manifest(ctx.store(), ctx.router()).await?;
        manifest_store::write_manifest_cas(ctx.store(), ctx.router(), &base, &initial_etag).await?;

        let view_repo = temp.path().join("view");
        run_git(["init", path_str(&view_repo)?], None)?;
        run_git(
            ["config", "user.email", "alice@example.com"],
            Some(&view_repo),
        )?;
        run_git(["config", "user.name", "Alice"], Some(&view_repo))?;
        std::fs::create_dir_all(view_repo.join("src"))?;
        std::fs::write(view_repo.join("src/app.txt"), b"allowed v1\n")?;
        run_git(["add", "src/app.txt"], Some(&view_repo))?;
        run_git(["commit", "-m", "filtered base"], Some(&view_repo))?;
        let view_old = run_git_capture(["-C", path_str(&view_repo)?, "rev-parse", "HEAD"])?
            .trim()
            .to_owned();
        std::fs::write(view_repo.join("src/app.txt"), b"allowed v2\n")?;
        run_git(["add", "src/app.txt"], Some(&view_repo))?;
        run_git(["commit", "-m", "allowed update"], Some(&view_repo))?;
        let view_new = run_git_capture(["-C", path_str(&view_repo)?, "rev-parse", "HEAD"])?
            .trim()
            .to_owned();
        let view_pack_bytes = git_capture_with_input(
            ["pack-objects", "--stdout", "--revs"],
            &view_repo,
            format!("{view_new}\n").as_bytes(),
        )?;
        let view_object_count = git_object_count(&view_repo, &format!("{view_new}\n"))?;
        let (view_pack_index, staged_objects) = put_staged_pack_delta(
            &ctx,
            5,
            view_pack_bytes,
            view_new.clone(),
            view_object_count,
            &view_repo.join(".git"),
        )
        .await?;

        let mut candidate = Manifest::default_for_repo("refs/heads/main");
        candidate.generation = 5;
        candidate
            .refs
            .insert("refs/heads/main".to_owned(), view_new.clone());
        candidate.pack_index_hash = view_pack_index;
        candidate.seal_git_validation();
        let update = PushRefUpdate {
            ref_name: "refs/heads/main".to_owned(),
            old_oid: Some(view_old),
            new_oid: view_new.clone(),
        };
        let push_commit_receipt =
            dependency_receipt(4, None, &candidate, std::slice::from_ref(&update));
        let plan = ProtectedPushPlan {
            schema_version: 1,
            repo_prefix: "org/repo".to_owned(),
            push_id: PUSH_ID.to_owned(),
            upload_prefix: format!("org/repo/staging/{PUSH_ID}/"),
            base_manifest_generation: Some(4),
            base_manifest_etag: None,
            ref_updates: vec![update],
            candidate_manifest: candidate,
            push_commit_receipt: Some(push_commit_receipt),
            staged_objects,
        };
        write_plan(&ctx, &plan).await?;
        prepare_receive(&ctx, plan.ref_updates.clone(), None).await?;

        let verified = verify_receive(&ctx).await?;
        assert_eq!(
            verified.verified_changed_paths,
            vec!["src/app.txt".to_owned()]
        );
        let retried = verify_receive(&ctx).await?;
        assert_eq!(retried.plan_digest, verified.plan_digest);

        let response =
            commit_receive(&ctx, "crab://bucket/org/repo", &verified.plan_digest, None).await?;
        let final_update = response
            .ref_updates
            .first()
            .ok_or_else(|| invalid("test materialization returned no ref update"))?;
        assert_ne!(final_update.new_oid, view_new);

        let final_state = ctx.read_base_state().await?;
        let final_oid = final_state
            .manifest()
            .refs
            .get("refs/heads/main")
            .ok_or_else(|| invalid("test final ref missing"))?;
        assert_eq!(final_oid, &final_update.new_oid);
        let visibility = crab_metadata::git_visibility::read(
            ctx.store(),
            ctx.router(),
            final_state.manifest().generation,
            &final_state.manifest().pack_index_hash,
            &final_state.manifest().git_validation_digest,
        )
        .await?;
        assert!(
            visibility.contains_hex_in_ref("refs/heads/main", final_oid),
            "the visibility proof must be durable when the protected ref commits"
        );

        let committed_packs = manifest_store::read_bulk_pack_list(
            ctx.store(),
            ctx.router(),
            &final_state.manifest().pack_index_hash,
        )
        .await?;
        for pack in &committed_packs {
            ctx.store()
                .head(&ctx.router().pack_index_path(&pack.pack_id))
                .await?;
            ctx.store()
                .head(&ctx.router().pack_reverse_index_path(&pack.pack_id))
                .await?;
        }
        for pack in committed_packs
            .iter()
            .filter(|pack| pack.pack_id != source_pack_id)
        {
            let idx_size = ctx
                .store()
                .head(&ctx.router().pack_index_path(&pack.pack_id))
                .await?
                .size;
            let rev_size = ctx
                .store()
                .head(&ctx.router().pack_reverse_index_path(&pack.pack_id))
                .await?
                .size;
            let kind_metadata_size = ctx
                .store()
                .head(&ctx.router().pack_kind_metadata_path(&pack.pack_id))
                .await?
                .size;
            let read_bytes = Arc::new(AtomicU64::new(0));
            let observer = Arc::clone(&read_bytes);
            let observed_store = Store::new(Arc::clone(ctx.store().inner()))
                .with_read_byte_observer(Arc::new(move |bytes| {
                    observer.fetch_add(bytes, Ordering::Relaxed);
                }));
            let observed_router = crab_storage::StoreLayout::new(
                observed_store.clone(),
                ctx.repo_prefix().to_owned(),
            );

            super::super::download_service_locator_evidence(
                &observed_store,
                &observed_router,
                pack,
            )
            .await?;

            assert_eq!(
                read_bytes.load(Ordering::Relaxed),
                20 + idx_size + rev_size + kind_metadata_size,
                "verified locator publication must not stream the pack body",
            );
        }
        let committed_pack_inventory = committed_packs
            .iter()
            .map(|pack| {
                let pack_id = merkle_hash_from_hex(&pack.pack_id, "test committed pack id")?;
                Ok((
                    pack_id,
                    crab_metadata::git_object_locator::GitPackInventoryEntry {
                        pack_id,
                        object_count: pack.object_count,
                        pack_size: pack.size,
                    },
                ))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        let oid = gix_hash::ObjectId::from_hex(final_oid.as_bytes())
            .map_err(|error| invalid(format!("test final oid decode: {error}")))?;
        let oid: [u8; 20] = oid
            .as_bytes()
            .try_into()
            .map_err(|_| invalid("test final oid was not SHA-1"))?;
        let locator_session = crab_metadata::git_object_locator::GitObjectLocatorSession::open(
            Arc::clone(ctx.store().inner()),
            ctx.router().repo_prefix(),
        )
        .await?;
        let locator = locator_session
            .lookup_batch(&[oid], &committed_pack_inventory)
            .await;
        locator_session.close().await?;
        let locator = locator?;
        assert!(
            matches!(
                locator.as_slice(),
                [crab_metadata::git_object_locator::GitObjectLookup::Hit(_)]
            ),
            "final commit locator was {locator:?}"
        );

        let git_dir = temp.path().join("final.git");
        run_git(["init", "--bare", path_str(&git_dir)?], None)?;
        install_base_packs(ctx.store(), ctx.router(), &git_dir).await?;
        let src_spec = format!("{final_oid}:src/app.txt");
        let secret_spec = format!("{final_oid}:secret/key.txt");
        let src = run_git_capture(["--git-dir", path_str(&git_dir)?, "show", &src_spec])?;
        let secret = run_git_capture(["--git-dir", path_str(&git_dir)?, "show", &secret_spec])?;

        assert_eq!(src, "allowed v2\n");
        assert_eq!(secret, "classified\n");
        Ok(())
    }
}
