//! Canonical protocol-v2 Git push path.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use crab_staging::StagingAreaReadOnly;
use crab_xet::hash::MerkleHash;
use gix_object::{Exists, Find, FindHeader};
use tokio_util::sync::CancellationToken;

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::metrics::Metrics;
use crate::git::pack::{
    PushPackConfig, RemotePackExclusions, generate_push_pack_files_with_exclusions,
    install_pack_file_locally_with_timeout,
};
use crate::git::push::{
    PushConfig, PushRejectReason, PushResult, PushTransferStats, RefPushOutcome, RefUpdate,
    check_ref_update, duplicate_destination_result,
};
use crate::git::remote_helper::PushSpec;

const POINTER_SCAN_ALLOCATION_BYTES: usize = 64 * 1024 * 1024;

/// Publish one remote-helper push batch through per-ref capsule heads.
///
/// Ref policy and Git-integrity checks complete before immutable pointer data is
/// uploaded. A single-ref head CAS or the per-attempt multi-ref transaction
/// protocol publishes refs; failed preparation can leave only safe immutable orphans.
#[expect(
    clippy::too_many_arguments,
    reason = "push pipeline dependencies are explicit"
)]
pub async fn run(
    config: &PushConfig,
    specs: &[PushSpec],
    store: &crate::storage::store::Store,
    router: &crate::storage::StoreLayout,
    advertised: Option<crab_read::capsule_protocol::CapsuleRefView>,
    hidden_ref_patterns: &[String],
    staging: Option<&Arc<StagingAreaReadOnly>>,
    caching_store: Option<&crab_cache_store::CachingStore>,
    metrics: Option<&Metrics>,
    cancel: &CancellationToken,
) -> Result<(
    PushResult,
    Option<crab_read::capsule_protocol::CapsuleRefView>,
)> {
    validate_publication_plan_context(config)?;
    let Some(plan_id) = config.mirror_plan_id.as_deref() else {
        return run_inner(
            config,
            specs,
            store,
            router,
            advertised,
            hidden_ref_patterns,
            staging,
            caching_store,
            metrics,
            cancel,
        )
        .await;
    };
    let layout = crab_storage::StoreLayout::with_global_prefix(
        store.as_storage().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    crab_remote::publication::with_capsule_plan(
        store.as_storage(),
        &layout,
        plan_id,
        config.lock_ttl,
        cancel,
        |cancel| async move {
            run_inner(
                config,
                specs,
                store,
                router,
                advertised,
                hidden_ref_patterns,
                staging,
                caching_store,
                metrics,
                &cancel,
            )
            .await
        },
    )
    .await
}

fn validate_publication_plan_context(config: &PushConfig) -> Result<()> {
    let Some(plan_id) = config.mirror_plan_id.as_deref() else {
        return Ok(());
    };
    if plan_id.len() != 64
        || !plan_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(CrabError::Protocol(
            "mirror plan identity must be 64 lowercase hexadecimal characters".to_owned(),
        ));
    }
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "push pipeline dependencies are explicit"
)]
async fn run_inner(
    config: &PushConfig,
    specs: &[PushSpec],
    store: &crate::storage::store::Store,
    router: &crate::storage::StoreLayout,
    advertised: Option<crab_read::capsule_protocol::CapsuleRefView>,
    hidden_ref_patterns: &[String],
    staging: Option<&Arc<StagingAreaReadOnly>>,
    caching_store: Option<&crab_cache_store::CachingStore>,
    metrics: Option<&Metrics>,
    cancel: &CancellationToken,
) -> Result<(
    PushResult,
    Option<crab_read::capsule_protocol::CapsuleRefView>,
)> {
    if let Some(result) = duplicate_destination_result(specs) {
        return Ok((result, advertised));
    }
    if cancel.is_cancelled() {
        return Err(CrabError::Cancelled);
    }

    let layout = crab_storage::StoreLayout::with_global_prefix(
        store.as_storage().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    let view = match advertised {
        Some(view) => view,
        None => {
            let root = crab_write::capsule_protocol::open_root(&layout).await?;
            let requested_refs = specs
                .iter()
                .map(|spec| spec.dst.clone())
                .collect::<BTreeSet<_>>();
            crab_read::capsule_protocol::open_ref_view_from_root_for_push(
                &layout,
                root,
                &requested_refs,
            )
            .await?
        }
    };
    let base = view.root_snapshot().clone();
    let push_ref_head_bases = view.push_ref_head_bases().clone();
    let git_dir = config
        .git_dir
        .clone()
        .map_or_else(super::discover::discover_git_dir, Ok)?;
    let common_git_dir = super::discover::resolve_common_dir(&git_dir);
    let source_names = specs
        .iter()
        .filter(|spec| !spec.src.is_empty())
        .map(|spec| spec.src.as_str())
        .collect::<Vec<_>>();
    let source_oids = crab_git::ref_resolve::resolve_refs_batch_at(&git_dir, &source_names)?;
    let peeled = crab_git::tag::peeled_revision_targets_at(
        &common_git_dir,
        &source_oids
            .iter()
            .map(|(name, oid)| (name.clone(), oid.clone()))
            .collect(),
    )?;
    let hidden = hidden_ref_matcher(hidden_ref_patterns)?;
    let root = base.record().root();
    let remote_refs = view.refs();
    let mut outcomes = HashMap::with_capacity(specs.len());
    let mut edits = Vec::with_capacity(specs.len());
    let mut updates = Vec::with_capacity(specs.len());
    let mut fast_forward_refs = BTreeSet::new();

    for spec in specs {
        if hidden.is_match(&spec.dst) {
            outcomes.insert(
                spec.dst.clone(),
                RefPushOutcome::Rejected(PushRejectReason::UnknownRefname {
                    name: spec.dst.clone(),
                }),
            );
            continue;
        }
        let current = remote_refs.get(&spec.dst).cloned();
        if config
            .expected_refs
            .get(&spec.dst)
            .is_some_and(|expected| expected != &current)
        {
            outcomes.insert(
                spec.dst.clone(),
                RefPushOutcome::Rejected(PushRejectReason::StaleInfo),
            );
            continue;
        }
        if spec.src.is_empty() {
            if current.is_none() {
                outcomes.insert(spec.dst.clone(), RefPushOutcome::Ok);
            } else if config.receive_deny_deletes {
                outcomes.insert(
                    spec.dst.clone(),
                    RefPushOutcome::Rejected(PushRejectReason::DenyDeletes),
                );
            } else if current_branch_is_denied(config, root.head(), &spec.dst) {
                outcomes.insert(
                    spec.dst.clone(),
                    RefPushOutcome::Rejected(PushRejectReason::DenyCurrentBranch),
                );
            } else {
                edits.push(crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                    spec.dst.clone(),
                    current,
                    None,
                    None,
                ));
            }
            continue;
        }

        let new_oid = source_oids.get(&spec.src).cloned().ok_or_else(|| {
            CrabError::Internal(format!("resolved push source {} is absent", spec.src))
        })?;
        if current.as_deref() == Some(new_oid.as_str()) {
            outcomes.insert(spec.dst.clone(), RefPushOutcome::Ok);
            continue;
        }
        if current_branch_is_denied(config, root.head(), &spec.dst) {
            outcomes.insert(
                spec.dst.clone(),
                RefPushOutcome::Rejected(PushRejectReason::DenyCurrentBranch),
            );
            continue;
        }
        let update = RefUpdate {
            ref_name: spec.dst.clone(),
            old_sha: current.clone(),
            new_sha: new_oid.clone(),
            force: spec.force || config.expected_refs.contains_key(&spec.dst),
        };
        if let Some(old_oid) = current.as_deref() {
            let is_fast_forward = if spec.dst.starts_with("refs/tags/") {
                Some(false)
            } else {
                is_ancestor(&common_git_dir, old_oid, &new_oid)?
            };
            let is_fast_forward = match is_fast_forward {
                Some(proven) => proven,
                // A forced rewrite need not have fetched the remote tip. The ref
                // lease was checked above; publication still compares the old tip.
                None if update.force && !config.receive_deny_non_fast_forwards => false,
                None => {
                    outcomes.insert(
                        spec.dst.clone(),
                        RefPushOutcome::Rejected(PushRejectReason::StaleInfo),
                    );
                    continue;
                }
            };
            let decision = if !is_fast_forward && config.receive_deny_non_fast_forwards {
                Err(PushRejectReason::DenyNonFastForward)
            } else {
                match check_ref_update(&update, is_fast_forward, None) {
                    crate::git::push::RefUpdateDecision::Proceed { .. } => Ok(()),
                    crate::git::push::RefUpdateDecision::Reject(reason) => Err(reason),
                }
            };
            if let Err(reason) = decision {
                outcomes.insert(spec.dst.clone(), RefPushOutcome::Rejected(reason));
                continue;
            }
            if is_fast_forward {
                fast_forward_refs.insert(spec.dst.clone());
            }
        }
        edits.push(crab_metadata::capsule_protocol::CapsuleRefEdit::new(
            spec.dst.clone(),
            current,
            Some(new_oid.clone()),
            peeled.get(&spec.src).cloned(),
        ));
        updates.push(update);
    }

    if let Some(blocked_by) = outcomes.iter().find_map(|(name, outcome)| {
        matches!(outcome, RefPushOutcome::Rejected(_)).then(|| name.clone())
    }) && config.atomic
    {
        for spec in specs {
            outcomes.entry(spec.dst.clone()).or_insert_with(|| {
                RefPushOutcome::Rejected(PushRejectReason::AtomicAbort {
                    blocked_by: blocked_by.clone(),
                })
            });
        }
        return Ok((PushResult::new(outcomes), Some(view)));
    }

    if edits.is_empty() {
        return Ok((PushResult::new(outcomes), Some(view)));
    }
    validate_candidate_namespace(remote_refs, &edits).map_err(|reason| {
        CrabError::Protocol(format!(
            "capsule-protocol ref transaction is invalid: {reason}"
        ))
    })?;
    if cancel.is_cancelled() {
        return Err(CrabError::Cancelled);
    }

    let prepared = prepare_git_packs(
        &common_git_dir,
        remote_refs,
        &updates,
        config.receive_max_input_size,
        config.force_full_graph,
    )
    .await?;
    if !prepared.pointers.is_empty()
        && let Some(staging) = staging
        && !staging.unresolved_publication_intents()?.is_empty()
    {
        // A native add can install Git's pointer before its local publication
        // acknowledgement. Recover under the canonical index lock before admission.
        let worktree = gix::open(&git_dir)
            .map_err(|error| CrabError::Io(std::io::Error::other(error)))?
            .workdir()
            .map(Path::to_path_buf)
            .ok_or_else(|| {
                CrabError::StagingCorrupt(
                    "native publication recovery requires a Git working tree".to_owned(),
                )
            })?;
        super::index::reconcile_publication_intents(staging, &worktree, cancel).await?;
    }

    let staging_snapshot = capture_staging_snapshot(staging, &prepared.pointers)?;
    let visibility_delta = prepare_visibility_delta(
        &common_git_dir,
        &edits,
        &fast_forward_refs,
        prepared.visibility_objects.as_deref(),
    )?;
    tracing::debug!(
        git_packs = prepared.packs.len(),
        pointers = prepared.pointers.len(),
        "prepared capsule-protocol Git payload"
    );
    let gc_writer = if prepared.pointers.is_empty() || config.protected_push.is_some() {
        None
    } else {
        Some(
            crate::maintenance::GcWriterLeases::acquire(
                store,
                router.global_prefix(),
                router.repo_prefix(),
                cancel,
            )
            .await?,
        )
    };
    let ref_names = edits
        .iter()
        .map(|edit| edit.ref_name().to_owned())
        .collect::<Vec<_>>();
    let changes_namespace = edits
        .iter()
        .any(|edit| edit.expected_old().is_none() != edit.new_oid().is_none());
    let lfs_tips = updates
        .iter()
        .map(|update| update.new_sha.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let lfs_remote_tips = if config.force_full_graph {
        Vec::new()
    } else {
        updates
            .iter()
            .filter_map(|update| update.old_sha.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    };
    let mut xet_stats = super::xet_publication::XetPublicationStats::default();
    let mut pointer_delta = crab_metadata::capsule_protocol::PointerCatalog::new();
    // Preparation retains large proof maps under GC admission; heap-pin it so
    // serving and helper callers do not carry that state on their async stack.
    let publication: Result<Option<Option<crab_coordination::write_coordinator::CommitOutcome>>> =
        Box::pin(async {
            // LFS bytes share the ref visibility boundary with Git and Xet data.
            // Publishing them here also covers mirror batches that own hook stdin.
            let lfs_objects = crate::lfs::publication::publish_reachable_objects(
                store.as_storage().clone(),
                router.repo_prefix().to_owned(),
                common_git_dir.clone(),
                lfs_tips,
                lfs_remote_tips,
                cancel,
            )
            .await?;
            let (prepared_pointer_delta, prepared_xet_stats) =
                super::xet_publication::prepare_delta(
                    &layout,
                    &base,
                    &prepared.pointers,
                    staging,
                    caching_store,
                    metrics,
                    config.upload_concurrency,
                    config.protected_push.is_none(),
                    cancel,
                )
                .await?;
            xet_stats = prepared_xet_stats;
            pointer_delta = prepared_pointer_delta;
            if config.protected_push.is_none()
                && let Some(replication) = config.active_active_replication.as_ref()
            {
                crate::replication::register_active_active_coordinator_for_repo(
                    store,
                    router,
                    replication,
                )
                .await?;
            }
            let transaction = match config.mirror_plan_id.as_deref() {
                Some(plan_id) => crab_metadata::capsule_protocol::CapsuleTransaction::for_plan(
                    base.record().digest(),
                    plan_id,
                    edits,
                )?,
                None => crab_metadata::capsule_protocol::CapsuleTransaction::new(
                    base.record().digest(),
                    edits,
                )?,
            };
            let mut sections = Vec::with_capacity(2);
            if !pointer_delta.is_empty() {
                sections.push(crab_metadata::capsule_protocol::CapsuleSection::new(
                    crab_metadata::capsule_protocol::CapsuleSectionKind::CatalogDelta,
                    pointer_delta.encode_delta()?,
                ));
            }
            if let Some(visibility_delta) = visibility_delta {
                sections.push(crab_metadata::capsule_protocol::CapsuleSection::new(
                    crab_metadata::capsule_protocol::CapsuleSectionKind::VisibilityDelta,
                    visibility_delta.encode()?,
                ));
            }
            let capsule = crab_metadata::capsule_protocol::Capsule::build(
                &transaction,
                prepared.packs,
                sections,
            )?;
            check_cancelled(cancel)?;
            if let Some(session) = config.protected_push.as_ref() {
                let outcome = super::protected_push::finalize_capsule_push(
                    session,
                    store,
                    router,
                    &transaction,
                    &capsule,
                    config.upload_concurrency,
                    cancel,
                )
                .await?;
                return Ok(Some(outcome));
            }
            let payload_objects = if config.active_active_replication.is_some() {
                pointer_delta
                    .xorbs()
                    .keys()
                    .map(|hash| layout.xorb_path(hash).to_string())
                    .chain(
                        pointer_delta
                            .shards()
                            .keys()
                            .map(|hash| layout.shard_path(hash).to_string()),
                    )
                    .chain(lfs_objects)
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let result = if changes_namespace {
                let commit_layout = layout.clone();
                let commit_push_ref_head_bases = push_ref_head_bases.clone();
                crab_write::with_ref_namespaces_wait(
                    layout.store(),
                    &layout,
                    &ref_names,
                    config.lock_ttl,
                    config.lock_wait,
                    cancel,
                    |scoped| async move {
                        if scoped.is_cancelled() {
                            return Err(CrabError::Cancelled);
                        }
                        crab_write::capsule_protocol::validate_ref_namespace(
                            &commit_layout,
                            base.record().root(),
                            transaction.edits(),
                        )
                        .await?;
                        publish_capsule(
                            config,
                            &commit_layout,
                            base,
                            &transaction,
                            &capsule,
                            &payload_objects,
                            &commit_push_ref_head_bases,
                            &scoped,
                        )
                        .await
                    },
                )
                .await
            } else {
                publish_capsule(
                    config,
                    &layout,
                    base,
                    &transaction,
                    &capsule,
                    &payload_objects,
                    &push_ref_head_bases,
                    cancel,
                )
                .await
            };
            match result {
                Ok(CapsulePublishAttempt::Committed(outcome)) => Ok(Some(outcome)),
                Ok(CapsulePublishAttempt::RefChanged) => Ok(None),
                Err(error) => Err(error),
            }
        })
        .await;
    // Root visibility precedes advisory discovery, while global admission
    // still pins immutable hint pages until their own root becomes durable.
    let hint_indexed = if publication.as_ref().is_ok_and(Option::is_some)
        && layout.store().staging_write_prefix().is_none()
    {
        match crab_metadata::capsule_protocol::write_capsule_chunk_candidates(
            &layout,
            &pointer_delta,
            &xet_stats.origins,
            &|| cancel.is_cancelled(),
        )
        .await
        {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(%error, "capsule committed; chunk candidate index needs repair");
                false
            }
        }
    } else {
        false
    };
    let release = match gc_writer {
        Some(writer) => writer.release().await,
        None => Ok(()),
    };
    let committed = match (publication, release) {
        (Ok(Some(committed)), release) => {
            // Visibility already owns the uploaded payload. Late cancellation
            // or fence cleanup failure cannot turn that commit into rejection.
            if let Err(error) = release {
                tracing::warn!(%error, "capsule committed; GC admission cleanup requires repair");
            }
            Some(committed)
        }
        (Ok(None), release) => {
            release?;
            None
        }
        (Err(error), release) => {
            if let Err(release_error) = release {
                tracing::warn!(
                    error = %release_error,
                    "capsule-protocol GC admission release failed after push failure"
                );
            }
            return Err(error);
        }
    };
    if let Some(active_active_commit) = committed {
        if hint_indexed {
            let cache_path = crate::cache::add_remote_candidate_cache_path(
                &crate::cache::default_cache_root(),
                &store.bucket_identity(),
                layout.global_prefix(),
            );
            if cache_path.is_file() {
                let hashes = pointer_delta
                    .xorbs()
                    .values()
                    .flat_map(|entry| entry.chunks())
                    .filter_map(|chunk| MerkleHash::from_hex(chunk.hash()).ok())
                    .collect::<Vec<_>>();
                let invalidation = tokio::task::spawn_blocking(move || {
                    crate::cache::add_remote_candidates::AddRemoteCandidateCache::open(&cache_path)
                        .and_then(|cache| cache.forget_negatives(&hashes))
                })
                .await;
                if !matches!(invalidation, Ok(Ok(()))) {
                    tracing::warn!("capsule committed; candidate cache invalidation failed");
                }
            }
        }
        if let Some(snapshot) = staging_snapshot
            && let Err(error) = snapshot.commit()
        {
            tracing::warn!(%error, "capsule committed; local staging cleanup needs maintenance");
        }
        for ref_name in ref_names {
            outcomes.insert(ref_name, RefPushOutcome::Ok);
        }
        let result = match active_active_commit {
            Some(outcome) => PushResult::new(outcomes).with_active_active_commit(outcome.into()),
            None => PushResult::new(outcomes),
        };
        return Ok((
            result.with_transfer_stats(PushTransferStats {
                xorbs_uploaded: xet_stats.xorbs_uploaded,
                shards_uploaded: xet_stats.shards_uploaded,
                xorb_bytes_uploaded: xet_stats.xorb_bytes_uploaded,
            }),
            None,
        ));
    }
    for ref_name in ref_names {
        outcomes.insert(
            ref_name,
            RefPushOutcome::Rejected(PushRejectReason::StaleInfo),
        );
    }
    Ok((PushResult::new(outcomes), None))
}

fn capture_staging_snapshot(
    staging: Option<&Arc<StagingAreaReadOnly>>,
    pointers: &[crab_types::pointer::Pointer],
) -> Result<Option<crab_staging::StagingPushSnapshot>> {
    let Some(staging) = staging.filter(|_| !pointers.is_empty()) else {
        return Ok(None);
    };
    // Fence sibling retirement before selecting local roots; snapshot pins
    // then protect exactly those leases through upload and the ref transition.
    let snapshot = crab_staging::StagingPushSnapshot::begin(
        Arc::clone(staging),
        uuid::Uuid::now_v7().to_string(),
    )?;
    let hashes = pointers
        .iter()
        .map(|pointer| MerkleHash::from(pointer.file_hash))
        .collect::<Vec<_>>();
    let selected = super::push_staging::recipes_for_selected_pointers(staging, &hashes)?;
    for pointer in pointers {
        if let Some(Some(recipe)) = selected.get(&MerkleHash::from(pointer.file_hash))
            && recipe.file_size() != pointer.size
        {
            return Err(CrabError::StagingCorrupt(
                "selected push recipe disagrees with pointer size".to_owned(),
            ));
        }
    }
    let recipes = selected.into_values().flatten().collect::<Vec<_>>();
    if recipes.is_empty() {
        return Ok(None);
    }
    snapshot.capture(&recipes)?;
    Ok(Some(snapshot))
}

enum CapsulePublishAttempt {
    Committed(Option<crab_coordination::write_coordinator::CommitOutcome>),
    RefChanged,
}

async fn publish_capsule(
    config: &PushConfig,
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    base: crab_metadata::capsule_protocol::RootSnapshot,
    transaction: &crab_metadata::capsule_protocol::CapsuleTransaction,
    capsule: &crab_metadata::capsule_protocol::Capsule,
    payload_objects: &[String],
    push_ref_head_bases: &BTreeMap<String, Option<(Bytes, crab_storage::ETag)>>,
    cancel: &CancellationToken,
) -> Result<CapsulePublishAttempt> {
    check_cancelled(cancel)?;
    let Some(replication) = config.active_active_replication.as_ref() else {
        let publish = async {
            let prepared = crab_write::capsule_protocol::prepare_publication_with_ref_head_bases(
                layout,
                base,
                transaction,
                capsule,
                push_ref_head_bases,
            )
            .await?;
            // Ref/GC renewal may fail while immutable preparation runs. Check
            // its scoped token before visibility, then resolve the commit fully.
            if cancel.is_cancelled() {
                return Err(crab_write::WriteError::Cancelled);
            }
            crab_write::capsule_protocol::publish_prepared(layout, prepared).await
        }
        .await;
        return match publish {
            Ok(_) => Ok(CapsulePublishAttempt::Committed(None)),
            Err(crab_write::WriteError::RefChanged { .. }) => Ok(CapsulePublishAttempt::RefChanged),
            Err(error) => Err(error.into()),
        };
    };
    let coordinator =
        config
            .active_active_coordinator
            .as_ref()
            .ok_or_else(|| CrabError::Configuration {
                key: "replication.coordinator".to_owned(),
                origin: "protocol-v2 active-active push requires a live coordinator".to_owned(),
            })?;
    let prepared =
        crab_write::capsule_protocol::prepare_coordinated_publication_with_ref_head_bases(
            layout,
            base,
            transaction,
            capsule,
            push_ref_head_bases,
        )
        .await?;
    let descriptor = prepared.descriptor().clone();
    let refs = transaction
        .edits()
        .iter()
        .map(
            |edit| crab_coordination::write_coordinator::CoordinatedRefUpdate {
                name: edit.ref_name().to_owned(),
                expected: edit.expected_old().map(str::to_owned),
                new: edit.new_oid().map(str::to_owned),
                // Git force policy was already evaluated. Consensus must still
                // compare the exact old value authenticated by the transaction.
                force: false,
            },
        )
        .collect::<Vec<_>>();
    let mut uploaded_objects = BTreeSet::new();
    uploaded_objects.insert(layout.capsule_path(&descriptor.run_hash).to_string());
    uploaded_objects.extend(payload_objects.iter().cloned());
    if let Some(plan_id) = transaction.plan_id() {
        uploaded_objects.insert(layout.capsule_plan_intent_path(plan_id).to_string());
    }
    let plan = crate::replication::plan_active_active_capsule_push(
        replication,
        config.active_active_writer.as_deref(),
        descriptor,
        refs,
        uploaded_objects.into_iter().collect(),
    )?;
    check_cancelled(cancel)?;
    let mut outcome = match crab_coordination::write_coordinator::commit_uploaded_push_refs(
        coordinator.as_ref(),
        plan.request.clone(),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(crab_coordination::CoordinationError::NonFastForward { .. }) => {
            return Ok(CapsulePublishAttempt::RefChanged);
        }
        Err(error) => return Err(error.into()),
    };
    match crab_write::capsule_protocol::materialize_coordinated_publication(layout, prepared).await
    {
        Ok(_) => {
            match coordinator
                .as_ref()
                .mark_region_materialized(&outcome.operation_id, &plan.request.region)
                .await
            {
                Ok(state) => outcome.state = state,
                Err(error) => tracing::warn!(
                    %error,
                    operation_id = %outcome.operation_id,
                    "protocol-v2 coordinator commit succeeded; materialization acknowledgement requires repair"
                ),
            }
        }
        Err(error) => tracing::warn!(
            %error,
            operation_id = %outcome.operation_id,
            "protocol-v2 coordinator commit succeeded; local capsule materialization requires repair"
        ),
    }
    Ok(CapsulePublishAttempt::Committed(Some(outcome)))
}

fn prepare_visibility_delta(
    git_dir: &Path,
    edits: &[crab_metadata::capsule_protocol::CapsuleRefEdit],
    fast_forward_refs: &BTreeSet<String>,
    fast_forward_pack_objects: Option<&[gix_hash::ObjectId]>,
) -> Result<Option<crab_metadata::capsule_protocol::CapsuleVisibilityDelta>> {
    let maximum = usize::try_from(crab_metadata::git_visibility::MAX_GIT_VISIBILITY_OBJECTS)
        .map_err(|_| CrabError::Internal("Git visibility limit does not fit usize".to_owned()))?;
    let rewritten_old_refs = edits
        .iter()
        .filter(|edit| edit.new_oid().is_some() && !fast_forward_refs.contains(edit.ref_name()))
        .filter_map(|edit| {
            edit.expected_old()
                .map(|oid| (edit.ref_name().to_owned(), oid.to_owned()))
        })
        .collect();
    let available_old_tips = locally_available_remote_tips(git_dir, &rewritten_old_refs)?;
    let mut visibility = BTreeMap::new();
    for edit in edits {
        let Some(new_oid) = edit.new_oid() else {
            continue;
        };
        let evidence = if let Some(pack_objects) = fast_forward_pack_objects
            && edits.len() == 1
            && fast_forward_refs.contains(edit.ref_name())
            && let Some(old_oid) = edit.expected_old()
            && let Ok(new_object_id) = gix_hash::ObjectId::from_hex(new_oid.as_bytes())
            && pack_objects.contains(&new_object_id)
        {
            // The pack excludes only this ref's old tip, so its members are the
            // complete visibility delta; another excluded tip could hide new objects.
            crab_metadata::git_visibility::GitVisibilityEdit::from_delta_objects(
                Some(old_oid.to_owned()),
                new_oid.to_owned(),
                pack_objects.iter().map(ToString::to_string).collect(),
                Vec::new(),
            )
        } else if let Some(old_oid) = edit.expected_old()
            && (fast_forward_refs.contains(edit.ref_name())
                || available_old_tips.iter().any(|tip| tip == old_oid))
        {
            let added = super::push::enumerate_visibility_difference(
                git_dir,
                new_oid,
                Some(old_oid),
                maximum,
            )?
            .ok_or_else(|| visibility_limit_error(edit.ref_name()))?;
            // Ancestry is already proven by the ref-update decision; old history is
            // therefore still reachable, so walking `old - new` only repeats that proof.
            let removed = if fast_forward_refs.contains(edit.ref_name()) {
                Vec::new()
            } else {
                super::push::enumerate_visibility_difference(
                    git_dir,
                    old_oid,
                    Some(new_oid),
                    maximum.saturating_sub(added.len()),
                )?
                .ok_or_else(|| visibility_limit_error(edit.ref_name()))?
            };
            crab_metadata::git_visibility::GitVisibilityEdit::from_delta_objects(
                Some(old_oid.to_owned()),
                new_oid.to_owned(),
                added,
                removed,
            )
        } else {
            // Rewrites may lack the old tip locally. A complete replacement
            // closure revokes its visibility without fetching discarded history.
            let objects =
                super::push::enumerate_visibility_difference(git_dir, new_oid, None, maximum)?
                    .ok_or_else(|| visibility_limit_error(edit.ref_name()))?;
            crab_metadata::git_visibility::GitVisibilityEdit::from_replacement_objects(
                edit.expected_old().map(str::to_owned),
                new_oid.to_owned(),
                objects,
            )
        };
        evidence.validate()?;
        visibility.insert(edit.ref_name().to_owned(), evidence);
    }
    if visibility.is_empty() {
        Ok(None)
    } else {
        crab_metadata::capsule_protocol::CapsuleVisibilityDelta::new(visibility)
            .map(Some)
            .map_err(Into::into)
    }
}

fn visibility_limit_error(ref_name: &str) -> CrabError {
    CrabError::Protocol(format!(
        "Git visibility for {ref_name} exceeds the protocol-v2 object limit"
    ))
}

fn current_branch_is_denied(config: &PushConfig, head: &str, destination: &str) -> bool {
    destination == head
        && config
            .receive_deny_current_branch
            .eq_ignore_ascii_case("refuse")
}

fn hidden_ref_matcher(patterns: &[String]) -> Result<globset::GlobSet> {
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(globset::Glob::new(pattern).map_err(|error| {
            CrabError::Protocol(format!("invalid transfer.hideRefs pattern: {error}"))
        })?);
    }
    builder.build().map_err(|error| {
        CrabError::Protocol(format!("invalid transfer.hideRefs patterns: {error}"))
    })
}

fn is_ancestor(git_dir: &Path, old_oid: &str, new_oid: &str) -> Result<Option<bool>> {
    if has_direct_parent(git_dir, old_oid, new_oid) {
        return Ok(Some(true));
    }

    let output = std::process::Command::new("git")
        .args(["--git-dir"])
        .arg(git_dir)
        .args(["merge-base", "--is-ancestor", old_oid, new_oid])
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "")
        .output()?;
    match output.status.code() {
        Some(0) => Ok(Some(true)),
        Some(1) => Ok(Some(false)),
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if super::push::is_missing_object_error(&stderr) {
                return Ok(None);
            }
            Err(CrabError::Internal(format!(
                "git merge-base could not prove ancestry: {}",
                stderr.trim()
            )))
        }
    }
}

