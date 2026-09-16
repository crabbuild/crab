//! Authenticated repository-history inspection, verification, and recovery.

#[path = "history_recovery_v2.rs"]
mod v2;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::{Args, Subcommand};
use crab_git::pack_locator::PackLocationIter;
use crab_xet::hash::{MerkleHash, compute_data_hash};
use crab_xet::shard::ShardReader;
use crab_xet::shard_parse::MAX_SHARD_SIZE_BYTES;
use crab_xet::xorb::format::MAX_XORB_SIZE;
use crab_xet::xorb::parser::XorbParser;
use schemars::JsonSchema;
use serde::Serialize;
use tokio::io::AsyncReadExt as _;
use tokio_util::sync::CancellationToken;

use crate::audit::{AuditEvent, AuditOutcome, NewAuditEvent, append_event, default_log_path};
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::core::output::{OutputMode, emit_json};
use crate::metadata::manifest::{
    Manifest, ManifestHistoryEntry, PackManifestEntry, read_bulk_pack_list, read_bulk_shard_list,
    read_pack_index, read_shard_index, select_manifest_history,
};
use crate::storage::StoreLayout;
use crate::storage::store::Store;

pub const HISTORY_LIST_SCHEMA: &str = "recover.history.list";
pub const HISTORY_PRUNE_SCHEMA: &str = "recover.history.prune";
pub const HISTORY_VERIFY_SCHEMA: &str = "recover.history.verify";
pub const HISTORY_RESTORE_SCHEMA: &str = "recover.history.restore";
pub const HISTORY_SCHEMA_VERSION: &str = "1.0";

