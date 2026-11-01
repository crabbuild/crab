//! Capsule Git pack maintenance from one captured repository root.

use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::storage::{StoreLayout, store::Store};

/// Outcome of a repack operation.
#[derive(Debug, Clone)]
pub struct RepackOutcome {
    /// Number of packs before repack.
    pub packs_before: usize,
    /// Number of packs after repack.
    pub packs_after: usize,
    /// Total bytes across all packs before repack.
    pub bytes_before: u64,
    /// Total bytes across all packs after repack.
    pub bytes_after: u64,
    /// Selected pack body bytes processed by this bounded roll-up.
    pub bytes_read: u64,
    /// Replacement pack body bytes submitted to immutable publication.
    pub bytes_written: u64,
    /// Wall-clock time for the operation.
    pub elapsed: Duration,
}

impl RepackOutcome {
    /// Convert to the structured output summary payload.
    pub fn to_summary(&self) -> RepackSummary {
        RepackSummary {
            packs_before: self.packs_before as u64,
            packs_after: self.packs_after as u64,
            bytes_before: self.bytes_before,
            bytes_after: self.bytes_after,
            bytes_read: self.bytes_read,
            bytes_written: self.bytes_written,
            elapsed_ms: self.elapsed.as_millis() as u64,
        }
    }
}

/// Terminal result payload for structured output.
#[derive(Debug, Serialize, JsonSchema)]
pub struct RepackSummary {
    /// Number of packs before repack.
    pub packs_before: u64,
    /// Number of packs after repack.
    pub packs_after: u64,
    /// Total bytes across all packs before repack.
    pub bytes_before: u64,
    /// Total bytes across all packs after repack.
    pub bytes_after: u64,
    /// Selected pack body bytes processed, excluding sidecars and transport overhead.
    #[serde(default)]
    pub bytes_read: u64,
    /// Replacement body bytes submitted, including verified identical-object reuse.
    #[serde(default)]
    pub bytes_written: u64,
    /// Wall-clock duration in milliseconds.
    pub elapsed_ms: u64,
}
/// Checkpoint a repository using an already authenticated protocol-v2 root.
pub async fn run_repack_from_root(
    store: &Store,
    prefix: &str,
    root: crab_metadata::capsule_protocol::RootSnapshot,
    dry_run: bool,
    cancel: &CancellationToken,
) -> Result<RepackOutcome> {
    const MAX_CHECKPOINT_BYTES: u64 = 8 * 1024 * 1024 * 1024;

    let started = Instant::now();
    check_cancelled(cancel)?;
    let router = StoreLayout::new(store.clone(), prefix.to_owned());
    let layout = crab_storage::StoreLayout::with_global_prefix(
        store.as_storage().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    // Preview needs authenticated pack controls, not complete per-ref membership
    // sets or the apply view's retained Git bodies. Keep shared admission paged.
    let view = if dry_run {
        crab_read::capsule_protocol::open_git_view_from_root(
            &layout,
            root,
            crab_read::capsule_protocol::CapsuleReadLimits {
                max_capsule_bytes: MAX_CHECKPOINT_BYTES,
                max_frontier_bytes: MAX_CHECKPOINT_BYTES,
            },
        )
        .await?
    } else {
        open_repack_view(&layout, root, MAX_CHECKPOINT_BYTES).await?
    };
    if view.refs().is_empty() {
        return Err(CrabError::Protocol(
            "cannot checkpoint an unborn repository".to_owned(),
        ));
    }
    let packs_before = view.git_pack_count();
    let bytes_before = view.git_pack_bytes()?;
    let (work, packs_after, bytes_after) = if dry_run {
        (
            crab_remote::checkpoint::CheckpointOutcome::default(),
            packs_before,
            bytes_before,
        )
    } else {
        check_cancelled(cancel)?;
        let repack = crab_remote::checkpoint::repack_capsule_repository_from_view(
            &layout,
            &view,
            MAX_CHECKPOINT_BYTES,
            cancel,
        )
        .await?;
        // Report this pass's exact publication, not a later ref capture that
        // could attribute another writer's packs to our repack.
        (repack.work, repack.packs_after, repack.bytes_after)
    };
    Ok(RepackOutcome {
        packs_before,
        packs_after,
        bytes_before,
        bytes_after,
        bytes_read: work.pack_bytes_read,
        bytes_written: work.pack_bytes_written,
        elapsed: started.elapsed(),
    })
}

async fn open_repack_view(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    root: crab_metadata::capsule_protocol::RootSnapshot,
    maximum_bytes: u64,
) -> crab_read::Result<crab_read::capsule_protocol::CapsuleRepositoryView> {
    let limits = crab_read::capsule_protocol::CapsuleReadLimits {
        max_capsule_bytes: maximum_bytes,
        max_frontier_bytes: maximum_bytes,
    };
    crab_read::capsule_protocol::open_view_from_root_for_repack(layout, root, limits).await
}

#[cfg(test)]
#[path = "repack_capsule_tests.rs"]
mod capsule_tests;