// A direct parent edge proves fast-forward without scanning prior history.
// Other shapes keep merge-base so missing-object and shallow outcomes remain unchanged.
fn has_direct_parent(git_dir: &Path, old_oid: &str, new_oid: &str) -> bool {
    let (Ok(old_oid), Ok(new_oid)) = (
        gix_hash::ObjectId::from_hex(old_oid.as_bytes()),
        gix_hash::ObjectId::from_hex(new_oid.as_bytes()),
    ) else {
        return false;
    };
    let old_commit = format!("{old_oid}^{{commit}}");
    let old_exists = std::process::Command::new("git")
        .args(["--git-dir"])
        .arg(git_dir)
        .args(["cat-file", "-e", &old_commit])
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if !old_exists.is_ok_and(|status| status.success()) {
        return false;
    }
    let new_oid = new_oid.to_string();
    let output = std::process::Command::new("git")
        .args(["--git-dir"])
        .arg(git_dir)
        .args(["rev-list", "--parents", "-n", "1", &new_oid])
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "")
        .output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let Ok(output) = String::from_utf8(output.stdout) else {
        return false;
    };
    let mut fields = output.split_whitespace();
    if fields.next() != Some(new_oid.as_str()) {
        return false;
    }
    fields
        .filter_map(|parent| gix_hash::ObjectId::from_hex(parent.as_bytes()).ok())
        .any(|parent| parent == old_oid)
}

