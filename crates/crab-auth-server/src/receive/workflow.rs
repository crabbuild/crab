use std::time::Duration;

use crab_auth::{PushFinalizeResponse, PushRefUpdate};
use crab_coordination::{GcFenceHeartbeat, GcFenceLease};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::capsule::{commit_capsule_candidate, verify_capsule_candidate};
use super::{
    PreparedViewScope, ReceiveContext, conflict, invalid, parse_active_active_receive_config,
};
use crate::error::{AuthServerError, Result};

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
struct VerifiedCapsuleReceiveEvidence {
    schema_version: u32,
    repo_prefix: String,
    push_id: String,
    source_plan_digest: String,
    prepare_digest: String,
    base_root_digest: String,
    transaction_id: String,
    run_hash: String,
    changed_paths: Vec<String>,
    replication_objects: Vec<String>,
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

async fn write_verified_capsule_receive_evidence(
    ctx: &ReceiveContext,
    plan: &super::ProtectedCapsulePushPlan,
    prepare: &super::PushPrepareRecord,
    changed_paths: Vec<String>,
    replication_objects: Vec<String>,
) -> Result<String> {
    let source_plan_digest = blake3::hash(
        &serde_json::to_vec(plan)
            .map_err(|error| invalid(format!("capsule push-plan serialize failed: {error}")))?,
    )
    .to_hex()
    .to_string();
    let evidence = VerifiedCapsuleReceiveEvidence {
        schema_version: 2,
        repo_prefix: ctx.repo_prefix().to_owned(),
        push_id: ctx.push_id().to_owned(),
        source_plan_digest,
        prepare_digest: prepare_digest(prepare)?,
        base_root_digest: plan.base_root_digest.clone(),
        transaction_id: plan.transaction_id.clone(),
        run_hash: plan.run_hash.clone(),
        changed_paths,
        replication_objects,
    };
    let body = serde_json::to_vec(&evidence).map_err(|error| {
        invalid(format!(
            "verified capsule receive serialize failed: {error}"
        ))
    })?;
    let digest = blake3::hash(&body).to_hex().to_string();
    ctx.write_verified_receive(bytes::Bytes::from(body)).await?;
    Ok(digest)
}

async fn read_verified_capsule_receive_evidence(
    ctx: &ReceiveContext,
    expected_digest: &str,
    plan: &super::ProtectedCapsulePushPlan,
    prepare: &super::PushPrepareRecord,
) -> Result<VerifiedCapsuleReceiveEvidence> {
    super::validate_hash_component(expected_digest, "verified receive digest")?;
    let body = ctx.read_verified_receive().await?;
    if blake3::hash(&body).to_hex().as_str() != expected_digest {
        return Err(conflict(
            "verified capsule receive evidence changed after authorization",
        ));
    }
    let evidence: VerifiedCapsuleReceiveEvidence =
        serde_json::from_slice(&body).map_err(|error| {
            invalid(format!(
                "invalid verified capsule receive evidence: {error}"
            ))
        })?;
    let source_plan_digest = blake3::hash(
        &serde_json::to_vec(plan)
            .map_err(|error| invalid(format!("capsule push-plan serialize failed: {error}")))?,
    )
    .to_hex()
    .to_string();
    if evidence.schema_version != 2
        || evidence.repo_prefix != ctx.repo_prefix()
        || evidence.push_id != ctx.push_id()
        || evidence.source_plan_digest != source_plan_digest
        || evidence.prepare_digest != prepare_digest(prepare)?
        || evidence.base_root_digest != plan.base_root_digest
        || evidence.transaction_id != plan.transaction_id
        || evidence.run_hash != plan.run_hash
    {
        return Err(conflict(
            "capsule source state changed after protected verification",
        ));
    }
    Ok(evidence)
}

/// Prepares a protected-push receive session after view authorization.
pub async fn prepare_receive(
    ctx: &ReceiveContext,
    ref_updates: Vec<PushRefUpdate>,
    view_scope: Option<PreparedViewScope>,
) -> Result<PreparedReceive> {
    ctx.validate_root().await?;
    let record = ctx.write_prepare_record(ref_updates, view_scope).await?;
    Ok(PreparedReceive {
        source_generation: record.source_manifest_generation,
    })
}

/// Verifies a staged protected-push receive plan.
pub async fn verify_receive(ctx: &ReceiveContext) -> Result<VerifiedReceive> {
    ctx.validate_root().await?;
    let plan = ctx.read_plan_document().await?;
    let verified = verify_capsule_candidate(ctx, &plan).await?;
    let digest = write_verified_capsule_receive_evidence(
        ctx,
        &plan,
        &verified.prepare,
        verified.changed_paths.clone(),
        verified.replication_objects,
    )
    .await?;
    Ok(VerifiedReceive {
        ref_updates: plan.ref_updates,
        verified_changed_paths: verified.changed_paths,
        plan_digest: digest,
        verified_staged_bytes: verified.staged_bytes,
    })
}

/// Commits a verified protected-push receive plan.
pub async fn commit_receive(
    ctx: &ReceiveContext,
    repo_url: &str,
    plan_digest: &str,
    active_active_json: Option<&str>,
) -> Result<PushFinalizeResponse> {
    ctx.validate_root().await?;
    // Unsupported plans cannot publish; reject them before acquiring writer
    // fences so stale protocol traffic cannot mutate coordination state.
    let plan = ctx.read_plan_document().await?;
    super::validate_protected_capsule_plan_shape(&plan, ctx.repo_prefix(), ctx.push_id())?;
    let active_active = parse_active_active_receive_config(active_active_json, repo_url)?;
    let operation_cancel = CancellationToken::new();
    let fences = ReceiveWriterFences::acquire(ctx, &operation_cancel).await?;
    // The operation owns ref leases and possibly in-flight publication. Fence
    // loss requests cancellation, but cleanup and commit resolution must drain
    // before either GC fence is released.
    let operation = commit_capsule_receive(
        ctx,
        plan,
        plan_digest,
        active_active.as_ref(),
        &operation_cancel,
    )
    .await;
    let release = fences.release().await;
    match (operation, release) {
        (Ok(response), release) => {
            if let Err(error) = release {
                tracing::warn!(%error, "protected receive committed; fence cleanup requires repair");
            }
            Ok(response)
        }
        (Err(error), _) => Err(error),
    }
}

pub(super) fn check_receive_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(crab_write::WriteError::Cancelled.into());
    }
    Ok(())
}

