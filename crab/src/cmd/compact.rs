//! Shard compaction: merge many small shards into fewer large ones.
//!
//! Downloads all shards referenced by a repo's shard-list, merges them
//! using xet-core's `merge_shards()`, uploads the compacted shards to
//! `.crab/shards/{first-two-hex}/{new_hash}`, and CAS-updates the shard-list and
//! ref-registry. Source shards are left for GC.
//!
//! When shards contain xorb-info entries from other repos (cross-repo
//! global dedup), a post-merge filtering step uses
//! `MDBMinimalShard::serialize_xorb_subset_only()` to strip xorb entries
//! not referenced by any file-info entry, producing smaller output shards.

use std::collections::HashSet;
use std::sync::Arc;

use bytes::Bytes;
use object_store::path::Path as ObjectPath;
use tracing::{debug, info, warn};

use crate::coordination::cas::cas_update_default;
use crate::core::error::{CrabError, Result};
use crate::storage::store::Store;
use crab_metadata::manifests::ShardList;
use crab_metadata::ref_registry::RefRegistry;
use crab_storage::canonical_global_content_path;
use crab_xet::hash::{MerkleHash, compute_data_hash};
use crab_xet::shard::{
    MDBMinimalShard, MDBShardFile, merge_shards, new_shard_file_cache, shard_set_union,
};
use xet_runtime::core::XetContext;

/// Default maximum compacted shard size (100 MiB).
pub const DEFAULT_MAX_SHARD_SIZE: u64 = 100 * 1024 * 1024;

