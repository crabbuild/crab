//! Capsule-protocol history inspection and recovery.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use crab_metadata::capsule_protocol::{HistorySegment, LayeredCheckpoint, RootSnapshot};
use crab_storage::StoreLayout as CapsuleStoreLayout;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::{
    HISTORY_LIST_SCHEMA, HISTORY_SCHEMA_VERSION, HistoryCmd, HistoryEntryPayload,
    HistoryListPayload, HistoryPruneArgs, HistoryPrunePayload, HistoryRestoreArgs,
    HistoryRestorePayload, HistoryVerificationPayload, emit_prune, emit_restore, emit_verification,
    record_prune_audit,
};
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::{OutputMode, emit_json};
use crate::storage::StoreLayout;
use crate::storage::store::Store;

const UNRECORDED_TIME: &str = "not-recorded";
const HISTORY_FENCE_TTL: Duration = Duration::from_hours(1);
const RESTORE_CHECKPOINT_BYTES: u64 = 8 * 1024 * 1024 * 1024;

struct VerifiedCapsuleHistory {
    segment: HistorySegment,
    checkpoint: LayeredCheckpoint,
    verification: HistoryVerificationPayload,
    _workspace: tempfile::TempDir,
}

pub(super) async fn run(
    command: &HistoryCmd,
    store: &Store,
    router: &StoreLayout,
    root: RootSnapshot,
    cancel: &CancellationToken,
) -> Result<()> {
    let layout = CapsuleStoreLayout::with_global_prefix(
        store.as_storage().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    match command {
        HistoryCmd::List(_) => run_list(&layout, &root, command.output_mode()).await,
        HistoryCmd::Verify(args) => {
            let verified = verify_history(
                &layout,
                &root,
                args.generation,
                args.digest.as_deref(),
                cancel,
            )
            .await?;
            emit_verification(&verified.verification, command.output_mode())
        }
        HistoryCmd::Prune(args) => {
            let payload = if args.apply {
                prune_history(store, router, &layout, args, cancel).await?
            } else {
                prune_preview(&layout, &root, args).await?
            };
            if payload.applied
                && let Err(error) = record_prune_audit(router.repo_prefix(), &payload)
            {
                warn!(%error, "failed to append protocol-v2 historical prune audit event");
            }
            emit_prune(&payload, command.output_mode())
        }
        HistoryCmd::Restore(args) => {
            let payload = if args.apply {
                restore_history(store, router, &layout, args, cancel).await?
            } else {
                restore_preview(&layout, &root, args, cancel).await?
            };
            emit_restore(&payload, command.output_mode())
        }
    }
}

async fn history_chain(
    layout: &CapsuleStoreLayout<crab_storage::Store>,
    root: &RootSnapshot,
) -> Result<Vec<HistorySegment>> {
    let Some(history) = root.record().root().history() else {
        return Ok(Vec::new());
    };
    crab_metadata::capsule_protocol::load_history_chain(
        layout,
        history,
        crab_metadata::capsule_protocol::MAX_HISTORY_CHAIN_SEGMENTS,
        crab_metadata::capsule_protocol::MAX_HISTORY_CHAIN_BYTES,
    )
    .await
    .map_err(Into::into)
}

async fn run_list(
    layout: &CapsuleStoreLayout<crab_storage::Store>,
    root: &RootSnapshot,
    mode: OutputMode,
) -> Result<()> {
    let mut entries = history_chain(layout, root)
        .await?
        .iter()
        .map(history_entry_payload)
        .collect::<Vec<_>>();
    entries.sort_unstable_by(|left, right| {
        left.generation
            .cmp(&right.generation)
            .then_with(|| left.digest.cmp(&right.digest))
    });
    let payload = HistoryListPayload {
        current_generation: root.record().root().generation(),
        entries,
    };
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(HISTORY_LIST_SCHEMA, HISTORY_SCHEMA_VERSION, &payload)?;
        }
        OutputMode::Text => {
            println!(
                "current generation: {}; historical roots: {}",
                payload.current_generation,
                payload.entries.len()
            );
            for entry in payload.entries {
                println!(
                    "{} {} refs={} bytes={}",
                    entry.generation, entry.digest, entry.refs, entry.manifest_bytes
                );
            }
        }
    }
    Ok(())
}

