//! Verified Git-pack consolidation for capsule-protocol checkpoints.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use bytes::Bytes;
use crab_git::repack::{GeometricRepackedPack, RepackSource};
use crab_metadata::capsule_protocol::CapsuleGitPack;
use crab_storage::{Store, StoreLayout};
use tokio_util::sync::CancellationToken;

/// A verified replacement Git-pack inventory for one checkpoint.
pub struct ConsolidatedGitPacks {
    packs: Vec<CapsuleGitPack>,
    source_pack_count: usize,
    source_pack_bytes: u64,
}

impl ConsolidatedGitPacks {
    /// Consume the consolidated packs ready for checkpoint construction.
    #[must_use]
    pub fn into_packs(self) -> Vec<CapsuleGitPack> {
        self.packs
    }

    /// Return the number of packs admitted from the pinned repository view.
    #[must_use]
    pub const fn source_pack_count(&self) -> usize {
        self.source_pack_count
    }

    /// Return the total admitted source pack-body bytes.
    #[must_use]
    pub const fn source_pack_bytes(&self) -> u64 {
        self.source_pack_bytes
    }
}

/// Failure while producing a complete replacement Git-pack inventory.
#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
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

/// Consolidate every Git pack visible from one authenticated repository view.
///
/// The replacement is accepted only after Git can resolve every current ref
/// and `fsck --strict` the resulting object database. The caller remains
/// responsible for atomically publishing the checkpoint with the pinned root.
pub async fn consolidate_git_packs(
    view: &crab_read::capsule_protocol::CapsuleRepositoryView,
    max_input_bytes: u64,
    max_output_bytes: u64,
    cancel: &CancellationToken,
) -> Result<ConsolidatedGitPacks, CheckpointError> {
    check_cancelled(cancel)?;
    let workspace = tempfile::tempdir()?;
    let git_dir = workspace.path().join("repository.git");
    let init_path = git_dir.clone();
    tokio::task::spawn_blocking(move || initialize_bare_repository(&init_path)).await??;
    check_cancelled(cancel)?;
    let installed =
        crab_read::capsule_protocol::install_git_packs(view, &git_dir, max_input_bytes).await?;
    check_cancelled(cancel)?;
    let refs = view.refs().values().cloned().collect::<BTreeSet<_>>();
    let worker_cancel = cancel.clone();
    tokio::task::spawn_blocking(move || {
        let _workspace = workspace;
        check_cancelled(&worker_cancel)?;
        let sources = repack_sources(installed)?;
        let source_pack_count = sources.len();
        let source_pack_bytes = sources.iter().try_fold(0_u64, |total, source| {
            total
                .checked_add(source.size)
                .ok_or(CheckpointError::OutputLimit {
                    maximum: max_input_bytes,
                })
        })?;
        let replacement = crab_git::repack::repack_repository_complete(&sources, &refs)?;
        check_cancelled(&worker_cancel)?;
        let packs = materialize_packs(
            replacement.packs(),
            &git_dir,
            max_output_bytes,
            &worker_cancel,
        )?;
        Ok(ConsolidatedGitPacks {
            packs,
            source_pack_count,
            source_pack_bytes,
        })
    })
    .await?
}

/// Compact a capsule repository once its immutable frontier reaches `threshold`.
///
/// The root and every ref position remain pinned through consolidation. A
/// concurrent root replacement is benign: its owner won publication and a
/// later maintenance pass can retry from that newer authority.
pub async fn publish_capsule_checkpoint(
    layout: &StoreLayout<Store>,
    threshold: u32,
    maximum_bytes: u64,
    cancel: &CancellationToken,
) -> Result<bool, CheckpointError> {
    check_cancelled(cancel)?;
    let view = crab_read::capsule_protocol::open_view(
        layout,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: maximum_bytes,
            max_frontier_bytes: maximum_bytes,
        },
    )
    .await?;
    let capsule_count = view
        .capsule_run_pointers()
        .iter()
        .try_fold(0_u32, |total, pointer| {
            total.checked_add(pointer.capsule_count())
        })
        .ok_or_else(|| {
            crab_metadata::error::MetadataError::Internal(
                "capsule checkpoint count overflowed".to_owned(),
            )
        })?;
    if capsule_count < threshold {
        return Ok(false);
    }
    let packs = consolidate_git_packs(&view, maximum_bytes, maximum_bytes, cancel)
        .await?
        .into_packs();
    if packs.is_empty() {
        return Ok(false);
    }
    let visibility = crab_metadata::capsule_protocol::CapsuleVisibilitySnapshot::from_index(
        &view.git_visibility_index()?,
    )?;
    let checkpoint = crab_metadata::capsule_protocol::Checkpoint::build_with_catalogs(
        view.root().root().generation(),
        view.root().digest(),
        packs,
        view.pointer_catalog()?,
        Some(visibility),
    )?;
    check_cancelled(cancel)?;
    let result = if view.visible_ref_transactions().is_empty() {
        crab_write::capsule_protocol::publish_checkpoint(
            layout,
            view.root_snapshot().clone(),
            &checkpoint,
        )
        .await
    } else {
        crab_write::capsule_protocol::publish_ref_checkpoint(
            layout,
            view.root_snapshot().clone(),
            &checkpoint,
            view.refs().clone(),
            view.peeled_refs().clone(),
            view.visible_ref_transactions().clone(),
        )
        .await
    };
    match result {
        Ok(_) => Ok(true),
        Err(crab_write::WriteError::CapsuleRootChanged { .. }) => Ok(false),
        Err(error) => Err(error.into()),
    }
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
        sources.push(RepackSource {
            canonical_id,
            path,
            index_path,
            reverse_index_path,
            size,
            object_count: locations.object_count(),
            verified_identity: None,
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
        let mut locations = crab_git::pack_locator::PackLocationIter::open(
            generated.index_path(),
            generated.reverse_index_path(),
            generated.pack_size,
        )
        .map_err(crab_git::pack::PackError::from)?;
        let object_ids = locations
            .by_ref()
            .map(|location| location.map(|location| location.oid))
            .collect::<Result<Vec<_>, _>>()
            .map_err(crab_git::pack::PackError::from)?;
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
        let locator = crab_git::pack_locator::encode_pack_kind_metadata(checksum, &ordered_kinds)
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
        packs.push(CapsuleGitPack::new(
            Bytes::from(pack),
            Bytes::from(index),
            Bytes::from(reverse),
            Bytes::from(locator),
            generated.git_sha1.clone(),
            generated.object_count,
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