async fn commit_capsule_receive(
    ctx: &ReceiveContext,
    plan: super::ProtectedCapsulePushPlan,
    plan_digest: &str,
    active_active: Option<&super::ActiveActiveReceiveConfig>,
    cancel: &CancellationToken,
) -> Result<PushFinalizeResponse> {
    check_receive_cancelled(cancel)?;
    let prepare = ctx.read_prepare_record().await?;
    let evidence =
        read_verified_capsule_receive_evidence(ctx, plan_digest, &plan, &prepare).await?;
    let outcome = commit_capsule_candidate(
        ctx,
        &plan,
        active_active,
        &evidence.replication_objects,
        cancel,
    )
    .await?;
    Ok(PushFinalizeResponse::updated_with_commit_outcome(
        plan.ref_updates,
        outcome.as_ref(),
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::io::Write;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use bytes::Bytes;
    use crab_metadata::{
        capsule_protocol::{
            Capsule, CapsuleGitPack, CapsuleRefEdit, CapsuleRun, CapsuleSection,
            CapsuleSectionKind, CapsuleTransaction, CapsuleVisibilityDelta, FileCatalogEntry,
            PointerCatalog, RepositoryRoot, RootRecord, ShardCatalogEntry, XorbCatalogEntry,
            XorbChunkEntry,
        },
        git_visibility::GitVisibilityEdit,
    };
    use crab_storage::{StagedWrite, Store};
    use crab_types::pointer::Pointer;
    use crab_xet::chunker::GearChunker;
    use crab_xet::hash::MerkleHash;
    use crab_xet::reconstruction::ChunkPlacementMap;
    use crab_xet::shard::{PushShardSession, file_info_from_placements, xorb_info_from_placements};
    use crab_xet::xorb::builder::{RunId, XorbBuilder};
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::error::AuthServerError;
    use crate::receive::invalid;

    const PUSH_ID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    async fn capsule_context() -> Result<ReceiveContext> {
        let context = ReceiveContext::from_store(
            Store::new(Arc::new(InMemory::new())),
            "org/repo".to_owned(),
            PUSH_ID.to_owned(),
        );
        let root =
            RootRecord::encode(RepositoryRoot::initial(&"a".repeat(64), "refs/heads/main")?)?;
        crab_metadata::capsule_protocol::create_root(context.router(), root).await?;
        Ok(context)
    }

    fn oid(ch: char) -> String {
        std::iter::repeat_n(ch, 40).collect()
    }

    fn blake3_hex(bytes: &[u8]) -> String {
        blake3::hash(bytes).to_hex().to_string()
    }

    struct XetFixture {
        pointer: Pointer,
        catalog: PointerCatalog,
        xorb_hash: MerkleHash,
        xorb_bytes: Bytes,
        shard_hash: MerkleHash,
        shard_bytes: Vec<u8>,
    }

    fn xet_fixture(content: &[u8]) -> Result<XetFixture> {
        let file_hash = MerkleHash::from(*blake3::hash(content).as_bytes());
        let mut chunker = GearChunker::new();
        let mut chunks = chunker.feed(content);
        if let Some(chunk) = chunker.finalize() {
            chunks.push(chunk);
        }
        let chunk_hashes = chunks.iter().map(|chunk| chunk.hash).collect::<Vec<_>>();
        let mut builder = XorbBuilder::new();
        for chunk in &chunks {
            builder.push(chunk, RunId(0))?;
        }
        let mut xorbs = builder.finalize()?;
        if xorbs.len() != 1 {
            return Err(invalid("test Xet fixture did not produce exactly one xorb"));
        }
        let xorb = xorbs.remove(0);
        let placements = xorb
            .placements
            .iter()
            .map(|placement| (placement.chunk_hash, placement.clone()))
            .collect::<ChunkPlacementMap>();
        let xorb_info = Arc::new(xorb_info_from_placements(xorb.hash, &xorb.placements)?);
        let file_info = file_info_from_placements(file_hash, &chunk_hashes, &placements)?;
        let mut shards = PushShardSession::new();
        shards.add_file_bundle(file_info, std::slice::from_ref(&xorb_info))?;
        let mut shards = shards.finalize()?;
        if shards.len() != 1 {
            return Err(invalid(
                "test Xet fixture did not produce exactly one shard",
            ));
        }
        let (shard_bytes, shard_hash) = shards.remove(0);
        let mut catalog = PointerCatalog::new();
        let mut catalog_chunks = xorb.placements.iter().collect::<Vec<_>>();
        catalog_chunks.sort_by_key(|placement| placement.chunk_index);
        catalog.insert_xorb(
            xorb.hash.hex(),
            XorbCatalogEntry::new(
                xorb.bytes.len() as u64,
                blake3_hex(&xorb.bytes),
                catalog_chunks
                    .into_iter()
                    .map(|placement| {
                        XorbChunkEntry::new(placement.chunk_hash.hex(), placement.uncompressed_size)
                    })
                    .collect(),
            ),
        )?;
        catalog.insert_shard(
            shard_hash.hex(),
            ShardCatalogEntry::new(shard_bytes.len() as u64, vec![xorb.hash.hex()]),
        )?;
        catalog.insert_file(
            file_hash.hex(),
            FileCatalogEntry::new(content.len() as u64, shard_hash.hex()),
        )?;
        catalog.encode()?;
        Ok(XetFixture {
            pointer: Pointer {
                file_hash: file_hash.into(),
                size: content.len() as u64,
                shard_hint: None,
            },
            catalog,
            xorb_hash: xorb.hash,
            xorb_bytes: xorb.bytes,
            shard_hash,
            shard_bytes,
        })
    }

    fn ref_update() -> PushRefUpdate {
        PushRefUpdate {
            ref_name: "refs/heads/main".to_owned(),
            old_oid: Some(oid('1')),
            new_oid: oid('2'),
        }
    }

    async fn write_plan(ctx: &ReceiveContext, plan: &impl Serialize) -> Result<()> {
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

    fn visibility_for_tip(
        repo: &Path,
        ref_name: &str,
        old_oid: Option<String>,
        new_oid: &str,
        revs: &str,
    ) -> Result<CapsuleVisibilityDelta> {
        let output =
            git_capture_with_input(["rev-list", "--objects", "--stdin"], repo, revs.as_bytes())?;
        let mut objects = String::from_utf8(output)
            .map_err(|_| invalid("test Git object list was not UTF-8"))?
            .lines()
            .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
            .collect::<Vec<_>>();
        objects.sort_unstable();
        objects.dedup();
        Ok(CapsuleVisibilityDelta::new(BTreeMap::from([(
            ref_name.to_owned(),
            GitVisibilityEdit::from_replacement_objects(old_oid, new_oid.to_owned(), objects),
        )]))?)
    }

    fn capsule_git_pack(
        source_git_dir: &Path,
        pack_bytes: &[u8],
        object_count: u64,
    ) -> Result<CapsuleGitPack> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source.pack");
        std::fs::write(&source, pack_bytes)?;
        let pack_id = blake3_hex(pack_bytes);
        let installed = crab_git::pack::install_pack_file_from_path(
            &temp.path().join("objects/pack"),
            &source,
            &pack_id,
            0,
            true,
        )?;
        let mut locations = crab_git::pack_locator::PackLocationIter::open(
            &installed.idx_path,
            &installed.rev_path,
            pack_bytes.len() as u64,
        )
        .map_err(crab_git::pack::PackError::from)?;
        let object_ids = locations
            .by_ref()
            .map(|location| location.map(|location| location.oid))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(crab_git::pack::PackError::from)?;
        let kinds = crab_git::object_kinds_from_git_dir(source_git_dir, &object_ids)?;
        let ordered_kinds = object_ids
            .iter()
            .map(|oid| {
                kinds
                    .get(oid)
                    .copied()
                    .ok_or_else(|| invalid("test capsule pack omitted an object kind"))
            })
            .collect::<Result<Vec<_>>>()?;
        let locator = crab_git::pack_locator::encode_pack_kind_metadata(
            locations.pack_checksum(),
            &ordered_kinds,
        )
        .map_err(crab_git::pack::PackError::from)?;
        Ok(CapsuleGitPack::new(
            Bytes::copy_from_slice(pack_bytes),
            Bytes::from(std::fs::read(installed.idx_path)?),
            Bytes::from(std::fs::read(installed.rev_path)?),
            Bytes::from(locator),
            locations.pack_checksum().to_string(),
            object_count,
        )?)
    }

    fn path_str(path: &Path) -> Result<&str> {
        path.to_str()
            .ok_or_else(|| invalid("temporary path is not valid UTF-8"))
    }

    #[tokio::test]
    async fn prepare_receive_writes_prepare_record() -> Result<()> {
        let ctx = capsule_context().await?;

        let prepared = prepare_receive(&ctx, vec![ref_update()], None).await?;

        assert_eq!(prepared.source_generation, 0);
        let record = ctx.read_prepare_record().await?;
        assert_eq!(record.source_manifest_generation, 0);
        assert_eq!(record.source_ref_updates[0].old_oid, None);
        Ok(())
    }

    #[tokio::test]
    async fn protected_capsule_receive_verifies_commits_and_retries() -> Result<()> {
        let ctx = capsule_context().await?;
        let source = tempfile::tempdir()?;
        run_git(["init", "--initial-branch=main"], Some(source.path()))?;
        run_git(
            ["config", "user.email", "alice@example.com"],
            Some(source.path()),
        )?;
        run_git(["config", "user.name", "Alice"], Some(source.path()))?;
        let xet_content = b"unrestricted protected Xet content\n";
        let xet = xet_fixture(xet_content)?;
        let lfs_content = b"unrestricted protected LFS content\n";
        let lfs_pointer = crab_git::LfsPointer {
            oid: sha2::Sha256::digest(lfs_content).into(),
            size: lfs_content.len() as u64,
            extensions: Vec::new(),
        };
        std::fs::write(source.path().join("asset.lfs"), lfs_pointer.serialize())?;
        std::fs::write(source.path().join("tracked.txt"), b"protected capsule\n")?;
        std::fs::write(source.path().join("model.bin"), xet.pointer.serialize())?;
        run_git(
            ["add", "tracked.txt", "model.bin", "asset.lfs"],
            Some(source.path()),
        )?;
        run_git(["commit", "-m", "capsule candidate"], Some(source.path()))?;
        let tip = run_git_capture(["-C", path_str(source.path())?, "rev-parse", "HEAD"])?
            .trim()
            .to_owned();
        let pack_bytes = git_capture_with_input(
            ["pack-objects", "--stdout", "--revs"],
            source.path(),
            format!("{tip}\n").as_bytes(),
        )?;
        let object_count = git_object_count(source.path(), &format!("{tip}\n"))?;
        let pack = capsule_git_pack(&source.path().join(".git"), &pack_bytes, object_count)?;
        let root = crab_metadata::capsule_protocol::load_root(ctx.router()).await?;
        let update = PushRefUpdate {
            ref_name: "refs/heads/main".to_owned(),
            old_oid: None,
            new_oid: tip.clone(),
        };
        prepare_receive(&ctx, vec![update.clone()], None).await?;
        let plan_id = "c".repeat(64);
        let transaction = CapsuleTransaction::for_plan(
            root.record().digest(),
            &plan_id,
            vec![CapsuleRefEdit::new(
                update.ref_name.clone(),
                None,
                Some(tip.clone()),
                None,
            )],
        )?;
        let object_ids = git_capture_with_input(
            ["rev-list", "--objects", "--stdin"],
            source.path(),
            format!("{tip}\n").as_bytes(),
        )?;
        let mut object_ids = String::from_utf8(object_ids)
            .map_err(|_| invalid("test Git object list was not UTF-8"))?
            .lines()
            .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
            .collect::<Vec<_>>();
        object_ids.sort_unstable();
        object_ids.dedup();
        let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
            update.ref_name.clone(),
            GitVisibilityEdit::from_replacement_objects(None, tip.clone(), object_ids),
        )]))?;
        let capsule = Capsule::build(
            &transaction,
            vec![pack],
            vec![
                CapsuleSection::new(CapsuleSectionKind::VisibilityDelta, visibility.encode()?),
                CapsuleSection::new(
                    CapsuleSectionKind::CatalogDelta,
                    xet.catalog.encode_delta()?,
                ),
            ],
        )?;
        let run = CapsuleRun::leaf(capsule)?;
        let run_key = ctx.router().capsule_path(run.hash()).to_string();
        let run_object = staged_object(run_key, run.bytes());
        put_staged(&ctx, &run_object, run.bytes().to_vec()).await?;
        let xorb_object = staged_object(
            ctx.router().xorb_path(&xet.xorb_hash).to_string(),
            &xet.xorb_bytes,
        );
        put_staged(&ctx, &xorb_object, xet.xorb_bytes.to_vec()).await?;
        let shard_object = staged_object(
            ctx.router().shard_path(&xet.shard_hash).to_string(),
            &xet.shard_bytes,
        );
        put_staged(&ctx, &shard_object, xet.shard_bytes.clone()).await?;
        let upload_prefix = format!("{}/staging/{PUSH_ID}/", ctx.repo_prefix());
        let staged = ctx
            .store()
            .clone()
            .with_staging_writes(upload_prefix.clone());
        let lfs = crab_lfs::LfsObjectStore::new(staged.clone(), ctx.repo_prefix());
        let payload = source.path().join("lfs-payload");
        std::fs::write(&payload, lfs_content)?;
        lfs.put_stream_with_size(&lfs_pointer.oid, Some(lfs_pointer.size), &payload)
            .await?;
        let mut objects = vec![run_object, xorb_object, shard_object];
        objects.extend(staged.flush_staged_writes(16).await?);
        let plan = super::super::ProtectedCapsulePushPlan {
            schema_version: crab_remote::protected::PROTECTED_CAPSULE_PUSH_PLAN_SCHEMA_VERSION,
            repo_prefix: ctx.repo_prefix().to_owned(),
            push_id: ctx.push_id().to_owned(),
            upload_prefix,
            base_root_digest: root.record().digest().to_owned(),
            transaction_id: transaction.id()?,
            run_hash: run.hash().to_owned(),
            run_size: run.bytes().len() as u64,
            ref_updates: vec![update],
            staged_objects: objects,
        };
        write_plan(&ctx, &plan).await?;

        let verified = verify_receive(&ctx).await?;

        assert_eq!(verified.ref_updates, plan.ref_updates);
        assert_eq!(
            verified.verified_changed_paths,
            vec!["asset.lfs", "model.bin", "tracked.txt"]
        );
        assert_eq!(
            verified.verified_staged_bytes,
            plan.staged_objects
                .iter()
                .map(|object| object.size)
                .sum::<u64>()
        );
        let response =
            commit_receive(&ctx, "crab://bucket/org/repo", &verified.plan_digest, None).await?;
        assert_eq!(response.ref_updates, plan.ref_updates);
        let view = crab_read::capsule_protocol::open_view(
            ctx.router(),
            crab_read::capsule_protocol::CapsuleReadLimits {
                max_capsule_bytes: u64::MAX,
                max_frontier_bytes: u64::MAX,
            },
        )
        .await?;
        assert_eq!(view.refs().get("refs/heads/main"), Some(&tip));
        assert_eq!(
            view.visible_ref_transactions().get("refs/heads/main"),
            Some(&plan.transaction_id)
        );
        let catalog = view
            .pointer_catalog_lookup(ctx.router(), 1024 * 1024)?
            .files(
                ctx.store(),
                &[MerkleHash::from(xet.pointer.file_hash).hex()],
            )
            .await?;
        assert_eq!(catalog, xet.catalog);
        crab_read::verify_capsule_pointer_catalog_objects(ctx.router(), &catalog).await?;
        let (body, _) = ctx
            .store()
            .get_with_etag(&ctx.router().xorb_path(&xet.xorb_hash))
            .await?;
        let parsed = crab_xet::xorb::parser::XorbParser::parse(body)?;
        assert_eq!(parsed.get_chunk(0)?.data.as_ref(), xet_content);
        let lfs = crab_lfs::LfsObjectStore::new(ctx.store().clone(), ctx.repo_prefix());
        assert_eq!(lfs.get(&lfs_pointer.oid).await?.as_ref(), lfs_content);
        let receipt = crab_metadata::capsule_protocol::resolve_capsule_plan_receipt(
            ctx.store(),
            ctx.router(),
            &plan_id,
        )
        .await?
        .ok_or_else(|| invalid("protected planned capsule did not publish a plan receipt"))?;
        assert_eq!(receipt.transaction(), &transaction);

        let retried =
            commit_receive(&ctx, "crab://bucket/org/repo", &verified.plan_digest, None).await?;
        assert_eq!(retried.ref_updates, plan.ref_updates);
        Ok(())
    }

    #[tokio::test]
    async fn protected_capsule_view_push_preserves_hidden_paths_and_dependencies() -> Result<()> {
        for count in [100, 10_000] {
            protected_capsule_view_push(false, count, None).await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn checkpointed_capsule_view_push_preserves_hidden_paths_and_dependencies() -> Result<()>
    {
        for count in [100, 10_000] {
            protected_capsule_view_push(true, count, None).await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn scoped_capsule_candidate_cannot_borrow_hidden_source_git_objects() -> Result<()> {
        protected_capsule_view_push(false, 100, Some("source")).await
    }

    #[tokio::test]
    async fn scoped_capsule_candidate_cannot_borrow_invisible_retained_git_objects() -> Result<()> {
        protected_capsule_view_push(true, 100, Some("filtered")).await
    }

    #[derive(Default)]
    struct CatalogReads(AtomicU64, AtomicU64);

    impl crab_storage::StorageObserver for CatalogReads {
        fn started(&self, _: crab_storage::StorageOperation) {}

        fn finished(&self, observation: crab_storage::StorageObservation) {
            if matches!(
                observation.operation,
                crab_storage::StorageOperation::Get | crab_storage::StorageOperation::Range
            ) {
                self.0.fetch_add(1, Ordering::SeqCst);
                self.1.fetch_add(observation.bytes_read, Ordering::SeqCst);
            }
        }
    }

    async fn historical_catalog(
        layout: &crab_storage::StoreLayout<Store>,
        count: usize,
    ) -> Result<PointerCatalog> {
        let mut builder = XorbBuilder::new();
        let mut files = Vec::new();
        for index in 0..count {
            let data = Bytes::from(format!("unrelated historical payload {index:016x}\n"));
            let file = MerkleHash::from(*blake3::hash(&data).as_bytes());
            let chunk = crab_xet::xorb::format::Chunk::new(data.clone());
            builder.push(&chunk, RunId(index as u64))?;
            files.push((file, chunk.hash, data.len() as u64));
        }
        let xorbs = builder.finalize()?;
        let placements = xorbs
            .iter()
            .flat_map(|xorb| xorb.placements.iter())
            .map(|placement| (placement.chunk_hash, placement.clone()))
            .collect::<ChunkPlacementMap>();
        let dependencies = xorbs
            .iter()
            .map(|xorb| {
                Ok((
                    xorb.hash,
                    Arc::new(xorb_info_from_placements(xorb.hash, &xorb.placements)?),
                ))
            })
            .collect::<crab_xet::error::Result<BTreeMap<_, _>>>()?;
        let mut shards = PushShardSession::new();
        let mut file_shards = Vec::new();
        let mut shard_xorbs = BTreeMap::<usize, BTreeSet<String>>::new();
        for (hash, chunk, size) in files {
            let dependency = &dependencies[&placements[&chunk].xorb_hash];
            let index = shards.add_file_bundle(
                file_info_from_placements(hash, &[chunk], &placements)?,
                std::slice::from_ref(dependency),
            )?;
            file_shards.push((hash, size, index));
            shard_xorbs
                .entry(index)
                .or_default()
                .insert(placements[&chunk].xorb_hash.hex());
        }
        let shards = shards.finalize()?;
        let mut catalog = PointerCatalog::new();
        for xorb in xorbs {
            let mut chunks = xorb.placements.iter().collect::<Vec<_>>();
            chunks.sort_by_key(|chunk| chunk.chunk_index);
            catalog.insert_xorb(
                xorb.hash.hex(),
                XorbCatalogEntry::new(
                    xorb.bytes.len() as u64,
                    blake3_hex(&xorb.bytes),
                    chunks
                        .into_iter()
                        .map(|chunk| {
                            XorbChunkEntry::new(chunk.chunk_hash.hex(), chunk.uncompressed_size)
                        })
                        .collect(),
                ),
            )?;
            layout
                .store()
                .put_exact(&layout.xorb_path(&xorb.hash), xorb.bytes)
                .await?;
        }
        for (index, (bytes, hash)) in shards.iter().enumerate() {
            catalog.insert_shard(
                hash.hex(),
                ShardCatalogEntry::new(
                    bytes.len() as u64,
                    shard_xorbs[&index].iter().cloned().collect(),
                ),
            )?;
            layout
                .store()
                .put_exact(&layout.shard_path(hash), Bytes::from(bytes.clone()))
                .await?;
        }
        for (hash, size, index) in file_shards {
            catalog.insert_file(
                hash.hex(),
                FileCatalogEntry::new(size, shards[index].1.hex()),
            )?;
        }
        Ok(catalog)
    }

    async fn protected_capsule_view_push(
        checkpointed: bool,
        count: usize,
        borrowed_from: Option<&str>,
    ) -> Result<()> {
        let ctx = capsule_context().await?;
        let counts = Arc::new(CatalogReads::default());
        let ctx = ReceiveContext::from_store(
            ctx.store().clone().with_storage_observer(counts.clone()),
            ctx.repo_prefix().to_owned(),
            ctx.push_id().to_owned(),
        );
        let history = historical_catalog(ctx.router(), count).await?;
        let hidden_content = b"source-only Xet content\n";
        let hidden = xet_fixture(hidden_content)?;
        ctx.store()
            .put_exact(
                &ctx.router().xorb_path(&hidden.xorb_hash),
                hidden.xorb_bytes.clone(),
            )
            .await?;
        ctx.store()
            .put_exact(
                &ctx.router().shard_path(&hidden.shard_hash),
                Bytes::from(hidden.shard_bytes.clone()),
            )
            .await?;
        let mut source_catalog = history.clone();
        source_catalog.apply(&hidden.catalog)?;
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        run_git(["init", "--initial-branch=main", path_str(&source)?], None)?;
        run_git(["config", "user.email", "alice@example.com"], Some(&source))?;
        run_git(["config", "user.name", "Alice"], Some(&source))?;
        std::fs::create_dir_all(source.join("src"))?;
        std::fs::create_dir_all(source.join("secret"))?;
        std::fs::write(source.join("src/app.txt"), b"allowed v1\n")?;
        std::fs::write(source.join("secret/key.txt"), b"classified\n")?;
        std::fs::write(source.join("secret/model.bin"), hidden.pointer.serialize())?;
        run_git(["add", "."], Some(&source))?;
        run_git(["commit", "-m", "source base"], Some(&source))?;
        let source_old = run_git_capture(["-C", path_str(&source)?, "rev-parse", "HEAD"])?
            .trim()
            .to_owned();
        let hidden_oid = run_git_capture([
            "-C",
            path_str(&source)?,
            "rev-parse",
            &format!("{source_old}:secret/key.txt"),
        ])?
        .trim()
        .to_owned();
        let source_pack_bytes = git_capture_with_input(
            ["pack-objects", "--stdout", "--revs"],
            &source,
            format!("{source_old}\n").as_bytes(),
        )?;
        let source_pack = capsule_git_pack(
            &source.join(".git"),
            &source_pack_bytes,
            git_object_count(&source, &format!("{source_old}\n"))?,
        )?;
        let source_root = crab_metadata::capsule_protocol::load_root(ctx.router()).await?;
        let source_transaction = CapsuleTransaction::new(
            source_root.record().digest(),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                Some(source_old.clone()),
                None,
            )],
        )?;
        let source_visibility = visibility_for_tip(
            &source,
            "refs/heads/main",
            None,
            &source_old,
            &format!("{source_old}\n"),
        )?;
        let source_capsule = Capsule::build(
            &source_transaction,
            vec![source_pack],
            vec![
                CapsuleSection::new(
                    CapsuleSectionKind::VisibilityDelta,
                    source_visibility.encode()?,
                ),
                CapsuleSection::new(
                    CapsuleSectionKind::CatalogDelta,
                    source_catalog.encode_delta()?,
                ),
            ],
        )?;
        crab_write::capsule_protocol::publish(
            ctx.router(),
            source_root,
            &source_transaction,
            &source_capsule,
        )
        .await?;

        let filtered = temp.path().join("filtered");
        run_git(
            ["init", "--initial-branch=main", path_str(&filtered)?],
            None,
        )?;
        run_git(
            ["config", "user.email", "alice@example.com"],
            Some(&filtered),
        )?;
        run_git(["config", "user.name", "Alice"], Some(&filtered))?;
        std::fs::create_dir_all(filtered.join("src"))?;
        std::fs::write(filtered.join("src/app.txt"), b"allowed v1\n")?;
        run_git(["add", "."], Some(&filtered))?;
        run_git(["commit", "-m", "source base"], Some(&filtered))?;
        let view_old = run_git_capture(["-C", path_str(&filtered)?, "rev-parse", "HEAD"])?
            .trim()
            .to_owned();
        let view_prefix = "org/repo/acl-views/v2/scope/base";
        let view_router = crab_storage::StoreLayout::with_global_prefix(
            ctx.store().clone(),
            view_prefix.to_owned(),
            format!("{view_prefix}/.crab"),
        );
        let view_root = crab_write::capsule_protocol::initialize(
            &view_router,
            &"c".repeat(64),
            "refs/heads/main",
        )
        .await?;
        assert_eq!(historical_catalog(&view_router, count).await?, history);
        let view_base_revisions = if borrowed_from == Some("filtered") {
            let oid = git_capture_with_input(
                ["hash-object", "-w", "--stdin"],
                &filtered,
                b"classified\n",
            )?;
            assert_eq!(String::from_utf8(oid).unwrap().trim(), hidden_oid);
            // Physical retention is permitted; the base visibility proof below
            // deliberately grants no ref access to this extra blob.
            format!("{view_old}\n{hidden_oid}\n")
        } else {
            format!("{view_old}\n")
        };
        let view_base_pack_bytes = git_capture_with_input(
            ["pack-objects", "--stdout", "--revs"],
            &filtered,
            view_base_revisions.as_bytes(),
        )?;
        let view_base_pack = capsule_git_pack(
            &filtered.join(".git"),
            &view_base_pack_bytes,
            git_object_count(&filtered, &view_base_revisions)?,
        )?;
        let view_base_transaction = CapsuleTransaction::new(
            view_root.record().digest(),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                Some(view_old.clone()),
                None,
            )],
        )?;
        let view_base_visibility = visibility_for_tip(
            &filtered,
            "refs/heads/main",
            None,
            &view_old,
            &format!("{view_old}\n"),
        )?;
        let xet_content = b"protected view Xet content\n";
        let xet = xet_fixture(xet_content)?;
        ctx.store()
            .put_exact(
                &view_router.xorb_path(&xet.xorb_hash),
                xet.xorb_bytes.clone(),
            )
            .await?;
        ctx.store()
            .put_exact(
                &view_router.shard_path(&xet.shard_hash),
                Bytes::from(xet.shard_bytes.clone()),
            )
            .await?;
        let mut filtered_catalog = history.clone();
        for (hash, entry) in xet.catalog.xorbs() {
            filtered_catalog.insert_xorb(hash.clone(), entry.clone())?;
        }
        for (hash, entry) in xet.catalog.shards() {
            filtered_catalog.insert_shard(hash.clone(), entry.clone())?;
        }
        let view_base_capsule = Capsule::build(
            &view_base_transaction,
            vec![view_base_pack],
            vec![
                CapsuleSection::new(
                    CapsuleSectionKind::VisibilityDelta,
                    view_base_visibility.encode()?,
                ),
                CapsuleSection::new(
                    CapsuleSectionKind::CatalogDelta,
                    filtered_catalog.encode_delta()?,
                ),
            ],
        )?;
        crab_write::capsule_protocol::publish(
            &view_router,
            view_root,
            &view_base_transaction,
            &view_base_capsule,
        )
        .await?;

        if checkpointed {
            for layout in [ctx.router(), &view_router] {
                let outcome = crab_remote::checkpoint::publish_capsule_checkpoint(
                    layout,
                    0,
                    2 * 1024 * 1024 * 1024,
                    &CancellationToken::new(),
                )
                .await
                .map_err(std::io::Error::other)?;
                assert!(outcome.published);
            }
        }

        let lfs_content = b"protected view LFS content\n";
        let lfs_oid = <[u8; 32]>::from(Sha256::digest(lfs_content));
        let lfs_pointer = crab_git::lfs_pointer::LfsPointer {
            oid: lfs_oid,
            size: lfs_content.len() as u64,
            extensions: Vec::new(),
        };
        std::fs::write(filtered.join("src/app.txt"), b"allowed v2\n")?;
        std::fs::write(filtered.join("src/model.bin"), xet.pointer.serialize())?;
        std::fs::write(filtered.join("src/asset.lfs"), lfs_pointer.serialize())?;
        run_git(
            ["add", "src/app.txt", "src/model.bin", "src/asset.lfs"],
            Some(&filtered),
        )?;
        if borrowed_from.is_some() {
            std::fs::write(filtered.join("src/leak.txt"), b"classified\n")?;
            run_git(["add", "src/leak.txt"], Some(&filtered))?;
        }
        run_git(["commit", "-m", "allowed update"], Some(&filtered))?;
        let view_new = run_git_capture(["-C", path_str(&filtered)?, "rev-parse", "HEAD"])?
            .trim()
            .to_owned();
        let update = PushRefUpdate {
            ref_name: "refs/heads/main".to_owned(),
            old_oid: Some(view_old.clone()),
            new_oid: view_new.clone(),
        };
        prepare_receive(
            &ctx,
            vec![update.clone()],
            Some(PreparedViewScope {
                repo_prefix: view_prefix.to_owned(),
                global_prefix: format!("{view_prefix}/.crab"),
                source_repo: ctx.repo_prefix().to_owned(),
                scope_hash: "d".repeat(64),
            }),
        )
        .await?;
        let view = crab_read::capsule_protocol::open_view(
            &view_router,
            crab_read::capsule_protocol::CapsuleReadLimits {
                max_capsule_bytes: u64::MAX,
                max_frontier_bytes: u64::MAX,
            },
        )
        .await?;
        let candidate_revisions = if borrowed_from.is_some() {
            // The candidate declares this source-only blob reachable but omits
            // its body from the incoming pack. Source materialization must not
            // satisfy that missing dependency from its broader private ODB.
            format!("{view_new}\n^{view_old}\n^{hidden_oid}\n")
        } else {
            format!("{view_new}\n^{view_old}\n")
        };
        let candidate_pack_bytes = git_capture_with_input(
            ["pack-objects", "--stdout", "--revs"],
            &filtered,
            candidate_revisions.as_bytes(),
        )?;
        let candidate_pack = capsule_git_pack(
            &filtered.join(".git"),
            &candidate_pack_bytes,
            git_object_count(&filtered, &candidate_revisions)?,
        )?;
        let candidate_transaction = CapsuleTransaction::new(
            view.root().digest(),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                Some(view_old.clone()),
                Some(view_new.clone()),
                None,
            )],
        )?;
        let candidate_visibility = visibility_for_tip(
            &filtered,
            "refs/heads/main",
            Some(view_old),
            &view_new,
            &format!("{view_new}\n"),
        )?;
        let mut candidate_catalog = PointerCatalog::new();
        for (hash, entry) in xet.catalog.files() {
            candidate_catalog.insert_file(hash.clone(), entry.clone())?;
        }
        let candidate = Capsule::build(
            &candidate_transaction,
            vec![candidate_pack],
            vec![
                CapsuleSection::new(
                    CapsuleSectionKind::VisibilityDelta,
                    candidate_visibility.encode()?,
                ),
                CapsuleSection::new(
                    CapsuleSectionKind::CatalogDelta,
                    candidate_catalog.encode_delta()?,
                ),
            ],
        )?;
        let run = CapsuleRun::leaf(candidate)?;
        let run_object = staged_object(
            ctx.router().capsule_path(run.hash()).to_string(),
            run.bytes(),
        );
        put_staged(&ctx, &run_object, run.bytes().to_vec()).await?;
        let lfs_key =
            crab_lfs::LfsObjectStore::object_path_for_prefix(ctx.router().repo_prefix(), &lfs_oid)
                .to_string();
        let lfs_object = staged_object(lfs_key.clone(), lfs_content);
        put_staged(&ctx, &lfs_object, lfs_content.to_vec()).await?;
        let plan = super::super::ProtectedCapsulePushPlan {
            schema_version: crab_remote::protected::PROTECTED_CAPSULE_PUSH_PLAN_SCHEMA_VERSION,
            repo_prefix: ctx.repo_prefix().to_owned(),
            push_id: ctx.push_id().to_owned(),
            upload_prefix: format!("{}/staging/{PUSH_ID}/", ctx.repo_prefix()),
            base_root_digest: view.root().digest().to_owned(),
            transaction_id: candidate_transaction.id()?,
            run_hash: run.hash().to_owned(),
            run_size: run.bytes().len() as u64,
            ref_updates: vec![update],
            staged_objects: vec![run_object, lfs_object],
        };
        write_plan(&ctx, &plan).await?;

        counts.0.store(0, Ordering::SeqCst);
        counts.1.store(0, Ordering::SeqCst);
        let result = verify_receive(&ctx).await;
        if borrowed_from.is_some() {
            assert!(
                result.is_err(),
                "scoped candidate borrowed a hidden {borrowed_from:?} blob without supplying its body"
            );
            let refs = crab_read::capsule_protocol::read_visible_refs(ctx.router()).await?;
            assert_eq!(refs["refs/heads/main"], source_old);
            return Ok(());
        }
        let verified = result?;
        assert!(
            counts.0.load(Ordering::SeqCst) <= 64 && counts.1.load(Ordering::SeqCst) <= 512 * 1024,
            "fixed protected verification exceeded read bounds: count={count}, checkpoint={checkpointed}, reads={}, bytes={}",
            counts.0.load(Ordering::SeqCst),
            counts.1.load(Ordering::SeqCst),
        );
        assert_eq!(
            verified.verified_changed_paths,
            vec!["src/app.txt", "src/asset.lfs", "src/model.bin"]
        );
        commit_receive(&ctx, "crab://bucket/org/repo", &verified.plan_digest, None).await?;
        commit_receive(&ctx, "crab://bucket/org/repo", &verified.plan_digest, None).await?;

        let source_view = crab_read::capsule_protocol::open_view(
            ctx.router(),
            crab_read::capsule_protocol::CapsuleReadLimits {
                max_capsule_bytes: u64::MAX,
                max_frontier_bytes: u64::MAX,
            },
        )
        .await?;
        let final_oid = source_view
            .refs()
            .get("refs/heads/main")
            .ok_or_else(|| invalid("synthesized source ref is missing"))?;
        assert_ne!(final_oid, &view_new);
        let final_git = temp.path().join("final.git");
        run_git(["init", "--bare", path_str(&final_git)?], None)?;
        crab_read::capsule_protocol::install_git_packs_from_store(
            &source_view,
            ctx.router(),
            &final_git,
            u64::MAX,
            None,
            &CancellationToken::new(),
        )
        .await?;
        assert_eq!(
            run_git_capture([
                "--git-dir",
                path_str(&final_git)?,
                "show",
                &format!("{final_oid}:src/app.txt"),
            ])?,
            "allowed v2\n"
        );
        assert_eq!(
            run_git_capture([
                "--git-dir",
                path_str(&final_git)?,
                "show",
                &format!("{final_oid}:secret/key.txt"),
            ])?,
            "classified\n"
        );
        let hidden_pointer = run_git_capture([
            "--git-dir",
            path_str(&final_git)?,
            "show",
            &format!("{final_oid}:secret/model.bin"),
        ])?;
        assert_eq!(Pointer::parse(hidden_pointer.as_bytes())?, hidden.pointer);
        let final_pointer = run_git_capture([
            "--git-dir",
            path_str(&final_git)?,
            "show",
            &format!("{final_oid}:src/model.bin"),
        ])?;
        assert_eq!(Pointer::parse(final_pointer.as_bytes())?, xet.pointer);
        let final_lfs_pointer = run_git_capture([
            "--git-dir",
            path_str(&final_git)?,
            "show",
            &format!("{final_oid}:src/asset.lfs"),
        ])?;
        assert_eq!(
            crab_git::lfs_pointer::LfsPointer::parse(final_lfs_pointer.as_bytes())
                .map_err(|error| invalid(error.to_string()))?,
            lfs_pointer
        );
        let source_catalog = source_view.pointer_catalog().await?;
        assert!(
            source_catalog
                .files()
                .contains_key(&MerkleHash::from(xet.pointer.file_hash).hex())
        );
        crab_read::verify_capsule_pointer_catalog_objects(ctx.router(), &source_catalog).await?;
        let local_cache = Arc::new(crab_cache::LocalCache::with_limits(
            temp.path().join("read-cache"),
            1024 * 1024,
            Some(1024 * 1024),
        ));
        let caching = crab_cache_store::CachingStore::new_with_local_cache(
            ctx.store().clone(),
            crab_cache_store::CacheConfig::default(),
            local_cache,
        )?;
        let hydrator =
            crab_read::ReadRuntimeBuilder::new(caching, ctx.router().clone(), 2).build()?;
        let lookup = crab_metadata::file_index_lookup::SharedFileIndexLookup::new_for_storage(
            ctx.store(),
            ctx.repo_prefix(),
        );
        for (name, pointer, expected) in [
            ("hydrated-model.bin", &xet.pointer, xet_content.as_slice()),
            (
                "hydrated-hidden.bin",
                &hidden.pointer,
                hidden_content.as_slice(),
            ),
        ] {
            let hydrated = temp.path().join(name);
            hydrator
                .reconstruct_to_writer_with_cancel(
                    pointer,
                    std::fs::File::create(&hydrated)?,
                    Some(&lookup),
                    &CancellationToken::new(),
                )
                .await?;
            assert_eq!(std::fs::read(hydrated)?, expected);
        }
        lookup.close().await?;
        let (stored_lfs, _) = ctx
            .store()
            .get_with_etag_bounded(&ObjectPath::from(lfs_key), lfs_content.len() as u64)
            .await?;
        assert_eq!(stored_lfs.as_ref(), lfs_content);
        Ok(())
    }

    #[tokio::test]
    async fn verified_receive_evidence_rejects_replacement() -> Result<()> {
        let ctx = capsule_context().await?;
        let prepare = ctx.write_prepare_record(vec![ref_update()], None).await?;
        let plan = super::super::ProtectedCapsulePushPlan {
            schema_version: crab_remote::protected::PROTECTED_CAPSULE_PUSH_PLAN_SCHEMA_VERSION,
            repo_prefix: ctx.repo_prefix().to_owned(),
            push_id: PUSH_ID.to_owned(),
            upload_prefix: format!("{}/staging/{PUSH_ID}/", ctx.repo_prefix()),
            base_root_digest: prepare.source_root_digest.clone().unwrap(),
            transaction_id: blake3_hex(b"transaction"),
            run_hash: blake3_hex(b"run"),
            run_size: 3,
            ref_updates: vec![ref_update()],
            staged_objects: Vec::new(),
        };
        let digest =
            write_verified_capsule_receive_evidence(&ctx, &plan, &prepare, Vec::new(), Vec::new())
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

        let error = read_verified_capsule_receive_evidence(&ctx, &digest, &plan, &prepare)
            .await
            .expect_err("replacement evidence must not authorize commit");

        assert!(matches!(error, AuthServerError::CasConflict { .. }));
        Ok(())
    }
}
