//! Shared live mount operations used by backend control planes.

use std::path::Path;
use std::sync::Arc;

use crate::core::error::{CrabError, Result};
use crate::hydration::HydrationService;
use crate::pipeline::{GitMetadataOwnership, PipelineConfig, PipelineOutput};
use crate::repository::{fetch_mount_ref, normalized_fetch_ref, refresh_mount_index};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountRuntimeUpdate {
    pub head_oid: String,
    pub head_ref: String,
    pub generation: i64,
}

pub async fn refresh_mount_runtime(
    output: &mut PipelineOutput,
    config: &PipelineConfig,
    mountpoint: &Path,
) -> Result<MountRuntimeUpdate> {
    let engine = Arc::clone(&output.engine);
    let mut prepared = output.clone();
    let mut selected_config = config.clone();
    let mountpoint = mountpoint.to_owned();
    let owner = Arc::clone(&engine);
    let (prepared, selected_config, update) = engine
        .run_control(async move {
            let engine = owner;
            let _reset = begin_mount_update(&engine, &mountpoint).await?;
            selected_config.ref_name = prepared.resolver.capture_view().tracked_ref.clone();
            let selection = selected_config.clone();
            let (head_oid, head_ref) = tokio::task::spawn_blocking(move || {
                resolve_refresh_head(
                    &selection.git_dir,
                    selection.git_metadata,
                    selection.ref_name.as_deref(),
                )
            })
            .await
            .map_err(CrabError::ReadTask)??;
            if prepared.resolver.head().as_ref() == Some(&(head_oid.clone(), head_ref.clone()))
                && prepared.snapshot.current_generation()? == Some(prepared.resolver.generation())
            {
                return Ok((
                    prepared.clone(),
                    selected_config,
                    MountRuntimeUpdate {
                        head_oid,
                        head_ref,
                        generation: prepared.resolver.generation(),
                    },
                ));
            }
            let update =
                publish_runtime_snapshot(&mut prepared, &selected_config, &head_oid, &head_ref)
                    .await?;
            Ok((prepared, selected_config, update))
        })
        .await?;
    *output = prepared;
    let _ = selected_config;
    Ok(update)
}

pub async fn switch_mount_runtime(
    output: &mut PipelineOutput,
    config: &mut PipelineConfig,
    mountpoint: &Path,
    new_ref: &str,
) -> Result<MountRuntimeUpdate> {
    let engine = Arc::clone(&output.engine);
    let mut prepared = output.clone();
    let mut selected_config = config.clone();
    let requested = new_ref.to_owned();
    let mountpoint = mountpoint.to_owned();
    let owner = Arc::clone(&engine);
    let (prepared, selected_config, update) = engine
        .run_control(async move {
            let engine = owner;
            let _reset = begin_mount_update(&engine, &mountpoint).await?;
            let full_ref = normalized_fetch_ref(&requested)?;
            let selection = selected_config.clone();
            let selected_ref = full_ref.clone();
            let (head_oid, _) = tokio::task::spawn_blocking(move || {
                resolve_refresh_head(
                    &selection.git_dir,
                    selection.git_metadata,
                    Some(&selected_ref),
                )
            })
            .await
            .map_err(CrabError::ReadTask)??;
            selected_config.ref_name = Some(full_ref.clone());
            let update =
                publish_runtime_snapshot(&mut prepared, &selected_config, &head_oid, &full_ref)
                    .await?;
            Ok((prepared, selected_config, update))
        })
        .await?;
    *output = prepared;
    *config = selected_config;
    Ok(update)
}

pub(crate) async fn capture_hydration(
    template: &Arc<HydrationService>,
    context: Option<&crate::MountReadContext>,
    git_dir: &Path,
) -> Result<Arc<HydrationService>> {
    let Some(context) = context else {
        return Ok(Arc::clone(template));
    };
    let remote = context.catalog.capture().await?;
    let staging = crate::repository::staging_root_for_git_dir(git_dir)?;
    Ok(template.for_reader(Arc::new(crab_read::PointerReader::new(
        Some(remote),
        Some(staging),
    ))))
}

