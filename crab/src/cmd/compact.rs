//! Shard compaction: merge many small shards into fewer large ones.
//!
//! For capsule repositories, pins one authenticated view, merges every shard
//! referenced by its pointer catalog, verifies the replacement dependency
//! closure, and publishes the catalog in an exact-root-CAS checkpoint. Legacy
//! repositories retain the standalone shard-list publication path. Source
//! shards are left for GC in both formats.
//!
//! When shards contain xorb-info entries from other repos (cross-repo
//! global dedup), a post-merge filtering step uses
//! `MDBMinimalShard::serialize_xorb_subset_only()` to strip xorb entries
//! not referenced by any file-info entry. Capsule repositories additionally
//! rebuild each output from only the authenticated file set and its exact
//! xorb dependencies.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

use bytes::Bytes;
use object_store::path::Path as ObjectPath;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::coordination::cas::cas_update_default;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::storage::store::Store;
use crab_metadata::manifests::ShardList;
use crab_xet::hash::{MerkleHash, compute_data_hash};
use crab_xet::shard::{
    MDBMinimalShard, MDBShardFile, merge_shards, new_shard_file_cache, shard_set_union,
};
use xet_runtime::core::XetContext;

/// Default maximum compacted shard size (100 MiB).
pub const DEFAULT_MAX_SHARD_SIZE: u64 = 100 * 1024 * 1024;
const MAX_SOURCE_SHARD_BYTES: u64 = 512 * 1024 * 1024;
const MAX_SHARD_LIST_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SHARD_LIST_ENTRIES: usize = 1_000_000;
const MAX_CAPSULE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// CLI arguments for `crab compact`.
#[derive(Debug, Clone)]
pub struct CompactArgs {
    /// Repo prefix (e.g. `org/models`).
    pub repo: String,
    /// S3 bucket name.
    pub bucket: String,
    /// Report what would happen without mutating.
    pub dry_run: bool,
    /// Maximum size of a compacted shard in bytes.
    pub max_shard_size: u64,
}

/// Outcome of a compaction run.
#[derive(Debug, Clone, Default)]
pub struct CompactOutcome {
    /// Number of source shards that were merged.
    pub source_shards: usize,
    /// Number of compacted shards produced.
    pub compacted_shards: usize,
    /// Whether this was a dry-run.
    pub dry_run: bool,
}

impl CompactOutcome {
    pub fn log(&self) {
        if self.dry_run {
            info!(
                source_shards = self.source_shards,
                compacted_shards = self.compacted_shards,
                "compaction dry-run complete (no mutations)"
            );
        } else {
            info!(
                source_shards = self.source_shards,
                compacted_shards = self.compacted_shards,
                "compaction complete"
            );
        }
    }
}

/// Run shard compaction for a single repo.
///
/// Selects the repository authority, merges its authenticated shard inventory,
/// and atomically publishes the replacement metadata for that format.
pub async fn run_compact(args: &CompactArgs, store: &Store) -> Result<CompactOutcome> {
    run_compact_with_cancel(args, store, &CancellationToken::new()).await
}

