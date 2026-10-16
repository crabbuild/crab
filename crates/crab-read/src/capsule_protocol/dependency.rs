//! Complete dependency proof with streamed captured catalogs and selected file bindings.
use super::{
    CapsuleDependencyLimits, CapsuleDependencyProof, CapsuleRepositoryView, LayeredCheckpoint,
    PackSourceKind, corrupt_path, install_git_packs_from_store, verify_layered_source,
};
use crate::{ReadError, Result};
use crab_metadata::capsule_protocol::{
    FileCatalogEntry, PointerCatalog, PointerCatalogCounts, PointerCatalogLookup,
};
use crab_storage::{Store, StoreLayout};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use tokio_util::sync::CancellationToken;

// Recovery owns an already assembled replacement catalog; captured views own
// authenticated lookup roots. Both feed the same Git scan and file-byte proof.
enum DependencyCatalog<'a> {
    Assembled(&'a PointerCatalog),
    Captured {
        view: &'a CapsuleRepositoryView,
        lookup: &'a PointerCatalogLookup,
    },
}

impl DependencyCatalog<'_> {
    async fn verify(
        &self,
        layout: &StoreLayout<Store>,
        cancel: &CancellationToken,
    ) -> Result<(PointerCatalogCounts, crate::ReadinessProbeStats)> {
        match self {
            Self::Assembled(catalog) => {
                let stats = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err(ReadError::Cancelled),
                    result = crate::verify_capsule_pointer_catalog_objects(layout, catalog) => result?,
                };
                Ok((
                    PointerCatalogCounts {
                        files: catalog.files().len() as u64,
                        shards: catalog.shards().len() as u64,
                        xorbs: catalog.xorbs().len() as u64,
                    },
                    stats,
                ))
            }
            Self::Captured { view, .. } => {
                let inspection = view.pointer_catalog_inspection().await?;
                crate::selection::verify_capsule_pointer_inspection(layout, inspection, cancel)
                    .await
            }
        }
    }

    async fn files(
        &self,
        layout: &StoreLayout<Store>,
        hashes: &[String],
    ) -> Result<BTreeMap<String, FileCatalogEntry>> {
        match self {
            Self::Assembled(catalog) => Ok(hashes
                .iter()
                .filter_map(|hash| {
                    catalog
                        .files()
                        .get(hash)
                        .map(|entry| (hash.clone(), entry.clone()))
                })
                .collect()),
            Self::Captured { lookup, .. } => {
                let selected = lookup.files(layout.store(), hashes).await?;
                Ok(selected.files().clone())
            }
        }
    }
}

