//! Historical manifest inspection, verification, and restoration.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use clap::{Args, Subcommand};
use crab_coordination::PushLock;
use crab_git::pack_locator::PackLocationIter;
use crab_xet::hash::{MerkleHash, compute_data_hash};
use crab_xet::shard::ShardReader;
use crab_xet::xorb::parser::XorbParser;
use schemars::JsonSchema;
use serde::Serialize;
use tokio::io::AsyncReadExt as _;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::audit::{AuditEvent, AuditOutcome, NewAuditEvent, append_event, default_log_path};
use crate::coordination::heartbeat::LockHeartbeat;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::{OutputMode, emit_json};
use crate::git::push::{
    CommittedManifestAnchor, CommittedPackIndex, publish_committed_pack_locators,
};
use crate::metadata::manifest::{
    Manifest, ManifestHistoryEntry, PackManifestEntry, list_manifest_history, read_bulk_pack_list,
    read_bulk_shard_list, read_manifest, read_pack_index, read_shard_index,
    select_manifest_history, write_manifest_cas,
};
use crate::storage::StoreLayout;
use crate::storage::store::Store;

pub const HISTORY_LIST_SCHEMA: &str = "recover.history.list";
pub const HISTORY_VERIFY_SCHEMA: &str = "recover.history.verify";
pub const HISTORY_RESTORE_SCHEMA: &str = "recover.history.restore";
pub const HISTORY_SCHEMA_VERSION: &str = "1.0";

const RECOVERY_LOCK_TTL: Duration = Duration::from_mins(5);

#[derive(Debug, Clone, Subcommand)]
pub enum HistoryCmd {
    /// List immutable historical repository roots.
    List(HistoryListArgs),
    /// Verify one historical root and its complete dependency closure.
    Verify(HistoryVerifyArgs),
    /// Preview or apply restoration of one verified historical root.
    Restore(HistoryRestoreArgs),
}

#[derive(Debug, Clone, Args)]
pub struct HistoryListArgs {
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct HistoryVerifyArgs {
    /// Historical manifest generation.
    pub generation: u64,
    /// Exact historical manifest digest when a generation has multiple roots.
    #[arg(long, value_name = "BLAKE3")]
    pub digest: Option<String>,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Args)]
pub struct HistoryRestoreArgs {
    /// Historical manifest generation.
    pub generation: u64,
    /// Exact historical manifest digest when a generation has multiple roots.
    #[arg(long, value_name = "BLAKE3")]
    pub digest: Option<String>,
    /// Commit the restore. Without this flag, only a verified preview is shown.
    #[arg(long)]
    pub apply: bool,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

impl HistoryCmd {
    #[must_use]
    pub fn output_mode(&self) -> OutputMode {
        OutputMode::from_flags(self.json(), false)
    }

    #[must_use]
    pub fn schema_name(&self) -> &'static str {
        match self {
            Self::List(_) => HISTORY_LIST_SCHEMA,
            Self::Verify(_) => HISTORY_VERIFY_SCHEMA,
            Self::Restore(_) => HISTORY_RESTORE_SCHEMA,
        }
    }

    #[must_use]
    pub fn applies_restore(&self) -> bool {
        matches!(self, Self::Restore(args) if args.apply)
    }

