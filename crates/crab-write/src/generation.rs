//! Lifecycle for generation-bound catalog publication.
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};

use crab_coordination::{
    CoordinationError, GIT_GENERATION_OWNER_RESOURCE, GIT_OBJECT_LOCATOR_RESOURCE,
    GcFenceHeartbeat, GcFenceLease, PushLock, PushLockAcquireContext,
};
use crab_metadata::{
    git_object_locator::{
        GitLocatorCoverage, GitObjectCatalogStats, GitObjectLocatorSession, GitObjectLocatorWriter,
        LocatorSweepStats,
    },
    manifest_store,
    manifests::{Manifest, PackManifestEntry},
    split_commit_graph::{
        CommitGraphInput, SplitCommitGraph, append_split_commit_graph, load_split_commit_graph,
        upload_split_commit_graph,
    },
};
use crab_remote_git::{
    OperationContext, OperationKind, RemoteGitObject, RemoteGitRepository, RemoteGitRuntime,
    RepositoryIdentity, RepositoryOptions,
};
use crab_storage::{Store, StoreLayout};
use crab_xet::hash::MerkleHash;
use tokio_util::sync::CancellationToken;

use crate::{Result, WriteError, catalog::publish_inventory, finish_after_cleanup};

const COMMIT_GRAPH_BATCH_SIZE: usize = 512;

struct WriterFence {
    lease: GcFenceLease,
    heartbeat: GcFenceHeartbeat,
}

impl WriterFence {
    async fn acquire(
        store: &Store,
        domain: &str,
        ttl: Duration,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        check_cancelled(cancel)?;
        let lease = GcFenceLease::acquire_writer(store.inner(), domain, ttl).await?;
        let heartbeat = GcFenceHeartbeat::spawn(&lease, cancel.clone(), ttl / 3);
        Ok(Self { lease, heartbeat })
    }

    async fn release(self) -> Result<()> {
        self.heartbeat.stop().await;
        self.lease.release().await.map_err(Into::into)
    }
}

/// Elect one owner and publish all committed repository state needed by readers.
///
/// Concurrent callers converge: a caller which loses owner election returns
/// successfully because the owner is responsible for the same canonical state.
/// Every acquired lease and GC fence is released before this function returns.
pub async fn ensure_readable(
    store: &Store,
    layout: &StoreLayout<Store>,
    identity: &RepositoryIdentity,
    runtime: Arc<RemoteGitRuntime>,
    options: RepositoryOptions,
    ttl: Duration,
    cancel: &CancellationToken,
) -> Result<()> {
    ensure_readable_state(
        store,
        layout,
        ttl,
        cancel,
        Some(CommitGraphMaintenance {
            identity,
            runtime,
            options,
        }),
    )
    .await
}

/// Elect one owner and publish committed repository state needed for object lookup.
///
/// This bounded maintenance path compacts the ref journal and advances exact-object
/// catalog coverage, but deliberately leaves commit-graph construction to
/// [`ensure_readable`]. Every acquired lease and GC fence is released before return.
pub async fn ensure_catalog_readable(
    store: &Store,
    layout: &StoreLayout<Store>,
    ttl: Duration,
    cancel: &CancellationToken,
) -> Result<()> {
    ensure_readable_state(store, layout, ttl, cancel, None).await
}

struct CommitGraphMaintenance<'a> {
    identity: &'a RepositoryIdentity,
    runtime: Arc<RemoteGitRuntime>,
    options: RepositoryOptions,
}

