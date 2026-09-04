//! Lifecycle for generation-bound catalog publication.
use std::{collections::HashMap, sync::Arc, time::Duration};

use crab_coordination::{
    CoordinationError, GIT_OBJECT_LOCATOR_RESOURCE, PushLock, PushLockAcquireContext,
};
use crab_metadata::{
    git_object_locator::{
        GitLocatorCoverage, GitObjectCatalogStats, GitObjectLocatorSession, GitObjectLocatorWriter,
        LocatorSweepStats,
    },
    manifest_store,
    manifests::{Manifest, PackManifestEntry},
};
use crab_storage::{Store, StoreLayout};
use crab_xet::hash::MerkleHash;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::{Result, WriteError, catalog::publish_inventory};

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
        let bindings = session.pack_bindings().await;
        let close = session.close().await;
        let bindings = match (bindings, close) {
            (Ok(bindings), Ok(())) => bindings,
            (Err(error), Ok(())) | (Ok(_), Err(error)) => return Err(error.into()),
            (Err(error), Err(close_error)) => {
                warn!(error = %close_error, "catalog planning session close also failed");
                return Err(error.into());
            }
        };
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
        let (maintenance, covered) = match (result, close) {
            (Ok(result), Ok(_)) => result,
            (Err(error), Ok(_)) => return Err(error),
            (Ok(_), Err(error)) => return Err(error.into()),
            (Err(error), Err(close_error)) => {
                warn!(error = %close_error, "catalog writer close also failed after publication");
                return Err(error);
            }
        };
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
    match (operation, release) {
        (Ok(result), Ok(())) => Ok(result),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error.into()),
        (Err(error), Err(release_error)) => {
            warn!(error = %release_error, "catalog lease release also failed after publication");
            Err(error)
        }
    }
}