    fn json(&self) -> bool {
        match self {
            Self::List(args) => args.json,
            Self::Verify(args) => args.json,
            Self::Restore(args) => args.json,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct HistoryEntryPayload {
    pub generation: u64,
    pub digest: String,
    pub created_at: String,
    pub session_id: String,
    pub refs: u64,
    pub manifest_bytes: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct HistoryListPayload {
    pub current_generation: u64,
    pub entries: Vec<HistoryEntryPayload>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct HistoryVerificationPayload {
    pub generation: u64,
    pub digest: String,
    pub refs: u64,
    pub packs: u64,
    pub git_objects: u64,
    pub shards: u64,
    pub xorbs: u64,
    pub dependency_objects: u64,
    pub dependency_bytes: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct HistoryRestorePayload {
    pub applied: bool,
    pub source_generation: u64,
    pub source_digest: String,
    pub previous_generation: u64,
    pub restored_generation: Option<u64>,
    pub refs_added: u64,
    pub refs_updated: u64,
    pub refs_deleted: u64,
    pub acceleration_rebuilt: bool,
    pub verification: HistoryVerificationPayload,
}

struct VerifiedPack {
    manifest: PackManifestEntry,
    index_path: PathBuf,
    reverse_index_path: PathBuf,
    git_sha1: String,
}

struct VerifiedHistory {
    entry: ManifestHistoryEntry,
    verification: HistoryVerificationPayload,
    _workspace: tempfile::TempDir,
    packs: Vec<VerifiedPack>,
}

struct RecoveryLease {
    lock: PushLock,
    heartbeat: LockHeartbeat,
}

pub async fn run(
    command: &HistoryCmd,
    store: &Store,
    prefix: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    let router = StoreLayout::new(store.clone(), prefix.to_owned());
    match command {
        HistoryCmd::List(_) => run_list(store, &router, command.output_mode()).await,
        HistoryCmd::Verify(args) => {
            let verified = verify_history(
                store,
                &router,
                args.generation,
                args.digest.as_deref(),
                cancel,
            )
            .await?;
            emit_verification(&verified.verification, command.output_mode());
            Ok(())
        }
        HistoryCmd::Restore(args) => {
            let payload = restore_history(store, &router, args, cancel).await?;
            if payload.applied
                && let Err(error) = record_restore_audit(prefix, &payload)
            {
                warn!(%error, "failed to append historical restore audit event");
            }
            emit_restore(&payload, command.output_mode());
            Ok(())
        }
    }
}

fn record_restore_audit(prefix: &str, payload: &HistoryRestorePayload) -> Result<()> {
    let event = AuditEvent::new(NewAuditEvent {
        operation: "recover.history.restore".to_owned(),
        outcome: AuditOutcome::Success,
        actor: None,
        repository: Some(prefix.to_owned()),
        details: serde_json::json!({
            "source_generation": payload.source_generation,
            "source_digest": payload.source_digest,
            "previous_generation": payload.previous_generation,
            "restored_generation": payload.restored_generation,
            "refs_added": payload.refs_added,
            "refs_updated": payload.refs_updated,
            "refs_deleted": payload.refs_deleted,
            "acceleration_rebuilt": payload.acceleration_rebuilt,
            "dependency_objects": payload.verification.dependency_objects,
            "dependency_bytes": payload.verification.dependency_bytes,
        }),
    });
    append_event(&default_log_path(), &event)
}

async fn run_list(store: &Store, router: &StoreLayout, mode: OutputMode) -> Result<()> {
    let (current, _) = read_manifest(store, router).await?;
    let entries = list_manifest_history(store, router)
        .await?
        .into_iter()
        .map(|entry| HistoryEntryPayload {
            generation: entry.generation,
            digest: entry.digest,
            created_at: entry.manifest.created_at,
            session_id: entry.manifest.session_id,
            refs: entry.manifest.refs.len() as u64,
            manifest_bytes: entry.size,
        })
        .collect::<Vec<_>>();
    let payload = HistoryListPayload {
        current_generation: current.generation,
        entries,
    };
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(HISTORY_LIST_SCHEMA, HISTORY_SCHEMA_VERSION, &payload);
        }
        OutputMode::Text => {
            println!(
                "current generation: {}; historical roots: {}",
                payload.current_generation,
                payload.entries.len()
            );
            for entry in payload.entries {
                println!(
                    "{} {} refs={} bytes={} created_at={}",
                    entry.generation,
                    entry.digest,
                    entry.refs,
                    entry.manifest_bytes,
                    entry.created_at
                );
            }
        }
    }
    Ok(())
}

async fn restore_history(
    store: &Store,
    router: &StoreLayout,
    args: &HistoryRestoreArgs,
    cancel: &CancellationToken,
) -> Result<HistoryRestorePayload> {
    let verified = verify_history(
        store,
        router,
        args.generation,
        args.digest.as_deref(),
        cancel,
    )
    .await?;
    let (current, current_etag) = read_manifest(store, router).await?;
    let (refs_added, refs_updated, refs_deleted) =
        ref_change_counts(&current, &verified.entry.manifest);
    let mut payload = HistoryRestorePayload {
        applied: false,
        source_generation: verified.entry.generation,
        source_digest: verified.entry.digest.clone(),
        previous_generation: current.generation,
        restored_generation: None,
        refs_added,
        refs_updated,
        refs_deleted,
        acceleration_rebuilt: false,
        verification: verified.verification.clone(),
    };
    if !args.apply {
        return Ok(payload);
    }

    let (restored, acceleration_rebuilt) =
        apply_verified_history(store, router, &verified, &current, &current_etag, cancel).await?;
    payload.applied = true;
    payload.restored_generation = Some(restored.generation);
    payload.acceleration_rebuilt = acceleration_rebuilt;
    Ok(payload)
}

async fn apply_verified_history(
    store: &Store,
    router: &StoreLayout,
    verified: &VerifiedHistory,
    current: &Manifest,
    current_etag: &str,
    cancel: &CancellationToken,
) -> Result<(Manifest, bool)> {
    let operation_cancel = cancel.child_token();
    let refs = recovery_lock_refs(current, &verified.entry.manifest);
    let leases = acquire_recovery_leases(store, router, &refs, &operation_cancel).await?;
    let operation = async {
        check_cancelled(&operation_cancel)?;
        let (pinned, pinned_etag) = read_manifest(store, router).await?;
        if pinned_etag != current_etag || pinned != *current {
            return Err(CrabError::CasConflict {
                path: router.manifest_path().as_ref().to_owned(),
                expected_etag: Some(current_etag.to_owned()),
            });
        }
        let generation = current.generation.checked_add(1).ok_or_else(|| {
            CrabError::Internal("manifest generation overflow during history restore".to_owned())
        })?;
        let mut restored = verified.entry.manifest.clone();
        restored.generation = generation;
        restored.created_at = now_iso8601();
        restored.pusher = None;
        restored.session_id = format!(
            "history-recovery-{}-{}",
            verified.entry.generation,
            &verified.entry.digest[..12]
        );
        restored.seal_git_validation();
        write_manifest_cas(store, router, &restored, current_etag).await?;
        Ok(restored)
    }
    .await;
    let release = release_recovery_leases(leases).await;
    let restored = match (operation, release) {
        (Ok(restored), Ok(())) => restored,
        (Err(error), _) | (Ok(_), Err(error)) => return Err(error),
    };

    let acceleration_rebuilt = rebuild_locator_inventory(store, router, &restored, verified)
        .await
        .map_or_else(
            |error| {
                warn!(%error, generation = restored.generation, "history restored; locator acceleration requires repair");
                false
            },
            |()| true,
        );
    Ok((restored, acceleration_rebuilt))
}

fn ref_change_counts(current: &Manifest, historical: &Manifest) -> (u64, u64, u64) {
    let added = historical
        .refs
        .keys()
        .filter(|name| !current.refs.contains_key(*name))
        .count() as u64;
    let updated = historical
        .refs
        .iter()
        .filter(|(name, oid)| current.refs.get(*name).is_some_and(|value| value != *oid))
        .count() as u64;
    let deleted = current
        .refs
        .keys()
        .filter(|name| !historical.refs.contains_key(*name))
        .count() as u64;
    (added, updated, deleted)
}

fn recovery_lock_refs(current: &Manifest, historical: &Manifest) -> Vec<String> {
    let mut refs = current.refs.keys().cloned().collect::<BTreeSet<_>>();
    refs.extend(historical.refs.keys().cloned());
    refs.into_iter().collect()
}

async fn acquire_recovery_leases(
    store: &Store,
    router: &StoreLayout,
    refs: &[String],
    cancel: &CancellationToken,
) -> Result<Vec<RecoveryLease>> {
    let mut leases = Vec::with_capacity(refs.len().max(1));
    for ref_name in refs.iter().map(Some).chain(refs.is_empty().then_some(None)) {
        if let Err(error) = check_cancelled(cancel) {
            let _ = release_recovery_leases(leases).await;
            return Err(error);
        }
        let acquired = match ref_name {
            Some(ref_name) => {
                PushLock::acquire_ref(
                    store.inner(),
                    router.repo_prefix(),
                    ref_name,
                    RECOVERY_LOCK_TTL,
                )
                .await
            }
            None => {
                PushLock::acquire_internal(
                    store.inner(),
                    router.repo_prefix(),
                    crab_coordination::HISTORY_RECOVERY_RESOURCE,
                    RECOVERY_LOCK_TTL,
                )
                .await
            }
        };
        let lock = match acquired.map_err(CrabError::from) {
            Ok(lock) => lock,
            Err(error) => {
                let _ = release_recovery_leases(leases).await;
                return Err(error);
            }
        };
        let heartbeat = LockHeartbeat::spawn(
            store.clone(),
            lock.path().to_owned(),
            lock.holder().to_owned(),
            lock.ttl(),
            lock.ttl() / 3,
            cancel.clone(),
        );
        leases.push(RecoveryLease { lock, heartbeat });
    }
    Ok(leases)
}

async fn release_recovery_leases(mut leases: Vec<RecoveryLease>) -> Result<()> {
    let mut first_error = None;
    while let Some(RecoveryLease { lock, heartbeat }) = leases.pop() {
        heartbeat.stop().await;
        if let Err(error) = lock.release().await.map_err(CrabError::from)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

async fn verify_history(
    store: &Store,
    router: &StoreLayout,
    generation: u64,
    digest: Option<&str>,
    cancel: &CancellationToken,
) -> Result<VerifiedHistory> {
    check_cancelled(cancel)?;
    let entry = select_manifest_history(store, router, generation, digest).await?;
    let workspace = tempfile::tempdir()?;
    let packs_dir = workspace.path().join("packs");
    tokio::fs::create_dir_all(&packs_dir).await?;
    let mut objects = BTreeMap::new();
    record_object(&mut objects, entry.path.clone(), entry.size)?;

    let shards = if entry.manifest.shard_index_hash.is_empty() {
        Vec::new()
    } else {
        let values = read_bulk_shard_list(store, router, &entry.manifest.shard_index_hash).await?;
        record_segmented_metadata(
            store,
            router,
            crab_metadata::segmented::SegmentKind::Shard,
            &entry.manifest.shard_index_hash,
            &mut objects,
        )
        .await?;
        values
    };
    let pack_manifests = if entry.manifest.pack_index_hash.is_empty() {
        Vec::new()
    } else {
        let values = read_bulk_pack_list(store, router, &entry.manifest.pack_index_hash).await?;
        record_segmented_metadata(
            store,
            router,
            crab_metadata::segmented::SegmentKind::Pack,
            &entry.manifest.pack_index_hash,
            &mut objects,
        )
        .await?;
        values
    };

    for (kind, hash) in [
        ("commit-graph", entry.manifest.commit_graph_hash.as_deref()),
        ("ref-registry", entry.manifest.ref_registry_hash.as_deref()),
    ] {
        if let Some(hash) = hash {
            let path = router.bulk_manifest_path(kind, hash);
            let expected = parse_blake3(&path, hash)?;
            let bytes = store.verify(&path, &expected).await?;
            record_object(&mut objects, path.as_ref().to_owned(), bytes.len() as u64)?;
        }
    }

    let mut verified_packs = Vec::with_capacity(pack_manifests.len());
    let mut git_objects = 0_u64;
    for pack in pack_manifests {
        check_cancelled(cancel)?;
        let pack_path = packs_dir.join(format!("{}.pack", pack.pack_id));
        let index_path = packs_dir.join(format!("{}.idx", pack.pack_id));
        let reverse_index_path = packs_dir.join(format!("{}.rev", pack.pack_id));
        let downloaded = store
            .download_to_path(&router.pack_path(&pack.pack_id), &pack_path)
            .await?;
        if downloaded != pack.size {
            return Err(CrabError::CorruptObject {
                path: router.pack_path(&pack.pack_id).as_ref().to_owned(),
                reason: format!("pack size is {downloaded}, expected {}", pack.size),
            });
        }
        let actual = hash_file(&pack_path).await?;
        if actual != pack.pack_id {
            return Err(CrabError::CorruptObject {
                path: router.pack_path(&pack.pack_id).as_ref().to_owned(),
                reason: format!("pack content hash is {actual}, expected {}", pack.pack_id),
            });
        }
        record_object(
            &mut objects,
            router.pack_path(&pack.pack_id).as_ref().to_owned(),
            downloaded,
        )?;
        let index_size = store
            .download_to_path(&router.pack_index_path(&pack.pack_id), &index_path)
            .await?;
        record_object(
            &mut objects,
            router.pack_index_path(&pack.pack_id).as_ref().to_owned(),
            index_size,
        )?;
        let remote_reverse_index = router.pack_reverse_index_path(&pack.pack_id);
        match store
            .download_to_path(&remote_reverse_index, &reverse_index_path)
            .await
        {
            Ok(reverse_index_size) => record_object(
                &mut objects,
                remote_reverse_index.as_ref().to_owned(),
                reverse_index_size,
            )?,
            Err(CrabError::NotFound { .. }) => {
                let index = index_path.clone();
                let reverse = reverse_index_path.clone();
                tokio::task::spawn_blocking(move || {
                    crab_git::pack_locator::write_pack_reverse_index(&index, &reverse)
                        .map_err(crab_git::pack::PackError::from)
                        .map_err(CrabError::from)
                })
                .await
                .map_err(|error| {
                    CrabError::Internal(format!("reverse-index generation worker failed: {error}"))
                })??;
            }
            Err(error) => return Err(error),
        }
        let locations = PackLocationIter::open(&index_path, &reverse_index_path, pack.size)
            .map_err(crab_git::pack::PackError::from)?;
        if locations.object_count() != pack.object_count {
            return Err(CrabError::CorruptObject {
                path: router.pack_index_path(&pack.pack_id).as_ref().to_owned(),
                reason: format!(
                    "pack index contains {} objects, expected {}",
                    locations.object_count(),
                    pack.object_count
                ),
            });
        }
        git_objects = git_objects.checked_add(pack.object_count).ok_or_else(|| {
            CrabError::Internal("Git object count overflow during history verification".to_owned())
        })?;
        let git_sha1 = locations.pack_checksum().to_string();
        drop(locations);
        verified_packs.push(VerifiedPack {
            manifest: pack,
            index_path,
            reverse_index_path,
            git_sha1,
        });
    }
    verify_git_repository(workspace.path(), &entry.manifest, &verified_packs).await?;

    let mut xorb_hashes = BTreeSet::new();
    for shard_hash in &shards {
        check_cancelled(cancel)?;
        let hash = MerkleHash::from_hex(shard_hash).map_err(|error| CrabError::CorruptObject {
            path: router.shard_path(shard_hash).as_ref().to_owned(),
            reason: format!("invalid shard hash: {error}"),
        })?;
        let path = router.shard_path(&hash);
        let (bytes, _) = store.get_with_etag(&path).await?;
        let actual = compute_data_hash(&bytes);
        if actual != hash {
            return Err(CrabError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!(
                    "shard content hash is {}, expected {}",
                    actual.hex(),
                    hash.hex()
                ),
            });
        }
        record_object(&mut objects, path.as_ref().to_owned(), bytes.len() as u64)?;
        let reader = ShardReader::from_bytes(bytes, hash);
        let shard = reader
            .shard_info_public()
            .map_err(|error| CrabError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!("failed to parse shard: {error}"),
            })?;
        let mut cursor = std::io::Cursor::new(reader.v1_data());
        let blocks = shard
            .read_all_xorb_blocks_full(&mut cursor)
            .map_err(|error| CrabError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!("failed to read shard xorb blocks: {error}"),
            })?;
        xorb_hashes.extend(blocks.into_iter().map(|block| block.metadata.xorb_hash));
        let mut cursor = std::io::Cursor::new(reader.v1_data());
        shard
            .read_all_file_info_sections(&mut cursor)
            .map_err(|error| CrabError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!("failed to read shard file records: {error}"),
            })?;
    }

    for hash in &xorb_hashes {
        check_cancelled(cancel)?;
        let path = router.xorb_path(hash);
        let (bytes, _) = store.get_with_etag(&path).await?;
        let parser =
            XorbParser::parse(bytes.clone()).map_err(|error| CrabError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!("failed to parse xorb: {error}"),
            })?;
        if parser.hash() != *hash {
            return Err(CrabError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!(
                    "xorb logical hash is {}, expected {}",
                    parser.hash().hex(),
                    hash.hex()
                ),
            });
        }
        parser
            .verify_payload_digest()
            .and_then(|()| parser.verify_all_chunks())
            .map_err(|error| CrabError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: format!("xorb payload verification failed: {error}"),
            })?;
        record_object(&mut objects, path.as_ref().to_owned(), bytes.len() as u64)?;
    }

    let dependency_bytes = objects.values().try_fold(0_u64, |total, size| {
        total
            .checked_add(*size)
            .ok_or_else(|| CrabError::Internal("dependency byte count overflow".to_owned()))
    })?;
    let verification = HistoryVerificationPayload {
        generation: entry.generation,
        digest: entry.digest.clone(),
        refs: entry.manifest.refs.len() as u64,
        packs: verified_packs.len() as u64,
        git_objects,
        shards: shards.len() as u64,
        xorbs: xorb_hashes.len() as u64,
        dependency_objects: objects.len() as u64,
        dependency_bytes,
    };
    Ok(VerifiedHistory {
        entry,
        verification,
        _workspace: workspace,
        packs: verified_packs,
    })
}

