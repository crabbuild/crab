//! Protocol-v2 protected-push candidate verification.

use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use crab_auth::PushRefUpdate;
use crab_coordination::write_coordinator::{CommitOutcome, CoordinatedRefUpdate};
use crab_metadata::capsule_protocol::{Capsule, CapsuleRun, PointerCatalog};
use crab_storage::content_hash_from_path;
use crab_xet::hash::{MerkleHash, compute_data_hash};
use crab_xet::shard::ShardReader;
use sha2::{Digest, Sha256};

use super::git_workspace::verify_capsule_git_candidate;
use super::{
    ActiveActiveReceiveConfig, ProtectedCapsulePushPlan, PushPrepareRecord, ReceiveContext,
    active_active_coordinator_registration, conflict, invalid, promote_staged_writes,
    read_verified_staged_object, strict_xorb_references_from_shard,
    validate_protected_capsule_plan_shape, validate_staged_xorb,
};
use crate::error::Result;
use crate::git_pointer_scan::ReachablePointerScan;

const MAX_PROTECTED_CAPSULE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

pub(super) struct VerifiedCapsuleCandidate {
    pub prepare: PushPrepareRecord,
    pub changed_paths: Vec<String>,
    pub staged_bytes: u64,
    pub replication_objects: Vec<String>,
}

