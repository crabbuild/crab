//! Capsule-protocol history inspection and recovery.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime};

use crab_metadata::capsule_protocol::{CapsuleGitPack, Checkpoint, HistorySegment, RootSnapshot};
use crab_storage::StoreLayout as CapsuleStoreLayout;
use futures_util::future::try_join_all;
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
    checkpoint: Checkpoint,
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
    let checkpoint = crab_metadata::capsule_protocol::load_checkpoint(layout, segment.checkpoint());
    let runs = try_join_all(
        segment
            .capsule_runs()
            .iter()
            .map(|pointer| crab_metadata::capsule_protocol::load_capsule_run(layout, pointer)),
    );
    let (checkpoint, runs) = tokio::try_join!(checkpoint, runs)?;
    check_cancelled(cancel)?;

    let catalog = checkpoint.pointer_catalog()?;
    crab_read::verify_capsule_pointer_catalog_objects(layout, &catalog).await?;
    check_cancelled(cancel)?;
    validate_visibility(&segment, &checkpoint)?;
    let git_packs = checkpoint_git_packs(&checkpoint)?;
    let workspace = verify_git_checkpoint(&segment, &git_packs).await?;

    let git_objects = checkpoint
        .git_packs()
        .iter()
        .try_fold(0_u64, |total, pack| total.checked_add(pack.object_count()))
        .ok_or_else(|| CrabError::Internal("history Git object count overflowed".to_owned()))?;
    let run_count = u64::try_from(runs.len())
        .map_err(|_| CrabError::Internal("history run count overflowed".to_owned()))?;
    let shard_count = u64::try_from(catalog.shards().len())
        .map_err(|_| CrabError::Internal("history shard count overflowed".to_owned()))?;
    let xorb_count = u64::try_from(catalog.xorbs().len())
        .map_err(|_| CrabError::Internal("history xorb count overflowed".to_owned()))?;
    let dependency_objects = 2_u64
        .checked_add(run_count)
        .and_then(|total| total.checked_add(shard_count))
        .and_then(|total| total.checked_add(xorb_count))
        .ok_or_else(|| CrabError::Internal("history dependency count overflowed".to_owned()))?;
    let segment_bytes = u64::try_from(segment.bytes().len())
        .map_err(|_| CrabError::Internal("history segment size overflowed".to_owned()))?;
    let checkpoint_bytes = u64::try_from(checkpoint.bytes().len())
        .map_err(|_| CrabError::Internal("history checkpoint size overflowed".to_owned()))?;
    let dependency_bytes = segment_bytes
        .checked_add(checkpoint_bytes)
        .and_then(|total| {
            segment
                .capsule_runs()
                .iter()
                .try_fold(total, |sum, run| sum.checked_add(run.size()))
        })
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
        packs: u64::try_from(checkpoint.git_packs().len()).unwrap_or(u64::MAX),
        git_objects,
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

fn validate_visibility(segment: &HistorySegment, checkpoint: &Checkpoint) -> Result<()> {
    let visibility = checkpoint
        .visibility_snapshot()?
        .ok_or_else(|| CrabError::CorruptObject {
            path: checkpoint.hash().to_owned(),
            reason: "historical checkpoint has no complete Git visibility snapshot".to_owned(),
        })?;
    if visibility.refs().keys().collect::<BTreeSet<_>>()
        != segment.refs().keys().collect::<BTreeSet<_>>()
    {
        return Err(CrabError::CorruptObject {
            path: checkpoint.hash().to_owned(),
            reason: "historical checkpoint visibility refs do not match retained refs".to_owned(),
        });
    }
    for (name, tip) in segment.refs() {
        if visibility
            .refs()
            .get(name)
            .is_none_or(|objects| objects.binary_search(tip).is_err())
        {
            return Err(CrabError::CorruptObject {
                path: checkpoint.hash().to_owned(),
                reason: format!("historical checkpoint visibility omits tip {tip} for {name}"),
            });
        }
    }
    Ok(())
}

fn checkpoint_git_packs(checkpoint: &Checkpoint) -> Result<Vec<CapsuleGitPack>> {
    checkpoint
        .git_packs()
        .iter()
        .map(|descriptor| {
            CapsuleGitPack::new(
                checkpoint.section_bytes(descriptor.pack_section())?,
                checkpoint.section_bytes(descriptor.index_section())?,
                checkpoint.section_bytes(descriptor.reverse_index_section())?,
                checkpoint.section_bytes(descriptor.locator_section())?,
                descriptor.git_checksum(),
                descriptor.object_count(),
            )
            .map_err(Into::into)
        })
        .collect()
}