#[derive(Debug, Clone, Subcommand)]
pub enum HistoryCmd {
    /// List immutable historical repository roots.
    List(HistoryListArgs),
    /// Preview or apply retention of the newest historical generations.
    Prune(HistoryPruneArgs),
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
pub struct HistoryPruneArgs {
    /// Number of newest distinct generations to retain.
    #[arg(long, value_name = "N", value_parser = parse_positive_usize)]
    pub keep_last: usize,
    /// Delete the planned historical roots. Without this flag, show a preview.
    #[arg(long)]
    pub apply: bool,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

fn parse_positive_usize(value: &str) -> std::result::Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|error| format!("invalid positive integer: {error}"))?;
    if parsed == 0 {
        return Err("value must be at least 1".to_owned());
    }
    Ok(parsed)
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
            Self::Prune(_) => HISTORY_PRUNE_SCHEMA,
            Self::Verify(_) => HISTORY_VERIFY_SCHEMA,
            Self::Restore(_) => HISTORY_RESTORE_SCHEMA,
        }
    }

    #[must_use]
    pub fn applies_restore(&self) -> bool {
        matches!(self, Self::Restore(args) if args.apply)
    }

    #[must_use]
    pub fn applies_prune(&self) -> bool {
        matches!(self, Self::Prune(args) if args.apply)
    }

    fn json(&self) -> bool {
        match self {
            Self::List(args) => args.json,
            Self::Prune(args) => args.json,
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
pub struct HistoryPrunePayload {
    pub applied: bool,
    #[schemars(range(min = 1))]
    pub keep_last: u64,
    pub roots_before: u64,
    pub roots_kept: u64,
    pub roots_pruned: u64,
    pub manifest_bytes_pruned: u64,
    pub pruned: Vec<HistoryEntryPayload>,
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
    git_sha1: String,
}

struct VerifiedHistory {
    entry: ManifestHistoryEntry,
    _workspace: tempfile::TempDir,
}

pub async fn run(
    command: &HistoryCmd,
    store: &Store,
    prefix: &str,
    root: crab_metadata::capsule_protocol::RootSnapshot,
    cancel: &CancellationToken,
) -> Result<()> {
    let router = StoreLayout::new(store.clone(), prefix.to_owned());
    v2::run(command, store, &router, root, cancel).await
}

pub(super) fn record_prune_audit(prefix: &str, payload: &HistoryPrunePayload) -> Result<()> {
    let event = AuditEvent::new(NewAuditEvent {
        operation: "recover.history.prune".to_owned(),
        outcome: AuditOutcome::Success,
        actor: None,
        repository: Some(prefix.to_owned()),
        details: serde_json::json!({
            "keep_last": payload.keep_last,
            "roots_before": payload.roots_before,
            "roots_kept": payload.roots_kept,
            "roots_pruned": payload.roots_pruned,
            "manifest_bytes_pruned": payload.manifest_bytes_pruned,
            "pruned": payload.pruned.iter().map(|entry| serde_json::json!({
                "generation": entry.generation,
                "digest": entry.digest,
            })).collect::<Vec<_>>(),
        }),
    });
    append_event(&default_log_path(), &event)
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

    if !entry.manifest.refs.is_empty() && !entry.manifest.pack_index_hash.is_empty() {
        let descriptor_path = router.shallow_closure_path(&entry.manifest.git_validation_digest);
        match store.get_with_etag(&descriptor_path).await {
            Ok((bytes, _)) => {
                record_object(
                    &mut objects,
                    descriptor_path.as_ref().to_owned(),
                    bytes.len() as u64,
                )?;
                let descriptor = crab_metadata::shallow_closure::decode_shallow_closure_descriptor(
                    &bytes,
                    descriptor_path.as_ref(),
                )?;
                if descriptor.generation != entry.manifest.generation
                    || descriptor.pack_index_hash != entry.manifest.pack_index_hash
                    || descriptor.git_validation_digest != entry.manifest.git_validation_digest
                {
                    return Err(CrabError::CorruptObject {
                        path: descriptor_path.as_ref().to_owned(),
                        reason: "shallow closure descriptor does not match historical manifest"
                            .to_owned(),
                    });
                }
                let storage_router = crab_storage::StoreLayout::new(
                    store.as_storage().clone(),
                    router.repo_prefix().to_owned(),
                );
                for reference in &descriptor.entries {
                    let path = router.repo_path(&reference.path);
                    crab_metadata::shallow_closure::load_shallow_closure_entry(
                        store.as_storage(),
                        &storage_router,
                        reference,
                        crab_metadata::shallow_closure::DEFAULT_MAX_SHALLOW_CLOSURE_ENTRY_BYTES,
                    )
                    .await
                    .map_err(CrabError::from)?;
                    record_object(&mut objects, path.as_ref().to_owned(), reference.bytes)?;
                }
            }
            Err(CrabError::NotFound { .. }) => {}
            Err(error) => return Err(error),
        }
    }

    let mut verified_packs = Vec::with_capacity(pack_manifests.len());
    for pack in pack_manifests {
        check_cancelled(cancel)?;
        let pack_path = packs_dir.join(format!("{}.pack", pack.pack_id));
        let index_path = packs_dir.join(format!("{}.idx", pack.pack_id));
        let reverse_index_path = packs_dir.join(format!("{}.rev", pack.pack_id));
        let downloaded = store
            .download_to_path_bounded(&router.pack_path(&pack.pack_id), &pack_path, pack.size)
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
        let index_maximum = crab_git::pack_locator::max_pack_index_size(pack.object_count)
            .ok_or_else(|| CrabError::CorruptObject {
                path: router.pack_index_path(&pack.pack_id).as_ref().to_owned(),
                reason: "Git pack index size overflows its bound".to_owned(),
            })?;
        let reverse_maximum = crab_git::pack_locator::pack_reverse_index_size(pack.object_count)
            .ok_or_else(|| CrabError::CorruptObject {
                path: router
                    .pack_reverse_index_path(&pack.pack_id)
                    .as_ref()
                    .to_owned(),
                reason: "Git reverse index size overflows its bound".to_owned(),
            })?;
        let index_size = store
            .download_to_path_bounded(
                &router.pack_index_path(&pack.pack_id),
                &index_path,
                index_maximum,
            )
            .await?;
        record_object(
            &mut objects,
            router.pack_index_path(&pack.pack_id).as_ref().to_owned(),
            index_size,
        )?;
        let remote_reverse_index = router.pack_reverse_index_path(&pack.pack_id);
        match store
            .download_to_path_bounded(&remote_reverse_index, &reverse_index_path, reverse_maximum)
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
        let git_sha1 = locations.pack_checksum().to_string();
        drop(locations);
        verified_packs.push(VerifiedPack {
            manifest: pack,
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
        let (bytes, _) = store
            .get_with_etag_bounded(&path, MAX_SHARD_SIZE_BYTES as u64)
            .await?;
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
        let (bytes, _) = store
            .get_with_etag_bounded(&path, MAX_XORB_SIZE as u64)
            .await?;
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

    Ok(VerifiedHistory {
        entry,
        _workspace: workspace,
    })
}

/// Rebuild one immutable historical Git visibility proof after full pack verification.
pub(crate) async fn rebuild_git_visibility_for_history(
    store: &Store,
    router: &StoreLayout,
    generation: u64,
    digest: &str,
    cancel: &CancellationToken,
) -> Result<()> {
    let verified = verify_history(store, router, generation, Some(digest), cancel).await?;
    let repository = verified._workspace.path().join("repository.git");
    crate::git::push::publish_git_visibility_index_from_git_dir(
        &repository,
        &verified.entry.manifest,
        store,
        router,
    )
    .await
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

pub(super) fn emit_verification(
    payload: &HistoryVerificationPayload,
    mode: OutputMode,
) -> Result<()> {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(HISTORY_VERIFY_SCHEMA, HISTORY_SCHEMA_VERSION, payload)?;
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
    Ok(())
}

pub(super) fn emit_prune(payload: &HistoryPrunePayload, mode: OutputMode) -> Result<()> {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(HISTORY_PRUNE_SCHEMA, HISTORY_SCHEMA_VERSION, payload)?;
        }
        OutputMode::Text => {
            let action = if payload.applied { "pruned" } else { "preview" };
            println!(
                "history prune {action}: before={} kept={} pruned={} manifest_bytes={} keep_last={}",
                payload.roots_before,
                payload.roots_kept,
                payload.roots_pruned,
                payload.manifest_bytes_pruned,
                payload.keep_last,
            );
            for entry in &payload.pruned {
                println!("{} {}", entry.generation, entry.digest);
            }
        }
    }
    Ok(())
}

pub(super) fn emit_restore(payload: &HistoryRestorePayload, mode: OutputMode) -> Result<()> {
    match mode {
        OutputMode::Json | OutputMode::Jsonl => {
            emit_json(HISTORY_RESTORE_SCHEMA, HISTORY_SCHEMA_VERSION, payload)?;
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
    Ok(())
}