/// Verify every Git and external content dependency reachable from one view.
///
/// This is a deep administrative proof, not a foreground read-path check. It
/// authenticates complete immutable sources, validates the shard/xorb catalog,
/// installs all Git packs in an isolated database, and hashes every reachable
/// Crab file and LFS object at origin. Source verification and pack installation
/// have separate intake phases bounded by `max_git_bytes`.
/// Callers must signal token cancellation and await completion before shutting
/// down the runtime; native installation and Git scan workers drain before return.
pub async fn verify_reachable_dependencies(
    layout: &StoreLayout<Store>,
    view: &CapsuleRepositoryView,
    limits: CapsuleDependencyLimits,
    cancellation: &CancellationToken,
) -> Result<CapsuleDependencyProof> {
    if cancellation.is_cancelled() {
        return Err(ReadError::Cancelled);
    }
    // Fetch-only views have no complete catalog authority. Reject them before
    // source I/O or temporary Git installation, even when their packs are invalid.
    let lookup =
        view.pointer_catalog_lookup(layout, crab_metadata::capsule_protocol::MAX_CATALOG_BYTES)?;
    let mut sources = BTreeMap::new();
    for source in view
        .layered_checkpoint()
        .into_iter()
        .flat_map(LayeredCheckpoint::sources)
        .chain(view.capsule_run_sources())
    {
        let path = match source.kind() {
            PackSourceKind::CapsuleRun => layout.capsule_path(source.object_hash()),
            PackSourceKind::PackLayer => layout.capsule_pack_layer_path(source.object_hash()),
        };
        if let Some(previous) = sources.insert(path.clone(), source)
            && previous != source
        {
            return Err(corrupt_path(
                path.to_string(),
                "immutable source has conflicting descriptors",
            ));
        }
    }
    let source_bytes = sources.values().try_fold(0_u64, |total, source| {
        total
            .checked_add(source.object_size())
            .ok_or_else(|| ReadError::internal("layered source byte count overflowed"))
    })?;
    if limits.max_git_bytes > 0 && source_bytes > limits.max_git_bytes {
        return Err(ReadError::CapsuleReadLimit {
            resource: "layered source bodies",
            maximum: limits.max_git_bytes,
        });
    }
    let pointers = view
        .capsule_run_pointers()
        .iter()
        .map(|pointer| (pointer.hash(), pointer))
        .collect::<BTreeMap<_, _>>();
    // Range installation proves Git members, not unused source framing. Deep
    // administrative proof authenticates each complete source once, separately.
    for source in sources.into_values() {
        let retained_run = match source.kind() {
            PackSourceKind::CapsuleRun => pointers.get(source.object_hash()).copied(),
            PackSourceKind::PackLayer => None,
        };
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ReadError::Cancelled),
            result = verify_layered_source(layout, source, retained_run, source.object_size()) => {
                result?;
            }
        }
    }
    if cancellation.is_cancelled() {
        return Err(ReadError::Cancelled);
    }
    let workspace = tempfile::tempdir()?;
    crab_git::initialize_bare_git_dir(workspace.path()).map_err(std::io::Error::other)?;
    let installation = install_git_packs_from_store(
        view,
        layout,
        workspace.path(),
        limits.max_git_bytes,
        None,
        cancellation,
    );
    tokio::pin!(installation);
    tokio::select! {
        biased;
        () = cancellation.cancelled() => {
            // A started blocking installer cannot be aborted. Keep its database
            // alive until it finishes instead of detaching writes into a removed path.
            let _ = installation.await;
            return Err(ReadError::Cancelled);
        }
        result = &mut installation => {
            result?;
        }
    }
    verify_installed_dependencies_from(
        layout,
        workspace.path(),
        view.refs(),
        DependencyCatalog::Captured {
            view,
            lookup: &lookup,
        },
        limits.pointer_scan,
        cancellation,
    )
    .await
}

/// Verify catalog bodies and reachable Crab/LFS pointers in an installed Git database.
///
/// Callers must install authenticated packs, pin the supplied refs and catalog,
/// and keep the database alive until this future completes. Token cancellation
/// drains its Git scan before returning; catalog and whole-file bytes are checked at origin.
pub async fn verify_installed_dependencies(
    layout: &StoreLayout<Store>,
    git_dir: &Path,
    refs: &BTreeMap<String, String>,
    catalog: &PointerCatalog,
    limits: crab_git::walk::PointerScanLimits,
    cancellation: &CancellationToken,
) -> Result<CapsuleDependencyProof> {
    verify_installed_dependencies_from(
        layout,
        git_dir,
        refs,
        DependencyCatalog::Assembled(catalog),
        limits,
        cancellation,
    )
    .await
}