pub(crate) async fn publish_runtime_snapshot(
    output: &mut PipelineOutput,
    config: &PipelineConfig,
    head_oid: &str,
    head_ref: &str,
) -> Result<MountRuntimeUpdate> {
    // Authenticate the next read view before publishing any visible tree state.
    // Failure keeps the prior resolver and its captured reader together.
    let hydration = capture_hydration(
        &output.hydration,
        output.read_context.as_ref(),
        &config.git_dir,
    )
    .await?;
    let snapshot = Arc::clone(&output.snapshot);
    let overlay = output.overlay.clone();
    let preparation = config.clone();
    let oid = head_oid.to_owned();
    let selected_ref = head_ref.to_owned();
    let (generation, commit_time) = tokio::task::spawn_blocking(move || {
        refresh_mount_index(
            &preparation.git_dir,
            preparation.mount_index_path().as_deref(),
            &oid,
        )?;
        snapshot.publish_generation_from_git(&preparation.git_dir, &oid, &selected_ref)?;
        let generation = snapshot
            .current_generation()?
            .ok_or_else(|| CrabError::Internal("no generation after publish".into()))?;
        if let Some(overlay) = overlay {
            overlay.reconcile(|path| {
                let node = snapshot.get_node(generation, path).ok().flatten()?;
                Some(crate::overlay::ReconcileBaseInfo {
                    is_dir: node.node_type == crate::snapshot::NodeType::Dir,
                    object_oid: node.object_oid,
                })
            })?;
        }
        let commit_time =
            crate::repository::commit_time_from_oid(&preparation.git_dir, &oid).unwrap_or(0);
        Ok::<_, CrabError>((generation, commit_time))
    })
    .await
    .map_err(CrabError::ReadTask)??;
    install_generation(
        output,
        head_oid,
        head_ref,
        generation,
        commit_time,
        hydration,
        config.ref_name.clone(),
    )
}

/// Install a committed Git tree and captured reader before overlay cleanup.
/// The caller retains its overlay-reset guard through adoption and cleanup.
pub async fn adopt_published_snapshot(
    output: &mut PipelineOutput,
    config: &PipelineConfig,
    head_oid: &str,
    head_ref: &str,
) -> Result<MountRuntimeUpdate> {
    let hydration = capture_hydration(
        &output.hydration,
        output.read_context.as_ref(),
        &config.git_dir,
    )
    .await?;
    let mut prepared = output.clone();
    let config = config.clone();
    let oid = head_oid.to_owned();
    let selected_ref = head_ref.to_owned();
    let (installed, update) = tokio::task::spawn_blocking(move || {
        let update =
            install_committed_tree(&mut prepared, &config, &oid, &selected_ref, hydration)?;
        Ok::<_, CrabError>((prepared, update))
    })
    .await
    .map_err(CrabError::ReadTask)??;
    *output = installed;
    Ok(update)
}

fn install_committed_tree(
    output: &mut PipelineOutput,
    config: &PipelineConfig,
    oid: &str,
    head_ref: &str,
    hydration: Arc<HydrationService>,
) -> Result<MountRuntimeUpdate> {
    refresh_mount_index(&config.git_dir, config.mount_index_path().as_deref(), oid)?;
    if output.snapshot.head_oid()?.as_deref() != Some(oid)
        || output.snapshot.ref_name()?.as_deref() != Some(head_ref)
    {
        output
            .snapshot
            .publish_generation_from_git(&config.git_dir, oid, head_ref)?;
    }
    let generation = output
        .snapshot
        .current_generation()?
        .ok_or_else(|| CrabError::Internal("published snapshot has no generation".into()))?;
    let commit_time = crate::repository::commit_time_from_oid(&config.git_dir, oid).unwrap_or(0);
    install_generation(
        output,
        oid,
        head_ref,
        generation,
        commit_time,
        hydration,
        config.ref_name.clone(),
    )
}

pub(crate) async fn begin_mount_update<'a>(
    engine: &'a crate::engine::VfsEngine,
    mountpoint: &Path,
) -> Result<crate::engine::OverlayResetGuard<'a>> {
    flush_mount_writes(mountpoint).await?;
    Ok(engine.begin_overlay_reset().await)
}

pub(crate) async fn inspect_overlay_runtime(
    engine: Arc<crate::engine::VfsEngine>,
    overlay: Arc<crate::overlay::OverlayStore>,
    mountpoint: std::path::PathBuf,
) -> Result<crate::publish::OverlayDiff> {
    let owner = Arc::clone(&engine);
    engine
        .run_control(async move {
            flush_mount_writes(&mountpoint).await?;
            // Inspection blocks mutations without changing epochs or invalidating
            // open readers. The freeze also excludes other-process publishers.
            let _quiesce = owner.quiesce_overlay().await;
            tokio::task::spawn_blocking(move || {
                let _freeze = overlay.freeze_writes()?;
                crate::publish::inspect_overlay_store(&overlay)
            })
            .await
            .map_err(CrabError::ReadTask)?
        })
        .await
}