pub(super) async fn commit_capsule_candidate(
    ctx: &ReceiveContext,
    plan: &ProtectedCapsulePushPlan,
    active_active: Option<&ActiveActiveReceiveConfig>,
    replication_objects: &[String],
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Option<CommitOutcome>> {
    let view = open_candidate_view(ctx).await?;
    if capsule_is_visible(&view, plan) {
        let Some(active_active) = active_active else {
            return Ok(None);
        };
        let run = read_candidate_run(ctx, plan).await?;
        let capsule = run
            .capsules()
            .first()
            .ok_or_else(|| invalid("protected capsule run is empty"))?;
        let transaction = capsule.transaction()?;
        let descriptor = crab_write::capsule_protocol::coordinated_publication_descriptor(
            &plan.base_root_digest,
            &plan.transaction_id,
            &plan.run_hash,
            plan.run_size,
        );
        return commit_active_active_candidate(
            ctx.store(),
            ctx.router(),
            ctx.repo_prefix(),
            active_active,
            descriptor,
            &transaction,
            replication_objects,
            None,
        )
        .await
        .map(Some);
    }

    let verified = verify_capsule_candidate(ctx, plan).await?;
    if verified.replication_objects != replication_objects {
        return Err(conflict(
            "capsule dependency closure changed after protected verification",
        ));
    }
    promote_staged_writes(ctx.store(), &plan.staged_objects).await?;
    let run = read_candidate_run(ctx, plan).await?;
    let capsule = run
        .capsules()
        .first()
        .cloned()
        .ok_or_else(|| invalid("protected capsule run is empty"))?;
    if let Some(delta) = capsule.pointer_catalog_delta()? {
        crab_metadata::ref_registry::union_register_repo_shards(
            ctx.store(),
            ctx.router(),
            delta.shards().keys().cloned().collect(),
        )
        .await?;
    }
    let transaction = capsule.transaction()?;
    let ref_names = transaction
        .edits()
        .iter()
        .map(|edit| edit.ref_name().to_owned())
        .collect::<Vec<_>>();
    let changes_namespace = transaction
        .edits()
        .iter()
        .any(|edit| edit.expected_old().is_none() != edit.new_oid().is_none());
    let base = view.root_snapshot().clone();
    if changes_namespace {
        let layout = ctx.router().clone();
        let repo_prefix = ctx.repo_prefix().to_owned();
        let active_active = active_active.cloned();
        let replication_objects = replication_objects.to_vec();
        return crab_write::with_ref_namespaces(
            ctx.store(),
            ctx.router(),
            &ref_names,
            crab_coordination::DEFAULT_PUSH_LOCK_TTL,
            cancel,
            |scoped| async move {
                if scoped.is_cancelled() {
                    return Err(crate::error::AuthServerError::from(
                        crab_write::WriteError::Cancelled,
                    ));
                }
                crab_write::capsule_protocol::validate_ref_namespace(
                    &layout,
                    base.record().root(),
                    transaction.edits(),
                )
                .await?;
                publish_verified_candidate(
                    layout.store(),
                    &layout,
                    &repo_prefix,
                    base,
                    &transaction,
                    &capsule,
                    active_active.as_ref(),
                    &replication_objects,
                )
                .await
            },
        )
        .await;
    }
    publish_verified_candidate(
        ctx.store(),
        ctx.router(),
        ctx.repo_prefix(),
        base,
        &transaction,
        &capsule,
        active_active,
        replication_objects,
    )
    .await
}

async fn publish_verified_candidate(
    store: &crab_storage::Store,
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    repo_prefix: &str,
    base: crab_metadata::capsule_protocol::RootSnapshot,
    transaction: &crab_metadata::capsule_protocol::CapsuleTransaction,
    capsule: &Capsule,
    active_active: Option<&ActiveActiveReceiveConfig>,
    replication_objects: &[String],
) -> Result<Option<CommitOutcome>> {
    let Some(active_active) = active_active else {
        crab_write::capsule_protocol::publish(layout, base, transaction, capsule).await?;
        return Ok(None);
    };
    let prepared = crab_write::capsule_protocol::prepare_coordinated_publication(
        layout,
        base,
        transaction,
        capsule,
    )
    .await?;
    let descriptor = prepared.descriptor().clone();
    commit_active_active_candidate(
        store,
        layout,
        repo_prefix,
        active_active,
        descriptor,
        transaction,
        replication_objects,
        Some(prepared),
    )
    .await
    .map(Some)
}

async fn commit_active_active_candidate(
    store: &crab_storage::Store,
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    repo_prefix: &str,
    active_active: &ActiveActiveReceiveConfig,
    descriptor: crab_coordination::write_coordinator::CoordinatedCapsulePublication,
    transaction: &crab_metadata::capsule_protocol::CapsuleTransaction,
    replication_objects: &[String],
    prepared: Option<crab_write::capsule_protocol::CoordinatedPreparedCapsulePublication>,
) -> Result<CommitOutcome> {
    let refs = transaction
        .edits()
        .iter()
        .map(|edit| CoordinatedRefUpdate {
            name: edit.ref_name().to_owned(),
            expected: edit.expected_old().map(str::to_owned),
            new: edit.new_oid().map(str::to_owned),
            force: false,
        })
        .collect::<Vec<_>>();
    let mut uploaded_objects = replication_objects.iter().cloned().collect::<BTreeSet<_>>();
    uploaded_objects.insert(layout.capsule_path(&descriptor.run_hash).to_string());
    let plan = crab_coordination::active_active::plan_active_active_capsule_push(
        &active_active.replication,
        Some(&active_active.writer),
        descriptor,
        refs,
        uploaded_objects.into_iter().collect(),
    )?;
    let registration = active_active_coordinator_registration(&active_active.replication)?;
    crab_metadata::ref_registry::register_active_active_coordinator_for_repo(
        store,
        layout,
        registration,
    )
    .await?;
    let coordinator = crab_coordination::active_active_write_coordinator_for_repo(
        &active_active.replication,
        repo_prefix,
    )
    .await?;
    let mut outcome = crab_coordination::write_coordinator::commit_uploaded_push_refs(
        coordinator.as_ref(),
        plan.request.clone(),
    )
    .await?;
    let materialized = match prepared {
        Some(prepared) => {
            match crab_write::capsule_protocol::materialize_coordinated_publication(
                layout, prepared,
            )
            .await
            {
                Ok(_) => true,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        operation_id = %outcome.operation_id,
                        "protected capsule coordinator commit succeeded; local materialization requires repair"
                    );
                    false
                }
            }
        }
        None => true,
    };
    if materialized {
        match coordinator
            .mark_region_materialized(&outcome.operation_id, &plan.request.region)
            .await
        {
            Ok(state) => outcome.state = state,
            Err(error) => tracing::warn!(
                %error,
                operation_id = %outcome.operation_id,
                "protected capsule materialization acknowledgement requires repair"
            ),
        }
    }
    Ok(outcome)
}

async fn open_candidate_view(
    ctx: &ReceiveContext,
) -> Result<crab_read::capsule_protocol::CapsuleRepositoryView> {
    crab_read::capsule_protocol::open_view(
        ctx.router(),
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: MAX_PROTECTED_CAPSULE_BYTES,
            max_frontier_bytes: MAX_PROTECTED_CAPSULE_BYTES,
        },
    )
    .await
    .map_err(Into::into)
}

fn capsule_is_visible(
    view: &crab_read::capsule_protocol::CapsuleRepositoryView,
    plan: &ProtectedCapsulePushPlan,
) -> bool {
    plan.ref_updates.iter().all(|update| {
        view.refs().get(&update.ref_name) == Some(&update.new_oid)
            && view.visible_ref_transactions().get(&update.ref_name) == Some(&plan.transaction_id)
    })
}