async fn verify_installed_dependencies_from(
    layout: &StoreLayout<Store>,
    git_dir: &Path,
    refs: &BTreeMap<String, String>,
    catalog: DependencyCatalog<'_>,
    limits: crab_git::walk::PointerScanLimits,
    cancellation: &CancellationToken,
) -> Result<CapsuleDependencyProof> {
    let (counts, catalog_stats) = catalog.verify(layout, cancellation).await?;
    let refs = refs
        .iter()
        .map(|(name, oid)| (name.clone(), oid.clone()))
        .collect::<Vec<_>>();
    let git_dir = git_dir.to_owned();
    let scan_cancel = cancellation.child_token();
    let worker_cancel = scan_cancel.clone();
    let scan = tokio::task::spawn_blocking(move || -> Result<_> {
        let scan = crab_git::walk::scan_pointers(&git_dir, &refs, limits, &|| {
            worker_cancel.is_cancelled()
        })?;
        crab_git::batch::verify_git_dir_blobs(&git_dir, &scan.unchecked_blobs, &|| {
            worker_cancel.is_cancelled()
        })?;
        Ok(scan)
    });
    tokio::pin!(scan);
    let scan = tokio::select! {
        biased;
        () = cancellation.cancelled() => {
            scan_cancel.cancel();
            // The worker borrows the temporary Git database by path. Drain it
            // before that database and any caller-owned admission are released.
            let _ = scan.await;
            return Err(ReadError::Cancelled);
        }
        result = &mut scan => result
            .map_err(|error| ReadError::Io(std::io::Error::other(error)))??,
    };

    let mut verified_files = BTreeSet::new();
    for batch in scan.pointers.chunks(64) {
        let hashes = batch
            .iter()
            .map(|pointer| crab_xet::hash::MerkleHash::from(pointer.file_hash).hex())
            .collect::<Vec<_>>();
        // Selected bindings share the captured roots with the complete proof.
        // Bound each query's recipe expansion independently of reachable history.
        let entries = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ReadError::Cancelled),
            result = catalog.files(layout, &hashes) => result?,
        };
        for pointer in batch {
            let hash = crab_xet::hash::MerkleHash::from(pointer.file_hash).hex();
            let Some(entry) = entries.get(&hash) else {
                return Err(ReadError::CorruptObject {
                    path: gix_hash::ObjectId::Sha1(pointer.oid).to_string(),
                    reason: format!("reachable Crab pointer {hash} is absent from the catalog"),
                });
            };
            if entry.size() != pointer.size {
                return Err(ReadError::CorruptObject {
                    path: gix_hash::ObjectId::Sha1(pointer.oid).to_string(),
                    reason: format!(
                        "reachable Crab pointer {hash} declares size {}, catalog declares {}",
                        pointer.size,
                        entry.size()
                    ),
                });
            }
            // Different Git pointer blobs can name the same content. Validate every
            // declared size above, but reconstruct each file version only once.
            if verified_files.insert(pointer.file_hash) {
                let pointer = crab_types::pointer::Pointer {
                    file_hash: pointer.file_hash,
                    size: pointer.size,
                    shard_hint: None,
                };
                crate::verify_catalog_file_recipe(layout, &pointer, entry, cancellation).await?;
            }
        }
    }

    let lfs = crab_lfs::LfsObjectStore::new(layout.store().clone(), layout.repo_prefix());
    let mut lfs_objects = BTreeMap::new();
    for blob in scan.lfs_pointers {
        if let Some(previous) = lfs_objects.insert(blob.pointer.oid, blob.pointer.size)
            && previous != blob.pointer.size
        {
            return Err(ReadError::CorruptObject {
                path: gix_hash::ObjectId::Sha1(blob.oid).to_string(),
                reason: "reachable LFS pointers declare conflicting sizes for one object"
                    .to_owned(),
            });
        }
    }
    for (oid, size) in &lfs_objects {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ReadError::Cancelled),
            result = lfs.verify_origin(oid, *size) => result?,
        }
    }

    Ok(CapsuleDependencyProof {
        catalog_files: counts.files,
        catalog_shards: counts.shards,
        catalog_xorbs: counts.xorbs,
        catalog_objects_read: catalog_stats.object_read_count,
        reachable_crab_pointers: scan.pointers.len() as u64,
        reachable_lfs_objects: lfs_objects.len() as u64,
        reachable_lfs_bytes: lfs_objects.values().try_fold(0_u64, |total, size| {
            total
                .checked_add(*size)
                .ok_or_else(|| ReadError::internal("LFS dependency byte count overflowed"))
        })?,
    })
}

#[cfg(test)]
#[path = "dependency_tests.rs"]
mod tests;