async fn record_segmented_metadata(
    store: &Store,
    router: &StoreLayout,
    kind: crab_metadata::segmented::SegmentKind,
    hash: &str,
    objects: &mut BTreeMap<String, u64>,
) -> Result<()> {
    let index = match kind {
        crab_metadata::segmented::SegmentKind::Pack => read_pack_index(store, router, hash).await?,
        crab_metadata::segmented::SegmentKind::Shard => {
            read_shard_index(store, router, hash).await?
        }
    };
    let index_path = router.repo_path(&crab_metadata::segmented::index_relative_path(kind, hash));
    let index_size = store.head(&index_path).await?.size;
    record_object(objects, index_path.as_ref().to_owned(), index_size)?;
    for segment in index.segments {
        let path = router.repo_path(&segment.path);
        let size = store.head(&path).await?.size;
        record_object(objects, path.as_ref().to_owned(), size)?;
    }
    Ok(())
}

fn record_object(objects: &mut BTreeMap<String, u64>, path: String, size: u64) -> Result<()> {
    if let Some(previous) = objects.insert(path.clone(), size)
        && previous != size
    {
        return Err(CrabError::CorruptObject {
            path,
            reason: format!("dependency size changed from {previous} to {size} while verifying"),
        });
    }
    Ok(())
}

