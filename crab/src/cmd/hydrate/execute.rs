//! Shared execution for command-selected and literal Git pointer inventories.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crab_types::pointer::Pointer;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::{
    HydrateProgress, HydrateSummary, Hydrator, RecoverPhaseStats, refresh_hydrated_index_entries,
    run_cow_phase, run_recover_phase, sibling_cow_candidates,
};
use crate::core::error::{CrabError, Result, check_cancelled};

pub(super) async fn hydrate_selected_in(
    root: &Path,
    selected_to_hydrate: &[(PathBuf, Pointer)],
    hydrator: &dyn Hydrator,
    recover_from: Option<&Path>,
    progress: Option<&Arc<HydrateProgress>>,
    cancel: &CancellationToken,
) -> Result<HydrateSummary> {
    check_cancelled(cancel)?;
    let to_hydrate = selected_to_hydrate.to_vec();
    let (to_hydrate, recover_stats) = if let Some(recover_path) = recover_from {
        run_recover_phase(recover_path, to_hydrate, cancel, progress)?
    } else {
        (to_hydrate, RecoverPhaseStats::default())
    };

    // Sibling caches locate candidates only; the CoW owner verifies the
    // original identity before publishing any destination.
    let candidate_root = root.to_path_buf();
    let cow_candidates =
        tokio::task::spawn_blocking(move || sibling_cow_candidates(&candidate_root))
            .await
            .map_err(|error| CrabError::Io(std::io::Error::other(error)))?;
    let (to_hydrate, cow_stats) =
        run_cow_phase(to_hydrate, &cow_candidates, cancel, progress).await?;
    let mut summary = hydrator
        .hydrate_batch(&to_hydrate, cancel, progress)
        .await?;

    summary.hydrated += recover_stats.recovered;
    summary.bytes_written += recover_stats.bytes_recovered;
    summary.recovered = recover_stats.recovered;
    summary.bytes_recovered = recover_stats.bytes_recovered;
    summary.verified_paths.extend(recover_stats.verified_paths);
    summary.hydrated += cow_stats.cloned;
    summary.bytes_written += cow_stats.bytes_cloned;
    summary.cow_cloned = cow_stats.cloned;
    summary.bytes_cow_cloned = cow_stats.bytes_cloned;
    summary.verified_paths.extend(cow_stats.verified_paths);

    // Publish only descriptor-safe proofs captured by successful atomic
    // writes. Sibling worktrees use this cache to locate CoW candidates;
    // they still hash each candidate before publication. Best-effort: an
    // unavailable cache only disables that local optimization.
    if summary.hydrated > 0 {
        let pointers = selected_to_hydrate
            .iter()
            .map(|(path, pointer)| (path.as_path(), pointer))
            .collect::<HashMap<_, _>>();
        let updates = summary
            .verified_paths
            .iter()
            .filter_map(|verified| {
                let pointer = pointers.get(verified.path.as_path())?;
                if pointer.file_hash != verified.file_hash || pointer.size != verified.size {
                    return None;
                }
                let rel = verified.path.strip_prefix(root).ok()?;
                let rel_str = rel.to_str()?.to_owned();
                // Backslash is a filename byte on Unix, not a separator.
                // Lossy conversion or cross-platform rewriting aliases cache rows.
                #[cfg(windows)]
                let rel_str = rel_str.replace('\\', "/");
                crate::cache::hydrated_pointer::entry_for_verified_stat(
                    verified.index_stat,
                    &pointer.serialize(),
                )
                .map(|entry| (rel_str, entry))
            })
            .collect::<Vec<_>>();
        if !updates.is_empty() {
            match crate::cache::hydrated_pointer::cache_path_for_worktree_root(root) {
                Ok(cache_path) => {
                    if let Err(e) =
                        crate::cache::HydratedPointerCache::update_on_disk(&cache_path, updates)
                    {
                        debug!(
                            path = %cache_path.display(),
                            error = %e,
                            "failed to persist hydrated-pointer cache (non-fatal)"
                        );
                    }
                }
                Err(e) => {
                    debug!(
                        root = %root.display(),
                        error = %e,
                        "hydrated-pointer cache unavailable for hydrate"
                    );
                }
            }
        }
        refresh_hydrated_index_entries(root, &summary.verified_paths);
    }

    Ok(summary)
}