pub(super) async fn verify_capsule_candidate(
    ctx: &ReceiveContext,
    plan: &ProtectedCapsulePushPlan,
) -> Result<VerifiedCapsuleCandidate> {
    validate_protected_capsule_plan_shape(plan, ctx.repo_prefix(), ctx.push_id())?;
    let prepare = ctx.read_prepare_record().await?;
    if prepare.schema_version != 2 {
        return Err(conflict(
            "capsule push-plan does not match the prepared repository protocol",
        ));
    }
    if prepare.view_scope.is_some() {
        return Err(invalid(
            "protocol-v2 protected view synthesis is not implemented",
        ));
    }
    if prepare.view_ref_updates != plan.ref_updates {
        return Err(conflict("staged ref updates do not match prepare record"));
    }
    let prepared_root = prepare
        .source_root_digest
        .as_deref()
        .ok_or_else(|| invalid("capsule prepare record is missing its root digest"))?;
    if prepared_root != plan.base_root_digest {
        return Err(conflict("capsule base root differs from prepare record"));
    }

    let view = crab_read::capsule_protocol::open_view(
        ctx.router(),
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: MAX_PROTECTED_CAPSULE_BYTES,
            max_frontier_bytes: MAX_PROTECTED_CAPSULE_BYTES,
        },
    )
    .await?;
    if view.root().digest() != plan.base_root_digest {
        return Err(conflict("capsule root changed since prepare"));
    }
    validate_prepared_ref_heads(&prepare, view.refs())?;

    let run = read_candidate_run(ctx, plan).await?;
    if run.level() != 0 || run.capsules().len() != 1 {
        return Err(invalid(
            "protected capsule publication must contain one level-zero capsule",
        ));
    }
    let capsule = run
        .capsules()
        .first()
        .ok_or_else(|| invalid("protected capsule run is empty"))?;
    validate_capsule_transaction(plan, capsule)?;

    let mut catalog = view.pointer_catalog()?;
    if let Some(delta) = capsule.pointer_catalog_delta()? {
        catalog.apply(&delta)?;
    }
    let (changed_paths, pointers) = verify_capsule_git_candidate(
        ctx.store(),
        ctx.router(),
        &view,
        capsule,
        &plan.ref_updates,
        MAX_PROTECTED_CAPSULE_BYTES,
    )
    .await?;
    let replication_objects = verify_pointer_dependencies(ctx, plan, &catalog, &pointers).await?;
    let staged_bytes = plan
        .staged_objects
        .iter()
        .try_fold(0_u64, |total, object| total.checked_add(object.size))
        .ok_or_else(|| invalid("verified staged object bytes exceed the supported range"))?;
    Ok(VerifiedCapsuleCandidate {
        prepare,
        changed_paths,
        staged_bytes,
        replication_objects: replication_objects.into_iter().collect(),
    })
}