fn parse_blake3(path: &object_store::path::Path, value: &str) -> Result<[u8; 32]> {
    blake3::Hash::from_hex(value)
        .map(|hash| *hash.as_bytes())
        .map_err(|error| CrabError::CorruptObject {
            path: path.as_ref().to_owned(),
            reason: format!("invalid Blake3 content hash: {error}"),
        })
}

async fn hash_file(path: &Path) -> Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

async fn verify_git_repository(
    workspace: &Path,
    manifest: &Manifest,
    packs: &[VerifiedPack],
) -> Result<()> {
    let repository = workspace.join("repository.git");
    let manifest = manifest.clone();
    let pack_inputs = packs
        .iter()
        .map(|pack| {
            (
                pack.manifest.clone(),
                workspace
                    .join("packs")
                    .join(format!("{}.pack", pack.manifest.pack_id)),
                pack.git_sha1.clone(),
            )
        })
        .collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || {
        prepare_git_repository(&repository, &manifest, &pack_inputs)
    })
    .await
    .map_err(|error| CrabError::Internal(format!("history verification worker failed: {error}")))?
}

fn prepare_git_repository(
    repository: &Path,
    manifest: &Manifest,
    packs: &[(PackManifestEntry, PathBuf, String)],
) -> Result<()> {
    run_git(
        Command::new("git")
            .arg("init")
            .arg("--bare")
            .arg("--quiet")
            .arg(repository),
        "initialize verification repository",
    )?;
    for (pack, path, indexed_git_sha1) in packs {
        let installed = crab_git::install_pack_file_from_path(
            &repository.join("objects/pack"),
            path,
            &pack.pack_id,
            pack.size,
            false,
        )?;
        if installed.git_sha1 != *indexed_git_sha1 {
            return Err(CrabError::CorruptObject {
                path: path.display().to_string(),
                reason: format!(
                    "canonical pack index checksum is {indexed_git_sha1}, pack trailer is {}",
                    installed.git_sha1
                ),
            });
        }
    }
    for (name, oid) in &manifest.refs {
        run_git(
            Command::new("git")
                .arg(format!("--git-dir={}", repository.display()))
                .arg("update-ref")
                .arg(name)
                .arg(oid),
            "install historical ref",
        )?;
    }
    run_git(
        Command::new("git")
            .arg(format!("--git-dir={}", repository.display()))
            .arg("symbolic-ref")
            .arg("HEAD")
            .arg(&manifest.head),
        "install historical HEAD",
    )?;
    run_git(
        Command::new("git")
            .arg(format!("--git-dir={}", repository.display()))
            .args(["fsck", "--strict", "--full", "--no-reflogs"]),
        "verify historical Git connectivity",
    )?;
    for (name, expected) in &manifest.peeled_refs {
        let output = git_command(
            Command::new("git")
                .arg(format!("--git-dir={}", repository.display()))
                .args(["rev-parse", "--verify"])
                .arg(format!("{name}^{{}}")),
        )
        .output()
        .map_err(CrabError::Io)?;
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

fn run_git(command: &mut Command, operation: &str) -> Result<()> {
    let status = git_command(command)
        .stderr(Stdio::null())
        .status()
        .map_err(CrabError::Io)?;
    if status.success() {
        Ok(())
    } else {
        Err(CrabError::Internal(format!("git failed to {operation}")))
    }
}

fn git_command(command: &mut Command) -> &mut Command {
    command
        .env_clear()
        .env(
            "PATH",
            std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin")),
        )
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0")
}

async fn rebuild_locator_inventory(
    store: &Store,
    router: &StoreLayout,
    restored: &Manifest,
    verified: &VerifiedHistory,
) -> Result<()> {
    let shard_index_hash = manifest_hash_or_default(&restored.shard_index_hash)?;
    let pack_index_hash = manifest_hash_or_default(&restored.pack_index_hash)?;
    let committed = verified
        .packs
        .iter()
        .map(|pack| CommittedPackIndex {
            pack: &pack.manifest,
            idx_path: &pack.index_path,
            rev_path: &pack.reverse_index_path,
            git_sha1: &pack.git_sha1,
        })
        .collect::<Vec<_>>();
    publish_committed_pack_locators(
        store,
        router,
        &committed,
        CommittedManifestAnchor {
            generation: restored.generation,
            shard_index_hash,
            pack_index_hash,
        },
        RECOVERY_LOCK_TTL,
    )
    .await?;
    Ok(())
}

fn manifest_hash_or_default(value: &str) -> Result<MerkleHash> {
    if value.is_empty() {
        return Ok(MerkleHash::default());
    }
    MerkleHash::from_hex(value)
        .map_err(|error| CrabError::Internal(format!("invalid committed manifest hash: {error}")))
}

fn now_iso8601() -> String {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = duration.as_secs();
    let days = seconds / 86_400;
    let time_of_day = seconds % 86_400;
    let (year, month, day) = days_to_ymd(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time_of_day / 3_600,
        (time_of_day % 3_600) / 60,
        time_of_day % 60
    )
}

fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    if month <= 2 {
        year += 1;
    }
    (year, month, day)
}

fn emit_verification(payload: &HistoryVerificationPayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(HISTORY_VERIFY_SCHEMA, HISTORY_SCHEMA_VERSION, payload);
        }
        OutputMode::Text => println!(
            "verified generation {} {}: refs={} packs={} git_objects={} shards={} xorbs={} dependency_objects={} dependency_bytes={}",
            payload.generation,
            payload.digest,
            payload.refs,
            payload.packs,
            payload.git_objects,
            payload.shards,
            payload.xorbs,
            payload.dependency_objects,
            payload.dependency_bytes
        ),
    }
}