fn validate_candidate_namespace(
    base_refs: &BTreeMap<String, String>,
    edits: &[crab_metadata::capsule_protocol::CapsuleRefEdit],
) -> std::result::Result<(), crab_git::refname::RefNamespaceError> {
    let mut refs = base_refs.clone();
    for edit in edits {
        match edit.new_oid() {
            Some(oid) => {
                refs.insert(edit.ref_name().to_owned(), oid.to_owned());
            }
            None => {
                refs.remove(edit.ref_name());
            }
        }
    }
    crab_git::refname::validate_ref_namespace(refs.keys().map(String::as_str))
}

async fn prepare_git_packs(
    git_dir: &Path,
    remote_refs: &BTreeMap<String, String>,
    updates: &[RefUpdate],
    max_input_size: u64,
    force_full_graph: bool,
) -> Result<PreparedGitPush> {
    if updates.is_empty() {
        return Ok(PreparedGitPush::default());
    }
    let excluded_tips = if force_full_graph {
        None
    } else {
        Some(locally_available_remote_tips(git_dir, remote_refs)?)
    };
    let pack_is_exact_visibility_delta = if !force_full_graph
        && let [update] = updates
        && !update.force
        && let Some(old_sha) = update.old_sha.as_ref()
        && remote_refs.get(&update.ref_name) == Some(old_sha)
    {
        excluded_tips
            .as_ref()
            .is_some_and(|tips| tips.contains(old_sha) && tips.iter().all(|tip| tip == old_sha))
    } else {
        false
    };
    let exclusions = excluded_tips.as_deref().map(RemotePackExclusions::RefTips);
    let generated = generate_push_pack_files_with_exclusions(
        updates,
        exclusions,
        &PushPackConfig {
            thin_packs: false,
            max_input_size,
            git_dir: Some(git_dir.to_owned()),
        },
    )
    .await?;
    prepare_generated_git_packs(
        git_dir,
        generated,
        max_input_size,
        pack_is_exact_visibility_delta,
    )
    .await
}