async fn verify_pointer_dependencies(
    ctx: &ReceiveContext,
    plan: &ProtectedCapsulePushPlan,
    catalog: &PointerCatalog,
    pointers: &ReachablePointerScan,
) -> Result<BTreeSet<String>> {
    let mut referenced = BTreeSet::from([ctx
        .router()
        .capsule_path(&plan.run_hash)
        .as_ref()
        .to_owned()]);
    let mut verified_shards = BTreeMap::<String, Bytes>::new();
    let mut verified_xorbs = BTreeSet::new();
    for pointer in &pointers.crab_pointers {
        let file_hash = MerkleHash::from(pointer.file_hash).hex();
        let file = catalog
            .files()
            .get(&file_hash)
            .ok_or_else(|| invalid("Crab pointer is absent from the candidate catalog"))?;
        if file.size() != pointer.size {
            return Err(invalid(
                "Crab pointer size differs from the candidate catalog",
            ));
        }
        let shard_hash = MerkleHash::from_hex(file.shard_hash())
            .map_err(|error| invalid(format!("candidate shard hash is invalid: {error}")))?;
        let shard = catalog
            .shards()
            .get(file.shard_hash())
            .ok_or_else(|| invalid("candidate catalog omits a pointer shard"))?;
        let shard_key = ctx.router().shard_path(&shard_hash).as_ref().to_owned();
        referenced.insert(shard_key.clone());
        let first_shard_use = !verified_shards.contains_key(file.shard_hash());
        let bytes = match verified_shards.get(file.shard_hash()) {
            Some(bytes) => bytes.clone(),
            None => {
                let bytes =
                    read_candidate_object(ctx, plan, &shard_key, shard.encoded_size()).await?;
                verified_shards.insert(file.shard_hash().to_owned(), bytes.clone());
                bytes
            }
        };
        if first_shard_use {
            if compute_data_hash(&bytes) != shard_hash {
                return Err(invalid(
                    "candidate shard body hash differs from its catalog",
                ));
            }
            let xorb_refs = strict_xorb_references_from_shard(&bytes)?;
            let actual = xorb_refs
                .keys()
                .filter_map(|key| content_hash_from_path(key, "xorbs"))
                .map(str::to_owned)
                .collect::<BTreeSet<_>>();
            let expected = shard.xorb_hashes().iter().cloned().collect::<BTreeSet<_>>();
            if actual != expected {
                return Err(invalid(
                    "candidate shard dependency closure differs from its catalog",
                ));
            }
            for (relative_key, chunks) in xorb_refs {
                let hash = content_hash_from_path(&relative_key, "xorbs")
                    .ok_or_else(|| invalid("candidate shard contains an invalid xorb key"))?;
                let xorb = catalog
                    .xorbs()
                    .get(hash)
                    .ok_or_else(|| invalid("candidate catalog omits a shard xorb"))?;
                if xorb.chunks().len() != chunks.len()
                    || xorb.chunks().iter().zip(&chunks).any(|(catalog, shard)| {
                        catalog.hash() != shard.hash.hex()
                            || catalog.uncompressed_size() != shard.uncompressed_size
                    })
                {
                    return Err(invalid(
                        "candidate xorb chunk catalog differs from its shard",
                    ));
                }
                let xorb_hash = MerkleHash::from_hex(hash)
                    .map_err(|error| invalid(format!("candidate xorb hash is invalid: {error}")))?;
                let key = ctx.router().xorb_path(&xorb_hash).as_ref().to_owned();
                referenced.insert(key.clone());
                if verified_xorbs.insert(hash.to_owned()) {
                    let bytes = read_candidate_object(ctx, plan, &key, xorb.encoded_size()).await?;
                    if blake3::hash(&bytes).to_hex().as_str() != xorb.body_digest() {
                        return Err(invalid(
                            "candidate xorb body digest differs from its catalog",
                        ));
                    }
                    validate_staged_xorb(&key, &bytes, &chunks)?;
                }
            }
        }
        let reader = ShardReader::from_bytes(bytes, shard_hash);
        let file_info = reader
            .get_file_info(&MerkleHash::from(pointer.file_hash))?
            .ok_or_else(|| invalid("candidate shard does not contain its pointer recipe"))?;
        if file_info.file_size() != pointer.size {
            return Err(invalid(
                "candidate shard recipe size differs from its pointer",
            ));
        }
    }
    for pointer in &pointers.lfs_pointers {
        let key = crab_lfs::LfsObjectStore::object_path_for_prefix(
            ctx.router().repo_prefix(),
            &pointer.oid,
        )
        .to_string();
        referenced.insert(key.clone());
        let bytes = read_candidate_object(ctx, plan, &key, pointer.size).await?;
        if <[u8; 32]>::from(Sha256::digest(&bytes)) != pointer.oid {
            return Err(invalid(
                "candidate LFS object digest differs from its pointer",
            ));
        }
    }
    if let Some(unreferenced) = plan
        .staged_objects
        .iter()
        .find(|object| !referenced.contains(&object.canonical_key))
    {
        return Err(invalid(format!(
            "staged object is not reachable from the candidate capsule: {}",
            unreferenced.canonical_key
        )));
    }
    Ok(referenced)
}

async fn read_candidate_object(
    ctx: &ReceiveContext,
    plan: &ProtectedCapsulePushPlan,
    canonical_key: &str,
    expected_size: u64,
) -> Result<Bytes> {
    let bytes = match plan
        .staged_objects
        .iter()
        .find(|object| object.canonical_key == canonical_key)
    {
        Some(object) => {
            if object.size != expected_size {
                return Err(invalid(
                    "staged dependency size differs from the candidate catalog",
                ));
            }
            read_verified_staged_object(ctx.store(), object).await?
        }
        None => {
            ctx.store()
                .get_with_etag_bounded(
                    &object_store::path::Path::from(canonical_key.to_owned()),
                    expected_size,
                )
                .await?
                .0
        }
    };
    if bytes.len() as u64 != expected_size {
        return Err(invalid(
            "candidate dependency size differs from the candidate catalog",
        ));
    }
    Ok(bytes)
}

