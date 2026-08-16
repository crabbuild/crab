use std::collections::{HashMap, HashSet};
use std::path::Path;

use async_trait::async_trait;
use crab_staging::push_plan::{
    ExistingChunkLookup, LocalXorbCandidateLookup, PlannedPlacement, PlannedXorb, PreparedXorbCache,
};
use crab_xet::hash::MerkleHash;
use crab_xet::xorb::builder::XorbBuilder;
use crab_xet::xorb::format::XorbRef;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::core::error::{CrabError, Result};
use crate::git::push::PushConfig;
use crate::git::url::CrabUrl;
use crate::replication::StoreResolver;
use crab_staging::StagingArea;

const ADD_PLAN_REMOTE_LOOKUP_BATCH: usize = 4096;

pub(crate) type AddPlanFile<'a> = crab_staging::push_plan::AddPlanFile<'a>;
pub(crate) type AddPushPlanSummary = crab_staging::push_plan::AddPushPlanSummary;

struct LocalXorbLookup;

struct RemotePlanContext {
    guard: crate::metadata::MetaDbGuard,
}

pub(crate) async fn prepare_file_push_plans_with_progress(
    staging: &StagingArea,
    repo_root: &Path,
    files: &[AddPlanFile<'_>],
    cancel: &CancellationToken,
    on_progress: Option<&mut (dyn FnMut(&AddPushPlanSummary) + Send)>,
) -> Result<AddPushPlanSummary> {
    if files.is_empty() {
        return Ok(AddPushPlanSummary::default());
    }

    let config = crate::core::config::Config::resolve_local().unwrap_or_else(|e| {
        warn!(error = %e, "add push-plan: failed to load config, using defaults");
        crate::core::config::Config::default()
    });
    let push_config = PushConfig::from_config(&config);
    let remote = if should_lookup_remote_existing(files, push_config.min_xorb_size) {
        open_remote_plan_context(&config, repo_root, cancel).await
    } else {
        debug!(
            files = files.len(),
            min_xorb_size = push_config.min_xorb_size,
            "add push-plan: skipping remote chunk-index lookup for small add"
        );
        None
    };
    let local_lookup = LocalXorbLookup;
    let build_xorb_builder = || {
        let builder = XorbBuilder::with_policy(push_config.compression_policy());
        push_config.configure_builder(builder)
    };
    let result = crab_staging::push_plan::prepare_file_push_plans_with_progress(
        staging,
        files,
        &build_xorb_builder,
        remote
            .as_ref()
            .map(|remote| remote as &dyn ExistingChunkLookup),
        Some(&local_lookup),
        cancel,
        on_progress,
    )
    .await;

    if let Some(remote) = remote
        && let Err(e) = remote.guard.close().await
    {
        warn!(error = %e, "add push-plan: failed to close MetaDb guard");
    }

    result.map_err(CrabError::from)
}

fn should_lookup_remote_existing(files: &[AddPlanFile<'_>], min_xorb_size: u64) -> bool {
    files
        .iter()
        .map(|file| file.size)
        .try_fold(0u64, |total, size| total.checked_add(size))
        .is_none_or(|total| total >= min_xorb_size)
}

#[async_trait]
impl LocalXorbCandidateLookup for LocalXorbLookup {
    async fn load_candidates(
        &self,
        prepared_cache: &mut PreparedXorbCache,
        wanted_chunks: &HashSet<MerkleHash>,
    ) -> crab_staging::Result<()> {
        if wanted_chunks.is_empty() {
            return Ok(());
        }

        let cache = crate::cache::LocalCache::new(crate::cache::default_cache_root());
        let chunks: Vec<MerkleHash> = wanted_chunks.iter().copied().collect();
        let candidates = match cache.cached_xorb_candidates_for_chunks(&chunks).await {
            Ok(candidates) => candidates,
            Err(e) => {
                warn!(
                    error = %e,
                    "add push-plan: local xorb cache lookup failed; continuing without local xorb reuse"
                );
                return Ok(());
            }
        };

        let mut loaded = 0u64;
        for candidate in candidates {
            let planned = PlannedXorb {
                hash: candidate.xorb_hash.hex(),
                payload_hash: blake3::Hash::from(candidate.payload_hash)
                    .to_hex()
                    .to_string(),
                bytes: candidate.bytes,
                upload: true,
                placements: candidate
                    .placements
                    .iter()
                    .map(PlannedPlacement::from_placement)
                    .collect(),
            };
            if let Err(e) = prepared_cache.insert_cached_xorb(candidate.path, &planned) {
                debug!(
                    error = %e,
                    "add push-plan: local cached xorb candidate ignored"
                );
                continue;
            }
            loaded += 1;
        }
        if loaded > 0 {
            debug!(
                cached_xorbs = loaded,
                "add push-plan: loaded local cached xorb candidates"
            );
        }
        Ok(())
    }
}

#[async_trait]
impl ExistingChunkLookup for RemotePlanContext {
    async fn lookup_existing_candidates(
        &self,
        chunks: &[(MerkleHash, u64)],
    ) -> crab_staging::Result<Vec<Option<XorbRef>>> {
        let hashes = unique_chunk_hashes(chunks);
        let store = match self.guard.chunk_index().await {
            Ok(store) => store,
            Err(e) => {
                if e.is_metadb_read_only_uninitialized() {
                    debug!(
                        error = %e,
                        "add push-plan: chunk-index is empty; preparing chunks as new"
                    );
                    return Ok(vec![None; chunks.len()]);
                }
                warn!(error = %e, "add push-plan: chunk-index unavailable; preparing chunks as new");
                return Ok(vec![None; chunks.len()]);
            }
        };

        let mut refs_by_hash = HashMap::with_capacity(hashes.len());
        let mut hits = 0u64;
        let mut batches = 0u64;
        for batch in hashes.chunks(ADD_PLAN_REMOTE_LOOKUP_BATCH) {
            batches += 1;
            match store.get_batch(batch).await {
                Ok(refs) if refs.len() == batch.len() => {
                    hits += refs.iter().filter(|xorb_ref| xorb_ref.is_some()).count() as u64;
                    refs_by_hash.extend(batch.iter().copied().zip(refs));
                }
                Ok(refs) => {
                    warn!(
                        returned = refs.len(),
                        requested = batch.len(),
                        "add push-plan: chunk-index lookup returned wrong batch size; preparing chunks as new"
                    );
                    return Ok(vec![None; chunks.len()]);
                }
                Err(e) => {
                    warn!(error = %e, "add push-plan: chunk-index lookup failed; preparing chunks as new");
                    return Ok(vec![None; chunks.len()]);
                }
            }
        }
        debug!(
            requested = hashes.len(),
            hits, batches, "add push-plan: chunk-index lookup complete"
        );
        Ok(expand_existing_refs(chunks, &refs_by_hash))
    }
}

fn unique_chunk_hashes(chunks: &[(MerkleHash, u64)]) -> Vec<MerkleHash> {
    let mut seen = HashSet::new();
    let mut hashes = Vec::new();
    for (hash, _) in chunks {
        if seen.insert(*hash) {
            hashes.push(*hash);
        }
    }
    hashes
}

fn expand_existing_refs(
    chunks: &[(MerkleHash, u64)],
    refs_by_hash: &HashMap<MerkleHash, Option<XorbRef>>,
) -> Vec<Option<XorbRef>> {
    chunks
        .iter()
        .map(|(hash, _)| refs_by_hash.get(hash).copied().flatten())
        .collect()
}

async fn open_remote_plan_context(
    config: &crate::core::config::Config,
    repo_root: &Path,
    cancel: &CancellationToken,
) -> Option<RemotePlanContext> {
    let remote_name = crate::cmd::push::resolve_remote_name(None);
    let remote_url = match crate::cmd::push::git_config_value(&format!("remote.{remote_name}.url"))
    {
        Some(url) if url.starts_with("crab://") => url,
        _ => return None,
    };
    let parsed = match CrabUrl::parse(&remote_url) {
        Ok(parsed) => parsed,
        Err(e) => {
            warn!(error = %e, "add push-plan: failed to parse crab remote URL");
            return None;
        }
    };
    let selection = match StoreResolver::new(config, &parsed, cancel)
        .write_store("add push plan")
        .await
    {
        Ok(selection) => selection,
        Err(e) => {
            warn!(error = %e, "add push-plan: remote store unavailable; preparing chunks as new");
            return None;
        }
    };
    let router = selection.router;
    let metadb_object_store = crab_cache_store::CachingStore::try_build_healthy(
        selection.store.as_storage().clone(),
        &config.cache,
    )
    .await
    .map(|cache| cache.object_store());
    let guard = crate::git::push::build_push_metadb_guard_with_object_store(
        &selection.store,
        metadb_object_store,
        &router,
        None,
        &config.metadb,
        true,
    );
    debug!(
        repo = %router.repo_prefix(),
        root = %repo_root.display(),
        "add push-plan: opened remote chunk-index context"
    );
    Some(RemotePlanContext { guard })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::metadata::{MetaDb, MetaDbConfig, MetaDbGuard};
    use crab_metadata::receipts::{CommittedChunkReceipt, OriginReceipt, RECEIPT_SCHEMA_VERSION};
    use crab_xet::hash::compute_data_hash;
    use object_store::ObjectStore;
    use object_store::memory::InMemory;

    fn committed_receipt(
        chunk_hash: MerkleHash,
        xorb_ref: XorbRef,
        source_shard_byte: u8,
    ) -> CommittedChunkReceipt {
        CommittedChunkReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            chunk_hash: chunk_hash.into(),
            xorb_hash: xorb_ref.xorb_hash.into(),
            chunk_index: xorb_ref.chunk_index,
            uncompressed_size: xorb_ref.uncompressed_size,
            origin: OriginReceipt::new(
                "canonical-origin".to_owned(),
                format!(".crab/xorbs/{}", xorb_ref.xorb_hash.hex()),
                xorb_ref.xorb_hash.into(),
                [9; 32],
                1024,
                Some("etag".to_owned()),
                None,
            ),
            source_repo_prefix: "org/source-repo".to_owned(),
            source_shard_hash: [source_shard_byte; 32],
            committed_generation: 1,
            shard_index_hash: [0xC1; 32],
            gc_registry_generation: 1,
        }
    }

    #[test]
    fn unique_chunk_hashes_preserves_first_seen_order() {
        let first = compute_data_hash(b"first");
        let second = compute_data_hash(b"second");
        let third = compute_data_hash(b"third");
        let chunks = vec![(first, 5), (second, 6), (first, 5), (third, 5), (second, 6)];

        assert_eq!(unique_chunk_hashes(&chunks), vec![first, second, third]);
    }

    #[test]
    fn expand_existing_refs_restores_duplicate_chunk_positions() {
        let first = compute_data_hash(b"duplicate-existing-first");
        let second = compute_data_hash(b"duplicate-existing-second");
        let third = compute_data_hash(b"duplicate-existing-third");
        let first_ref = XorbRef {
            xorb_hash: MerkleHash::from([0xA1; 32]),
            chunk_index: 7,
            uncompressed_size: 24,
        };
        let third_ref = XorbRef {
            xorb_hash: MerkleHash::from([0xA3; 32]),
            chunk_index: 3,
            uncompressed_size: 24,
        };
        let chunks = vec![
            (first, 24),
            (second, 25),
            (first, 24),
            (third, 24),
            (second, 25),
        ];
        let refs_by_hash = HashMap::from([
            (first, Some(first_ref)),
            (second, None),
            (third, Some(third_ref)),
        ]);

        assert_eq!(
            expand_existing_refs(&chunks, &refs_by_hash),
            vec![
                Some(first_ref),
                None,
                Some(first_ref),
                Some(third_ref),
                None
            ]
        );
    }

    #[test]
    fn remote_existing_lookup_skips_small_add_batches() {
        let chunks = vec![(compute_data_hash(b"small chunk"), 512)];
        let file_hash: [u8; 32] = compute_data_hash(b"small file").into();
        let files = vec![AddPlanFile {
            file_hash,
            size: 512,
            chunks: &chunks,
        }];

        assert!(!should_lookup_remote_existing(&files, 1024));
    }

    #[test]
    fn remote_existing_lookup_uses_large_add_batches() {
        let chunks = vec![
            (compute_data_hash(b"first large chunk"), 1024),
            (compute_data_hash(b"second large chunk"), 1024),
        ];
        let file_hash: [u8; 32] = compute_data_hash(b"large file").into();
        let files = vec![AddPlanFile {
            file_hash,
            size: 2048,
            chunks: &chunks,
        }];

        assert!(should_lookup_remote_existing(&files, 1024));
    }

    #[tokio::test]
    async fn remote_lookup_treats_uninitialized_chunk_index_as_empty() {
        let backing: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let config = MetaDbConfig {
            local_chunk_index_path: cache_dir.path().join("chunk-index.sqlite"),
            read_only: true,
            ..MetaDbConfig::for_repo("org/new-repo")
        };
        let guard = MetaDbGuard::new(MetaDb::new(backing, String::from("org/new-repo"), config));
        let remote = RemotePlanContext { guard };
        let first = compute_data_hash(b"new bucket first chunk");
        let second = compute_data_hash(b"new bucket second chunk");

        let refs = remote
            .lookup_existing_candidates(&[(first, 11), (second, 12), (first, 11)])
            .await
            .expect("uninitialized chunk_index_db should behave as empty");

        assert_eq!(refs, vec![None, None, None]);
        remote.guard.close().await.expect("close guard");
    }

    #[tokio::test]
    async fn remote_lookup_uses_bucket_global_chunk_index_across_repos() {
        let backing: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let seed_cache = tempfile::tempdir().expect("seed cache");
        let reader_cache = tempfile::tempdir().expect("reader cache");
        let seed_config = MetaDbConfig {
            local_chunk_index_path: seed_cache.path().join("chunk-index.sqlite"),
            read_only: false,
            ..MetaDbConfig::for_repo("org/source-repo")
        };
        let seed_guard = MetaDbGuard::new(MetaDb::new(
            Arc::clone(&backing),
            String::from("org/source-repo"),
            seed_config,
        ));
        let first = compute_data_hash(b"shared bucket first chunk");
        let second = compute_data_hash(b"shared bucket second chunk");
        let missing = compute_data_hash(b"shared bucket missing chunk");
        let first_ref = XorbRef {
            xorb_hash: MerkleHash::from([0xB1; 32]),
            chunk_index: 4,
            uncompressed_size: 11,
        };
        let second_ref = XorbRef {
            xorb_hash: MerkleHash::from([0xB2; 32]),
            chunk_index: 9,
            uncompressed_size: 12,
        };
        let chunk_store = seed_guard.chunk_index().await.expect("seed chunk index");
        let mut txn = seed_guard.new_transaction().expect("transaction");
        chunk_store
            .save_committed_receipts(
                &mut txn,
                &[
                    (first, committed_receipt(first, first_ref, 0xD1)),
                    (second, committed_receipt(second, second_ref, 0xD2)),
                ],
            )
            .expect("seed committed receipts");
        let receipt = seed_guard.commit(txn).await.expect("seed chunk index");
        assert_eq!(receipt.chunk_ops_written, 8);
        seed_guard.close().await.expect("close seed guard");

        let reader_config = MetaDbConfig {
            local_chunk_index_path: reader_cache.path().join("chunk-index.sqlite"),
            read_only: true,
            ..MetaDbConfig::for_repo("org/target-repo")
        };
        let reader_guard = MetaDbGuard::new(MetaDb::new(
            Arc::clone(&backing),
            String::from("org/target-repo"),
            reader_config,
        ));
        let remote = RemotePlanContext {
            guard: reader_guard,
        };

        let refs = remote
            .lookup_existing_candidates(&[(first, 11), (missing, 99), (second, 12), (first, 11)])
            .await
            .expect("lookup across repos");

        assert_eq!(
            refs,
            vec![Some(first_ref), None, Some(second_ref), Some(first_ref)]
        );
        remote.guard.close().await.expect("close reader guard");
    }
}