async fn ensure_readable_state(
    store: &Store,
    layout: &StoreLayout<Store>,
    ttl: Duration,
    cancel: &CancellationToken,
    commit_graph: Option<CommitGraphMaintenance<'_>>,
) -> Result<()> {
    let mut context = PushLockAcquireContext::new(Arc::clone(store.inner()));
    let mut owner = match context
        .try_acquire_internal(layout.repo_prefix(), GIT_GENERATION_OWNER_RESOURCE, ttl)
        .await
    {
        Ok(owner) => owner,
        Err(CoordinationError::PushLockHeld { .. }) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    // Keep the full catalog and graph operation behind one heap indirection.
    // Large committed bursts otherwise exhaust a default Tokio worker stack.
    let maintenance = Box::pin(async {
        let global = WriterFence::acquire(store, layout.global_prefix(), ttl, cancel).await?;
        let repo = match WriterFence::acquire(store, layout.repo_prefix(), ttl, cancel).await {
            Ok(repo) => repo,
            Err(error) => {
                let _ = global.release().await;
                return Err(error);
            }
        };
        let mut result = async {
            if commit_graph.is_none() {
                return make_catalog_readable(store, layout, ttl, cancel).await;
            }
            let (manifest, _) = manifest_store::read_manifest(store, layout).await?;
            let Some(manifest) = make_readable(store, layout, ttl, manifest.pusher, cancel).await?
            else {
                return Ok(());
            };
            let Some(commit_graph) = commit_graph else {
                return Ok(());
            };
            maintain_commit_graph(
                store,
                layout,
                &manifest,
                commit_graph.identity,
                commit_graph.runtime,
                commit_graph.options,
                cancel,
            )
            .await
            .map(drop)
        }
        .await;
        for fence in [repo, global] {
            result = result.and(fence.release().await);
        }
        result
    });
    let result = crab_coordination::while_renewing(&mut owner, Some(cancel), maintenance).await;
    result.and(owner.release().await.map_err(Into::into))
}

async fn make_catalog_readable(
    store: &Store,
    layout: &StoreLayout<Store>,
    lock_ttl: Duration,
    cancel: &CancellationToken,
) -> Result<()> {
    check_cancelled(cancel)?;
    let (base, _) = manifest_store::read_manifest(store, layout).await?;
    crate::journal::compact_once_for_owner(store, layout, lock_ttl, base.pusher, cancel).await?;
    check_cancelled(cancel)?;
    let (manifest, _) = manifest_store::read_manifest(store, layout).await?;
    let packs = if manifest.pack_index_hash.is_empty() {
        Vec::new()
    } else {
        manifest_store::read_bulk_pack_list(store, layout, &manifest.pack_index_hash).await?
    };
    if maintain_catalog(store, layout, &manifest, &packs, lock_ttl, cancel)
        .await?
        .is_none()
    {
        return Ok(());
    }
    check_cancelled(cancel)?;
    let visible = manifest.refs.is_empty()
        || crab_metadata::git_visibility::ensure_catalog_bound(store, layout, &manifest).await?;
    if !visible {
        return Err(WriteError::VisibilityUnavailable {
            generation: manifest.generation,
        });
    }
    Ok(())
}

/// Complete the read path for already committed refs using their verified visibility evidence.
///
/// The caller retains generation-owner election and global/repository GC writer
/// fences. Await completion, including after cancellation, to close catalog handles
/// and release internal leases. This does not validate or commit incoming refs,
/// rebuild missing visibility from unverified objects, or publish index receipts.
/// Returns the ready manifest, or none when concurrent publication requires another
/// pass. An error cannot imply rollback: refs may already have been committed.
pub async fn make_readable(
    store: &Store,
    layout: &StoreLayout<Store>,
    lock_ttl: Duration,
    pusher: Option<String>,
    cancel: &CancellationToken,
) -> Result<Option<Manifest>> {
    check_cancelled(cancel)?;
    crate::journal::compact_for_owner(store, layout, lock_ttl, pusher, cancel).await?;
    check_cancelled(cancel)?;
    let snapshot = manifest_store::read_repository_snapshot(store, layout).await?;
    if !snapshot.journal.transactions.is_empty() {
        return Ok(None);
    }
    let manifest = snapshot.manifest;
    if maintain_catalog(
        store,
        layout,
        &manifest,
        &snapshot.journal.packs,
        lock_ttl,
        cancel,
    )
    .await?
    .is_none()
    {
        return Ok(None);
    }
    check_cancelled(cancel)?;
    let visible = manifest.refs.is_empty()
        || crab_metadata::git_visibility::ensure_catalog_bound(store, layout, &manifest).await?;
    check_cancelled(cancel)?;
    // A ready catalog alone is insufficient. A new active journal can appear
    // without advancing the manifest and makes remote Git readers wait again.
    let current = manifest_store::read_repository_snapshot(store, layout).await?;
    if !same_generation(&current.manifest, &manifest) || !current.journal.transactions.is_empty() {
        return Ok(None);
    }
    if !visible {
        return Err(WriteError::VisibilityUnavailable {
            generation: manifest.generation,
        });
    }
    Ok(Some(manifest))
}

/// Build and attach a complete commit graph for a still-current readable generation.
///
/// The caller owns generation-owner election and GC writer fences. Commit objects
/// are read from the remote catalog without materializing packs or a checkout.
/// Returns false when the captured generation was superseded before publication.
pub async fn maintain_commit_graph(
    store: &Store,
    layout: &StoreLayout<Store>,
    manifest: &Manifest,
    identity: &RepositoryIdentity,
    runtime: Arc<RemoteGitRuntime>,
    options: RepositoryOptions,
    cancel: &CancellationToken,
) -> Result<bool> {
    check_cancelled(cancel)?;
    if manifest.refs.is_empty() || manifest.commit_graph_hash.is_some() {
        return Ok(true);
    }
    let base = load_previous_commit_graph(
        store,
        layout,
        manifest,
        options.object_limits().max_commit_graph_bytes,
    )
    .await?;
    let roots = commit_graph_roots(manifest)?;
    let repository = RemoteGitRepository::open(
        store.clone(),
        layout.clone(),
        identity.clone(),
        runtime,
        options,
        cancel,
    )
    .await?;
    if repository.generation() != manifest.generation {
        return Ok(false);
    }
    let operation = repository.operation(OperationKind::History, cancel).await?;
    let additions = collect_commit_graph_inputs(&operation, base.as_ref(), &roots).await;
    let additions = operation.finish(additions).await?;
    if !repository.is_current(cancel).await? {
        return Ok(false);
    }
    let write = append_split_commit_graph(
        base,
        manifest.generation,
        manifest.pack_index_hash.clone(),
        manifest.git_validation_digest.clone(),
        &roots,
        additions,
    )?
    .ok_or_else(|| {
        WriteError::Internal("remote commit graph traversal was incomplete".to_owned())
    })?;
    upload_split_commit_graph(store, layout, &write).await?;
    attach_commit_graph_if_current(store, layout, manifest, &write.descriptor_hash).await
}

async fn load_previous_commit_graph(
    store: &Store,
    layout: &StoreLayout<Store>,
    manifest: &Manifest,
    maximum_bytes: u64,
) -> Result<Option<SplitCommitGraph>> {
    let Some(previous_generation) = manifest.generation.checked_sub(1) else {
        return Ok(None);
    };
    let history =
        manifest_store::list_manifest_history_for_generation(store, layout, previous_generation)
            .await?;
    let Some(base_manifest) = history
        .iter()
        .find(|entry| entry.manifest.commit_graph_hash.is_some())
        .map(|entry| &entry.manifest)
    else {
        return Ok(None);
    };
    let hash = base_manifest.commit_graph_hash.as_deref().ok_or_else(|| {
        WriteError::Internal("historical commit graph hash disappeared".to_owned())
    })?;
    let graph = load_split_commit_graph(store, layout, hash, maximum_bytes).await?;
    let roots = commit_graph_roots(base_manifest)?;
    if graph.descriptor.generation != base_manifest.generation
        || graph.descriptor.pack_index_hash != base_manifest.pack_index_hash
        || graph.descriptor.git_validation_digest != base_manifest.git_validation_digest
        || roots.iter().any(|root| !graph.contains(root))
    {
        return Err(WriteError::CorruptObject {
            path: layout.bulk_manifest_path("commit-graph", hash).to_string(),
            reason: "historical commit graph does not match its committed Git state".to_owned(),
        });
    }
    Ok(Some(graph))
}

fn commit_graph_roots(manifest: &Manifest) -> Result<Vec<[u8; 20]>> {
    manifest
        .refs
        .iter()
        .map(|(name, oid)| manifest.peeled_refs.get(name).unwrap_or(oid))
        .map(|value| {
            let oid = gix_hash::ObjectId::from_hex(value.as_bytes()).map_err(|source| {
                WriteError::CorruptObject {
                    path: "manifest".to_owned(),
                    reason: format!("invalid commit graph root {value}: {source}"),
                }
            })?;
            oid.as_bytes()
                .try_into()
                .map_err(|_| WriteError::CorruptObject {
                    path: "manifest".to_owned(),
                    reason: format!("commit graph root is not SHA-1: {value}"),
                })
        })
        .collect()
}

async fn collect_commit_graph_inputs(
    operation: &OperationContext,
    base: Option<&SplitCommitGraph>,
    roots: &[[u8; 20]],
) -> std::result::Result<Vec<CommitGraphInput>, crab_remote_git::Error> {
    let mut pending = VecDeque::new();
    let mut queued = HashSet::new();
    for root in roots {
        if base.is_none_or(|graph| !graph.contains(root)) && queued.insert(*root) {
            pending.push_back(*root);
        }
    }
    let mut additions = Vec::new();
    while !pending.is_empty() {
        let mut requested = Vec::with_capacity(COMMIT_GRAPH_BATCH_SIZE);
        while requested.len() < COMMIT_GRAPH_BATCH_SIZE {
            let Some(oid) = pending.pop_front() else {
                break;
            };
            if base.is_none_or(|graph| !graph.contains(&oid)) {
                requested.push(gix_hash::ObjectId::from(oid));
            }
        }
        if requested.is_empty() {
            continue;
        }
        let objects = operation.read_objects(&requested).await?;
        if objects.len() != requested.len() {
            return Err(crab_remote_git::Error::Corrupt {
                stage: crab_remote_git::CorruptionStage::Commit,
            });
        }
        for (expected, object) in requested.into_iter().zip(objects) {
            if object.oid != expected {
                return Err(crab_remote_git::Error::Corrupt {
                    stage: crab_remote_git::CorruptionStage::Commit,
                });
            }
            let input = commit_graph_input(&object)?;
            for parent in &input.parents {
                if base.is_none_or(|graph| !graph.contains(parent)) && queued.insert(*parent) {
                    pending.push_back(*parent);
                }
            }
            additions.push(input);
        }
    }
    Ok(additions)
}

fn commit_graph_input(
    object: &RemoteGitObject,
) -> std::result::Result<CommitGraphInput, crab_remote_git::Error> {
    if object.kind != gix_object::Kind::Commit {
        return Err(crab_remote_git::Error::ObjectKind {
            oid: object.oid,
            expected: gix_object::Kind::Commit,
            actual: object.kind,
        });
    }
    let parsed = gix_object::CommitRef::from_bytes(&object.data, gix_hash::Kind::Sha1)
        .map_err(|source| crab_remote_git::Error::CommitParse {
            oid: object.oid,
            source,
        })?
        .into_owned()
        .map_err(|source| crab_remote_git::Error::CommitParse {
            oid: object.oid,
            source,
        })?;
    let parents = parsed.parents.into_vec();
    Ok(CommitGraphInput {
        oid: object
            .oid
            .as_bytes()
            .try_into()
            .map_err(|_| crab_remote_git::Error::Corrupt {
                stage: crab_remote_git::CorruptionStage::Commit,
            })?,
        tree_oid: parsed.tree.as_bytes().try_into().map_err(|_| {
            crab_remote_git::Error::Corrupt {
                stage: crab_remote_git::CorruptionStage::Commit,
            }
        })?,
        commit_time: parsed.committer.time.seconds,
        parents: parents
            .iter()
            .map(|parent| {
                parent
                    .as_bytes()
                    .try_into()
                    .map_err(|_| crab_remote_git::Error::Corrupt {
                        stage: crab_remote_git::CorruptionStage::Commit,
                    })
            })
            .collect::<std::result::Result<Vec<_>, _>>()?,
    })
}

async fn attach_commit_graph_if_current(
    store: &Store,
    layout: &StoreLayout<Store>,
    manifest: &Manifest,
    hash: &str,
) -> Result<bool> {
    for attempt in 0..3 {
        let (mut current, etag) = manifest_store::read_manifest(store, layout).await?;
        if !same_generation(&current, manifest) {
            return Ok(false);
        }
        if current.commit_graph_hash.is_some() {
            return Ok(true);
        }
        current.commit_graph_hash = Some(hash.to_owned());
        match manifest_store::write_manifest_cas(store, layout, &current, &etag).await {
            Ok(_) => return Ok(true),
            Err(crab_metadata::error::MetadataError::ManifestCasConflict { .. }) if attempt < 2 => {
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err(WriteError::Internal(
        "commit graph manifest CAS retries exhausted".to_owned(),
    ))
}

/// Committed index identity shared by publication and generation maintenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommittedManifestAnchor {
    pub generation: u64,
    pub shard_index_hash: MerkleHash,
    pub pack_index_hash: MerkleHash,
}

/// Parse a manifest's index identity, or return none for an empty index inventory.
pub fn committed_manifest_anchor(manifest: &Manifest) -> Result<Option<CommittedManifestAnchor>> {
    if manifest.shard_index_hash.is_empty() && manifest.pack_index_hash.is_empty() {
        return Ok(None);
    }
    let shard_index_hash = if manifest.shard_index_hash.is_empty() {
        MerkleHash::default()
    } else {
        MerkleHash::from_hex(&manifest.shard_index_hash).map_err(|error| {
            WriteError::ManifestHash {
                field: "shard-index",
                source: Box::new(error),
            }
        })?
    };
    let pack_index_hash = if manifest.pack_index_hash.is_empty() {
        MerkleHash::default()
    } else {
        MerkleHash::from_hex(&manifest.pack_index_hash).map_err(|error| {
            WriteError::ManifestHash {
                field: "pack-index",
                source: Box::new(error),
            }
        })?
    };
    Ok(Some(CommittedManifestAnchor {
        generation: manifest.generation,
        shard_index_hash,
        pack_index_hash,
    }))
}

/// Count object rows not already covered by committed pack bindings.
#[must_use]
pub fn uncovered_locator_object_rows(
    coverage: Option<crab_metadata::git_object_locator::GitLocatorCoverage>,
    bindings: &[crab_metadata::git_object_locator::GitPackLocatorBinding],
    packs: &[PackManifestEntry],
) -> u64 {
    let Some(coverage) = coverage else {
        return packs
            .iter()
            .fold(0_u64, |total, pack| total.saturating_add(pack.object_count));
    };
    let covered = bindings
        .iter()
        .map(|binding| {
            (
                binding.record.pack_id,
                (
                    binding.record.committed_generation,
                    binding.record.object_count,
                    binding.record.pack_size,
                ),
            )
        })
        .collect::<HashMap<_, _>>();
    packs
        .iter()
        .filter(|pack| {
            let Ok(pack_id) = MerkleHash::from_hex(&pack.pack_id) else {
                return true;
            };
            !covered
                .get(&pack_id)
                .is_some_and(|(committed_generation, object_count, pack_size)| {
                    *object_count == pack.object_count
                        && *pack_size == pack.size
                        && *committed_generation <= coverage.generation
                })
        })
        .fold(0_u64, |total, pack| total.saturating_add(pack.object_count))
}

/// Catalog work completed for a still-current manifest.
#[derive(Debug)]
pub struct CatalogMaintenance {
    pub advanced: bool,
    pub stats: GitObjectCatalogStats,
    pub sweep: LocatorSweepStats,
}

fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(WriteError::Cancelled);
    }
    Ok(())
}

fn same_generation(left: &Manifest, right: &Manifest) -> bool {
    left.generation == right.generation
        && left.shard_index_hash == right.shard_index_hash
        && left.pack_index_hash == right.pack_index_hash
        && left.git_validation_digest == right.git_validation_digest
}

async fn acquire_catalog_lock(
    store: &Store,
    layout: &StoreLayout<Store>,
    ttl: Duration,
    cancel: &CancellationToken,
) -> Result<PushLock> {
    let mut context = PushLockAcquireContext::new(Arc::clone(store.inner()));
    let mut attempt = 0_u32;
    loop {
        check_cancelled(cancel)?;
        match context
            .acquire_internal(layout.repo_prefix(), GIT_OBJECT_LOCATOR_RESOURCE, ttl)
            .await
        {
            Ok(lock) => return Ok(lock),
            Err(CoordinationError::PushLockHeld { .. }) => {
                let delay = Duration::from_millis(
                    100_u64
                        .saturating_mul(1_u64.checked_shl(attempt.min(6)).unwrap_or(u64::MAX))
                        .min(5_000),
                );
                attempt = attempt.saturating_add(1);
                tokio::select! {
                    () = cancel.cancelled() => return Err(WriteError::Cancelled),
                    () = tokio::time::sleep(delay) => {}
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Publish the complete pack inventory for a pinned manifest under a renewable lease.
///
/// `packs` must be the complete inventory selected from `manifest`. The caller
/// owns generation-service election and GC fencing. Cancellation is cooperative:
/// await to completion so all SlateDB handles close and the writer lease releases.
/// Returns none when the manifest is superseded, including during publication.
/// A successful catalog result does not establish visibility or receipt readiness.
pub async fn maintain_catalog(
    store: &Store,
    layout: &StoreLayout<Store>,
    manifest: &Manifest,
    packs: &[PackManifestEntry],
    lock_ttl: Duration,
    cancel: &CancellationToken,
) -> Result<Option<CatalogMaintenance>> {
    check_cancelled(cancel)?;
    let anchor = committed_manifest_anchor(manifest)?;
    let mut lock = acquire_catalog_lock(store, layout, lock_ttl, cancel).await?;
    let operation = crab_coordination::while_renewing(&mut lock, Some(cancel), async move {
        check_cancelled(cancel)?;
        let (current, _) = manifest_store::read_manifest(store, layout).await?;
        if !same_generation(&current, manifest) {
            return Ok(None);
        }
        // Plan only after fencing and checking the captured manifest. A stale
        // caller must not reopen or scan a repository-sized catalog.
        let session =
            GitObjectLocatorSession::open(Arc::clone(store.inner()), layout.repo_prefix()).await?;
        let coverage = session.coverage();
        let bindings = session.pack_bindings().await.map_err(WriteError::from);
        let close = session.close().await;
        let bindings = finish_after_cleanup(
            bindings,
            close,
            "catalog planning session close also failed",
        )?;
        check_cancelled(cancel)?;
        let planned_rows = uncovered_locator_object_rows(coverage, &bindings, packs);
        let unchanged = anchor.is_some_and(|anchor| {
            coverage.is_some_and(|coverage| coverage.pack_index_hash == anchor.pack_index_hash)
        }) && planned_rows == 0;
        let mut writer = if unchanged {
            GitObjectLocatorWriter::open_for_coverage_update(
                Arc::clone(store.inner()),
                layout.repo_prefix(),
            )
            .await?
        } else {
            GitObjectLocatorWriter::open_for_publication(
                Arc::clone(store.inner()),
                layout.repo_prefix(),
                planned_rows,
            )
            .await?
        };
        let expected = anchor.map(|anchor| GitLocatorCoverage {
            generation: anchor.generation,
            pack_index_hash: anchor.pack_index_hash,
        });
        let result = async {
            let (advanced, sweep) = if let Some(expected) =
                expected.filter(|expected| writer.coverage() != Some(*expected))
            {
                let result = publish_inventory(
                    &mut writer,
                    store,
                    layout,
                    &mut HashMap::new(),
                    expected,
                    packs,
                    true,
                    cancel,
                )
                .await?;
                if result.0 {
                    writer.publish_checkpoint().await?;
                }
                result
            } else {
                (false, LocatorSweepStats::default())
            };
            let covered = expected.is_none_or(|expected| writer.coverage() == Some(expected));
            Ok::<_, WriteError>((
                CatalogMaintenance {
                    advanced,
                    stats: writer.catalog_stats().await?,
                    sweep,
                },
                covered,
            ))
        }
        .await;
        let close = writer.close().await;
        let (maintenance, covered) = finish_after_cleanup(
            result,
            close,
            "catalog writer close also failed after publication",
        )?;
        check_cancelled(cancel)?;
        let (current, _) = manifest_store::read_manifest(store, layout).await?;
        if !same_generation(&current, manifest) {
            return Ok(None);
        }
        if !covered {
            return Err(WriteError::Internal(
                "catalog publication ended without committed coverage".to_owned(),
            ));
        }
        Ok(Some(maintenance))
    })
    .await;
    let release = lock.release().await;
    finish_after_cleanup(
        operation,
        release,
        "catalog lease release also failed after publication",
    )
}