async fn read_candidate_run(
    ctx: &ReceiveContext,
    plan: &ProtectedCapsulePushPlan,
) -> Result<CapsuleRun> {
    let canonical = ctx.router().capsule_path(&plan.run_hash);
    let bytes = match plan
        .staged_objects
        .iter()
        .find(|object| object.canonical_key == canonical.as_ref())
    {
        Some(object) => read_verified_staged_object(ctx.store(), object).await?,
        None => {
            ctx.store()
                .get_with_etag_bounded(&canonical, plan.run_size)
                .await?
                .0
        }
    };
    if bytes.len() as u64 != plan.run_size {
        return Err(invalid("capsule run size differs from push-plan"));
    }
    if blake3::hash(&bytes).to_hex().as_str() != plan.run_hash {
        return Err(invalid("capsule run hash differs from push-plan"));
    }
    let run = CapsuleRun::decode(bytes)?;
    if run.hash() != plan.run_hash {
        return Err(invalid(
            "decoded capsule run identity differs from push-plan",
        ));
    }
    Ok(run)
}

fn validate_prepared_ref_heads(
    prepare: &PushPrepareRecord,
    current: &BTreeMap<String, String>,
) -> Result<()> {
    for update in &prepare.source_ref_updates {
        if current.get(&update.ref_name).map(String::as_str) != update.old_oid.as_deref() {
            return Err(conflict(format!(
                "source ref changed since prepare: {}",
                update.ref_name
            )));
        }
    }
    Ok(())
}

fn validate_capsule_transaction(plan: &ProtectedCapsulePushPlan, capsule: &Capsule) -> Result<()> {
    if capsule.base_root_digest() != plan.base_root_digest
        || capsule.transaction_id() != plan.transaction_id
    {
        return Err(invalid(
            "capsule identity differs from the protected push-plan",
        ));
    }
    let transaction = capsule.transaction()?;
    if transaction.base_root_digest() != plan.base_root_digest
        || transaction.id()? != plan.transaction_id
    {
        return Err(invalid(
            "capsule transaction differs from the protected push-plan",
        ));
    }
    let edits = transaction
        .edits()
        .iter()
        .map(|edit| {
            Ok((
                edit.ref_name().to_owned(),
                (
                    edit.expected_old().map(str::to_owned),
                    edit.new_oid().map(str::to_owned).ok_or_else(|| {
                        invalid("protected capsule does not support ref deletion")
                    })?,
                ),
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let updates = plan
        .ref_updates
        .iter()
        .map(|update: &PushRefUpdate| {
            (
                update.ref_name.clone(),
                (update.old_oid.clone(), update.new_oid.clone()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    if edits != updates {
        return Err(invalid(
            "capsule transaction ref edits differ from the protected push-plan",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crab_metadata::capsule_protocol::{CapsuleRefEdit, CapsuleTransaction};

    use super::*;

    fn oid(ch: char) -> String {
        std::iter::repeat_n(ch, 40).collect()
    }

    fn hash(ch: char) -> String {
        std::iter::repeat_n(ch, 64).collect()
    }

    #[test]
    fn protected_plan_binds_exact_capsule_transaction() {
        let base = hash('a');
        let transaction = CapsuleTransaction::new(
            &base,
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                Some(oid('1')),
                Some(oid('2')),
                None,
            )],
        )
        .expect("transaction");
        let capsule = Capsule::build(&transaction, Vec::new(), Vec::new()).expect("capsule");
        let plan = ProtectedCapsulePushPlan {
            schema_version: crab_remote::protected::PROTECTED_CAPSULE_PUSH_PLAN_SCHEMA_VERSION,
            repo_prefix: "org/repo".to_owned(),
            push_id: "b".repeat(32),
            upload_prefix: format!("org/repo/staging/{}/", "b".repeat(32)),
            base_root_digest: base,
            transaction_id: transaction.id().expect("transaction id"),
            run_hash: hash('c'),
            run_size: 1,
            ref_updates: vec![PushRefUpdate {
                ref_name: "refs/heads/main".to_owned(),
                old_oid: Some(oid('1')),
                new_oid: oid('2'),
            }],
            staged_objects: Vec::new(),
        };

        validate_capsule_transaction(&plan, &capsule).expect("matching transaction");
    }
}