fn emit_restore(payload: &HistoryRestorePayload, mode: OutputMode) {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(HISTORY_RESTORE_SCHEMA, HISTORY_SCHEMA_VERSION, payload);
        }
        OutputMode::Text => {
            let action = if payload.applied {
                "restored"
            } else {
                "preview"
            };
            println!(
                "history restore {action}: source={} current={} restored={:?} refs_added={} refs_updated={} refs_deleted={} acceleration_rebuilt={}",
                payload.source_generation,
                payload.previous_generation,
                payload.restored_generation,
                payload.refs_added,
                payload.refs_updated,
                payload.refs_deleted,
                payload.acceleration_rebuilt
            );
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "test assertions"
)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use object_store::memory::InMemory;

    use super::*;
    use crate::metadata::manifest::{create_manifest, read_manifest, write_manifest_cas};

    fn memory_store() -> Store {
        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        Store::new(inner)
    }

    async fn repository_with_history() -> (Store, StoreLayout, Manifest) {
        let store = memory_store();
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        let mut historical = Manifest::default_for_repo("refs/heads/main");
        historical.created_at = "2026-01-01T00:00:00Z".to_owned();
        historical.session_id = "known-good".to_owned();
        historical.seal_git_validation();
        create_manifest(&store, &router, &historical).await.unwrap();
        let (_, etag) = read_manifest(&store, &router).await.unwrap();
        let mut current = historical.clone();
        current.generation = 1;
        current.created_at = "2026-01-02T00:00:00Z".to_owned();
        current.session_id = "bad-push".to_owned();
        current.seal_git_validation();
        write_manifest_cas(&store, &router, &current, &etag)
            .await
            .unwrap();
        (store, router, historical)
    }

    async fn put_history(store: &Store, router: &StoreLayout, manifest: &Manifest) -> String {
        let body = serde_json::to_vec_pretty(manifest).unwrap();
        let digest = blake3::hash(&body).to_hex().to_string();
        store
            .put_exact(
                &router.manifest_history_path(manifest.generation, &digest),
                Bytes::from(body),
            )
            .await
            .unwrap();
        digest
    }

    #[tokio::test]
    async fn verify_preview_and_restore_republish_historical_state_monotonically() {
        let (store, router, historical) = repository_with_history().await;
        let cancel = CancellationToken::new();

        let verified = verify_history(&store, &router, 0, None, &cancel)
            .await
            .unwrap();
        assert_eq!(verified.verification.dependency_objects, 1);

        let preview = restore_history(
            &store,
            &router,
            &HistoryRestoreArgs {
                generation: 0,
                digest: None,
                apply: false,
                json: false,
            },
            &cancel,
        )
        .await
        .unwrap();
        assert!(!preview.applied);
        assert_eq!(
            read_manifest(&store, &router).await.unwrap().0.generation,
            1
        );

        let restored = restore_history(
            &store,
            &router,
            &HistoryRestoreArgs {
                generation: 0,
                digest: None,
                apply: true,
                json: false,
            },
            &cancel,
        )
        .await
        .unwrap();
        let current = read_manifest(&store, &router).await.unwrap().0;

        assert!(restored.applied);
        assert_eq!(restored.restored_generation, Some(2));
        assert_eq!(current.generation, 2);
        assert_eq!(current.refs, historical.refs);
        assert_eq!(current.head, historical.head);
        assert_eq!(
            list_manifest_history(&store, &router).await.unwrap().len(),
            2
        );
    }

    #[tokio::test]
    async fn ambiguous_generation_requires_digest_selection() {
        let (store, router, historical) = repository_with_history().await;
        let mut alternative = historical;
        alternative.session_id = "alternative-root".to_owned();
        let digest = put_history(&store, &router, &alternative).await;

        assert!(
            verify_history(&store, &router, 0, None, &CancellationToken::new())
                .await
                .is_err()
        );
        let selected = verify_history(&store, &router, 0, Some(&digest), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(selected.entry.digest, digest);
    }

    #[tokio::test]
    async fn verification_rejects_missing_and_corrupt_dependencies() {
        let store = memory_store();
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        let mut missing = Manifest::default_for_repo("refs/heads/main");
        missing.shard_index_hash = "a".repeat(64);
        missing.seal_git_validation();
        put_history(&store, &router, &missing).await;
        assert!(
            verify_history(&store, &router, 0, None, &CancellationToken::new())
                .await
                .is_err()
        );

        let corrupt_store = memory_store();
        let corrupt_router = StoreLayout::new(corrupt_store.clone(), "org/repo".to_owned());
        let mut corrupt = Manifest::default_for_repo("refs/heads/main");
        corrupt.commit_graph_hash = Some("b".repeat(64));
        corrupt.seal_git_validation();
        put_history(&corrupt_store, &corrupt_router, &corrupt).await;
        corrupt_store
            .put(
                &corrupt_router.bulk_manifest_path("commit-graph", &"b".repeat(64)),
                Bytes::from_static(b"corrupt"),
            )
            .await
            .unwrap();
        assert!(
            verify_history(
                &corrupt_store,
                &corrupt_router,
                0,
                None,
                &CancellationToken::new(),
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn stale_pinned_current_aborts_restore_and_releases_lease() {
        let (store, router, _) = repository_with_history().await;
        let cancel = CancellationToken::new();
        let verified = verify_history(&store, &router, 0, None, &cancel)
            .await
            .unwrap();
        let (pinned, pinned_etag) = read_manifest(&store, &router).await.unwrap();
        let mut concurrent = pinned.clone();
        concurrent.generation += 1;
        concurrent.session_id = "concurrent-push".to_owned();
        concurrent.seal_git_validation();
        write_manifest_cas(&store, &router, &concurrent, &pinned_etag)
            .await
            .unwrap();

        let error =
            apply_verified_history(&store, &router, &verified, &pinned, &pinned_etag, &cancel)
                .await
                .unwrap_err();
        assert!(matches!(error, CrabError::CasConflict { .. }));
        let lock = PushLock::acquire_internal(
            store.inner(),
            router.repo_prefix(),
            crab_coordination::HISTORY_RECOVERY_RESOURCE,
            RECOVERY_LOCK_TTL,
        )
        .await
        .unwrap();
        lock.release().await.unwrap();
    }

    #[tokio::test]
    async fn held_internal_recovery_lease_blocks_restore_without_moving_manifest() {
        let (store, router, _) = repository_with_history().await;
        let lock = PushLock::acquire_internal(
            store.inner(),
            router.repo_prefix(),
            crab_coordination::HISTORY_RECOVERY_RESOURCE,
            RECOVERY_LOCK_TTL,
        )
        .await
        .unwrap();

        let error = restore_history(
            &store,
            &router,
            &HistoryRestoreArgs {
                generation: 0,
                digest: None,
                apply: true,
                json: false,
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, CrabError::PushLockHeld { .. }));
        assert_eq!(
            read_manifest(&store, &router).await.unwrap().0.generation,
            1
        );
        lock.release().await.unwrap();
    }
}