/// Run compaction with the caller's cancellation boundary.
pub async fn run_compact_with_cancel(
    args: &CompactArgs,
    store: &Store,
    cancel: &CancellationToken,
) -> Result<CompactOutcome> {
    validate_max_shard_size(args.max_shard_size)?;
    check_cancelled(cancel)?;
    if args.dry_run {
        return run_compact_inner(args, store, cancel).await;
    }
    let layout = crab_storage::StoreLayout::new(store.as_storage().clone(), args.repo.clone());
    let writer = crate::maintenance::RepositoryMaintenanceLease::acquire(
        store,
        layout.global_prefix(),
        layout.repo_prefix(),
        cancel,
    )
    .await?;
    let operation = tokio::select! {
        biased;
        () = cancel.cancelled() => Err(CrabError::Cancelled),
        result = run_compact_inner(args, store, cancel) => result,
    };
    let release = writer.release().await;
    match (operation, release) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

async fn run_compact_inner(
    args: &CompactArgs,
    store: &Store,
    cancel: &CancellationToken,
) -> Result<CompactOutcome> {
    check_cancelled(cancel)?;
    let layout = crab_storage::StoreLayout::new(store.as_storage().clone(), args.repo.clone());
    match crab_metadata::capsule_protocol::load_root(&layout).await {
        Ok(root) => run_capsule_compact_inner(args, store, &layout, root, cancel).await,
        Err(crab_metadata::error::MetadataError::Storage {
            source: crab_storage::StorageError::NotFound { .. },
        }) => run_legacy_compact_inner(args, store, cancel).await,
        Err(error) => Err(error.into()),
    }
}

async fn run_legacy_compact_inner(
    args: &CompactArgs,
    store: &Store,
    cancel: &CancellationToken,
) -> Result<CompactOutcome> {
    let layout = crab_storage::StoreLayout::new(store.as_storage().clone(), args.repo.clone());
    let shard_list_path = layout.repo_path("manifests/shard-list").to_string();
    let shard_list = read_shard_list(store, &shard_list_path).await?;
    let source_hashes = shard_list.entries.clone();
    if source_hashes.is_empty() {
        info!(repo = %args.repo, "no shards to compact");
        return Ok(CompactOutcome {
            dry_run: args.dry_run,
            ..CompactOutcome::default()
        });
    }

    info!(
        repo = %args.repo,
        shard_count = source_hashes.len(),
        "read shard-list"
    );

    if args.dry_run {
        let outcome = CompactOutcome {
            source_shards: source_hashes.len(),
            compacted_shards: 0,
            dry_run: true,
        };
        info!(
            source_shards = outcome.source_shards,
            max_shard_size = args.max_shard_size,
            "would compact shards (dry-run)"
        );
        outcome.log();
        return Ok(outcome);
    }
    let compacted =
        prepare_compacted_shards(args, store, &layout, &source_hashes, None, cancel).await?;
    if compacted.is_empty() {
        return Ok(CompactOutcome {
            source_shards: source_hashes.len(),
            compacted_shards: 0,
            dry_run: false,
        });
    }
    let new_hashes = upload_compacted_shards(store, &layout, &compacted, cancel).await?;
    let source_set: HashSet<&str> = source_hashes.iter().map(String::as_str).collect();
    let new_hash_set: Vec<String> = new_hashes.clone();
    cas_update_default::<ShardList, _>(store, &shard_list_path, |list| {
        list.entries.retain(|h| !source_set.contains(h.as_str()));
        list.entries.extend(new_hash_set.clone());
        list.generation += 1;
        debug!(
            generation = list.generation,
            entries = list.entries.len(),
            "updated shard-list"
        );
    })
    .await?;
    let updated_shard_list = read_shard_list(store, &shard_list_path).await?;
    let final_hashes = updated_shard_list.entries.clone();
    let generation = crab_metadata::ref_registry::union_register_repo_shards(
        layout.store(),
        &layout,
        final_hashes,
    )
    .await?;
    debug!(generation, repo = %args.repo, "updated ref-registry");

    let outcome = CompactOutcome {
        source_shards: source_hashes.len(),
        compacted_shards: compacted.len(),
        dry_run: false,
    };
    outcome.log();
    Ok(outcome)
}

async fn run_capsule_compact_inner(
    args: &CompactArgs,
    store: &Store,
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    root: crab_metadata::capsule_protocol::RootSnapshot,
    cancel: &CancellationToken,
) -> Result<CompactOutcome> {
    let view = crab_read::capsule_protocol::open_view_from_root_with_control(
        layout,
        root,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: MAX_CAPSULE_BYTES,
            max_frontier_bytes: MAX_CAPSULE_BYTES,
        },
    )
    .await?;
    if view.refs().is_empty() {
        info!(repo = %args.repo, protocol = "capsule-v2", "no visible refs to compact");
        return Ok(CompactOutcome {
            dry_run: args.dry_run,
            ..CompactOutcome::default()
        });
    }
    let catalog = view.pointer_catalog()?;
    let source_hashes = catalog
        .files()
        .values()
        .map(|file| file.shard_hash().to_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let selected_files = catalog.files().keys().cloned().collect::<BTreeSet<_>>();
    if source_hashes.is_empty() {
        info!(repo = %args.repo, protocol = "capsule-v2", "no shards to compact");
        return Ok(CompactOutcome {
            dry_run: args.dry_run,
            ..CompactOutcome::default()
        });
    }
    if args.dry_run {
        let outcome = CompactOutcome {
            source_shards: source_hashes.len(),
            compacted_shards: 0,
            dry_run: true,
        };
        outcome.log();
        return Ok(outcome);
    }

    let compacted = prepare_compacted_shards(
        args,
        store,
        layout,
        &source_hashes,
        Some(&selected_files),
        cancel,
    )
    .await?;
    if compacted.is_empty() {
        return Ok(CompactOutcome {
            source_shards: source_hashes.len(),
            compacted_shards: 0,
            dry_run: false,
        });
    }
    let replacement = compacted_pointer_catalog(&catalog, &compacted)?;
    let new_hashes = upload_compacted_shards(store, layout, &compacted, cancel).await?;
    crab_read::verify_capsule_pointer_catalog_objects(layout, &replacement).await?;
    crab_metadata::ref_registry::union_register_repo_shards(layout.store(), layout, new_hashes)
        .await?;
    let published = crab_remote::checkpoint::publish_capsule_checkpoint_with_catalog_from_view(
        layout,
        &view,
        replacement,
        MAX_CAPSULE_BYTES,
        cancel,
    )
    .await
    .map_err(map_checkpoint_error)?;
    if !published {
        return Err(CrabError::CasConflict {
            path: layout.capsule_root_path().to_string(),
            expected_etag: None,
        });
    }
    let outcome = CompactOutcome {
        source_shards: source_hashes.len(),
        compacted_shards: compacted.len(),
        dry_run: false,
    };
    outcome.log();
    Ok(outcome)
}

struct PreparedCompactedShard {
    hash: MerkleHash,
    body: Bytes,
    files: Vec<String>,
    xorbs: Vec<String>,
}

async fn prepare_compacted_shards(
    args: &CompactArgs,
    store: &Store,
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    source_hashes: &[String],
    selected_files: Option<&BTreeSet<String>>,
    cancel: &CancellationToken,
) -> Result<Vec<PreparedCompactedShard>> {
    let source_dir = tempfile::tempdir().map_err(CrabError::Io)?;
    let target_dir = tempfile::tempdir().map_err(CrabError::Io)?;
    download_shards(store, layout, source_hashes, source_dir.path(), cancel).await?;
    let xet_context = XetContext::default().map_err(|error| {
        CrabError::Internal(format!("failed to initialize xet context: {error}"))
    })?;
    let runtime = Arc::clone(&xet_context.runtime);
    let shard_file_cache = new_shard_file_cache();
    let merge_result = tokio::task::spawn_blocking({
        let source_path = source_dir.path().to_owned();
        let target_path = target_dir.path().to_owned();
        let max_size = args.max_shard_size;
        move || {
            merge_shards(
                &runtime,
                source_path,
                target_path,
                max_size,
                false,
                &shard_file_cache,
            )
        }
    })
    .await
    .map_err(|error| CrabError::Internal(format!("merge_shards join error: {error}")))?
    .map_err(|error| CrabError::Internal(format!("merge_shards failed: {error}")))?;
    info!(
        merged_count = merge_result.merged_shards.len(),
        obsolete_count = merge_result.obsolete_shards.len(),
        "merge complete"
    );
    let filter_dir = tempfile::tempdir().map_err(CrabError::Io)?;
    let filtered = tokio::task::spawn_blocking({
        let merged = merge_result.merged_shards;
        let filter_path = filter_dir.path().to_owned();
        let selected_files = selected_files.cloned();
        move || filter_unreferenced_xorbs(&merged, &filter_path, selected_files.as_ref())
    })
    .await
    .map_err(|error| {
        CrabError::Internal(format!("filter_unreferenced_xorbs join error: {error}"))
    })??;
    filtered
        .into_iter()
        .map(|shard| inspect_compacted_shard(&shard))
        .collect()
}

fn inspect_compacted_shard(shard: &MDBShardFile) -> Result<PreparedCompactedShard> {
    let mut body = Vec::new();
    shard
        .read_into_buffer(&mut body)
        .map_err(|error| CrabError::Internal(format!("read merged shard: {error}")))?;
    let actual = compute_data_hash(&body);
    if actual != shard.shard_hash {
        return Err(CrabError::CorruptObject {
            path: format!("compacted shard {}", shard.shard_hash.hex()),
            reason: format!(
                "shard content hash is {actual}, expected {}",
                shard.shard_hash
            ),
        });
    }
    let parsed = MDBMinimalShard::from_reader(&mut std::io::Cursor::new(&body), true, true)
        .map_err(|error| CrabError::Internal(format!("parse compacted shard: {error}")))?;
    let mut files = (0..parsed.num_files())
        .filter_map(|index| parsed.file(index))
        .map(|file| file.file_hash().hex())
        .collect::<Vec<_>>();
    let mut xorbs = (0..parsed.num_xorb())
        .filter_map(|index| parsed.xorb(index))
        .map(|xorb| xorb.xorb_hash().hex())
        .collect::<Vec<_>>();
    files.sort_unstable();
    xorbs.sort_unstable();
    if files.windows(2).any(|pair| pair[0] == pair[1])
        || xorbs.windows(2).any(|pair| pair[0] == pair[1])
    {
        return Err(CrabError::CorruptObject {
            path: format!("compacted shard {}", shard.shard_hash.hex()),
            reason: "compacted shard contains duplicate file or xorb identities".to_owned(),
        });
    }
    Ok(PreparedCompactedShard {
        hash: shard.shard_hash,
        body: Bytes::from(body),
        files,
        xorbs,
    })
}

fn compacted_pointer_catalog(
    current: &crab_metadata::capsule_protocol::PointerCatalog,
    compacted: &[PreparedCompactedShard],
) -> Result<crab_metadata::capsule_protocol::PointerCatalog> {
    let mut file_shards = BTreeMap::new();
    for shard in compacted {
        for file in &shard.files {
            if file_shards.insert(file.clone(), shard.hash.hex()).is_some() {
                return Err(CrabError::CorruptObject {
                    path: "compacted shard set".to_owned(),
                    reason: format!("file {file} occurs in more than one compacted shard"),
                });
            }
        }
    }
    if file_shards.len() != current.files().len()
        || current
            .files()
            .keys()
            .any(|file| !file_shards.contains_key(file))
    {
        return Err(CrabError::CorruptObject {
            path: "compacted shard set".to_owned(),
            reason: "compacted shards do not cover every authenticated file exactly once"
                .to_owned(),
        });
    }
    let mut replacement = crab_metadata::capsule_protocol::PointerCatalog::new();
    for shard in compacted {
        for xorb in &shard.xorbs {
            let entry = current
                .xorbs()
                .get(xorb)
                .ok_or_else(|| CrabError::CorruptObject {
                    path: "compacted shard set".to_owned(),
                    reason: format!("compacted shard references absent xorb {xorb}"),
                })?;
            replacement.insert_xorb(xorb.clone(), entry.clone())?;
        }
        replacement.insert_shard(
            shard.hash.hex(),
            crab_metadata::capsule_protocol::ShardCatalogEntry::new(
                shard.body.len() as u64,
                shard.xorbs.clone(),
            ),
        )?;
    }
    for (file, entry) in current.files() {
        let shard = file_shards
            .get(file)
            .ok_or_else(|| CrabError::CorruptObject {
                path: "compacted shard set".to_owned(),
                reason: format!("compacted shard mapping lost file {file}"),
            })?;
        replacement.insert_file(
            file.clone(),
            crab_metadata::capsule_protocol::FileCatalogEntry::new(entry.size(), shard.clone()),
        )?;
    }
    replacement.encode()?;
    Ok(replacement)
}

async fn upload_compacted_shards(
    store: &Store,
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    compacted: &[PreparedCompactedShard],
    cancel: &CancellationToken,
) -> Result<Vec<String>> {
    let upload_dir = tempfile::tempdir().map_err(CrabError::Io)?;
    let mut hashes = Vec::with_capacity(compacted.len());
    for shard in compacted {
        check_cancelled(cancel)?;
        let hash = shard.hash.hex();
        let path = layout.shard_path(&shard.hash);
        let local_path = upload_dir.path().join(format!("upload-{hash}.shard"));
        tokio::fs::write(&local_path, &shard.body)
            .await
            .map_err(CrabError::Io)?;
        store
            .put_multipart_file_retry_with_xet_hash(
                &path,
                &local_path,
                shard.body.len() as u64,
                shard.hash.into(),
                8 * 1024 * 1024,
                cancel,
                None,
            )
            .await?;
        crate::cmd::gc::closure::publish(
            store,
            layout.global_prefix(),
            &shard.hash,
            shard.body.clone(),
            path.as_ref(),
        )
        .await?;
        hashes.push(hash);
    }
    Ok(hashes)
}

fn map_checkpoint_error(error: crab_remote::checkpoint::CheckpointError) -> CrabError {
    match error {
        crab_remote::checkpoint::CheckpointError::Cancelled => CrabError::Cancelled,
        crab_remote::checkpoint::CheckpointError::Read(source) => source.into(),
        crab_remote::checkpoint::CheckpointError::Repack(source) => source.into(),
        crab_remote::checkpoint::CheckpointError::Pack(source) => source.into(),
        crab_remote::checkpoint::CheckpointError::Metadata(source) => source.into(),
        crab_remote::checkpoint::CheckpointError::Write(source) => source.into(),
        crab_remote::checkpoint::CheckpointError::Io(source) => source.into(),
        other => CrabError::Internal(other.to_string()),
    }
}

/// Read the shard-list manifest from the store.
///
/// Returns a default (empty) list if the manifest does not exist yet.
async fn read_shard_list(store: &Store, path: &str) -> Result<ShardList> {
    let obj_path = ObjectPath::from(path);
    match store
        .get_with_etag_bounded(&obj_path, MAX_SHARD_LIST_BYTES)
        .await
    {
        Ok((body, _etag)) => {
            let list: ShardList =
                serde_json::from_slice(&body).map_err(|e| CrabError::CorruptObject {
                    path: path.to_string(),
                    reason: format!("invalid shard-list JSON: {e}"),
                })?;
            if list.entries.len() > MAX_SHARD_LIST_ENTRIES {
                return Err(CrabError::Configuration {
                    key: "compact shard-list entries".to_owned(),
                    origin: format!(
                        "shard-list contains {} entries; bounded compaction supports at most {MAX_SHARD_LIST_ENTRIES}",
                        list.entries.len()
                    ),
                });
            }
            Ok(list)
        }
        Err(CrabError::NotFound { .. }) => Ok(ShardList::default()),
        Err(e) => Err(e),
    }
}

/// Download all shards by hash into a local directory as `MDBShardFile` instances.
async fn download_shards(
    store: &Store,
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    shard_hashes: &[String],
    target_dir: &std::path::Path,
    cancel: &CancellationToken,
) -> Result<()> {
    let shard_file_cache = new_shard_file_cache();
    for hash_hex in shard_hashes {
        check_cancelled(cancel)?;
        let expected =
            MerkleHash::from_hex(hash_hex).map_err(|error| CrabError::CorruptObject {
                path: format!("shard identity {hash_hex}"),
                reason: format!("invalid shard hash: {error}"),
            })?;
        let shard_path = layout.shard_path(&expected);
        let (data, _) = store
            .get_with_etag_bounded(&shard_path, MAX_SOURCE_SHARD_BYTES)
            .await
            .map_err(|error| match error {
                CrabError::NotFound { .. } => CrabError::CorruptObject {
                    path: shard_path.to_string(),
                    reason: "shard-list references a missing shard".to_owned(),
                },
                error => error,
            })?;
        let actual = compute_data_hash(&data);
        if actual != expected {
            return Err(CrabError::CorruptObject {
                path: shard_path.to_string(),
                reason: format!("shard content hash is {actual}, expected {expected}"),
            });
        }

        // Write shard bytes to the temp directory via MDBShardFile.
        let mut cursor = std::io::Cursor::new(data.as_ref());
        MDBShardFile::write_out_from_reader(target_dir, &mut cursor, &shard_file_cache).map_err(
            |e| CrabError::Internal(format!("failed to write shard {hash_hex} to temp dir: {e}")),
        )?;

        debug!(shard = %hash_hex, size = data.len(), "downloaded shard");
    }
    Ok(())
}

/// Post-process merged shards to strip xorb-info entries not referenced by
/// any file-info entry. Returns filtered `MDBShardFile` handles, reusing the
/// originals when no filtering was needed.
///
/// In the global-dedup layout, a shard may carry xorb-info from other repos.
/// Stripping those entries keeps compacted shards lean and avoids downloading
/// irrelevant xorb metadata during future shard syncs.
fn filter_unreferenced_xorbs(
    merged: &[Arc<MDBShardFile>],
    output_dir: &std::path::Path,
    selected_files: Option<&BTreeSet<String>>,
) -> std::result::Result<Vec<Arc<MDBShardFile>>, CrabError> {
    let shard_file_cache = new_shard_file_cache();
    let mut result = Vec::with_capacity(merged.len());

    for shard_file in merged {
        let mut buf = Vec::new();
        shard_file
            .read_into_buffer(&mut buf)
            .map_err(|e| CrabError::Internal(format!("read merged shard: {e}")))?;

        // Parse with both file-info and xorb-info to determine referenced xorbs.
        let min_shard =
            MDBMinimalShard::from_reader(&mut std::io::Cursor::new(&buf), true, true)
                .map_err(|e| CrabError::Internal(format!("parse shard for filtering: {e}")))?;

        if let Some(selected_files) = selected_files {
            let selected = (0..min_shard.num_files())
                .filter_map(|index| min_shard.file(index))
                .filter(|file| selected_files.contains(&file.file_hash().hex()))
                .map(crab_xet::shard::MDBFileInfo::from)
                .collect::<Vec<_>>();
            if selected.is_empty() {
                continue;
            }
            let referenced = selected
                .iter()
                .flat_map(|file| file.segments.iter().map(|segment| segment.xorb_hash))
                .collect::<BTreeSet<_>>();
            let xorbs = (0..min_shard.num_xorb())
                .filter_map(|index| min_shard.xorb(index))
                .map(|xorb| {
                    let info = Arc::new(crab_xet::shard::MDBXorbInfo::from(xorb));
                    (info.metadata.xorb_hash, info)
                })
                .collect::<BTreeMap<_, _>>();
            let mut writer = crab_xet::shard::ShardWriter::new();
            for xorb in referenced {
                let info = xorbs.get(&xorb).ok_or_else(|| CrabError::CorruptObject {
                    path: format!("source shard {}", shard_file.shard_hash.hex()),
                    reason: format!("selected file references absent xorb {}", xorb.hex()),
                })?;
                writer.add_xorb(Arc::clone(info))?;
            }
            for file in selected {
                writer.add_file(file)?;
            }
            let (filtered, _) = writer.finalize()?;
            let filtered_handle = MDBShardFile::write_out_from_reader(
                output_dir,
                &mut std::io::Cursor::new(filtered),
                &shard_file_cache,
            )
            .map_err(|e| CrabError::Internal(format!("write selected-file shard: {e}")))?;
            result.push(filtered_handle);
            continue;
        }

        // Collect xorb hashes referenced by file entries.
        let mut referenced: HashSet<MerkleHash> = HashSet::new();
        for fi_idx in 0..min_shard.num_files() {
            if let Some(file_info) = min_shard.file(fi_idx) {
                for entry_idx in 0..file_info.num_entries() {
                    let entry = file_info.entry(entry_idx);
                    referenced.insert(entry.xorb_hash);
                }
            }
        }

        let total_xorbs = min_shard.num_xorb();
        let unreferenced_count = (0..total_xorbs)
            .filter(|&i| {
                min_shard
                    .xorb(i)
                    .is_some_and(|x| !referenced.contains(&x.xorb_hash()))
            })
            .count();

        if unreferenced_count == 0 {
            // All xorbs are referenced — keep the shard as-is.
            result.push(shard_file.clone());
            continue;
        }

        debug!(
            shard = %shard_file.shard_hash.hex(),
            total_xorbs,
            unreferenced_count,
            "filtering unreferenced xorbs from merged shard"
        );

        // Build a file-only shard (no xorb-info).
        let file_only_shard =
            MDBMinimalShard::from_reader(&mut std::io::Cursor::new(&buf), true, false)
                .map_err(|e| CrabError::Internal(format!("parse file-only shard: {e}")))?;
        let mut file_only_buf = Vec::new();
        file_only_shard
            .serialize(&mut file_only_buf, false)
            .map_err(|e| CrabError::Internal(format!("serialize file-only shard: {e}")))?;

        // Build an xorb-only shard with only the referenced xorbs.
        let mut xorb_only_buf = Vec::new();
        min_shard
            .serialize_xorb_subset_only(&mut xorb_only_buf, |xorb_view| {
                referenced.contains(&xorb_view.xorb_hash())
            })
            .map_err(|e| CrabError::Internal(format!("serialize xorb subset: {e}")))?;

        // Union the file-only and xorb-only shards into a single filtered shard.
        let file_only_handle = MDBShardFile::write_out_from_reader(
            output_dir,
            &mut std::io::Cursor::new(&file_only_buf),
            &shard_file_cache,
        )
        .map_err(|e| CrabError::Internal(format!("write file-only shard: {e}")))?;
        let xorb_only_handle = MDBShardFile::write_out_from_reader(
            output_dir,
            &mut std::io::Cursor::new(&xorb_only_buf),
            &shard_file_cache,
        )
        .map_err(|e| CrabError::Internal(format!("write xorb-only shard: {e}")))?;

        let mut combined_buf = Vec::new();
        let mut fo_data = Vec::new();
        file_only_handle
            .read_into_buffer(&mut fo_data)
            .map_err(|e| CrabError::Internal(format!("read file-only shard: {e}")))?;
        let mut xo_data = Vec::new();
        xorb_only_handle
            .read_into_buffer(&mut xo_data)
            .map_err(|e| CrabError::Internal(format!("read xorb-only shard: {e}")))?;

        shard_set_union(
            &file_only_handle.shard,
            &mut std::io::Cursor::new(&fo_data),
            &xorb_only_handle.shard,
            &mut std::io::Cursor::new(&xo_data),
            &mut combined_buf,
        )
        .map_err(|e| CrabError::Internal(format!("union file+xorb shards: {e}")))?;

        let filtered_handle = MDBShardFile::write_out_from_reader(
            output_dir,
            &mut std::io::Cursor::new(&combined_buf),
            &shard_file_cache,
        )
        .map_err(|e| CrabError::Internal(format!("write filtered shard: {e}")))?;

        debug!(
            original_size = buf.len(),
            filtered_size = combined_buf.len(),
            shard = %filtered_handle.shard_hash.hex(),
            "produced filtered shard"
        );

        result.push(filtered_handle);
    }

    Ok(result)
}

/// Parse a human-readable size string (e.g. `100MiB`, `50MB`, `1GiB`) into bytes.
///
/// Supports suffixes: `B`, `KiB`/`KB`, `MiB`/`MB`, `GiB`/`GB`.
/// A bare number is treated as bytes.
pub fn parse_size_str(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num_part, multiplier) = if let Some(n) = s.strip_suffix("GiB") {
        (n.trim(), 1024 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix("GB") {
        (n.trim(), 1_000_000_000)
    } else if let Some(n) = s.strip_suffix("MiB") {
        (n.trim(), 1024 * 1024)
    } else if let Some(n) = s.strip_suffix("MB") {
        (n.trim(), 1_000_000)
    } else if let Some(n) = s.strip_suffix("KiB") {
        (n.trim(), 1024)
    } else if let Some(n) = s.strip_suffix("KB") {
        (n.trim(), 1000)
    } else if let Some(n) = s.strip_suffix('B') {
        (n.trim(), 1)
    } else {
        (s, 1)
    };

    let value: u64 = num_part.parse().map_err(|_| CrabError::Configuration {
        key: format!("invalid size: {s}"),
        origin: "cli".into(),
    })?;

    value
        .checked_mul(multiplier)
        .ok_or_else(|| CrabError::Configuration {
            key: format!("invalid size: {s}"),
            origin: "size overflows u64".into(),
        })
}

fn validate_max_shard_size(value: u64) -> Result<()> {
    const MIN_SHARD_SIZE: u64 = 1024 * 1024;
    if !(MIN_SHARD_SIZE..=MAX_SOURCE_SHARD_BYTES).contains(&value) {
        return Err(CrabError::Configuration {
            key: "max_shard_size".to_owned(),
            origin: format!(
                "expected a value from {MIN_SHARD_SIZE} through {MAX_SOURCE_SHARD_BYTES} bytes"
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    use std::sync::Arc;

    fn memory_store() -> Store {
        Store::new(Arc::new(InMemory::new()))
    }

    fn git_pack_fixture() -> (String, crab_metadata::capsule_protocol::CapsuleGitPack) {
        let workspace = tempfile::tempdir().unwrap();
        let git_dir = workspace.path().join("repository.git");
        assert!(
            Command::new("git")
                .args(["init", "--bare", "--quiet"])
                .arg(&git_dir)
                .status()
                .unwrap()
                .success()
        );
        let mut hash = Command::new("git")
            .arg("--git-dir")
            .arg(&git_dir)
            .args(["hash-object", "-t", "tree", "-w", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        hash.stdin.take().unwrap().write_all(b"").unwrap();
        let tree = String::from_utf8(hash.wait_with_output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_owned();
        let mut commit = Command::new("git")
            .arg("--git-dir")
            .arg(&git_dir)
            .args(["commit-tree", &tree])
            .env("GIT_AUTHOR_NAME", "Crab Test")
            .env("GIT_AUTHOR_EMAIL", "crab@example.invalid")
            .env("GIT_AUTHOR_DATE", "@1 +0000")
            .env("GIT_COMMITTER_NAME", "Crab Test")
            .env("GIT_COMMITTER_EMAIL", "crab@example.invalid")
            .env("GIT_COMMITTER_DATE", "@1 +0000")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        commit.stdin.take().unwrap().write_all(b"commit\n").unwrap();
        let tip = String::from_utf8(commit.wait_with_output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_owned();
        assert!(
            Command::new("git")
                .arg("--git-dir")
                .arg(&git_dir)
                .args(["update-ref", "refs/heads/main", &tip])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .arg("--git-dir")
                .arg(&git_dir)
                .args(["repack", "-a", "-d", "--depth=64"])
                .status()
                .unwrap()
                .success()
        );
        let source_pack = std::fs::read_dir(git_dir.join("objects/pack"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "pack")
            })
            .unwrap();
        let pack_bytes = std::fs::read(&source_pack).unwrap();
        let canonical_id = blake3::hash(&pack_bytes).to_hex().to_string();
        let installed_dir = workspace.path().join("installed");
        std::fs::create_dir_all(&installed_dir).unwrap();
        let installed = crab_git::pack::install_pack_file_from_path(
            &installed_dir,
            &source_pack,
            &canonical_id,
            MAX_CAPSULE_BYTES,
            true,
        )
        .unwrap();
        let mut locations = crab_git::pack_locator::PackLocationIter::open(
            &installed.idx_path,
            &installed.rev_path,
            pack_bytes.len() as u64,
        )
        .unwrap();
        let object_count = locations.object_count();
        let object_ids = locations
            .by_ref()
            .map(|location| location.unwrap().oid)
            .collect::<Vec<_>>();
        let kinds = crab_git::pack::object_kinds_from_git_dir(&git_dir, &object_ids).unwrap();
        let ordered_kinds = object_ids
            .iter()
            .map(|oid| *kinds.get(oid).unwrap())
            .collect::<Vec<_>>();
        let checksum = gix_hash::ObjectId::from_hex(installed.git_sha1.as_bytes()).unwrap();
        let locator =
            crab_git::pack_locator::encode_pack_kind_metadata(checksum, &ordered_kinds).unwrap();
        let pack = crab_metadata::capsule_protocol::CapsuleGitPack::new(
            Bytes::from(pack_bytes),
            Bytes::from(std::fs::read(&installed.idx_path).unwrap()),
            Bytes::from(std::fs::read(&installed.rev_path).unwrap()),
            Bytes::from(locator),
            installed.git_sha1,
            object_count,
        )
        .unwrap();
        (tip, pack)
    }

    struct XetShardFixture {
        content_size: u64,
        file_hash: MerkleHash,
        xorb_hash: MerkleHash,
        xorb_body: Bytes,
        chunk_hash: MerkleHash,
        chunk_size: u32,
        shard_hash: MerkleHash,
        shard_body: Bytes,
    }

    fn xet_file_shard(byte: u8) -> XetShardFixture {
        use crab_xet::shard::{
            FileDataSequenceEntry, FileDataSequenceHeader, MDBFileInfo, MDBXorbInfo, ShardWriter,
            XorbChunkSequenceEntry, XorbChunkSequenceHeader,
        };
        use crab_xet::xorb::builder::{RunId, XorbBuilder};
        use crab_xet::xorb::format::Chunk;

        let content = Bytes::from(vec![byte; 1024]);
        let chunk = Chunk::new(content.clone());
        let mut builder = XorbBuilder::new();
        builder.push(&chunk, RunId(0)).unwrap();
        let xorb = builder.finalize().unwrap().remove(0);
        let mut writer = ShardWriter::new();
        writer
            .add_xorb(Arc::new(MDBXorbInfo {
                metadata: XorbChunkSequenceHeader::new(xorb.hash, 1, content.len()),
                chunks: vec![XorbChunkSequenceEntry::new(
                    chunk.hash,
                    content.len() as u32,
                    0,
                )],
            }))
            .unwrap();
        writer
            .add_file(MDBFileInfo {
                metadata: FileDataSequenceHeader::new(chunk.hash, 1, false, false),
                segments: vec![FileDataSequenceEntry::new(
                    xorb.hash,
                    content.len() as u32,
                    0,
                    1,
                )],
                verification: Vec::new(),
                metadata_ext: None,
            })
            .unwrap();
        let (bytes, hash) = writer.finalize().unwrap();
        XetShardFixture {
            content_size: content.len() as u64,
            file_hash: chunk.hash,
            xorb_hash: xorb.hash,
            xorb_body: xorb.bytes,
            chunk_hash: chunk.hash,
            chunk_size: content.len() as u32,
            shard_hash: hash,
            shard_body: Bytes::from(bytes),
        }
    }

    #[test]
    fn parse_size_mib() {
        assert_eq!(parse_size_str("100MiB").unwrap(), 100 * 1024 * 1024);
    }

    #[test]
    fn parse_size_mb() {
        assert_eq!(parse_size_str("50MB").unwrap(), 50_000_000);
    }

    #[test]
    fn parse_size_gib() {
        assert_eq!(parse_size_str("1GiB").unwrap(), 1024 * 1024 * 1024);
    }

    #[test]
    fn parse_size_bare_number() {
        assert_eq!(parse_size_str("4096").unwrap(), 4096);
    }

    #[test]
    fn parse_size_invalid_errors() {
        assert!(parse_size_str("notanumber").is_err());
    }

    #[tokio::test]
    async fn read_shard_list_missing_returns_default() {
        let store = memory_store();
        let list = read_shard_list(&store, "org/models/manifests/shard-list")
            .await
            .unwrap();
        assert_eq!(list.generation, 0);
        assert!(list.entries.is_empty());
    }

    #[tokio::test]
    async fn read_shard_list_valid_json() {
        let store = memory_store();
        let list = ShardList {
            generation: 3,
            entries: vec!["aaa".to_string(), "bbb".to_string()],
        };
        let body = serde_json::to_vec(&list).unwrap();
        let path = ObjectPath::from("org/models/manifests/shard-list");
        store.put(&path, Bytes::from(body)).await.unwrap();

        let loaded = read_shard_list(&store, "org/models/manifests/shard-list")
            .await
            .unwrap();
        assert_eq!(loaded.generation, 3);
        assert_eq!(loaded.entries.len(), 2);
    }

    #[tokio::test]
    async fn compact_empty_shard_list_is_noop() {
        let store = memory_store();
        let args = CompactArgs {
            repo: "org/models".to_string(),
            bucket: "test-bucket".to_string(),
            dry_run: false,
            max_shard_size: DEFAULT_MAX_SHARD_SIZE,
        };
        let outcome = run_compact(&args, &store).await.unwrap();
        assert_eq!(outcome.source_shards, 0);
        assert_eq!(outcome.compacted_shards, 0);
    }

    #[tokio::test]
    async fn compact_dry_run_does_not_mutate() {
        let store = memory_store();
        // Set up a shard-list with entries.
        let list = ShardList {
            generation: 1,
            entries: vec!["abc123".to_string(), "def456".to_string()],
        };
        let body = serde_json::to_vec(&list).unwrap();
        let path = ObjectPath::from("org/models/manifests/shard-list");
        store.put(&path, Bytes::from(body)).await.unwrap();

        let args = CompactArgs {
            repo: "org/models".to_string(),
            bucket: "test-bucket".to_string(),
            dry_run: true,
            max_shard_size: DEFAULT_MAX_SHARD_SIZE,
        };
        let outcome = run_compact(&args, &store).await.unwrap();
        assert!(outcome.dry_run);
        assert_eq!(outcome.source_shards, 2);

        // Shard-list should be unchanged.
        let after = read_shard_list(&store, "org/models/manifests/shard-list")
            .await
            .unwrap();
        assert_eq!(after.generation, 1);
        assert_eq!(after.entries.len(), 2);
    }

    #[tokio::test]
    async fn corrupt_capsule_root_never_falls_back_to_legacy_shard_list() {
        let store = memory_store();
        let repo = "org/corrupt-capsule-compact";
        let layout = crab_storage::StoreLayout::new(store.as_storage().clone(), repo.to_owned());
        store
            .put(
                &layout.capsule_root_path(),
                Bytes::from_static(b"corrupt root"),
            )
            .await
            .unwrap();
        let body = serde_json::to_vec(&ShardList {
            generation: 1,
            entries: vec![MerkleHash::from([1_u64; 4]).hex()],
        })
        .unwrap();
        store
            .put(&layout.repo_path("manifests/shard-list"), Bytes::from(body))
            .await
            .unwrap();

        let result = run_compact(
            &CompactArgs {
                repo: repo.to_owned(),
                bucket: "test-bucket".to_owned(),
                dry_run: true,
                max_shard_size: DEFAULT_MAX_SHARD_SIZE,
            },
            &store,
        )
        .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn capsule_dry_run_uses_authenticated_catalog_without_legacy_shard_list() {
        let store = memory_store();
        let layout = crab_storage::StoreLayout::new(
            store.as_storage().clone(),
            "org/capsule-compact".to_owned(),
        );
        let root =
            crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
                .await
                .unwrap();
        let xorb = MerkleHash::from([1_u64; 4]).hex();
        let shard = MerkleHash::from([2_u64; 4]).hex();
        let file = MerkleHash::from([3_u64; 4]).hex();
        let mut catalog = crab_metadata::capsule_protocol::PointerCatalog::new();
        catalog
            .insert_xorb(
                xorb.clone(),
                crab_metadata::capsule_protocol::XorbCatalogEntry::new(
                    1,
                    "4".repeat(64),
                    vec![crab_metadata::capsule_protocol::XorbChunkEntry::new(
                        "5".repeat(64),
                        1,
                    )],
                ),
            )
            .unwrap();
        catalog
            .insert_shard(
                shard.clone(),
                crab_metadata::capsule_protocol::ShardCatalogEntry::new(1, vec![xorb]),
            )
            .unwrap();
        catalog
            .insert_file(
                file,
                crab_metadata::capsule_protocol::FileCatalogEntry::new(1, shard),
            )
            .unwrap();
        let pack = crab_metadata::capsule_protocol::CapsuleGitPack::new(
            Bytes::from_static(b"pack"),
            Bytes::from_static(b"index"),
            Bytes::from_static(b"reverse"),
            Bytes::from_static(b"locator"),
            "6".repeat(40),
            1,
        )
        .unwrap();
        let checkpoint = crab_metadata::capsule_protocol::Checkpoint::build_with_catalogs(
            root.record().root().generation(),
            root.record().digest(),
            vec![pack],
            catalog,
            None,
        )
        .unwrap();
        let transaction_id = "7".repeat(64);
        let capsule = crab_metadata::capsule_protocol::CapsulePointer::new(
            "8".repeat(64),
            1,
            0,
            vec![transaction_id.clone()],
            root.record().digest(),
        )
        .unwrap();
        crab_write::capsule_protocol::publish_ref_checkpoint(
            &layout,
            root,
            &checkpoint,
            BTreeMap::from([("refs/heads/main".to_owned(), "9".repeat(40))]),
            BTreeMap::new(),
            BTreeMap::from([("refs/heads/main".to_owned(), transaction_id)]),
            vec![capsule],
        )
        .await
        .unwrap();

        let outcome = run_compact(
            &CompactArgs {
                repo: "org/capsule-compact".to_owned(),
                bucket: "test-bucket".to_owned(),
                dry_run: true,
                max_shard_size: DEFAULT_MAX_SHARD_SIZE,
            },
            &store,
        )
        .await
        .unwrap();

        assert!(outcome.dry_run);
        assert_eq!(outcome.source_shards, 1);
        assert!(matches!(
            store
                .get_with_etag(&ObjectPath::from(
                    "org/capsule-compact/manifests/shard-list"
                ))
                .await,
            Err(CrabError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn capsule_compaction_publishes_verified_checkpoint_without_legacy_metadata() {
        let store = memory_store().with_storage_scope(crab_types::storage::StorageScope {
            repo_prefix: "scoped/capsule-compact-apply".to_owned(),
            global_prefix: "scoped/capsule-compact-apply/.crab".to_owned(),
            source_repo: "org/capsule-compact-apply".to_owned(),
            scope_hash: "a".repeat(64),
        });
        let layout = crab_storage::StoreLayout::new(
            store.as_storage().clone(),
            "org/capsule-compact-apply".to_owned(),
        );
        let root =
            crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
                .await
                .unwrap();
        let fixture_a = xet_file_shard(11);
        let fixture_b = xet_file_shard(12);
        for fixture in [&fixture_a, &fixture_b] {
            layout
                .store()
                .put(
                    &layout.xorb_path(&fixture.xorb_hash),
                    fixture.xorb_body.clone(),
                )
                .await
                .unwrap();
            layout
                .store()
                .put(
                    &layout.shard_path(&fixture.shard_hash),
                    fixture.shard_body.clone(),
                )
                .await
                .unwrap();
        }
        let mut catalog = crab_metadata::capsule_protocol::PointerCatalog::new();
        for fixture in [&fixture_a, &fixture_b] {
            catalog
                .insert_xorb(
                    fixture.xorb_hash.hex(),
                    crab_metadata::capsule_protocol::XorbCatalogEntry::new(
                        fixture.xorb_body.len() as u64,
                        blake3::hash(&fixture.xorb_body).to_hex().to_string(),
                        vec![crab_metadata::capsule_protocol::XorbChunkEntry::new(
                            fixture.chunk_hash.hex(),
                            fixture.chunk_size,
                        )],
                    ),
                )
                .unwrap();
            catalog
                .insert_shard(
                    fixture.shard_hash.hex(),
                    crab_metadata::capsule_protocol::ShardCatalogEntry::new(
                        fixture.shard_body.len() as u64,
                        vec![fixture.xorb_hash.hex()],
                    ),
                )
                .unwrap();
            catalog
                .insert_file(
                    fixture.file_hash.hex(),
                    crab_metadata::capsule_protocol::FileCatalogEntry::new(
                        fixture.content_size,
                        fixture.shard_hash.hex(),
                    ),
                )
                .unwrap();
        }
        let (tip, pack) = git_pack_fixture();
        let transaction = crab_metadata::capsule_protocol::CapsuleTransaction::new(
            root.record().digest(),
            vec![crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                Some(tip.clone()),
                None,
            )],
        )
        .unwrap();
        let visibility =
            crab_metadata::capsule_protocol::CapsuleVisibilityDelta::new(BTreeMap::from([(
                "refs/heads/main".to_owned(),
                crab_metadata::git_visibility::GitVisibilityEdit::from_replacement_objects(
                    None,
                    tip.clone(),
                    vec![tip],
                ),
            )]))
            .unwrap();
        let capsule = crab_metadata::capsule_protocol::Capsule::build(
            &transaction,
            vec![pack],
            vec![
                crab_metadata::capsule_protocol::CapsuleSection::new(
                    crab_metadata::capsule_protocol::CapsuleSectionKind::CatalogDelta,
                    catalog.encode_delta().unwrap(),
                ),
                crab_metadata::capsule_protocol::CapsuleSection::new(
                    crab_metadata::capsule_protocol::CapsuleSectionKind::VisibilityDelta,
                    visibility.encode().unwrap(),
                ),
            ],
        )
        .unwrap();
        crab_write::capsule_protocol::publish(&layout, root, &transaction, &capsule)
            .await
            .unwrap();

        let outcome = run_compact(
            &CompactArgs {
                repo: "org/capsule-compact-apply".to_owned(),
                bucket: "test-bucket".to_owned(),
                dry_run: false,
                max_shard_size: 1024 * 1024,
            },
            &store,
        )
        .await
        .unwrap();

        assert_eq!(outcome.source_shards, 2);
        assert_eq!(outcome.compacted_shards, 1);
        let view = crab_read::capsule_protocol::open_view(
            &layout,
            crab_read::capsule_protocol::CapsuleReadLimits {
                max_capsule_bytes: MAX_CAPSULE_BYTES,
                max_frontier_bytes: MAX_CAPSULE_BYTES,
            },
        )
        .await
        .unwrap();
        let compacted = view.pointer_catalog().unwrap();
        assert_eq!(compacted.shards().len(), 1);
        assert_eq!(
            compacted
                .files()
                .get(&fixture_a.file_hash.hex())
                .unwrap()
                .shard_hash(),
            compacted
                .files()
                .get(&fixture_b.file_hash.hex())
                .unwrap()
                .shard_hash()
        );
        assert!(!compacted.shards().contains_key(&fixture_a.shard_hash.hex()));
        assert!(!compacted.shards().contains_key(&fixture_b.shard_hash.hex()));
        assert_eq!(compacted.xorbs().len(), 2);
        assert!(view.root().root().checkpoint().is_some());
        assert!(matches!(
            store
                .get_with_etag(&layout.repo_path("manifests/shard-list"))
                .await,
            Err(CrabError::NotFound { .. })
        ));
    }

    #[test]
    fn capsule_compaction_remaps_every_file_and_prunes_old_shards() {
        let old_shard_a = MerkleHash::from([1_u64; 4]).hex();
        let old_shard_b = MerkleHash::from([2_u64; 4]).hex();
        let new_shard = MerkleHash::from([3_u64; 4]);
        let file_a = MerkleHash::from([4_u64; 4]).hex();
        let file_b = MerkleHash::from([5_u64; 4]).hex();
        let xorb_a = MerkleHash::from([6_u64; 4]).hex();
        let xorb_b = MerkleHash::from([7_u64; 4]).hex();
        let mut current = crab_metadata::capsule_protocol::PointerCatalog::new();
        for (xorb, chunk, digest) in [
            (&xorb_a, "8".repeat(64), "9".repeat(64)),
            (&xorb_b, "a".repeat(64), "b".repeat(64)),
        ] {
            current
                .insert_xorb(
                    xorb.clone(),
                    crab_metadata::capsule_protocol::XorbCatalogEntry::new(
                        10,
                        digest,
                        vec![crab_metadata::capsule_protocol::XorbChunkEntry::new(
                            chunk, 10,
                        )],
                    ),
                )
                .unwrap();
        }
        current
            .insert_shard(
                old_shard_a.clone(),
                crab_metadata::capsule_protocol::ShardCatalogEntry::new(10, vec![xorb_a.clone()]),
            )
            .unwrap();
        current
            .insert_shard(
                old_shard_b.clone(),
                crab_metadata::capsule_protocol::ShardCatalogEntry::new(10, vec![xorb_b.clone()]),
            )
            .unwrap();
        current
            .insert_file(
                file_a.clone(),
                crab_metadata::capsule_protocol::FileCatalogEntry::new(10, old_shard_a.clone()),
            )
            .unwrap();
        current
            .insert_file(
                file_b.clone(),
                crab_metadata::capsule_protocol::FileCatalogEntry::new(20, old_shard_b.clone()),
            )
            .unwrap();
        let compacted = [PreparedCompactedShard {
            hash: new_shard,
            body: Bytes::from_static(b"compacted"),
            files: vec![file_a.clone(), file_b.clone()],
            xorbs: vec![xorb_a.clone(), xorb_b.clone()],
        }];

        let replacement = compacted_pointer_catalog(&current, &compacted).unwrap();

        assert_eq!(replacement.shards().len(), 1);
        assert!(!replacement.shards().contains_key(&old_shard_a));
        assert!(!replacement.shards().contains_key(&old_shard_b));
        assert_eq!(
            replacement.files().get(&file_a).unwrap().shard_hash(),
            new_shard.hex()
        );
        assert_eq!(
            replacement.files().get(&file_b).unwrap().shard_hash(),
            new_shard.hex()
        );
        assert_eq!(replacement.xorbs().len(), 2);
    }

    #[test]
    fn filter_strips_unreferenced_xorbs() {
        use crab_xet::shard::ShardWriter;
        use crab_xet::shard::{
            FileDataSequenceEntry, FileDataSequenceHeader, MDBFileInfo, MDBXorbInfo,
            XorbChunkSequenceEntry, XorbChunkSequenceHeader,
        };

        let make_xorb = |seed: u64, n: usize| -> Arc<MDBXorbInfo> {
            let h = MerkleHash::from([seed, seed, seed, seed]);
            let chunks: Vec<XorbChunkSequenceEntry> = (0..n)
                .map(|i| {
                    let ch = seed.wrapping_add(i as u64 + 1);
                    XorbChunkSequenceEntry::new(
                        MerkleHash::from([ch, ch, ch, ch]),
                        1024u32,
                        (i as u32) * 1024,
                    )
                })
                .collect();
            Arc::new(MDBXorbInfo {
                metadata: XorbChunkSequenceHeader::new(h, n, n * 1024),
                chunks,
            })
        };

        // Build a shard with 2 xorbs but only 1 referenced by a file entry.
        let referenced_xorb = make_xorb(1, 2);
        let unreferenced_xorb = make_xorb(2, 3);

        let file_info = MDBFileInfo {
            metadata: FileDataSequenceHeader::new(
                MerkleHash::from([10u64, 10, 10, 10]),
                1u32,
                false,
                false,
            ),
            segments: vec![FileDataSequenceEntry::new(
                MerkleHash::from([1u64, 1, 1, 1]), // references xorb seed=1
                1024u32,
                0u32,
                1u32,
            )],
            verification: vec![],
            metadata_ext: None,
        };

        let mut writer = ShardWriter::new();
        writer.add_xorb(referenced_xorb).unwrap();
        writer.add_xorb(unreferenced_xorb).unwrap();
        writer.add_file(file_info).unwrap();
        let (shard_bytes, _hash) = writer.finalize().unwrap();

        // Write shard to a temp dir and load as MDBShardFile.
        let source_dir = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();
        let shard_file_cache = new_shard_file_cache();
        let shard_file = MDBShardFile::write_out_from_reader(
            source_dir.path(),
            &mut std::io::Cursor::new(&shard_bytes),
            &shard_file_cache,
        )
        .unwrap();

        // Verify the original shard has 2 xorbs.
        let original =
            MDBMinimalShard::from_reader(&mut std::io::Cursor::new(&shard_bytes), true, true)
                .unwrap();
        assert_eq!(original.num_xorb(), 2);
        assert_eq!(original.num_files(), 1);

        // Run the filter.
        let filtered = filter_unreferenced_xorbs(&[shard_file], output_dir.path(), None).unwrap();

        assert_eq!(filtered.len(), 1);

        // Parse the filtered shard and verify only the referenced xorb remains.
        let mut filtered_buf = Vec::new();
        filtered[0].read_into_buffer(&mut filtered_buf).unwrap();
        let filtered_shard =
            MDBMinimalShard::from_reader(&mut std::io::Cursor::new(&filtered_buf), true, true)
                .unwrap();

        assert_eq!(
            filtered_shard.num_xorb(),
            1,
            "should have only the referenced xorb"
        );
        assert_eq!(
            filtered_shard.xorb(0).unwrap().xorb_hash(),
            MerkleHash::from([1u64, 1, 1, 1]),
            "remaining xorb should be the referenced one"
        );
        assert_eq!(
            filtered_shard.num_files(),
            1,
            "file-info should be preserved"
        );
    }

    #[test]
    fn capsule_filter_keeps_only_authenticated_files_and_dependencies() {
        use crab_xet::shard::ShardWriter;

        let retained = xet_file_shard(31);
        let foreign = xet_file_shard(32);
        let mut writer = ShardWriter::new();
        for fixture in [&retained, &foreign] {
            let parsed = MDBMinimalShard::from_reader(
                &mut std::io::Cursor::new(&fixture.shard_body),
                true,
                true,
            )
            .unwrap();
            writer
                .add_xorb(Arc::new(crab_xet::shard::MDBXorbInfo::from(
                    parsed.xorb(0).unwrap(),
                )))
                .unwrap();
            writer
                .add_file(crab_xet::shard::MDBFileInfo::from(parsed.file(0).unwrap()))
                .unwrap();
        }
        let (body, _) = writer.finalize().unwrap();
        let source_dir = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();
        let shard_file = MDBShardFile::write_out_from_reader(
            source_dir.path(),
            &mut std::io::Cursor::new(body),
            &new_shard_file_cache(),
        )
        .unwrap();

        let filtered = filter_unreferenced_xorbs(
            &[shard_file],
            output_dir.path(),
            Some(&BTreeSet::from([retained.file_hash.hex()])),
        )
        .unwrap();

        assert_eq!(filtered.len(), 1);
        let mut body = Vec::new();
        filtered[0].read_into_buffer(&mut body).unwrap();
        let parsed =
            MDBMinimalShard::from_reader(&mut std::io::Cursor::new(body), true, true).unwrap();
        assert_eq!(parsed.num_files(), 1);
        assert_eq!(parsed.file(0).unwrap().file_hash(), retained.file_hash);
        assert_eq!(parsed.num_xorb(), 1);
        assert_eq!(parsed.xorb(0).unwrap().xorb_hash(), retained.xorb_hash);
    }

    #[test]
    fn filter_noop_when_all_xorbs_referenced() {
        use crab_xet::shard::ShardWriter;
        use crab_xet::shard::{
            FileDataSequenceEntry, FileDataSequenceHeader, MDBFileInfo, MDBXorbInfo,
            XorbChunkSequenceEntry, XorbChunkSequenceHeader,
        };

        let xorb = Arc::new(MDBXorbInfo {
            metadata: XorbChunkSequenceHeader::new(MerkleHash::from([1u64, 1, 1, 1]), 1, 1024),
            chunks: vec![XorbChunkSequenceEntry::new(
                MerkleHash::from([2u64, 2, 2, 2]),
                1024u32,
                0u32,
            )],
        });

        let file_info = MDBFileInfo {
            metadata: FileDataSequenceHeader::new(
                MerkleHash::from([10u64, 10, 10, 10]),
                1u32,
                false,
                false,
            ),
            segments: vec![FileDataSequenceEntry::new(
                MerkleHash::from([1u64, 1, 1, 1]),
                1024u32,
                0u32,
                1u32,
            )],
            verification: vec![],
            metadata_ext: None,
        };

        let mut writer = ShardWriter::new();
        writer.add_xorb(xorb).unwrap();
        writer.add_file(file_info).unwrap();
        let (shard_bytes, _hash) = writer.finalize().unwrap();

        let source_dir = tempfile::tempdir().unwrap();
        let output_dir = tempfile::tempdir().unwrap();
        let shard_file_cache = new_shard_file_cache();
        let shard_file = MDBShardFile::write_out_from_reader(
            source_dir.path(),
            &mut std::io::Cursor::new(&shard_bytes),
            &shard_file_cache,
        )
        .unwrap();

        let original_hash = shard_file.shard_hash;

        let filtered = filter_unreferenced_xorbs(&[shard_file], output_dir.path(), None).unwrap();

        assert_eq!(filtered.len(), 1);
        // When all xorbs are referenced, the original shard is returned as-is.
        assert_eq!(filtered[0].shard_hash, original_hash);
    }
}
