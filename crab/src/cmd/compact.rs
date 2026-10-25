//! Shard compaction: merge many small shards into fewer large ones.
//!
//! Pins one authenticated Capsule view, merges shards referenced by its pointer
//! catalog, verifies the replacement closure, and publishes an exact-root-CAS
//! checkpoint. Repositories without a Capsule root are rejected. Source shards
//! remain available for GC after publication.
//!
//! Global dedup shards can contain files owned by other repositories. Output
//! shards retain only authenticated files and their exact xorb dependencies.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bytes::Bytes;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::storage::store::Store;
use crab_xet::hash::{MerkleHash, compute_data_hash};
use crab_xet::shard::{MDBMinimalShard, MDBShardFile, merge_shards, new_shard_file_cache};
use xet_runtime::core::XetContext;

/// Default maximum compacted shard size (100 MiB).
pub const DEFAULT_MAX_SHARD_SIZE: u64 = 100 * 1024 * 1024;
const MAX_SOURCE_SHARD_BYTES: u64 = 512 * 1024 * 1024;
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
/// Requires a Capsule root and atomically publishes its replacement catalog.
pub async fn run_compact(args: &CompactArgs, store: &Store) -> Result<CompactOutcome> {
    run_compact_with_cancel(args, store, &CancellationToken::new()).await
}

/// Run compaction with the caller's cancellation boundary.
///
/// Await completion after cancellation to resolve any in-flight root publication.
pub async fn run_compact_with_cancel(
    args: &CompactArgs,
    store: &Store,
    cancel: &CancellationToken,
) -> Result<CompactOutcome> {
    validate_max_shard_size(args.max_shard_size)?;
    check_cancelled(cancel)?;
    let layout = crab_storage::StoreLayout::new(store.as_storage().clone(), args.repo.clone());
    let root = crab_metadata::capsule_protocol::load_root(&layout).await?;
    if args.dry_run {
        return Box::pin(compact_view(args, store, &layout, root, cancel)).await;
    }
    let writer = crate::maintenance::RepositoryMaintenanceLease::acquire(
        store,
        layout.global_prefix(),
        layout.repo_prefix(),
        cancel,
    )
    .await?;
    // Checkpoint publication checks cancellation before CAS, then resolves the
    // commit. Dropping that future could discard a remotely committed result.
    let operation = Box::pin(compact_view(args, store, &layout, root, cancel)).await;
    let release = writer.release().await;
    match (operation, release) {
        (Ok(outcome), Err(CrabError::Cancelled)) if outcome.compacted_shards > 0 => {
            // Release reports cancellation only after both fences are released.
            // A completed root CAS must retain its committed outcome.
            Ok(outcome)
        }
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

async fn compact_view(
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
    let catalog = view.pointer_catalog().await?;
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

    let compacted =
        prepare_compacted_shards(args, store, layout, &source_hashes, &selected_files, cancel)
            .await?;
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
    if !published.published {
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
    selected_files: &BTreeSet<String>,
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
        let selected_files = selected_files.clone();
        move || select_catalog_files(&merged, &filter_path, &selected_files)
    })
    .await
    .map_err(|error| CrabError::Internal(format!("select_catalog_files join error: {error}")))??;
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
                    reason: "authenticated catalog references a missing shard".to_owned(),
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

        // Authenticate the complete stored object above. Xet seeks its footer
        // from EOF, so private merge inputs must omit Crab's bloom trailer.
        let mut cursor = std::io::Cursor::new(crab_xet::shard_parse::strip_bloom_trailer(&data));
        MDBShardFile::write_out_from_reader(target_dir, &mut cursor, &shard_file_cache).map_err(
            |e| CrabError::Internal(format!("failed to write shard {hash_hex} to temp dir: {e}")),
        )?;

        debug!(shard = %hash_hex, size = data.len(), "downloaded shard");
    }
    Ok(())
}

// Global dedup shards may carry foreign file metadata. Rebuild from captured
// catalog membership so only its files and xorb dependencies become visible.
fn select_catalog_files(
    merged: &[Arc<MDBShardFile>],
    output_dir: &std::path::Path,
    selected_files: &BTreeSet<String>,
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
#[path = "compact_tests.rs"]
mod tests;
