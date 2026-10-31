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

mod shared_visibility;

const LOCAL_OBJECT_ALLOCATION_BYTES: usize = 64 * 1024 * 1024;

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
    // Reviewed plan admission encloses this operation, including no-op batches.
    // Returning earlier would let an attempted plan bypass its replay fence.
    if specs.is_empty() {
        return Ok((PushResult::empty(), advertised));
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
    // A shared graph can inspect a large physical history. Keep that blocking
    // work off the async runtime so cancellation and lease supervision can run.
    let visibility_git_dir = common_git_dir.clone();
    let visibility_edits = edits.clone();
    let visibility_fast_forwards = fast_forward_refs.clone();
    let visibility_pack_objects = prepared.visibility_objects.clone();
    let visibility_cancel = cancel.clone();
    let visibility_delta = tokio::task::spawn_blocking(move || {
        prepare_visibility_delta(
            &visibility_git_dir,
            &visibility_edits,
            &visibility_fast_forwards,
            visibility_pack_objects.as_deref(),
            &visibility_cancel,
        )
    })
    .await
    .map_err(|error| {
        CrabError::Internal(format!("Git visibility preparation worker failed: {error}"))
    })??;
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
    let publication: Result<Option<Option<crate::git::push::PushCommitMetadata>>> =
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
                Ok(CapsulePublishAttempt::Committed(outcome)) => Ok(Some(outcome.map(Into::into))),
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
            Some(outcome) => PushResult::new(outcomes).with_active_active_commit(outcome),
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
    cancel: &CancellationToken,
) -> Result<Option<crab_metadata::capsule_protocol::CapsuleVisibilityDelta>> {
    check_cancelled(cancel)?;
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
    let available_old_tips = locally_available_remote_tips(git_dir, &rewritten_old_refs)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let mut shared = shared_visibility::prepare_replacements(
        git_dir,
        edits,
        fast_forward_refs,
        &available_old_tips,
        cancel,
    )?;
    let mut visibility = BTreeMap::new();
    for edit in edits {
        check_cancelled(cancel)?;
        let Some(new_oid) = edit.new_oid() else {
            continue;
        };
        let evidence = if let Some(added) = shared.remove(edit.ref_name()) {
            crab_metadata::git_visibility::GitVisibilityEdit {
                version: crab_metadata::git_visibility::GIT_VISIBILITY_EDIT_VERSION,
                old_oid: edit.expected_old().map(str::to_owned),
                new_oid: new_oid.to_owned(),
                replaces: true,
                added,
                removed: crab_metadata::git_visibility::GitVisibilityObjects::default(),
            }
        } else if let Some(pack_objects) = fast_forward_pack_objects
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
            && (fast_forward_refs.contains(edit.ref_name()) || available_old_tips.contains(old_oid))
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

    let output = crab_git::raw_object_command(git_dir)
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
// Both checks use the same physical history as packing and visibility; a local
// replacement or graft must not turn a divergent update into a fast-forward.
fn has_direct_parent(git_dir: &Path, old_oid: &str, new_oid: &str) -> bool {
    let (Ok(old_oid), Ok(new_oid)) = (
        gix_hash::ObjectId::from_hex(old_oid.as_bytes()),
        gix_hash::ObjectId::from_hex(new_oid.as_bytes()),
    ) else {
        return false;
    };
    // Read physical objects directly: no replacement refs, grafts, shallow
    // boundary or lazy fetch may manufacture the edge used by admission.
    let Ok(objects) = gix_odb::at_opts(
        git_dir.join("objects"),
        [],
        gix_odb::store::init::Options {
            alloc_limit_bytes: Some(LOCAL_OBJECT_ALLOCATION_BYTES),
            ..Default::default()
        },
    ) else {
        return false;
    };
    let mut buffer = Vec::new();
    let Ok(Some(parent)) = objects.try_find(&old_oid, &mut buffer) else {
        return false;
    };
    if parent.kind != gix_object::Kind::Commit
        || parent.verify_checksum(&old_oid).is_err()
        || gix_object::CommitRef::from_bytes(parent.data, gix_hash::Kind::Sha1).is_err()
    {
        return false;
    }
    let Ok(Some(child)) = objects.try_find(&new_oid, &mut buffer) else {
        return false;
    };
    if child.kind != gix_object::Kind::Commit || child.verify_checksum(&new_oid).is_err() {
        return false;
    }
    gix_object::CommitRef::from_bytes(child.data, gix_hash::Kind::Sha1)
        .is_ok_and(|commit| commit.parents().any(|parent| parent == old_oid))
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
        let inventory = inspect_generated_objects(git_dir, &object_ids)?;
        pointers.extend(inventory.pointers);
        let checksum =
            gix_hash::ObjectId::from_hex(installed.git_sha1.as_bytes()).map_err(|error| {
                CrabError::Internal(format!("generated pack checksum is invalid: {error}"))
            })?;
        let locator = crab_git::pack_locator::encode_pack_kind_metadata(checksum, &inventory.kinds)
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

struct GeneratedObjectInventory {
    kinds: Vec<gix_object::Kind>,
    pointers: Vec<crab_types::pointer::Pointer>,
}

// Native index-pack has already validated the generated pack. Inspect local
// physical headers once for its ordered locator kinds and small pointer blobs.
fn inspect_generated_objects(
    git_dir: &Path,
    object_ids: &[gix_hash::ObjectId],
) -> Result<GeneratedObjectInventory> {
    let objects = gix_odb::at_opts(
        git_dir.join("objects"),
        [],
        gix_odb::store::init::Options {
            alloc_limit_bytes: Some(LOCAL_OBJECT_ALLOCATION_BYTES),
            ..Default::default()
        },
    )
    .map_err(|error| CrabError::Internal(format!("failed to open local Git objects: {error}")))?;
    let mut buffer = Vec::new();
    let mut pointers = Vec::new();
    let mut kinds = Vec::with_capacity(object_ids.len());
    for oid in object_ids {
        let header = objects
            .try_header(oid)
            .map_err(|error| {
                CrabError::Internal(format!("failed to inspect Git object {oid}: {error}"))
            })?
            .ok_or_else(|| CrabError::CorruptObject {
                path: git_dir.display().to_string(),
                reason: format!("generated pack object {oid} is missing locally"),
            })?;
        kinds.push(header.kind);
        if header.kind != gix_object::Kind::Blob
            || header.size > crab_types::pointer::MAX_POINTER_SIZE as u64
        {
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
    Ok(GeneratedObjectInventory { kinds, pointers })
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
#[path = "capsule_push_tests.rs"]
mod tests;
