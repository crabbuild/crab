//! Verified Git-pack consolidation for capsule-protocol checkpoints.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use bytes::Bytes;
use crab_git::pack::VerifiedPackIdentity;
use crab_git::repack::{GeometricRepackedPack, RepackSource};
use crab_metadata::capsule_protocol::{
    CapsuleGitPack, LayeredCheckpoint, LayeredObjectMember, LayeredVisibilitySnapshot, PackLayer,
    PackMemberDescriptor, PackRange, PackSourceDescriptor, PackSourceKind, PointerCatalog,
    source_catalog_digest,
};
use crab_storage::{Store, StoreLayout};
use tokio_util::sync::CancellationToken;

const LAYERED_MAX_PHYSICAL_SOURCES: usize = 64;
// One run can expand to hundreds of local packs; bound clone/index work after roll-up.
const LAYERED_PACK_MEMBER_TARGET: usize = 8;
// Physical maintenance is bounded independently of logical checkpointing.
// The format's hard source limit can still require a minimal admission roll-up.
const LAYERED_SUFFIX_BYTE_BUDGET: u64 = 512 * 1024 * 1024;

/// Publication status and logical pack-body work completed by maintenance.
///
/// Body counts exclude metadata, sidecars, external-base resolution, readback
/// verification and transport retries. Writes count replacement bodies submitted
/// to verified immutable publication, including an already-present identical
/// object. A lost root CAS retains the work counts with `published = false`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointOutcome {
    /// Whether this pass replaced the root, independently of work already done.
    pub published: bool,
    /// Distinct selected pack bodies installed for consolidation.
    pub pack_bytes_read: u64,
    /// Replacement pack bodies submitted to verified immutable publication.
    pub pack_bytes_written: u64,
}

impl CheckpointOutcome {
    /// Combine the work of consecutive logical and physical maintenance passes.
    pub fn combine(self, next: Self) -> Result<Self, CheckpointError> {
        Ok(Self {
            published: self.published || next.published,
            pack_bytes_read: self
                .pack_bytes_read
                .checked_add(next.pack_bytes_read)
                .ok_or(CheckpointError::AccountingOverflow)?,
            pack_bytes_written: self
                .pack_bytes_written
                .checked_add(next.pack_bytes_written)
                .ok_or(CheckpointError::AccountingOverflow)?,
        })
    }
}

struct LayeredConsolidation {
    sources: Vec<PackSourceDescriptor>,
    member_oids: BTreeMap<String, Vec<Vec<[u8; 20]>>>,
    work: CheckpointOutcome,
}

/// Independent maintenance phases and the last successfully published inventory.
///
/// Inventory excludes concurrent ref-head changes not captured by this pass.
/// If neither phase publishes, it describes the original caller's view.
#[derive(Debug, Clone, Copy)]
pub struct MaintenanceOutcome {
    /// Logical publication, including any source-limit admission work.
    pub checkpointed: CheckpointOutcome,
    /// Independently published physical suffix replacement.
    pub repacked: CheckpointOutcome,
    /// Distinct pack bodies in this pass's resulting inventory.
    pub packs_after: usize,
    /// Total authenticated pack-body bytes in that inventory.
    pub bytes_after: u64,
}

/// One atomic interactive repack and its resulting pinned inventory.
#[derive(Debug, Clone, Copy)]
pub struct CapsuleRepackOutcome {
    /// Body work and publication status for this pass.
    pub work: CheckpointOutcome,
    /// Distinct pack bodies in the published inventory, or the input inventory on CAS loss.
    pub packs_after: usize,
    /// Authenticated pack-body bytes in that inventory.
    pub bytes_after: u64,
}

#[derive(Default)]
struct CheckpointPublication {
    work: CheckpointOutcome,
    committed: Option<(
        crab_metadata::capsule_protocol::RootSnapshot,
        LayeredCheckpoint,
    )>,
}