fn history_entry_payload(segment: &HistorySegment) -> HistoryEntryPayload {
    HistoryEntryPayload {
        generation: segment.checkpoint().covered_generation(),
        digest: segment.hash().to_owned(),
        created_at: UNRECORDED_TIME.to_owned(),
        session_id: segment.checkpoint().covered_root_digest().to_owned(),
        refs: u64::try_from(segment.refs().len()).unwrap_or(u64::MAX),
        manifest_bytes: u64::try_from(segment.bytes().len()).unwrap_or(u64::MAX),
    }
}

async fn select_history(
    layout: &CapsuleStoreLayout<crab_storage::Store>,
    root: &RootSnapshot,
    generation: u64,
    digest: Option<&str>,
) -> Result<HistorySegment> {
    if let Some(digest) = digest
        && (digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
    {
        return Err(CrabError::Protocol(
            "history digest must be 64 lowercase hexadecimal characters".to_owned(),
        ));
    }
    let mut matches = history_chain(layout, root)
        .await?
        .into_iter()
        .filter(|segment| {
            segment.checkpoint().covered_generation() == generation
                && digest.is_none_or(|expected| segment.hash() == expected)
        });
    let selected = matches.next().ok_or_else(|| CrabError::CorruptObject {
        path: layout.repo_path("v2/history").to_string(),
        reason: format!("historical checkpoint generation {generation} was not found"),
    })?;
    if matches.next().is_some() {
        return Err(CrabError::CorruptObject {
            path: layout.repo_path("v2/history").to_string(),
            reason: format!(
                "historical checkpoint generation {generation} is ambiguous; select a digest"
            ),
        });
    }
    Ok(selected)
}

async fn verify_history(
    layout: &CapsuleStoreLayout<crab_storage::Store>,
    root: &RootSnapshot,
    generation: u64,
    digest: Option<&str>,
    cancel: &CancellationToken,
) -> Result<VerifiedCapsuleHistory> {
    check_cancelled(cancel)?;
    let segment = select_history(layout, root, generation, digest).await?;
    let checkpoint =
        crab_metadata::capsule_protocol::load_layered_checkpoint(layout, segment.checkpoint())
            .await?;
    let (source_count, source_bytes) =
        verify_history_sources(layout, &segment, &checkpoint, cancel).await?;
    check_cancelled(cancel)?;

    let catalog = checkpoint.pointer_catalog()?;
    validate_visibility(&segment, &checkpoint)?;
    let workspace = verify_git_checkpoint(layout, &segment, &checkpoint).await?;
    let dependencies = crab_read::capsule_protocol::verify_installed_dependencies(
        layout,
        &workspace.path().join("repository.git"),
        segment.refs(),
        &catalog,
        crate::cmd::fsck_store::CAPSULE_GIT_SCAN_LIMITS,
        cancel,
    )
    .await?;
    check_cancelled(cancel)?;
    let shard_count = u64::try_from(catalog.shards().len())
        .map_err(|_| CrabError::Internal("history shard count overflowed".to_owned()))?;
    let xorb_count = u64::try_from(catalog.xorbs().len())
        .map_err(|_| CrabError::Internal("history xorb count overflowed".to_owned()))?;
    let dependency_objects = 2_u64
        .checked_add(source_count)
        .and_then(|total| total.checked_add(shard_count))
        .and_then(|total| total.checked_add(xorb_count))
        .and_then(|total| total.checked_add(dependencies.reachable_lfs_objects))
        .ok_or_else(|| CrabError::Internal("history dependency count overflowed".to_owned()))?;
    let segment_bytes = u64::try_from(segment.bytes().len())
        .map_err(|_| CrabError::Internal("history segment size overflowed".to_owned()))?;
    let checkpoint_bytes = u64::try_from(checkpoint.bytes().len())
        .map_err(|_| CrabError::Internal("history checkpoint size overflowed".to_owned()))?;
    let dependency_bytes = segment_bytes
        .checked_add(checkpoint_bytes)
        .and_then(|total| total.checked_add(source_bytes))
        .and_then(|total| total.checked_add(dependencies.reachable_lfs_bytes))
        .and_then(|total| {
            catalog
                .shards()
                .values()
                .try_fold(total, |sum, shard| sum.checked_add(shard.encoded_size()))
        })
        .and_then(|total| {
            catalog
                .xorbs()
                .values()
                .try_fold(total, |sum, xorb| sum.checked_add(xorb.encoded_size()))
        })
        .ok_or_else(|| CrabError::Internal("history dependency bytes overflowed".to_owned()))?;
    let verification = HistoryVerificationPayload {
        generation: segment.checkpoint().covered_generation(),
        digest: segment.hash().to_owned(),
        refs: u64::try_from(segment.refs().len()).unwrap_or(u64::MAX),
        packs: u64::from(checkpoint.pack_count()?),
        git_objects: checkpoint.object_count()?,
        shards: shard_count,
        xorbs: xorb_count,
        dependency_objects,
        dependency_bytes,
    };
    Ok(VerifiedCapsuleHistory {
        segment,
        checkpoint,
        verification,
        _workspace: workspace,
    })
}

async fn verify_history_sources(
    layout: &CapsuleStoreLayout<crab_storage::Store>,
    segment: &HistorySegment,
    checkpoint: &LayeredCheckpoint,
    cancel: &CancellationToken,
) -> Result<(u64, u64)> {
    let mut runs = segment
        .capsule_runs()
        .iter()
        .map(|run| (run.hash(), run))
        .collect::<BTreeMap<_, _>>();
    for source in checkpoint.sources() {
        check_cancelled(cancel)?;
        let run = if source.kind() == crab_metadata::capsule_protocol::PackSourceKind::CapsuleRun {
            runs.remove(source.object_hash())
        } else {
            None
        };
        crab_read::capsule_protocol::verify_layered_source(
            layout,
            source,
            run,
            RESTORE_CHECKPOINT_BYTES,
        )
        .await?;
    }
    for run in runs.values() {
        check_cancelled(cancel)?;
        if run.size() > RESTORE_CHECKPOINT_BYTES {
            return Err(crab_read::ReadError::CapsuleReadLimit {
                resource: "retained history run",
                maximum: RESTORE_CHECKPOINT_BYTES,
            }
            .into());
        }
        crab_metadata::capsule_protocol::load_capsule_run(layout, run).await?;
    }
    // A retained run can also be a physical pack source. Count its immutable
    // body once while still authenticating both descriptions above.
    let mut sizes = checkpoint
        .sources()
        .iter()
        .map(|source| source.object_size())
        .chain(runs.values().map(|run| run.size()));
    sizes.try_fold((0_u64, 0_u64), |(count, bytes), size| {
        count
            .checked_add(1)
            .zip(bytes.checked_add(size))
            .ok_or_else(|| CrabError::Internal("history source accounting overflowed".to_owned()))
    })
}

fn validate_visibility(segment: &HistorySegment, checkpoint: &LayeredCheckpoint) -> Result<()> {
    let visibility = checkpoint
        .visibility_index(0, &"0".repeat(64), &"0".repeat(64))?
        .ok_or_else(|| CrabError::CorruptObject {
            path: checkpoint.hash().to_owned(),
            reason: "historical checkpoint has no complete Git visibility snapshot".to_owned(),
        })?;
    if visibility.ref_count() != segment.refs().len()
        || segment
            .refs()
            .keys()
            .any(|name| !visibility.contains_ref(name))
    {
        return Err(CrabError::CorruptObject {
            path: checkpoint.hash().to_owned(),
            reason: "historical checkpoint visibility refs do not match retained refs".to_owned(),
        });
    }
    for (name, tip) in segment.refs() {
        if !visibility.contains_hex_in_ref(name, tip)
            || segment
                .peeled_refs()
                .get(name)
                .is_some_and(|peeled| !visibility.contains_hex_in_ref(name, peeled))
        {
            return Err(CrabError::CorruptObject {
                path: checkpoint.hash().to_owned(),
                reason: format!("historical checkpoint visibility omits tip {tip} for {name}"),
            });
        }
    }
    Ok(())
}

async fn verify_git_checkpoint(
    layout: &CapsuleStoreLayout<crab_storage::Store>,
    segment: &HistorySegment,
    checkpoint: &LayeredCheckpoint,
) -> Result<tempfile::TempDir> {
    let workspace = tempfile::tempdir()?;
    let repository = workspace.path().join("repository.git");
    let repository = tokio::task::spawn_blocking(move || {
        super::run_git(
            Command::new("git")
                .args(["init", "--bare", "--quiet"])
                .arg(&repository),
            "initialize capsule history verification repository",
        )?;
        Ok::<_, CrabError>(repository)
    })
    .await
    .map_err(|error| CrabError::Io(std::io::Error::other(error)))??;
    crab_read::capsule_protocol::install_layered_checkpoint(
        checkpoint,
        layout,
        &repository,
        RESTORE_CHECKPOINT_BYTES,
    )
    .await?;
    let refs = segment.refs().clone();
    let peeled_refs = segment.peeled_refs().clone();
    let head = segment.head().to_owned();
    tokio::task::spawn_blocking(move || verify_git_refs(&repository, &refs, &peeled_refs, &head))
        .await
        .map_err(|error| CrabError::Io(std::io::Error::other(error)))??;
    Ok(workspace)
}

fn verify_git_refs(
    repository: &Path,
    refs: &BTreeMap<String, String>,
    peeled_refs: &BTreeMap<String, String>,
    head: &str,
) -> Result<()> {
    for (name, oid) in refs {
        super::run_git(
            Command::new("git")
                .arg(format!("--git-dir={}", repository.display()))
                .arg("update-ref")
                .arg(name)
                .arg(oid),
            "install capsule historical ref",
        )?;
    }
    super::run_git(
        Command::new("git")
            .arg(format!("--git-dir={}", repository.display()))
            .arg("symbolic-ref")
            .arg("HEAD")
            .arg(head),
        "install capsule historical HEAD",
    )?;
    super::run_git(
        Command::new("git")
            .arg(format!("--git-dir={}", repository.display()))
            .args(["fsck", "--strict", "--full", "--no-reflogs"])
            .stdout(std::process::Stdio::null()),
        "verify capsule historical Git connectivity",
    )?;
    for (name, expected) in peeled_refs {
        let output = super::git_command(
            Command::new("git")
                .arg(format!("--git-dir={}", repository.display()))
                .args(["rev-parse", "--verify"])
                .arg(format!("{name}^{{}}")),
        )
        .output()?;
        let actual = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !output.status.success() || actual != *expected {
            return Err(CrabError::CorruptObject {
                path: name.clone(),
                reason: format!("peeled ref resolves to {actual}, expected {expected}"),
            });
        }
    }
    Ok(())
}

async fn prune_preview(
    layout: &CapsuleStoreLayout<crab_storage::Store>,
    root: &RootSnapshot,
    args: &HistoryPruneArgs,
) -> Result<HistoryPrunePayload> {
    let chain = history_chain(layout, root).await?;
    Ok(prune_payload(&chain, args.keep_last, false))
}

async fn prune_history(
    store: &Store,
    router: &StoreLayout,
    layout: &CapsuleStoreLayout<crab_storage::Store>,
    args: &HistoryPruneArgs,
    cancel: &CancellationToken,
) -> Result<HistoryPrunePayload> {
    let lease =
        crate::maintenance::GcSweepLease::acquire(store, router.repo_prefix(), cancel).await?;
    let operation = async {
        check_cancelled(cancel)?;
        let base = crab_write::capsule_protocol::open_root(layout).await?;
        let chain = history_chain(layout, &base).await?;
        let mut payload = prune_payload(&chain, args.keep_last, true);
        if payload.roots_pruned == 0 {
            return Ok(payload);
        }
        // Finish fallible local preparation before acquiring the root fence;
        // all work after acquisition must flow through its release boundary.
        let rebuilt = rebuild_retained_history(&chain, args.keep_last)?;
        let fence_id = blake3::hash(uuid::Uuid::now_v7().as_bytes())
            .to_hex()
            .to_string();
        let expires_at_unix = SystemTime::now()
            .checked_add(HISTORY_FENCE_TTL)
            .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs())
            .ok_or_else(|| {
                CrabError::Internal("history fence expiry cannot be represented".to_owned())
            })?;
        let fenced = crab_write::capsule_protocol::begin_gc(
            layout,
            base,
            crab_metadata::capsule_protocol::GcFence::new(&fence_id, expires_at_unix)?,
        )
        .await?;
        let replacement =
            crab_write::capsule_protocol::replace_history(layout, fenced.clone(), &rebuilt).await;
        let (primary, release_base) = match replacement {
            Ok(root) => (None, root),
            Err(error) => (Some(CrabError::from(error)), fenced),
        };
        let release = crab_write::capsule_protocol::end_gc(layout, release_base, &fence_id).await;
        match (primary, release) {
            (None, Ok(_)) => {
                payload.applied = true;
                Ok(payload)
            }
            (Some(error), _) => Err(error),
            (None, Err(error)) => Err(error.into()),
        }
    }
    .await;
    let release = lease.release().await;
    match (operation, release) {
        (Ok(payload), Ok(())) => Ok(payload),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

fn prune_payload(chain: &[HistorySegment], keep_last: usize, applied: bool) -> HistoryPrunePayload {
    let pruned = chain.iter().skip(keep_last).collect::<Vec<_>>();
    HistoryPrunePayload {
        applied,
        keep_last: u64::try_from(keep_last).unwrap_or(u64::MAX),
        roots_before: u64::try_from(chain.len()).unwrap_or(u64::MAX),
        roots_kept: u64::try_from(chain.len().min(keep_last)).unwrap_or(u64::MAX),
        roots_pruned: u64::try_from(pruned.len()).unwrap_or(u64::MAX),
        manifest_bytes_pruned: pruned.iter().fold(0_u64, |total, segment| {
            total.saturating_add(u64::try_from(segment.bytes().len()).unwrap_or(u64::MAX))
        }),
        pruned: pruned.into_iter().map(history_entry_payload).collect(),
    }
}

fn rebuild_retained_history(
    chain: &[HistorySegment],
    keep_last: usize,
) -> Result<Vec<HistorySegment>> {
    let retained = &chain[..chain.len().min(keep_last)];
    let mut previous = None;
    let mut rebuilt = Vec::with_capacity(retained.len());
    for segment in retained.iter().rev() {
        let state = crab_metadata::capsule_protocol::HistorySegmentState::new(
            segment.refs().clone(),
            segment.peeled_refs().clone(),
            segment.head().to_owned(),
            segment.compacted_ref_transactions().clone(),
            segment.capsule_runs().to_vec(),
        );
        let replacement = HistorySegment::build(segment.checkpoint().clone(), previous, state)?;
        previous = Some(replacement.pointer()?);
        rebuilt.push(replacement);
    }
    rebuilt.reverse();
    Ok(rebuilt)
}

async fn restore_preview(
    layout: &CapsuleStoreLayout<crab_storage::Store>,
    root: &RootSnapshot,
    args: &HistoryRestoreArgs,
    cancel: &CancellationToken,
) -> Result<HistoryRestorePayload> {
    let verified = verify_history(
        layout,
        root,
        args.generation,
        args.digest.as_deref(),
        cancel,
    )
    .await?;
    let current = crab_read::capsule_protocol::open_view_from_root_with_control(
        layout,
        root.clone(),
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 2 * 1024 * 1024 * 1024,
            max_frontier_bytes: 8 * 1024 * 1024 * 1024,
        },
    )
    .await?;
    let (refs_added, refs_updated, refs_deleted) =
        ref_change_counts(current.refs(), verified.segment.refs());
    Ok(HistoryRestorePayload {
        applied: false,
        source_generation: verified.segment.checkpoint().covered_generation(),
        source_digest: verified.segment.hash().to_owned(),
        previous_generation: root.record().root().generation(),
        restored_generation: None,
        refs_added,
        refs_updated,
        refs_deleted,
        acceleration_rebuilt: false,
        verification: verified.verification,
    })
}

async fn restore_history(
    store: &Store,
    router: &StoreLayout,
    layout: &CapsuleStoreLayout<crab_storage::Store>,
    args: &HistoryRestoreArgs,
    cancel: &CancellationToken,
) -> Result<HistoryRestorePayload> {
    let lease =
        crate::maintenance::GcSweepLease::acquire(store, router.repo_prefix(), cancel).await?;
    let operation = async {
        check_cancelled(cancel)?;
        let base = crab_write::capsule_protocol::open_root(layout).await?;
        let verified = verify_history(
            layout,
            &base,
            args.generation,
            args.digest.as_deref(),
            cancel,
        )
        .await?;
        let view = crab_read::capsule_protocol::open_view_from_root(
            layout,
            base,
            crab_read::capsule_protocol::CapsuleReadLimits {
                max_capsule_bytes: RESTORE_CHECKPOINT_BYTES,
                max_frontier_bytes: RESTORE_CHECKPOINT_BYTES,
            },
        )
        .await?;
        let previous_generation = view.root().root().generation();
        let (refs_added, refs_updated, refs_deleted) =
            ref_change_counts(view.refs(), verified.segment.refs());
        let current_catalog = view.pointer_catalog()?;
        // Keep both catalogs: history may use a recipe no longer in the current
        // view, while newer content must remain available after a rollback.
        let mut restored_catalog = verified.checkpoint.pointer_catalog()?;
        restored_catalog.apply(&current_catalog)?;
        let outcome = crab_remote::checkpoint::publish_capsule_checkpoint_from_view(
            layout,
            &view,
            0,
            RESTORE_CHECKPOINT_BYTES,
            cancel,
        )
        .await
        .map_err(map_checkpoint_error)?;
        let checkpointed = crab_write::capsule_protocol::open_root(layout).await?;
        // The shared publisher reports CAS loss as a no-op. Restore cannot
        // fence an unrelated winner using the catalog and refs captured above.
        if !outcome.published
            || checkpointed
                .record()
                .root()
                .checkpoint()
                .is_none_or(|checkpoint| checkpoint.covered_root_digest() != view.root().digest())
            || checkpointed.record().root().refs() != view.refs()
            || checkpointed.record().root().peeled_refs() != view.peeled_refs()
            || checkpointed.record().root().compacted_ref_transactions()
                != view.visible_ref_transactions()
        {
            return Err(crab_write::WriteError::CapsuleRootChanged {
                path: layout.capsule_root_path().to_string(),
            }
            .into());
        }
        check_cancelled(cancel)?;

        let fence_id = blake3::hash(uuid::Uuid::now_v7().as_bytes())
            .to_hex()
            .to_string();
        let ref_epoch = {
            let mut hasher = blake3::Hasher::new_derive_key("crab capsule ref epoch v2");
            hasher.update(checkpointed.record().digest().as_bytes());
            hasher.update(fence_id.as_bytes());
            hasher.finalize().to_hex().to_string()
        };
        let expires_at_unix = SystemTime::now()
            .checked_add(HISTORY_FENCE_TTL)
            .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs())
            .ok_or_else(|| {
                CrabError::Internal("history fence expiry cannot be represented".to_owned())
            })?;
        let fenced = crab_write::capsule_protocol::begin_restore(
            layout,
            checkpointed,
            crab_metadata::capsule_protocol::GcFence::new(&fence_id, expires_at_unix)?,
            ref_epoch,
        )
        .await?;

        let restore = async {
            let checkpoint = match verified.checkpoint.visibility_ordinal_snapshot()? {
                Some(visibility) => LayeredCheckpoint::build_with_ordinal_visibility(
                    fenced.record().root().generation(),
                    fenced.record().digest(),
                    verified.checkpoint.sources().to_vec(),
                    restored_catalog,
                    Some(visibility),
                )?,
                None => LayeredCheckpoint::build(
                    fenced.record().root().generation(),
                    fenced.record().digest(),
                    verified.checkpoint.sources().to_vec(),
                    restored_catalog,
                    verified.checkpoint.visibility_snapshot()?,
                )?,
            };
            check_cancelled(cancel)?;
            crab_write::capsule_protocol::restore_checkpoint(
                layout,
                fenced.clone(),
                &checkpoint,
                verified.segment.refs().clone(),
                verified.segment.peeled_refs().clone(),
                verified.segment.head().to_owned(),
            )
            .await
            .map_err(CrabError::from)
        }
        .await;
        let (primary, release_base) = match restore {
            Ok(root) => (None, root),
            Err(error) => (Some(error), fenced),
        };
        let release = crab_write::capsule_protocol::end_gc(layout, release_base, &fence_id).await;
        match (primary, release) {
            (None, Ok(root)) => Ok(HistoryRestorePayload {
                applied: true,
                source_generation: verified.segment.checkpoint().covered_generation(),
                source_digest: verified.segment.hash().to_owned(),
                previous_generation,
                restored_generation: Some(root.record().root().generation()),
                refs_added,
                refs_updated,
                refs_deleted,
                acceleration_rebuilt: true,
                verification: verified.verification,
            }),
            (Some(error), _) => Err(error),
            (None, Err(error)) => Err(error.into()),
        }
    }
    .await;
    let release = lease.release().await;
    match (operation, release) {
        (Ok(payload), Ok(())) => Ok(payload),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

fn map_checkpoint_error(error: crab_remote::checkpoint::CheckpointError) -> CrabError {
    match error {
        crab_remote::checkpoint::CheckpointError::Cancelled => CrabError::Cancelled,
        crab_remote::checkpoint::CheckpointError::Read(source) => source.into(),
        crab_remote::checkpoint::CheckpointError::Repack(source) => source.into(),
        crab_remote::checkpoint::CheckpointError::Pack(source) => source.into(),
        crab_remote::checkpoint::CheckpointError::Metadata(source) => source.into(),
        crab_remote::checkpoint::CheckpointError::Storage(source) => source.into(),
        crab_remote::checkpoint::CheckpointError::Write(source) => source.into(),
        crab_remote::checkpoint::CheckpointError::Io(source) => source.into(),
        crab_remote::checkpoint::CheckpointError::Worker(source) => {
            CrabError::Io(std::io::Error::other(source))
        }
        other => CrabError::Internal(other.to_string()),
    }
}

fn ref_change_counts(
    current: &BTreeMap<String, String>,
    historical: &BTreeMap<String, String>,
) -> (u64, u64, u64) {
    let added = historical
        .keys()
        .filter(|name| !current.contains_key(*name))
        .count() as u64;
    let updated = historical
        .iter()
        .filter(|(name, oid)| current.get(*name).is_some_and(|value| value != *oid))
        .count() as u64;
    let deleted = current
        .keys()
        .filter(|name| !historical.contains_key(*name))
        .count() as u64;
    (added, updated, deleted)
}

#[cfg(test)]
#[path = "history_recovery_v2/layered_tests.rs"]
mod layered_tests;

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use crab_metadata::capsule_protocol::{CapsulePointer, CheckpointPointer, HistorySegmentState};

    fn segment(
        generation: u64,
        previous: Option<crab_metadata::capsule_protocol::HistorySegmentPointer>,
    ) -> HistorySegment {
        let root_digest = format!("{:064x}", generation + 100);
        let checkpoint = CheckpointPointer::new_layered(
            format!("{:064x}", generation + 200),
            1,
            0,
            1,
            "0".repeat(64),
            generation,
            &root_digest,
            1,
            1,
        )
        .unwrap();
        let run = CapsulePointer::new(
            format!("{:064x}", generation + 300),
            2,
            1,
            1,
            "0".repeat(64),
            0,
            vec![format!("{:064x}", generation + 400)],
            root_digest,
        )
        .unwrap();
        HistorySegment::build(
            checkpoint,
            previous,
            HistorySegmentState::new(
                BTreeMap::from([(
                    "refs/heads/main".to_owned(),
                    format!("{:040x}", generation + 1),
                )]),
                BTreeMap::new(),
                "refs/heads/main".to_owned(),
                BTreeMap::new(),
                vec![run],
            ),
        )
        .unwrap()
    }

    #[test]
    fn retained_history_is_relinked_without_pruned_predecessors() {
        let oldest = segment(1, None);
        let middle = segment(2, Some(oldest.pointer().unwrap()));
        let newest = segment(3, Some(middle.pointer().unwrap()));
        let chain = vec![newest, middle, oldest];

        let rebuilt = rebuild_retained_history(&chain, 2).unwrap();

        assert_eq!(rebuilt.len(), 2);
        assert_eq!(rebuilt[0].checkpoint(), chain[0].checkpoint());
        assert_eq!(rebuilt[1].checkpoint(), chain[1].checkpoint());
        assert_eq!(rebuilt[0].previous().unwrap().hash(), rebuilt[1].hash());
        assert!(rebuilt[1].previous().is_none());
        assert_ne!(rebuilt[0].hash(), chain[0].hash());
    }

    #[test]
    fn prune_payload_reports_only_removed_segments() {
        let oldest = segment(1, None);
        let middle = segment(2, Some(oldest.pointer().unwrap()));
        let newest = segment(3, Some(middle.pointer().unwrap()));

        let payload = prune_payload(&[newest, middle, oldest], 2, false);

        assert!(!payload.applied);
        assert_eq!(payload.roots_before, 3);
        assert_eq!(payload.roots_kept, 2);
        assert_eq!(payload.roots_pruned, 1);
        assert_eq!(payload.pruned[0].generation, 1);
    }
}