#[derive(Default)]
struct PreparedGitPush {
    packs: Vec<crab_metadata::capsule_protocol::CapsuleGitPack>,
    pointers: Vec<crab_types::pointer::Pointer>,
    /// Exact new-ref object set only when pack exclusions equal its prior tip.
    visibility_objects: Option<Vec<gix_hash::ObjectId>>,
}

async fn prepare_generated_git_packs(
    git_dir: &Path,
    generated: Vec<crate::git::pack::PackedFileData>,
    max_input_size: u64,
    retain_visibility_objects: bool,
) -> Result<PreparedGitPush> {
    let evidence_dir = tempfile::Builder::new()
        .prefix(".crab-v2-push-evidence-")
        .tempdir_in(git_dir.join("objects"))?;
    let mut packs = Vec::with_capacity(generated.len());
    let mut pointers = Vec::new();
    let mut visibility_objects = retain_visibility_objects.then(Vec::new);
    let max_visibility_objects =
        crab_metadata::git_visibility::MAX_SYNCHRONOUS_GIT_VISIBILITY_OBJECTS as usize;
    for generated in generated {
        if generated.object_count == 0 {
            continue;
        }
        let installed = install_pack_file_locally_with_timeout(
            evidence_dir.path(),
            generated.pack_path.as_ref(),
            &generated.pack_blake3_hex,
            max_input_size,
            true,
        )
        .await
        .map_err(map_push_pack_error)?;
        let mut locations = crab_git::pack_locator::PackLocationIter::open(
            &installed.idx_path,
            &installed.rev_path,
            generated.pack_size,
        )
        .map_err(crab_git::pack::PackError::from)?;
        if locations.object_count() != generated.object_count
            || locations.pack_checksum().to_string() != installed.git_sha1
        {
            return Err(CrabError::CorruptObject {
                path: generated.pack_path.display().to_string(),
                reason: "generated pack identity disagrees with its verified locator".to_owned(),
            });
        }
        let object_ids = locations
            .by_ref()
            .map(|location| location.map(|location| location.oid))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(crab_git::pack::PackError::from)?;
        if visibility_objects.as_ref().is_some_and(|objects| {
            objects.len().saturating_add(object_ids.len()) > max_visibility_objects
        }) {
            visibility_objects = None;
        }
        if let Some(objects) = &mut visibility_objects {
            objects.extend(object_ids.iter().copied());
        }
        let kinds = crab_git::object_kinds_from_git_dir(git_dir, &object_ids)?;
        pointers.extend(collect_pointers(git_dir, &object_ids, &kinds)?);
        let ordered_kinds = object_ids
            .iter()
            .map(|oid| {
                kinds
                    .get(oid)
                    .copied()
                    .ok_or_else(|| CrabError::CorruptObject {
                        path: generated.pack_path.display().to_string(),
                        reason: format!("Git object-kind query omitted {oid}"),
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        let checksum =
            gix_hash::ObjectId::from_hex(installed.git_sha1.as_bytes()).map_err(|error| {
                CrabError::Internal(format!("generated pack checksum is invalid: {error}"))
            })?;
        let locator = crab_git::pack_locator::encode_pack_kind_metadata(checksum, &ordered_kinds)
            .map_err(crab_git::pack::PackError::from)?;
        packs.push(crab_metadata::capsule_protocol::CapsuleGitPack::new(
            Bytes::from(std::fs::read(
                generated.pack_path.as_ref() as &std::path::Path
            )?),
            Bytes::from(std::fs::read(&installed.idx_path)?),
            Bytes::from(std::fs::read(&installed.rev_path)?),
            Bytes::from(locator),
            installed.git_sha1,
            generated.object_count,
        )?);
    }
    pointers.sort_by_key(|pointer| (pointer.file_hash, pointer.size));
    pointers.dedup_by_key(|pointer| (pointer.file_hash, pointer.size));
    Ok(PreparedGitPush {
        packs,
        pointers,
        visibility_objects,
    })
}

fn locally_available_remote_tips(
    git_dir: &Path,
    remote_refs: &BTreeMap<String, String>,
) -> Result<Vec<String>> {
    let objects = gix_odb::at(git_dir.join("objects")).map_err(|error| {
        CrabError::Internal(format!("failed to open local Git object database: {error}"))
    })?;
    Ok(remote_refs
        .values()
        .filter_map(|oid| gix_hash::ObjectId::from_hex(oid.as_bytes()).ok())
        .filter(|oid| objects.exists(oid))
        .map(|oid| oid.to_string())
        .collect())
}

fn collect_pointers(
    git_dir: &Path,
    object_ids: &[gix_hash::ObjectId],
    kinds: &HashMap<gix_hash::ObjectId, gix_object::Kind>,
) -> Result<Vec<crab_types::pointer::Pointer>> {
    let objects = gix_odb::at_opts(
        git_dir.join("objects"),
        [],
        gix_odb::store::init::Options {
            alloc_limit_bytes: Some(POINTER_SCAN_ALLOCATION_BYTES),
            ..Default::default()
        },
    )
    .map_err(|error| CrabError::Internal(format!("failed to open local Git objects: {error}")))?;
    let mut buffer = Vec::new();
    let mut pointers = Vec::new();
    for oid in object_ids
        .iter()
        .filter(|oid| kinds.get(*oid) == Some(&gix_object::Kind::Blob))
    {
        let header = objects
            .try_header(oid)
            .map_err(|error| {
                CrabError::Internal(format!("failed to inspect Git blob {oid}: {error}"))
            })?
            .ok_or_else(|| CrabError::CorruptObject {
                path: git_dir.display().to_string(),
                reason: format!("generated pack object {oid} is missing locally"),
            })?;
        if header.size > crab_types::pointer::MAX_POINTER_SIZE as u64 {
            continue;
        }
        let data = objects
            .try_find(oid, &mut buffer)
            .map_err(|error| {
                CrabError::Internal(format!("failed to read Git blob {oid}: {error}"))
            })?
            .ok_or_else(|| CrabError::CorruptObject {
                path: git_dir.display().to_string(),
                reason: format!("generated pack object {oid} is missing locally"),
            })?;
        data.verify_checksum(oid)
            .map_err(|error| CrabError::CorruptObject {
                path: git_dir.display().to_string(),
                reason: format!("Git blob {oid} failed checksum validation: {error}"),
            })?;
        if let Ok(pointer) = crab_types::pointer::Pointer::parse(data.data) {
            pointers.push(pointer);
        }
    }
    Ok(pointers)
}

fn map_push_pack_error(error: CrabError) -> CrabError {
    match error {
        CrabError::FetchMalformedObject {
            oid, kind, detail, ..
        } => CrabError::PushMalformedObject { oid, kind, detail },
        other => other,
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests {
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    use crab_storage::{StorageObservation, StorageObserver, StorageOperation};
    use object_store::memory::InMemory;
    use sha2::{Digest, Sha256};

    use super::*;

    mod cancellation;

    #[derive(Default)]
    struct RecordingObserver {
        observations: Mutex<Vec<StorageObservation>>,
    }

    impl RecordingObserver {
        fn count(&self) -> usize {
            self.observations.lock().expect("observer lock").len()
        }
    }

    impl StorageObserver for RecordingObserver {
        fn started(&self, _operation: StorageOperation) {}

        fn finished(&self, observation: StorageObservation) {
            self.observations
                .lock()
                .expect("observer lock")
                .push(observation);
        }
    }

    fn git(repository: &Path, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(arguments)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("Git output is UTF-8")
            .trim()
            .to_owned()
    }

    fn commit(repository: &Path, contents: &str) -> String {
        std::fs::write(repository.join("tracked.txt"), contents).expect("write fixture");
        git(repository, &["add", "tracked.txt"]);
        git(repository, &["commit", "-m", contents]);
        git(repository, &["rev-parse", "HEAD"])
    }

    async fn publish_ref_edits_with_visibility(
        layout: &crab_storage::StoreLayout<crab_storage::Store>,
        git_dir: &Path,
        edits: Vec<crab_metadata::capsule_protocol::CapsuleRefEdit>,
    ) {
        let root = crab_write::capsule_protocol::open_root(layout)
            .await
            .expect("open root");
        let visibility = prepare_visibility_delta(git_dir, &edits, &BTreeSet::new(), None)
            .expect("prepare Git visibility");
        let transaction =
            crab_metadata::capsule_protocol::CapsuleTransaction::new(root.record().digest(), edits)
                .expect("build ref transaction");
        let sections = visibility
            .map(|visibility| {
                visibility
                    .encode()
                    .map(|bytes| {
                        vec![crab_metadata::capsule_protocol::CapsuleSection::new(
                            crab_metadata::capsule_protocol::CapsuleSectionKind::VisibilityDelta,
                            bytes,
                        )]
                    })
                    .expect("encode Git visibility")
            })
            .unwrap_or_default();
        let capsule =
            crab_metadata::capsule_protocol::Capsule::build(&transaction, Vec::new(), sections)
                .expect("build ref capsule");
        crab_write::capsule_protocol::publish(layout, root, &transaction, &capsule)
            .await
            .expect("publish ref transaction");
    }

    fn active_active_replication() -> crate::replication::ReplicationConfig {
        crate::replication::ReplicationConfig {
            primary: None,
            mode: crate::replication::ReplicationMode::ActiveActive,
            coordinator: Some(crate::replication::ReplicationCoordinatorConfig {
                kind: crate::replication::ReplicationCoordinatorKind::Managed,
                url: "dynamodb://test".to_owned(),
                region: "us-east-1".to_owned(),
                failover_regions: vec!["us-west-2".to_owned()],
                consistency: crate::replication::ReplicationCoordinatorConsistency::Linearizable,
            }),
            writers: vec![
                crate::replication::WriterConfig {
                    name: "east".to_owned(),
                    url: "crab://east/test".to_owned(),
                    region: "us-east-1".to_owned(),
                    enabled: true,
                },
                crate::replication::WriterConfig {
                    name: "west".to_owned(),
                    url: "crab://west/test".to_owned(),
                    region: "us-west-2".to_owned(),
                    enabled: true,
                },
            ],
            replicas: Vec::new(),
        }
    }

    #[test]
    fn active_active_mirror_plan_passes_protocol_validation() {
        let config = PushConfig {
            mirror_plan_id: Some("a".repeat(64)),
            active_active_replication: Some(active_active_replication()),
            ..PushConfig::default()
        };

        validate_publication_plan_context(&config).unwrap();
    }

    #[tokio::test]
    async fn active_active_capsule_commit_materializes_local_ref_and_retains_repair_proof() {
        let store = crab_storage::Store::new(Arc::new(InMemory::new()));
        let layout = crab_storage::StoreLayout::new(store, "repositories/test".to_owned());
        let base =
            crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
                .await
                .unwrap();
        let transaction = crab_metadata::capsule_protocol::CapsuleTransaction::new(
            base.record().digest(),
            vec![crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                Some("2".repeat(40)),
                None,
            )],
        )
        .unwrap();
        let capsule =
            crab_metadata::capsule_protocol::Capsule::build(&transaction, Vec::new(), Vec::new())
                .unwrap();
        let coordinator = Arc::new(crab_coordination::InMemoryWriteCoordinator::new());
        let config = PushConfig {
            active_active_replication: Some(active_active_replication()),
            active_active_writer: Some("east".to_owned()),
            active_active_coordinator: Some(crate::git::push::ActiveActiveWriteCoordinator::new(
                coordinator.clone(),
            )),
            ..PushConfig::default()
        };
        let attempt = publish_capsule(
            &config,
            &layout,
            base,
            &transaction,
            &capsule,
            &[],
            &BTreeMap::new(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let CapsulePublishAttempt::Committed(Some(outcome)) = attempt else {
            panic!("active-active capsule publication did not commit");
        };
        assert_eq!(outcome.commit_sequence, 1);
        let view = crab_read::capsule_protocol::open_view(
            &layout,
            crab_read::capsule_protocol::CapsuleReadLimits {
                max_capsule_bytes: 1024 * 1024,
                max_frontier_bytes: 1024 * 1024,
            },
        )
        .await
        .unwrap();
        assert_eq!(view.refs().get("refs/heads/main"), Some(&"2".repeat(40)));
        let repair = coordinator.repair_snapshot().await.unwrap();
        assert_eq!(repair.materialization_gaps.len(), 1);
        assert_eq!(repair.materialization_gaps[0].region, "us-west-2");
        assert!(repair.materialization_gaps[0].capsule_publication.is_some());
    }

    #[tokio::test]
    async fn active_active_mirror_commit_publishes_receipt_and_replicates_intent() {
        let store = crab_storage::Store::new(Arc::new(InMemory::new()));
        let layout = crab_storage::StoreLayout::new(store, "repositories/test".to_owned());
        let base =
            crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
                .await
                .unwrap();
        let plan_id = "a".repeat(64);
        let transaction = crab_metadata::capsule_protocol::CapsuleTransaction::for_plan(
            base.record().digest(),
            &plan_id,
            vec![crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                Some("2".repeat(40)),
                None,
            )],
        )
        .unwrap();
        let capsule =
            crab_metadata::capsule_protocol::Capsule::build(&transaction, Vec::new(), Vec::new())
                .unwrap();
        let coordinator = Arc::new(crab_coordination::InMemoryWriteCoordinator::new());
        let config = PushConfig {
            mirror_plan_id: Some(plan_id.clone()),
            active_active_replication: Some(active_active_replication()),
            active_active_writer: Some("east".to_owned()),
            active_active_coordinator: Some(crate::git::push::ActiveActiveWriteCoordinator::new(
                coordinator.clone(),
            )),
            ..PushConfig::default()
        };

        let attempt = publish_capsule(
            &config,
            &layout,
            base,
            &transaction,
            &capsule,
            &[],
            &BTreeMap::new(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(attempt, CapsulePublishAttempt::Committed(Some(_))));
        let receipt = crab_metadata::capsule_protocol::resolve_capsule_plan_receipt(
            layout.store(),
            &layout,
            &plan_id,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(receipt.transaction(), &transaction);
        let repair = coordinator.repair_snapshot().await.unwrap();
        assert!(
            repair.materialization_gaps[0]
                .uploaded_objects
                .contains(&layout.capsule_plan_intent_path(&plan_id).to_string())
        );
    }

    #[tokio::test]
    async fn force_rewrite_without_local_remote_tip_preserves_policy_and_visibility() {
        let _git_env = crate::test::git_repo::CleanGitEnvGuard::new();
        for (name, force, lease, deny, succeeds) in [
            ("ordinary", false, None, false, false),
            ("force", true, None, false, true),
            ("lease", false, Some(true), false, true),
            ("stale-lease", true, Some(false), false, false),
            ("deny-force", true, None, true, false),
            ("deny-lease", false, Some(true), true, false),
        ] {
            let source = tempfile::tempdir().expect("source repository");
            git(source.path(), &["init", "-b", "main"]);
            git(source.path(), &["config", "user.name", "Crab Test"]);
            git(
                source.path(),
                &["config", "user.email", "crab@example.invalid"],
            );
            let first = commit(source.path(), "first");
            let contender_root = tempfile::tempdir().expect("contender directory");
            let contender = contender_root.path().join("repo");
            git(
                contender_root.path(),
                &[
                    "clone",
                    "--no-local",
                    source.path().to_str().unwrap(),
                    "repo",
                ],
            );
            git(&contender, &["config", "user.name", "Crab Test"]);
            git(
                &contender,
                &["config", "user.email", "crab@example.invalid"],
            );
            let second = commit(source.path(), "second");
            let divergent = commit(&contender, "divergent");
            let git_dir = contender.join(".git");
            assert_eq!(
                is_ancestor(&git_dir, &second, &divergent).unwrap(),
                None,
                "{name}"
            );

            let store = crate::storage::store::Store::from_storage(crab_storage::Store::new(
                Arc::new(InMemory::new()),
            ));
            let router = crate::storage::StoreLayout::new(store.clone(), "repos/test".to_owned());
            let layout =
                crab_storage::StoreLayout::new(store.as_storage().clone(), "repos/test".to_owned());
            crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
                .await
                .unwrap();
            let limits = crab_read::capsule_protocol::CapsuleReadLimits {
                max_capsule_bytes: 16 * 1024 * 1024,
                max_frontier_bytes: 16 * 1024 * 1024,
            };
            let spec = PushSpec {
                force: false,
                src: "refs/heads/main".to_owned(),
                dst: "refs/heads/main".to_owned(),
            };
            let config = PushConfig {
                git_dir: Some(source.path().join(".git")),
                ..PushConfig::default()
            };
            let (initial, _) = run(
                &config,
                std::slice::from_ref(&spec),
                &store,
                &router,
                None,
                &[],
                None,
                None,
                None,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(initial.all_ok(), "{name}: initial publication");
            let mut config = PushConfig {
                git_dir: Some(git_dir),
                receive_deny_non_fast_forwards: deny,
                ..PushConfig::default()
            };
            if let Some(matches) = lease {
                config.expected_refs.insert(
                    "refs/heads/main".to_owned(),
                    Some(if matches { second.clone() } else { first }),
                );
            }
            let spec = PushSpec { force, ..spec };
            let (result, _) = run(
                &config,
                &[spec],
                &store,
                &router,
                None,
                &[],
                None,
                None,
                None,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(result.all_ok(), succeeds, "{name}");
            let view = crab_read::capsule_protocol::open_view(&layout, limits)
                .await
                .unwrap();
            assert_eq!(
                view.refs()["refs/heads/main"],
                if succeeds {
                    divergent.clone()
                } else {
                    second.clone()
                },
                "{name}: remote tip"
            );
            if succeeds {
                let visibility = view.git_visibility_index().await.unwrap();
                let refs = visibility.ref_closures();
                let closure = &refs["refs/heads/main"];
                assert!(
                    closure.binary_search(&divergent).is_ok()
                        && closure.binary_search(&second).is_err(),
                    "{name}: rewrite must retire old visibility"
                );
                let destination = tempfile::tempdir().unwrap();
                git(destination.path(), &["init", "--bare"]);
                crab_read::capsule_protocol::install_git_packs_from_store(
                    &view,
                    &layout,
                    destination.path(),
                    16 * 1024 * 1024,
                    None,
                    &CancellationToken::new(),
                )
                .await
                .unwrap();
                git(
                    destination.path(),
                    &["cat-file", "-e", &format!("{divergent}^{{commit}}")],
                );
                git(destination.path(), &["fsck", "--connectivity-only"]);
            }
        }
    }

    #[test]
    fn missing_remote_tip_requests_refresh_instead_of_internal_failure() {
        let source = tempfile::tempdir().expect("source repository");
        git(source.path(), &["init", "--initial-branch=main"]);
        git(source.path(), &["config", "user.name", "Crab Test"]);
        git(
            source.path(),
            &["config", "user.email", "crab@example.invalid"],
        );
        let tip = commit(source.path(), "tip");

        assert_eq!(
            is_ancestor(&source.path().join(".git"), &"f".repeat(40), &tip)
                .expect("missing object is a normal refresh condition"),
            None
        );
    }

    #[test]
    fn direct_parent_proves_fast_forward() {
        let source = tempfile::tempdir().expect("source repository");
        git(source.path(), &["init", "--initial-branch=main"]);
        git(source.path(), &["config", "user.name", "Crab Test"]);
        git(
            source.path(),
            &["config", "user.email", "crab@example.invalid"],
        );
        let parent = commit(source.path(), "parent");
        let child = commit(source.path(), "child");
        let git_dir = source.path().join(".git");

        assert!(has_direct_parent(&git_dir, &parent, &child));
        assert_eq!(is_ancestor(&git_dir, &parent, &child).unwrap(), Some(true));

        git(source.path(), &["branch", "side", &parent]);
        git(source.path(), &["checkout", "side"]);
        std::fs::write(source.path().join("side.txt"), "side").expect("write side fixture");
        git(source.path(), &["add", "side.txt"]);
        git(source.path(), &["commit", "-m", "side"]);
        let second_parent = git(source.path(), &["rev-parse", "HEAD"]);
        git(source.path(), &["checkout", "main"]);
        git(source.path(), &["merge", "--no-ff", "--no-edit", "side"]);
        let merge = git(source.path(), &["rev-parse", "HEAD"]);

        assert!(has_direct_parent(&git_dir, &child, &merge));
        assert!(has_direct_parent(&git_dir, &second_parent, &merge));
        assert_eq!(
            is_ancestor(&git_dir, &second_parent, &merge).unwrap(),
            Some(true)
        );
    }

    #[test]
    fn non_direct_ancestry_and_divergence_keep_the_git_graph_check() {
        let source = tempfile::tempdir().expect("source repository");
        git(source.path(), &["init", "--initial-branch=main"]);
        git(source.path(), &["config", "user.name", "Crab Test"]);
        git(
            source.path(),
            &["config", "user.email", "crab@example.invalid"],
        );
        let root = commit(source.path(), "root");
        git(source.path(), &["branch", "side"]);
        let main_tip = commit(source.path(), "main");
        let main_descendant = commit(source.path(), "main descendant");
        git(source.path(), &["checkout", "side"]);
        let side_tip = commit(source.path(), "side");
        let git_dir = source.path().join(".git");

        assert!(!has_direct_parent(&git_dir, &root, &main_descendant));
        assert_eq!(
            is_ancestor(&git_dir, &root, &main_descendant).unwrap(),
            Some(true)
        );
        assert!(!has_direct_parent(&git_dir, &main_tip, &side_tip));
        assert_eq!(
            is_ancestor(&git_dir, &main_tip, &side_tip).unwrap(),
            Some(false)
        );
    }

    #[test]
    fn new_ref_visibility_is_self_contained() {
        let source = tempfile::tempdir().expect("source repository");
        git(source.path(), &["init", "--initial-branch=main"]);
        git(source.path(), &["config", "user.name", "Crab Test"]);
        git(
            source.path(),
            &["config", "user.email", "crab@example.invalid"],
        );
        let tip = commit(source.path(), "tip");
        let ref_name = "refs/heads/feature";
        let edits = [crab_metadata::capsule_protocol::CapsuleRefEdit::new(
            ref_name,
            None,
            Some(tip.clone()),
            None,
        )];

        let visibility =
            prepare_visibility_delta(&source.path().join(".git"), &edits, &BTreeSet::new(), None)
                .expect("prepare Git visibility")
                .expect("new ref has visibility evidence");
        let evidence = visibility
            .edits()
            .get(ref_name)
            .expect("feature visibility evidence");

        assert!(evidence.replaces);
        assert_eq!(evidence.old_oid, None);
        assert!(evidence.added.binary_search(&tip).is_ok());
    }

    #[tokio::test]
    async fn fast_forward_visibility_reuses_proven_ancestry_for_empty_removals() {
        let source = tempfile::tempdir().expect("source repository");
        git(source.path(), &["init", "--initial-branch=main"]);
        git(source.path(), &["config", "user.name", "Crab Test"]);
        git(
            source.path(),
            &["config", "user.email", "crab@example.invalid"],
        );
        let old_oid = commit(source.path(), "first");
        let new_oid = commit(source.path(), "second");
        let git_dir = source.path().join(".git");
        let ref_name = "refs/heads/main";
        let edits = [crab_metadata::capsule_protocol::CapsuleRefEdit::new(
            ref_name,
            Some(old_oid.clone()),
            Some(new_oid.clone()),
            None,
        )];
        let fast_forward_refs = BTreeSet::from([ref_name.to_owned()]);
        let remote_refs = BTreeMap::from([(ref_name.to_owned(), old_oid.clone())]);
        let updates = [RefUpdate {
            ref_name: ref_name.to_owned(),
            old_sha: Some(old_oid.clone()),
            new_sha: new_oid.clone(),
            force: false,
        }];
        let prepared = prepare_git_packs(&git_dir, &remote_refs, &updates, 16 * 1024 * 1024, false)
            .await
            .expect("prepare incremental Git pack");
        let pack_objects = prepared
            .visibility_objects
            .as_deref()
            .expect("single-ref pack is an exact visibility delta");
        let visibility =
            prepare_visibility_delta(&git_dir, &edits, &fast_forward_refs, Some(pack_objects))
                .expect("prepare fast-forward visibility")
                .expect("existing ref has visibility evidence");
        let evidence = visibility
            .edits()
            .get(ref_name)
            .expect("main visibility evidence");
        let expected_added = super::super::push::enumerate_visibility_difference(
            &git_dir,
            &new_oid,
            Some(&old_oid),
            usize::try_from(crab_metadata::git_visibility::MAX_GIT_VISIBILITY_OBJECTS)
                .expect("visibility limit fits usize"),
        )
        .expect("enumerate newly reachable objects")
        .expect("visibility is below its limit")
        .into_iter()
        .collect::<BTreeSet<_>>();

        assert_eq!(evidence.old_oid.as_deref(), Some(old_oid.as_str()));
        assert_eq!(evidence.new_oid, new_oid);
        assert_eq!(
            evidence.added.iter().cloned().collect::<BTreeSet<_>>(),
            expected_added
        );
        assert_eq!(
            pack_objects
                .iter()
                .map(ToString::to_string)
                .collect::<BTreeSet<_>>(),
            expected_added
        );
        assert!(evidence.removed.is_empty());
        evidence.validate().expect("visibility edit remains valid");
    }

    #[tokio::test]
    async fn sibling_ref_objects_are_not_used_as_a_fast_forward_visibility_delta() {
        let source = tempfile::tempdir().expect("source repository");
        git(source.path(), &["init", "--initial-branch=main"]);
        git(source.path(), &["config", "user.name", "Crab Test"]);
        git(
            source.path(),
            &["config", "user.email", "crab@example.invalid"],
        );
        let old_oid = commit(source.path(), "root");
        git(source.path(), &["branch", "side"]);
        git(source.path(), &["checkout", "side"]);
        git(source.path(), &["commit", "--allow-empty", "-m", "side"]);
        let side_oid = git(source.path(), &["rev-parse", "HEAD"]);
        git(source.path(), &["checkout", "main"]);
        commit(source.path(), "main");
        git(source.path(), &["merge", "--no-ff", "--no-edit", "side"]);
        let new_oid = git(source.path(), &["rev-parse", "HEAD"]);
        let git_dir = source.path().join(".git");
        let ref_name = "refs/heads/main";
        let remote_refs = BTreeMap::from([
            (ref_name.to_owned(), old_oid.clone()),
            ("refs/heads/side".to_owned(), side_oid.clone()),
        ]);
        let updates = [RefUpdate {
            ref_name: ref_name.to_owned(),
            old_sha: Some(old_oid.clone()),
            new_sha: new_oid.clone(),
            force: false,
        }];

        let prepared = prepare_git_packs(&git_dir, &remote_refs, &updates, 16 * 1024 * 1024, false)
            .await
            .expect("prepare incremental Git pack");
        assert!(prepared.visibility_objects.is_none());

        let edits = [crab_metadata::capsule_protocol::CapsuleRefEdit::new(
            ref_name,
            Some(old_oid),
            Some(new_oid.clone()),
            None,
        )];
        let fast_forward_refs = BTreeSet::from([ref_name.to_owned()]);
        let visibility = prepare_visibility_delta(&git_dir, &edits, &fast_forward_refs, None)
            .expect("prepare full fast-forward visibility")
            .expect("existing ref has visibility evidence");
        let evidence = visibility
            .edits()
            .get(ref_name)
            .expect("main visibility evidence");

        assert!(evidence.added.binary_search(&side_oid).is_ok());
        assert_eq!(evidence.new_oid, new_oid);
        assert!(evidence.removed.is_empty());
    }

    #[test]
    fn non_fast_forward_visibility_keeps_removed_objects() {
        let source = tempfile::tempdir().expect("source repository");
        git(source.path(), &["init", "--initial-branch=main"]);
        git(source.path(), &["config", "user.name", "Crab Test"]);
        git(
            source.path(),
            &["config", "user.email", "crab@example.invalid"],
        );
        let new_oid = commit(source.path(), "first");
        let old_oid = commit(source.path(), "second");
        let git_dir = source.path().join(".git");
        let ref_name = "refs/heads/main";
        let edits = [crab_metadata::capsule_protocol::CapsuleRefEdit::new(
            ref_name,
            Some(old_oid.clone()),
            Some(new_oid.clone()),
            None,
        )];
        let visibility = prepare_visibility_delta(&git_dir, &edits, &BTreeSet::new(), None)
            .expect("prepare non-fast-forward visibility")
            .expect("existing ref has visibility evidence");
        let evidence = visibility
            .edits()
            .get(ref_name)
            .expect("main visibility evidence");
        let expected_removed = super::super::push::enumerate_visibility_difference(
            &git_dir,
            &old_oid,
            Some(&new_oid),
            usize::try_from(crab_metadata::git_visibility::MAX_GIT_VISIBILITY_OBJECTS)
                .expect("visibility limit fits usize"),
        )
        .expect("enumerate no-longer-reachable objects")
        .expect("visibility is below its limit")
        .into_iter()
        .collect::<BTreeSet<_>>();

        assert!(!expected_removed.is_empty());
        assert_eq!(
            evidence.removed.iter().cloned().collect::<BTreeSet<_>>(),
            expected_removed
        );
        evidence.validate().expect("visibility edit remains valid");
    }

    #[tokio::test]
    async fn full_graph_pack_contains_more_history_than_incremental_pack() {
        let source = tempfile::tempdir().expect("source repository");
        git(source.path(), &["init", "-b", "main"]);
        git(source.path(), &["config", "user.name", "Crab Test"]);
        git(
            source.path(),
            &["config", "user.email", "crab@example.invalid"],
        );
        let first = commit(source.path(), "first");
        let second = commit(source.path(), "second");
        let git_dir = source.path().join(".git");
        let remote_refs = BTreeMap::from([("refs/heads/main".to_owned(), first.clone())]);
        let updates = [RefUpdate {
            ref_name: "refs/heads/main".to_owned(),
            old_sha: Some(first),
            new_sha: second,
            force: false,
        }];

        let incremental =
            prepare_git_packs(&git_dir, &remote_refs, &updates, 16 * 1024 * 1024, false)
                .await
                .expect("prepare incremental pack");
        let full = prepare_git_packs(&git_dir, &remote_refs, &updates, 16 * 1024 * 1024, true)
            .await
            .expect("prepare full graph pack");
        let incremental_bytes = incremental
            .packs
            .iter()
            .map(crab_metadata::capsule_protocol::CapsuleGitPack::pack_size)
            .sum::<u64>();
        let full_bytes = full
            .packs
            .iter()
            .map(crab_metadata::capsule_protocol::CapsuleGitPack::pack_size)
            .sum::<u64>();

        assert!(full_bytes > incremental_bytes);
    }

    #[tokio::test]
    async fn capsule_push_publishes_reachable_lfs_dependencies() {
        let source = tempfile::tempdir().expect("source repository");
        git(source.path(), &["init", "-b", "main"]);
        git(source.path(), &["config", "user.name", "Crab Test"]);
        git(
            source.path(),
            &["config", "user.email", "crab@example.invalid"],
        );
        let content = b"capsule LFS dependency".to_vec();
        let pointer = crab_git::lfs_pointer::LfsPointer {
            oid: Sha256::digest(&content).into(),
            size: content.len() as u64,
            extensions: Vec::new(),
        };
        std::fs::write(source.path().join("asset.bin"), pointer.serialize())
            .expect("write LFS pointer");
        git(source.path(), &["add", "asset.bin"]);
        git(source.path(), &["commit", "-m", "LFS dependency"]);
        let lfs_dir = crate::lfs::config::LfsConfig::resolve_storage_dir(source.path())
            .expect("resolve local LFS store");
        crate::lfs::cache::install_bytes(&lfs_dir, &pointer.oid, pointer.size, &content)
            .expect("install local LFS object");

        let store = crate::storage::store::Store::new(Arc::new(InMemory::new()));
        let router = crate::storage::StoreLayout::new(store.clone(), "repos/lfs".to_owned());
        let layout =
            crab_storage::StoreLayout::new(store.as_storage().clone(), "repos/lfs".to_owned());
        let root =
            crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
                .await
                .expect("initialize root");
        let view = crab_read::capsule_protocol::open_view_from_root(
            &layout,
            root,
            crab_read::capsule_protocol::CapsuleReadLimits {
                max_capsule_bytes: 16 * 1024 * 1024,
                max_frontier_bytes: 16 * 1024 * 1024,
            },
        )
        .await
        .expect("open initial view");
        let config = PushConfig {
            git_dir: Some(source.path().join(".git")),
            ..PushConfig::default()
        };

        let (result, _) = run(
            &config,
            &[PushSpec {
                force: false,
                src: "refs/heads/main".to_owned(),
                dst: "refs/heads/main".to_owned(),
            }],
            &store,
            &router,
            Some(view.into()),
            &[],
            None,
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("publish capsule ref and LFS dependency");

        assert!(result.all_ok());
        let remote = crab_lfs::LfsObjectStore::new(store.as_storage().clone(), "repos/lfs");
        assert_eq!(
            remote
                .verify(&pointer.oid)
                .await
                .expect("verify remote LFS"),
            Bytes::from(content)
        );
    }

    #[tokio::test]
    async fn real_git_incremental_push_round_trips_with_bounded_requests() {
        let _git_env = crate::test::git_repo::CleanGitEnvGuard::new();
        let source = tempfile::tempdir().expect("source repository");
        git(source.path(), &["init", "-b", "main"]);
        git(source.path(), &["config", "user.name", "Crab Test"]);
        git(
            source.path(),
            &["config", "user.email", "crab@example.invalid"],
        );
        let first = commit(source.path(), "first");

        let observer = Arc::new(RecordingObserver::default());
        let storage = crab_storage::Store::new(Arc::new(InMemory::new()))
            .with_storage_observer(Arc::clone(&observer) as Arc<dyn StorageObserver>);
        let store = crate::storage::store::Store::from_storage(storage);
        let router = crate::storage::StoreLayout::new(store.clone(), "repos/test".to_owned());
        let layout =
            crab_storage::StoreLayout::new(store.as_storage().clone(), "repos/test".to_owned());
        let root =
            crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
                .await
                .expect("initialize root");
        let limits = crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 16 * 1024 * 1024,
            max_frontier_bytes: 16 * 1024 * 1024,
        };
        let initial_view = crab_read::capsule_protocol::open_view_from_root(&layout, root, limits)
            .await
            .expect("open initial view");
        let mut config = PushConfig {
            git_dir: Some(source.path().join(".git")),
            ..PushConfig::default()
        };
        let spec = PushSpec {
            force: false,
            src: "refs/heads/main".to_owned(),
            dst: "refs/heads/main".to_owned(),
        };

        let before_first = observer.count();
        let (result, committed) = run(
            &config,
            std::slice::from_ref(&spec),
            &store,
            &router,
            Some(initial_view.into()),
            &[],
            None,
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("first push");
        assert!(result.all_ok());
        assert!(committed.is_none());
        let first_requests = observer.count() - before_first;
        assert!(
            first_requests <= 20,
            "first push used {first_requests} requests"
        );
        let committed = crab_read::capsule_protocol::open_view(&layout, limits)
            .await
            .expect("open committed first push");
        assert_eq!(committed.refs()["refs/heads/main"], first);

        let destination = tempfile::tempdir().expect("destination repository");
        git(destination.path(), &["init", "--bare"]);
        let view = crab_read::capsule_protocol::open_view(&layout, limits)
            .await
            .expect("open first generation");
        crab_read::capsule_protocol::install_git_packs(&view, destination.path(), 16 * 1024 * 1024)
            .await
            .expect("install first pack");
        git(
            destination.path(),
            &["cat-file", "-e", &format!("{first}^{{commit}}")],
        );

        let second = commit(source.path(), "second");
        config
            .expected_refs
            .insert("refs/heads/main".to_owned(), Some(first.clone()));
        let before_second = observer.count();
        let (result, committed) = run(
            &config,
            &[spec],
            &store,
            &router,
            None,
            &[],
            None,
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("incremental push");
        assert!(result.all_ok());
        assert!(committed.is_none());
        let second_requests = observer.count() - before_second;
        assert_eq!(
            second_requests, 6,
            "incremental push should reuse the admitted ref-head snapshot"
        );
        let committed = crab_read::capsule_protocol::open_view(&layout, limits)
            .await
            .expect("open committed second push");
        assert_eq!(committed.refs()["refs/heads/main"], second);
        assert_eq!(committed.capsules().len(), 2);

        let repack_workspace = tempfile::tempdir().expect("repack workspace");
        let before_repack = observer.count();
        let repack = crate::cmd::repack::run_repack_from_root(
            &store,
            "repos/test",
            committed.root_snapshot().clone(),
            &crate::cmd::repack::RepackConfig {
                workspace_root: repack_workspace.path().to_owned(),
                ..crate::cmd::repack::RepackConfig::default()
            },
            &CancellationToken::new(),
        )
        .await
        .expect("checkpoint repack");
        assert_eq!(repack.packs_before, 2);
        assert_eq!(repack.packs_after, 1);
        assert!(repack.bytes_read > 0);
        assert_eq!(repack.bytes_read, repack.bytes_before);
        assert!(repack.bytes_written > 0);
        assert_eq!(repack.bytes_written, repack.bytes_after);
        let repack_requests = observer.count() - before_repack;
        let repack_operations = observer.observations.lock().expect("observer lock")
            [before_repack..]
            .iter()
            .map(|observation| observation.operation)
            .collect::<Vec<_>>();
        let checkpoint_root = crab_write::capsule_protocol::open_root(&layout)
            .await
            .expect("checkpoint root");
        assert!(checkpoint_root.record().root().checkpoint().is_some());
        assert!(
            checkpoint_root
                .record()
                .root()
                .capsule_frontier()
                .is_empty()
        );
        let checkpoint_view =
            crab_read::capsule_protocol::open_view_from_root(&layout, checkpoint_root, limits)
                .await
                .expect("open checkpoint view");
        assert!(checkpoint_view.layered_checkpoint().is_some());
        assert_eq!(checkpoint_view.refs(), committed.refs());

        let third = commit(source.path(), "third");
        config
            .expected_refs
            .insert("refs/heads/main".to_owned(), Some(second.clone()));
        let (result, _) = run(
            &config,
            &[PushSpec {
                force: false,
                src: "refs/heads/main".to_owned(),
                dst: "refs/heads/main".to_owned(),
            }],
            &store,
            &router,
            Some(checkpoint_view.into()),
            &[],
            None,
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("post-checkpoint push");
        assert!(result.all_ok());

        let fresh = tempfile::tempdir().expect("fresh clone target");
        git(fresh.path(), &["init", "--bare"]);
        let view = crab_read::capsule_protocol::open_view(&layout, limits)
            .await
            .expect("open checkpoint and delta");
        assert!(view.layered_checkpoint().is_some());
        assert_eq!(view.capsules().len(), 1);
        crab_read::capsule_protocol::install_git_packs_from_store(
            &view,
            &layout,
            fresh.path(),
            16 * 1024 * 1024,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("install checkpoint and delta");
        git(
            fresh.path(),
            &["cat-file", "-e", &format!("{third}^{{commit}}")],
        );

        let orphan = layout.capsule_path(&"f".repeat(64));
        store
            .put(&orphan, Bytes::from_static(b"unreachable capsule"))
            .await
            .expect("write GC orphan");
        let gc = crate::cmd::gc::run_repo_remote_gc(
            &crate::cmd::gc::GcArgs {
                force: true,
                yes: true,
                ..crate::cmd::gc::GcArgs::default()
            },
            &store,
            &router,
            &std::collections::HashSet::new(),
            &CancellationToken::new(),
            std::time::Duration::ZERO,
            None,
        )
        .await
        .expect("capsule-protocol GC");
        assert_eq!(gc.packs_deleted, 0);
        store
            .head(&orphan)
            .await
            .expect("capsule GC preserves immutable-object grace even when forced");
        let root = crab_write::capsule_protocol::open_root(&layout)
            .await
            .expect("root after GC");
        assert!(root.record().root().gc_fence().is_none());
        crab_read::capsule_protocol::open_view(
            &layout,
            crab_read::capsule_protocol::CapsuleReadLimits {
                max_capsule_bytes: 16 * 1024 * 1024,
                max_frontier_bytes: 16 * 1024 * 1024,
            },
        )
        .await
        .expect("GC preserves every referenced checkpoint and capsule");
        assert!(
            repack_requests <= 12,
            "repack used {repack_requests} requests: {repack_operations:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn capsule_push_retires_staging_only_after_confirmed_ref_publication() {
        use bstr::ByteSlice;
        use crab_staging::stream::{StreamStageProgress, stage_reader_streaming};
        let _git_env = crate::test::git_repo::CleanGitEnvGuard::new();
        for (stale, native_intent, restored_index, cancel_after_commit) in [
            (false, false, false, false),
            (true, false, false, false),
            (false, true, false, false),
            (true, true, false, false),
            (false, true, true, false),
            (false, false, false, true),
        ] {
            let source = tempfile::tempdir().unwrap();
            git(source.path(), &["init", "-b", "main"]);
            git(source.path(), &["config", "user.name", "Crab Test"]);
            git(
                source.path(),
                &["config", "user.email", "crab@example.invalid"],
            );
            let first = commit(source.path(), "initial");
            let staging_root = source.path().join(".crab/staging");
            let staging = crab_staging::StagingArea::open(staging_root.clone())
                .await
                .unwrap();
            let staged = stage_reader_streaming(
                &mut std::io::Cursor::new(b"capsule payload"),
                Path::new("model.bin"),
                &staging,
                StreamStageProgress::default(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            if !native_intent {
                staging
                    .delegate_publication(&[(&staged.batch_id, Path::new("model.bin"))])
                    .unwrap();
            }
            let pointer = crab_types::pointer::Pointer {
                file_hash: staged.file_hash,
                size: staged.size,
                shard_hint: None,
            };
            std::fs::write(source.path().join("model.bin"), pointer.serialize()).unwrap();
            if native_intent {
                let index = gix_index::File::at_or_default(
                    source.path().join(".git/index"),
                    gix_hash::Kind::Sha1,
                    false,
                    gix_index::decode::Options::default(),
                )
                .unwrap();
                let previous_index_state =
                    super::super::index::git_index_path_state(&index, b"model.bin".as_bstr());
                staging
                    .create_publication_intent(&[crab_staging::PublicationIntentEntry {
                        batch_id: staged.batch_id.clone(),
                        path: "model.bin".into(),
                        expected_pointer_oid: git(
                            source.path(),
                            &["hash-object", "-w", "model.bin"],
                        ),
                        previous_index_state,
                    }])
                    .unwrap();
            }
            git(source.path(), &["add", "model.bin"]);
            git(source.path(), &["commit", "-m", "managed payload"]);
            let tip = git(source.path(), &["rev-parse", "HEAD"]);
            if restored_index {
                git(source.path(), &["read-tree", "HEAD^"]);
            }
            staging.close().await.unwrap();
            let staging = Arc::new(StagingAreaReadOnly::open(staging_root).await.unwrap());
            let hooked = Arc::new(crab_storage::test_support::CountingObjectStore::new(
                Arc::new(InMemory::new()),
            ));
            let storage = crab_storage::Store::new(hooked.clone());
            let store = crate::storage::store::Store::from_storage(storage);
            let router = crate::storage::StoreLayout::new(store.clone(), "repos/test".to_owned());
            let layout =
                crab_storage::StoreLayout::new(store.as_storage().clone(), "repos/test".to_owned());
            let root = crab_write::capsule_protocol::initialize(
                &layout,
                &"1".repeat(64),
                "refs/heads/main",
            )
            .await
            .unwrap();
            let limits = crab_read::capsule_protocol::CapsuleReadLimits {
                max_capsule_bytes: 16 * 1024 * 1024,
                max_frontier_bytes: 16 * 1024 * 1024,
            };
            let initial = crab_read::capsule_protocol::open_view_from_root(&layout, root, limits)
                .await
                .unwrap();
            if stale {
                publish_ref_edits_with_visibility(
                    &layout,
                    &source.path().join(".git"),
                    vec![crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                        "refs/heads/main",
                        None,
                        Some(first.clone()),
                        None,
                    )],
                )
                .await;
            }
            let config = PushConfig {
                git_dir: Some(source.path().join(".git")),
                ..Default::default()
            };
            let cancel = CancellationToken::new();
            if cancel_after_commit {
                hooked.cancel_after_put(layout.capsule_ref_heads_prefix(), cancel.clone());
            }
            let (result, _) = run(
                &config,
                &[PushSpec {
                    force: false,
                    src: "refs/heads/main".to_owned(),
                    dst: "refs/heads/main".to_owned(),
                }],
                &store,
                &router,
                Some(initial.into()),
                &[],
                Some(&staging),
                None,
                None,
                &cancel,
            )
            .await
            .unwrap();
            assert_eq!(cancel.is_cancelled(), cancel_after_commit);
            if cancel_after_commit {
                for domain in [layout.global_prefix(), layout.repo_prefix()] {
                    let sweep = crab_coordination::GcFenceLease::acquire_sweep(
                        layout.store().inner(),
                        domain,
                        std::time::Duration::from_secs(60),
                    )
                    .await
                    .unwrap();
                    sweep.release().await.unwrap();
                }
            }
            let health = staging.lifecycle_health().unwrap();
            let view = crab_read::capsule_protocol::open_view(&layout, limits)
                .await
                .unwrap();
            assert!(
                result.all_ok() != stale
                    && view.refs().get("refs/heads/main")
                        == Some(if stale { &first } else { &tip })
                    && health.path_leases == u64::from(stale)
                    && health.payloads == u64::from(stale)
                    && health.open_push_snapshots == 0
                    && health.committed_push_snapshots == 0
                    && health.unresolved_publications == 0
                    && staging.list_inflight().unwrap().is_empty(),
                "stale={stale}, native_intent={native_intent}, restored_index={restored_index}, cancel_after_commit={cancel_after_commit}, health={health:?}"
            );
        }
    }

    #[tokio::test]
    async fn single_ref_successor_extends_committed_multi_ref_history() {
        let source = tempfile::tempdir().expect("source repository");
        git(source.path(), &["init", "-b", "main"]);
        git(source.path(), &["config", "user.name", "Crab Test"]);
        git(
            source.path(),
            &["config", "user.email", "crab@example.invalid"],
        );
        let first = commit(source.path(), "first");
        let second = commit(source.path(), "second");
        git(source.path(), &["reset", "--hard", &first]);
        let third = commit(source.path(), "third");

        let storage = crab_storage::Store::new(Arc::new(InMemory::new()));
        let layout = crab_storage::StoreLayout::new(storage, "repos/ref-history".to_owned());
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .expect("initialize root");
        let limits = crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 16 * 1024 * 1024,
            max_frontier_bytes: 16 * 1024 * 1024,
        };
        let main = "refs/heads/main";
        let sibling = &format!("refs/heads/{}", "a".repeat(40));

        for edits in [
            vec![
                crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                    main,
                    None,
                    Some(first.clone()),
                    None,
                ),
                crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                    sibling,
                    None,
                    Some(first.clone()),
                    None,
                ),
            ],
            vec![
                crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                    main,
                    Some(first.clone()),
                    Some(second.clone()),
                    None,
                ),
                crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                    sibling,
                    Some(first.clone()),
                    Some(second.clone()),
                    None,
                ),
            ],
            vec![
                crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                    main,
                    Some(second.clone()),
                    Some(first.clone()),
                    None,
                ),
                crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                    sibling,
                    Some(second.clone()),
                    None,
                    None,
                ),
            ],
        ] {
            publish_ref_edits_with_visibility(&layout, &source.path().join(".git"), edits).await;
            crab_read::capsule_protocol::open_view(&layout, limits)
                .await
                .expect("multi-ref history remains readable");
        }

        publish_ref_edits_with_visibility(
            &layout,
            &source.path().join(".git"),
            vec![crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                main,
                Some(first),
                Some(third.clone()),
                None,
            )],
        )
        .await;

        let view = crab_read::capsule_protocol::open_view(&layout, limits)
            .await
            .expect("single-ref successor must preserve readable multi-ref history");
        assert_eq!(view.refs().get(main), Some(&third));
        assert!(!view.refs().contains_key(sibling));
    }
}