async fn verify_git_checkpoint(
    segment: &HistorySegment,
    packs: &[CapsuleGitPack],
) -> Result<tempfile::TempDir> {
    let workspace = tempfile::tempdir()?;
    let repository = workspace.path().join("repository.git");
    let pack_count = u32::try_from(packs.len())
        .map_err(|_| CrabError::Internal("history pack count overflowed".to_owned()))?;
    let inputs = segment
        .checkpoint()
        .pack_count()
        .eq(&pack_count)
        .then(|| packs.to_vec())
        .ok_or_else(|| CrabError::CorruptObject {
            path: segment.checkpoint().hash().to_owned(),
            reason: "historical checkpoint pack count changed after decoding".to_owned(),
        })?;
    let refs = segment.refs().clone();
    let peeled_refs = segment.peeled_refs().clone();
    let head = segment.head().to_owned();
    let verification_root = workspace.path().to_owned();
    tokio::task::spawn_blocking(move || {
        prepare_git_repository(
            &verification_root,
            &repository,
            &refs,
            &peeled_refs,
            &head,
            &inputs,
        )
    })
    .await
    .map_err(|error| {
        CrabError::Internal(format!("history verification worker failed: {error}"))
    })??;
    Ok(workspace)
}

fn prepare_git_repository(
    workspace: &Path,
    repository: &Path,
    refs: &BTreeMap<String, String>,
    peeled_refs: &BTreeMap<String, String>,
    head: &str,
    packs: &[CapsuleGitPack],
) -> Result<()> {
    super::run_git(
        Command::new("git")
            .arg("init")
            .arg("--bare")
            .arg("--quiet")
            .arg(repository),
        "initialize capsule history verification repository",
    )?;
    let sources = workspace.join("packs");
    std::fs::create_dir_all(&sources)?;
    for (index, pack) in packs.iter().enumerate() {
        let pack_path = sources.join(format!("{index}.pack"));
        std::fs::write(&pack_path, pack.pack_bytes())?;
        let identity = blake3::hash(pack.pack_bytes()).to_hex().to_string();
        let installed = crab_git::install_pack_file_from_path(
            &repository.join("objects/pack"),
            &pack_path,
            &identity,
            pack.pack_size(),
            false,
        )?;
        if installed.git_sha1 != pack.git_checksum() {
            return Err(CrabError::CorruptObject {
                path: identity,
                reason: format!(
                    "historical pack trailer is {}, checkpoint declares {}",
                    installed.git_sha1,
                    pack.git_checksum()
                ),
            });
        }
        let index_path = sources.join(format!("{index}.idx"));
        let reverse_path = sources.join(format!("{index}.rev"));
        std::fs::write(&index_path, pack.index_bytes())?;
        std::fs::write(&reverse_path, pack.reverse_index_bytes())?;
        let locations = crab_git::pack_locator::PackLocationIter::open(
            &index_path,
            &reverse_path,
            pack.pack_size(),
        )?;
        if locations.object_count() != pack.object_count()
            || locations.pack_checksum().to_string() != pack.git_checksum()
        {
            return Err(CrabError::CorruptObject {
                path: identity,
                reason: "historical pack index disagrees with its checkpoint descriptor".to_owned(),
            });
        }
        crab_git::decode_pack_kind_metadata(pack.locator_bytes(), locations)?;
    }
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
            .args(["fsck", "--strict", "--full", "--no-reflogs"]),
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
        let rebuilt = rebuild_retained_history(&chain, args.keep_last)?;
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
        let current_visibility =
            crab_metadata::capsule_protocol::CapsuleVisibilitySnapshot::from_index(
                &view.git_visibility_index()?,
            )?;
        let current_packs = crab_remote::checkpoint::consolidate_git_packs(
            &view,
            RESTORE_CHECKPOINT_BYTES,
            RESTORE_CHECKPOINT_BYTES,
            cancel,
        )
        .await
        .map_err(map_checkpoint_error)?
        .into_packs();
        let current_checkpoint = Checkpoint::build_with_catalogs(
            view.root().root().generation(),
            view.root().digest(),
            current_packs,
            current_catalog.clone(),
            Some(current_visibility),
        )?;
        let checkpointed = crab_write::capsule_protocol::publish_ref_checkpoint(
            layout,
            view.root_snapshot().clone(),
            &current_checkpoint,
            view.refs().clone(),
            view.peeled_refs().clone(),
            view.visible_ref_transactions().clone(),
            view.capsule_run_pointers().to_vec(),
        )
        .await?;
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
            let visibility = verified.checkpoint.visibility_snapshot()?.ok_or_else(|| {
                CrabError::CorruptObject {
                    path: verified.checkpoint.hash().to_owned(),
                    reason: "historical checkpoint has no complete Git visibility snapshot"
                        .to_owned(),
                }
            })?;
            let checkpoint = Checkpoint::build_with_catalogs(
                fenced.record().root().generation(),
                fenced.record().digest(),
                checkpoint_git_packs(&verified.checkpoint)?,
                current_catalog,
                Some(visibility),
            )?;
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
        crab_remote::checkpoint::CheckpointError::Io(source) => source.into(),
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
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use crab_metadata::capsule_protocol::{CapsulePointer, CheckpointPointer, HistorySegmentState};

    fn segment(
        generation: u64,
        previous: Option<crab_metadata::capsule_protocol::HistorySegmentPointer>,
    ) -> HistorySegment {
        let root_digest = format!("{:064x}", generation + 100);
        let checkpoint = CheckpointPointer::new(
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
            1,
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