/// Global prefix for content-addressed objects.
const GLOBAL_PREFIX: &str = ".crab";

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
/// Downloads all shards from the repo's shard-list, merges them via
/// xet-core's `merge_shards()`, uploads the results, and CAS-updates
/// the shard-list and ref-registry.
pub async fn run_compact(args: &CompactArgs, store: &Store) -> Result<CompactOutcome> {
    let shard_list_path = format!("{}/manifests/shard-list", args.repo);

    // Step 1: Read the per-repo shard-list.
    let shard_list = read_shard_list(store, &shard_list_path).await?;
    let source_hashes: Vec<String> = shard_list.entries.clone();

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

    // Step 2: Download all shards to a temp directory.
    let source_dir = tempfile::tempdir().map_err(|e| {
        CrabError::Io(std::io::Error::new(
            e.kind(),
            format!("failed to create temp dir: {e}"),
        ))
    })?;
    let target_dir = tempfile::tempdir().map_err(|e| {
        CrabError::Io(std::io::Error::new(
            e.kind(),
            format!("failed to create temp dir: {e}"),
        ))
    })?;

    download_shards(store, &source_hashes, source_dir.path()).await?;

    // Step 3: Merge shards via xet-core.
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
    .map_err(|e| CrabError::Internal(format!("merge_shards join error: {e}")))?
    .map_err(|e| CrabError::Internal(format!("merge_shards failed: {e}")))?;

    let merged = merge_result.merged_shards;
    info!(
        merged_count = merged.len(),
        obsolete_count = merge_result.obsolete_shards.len(),
        "merge complete"
    );

    if merged.is_empty() {
        return Ok(CompactOutcome {
            source_shards: source_hashes.len(),
            compacted_shards: 0,
            dry_run: false,
        });
    }

    // Step 3b: Filter unreferenced xorbs from merged shards.
    // In the global-dedup layout, merged shards may carry xorb-info from
    // other repos. Strip those entries so the compacted output is lean.
    let filter_dir = tempfile::tempdir().map_err(|e| {
        CrabError::Io(std::io::Error::new(
            e.kind(),
            format!("failed to create filter temp dir: {e}"),
        ))
    })?;
    let filtered = tokio::task::spawn_blocking({
        let merged_clone = merged.clone();
        let filter_path = filter_dir.path().to_owned();
        move || filter_unreferenced_xorbs(&merged_clone, &filter_path)
    })
    .await
    .map_err(|e| CrabError::Internal(format!("filter_unreferenced_xorbs join error: {e}")))??;

    // Step 4: Upload merged shards to the canonical global shard namespace.
    let mut new_hashes: Vec<String> = Vec::with_capacity(filtered.len());
    for shard_file in &filtered {
        let hash_hex = shard_file.shard_hash.hex();
        let shard_path = canonical_global_content_path("shards", &hash_hex);

        let mut buf = Vec::new();
        shard_file
            .read_into_buffer(&mut buf)
            .map_err(|e| CrabError::Internal(format!("read merged shard: {e}")))?;

        // Verify hash before upload.
        let computed = compute_data_hash(&buf);
        if computed != shard_file.shard_hash {
            return Err(CrabError::CorruptObject {
                path: shard_path.to_string(),
                reason: format!(
                    "hash mismatch: expected {}, computed {}",
                    hash_hex,
                    computed.hex()
                ),
            });
        }

        debug!(hash = %hash_hex, size = buf.len(), "uploading compacted shard");
        store.put(&shard_path, Bytes::from(buf)).await?;
        new_hashes.push(hash_hex);
    }

    // Step 5: CAS-update the shard-list — replace source hashes with compacted.
    let source_set: HashSet<&str> = source_hashes.iter().map(String::as_str).collect();
    let new_hash_set: Vec<String> = new_hashes.clone();

    cas_update_default::<ShardList, _>(store, &shard_list_path, |list| {
        // Remove all source shard hashes and add the new compacted ones.
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

    // Step 6: Update the ref-registry to reflect the new shard set.
    let repo_prefix = args.repo.clone();
    let registry_path = format!("{GLOBAL_PREFIX}/ref-registry");

    // Re-read the shard-list to get the authoritative set after CAS.
    let updated_shard_list = read_shard_list(store, &shard_list_path).await?;
    let final_hashes = updated_shard_list.entries.clone();

    cas_update_default::<RefRegistry, _>(store, &registry_path, |reg| {
        reg.register(&repo_prefix, final_hashes.clone());
        reg.generation += 1;
        debug!(
            generation = reg.generation,
            repo = %repo_prefix,
            "updated ref-registry"
        );
    })
    .await?;

    let outcome = CompactOutcome {
        source_shards: source_hashes.len(),
        compacted_shards: filtered.len(),
        dry_run: false,
    };
    outcome.log();
    Ok(outcome)
}

/// Read the shard-list manifest from the store.
///
/// Returns a default (empty) list if the manifest does not exist yet.
async fn read_shard_list(store: &Store, path: &str) -> Result<ShardList> {
    let obj_path = ObjectPath::from(path);
    match store.get_with_etag(&obj_path).await {
        Ok((body, _etag)) => {
            let list: ShardList =
                serde_json::from_slice(&body).map_err(|e| CrabError::CorruptObject {
                    path: path.to_string(),
                    reason: format!("invalid shard-list JSON: {e}"),
                })?;
            Ok(list)
        }
        Err(CrabError::NotFound { .. }) => Ok(ShardList::default()),
        Err(e) => Err(e),
    }
}

/// Download all shards by hash into a local directory as `MDBShardFile` instances.
async fn download_shards(
    store: &Store,
    shard_hashes: &[String],
    target_dir: &std::path::Path,
) -> Result<()> {
    let shard_file_cache = new_shard_file_cache();
    for hash_hex in shard_hashes {
        let shard_path = canonical_global_content_path("shards", hash_hex);

        let data = match store.get_with_etag(&shard_path).await {
            Ok((body, _)) => body,
            Err(e) => {
                warn!(shard = %hash_hex, error = %e, "failed to download shard, skipping");
                continue;
            }
        };

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

    Ok(value * multiplier)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;
    use std::sync::Arc;

    fn memory_store() -> Store {
        Store::new(Arc::new(InMemory::new()))
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
                4096u32,
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
        let filtered = filter_unreferenced_xorbs(&[shard_file], output_dir.path()).unwrap();

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
                4096u32,
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

        let filtered = filter_unreferenced_xorbs(&[shard_file], output_dir.path()).unwrap();

        assert_eq!(filtered.len(), 1);
        // When all xorbs are referenced, the original shard is returned as-is.
        assert_eq!(filtered[0].shard_hash, original_hash);
    }
}