/// Flush client writeback before taking the engine mutation gate.
///
/// Linux waits for syncfs; macOS retains its global sync request. A missing
/// synthetic Git file identifies an inactive helper with no client to flush.
pub async fn flush_mount_writes(mountpoint: &Path) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let gitfile = mountpoint.join(".git");
        tokio::task::spawn_blocking(move || -> Result<()> {
            use std::os::fd::AsRawFd;
            // Helpers may serve controls before client mount or after unmount.
            // The synthetic Git file identifies the client without opendir's
            // eager enumeration/prefetch of a potentially large root directory.
            let file = match std::fs::File::open(gitfile) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error.into()),
            };
            let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::uninit();
            // SAFETY: file is live and filesystem points to writable statfs storage.
            if unsafe { libc::fstatfs(file.as_raw_fd(), filesystem.as_mut_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            // SAFETY: successful fstatfs initialized the complete output structure.
            let filesystem = unsafe { filesystem.assume_init() };
            if filesystem.f_type == libc::FUSE_SUPER_MAGIC
                || filesystem.f_type == libc::NFS_SUPER_MAGIC
            {
                // Flush mapped/client writeback before the engine gate blocks
                // those same WRITE callbacks. Errors preserve the dirty overlay.
                // SAFETY: file is a live descriptor on the mounted filesystem.
                if unsafe { libc::syncfs(file.as_raw_fd()) } != 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
            Ok(())
        })
        .await
        .map_err(CrabError::ReadTask)??;
    }
    #[cfg(target_os = "macos")]
    {
        let gitfile = mountpoint.join(".git");
        tokio::task::spawn_blocking(move || -> Result<()> {
            let _client = match std::fs::File::open(gitfile) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error.into()),
            };
            // Preserve the CLI's writeback request in the live owner. A waiting
            // per-mount macOS barrier still needs native filesystem qualification.
            // SAFETY: sync takes no arguments and requests filesystem writeback.
            unsafe { libc::sync() };
            Ok(())
        })
        .await
        .map_err(CrabError::ReadTask)??;
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = mountpoint;
    Ok(())
}

pub(crate) async fn commit_mount_runtime(
    output: &mut PipelineOutput,
    config: &PipelineConfig,
    _reset: &crate::engine::OverlayResetGuard<'_>,
    message: String,
    push: bool,
) -> Result<crate::publish::OverlayCommitResult> {
    let mut selected = config.clone();
    selected.ref_name = output.resolver.capture_view().tracked_ref.clone();
    let head_ref = output
        .resolver
        .head()
        .ok_or_else(|| CrabError::Internal("live mount has no HEAD".into()))?
        .1;
    let options = crate::publish::OverlayCommitOptions {
        cache_dir: config.cache_dir.clone(),
        git_dir: config.git_dir.clone(),
        ref_name: head_ref,
        message,
        push,
    };
    let mut view = LiveCommitView {
        output: output.clone(),
        config: selected,
        runtime: tokio::runtime::Handle::current(),
    };
    let (result, installed) = tokio::task::spawn_blocking(move || {
        let result = crate::publish::commit_overlay_with_view(&options, &mut view)?;
        Ok::<_, CrabError>((result, view.output))
    })
    .await
    .map_err(CrabError::ReadTask)??;
    *output = installed;
    Ok(result)
}

struct LiveCommitView {
    output: PipelineOutput,
    config: PipelineConfig,
    runtime: tokio::runtime::Handle,
}

impl crate::publish::CommitView for LiveCommitView {
    fn head_oid(&self) -> Result<Option<String>> {
        Ok(self.output.resolver.head().map(|head| head.0))
    }

    fn install(
        &mut self,
        options: &crate::publish::OverlayCommitOptions,
        oid: &str,
        records: &[crate::overlay::OverlayRecord],
    ) -> Result<()> {
        #[cfg(feature = "fuse")]
        let files = self.output.engine.capture_open_commit(records)?;
        #[cfg(not(feature = "fuse"))]
        let _ = records;
        // Runs on the publisher's blocking thread with its freeze lease. No
        // FUSE content lock is acquired while the engine write gate is held.
        let hydration = self.runtime.block_on(capture_hydration(
            &self.output.hydration,
            self.output.read_context.as_ref(),
            &self.config.git_dir,
        ))?;
        install_committed_tree(
            &mut self.output,
            &self.config,
            oid,
            &options.ref_name,
            hydration,
        )?;
        #[cfg(feature = "fuse")]
        self.output.engine.adopt_open_commit(files)?;
        Ok(())
    }
}

fn install_generation(
    output: &mut PipelineOutput,
    head_oid: &str,
    head_ref: &str,
    generation: i64,
    commit_time: i64,
    hydration: Arc<HydrationService>,
    tracked_ref: Option<String>,
) -> Result<MountRuntimeUpdate> {
    output
        .resolver
        .set_generation(generation, commit_time, Some(hydration), tracked_ref)?;
    output.engine.invalidate_read_source_cache();
    output.head_oid = head_oid.to_owned();
    output.head_ref = head_ref.to_owned();
    output.generation = generation;
    Ok(MountRuntimeUpdate {
        head_oid: head_oid.to_owned(),
        head_ref: head_ref.to_owned(),
        generation,
    })
}

fn resolve_refresh_head(
    git_dir: &Path,
    ownership: GitMetadataOwnership,
    tracked_ref: Option<&str>,
) -> Result<(String, String)> {
    match ownership {
        GitMetadataOwnership::Mount => fetch_mount_ref(git_dir, tracked_ref.unwrap_or("HEAD")),
        GitMetadataOwnership::Borrowed => {
            crate::repository::resolve_mount_ref(git_dir, tracked_ref)
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test setup and assertions fail on unexpected errors"
)]
#[path = "mount_runtime_tests.rs"]
mod tests;