/// Failure while producing a complete replacement Git-pack inventory.
#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error("checkpoint pack-body accounting overflowed")]
    AccountingOverflow,
    #[error("checkpoint consolidation cancelled")]
    Cancelled,
    #[error("capsule repository read failed")]
    Read(#[from] crab_read::ReadError),
    #[error("Git pack consolidation failed")]
    Repack(#[from] crab_git::repack::RepackError),
    #[error("Git pack evidence failed")]
    Pack(#[from] crab_git::pack::PackError),
    #[error("checkpoint metadata failed")]
    Metadata(#[from] crab_metadata::error::MetadataError),
    #[error("checkpoint storage failed")]
    Storage(#[from] crab_storage::StorageError),
    #[error("checkpoint publication failed")]
    Write(#[from] crab_write::WriteError),
    #[error("checkpoint file I/O failed")]
    Io(#[from] std::io::Error),
    #[error("checkpoint worker failed")]
    Worker(#[from] tokio::task::JoinError),
    #[error("temporary Git repository initialization failed: {stderr}")]
    GitInit { stderr: String },
    #[error("checkpoint output exceeds the {maximum}-byte limit")]
    OutputLimit { maximum: u64 },
    #[error("checkpoint pack path has no canonical content identifier: {path}")]
    InvalidPackPath { path: PathBuf },
}

/// Compact a capsule repository once its immutable frontier reaches `threshold`.
///
/// The root and every ref position remain pinned through publication. A
/// concurrent root replacement is benign: its owner won publication and a
/// later maintenance pass can retry from that newer authority. Layered pack
/// bodies are unchanged unless admitting sources would exceed the format limit.
pub async fn publish_capsule_checkpoint(
    layout: &StoreLayout<Store>,
    threshold: u32,
    maximum_bytes: u64,
    cancel: &CancellationToken,
) -> Result<CheckpointOutcome, CheckpointError> {
    check_cancelled(cancel)?;
    let root = crab_write::capsule_protocol::open_root(layout).await?;
    let limits = crab_read::capsule_protocol::CapsuleReadLimits {
        max_capsule_bytes: maximum_bytes,
        max_frontier_bytes: maximum_bytes,
    };
    let view =
        crab_read::capsule_protocol::open_view_from_root_for_checkpoint(layout, root, limits)
            .await?;
    publish_capsule_checkpoint_from_view(layout, &view, threshold, maximum_bytes, cancel).await
}

/// Run logical checkpointing followed by independently pinned physical maintenance.
///
/// Background owners use this pass even below the frontier threshold so a
/// previously interrupted physical repack can finish without another push.
/// Foreground admission must call [`publish_capsule_checkpoint`] instead.
pub async fn maintain_capsule_repository(
    layout: &StoreLayout<Store>,
    threshold: u32,
    maximum_bytes: u64,
    cancel: &CancellationToken,
) -> Result<CheckpointOutcome, CheckpointError> {
    check_cancelled(cancel)?;
    let root = crab_write::capsule_protocol::open_root(layout).await?;
    let view = crab_read::capsule_protocol::open_view_from_root_for_checkpoint(
        layout,
        root,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: maximum_bytes,
            max_frontier_bytes: maximum_bytes,
        },
    )
    .await?;
    let outcome =
        maintain_capsule_repository_from_view(layout, &view, threshold, maximum_bytes, cancel)
            .await?;
    outcome.checkpointed.combine(outcome.repacked)
}

/// Maintain a pinned view, retaining successful publication receipts between phases.
///
/// Each phase keeps its own root CAS. Reusing the exact committed root and
/// checkpoint avoids rereads without folding newer ref heads or trusting a
/// later, unrelated root. Uncertain publication still fails in the writer.
pub async fn maintain_capsule_repository_from_view(
    layout: &StoreLayout<Store>,
    view: &crab_read::capsule_protocol::CapsuleRepositoryView,
    threshold: u32,
    maximum_bytes: u64,
    cancel: &CancellationToken,
) -> Result<MaintenanceOutcome, CheckpointError> {
    check_cancelled(cancel)?;
    let count = view.capsule_count()?;
    let logical_due =
        count >= u64::from(threshold) && (count > 0 || view.layered_checkpoint().is_none());
    let logical = if logical_due {
        publish_checkpoint_with_catalog(
            layout,
            view,
            view.pointer_catalog()?,
            maximum_bytes,
            cancel,
        )
        .await?
    } else {
        CheckpointPublication::default()
    };
    check_cancelled(cancel)?;
    let limits = crab_read::capsule_protocol::CapsuleReadLimits {
        max_capsule_bytes: maximum_bytes,
        max_frontier_bytes: maximum_bytes,
    };
    let retained = logical.committed.or_else(|| {
        // Once logical publication loses its CAS, this root is already stale.
        // Keep completed work counts, but do not build another doomed replacement.
        if logical_due {
            return None;
        }
        view.layered_checkpoint()
            .map(|checkpoint| (view.root_snapshot().clone(), checkpoint.clone()))
    });
    let compacted = retained
        .map(|(root, checkpoint)| {
            crab_read::capsule_protocol::compacted_view_from_checkpoint(root, checkpoint, limits)
        })
        .transpose()?;
    let physical = match compacted.as_ref() {
        Some(compacted) => {
            repack_checkpoint_from_view(layout, compacted, maximum_bytes, cancel).await?
        }
        None => CheckpointPublication::default(),
    };
    let published = physical
        .committed
        .map(|(root, checkpoint)| {
            crab_read::capsule_protocol::compacted_view_from_checkpoint(root, checkpoint, limits)
        })
        .transpose()?;
    let inventory = published
        .as_ref()
        .or(compacted.as_ref().filter(|_| logical.work.published))
        .unwrap_or(view);
    Ok(MaintenanceOutcome {
        checkpointed: logical.work,
        repacked: physical.work,
        packs_after: inventory.git_pack_count(),
        bytes_after: inventory.git_pack_bytes()?,
    })
}

/// Checkpoint and geometrically compact one pinned view with a single root CAS.
///
/// Interactive repack has one publication boundary. Background maintenance
/// retains its independently cancellable logical and physical phases.
pub async fn repack_capsule_repository_from_view(
    layout: &StoreLayout<Store>,
    view: &crab_read::capsule_protocol::CapsuleRepositoryView,
    maximum_bytes: u64,
    cancel: &CancellationToken,
) -> Result<CapsuleRepackOutcome, CheckpointError> {
    check_cancelled(cancel)?;
    let sources = checkpoint_sources(view);
    let logical_due = view.capsule_count()? > 0;
    let mut publication = CheckpointPublication::default();

    if !sources.is_empty() {
        let geometric_start = layered_suffix_start(&sources)?;
        let admission_start = (sources.len() > LAYERED_MAX_PHYSICAL_SOURCES)
            .then_some(LAYERED_MAX_PHYSICAL_SOURCES - 1);
        let selected_start = match (geometric_start, admission_start) {
            (Some(geometric), Some(admission)) => Some(geometric.min(admission)),
            (Some(geometric), None) => Some(geometric),
            (None, Some(admission)) => Some(admission),
            (None, None) => None,
        };

        if let Some(selected_start) = selected_start {
            let consolidation = consolidate_layered_suffix(
                layout,
                view,
                sources,
                selected_start,
                maximum_bytes,
                cancel,
            )
            .await?;
            let mut member_oids = view.capsule_run_member_oids().clone();
            member_oids.extend(consolidation.member_oids);
            let committed = publish_layered_sources(
                layout,
                view,
                consolidation.sources,
                member_oids,
                view.pointer_catalog()?,
                maximum_bytes,
                cancel,
            )
            .await?;
            publication = CheckpointPublication {
                work: CheckpointOutcome {
                    published: committed.is_some(),
                    ..consolidation.work
                },
                committed,
            };
        } else if logical_due {
            publication = publish_checkpoint_with_catalog(
                layout,
                view,
                view.pointer_catalog()?,
                maximum_bytes,
                cancel,
            )
            .await?;
        }
    }

    let limits = crab_read::capsule_protocol::CapsuleReadLimits {
        max_capsule_bytes: maximum_bytes,
        max_frontier_bytes: maximum_bytes,
    };
    let published = publication
        .committed
        .map(|(root, checkpoint)| {
            crab_read::capsule_protocol::compacted_view_from_checkpoint(root, checkpoint, limits)
        })
        .transpose()?;
    let inventory = published.as_ref().unwrap_or(view);
    Ok(CapsuleRepackOutcome {
        work: publication.work,
        packs_after: inventory.git_pack_count(),
        bytes_after: inventory.git_pack_bytes()?,
    })
}

/// Compact one already authenticated repository view when its frontier is due.
///
/// Reusing the caller's pinned view avoids a second mutable-root and ref-head
/// capture. Publication still compares against that exact root, so concurrent
/// writers either win cleanly or leave this maintenance pass as a no-op.
pub async fn publish_capsule_checkpoint_from_view(
    layout: &StoreLayout<Store>,
    view: &crab_read::capsule_protocol::CapsuleRepositoryView,
    threshold: u32,
    maximum_bytes: u64,
    cancel: &CancellationToken,
) -> Result<CheckpointOutcome, CheckpointError> {
    check_cancelled(cancel)?;
    if view.capsule_count()? < u64::from(threshold) {
        return Ok(CheckpointOutcome::default());
    }
    let catalog = view.pointer_catalog()?;
    publish_capsule_checkpoint_with_catalog_from_view(layout, view, catalog, maximum_bytes, cancel)
        .await
}

/// Publish one checkpoint from a pinned view with a replacement pointer catalog.
///
/// The caller must make every external object named by `catalog` durable and
/// verify its complete dependency closure before calling. Publication retains
/// the captured ref frontier and succeeds only against the view's exact root.
pub async fn publish_capsule_checkpoint_with_catalog_from_view(
    layout: &StoreLayout<Store>,
    view: &crab_read::capsule_protocol::CapsuleRepositoryView,
    catalog: crab_metadata::capsule_protocol::PointerCatalog,
    maximum_bytes: u64,
    cancel: &CancellationToken,
) -> Result<CheckpointOutcome, CheckpointError> {
    Ok(
        publish_checkpoint_with_catalog(layout, view, catalog, maximum_bytes, cancel)
            .await?
            .work,
    )
}

async fn publish_checkpoint_with_catalog(
    layout: &StoreLayout<Store>,
    view: &crab_read::capsule_protocol::CapsuleRepositoryView,
    catalog: PointerCatalog,
    maximum_bytes: u64,
    cancel: &CancellationToken,
) -> Result<CheckpointPublication, CheckpointError> {
    check_cancelled(cancel)?;
    let mut sources = checkpoint_sources(view);
    if sources.is_empty() {
        return Ok(CheckpointPublication::default());
    }
    let mut member_oids = view.capsule_run_member_oids().clone();
    let mut work = CheckpointOutcome::default();
    if sources.len() > LAYERED_MAX_PHYSICAL_SOURCES {
        // Only the smallest suffix needed for format admission may block this
        // logical publication; geometric debt belongs to physical maintenance.
        let selected_start = LAYERED_MAX_PHYSICAL_SOURCES - 1;
        let consolidation = consolidate_layered_suffix(
            layout,
            view,
            sources,
            selected_start,
            maximum_bytes,
            cancel,
        )
        .await?;
        sources = consolidation.sources;
        member_oids.extend(consolidation.member_oids);
        work = consolidation.work;
    }
    let committed = publish_layered_sources(
        layout,
        view,
        sources,
        member_oids,
        catalog,
        maximum_bytes,
        cancel,
    )
    .await?;
    work.published = committed.is_some();
    Ok(CheckpointPublication { work, committed })
}

fn checkpoint_sources(
    view: &crab_read::capsule_protocol::CapsuleRepositoryView,
) -> Vec<PackSourceDescriptor> {
    let mut sources = Vec::new();
    let mut source_hashes = BTreeSet::new();
    if let Some(existing) = view.layered_checkpoint() {
        for source in existing.sources() {
            if source_hashes.insert(source.object_hash().to_owned()) {
                sources.push(source.clone());
            }
        }
    }
    for source in view.capsule_run_sources() {
        if source_hashes.insert(source.object_hash().to_owned()) {
            sources.push(source.clone());
        }
    }
    sources
}

/// Repack a bounded source suffix belonging to one already published checkpoint.
///
/// Newer per-ref heads are neither folded nor rewritten. The replacement keeps
/// the checkpoint's refs, transaction positions and history, and uses its exact
/// root CAS token. A stale root loses publication without exposing partial state.
pub async fn repack_capsule_checkpoint_from_root(
    layout: &StoreLayout<Store>,
    root: crab_metadata::capsule_protocol::RootSnapshot,
    maximum_bytes: u64,
    cancel: &CancellationToken,
) -> Result<CheckpointOutcome, CheckpointError> {
    check_cancelled(cancel)?;
    if root.record().root().checkpoint().is_none() {
        return Ok(CheckpointOutcome::default());
    }
    let view = crab_read::capsule_protocol::open_compacted_view_from_root(
        layout,
        root,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: maximum_bytes,
            max_frontier_bytes: maximum_bytes,
        },
    )
    .await?;
    Ok(
        repack_checkpoint_from_view(layout, &view, maximum_bytes, cancel)
            .await?
            .work,
    )
}

async fn repack_checkpoint_from_view(
    layout: &StoreLayout<Store>,
    view: &crab_read::capsule_protocol::CapsuleRepositoryView,
    maximum_bytes: u64,
    cancel: &CancellationToken,
) -> Result<CheckpointPublication, CheckpointError> {
    check_cancelled(cancel)?;
    let checkpoint = view.layered_checkpoint().ok_or_else(|| {
        layered_admission_error("physical maintenance requires a layered checkpoint")
    })?;
    let Some(selected_start) = layered_suffix_start(checkpoint.sources())? else {
        return Ok(CheckpointPublication::default());
    };
    let catalog = checkpoint.pointer_catalog()?;
    let consolidation = consolidate_layered_suffix(
        layout,
        view,
        checkpoint.sources().to_vec(),
        selected_start,
        maximum_bytes,
        cancel,
    )
    .await?;
    let committed = publish_layered_sources(
        layout,
        view,
        consolidation.sources,
        consolidation.member_oids,
        catalog,
        maximum_bytes,
        cancel,
    )
    .await?;
    Ok(CheckpointPublication {
        work: CheckpointOutcome {
            published: committed.is_some(),
            ..consolidation.work
        },
        committed,
    })
}

async fn publish_layered_sources(
    layout: &StoreLayout<Store>,
    view: &crab_read::capsule_protocol::CapsuleRepositoryView,
    sources: Vec<PackSourceDescriptor>,
    member_oids: BTreeMap<String, Vec<Vec<[u8; 20]>>>,
    catalog: PointerCatalog,
    maximum_bytes: u64,
    cancel: &CancellationToken,
) -> Result<
    Option<(
        crab_metadata::capsule_protocol::RootSnapshot,
        LayeredCheckpoint,
    )>,
    CheckpointError,
> {
    let catalog_digest = source_catalog_digest(&sources)?;
    let visibility_index = view.git_visibility_index()?;
    let member_admission = layered_member_admission(
        &visibility_index,
        &sources,
        view.layered_checkpoint(),
        &member_oids,
    )?;
    let visibility = LayeredVisibilitySnapshot::from_index_with_member_admission(
        &visibility_index,
        &catalog_digest,
        member_admission,
    )?;
    let checkpoint =
        crab_metadata::capsule_protocol::LayeredCheckpoint::build_with_ordinal_visibility(
            view.root().root().generation(),
            view.root().digest(),
            sources,
            catalog,
            Some(visibility),
        )?;
    if checkpoint.bytes().len() as u64 > maximum_bytes && maximum_bytes > 0 {
        return Err(CheckpointError::OutputLimit {
            maximum: maximum_bytes,
        });
    }
    check_cancelled(cancel)?;
    let result = if view.visible_ref_transactions().is_empty() {
        crab_write::capsule_protocol::publish_layered_checkpoint(
            layout,
            view.root_snapshot().clone(),
            &checkpoint,
        )
        .await
    } else {
        crab_write::capsule_protocol::publish_ref_layered_checkpoint(
            layout,
            view.root_snapshot().clone(),
            &checkpoint,
            view.refs().clone(),
            view.peeled_refs().clone(),
            view.visible_ref_transactions().clone(),
            view.capsule_run_pointers().to_vec(),
        )
        .await
    };
    match result {
        Ok(root) => Ok(Some((root, checkpoint))),
        Err(crab_write::WriteError::CapsuleRootChanged { .. }) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

async fn consolidate_layered_suffix(
    layout: &StoreLayout<Store>,
    view: &crab_read::capsule_protocol::CapsuleRepositoryView,
    sources: Vec<PackSourceDescriptor>,
    selected_start: usize,
    maximum_bytes: u64,
    cancel: &CancellationToken,
) -> Result<LayeredConsolidation, CheckpointError> {
    if selected_start >= sources.len() {
        return Err(CheckpointError::Repack(
            crab_git::repack::RepackError::SourceIntegrity {
                pack_id: "layered-suffix".to_owned(),
                reason: "layered suffix start is outside the source inventory".to_owned(),
            },
        ));
    }
    let selected = sources[selected_start..]
        .iter()
        .flat_map(|source| source.members())
        .map(|member| member.pack().blake3().to_owned())
        .collect::<BTreeSet<_>>();
    if selected.is_empty() {
        return Err(CheckpointError::Repack(
            crab_git::repack::RepackError::SourceIntegrity {
                pack_id: "layered-suffix".to_owned(),
                reason: "layered suffix has no packs to consolidate".to_owned(),
            },
        ));
    }
    let delta_base_ids = sources[selected_start..]
        .iter()
        .flat_map(|source| source.members())
        .flat_map(|member| member.external_delta_bases())
        .map(|base| {
            gix_hash::ObjectId::from_hex(base.as_bytes()).map_err(|error| {
                CheckpointError::Repack(crab_git::repack::RepackError::SourceIntegrity {
                    pack_id: "layered-suffix".to_owned(),
                    reason: format!("external REF_DELTA base is invalid: {error}"),
                })
            })
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    let delta_bases =
        read_layered_delta_bases(layout, view, &delta_base_ids, maximum_bytes, cancel).await?;
    check_cancelled(cancel)?;
    let workspace = tempfile::tempdir()?;
    let git_dir = workspace.path().join("repository.git");
    let init_path = git_dir.clone();
    tokio::task::spawn_blocking(move || initialize_bare_repository(&init_path)).await??;
    // Pack identities may occur in both the stable prefix and selected suffix.
    // Restrict physical sources as well as members so duplicates cannot pull
    // stable bodies into maintenance or move their authenticated member slots.
    let installed =
        crab_read::capsule_protocol::install_layered_git_packs_from_store_sources_selected(
            &sources[selected_start..],
            &[],
            view.capsules(),
            layout,
            &git_dir,
            maximum_bytes,
            Some(&selected),
            cancel,
        )
        .await?;
    check_cancelled(cancel)?;
    let sources_for_repack = repack_sources(installed)?;
    let pack_bytes_read = sources_for_repack.iter().try_fold(0_u64, |total, source| {
        total
            .checked_add(source.size)
            .ok_or(CheckpointError::AccountingOverflow)
    })?;
    let delta_bases_for_repack = delta_bases;
    let mut kind_git_dir = git_dir.clone();
    if !delta_bases_for_repack.is_empty() {
        kind_git_dir = workspace.path().join("resolved.git");
    }
    let index_concurrency =
        std::thread::available_parallelism().map_or(1, |parallelism| parallelism.get().min(8));
    let (replacement, kind_git_dir) = tokio::task::spawn_blocking(move || {
        // Native Git requires self-contained on-disk packs even for kind queries.
        // Repair only a scratch copy; temporary bases must not enter the selected
        // inventory or the byte-preserving replacement's object universe.
        if !delta_bases_for_repack.is_empty() {
            initialize_bare_repository(&kind_git_dir)?;
            crab_git::repack::resolve_pack_delta_bases(
                &kind_git_dir,
                &sources_for_repack,
                &delta_bases_for_repack,
            )?;
        }
        let replacement = crab_git::repack::consolidate_committed_pack_suffix_with_delta_bases(
            &sources_for_repack,
            &delta_bases_for_repack,
            index_concurrency,
        )?;
        Ok::<_, CheckpointError>((replacement, kind_git_dir))
    })
    .await??;
    check_cancelled(cancel)?;
    let packs = materialize_packs(replacement.packs(), &kind_git_dir, maximum_bytes, cancel)?;
    if packs.is_empty() {
        return Err(CheckpointError::Repack(
            crab_git::repack::RepackError::SourceIntegrity {
                pack_id: "layered-suffix".to_owned(),
                reason: "layered suffix consolidation produced no pack".to_owned(),
            },
        ));
    }
    let mut retained = sources[..selected_start].to_vec();
    let mut member_oids = BTreeMap::new();
    let mut pack_bytes_written = 0_u64;
    for pack in packs {
        let (object_ids, pack_checksum) =
            crab_git::pack_locator::sorted_object_ids_from_index_bytes(pack.index_bytes())
                .map_err(|error| {
                    CheckpointError::Repack(crab_git::repack::RepackError::SourceIntegrity {
                        pack_id: pack.git_checksum().to_owned(),
                        reason: format!("replacement pack index is invalid: {error}"),
                    })
                })?;
        if object_ids.len() as u64 != pack.object_count()
            || pack_checksum.to_string() != pack.git_checksum()
        {
            return Err(CheckpointError::Repack(
                crab_git::repack::RepackError::SourceIntegrity {
                    pack_id: pack.git_checksum().to_owned(),
                    reason: "replacement pack index does not match its descriptor".to_owned(),
                },
            ));
        }
        let layer = PackLayer::build(&pack)?;
        let layer_hash = layer.hash().to_owned();
        layout
            .store()
            .put_if_absent_verified(
                &layout.capsule_pack_layer_path(&layer_hash),
                layer.bytes().clone(),
            )
            .await?;
        pack_bytes_written = pack_bytes_written
            .checked_add(pack.pack_size())
            .ok_or(CheckpointError::AccountingOverflow)?;
        retained.push(layer.source_descriptor()?);
        member_oids.insert(
            layer_hash,
            vec![
                object_ids
                    .into_iter()
                    .map(|object_id| {
                        object_id.as_bytes().try_into().map_err(|_| {
                            CheckpointError::Repack(
                                crab_git::repack::RepackError::SourceIntegrity {
                                    pack_id: pack.git_checksum().to_owned(),
                                    reason: "replacement pack index is not SHA-1".to_owned(),
                                },
                            )
                        })
                    })
                    .collect::<Result<Vec<[u8; 20]>, CheckpointError>>()?,
            ],
        );
    }
    Ok(LayeredConsolidation {
        sources: retained,
        member_oids,
        work: CheckpointOutcome {
            published: false,
            pack_bytes_read,
            pack_bytes_written,
        },
    })
}

fn layered_member_admission(
    index: &crab_metadata::git_visibility::GitVisibilityIndex,
    sources: &[PackSourceDescriptor],
    existing: Option<&LayeredCheckpoint>,
    member_oids: &BTreeMap<String, Vec<Vec<[u8; 20]>>>,
) -> Result<Vec<LayeredObjectMember>, CheckpointError> {
    let mut by_oid = BTreeMap::<[u8; 20], LayeredObjectMember>::new();

    if let Some(existing) = existing
        && let Some(snapshot) = existing.visibility_ordinal_snapshot()?
        && let Some(admission) = snapshot.member_admission()
    {
        if admission.len() != snapshot.objects().len() {
            return Err(layered_admission_error(
                "existing visibility member admission count is invalid",
            ));
        }
        for (oid, member) in snapshot.objects().iter().zip(admission) {
            let old_source = existing
                .sources()
                .get(usize::from(member.source_index()))
                .ok_or_else(|| {
                    layered_admission_error("existing visibility source admission is invalid")
                })?;
            let Some(source_index) = sources
                .iter()
                .position(|source| source.object_hash() == old_source.object_hash())
            else {
                continue;
            };
            if sources[source_index]
                .members()
                .get(usize::from(member.member_index()))
                .is_none()
            {
                return Err(layered_admission_error(
                    "existing visibility member admission is invalid",
                ));
            }
            let source_index = u16::try_from(source_index)
                .map_err(|_| layered_admission_error("layered source index overflows"))?;
            by_oid.insert(
                *oid,
                LayeredObjectMember::new(source_index, member.member_index()),
            );
        }
    }

    for (source_index, source) in sources.iter().enumerate() {
        let Some(members) = member_oids.get(source.object_hash()) else {
            continue;
        };
        if members.len() != source.members().len() {
            return Err(layered_admission_error(
                "source member admission count does not match its descriptor",
            ));
        }
        let source_index = u16::try_from(source_index)
            .map_err(|_| layered_admission_error("layered source index overflows"))?;
        for (member_index, object_ids) in members.iter().enumerate() {
            let descriptor = &source.members()[member_index];
            if object_ids.len() as u64 != descriptor.object_count() {
                return Err(layered_admission_error(
                    "source member object admission count does not match its descriptor",
                ));
            }
            let member_index = u16::try_from(member_index)
                .map_err(|_| layered_admission_error("layered member index overflows"))?;
            for oid in object_ids {
                by_oid.insert(*oid, LayeredObjectMember::new(source_index, member_index));
            }
        }
    }

    index
        .objects()
        .iter()
        .map(|oid| {
            by_oid.get(oid).copied().ok_or_else(|| {
                layered_admission_error("visible Git object has no authenticated pack member")
            })
        })
        .collect()
}

fn layered_admission_error(reason: impl Into<String>) -> CheckpointError {
    CheckpointError::Repack(crab_git::repack::RepackError::SourceIntegrity {
        pack_id: "layered-visibility".to_owned(),
        reason: reason.into(),
    })
}

async fn read_layered_delta_bases(
    layout: &StoreLayout<Store>,
    view: &crab_read::capsule_protocol::CapsuleRepositoryView,
    base_ids: &BTreeSet<gix_hash::ObjectId>,
    maximum_bytes: u64,
    cancel: &CancellationToken,
) -> Result<Vec<crab_git::repack::RepackDeltaBase>, CheckpointError> {
    if base_ids.is_empty() {
        return Ok(Vec::new());
    }
    check_cancelled(cancel)?;
    let bucket = layout.store().bucket_identity();
    let provider = format!("{:?}:{}:{}", bucket.cloud, bucket.host, bucket.container);
    let identity =
        crab_remote_git::RepositoryIdentity::new(provider, layout.repo_prefix().to_owned(), 1)
            .map_err(|error| crab_read::ReadError::Internal(error.to_string()))?;
    let options = crab_read::upload_pack_repository_options()
        .map_err(|error| crab_read::ReadError::Internal(error.to_string()))?;
    let requested = base_ids.iter().copied().collect::<Vec<_>>();
    let runtime = Arc::new(crab_remote_git::RemoteGitRuntime::default());
    let objects: crab_read::Result<_> = async {
        let repository = view
            .git_repository_from_store(
                layout.clone(),
                identity,
                runtime.clone(),
                options,
                maximum_bytes,
                cancel,
            )
            .await?;
        let operation = repository
            .operation(crab_remote_git::OperationKind::Repository, cancel)
            .await
            .map_err(|error| crab_read::ReadError::Internal(error.to_string()))?;
        let objects = operation.read_objects(&requested).await;
        operation
            .finish(objects)
            .await
            .map_err(|error| crab_read::ReadError::Internal(error.to_string()))
    }
    .await;
    // This runtime has no longer-lived owner. Finish/drop the context before
    // draining its tracked work, including when opening or reading failed.
    runtime.shutdown().await;
    let objects = objects?;
    if objects.len() != requested.len()
        || objects
            .iter()
            .zip(&requested)
            .any(|(object, expected)| object.oid != *expected)
    {
        return Err(CheckpointError::Repack(
            crab_git::repack::RepackError::SourceIntegrity {
                pack_id: "layered-suffix".to_owned(),
                reason: "external REF_DELTA base read returned the wrong object set".to_owned(),
            },
        ));
    }
    Ok(objects
        .into_iter()
        .map(|object| crab_git::repack::RepackDeltaBase {
            oid: object.oid,
            kind: object.kind,
            data: object.data.to_vec(),
        })
        .collect())
}

fn layered_suffix_start(
    sources: &[PackSourceDescriptor],
) -> Result<Option<usize>, CheckpointError> {
    let weights = sources
        .iter()
        .map(PackSourceDescriptor::compressed_bytes)
        .collect::<Result<Vec<_>, _>>()?;
    let member_counts = sources
        .iter()
        .map(|source| source.members().len())
        .collect::<Vec<_>>();
    Ok(layered_suffix_start_for_inventory(&weights, &member_counts))
}

fn layered_suffix_start_for_inventory(weights: &[u64], member_counts: &[usize]) -> Option<usize> {
    if weights.len() != member_counts.len() {
        return None;
    }
    let geometric = layered_suffix_start_for_weights(weights);
    let member_bound = layered_member_suffix_start(member_counts).filter(|start| {
        weights[*start..]
            .iter()
            .copied()
            .fold(0_u64, u64::saturating_add)
            <= LAYERED_SUFFIX_BYTE_BUDGET
    });
    match (geometric, member_bound) {
        (Some(geometric), Some(member_bound)) => Some(geometric.min(member_bound)),
        (Some(geometric), None) => Some(geometric),
        (None, Some(member_bound)) => Some(member_bound),
        (None, None) => None,
    }
}

fn layered_member_suffix_start(member_counts: &[usize]) -> Option<usize> {
    let total_members = member_counts
        .iter()
        .copied()
        .fold(0_usize, usize::saturating_add);
    if total_members <= LAYERED_PACK_MEMBER_TARGET {
        return None;
    }

    let members_to_replace = total_members - LAYERED_PACK_MEMBER_TARGET + 1;
    let mut selected_members = 0_usize;
    for (index, member_count) in member_counts.iter().enumerate().rev() {
        selected_members = selected_members.saturating_add(*member_count);
        if selected_members >= members_to_replace {
            return Some(index);
        }
    }
    None
}

fn layered_suffix_start_for_weights(weights: &[u64]) -> Option<usize> {
    if weights.len() < 2 {
        return None;
    }
    let geometric_rollup = crab_git::repack::incremental_repack_cut_in_order(weights, 2);
    let minimum_rollup = weights
        .len()
        .saturating_sub(LAYERED_MAX_PHYSICAL_SOURCES.saturating_sub(1));
    let rollup_count = geometric_rollup.max(minimum_rollup);
    if rollup_count == 0 {
        return None;
    }
    let rollup_count = rollup_count.max(2).min(weights.len());
    let start = weights.len() - rollup_count;
    let suffix_bytes = weights[start..]
        .iter()
        .copied()
        .fold(0_u64, u64::saturating_add);
    if suffix_bytes > LAYERED_SUFFIX_BYTE_BUDGET {
        if minimum_rollup == 0 {
            return None;
        }
        // A source-count overflow is the only case where we accept a budget
        // overrun: retaining all sources would make the checkpoint invalid.
        let forced_start = weights.len() - minimum_rollup.max(2).min(weights.len());
        return Some(forced_start);
    }
    Some(start)
}

fn repack_sources(installed: Vec<PathBuf>) -> Result<Vec<RepackSource>, CheckpointError> {
    let mut unique = BTreeSet::new();
    let mut sources = Vec::new();
    for path in installed {
        if !unique.insert(path.clone()) {
            continue;
        }
        let canonical_id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.strip_prefix("pack-"))
            .filter(|id| !id.is_empty())
            .ok_or_else(|| CheckpointError::InvalidPackPath { path: path.clone() })?
            .to_owned();
        let size = std::fs::metadata(&path)?.len();
        let index_path = path.with_extension("idx");
        let reverse_index_path = path.with_extension("rev");
        let locations =
            crab_git::pack_locator::PackLocationIter::open(&index_path, &reverse_index_path, size)
                .map_err(crab_git::pack::PackError::from)?;
        let content_hash = blake3::Hash::from_hex(&canonical_id)
            .map_err(|_| CheckpointError::InvalidPackPath { path: path.clone() })?;
        let git_sha1 = locations
            .pack_checksum()
            .as_bytes()
            .try_into()
            .map_err(|_| CheckpointError::InvalidPackPath { path: path.clone() })?;
        sources.push(RepackSource {
            canonical_id,
            path,
            index_path,
            reverse_index_path,
            size,
            object_count: locations.object_count(),
            verified_identity: Some(VerifiedPackIdentity {
                git_sha1,
                content_hash: *content_hash.as_bytes(),
            }),
        });
    }
    Ok(sources)
}

fn materialize_packs(
    generated: &[GeometricRepackedPack],
    git_dir: &Path,
    maximum: u64,
    cancel: &CancellationToken,
) -> Result<Vec<CapsuleGitPack>, CheckpointError> {
    let mut total = 0_u64;
    let mut packs = Vec::with_capacity(generated.len());
    for generated in generated {
        check_cancelled(cancel)?;
        let locations = crab_git::pack_locator::PackLocationIter::open(
            generated.index_path(),
            generated.reverse_index_path(),
            generated.pack_size,
        )
        .map_err(crab_git::pack::PackError::from)?;
        let locations = locations
            .collect::<Result<Vec<_>, _>>()
            .map_err(crab_git::pack::PackError::from)?;
        let object_ids = locations
            .iter()
            .map(|location| location.oid)
            .collect::<Vec<_>>();
        let kinds = crab_git::pack::object_kinds_from_git_dir(git_dir, &object_ids)?;
        let ordered_kinds = object_ids
            .iter()
            .map(|oid| {
                kinds
                    .get(oid)
                    .copied()
                    .ok_or_else(|| crab_git::pack::PackError::ObjectKindQuery {
                        path: git_dir.to_owned(),
                        detail: format!("Git omitted object {oid} from the kind query"),
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let checksum =
            gix_hash::ObjectId::from_hex(generated.git_sha1.as_bytes()).map_err(|error| {
                crab_git::pack::PackError::ObjectKindQuery {
                    path: generated.pack_path().to_owned(),
                    detail: format!("generated pack checksum is invalid: {error}"),
                }
            })?;
        let locator_entries = ordered_kinds
            .iter()
            .zip(&locations)
            .map(|(kind, location)| {
                (
                    *kind,
                    generated.external_delta_bases().get(&location.oid).copied(),
                )
            })
            .collect::<Vec<_>>();
        let locator = crab_git::pack_locator::encode_pack_kind_metadata_with_external_deltas(
            checksum,
            &locator_entries,
        )
        .map_err(crab_git::pack::PackError::from)?;
        let pack = std::fs::read(generated.pack_path())?;
        let index = std::fs::read(generated.index_path())?;
        let reverse = std::fs::read(generated.reverse_index_path())?;
        for length in [pack.len(), index.len(), reverse.len(), locator.len()] {
            total = total
                .checked_add(u64::try_from(length).unwrap_or(u64::MAX))
                .ok_or(CheckpointError::OutputLimit { maximum })?;
            if maximum > 0 && total > maximum {
                return Err(CheckpointError::OutputLimit { maximum });
            }
        }
        let external_delta_bases = generated
            .external_delta_bases()
            .values()
            .map(ToString::to_string)
            .collect::<BTreeSet<_>>();
        packs.push(CapsuleGitPack::new_with_external_delta_bases(
            Bytes::from(pack),
            Bytes::from(index),
            Bytes::from(reverse),
            Bytes::from(locator),
            generated.git_sha1.clone(),
            generated.object_count,
            external_delta_bases.into_iter().collect(),
        )?);
    }
    Ok(packs)
}

fn initialize_bare_repository(path: &Path) -> Result<(), CheckpointError> {
    let mut command = Command::new("git");
    command.args(["init", "--bare", "--quiet"]).arg(path);
    for variable in [
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_OBJECT_DIRECTORY",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
    ] {
        command.env_remove(variable);
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    let output = command.output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(CheckpointError::GitInit {
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

fn check_cancelled(cancel: &CancellationToken) -> Result<(), CheckpointError> {
    if cancel.is_cancelled() {
        Err(CheckpointError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_work_combines_independently_of_publication_and_rejects_overflow() {
        let logical = CheckpointOutcome {
            published: true,
            pack_bytes_read: 100,
            pack_bytes_written: 60,
        };
        let physical = CheckpointOutcome {
            published: false,
            pack_bytes_read: 80,
            pack_bytes_written: 40,
        };
        assert_eq!(
            logical.combine(physical).unwrap(),
            CheckpointOutcome {
                published: true,
                pack_bytes_read: 180,
                pack_bytes_written: 100,
            }
        );
        for overflowing in [
            CheckpointOutcome {
                pack_bytes_read: u64::MAX,
                ..Default::default()
            },
            CheckpointOutcome {
                pack_bytes_written: u64::MAX,
                ..Default::default()
            },
        ] {
            assert!(matches!(
                logical.combine(overflowing),
                Err(CheckpointError::AccountingOverflow)
            ));
        }
    }

    fn source(pack_length: usize) -> PackSourceDescriptor {
        let pack = CapsuleGitPack::new(
            Bytes::from(vec![b'p'; pack_length]),
            Bytes::from_static(b"index"),
            Bytes::from_static(b"reverse"),
            Bytes::from_static(b"locator"),
            "0".repeat(40),
            1,
        )
        .expect("pack descriptor");
        PackLayer::build(&pack)
            .expect("layer")
            .source_descriptor()
            .expect("source descriptor")
    }

    fn capsule_run_source(member_count: usize) -> PackSourceDescriptor {
        let mut members = Vec::with_capacity(member_count);
        for index in 0..member_count {
            let offset = u64::try_from(index).unwrap() * 16;
            let pack_bytes = u64::try_from(index).unwrap().to_be_bytes();
            members.push(
                PackMemberDescriptor::new(
                    PackRange::new(offset, &pack_bytes).unwrap(),
                    PackRange::new(offset + 8, b"i").unwrap(),
                    PackRange::new(offset + 9, b"r").unwrap(),
                    PackRange::new(offset + 10, b"l").unwrap(),
                    format!("{index:040x}"),
                    1,
                    Vec::new(),
                )
                .unwrap(),
            );
        }
        PackSourceDescriptor::new(
            PackSourceKind::CapsuleRun,
            "a".repeat(64),
            u64::try_from(member_count).unwrap() * 16 + 8,
            u64::try_from(member_count).unwrap() * 16,
            8,
            "b".repeat(64),
            members,
        )
        .unwrap()
    }

    #[test]
    fn layered_suffix_uses_weighted_geometric_cut() {
        let sources = [900, 700, 9, 9].into_iter().map(source).collect::<Vec<_>>();

        assert_eq!(layered_suffix_start(&sources).expect("selection"), Some(2));
    }

    #[test]
    fn layered_suffix_keeps_already_geometric_inventory() {
        let sources = [900, 9].into_iter().map(source).collect::<Vec<_>>();

        assert_eq!(layered_suffix_start(&sources).expect("selection"), None);
    }

    #[test]
    fn layered_suffix_compacts_member_heavy_run_after_large_stable_source() {
        let sources = vec![source(100_000), capsule_run_source(500)];

        assert_eq!(layered_suffix_start(&sources).expect("selection"), Some(1));
    }

    #[test]
    fn layered_member_suffix_stays_at_target_after_one_pack_replacement() {
        assert_eq!(layered_member_suffix_start(&[1_usize; 8]), None);
        assert_eq!(layered_member_suffix_start(&[1_usize; 9]), Some(7));
    }

    #[test]
    fn layered_suffix_defers_member_rollup_over_byte_budget() {
        let weights = [100_000, LAYERED_SUFFIX_BYTE_BUDGET + 1];
        let member_counts = [1, LAYERED_PACK_MEMBER_TARGET + 1];

        assert_eq!(layered_member_suffix_start(&member_counts), Some(1));
        assert_eq!(
            layered_suffix_start_for_inventory(&weights, &member_counts),
            None
        );
    }

    #[test]
    fn layered_suffix_keeps_geometric_rollup_when_under_budget() {
        let weights = vec![1; LAYERED_MAX_PHYSICAL_SOURCES + 1];

        assert_eq!(layered_suffix_start_for_weights(&weights), Some(1));
    }

    #[test]
    fn layered_suffix_defers_an_over_budget_geometric_rollup() {
        let weights = vec![1, 1, LAYERED_SUFFIX_BYTE_BUDGET];

        assert_eq!(layered_suffix_start_for_weights(&weights), None);
    }

    #[test]
    fn layered_suffix_budget_yields_to_source_bound() {
        let weights = vec![1; LAYERED_MAX_PHYSICAL_SOURCES + 1];
        let mut weights = weights;
        let last = weights.len() - 1;
        weights[last] = LAYERED_SUFFIX_BYTE_BUDGET;

        assert_eq!(
            layered_suffix_start_for_weights(&weights),
            Some(LAYERED_MAX_PHYSICAL_SOURCES - 1)
        );
    }
}
