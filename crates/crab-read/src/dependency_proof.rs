//! Generation-bound content verification for dependencies found by Git validation.

use std::{collections::BTreeMap, time::Duration};

use crab_git::{pointer_detect::PointerKind, receive_plan::PointerDependency};
use crab_metadata::{
    file_index_lookup::{FileIndexLookupLimits, FileIndexLookupSession},
    manifest_store::RepositorySnapshot,
};
use crab_storage::{Store, StoreLayout};
use crab_xet::xorb::format::MerkleHash;
use gix_hash::ObjectId;
use tokio_util::sync::CancellationToken;

use crate::pointer_proof::{PointerProofError, PointerProofLimits, verify_crab_pointer};

/// Bounds for a complete dependency batch, including selection and admission waits.
#[derive(Clone, Copy)]
pub struct DependencyProofLimits {
    pub max_dependencies: usize,
    pub max_total_file_bytes: u64,
    pub max_duration: Duration,
    pub lookup: FileIndexLookupLimits,
    pub content: PointerProofLimits,
}

/// Dependency failures do not publish refs, create receipts or modify content.
#[derive(Debug, thiserror::Error)]
pub enum DependencyProofError {
    #[error("dependency verification exceeds {0}")]
    Limit(&'static str),
    #[error("invalid dependency in Git blob {blob}: {reason}")]
    Invalid {
        blob: ObjectId,
        reason: &'static str,
    },
    #[error("dependency shard selection failed")]
    Metadata(#[from] crab_metadata::error::MetadataError),
    #[error("Crab dependency in Git blob {blob} failed content verification")]
    Crab {
        blob: ObjectId,
        #[source]
        source: PointerProofError,
    },
    #[error("LFS dependency in Git blob {blob} failed content verification")]
    Lfs {
        blob: ObjectId,
        #[source]
        source: crab_lfs::LfsError,
    },
    #[error("dependency verification cancelled")]
    Cancelled,
    #[error("dependency verification deadline exceeded")]
    Deadline,
}

type Result<T> = std::result::Result<T, DependencyProofError>;

/// Verify all inspected Git pointer dependencies against one captured repository.
///
/// Supply an origin-only layout and its `read_repository_snapshot` result from
/// the same base as graph validation. Crab hints cannot widen that inventory.
/// LFS receipts and replica fallback are not used. Duplicate content is read
/// once; conflicting sizes are rejected before reads. Extensions stay client-side:
/// LFS's primary OID and size describe the stored, transformed bytes.
///
/// Successful body traffic is bounded by lookup limits plus at most
/// `max_dependencies * content.max_read_bytes`, excluding transport retries.
/// One content proof runs at a time; stage admission also bounds concurrent
/// batches. Timeout cancels child proofs, whose CPU jobs retain their permits.
/// Success is content evidence only: the publisher must hold GC fences and
/// recheck the exact base before changing refs. No durable evidence is written.
pub async fn verify_dependencies(
    layout: &StoreLayout<Store>,
    snapshot: &RepositorySnapshot,
    dependencies: &[PointerDependency],
    limits: DependencyProofLimits,
    cancellation: &CancellationToken,
) -> Result<()> {
    let cancellation = cancellation.child_token();
    let _guard = cancellation.clone().drop_guard();
    let proof = async {
        let unique = normalize(dependencies, limits)?;
        let hashes: Vec<_> = unique
            .values()
            .filter_map(|dependency| match &dependency.pointer {
                PointerKind::Crab(pointer) => Some(MerkleHash::from(pointer.file_hash)),
                _ => None,
            })
            .collect();
        let shards = if hashes.is_empty() {
            Vec::new()
        } else {
            FileIndexLookupSession::for_snapshot(layout, snapshot, limits.lookup)?
                .lookup_batch(&hashes)
                .await?
        };
        let mut shards = shards.into_iter();
        let lfs = crab_lfs::LfsObjectStore::new(layout.store().clone(), layout.repo_prefix());
        for dependency in unique.values() {
            if cancellation.is_cancelled() {
                return Err(DependencyProofError::Cancelled);
            }
            let blob = dependency.blob;
            match &dependency.pointer {
                PointerKind::Crab(pointer) => {
                    let shard = shards
                        .next()
                        .flatten()
                        .ok_or(DependencyProofError::Invalid {
                            blob,
                            reason: "content is absent from the captured shard inventory",
                        })?;
                    verify_crab_pointer(layout, pointer, shard, limits.content, &cancellation)
                        .await
                        .map_err(|source| DependencyProofError::Crab { blob, source })?;
                }
                PointerKind::Lfs(pointer) => lfs
                    .verify_origin(&pointer.oid, pointer.size)
                    .await
                    .map_err(|source| DependencyProofError::Lfs { blob, source })?,
                PointerKind::NotAPointer => {
                    return Err(DependencyProofError::Invalid {
                        blob,
                        reason: "not a recognized pointer",
                    });
                }
            }
        }
        Ok(())
    };
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(DependencyProofError::Cancelled),
        result = tokio::time::timeout(limits.max_duration, proof) => {
            result.map_err(|_| DependencyProofError::Deadline)?
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ContentId {
    Crab([u8; 32]),
    Lfs([u8; 32]),
}

fn normalize(
    dependencies: &[PointerDependency],
    limits: DependencyProofLimits,
) -> Result<BTreeMap<ContentId, &PointerDependency>> {
    if dependencies.len() > limits.max_dependencies {
        return Err(DependencyProofError::Limit("pointer count"));
    }
    let mut unique: BTreeMap<_, &PointerDependency> = BTreeMap::new();
    let mut total = 0u64;
    for dependency in dependencies {
        let (id, size) = identity(dependency)?;
        if size > limits.content.max_file_bytes
            || (matches!(id, ContentId::Lfs(_)) && size > limits.content.max_read_bytes)
        {
            return Err(DependencyProofError::Limit("file bytes"));
        }
        if let Some(previous) = unique.get(&id) {
            if identity(previous)?.1 != size {
                return Err(DependencyProofError::Invalid {
                    blob: dependency.blob,
                    reason: "conflicting sizes for the same content identity",
                });
            }
            continue;
        }
        total = total
            .checked_add(size)
            .filter(|total| *total <= limits.max_total_file_bytes)
            .ok_or(DependencyProofError::Limit("total file bytes"))?;
        unique.insert(id, dependency);
    }
    Ok(unique)
}

fn identity(dependency: &PointerDependency) -> Result<(ContentId, u64)> {
    match &dependency.pointer {
        PointerKind::Crab(pointer) => Ok((ContentId::Crab(pointer.file_hash), pointer.size)),
        PointerKind::Lfs(pointer) => Ok((ContentId::Lfs(pointer.oid), pointer.size)),
        PointerKind::NotAPointer => Err(DependencyProofError::Invalid {
            blob: dependency.blob,
            reason: "not a recognized pointer",
        }),
    }
}

#[cfg(test)]
mod tests;
