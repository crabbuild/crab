//! Protocol-v2 protected-push candidate verification.

use std::collections::{BTreeMap, BTreeSet};

use crab_auth::PushRefUpdate;
use crab_coordination::write_coordinator::{CommitOutcome, CoordinatedRefUpdate};
use crab_metadata::capsule_protocol::{Capsule, CapsuleRun, CapsuleSection, CapsuleSectionKind};

use super::capsule_dependencies::{
    DependencyCopy, promote_dependency_copies, source_pointer_delta, verify_pointer_dependencies,
};
use super::git_workspace::{materialize_capsule_source_push, verify_capsule_git_candidate};
use super::{
    ActiveActiveReceiveConfig, ProtectedCapsulePushPlan, PushPrepareRecord, ReceiveContext,
    active_active_coordinator_registration, conflict, invalid, promote_staged_writes,
    read_verified_staged_object, validate_protected_capsule_plan_shape,
};
use crate::error::Result;

const MAX_PROTECTED_CAPSULE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

pub(super) struct VerifiedCapsuleCandidate {
    pub prepare: PushPrepareRecord,
    pub changed_paths: Vec<String>,
    pub staged_bytes: u64,
    pub replication_objects: Vec<String>,
    pub publication: Capsule,
    dependency_copies: Vec<DependencyCopy>,
}

pub(super) async fn commit_capsule_candidate(
    ctx: &ReceiveContext,
    plan: &ProtectedCapsulePushPlan,
    active_active: Option<&ActiveActiveReceiveConfig>,
    replication_objects: &[String],
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Option<CommitOutcome>> {
    let view = open_candidate_view(ctx).await?;
    if plan.ref_updates.iter().all(|update| {
        view.refs().get(&update.ref_name) == Some(&update.new_oid)
            && view.visible_ref_transactions().get(&update.ref_name) == Some(&plan.transaction_id)
    }) {
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
    let capsule = verified.publication;
    let transaction = capsule.transaction()?;
    if capsule_is_visible(view.refs(), view.visible_ref_transactions(), &transaction) {
        let Some(active_active) = active_active else {
            return Ok(None);
        };
        let run = crab_write::capsule_protocol::capsule_leaf_run(&capsule)?;
        let descriptor = crab_write::capsule_protocol::coordinated_publication_descriptor(
            transaction.base_root_digest(),
            &transaction.id()?,
            run.hash(),
            run.bytes().len() as u64,
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

    promote_staged_writes(ctx.store(), &plan.staged_objects).await?;
    promote_dependency_copies(ctx, &verified.dependency_copies).await?;
    if let Some(delta) = capsule.pointer_catalog_delta()? {
        crab_metadata::ref_registry::union_register_repo_shards(
            ctx.store(),
            ctx.router(),
            delta.shards().keys().cloned().collect(),
        )
        .await?;
    }
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
) -> Result<crab_read::capsule_protocol::CapsuleRefView> {
    let root = crab_metadata::capsule_protocol::load_root(ctx.router()).await?;
    crab_read::capsule_protocol::open_ref_view_from_root(ctx.router(), root)
        .await
        .map_err(Into::into)
}

fn capsule_is_visible(
    refs: &BTreeMap<String, String>,
    visible_ref_transactions: &BTreeMap<String, String>,
    transaction: &crab_metadata::capsule_protocol::CapsuleTransaction,
) -> bool {
    let Ok(transaction_id) = transaction.id() else {
        return false;
    };
    transaction.edits().iter().all(|edit| {
        refs.get(edit.ref_name()).map(String::as_str) == edit.new_oid()
            && visible_ref_transactions.get(edit.ref_name()) == Some(&transaction_id)
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
    if prepare.view_ref_updates != plan.ref_updates {
        return Err(conflict("staged ref updates do not match prepare record"));
    }
    let prepared_root = prepare
        .source_root_digest
        .as_deref()
        .ok_or_else(|| invalid("capsule prepare record is missing its root digest"))?;
    let source_view = crab_read::capsule_protocol::open_view(
        ctx.router(),
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: MAX_PROTECTED_CAPSULE_BYTES,
            max_frontier_bytes: MAX_PROTECTED_CAPSULE_BYTES,
        },
    )
    .await?;
    if source_view.root().digest() != prepared_root {
        return Err(conflict("source capsule root changed since prepare"));
    }
    let run = read_candidate_run(ctx, plan).await?;
    if run.level() != 0 || run.capsules().len() != 1 {
        return Err(invalid(
            "protected capsule publication must contain one level-zero capsule",
        ));
    }
    let candidate = run
        .capsules()
        .first()
        .ok_or_else(|| invalid("protected capsule run is empty"))?;
    validate_capsule_transaction(plan, candidate)?;

    let (changed_paths, publication, replication_objects, dependency_copies) =
        match prepare.view_scope.as_ref() {
            None => {
                if prepared_root != plan.base_root_digest {
                    return Err(conflict("capsule base root differs from prepare record"));
                }
                let mut catalog = source_view.pointer_catalog()?;
                if let Some(delta) = candidate.pointer_catalog_delta()? {
                    catalog.apply(&delta)?;
                }
                let (changed_paths, pointers) = verify_capsule_git_candidate(
                    ctx.store(),
                    ctx.router(),
                    &source_view,
                    candidate,
                    &plan.ref_updates,
                    MAX_PROTECTED_CAPSULE_BYTES,
                )
                .await?;
                let (objects, copies) =
                    verify_pointer_dependencies(ctx, plan, &catalog, &pointers, None).await?;
                (changed_paths, candidate.clone(), objects, copies)
            }
            Some(scope) => {
                let filtered_router = crab_storage::StoreLayout::with_global_prefix(
                    ctx.store().clone(),
                    scope.repo_prefix.clone(),
                    scope.global_prefix.clone(),
                );
                let filtered_view = crab_read::capsule_protocol::open_view(
                    &filtered_router,
                    crab_read::capsule_protocol::CapsuleReadLimits {
                        max_capsule_bytes: MAX_PROTECTED_CAPSULE_BYTES,
                        max_frontier_bytes: MAX_PROTECTED_CAPSULE_BYTES,
                    },
                )
                .await?;
                if filtered_view.root().digest() != plan.base_root_digest {
                    return Err(conflict("filtered capsule root changed since prepare"));
                }
                validate_ref_heads(&prepare.view_ref_updates, filtered_view.refs())?;
                let candidate_delta = candidate.pointer_catalog_delta()?.unwrap_or_default();
                let mut candidate_catalog = filtered_view.pointer_catalog()?;
                candidate_catalog.apply(&candidate_delta)?;
                let materialized = materialize_capsule_source_push(
                    ctx.router(),
                    &source_view,
                    &filtered_view,
                    candidate,
                    &plan.ref_updates,
                    &prepare.source_ref_updates,
                    &plan.transaction_id,
                    MAX_PROTECTED_CAPSULE_BYTES,
                )
                .await?;
                let source_catalog = source_view.pointer_catalog()?;
                let delta = source_pointer_delta(
                    &source_catalog,
                    &candidate_catalog,
                    &candidate_delta,
                    &materialized.pointers,
                )?;
                let mut final_catalog = source_catalog;
                final_catalog.apply(&delta)?;
                let (objects, copies) = verify_pointer_dependencies(
                    ctx,
                    plan,
                    &final_catalog,
                    &materialized.pointers,
                    Some(&filtered_router),
                )
                .await?;
                let mut sections = vec![CapsuleSection::new(
                    CapsuleSectionKind::VisibilityDelta,
                    materialized.visibility.encode()?,
                )];
                if !delta.is_empty() {
                    sections.push(CapsuleSection::new(
                        CapsuleSectionKind::CatalogDelta,
                        delta.encode_delta()?,
                    ));
                }
                let publication =
                    Capsule::build(&materialized.transaction, materialized.git_packs, sections)?;
                (materialized.changed_paths, publication, objects, copies)
            }
        };
    let publication_transaction = publication.transaction()?;
    if !capsule_is_visible(
        source_view.refs(),
        source_view.visible_ref_transactions(),
        &publication_transaction,
    ) {
        validate_ref_heads(&prepare.source_ref_updates, source_view.refs())?;
    }
    let mut replication_objects = replication_objects;
    replication_objects.remove(ctx.router().capsule_path(&plan.run_hash).as_ref());
    let publication_run = crab_write::capsule_protocol::capsule_leaf_run(&publication)?;
    replication_objects.insert(
        ctx.router()
            .capsule_path(publication_run.hash())
            .to_string(),
    );
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
        publication,
        dependency_copies,
    })
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

fn validate_ref_heads(updates: &[PushRefUpdate], current: &BTreeMap<String, String>) -> Result<()> {
    for update in updates {
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
    use crab_metadata::capsule_protocol::{
        CapsuleRefEdit, CapsuleTransaction, FileCatalogEntry, PointerCatalog, ShardCatalogEntry,
        XorbCatalogEntry, XorbChunkEntry,
    };
    use crab_types::pointer::Pointer;
    use crab_xet::hash::MerkleHash;

    use super::*;
    use crate::git_pointer_scan::ReachablePointerScan;

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

    #[test]
    fn scoped_source_delta_carries_external_xorb_and_shard_closure() {
        let file_hash = hash('f');
        let shard_hash = hash('d');
        let xorb_hash = hash('e');
        let mut candidate = PointerCatalog::new();
        candidate
            .insert_xorb(
                xorb_hash.clone(),
                XorbCatalogEntry::new(12, hash('b'), vec![XorbChunkEntry::new(hash('c'), 7)]),
            )
            .unwrap();
        candidate
            .insert_shard(
                shard_hash.clone(),
                ShardCatalogEntry::new(9, vec![xorb_hash.clone()]),
            )
            .unwrap();
        candidate
            .insert_file(
                file_hash.clone(),
                FileCatalogEntry::new(7, shard_hash.clone()),
            )
            .unwrap();
        let delta = source_pointer_delta(
            &PointerCatalog::new(),
            &candidate,
            &candidate,
            &ReachablePointerScan {
                crab_pointers: vec![Pointer {
                    file_hash: MerkleHash::from_hex(&file_hash).unwrap().into(),
                    size: 7,
                    shard_hint: None,
                }],
                lfs_pointers: Vec::new(),
            },
        )
        .unwrap();

        assert!(delta.files().contains_key(&file_hash));
        assert!(delta.shards().contains_key(&shard_hash));
        assert!(delta.xorbs().contains_key(&xorb_hash));
    }
}
