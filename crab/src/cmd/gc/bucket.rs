//! Bucket-scope garbage collection for globally deduplicated storage.
//!
//! Enumerates all content-addressed objects under `.crab/` and deletes
//! those not referenced by any repo in the ref-registry, subject to a
//! configurable grace period.
//!
//! Algorithm:
//! 1. Load ref-registry → compute `referenced_shards`.
//! 2. List `.crab/shards/` → unreferenced + expired = shard candidates.
//! 3. For each referenced shard, extract xorb hashes → `referenced_xorbs`.
//! 4. List `.crab/xorbs/` → unreferenced + expired = xorb candidates.
//! 5. Dry-run reports; otherwise delete candidates.
//!
//! The legacy `.crab/file-index/` enumeration is gone — per-file
//! objects don't exist anymore. Dead-entry tombstones in the per-repo
//! `file_index_db` are a future enhancement; for now the
//! content-addressed idempotency of SlateDB keys keeps any orphan
//! entries harmless until the GC sweep grows a tombstone pass.

use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use futures_util::TryStreamExt;
use object_store::ObjectStoreExt;
use object_store::path::Path as ObjectPath;
use tracing::{debug, info, warn};

use crate::coordination::cas::cas_update_default;
use crate::core::error::{CrabError, Result};
use crate::storage::StoreLayout;
use crate::storage::store::Store;
use crab_metadata::ref_registry::RefRegistry;
use crab_xet::hash::MerkleHash;
use crab_xet::shard::ShardReader;

/// CLI arguments for `crab gc --scope=bucket`.
#[derive(Debug, Clone)]
pub struct BucketGcArgs {
    /// S3 bucket name (e.g. `my-bucket`).
    pub bucket: String,
    /// Report candidates without deleting.
    pub dry_run: bool,
    /// Minimum age before an unreferenced object is eligible for deletion.
    pub grace_period: Duration,
    /// Bypass object age checks, but never registry or coordinator safety proof.
    pub force: bool,
}

/// Structured outcome of a bucket-scope GC run.
#[derive(Debug, Clone, Default)]
pub struct BucketGcOutcome {
    pub shards_deleted: u64,
    pub xorbs_deleted: u64,
    pub file_index_deleted: u64,
    pub bytes_reclaimed: u64,
    pub dry_run: bool,
}

impl BucketGcOutcome {
    pub fn log(&self) {
        if self.dry_run {
            info!(
                shards = self.shards_deleted,
                xorbs = self.xorbs_deleted,
                file_index = self.file_index_deleted,
                bytes = self.bytes_reclaimed,
                "bucket gc dry-run complete (no objects deleted)"
            );
        } else {
            info!(
                shards = self.shards_deleted,
                xorbs = self.xorbs_deleted,
                file_index = self.file_index_deleted,
                bytes = self.bytes_reclaimed,
                "bucket gc complete"
            );
        }
    }

    /// Convert bucket GC counters to the command's shared output schema.
    pub fn to_summary(&self) -> super::GcSummary {
        super::GcSummary {
            packs_deleted: 0,
            xorbs_deleted: self.xorbs_deleted,
            shards_deleted: self.shards_deleted,
            bytes_reclaimed: self.bytes_reclaimed,
            dry_run: self.dry_run,
            cancelled: false,
            partial_enumeration: false,
        }
    }
}

/// Minimum allowed grace period (1 hour).
const MIN_GRACE_PERIOD: Duration = Duration::from_secs(3600);

/// Global prefix for content-addressed objects.
const GLOBAL_PREFIX: &str = ".crab";

/// Run bucket-scope garbage collection.
///
/// Loads the ref-registry, computes reachable sets, lists all global
/// objects, and deletes (or reports) unreferenced objects past the grace
/// period.
pub async fn run_bucket_gc(
    args: &BucketGcArgs,
    store: &Store,
    coordinator_protected_keys: &HashSet<String>,
    coordinator_protected_repos: &HashSet<String>,
) -> Result<BucketGcOutcome> {
    let mut outcome = BucketGcOutcome {
        dry_run: args.dry_run,
        ..BucketGcOutcome::default()
    };

    let effective_grace = args.grace_period.max(MIN_GRACE_PERIOD);
    let now = SystemTime::now();
    let cutoff = now - effective_grace;

    // Step 1: Load ref-registry.
    let registry = load_ref_registry(store, args.force).await?;
    if !args.dry_run {
        ensure_registry_complete_for_destructive_gc(&registry)?;
        ensure_active_active_bucket_gc_proof(&registry, coordinator_protected_repos)?;
    }
    let mut referenced_shards = registry.all_referenced_shards();
    referenced_shards.extend(historical_referenced_shards(store, &registry).await?);
    info!(
        repos = registry.repos.len(),
        referenced_shards = referenced_shards.len(),
        "loaded ref-registry"
    );

    // Step 2: List shards, find unreferenced candidates.
    let shard_objects = list_global_objects(store, "shards").await?;
    let listed_shards = shard_objects
        .iter()
        .map(|object| extract_hash_from_key(&object.location))
        .collect::<HashSet<_>>();
    let mut missing_referenced_shards = referenced_shards
        .difference(&listed_shards)
        .cloned()
        .collect::<Vec<_>>();
    missing_referenced_shards.sort();
    if let Some(missing) = missing_referenced_shards.first() {
        return Err(CrabError::CorruptObject {
            path: format!("{GLOBAL_PREFIX}/shards/{missing}"),
            reason: format!(
                "ref-registry references {} missing shard object(s)",
                missing_referenced_shards.len()
            ),
        });
    }
    let ShardGcPartition {
        unreferenced: unreferenced_shards,
        referenced: referenced_shard_objects,
        protected_count: protected_shards,
    } = partition_shards_for_gc(
        shard_objects,
        &referenced_shards,
        coordinator_protected_keys,
    );

    let shard_candidates = filter_by_grace(unreferenced_shards, cutoff, args.force);
    debug!(
        shard_candidates = shard_candidates.len(),
        protected_shards, "unreferenced shards eligible for deletion"
    );

    // Step 3: Download each referenced shard once, in parallel, and
    // extract both xorb hashes (for step 4) and file hashes (for step 5)
    // in a single pass. See findings CR5-F3 and CR5-F4.
    let ShardHashes {
        xorb_hashes: referenced_xorbs,
        file_hashes: referenced_file_hashes,
    } = extract_hashes_from_shards(store, &referenced_shard_objects).await?;
    info!(
        referenced_xorbs = referenced_xorbs.len(),
        referenced_file_hashes = referenced_file_hashes.len(),
        "computed referenced xorbs + file-index entries from shards"
    );

    // Step 4: List xorbs, find unreferenced candidates.
    let xorb_objects = list_global_objects(store, "xorbs").await?;
    let xorb_partition =
        partition_xorbs_for_gc(xorb_objects, &referenced_xorbs, coordinator_protected_keys);
    let protected_xorbs = xorb_partition.protected_count;
    let unreferenced_xorbs = xorb_partition.unreferenced;
    let xorb_candidates = filter_by_grace(unreferenced_xorbs, cutoff, args.force);
    debug!(
        xorb_candidates = xorb_candidates.len(),
        protected_xorbs, "unreferenced xorbs eligible for deletion"
    );

    // Legacy per-file `.crab/file-index/{hash}` enumeration is gone —
    // file_index lives in the per-repo `file_index_db` SlateDB now.
    // Dead-entry tombstoning through a MetaDb `Transaction` is a
    // future enhancement; today, orphaned entries are harmless
    // (content-addressed keys) and get compacted away by SlateDB's
    // background compaction. `referenced_file_hashes` is still read
    // above so we can plug the tombstone pass in without touching the
    // shard-download path.
    let _ = &referenced_file_hashes;

    // Step 6: Delete or report.
    delete_or_report(
        store,
        "shards",
        &shard_candidates,
        args.dry_run,
        &mut outcome,
    )
    .await?;
    delete_or_report(store, "xorbs", &xorb_candidates, args.dry_run, &mut outcome).await?;

    outcome.log();
    Ok(outcome)
}

async fn historical_referenced_shards(
    store: &Store,
    registry: &RefRegistry,
) -> Result<HashSet<String>> {
    let storage = store.as_storage();
    let mut shards = HashSet::new();
    let mut repositories = registry.repos.keys().collect::<Vec<_>>();
    repositories.sort_unstable();
    for repo_prefix in repositories {
        let router = crab_storage::StoreLayout::new(storage.clone(), repo_prefix.clone());
        for entry in crab_metadata::manifest_store::list_manifest_history(storage, &router).await? {
            if entry.manifest.shard_index_hash.is_empty() {
                continue;
            }
            shards.extend(
                crab_metadata::manifest_store::read_bulk_shard_list(
                    storage,
                    &router,
                    &entry.manifest.shard_index_hash,
                )
                .await?,
            );
        }
    }
    Ok(shards)
}

fn ensure_registry_complete_for_destructive_gc(registry: &RefRegistry) -> Result<()> {
    if registry.is_complete_for_destructive_gc() {
        return Ok(());
    }
    Err(CrabError::Configuration {
        key: "gc.bucket.ref_registry_completeness".into(),
        origin: "destructive bucket garbage collection requires a schema-current ref-registry produced by a complete manifest backfill; run registry repair before retrying"
            .into(),
    })
}

fn ensure_active_active_bucket_gc_proof(
    registry: &RefRegistry,
    coordinator_protected_repos: &HashSet<String>,
) -> Result<()> {
    let missing = registry.active_active_repos_missing_gc_proof(coordinator_protected_repos);
    if missing.is_empty() {
        return Ok(());
    }
    Err(CrabError::Configuration {
        key: "gc.bucket.active_active_proof".into(),
        origin: format!(
            "bucket garbage collection requires coordinator GC safety snapshots for every active-active repo before deleting shared .crab/ objects; missing proof for {}",
            missing.join(", ")
        ),
    })
}

/// Load the ref-registry from `.crab/ref-registry`.
///
/// If the registry doesn't exist and `force` is false, returns an error
/// advising the user to use `--force`. If `force` is true, returns an
/// explicitly incomplete registry. Dry-run can inspect it, but destructive
/// GC still fails closed until a manifest backfill establishes coverage.
pub async fn load_ref_registry(store: &Store, force: bool) -> Result<RefRegistry> {
    let path = ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry"));
    match store.get_with_etag(&path).await {
        Ok((body, _etag)) => {
            let registry: RefRegistry =
                serde_json::from_slice(&body).map_err(|e| CrabError::CorruptObject {
                    path: format!("{GLOBAL_PREFIX}/ref-registry"),
                    reason: format!("invalid JSON: {e}"),
                })?;
            Ok(registry)
        }
        Err(CrabError::NotFound { .. }) => {
            if force {
                warn!("ref-registry not found; --force specified, treating as incomplete");
                let mut registry = RefRegistry::default();
                registry.schema_version = 0;
                Ok(registry)
            } else {
                Err(CrabError::NotFound {
                    path: format!(
                        "{GLOBAL_PREFIX}/ref-registry (use --force to proceed without registry)"
                    ),
                })
            }
        }
        Err(e) => Err(e),
    }
}

/// Metadata for a listed object.
#[derive(Debug, Clone)]
struct ListedObject {
    location: String,
    size: u64,
    last_modified: SystemTime,
}

struct ShardGcPartition {
    unreferenced: Vec<ListedObject>,
    referenced: Vec<ListedObject>,
    protected_count: usize,
}

fn partition_shards_for_gc(
    shard_objects: Vec<ListedObject>,
    referenced_shards: &HashSet<String>,
    coordinator_protected_keys: &HashSet<String>,
) -> ShardGcPartition {
    let mut unreferenced = Vec::new();
    let mut referenced = Vec::new();
    let mut protected_count = 0;

    for obj in shard_objects {
        if coordinator_protected_keys.contains(&obj.location) {
            protected_count += 1;
            referenced.push(obj);
            continue;
        }
        let hash = extract_hash_from_key(&obj.location);
        if referenced_shards.contains(&hash) {
            referenced.push(obj);
        } else {
            unreferenced.push(obj);
        }
    }

    ShardGcPartition {
        unreferenced,
        referenced,
        protected_count,
    }
}

struct XorbGcPartition {
    unreferenced: Vec<ListedObject>,
    protected_count: usize,
}

fn partition_xorbs_for_gc(
    xorb_objects: Vec<ListedObject>,
    referenced_xorbs: &HashSet<String>,
    coordinator_protected_keys: &HashSet<String>,
) -> XorbGcPartition {
    let mut unreferenced = Vec::new();
    let mut protected_count = 0;

    for obj in xorb_objects {
        if coordinator_protected_keys.contains(&obj.location) {
            protected_count += 1;
            continue;
        }
        let hash = extract_hash_from_key(&obj.location);
        if !referenced_xorbs.contains(&hash) {
            unreferenced.push(obj);
        }
    }

    XorbGcPartition {
        unreferenced,
        protected_count,
    }
}

/// List all objects under `.crab/{kind}/`.
async fn list_global_objects(store: &Store, kind: &str) -> Result<Vec<ListedObject>> {
    let prefix = ObjectPath::from(format!("{GLOBAL_PREFIX}/{kind}/"));
    let stream = store.inner().list(Some(&prefix));
    let objects: Vec<_> = stream.try_collect().await.map_err(CrabError::Storage)?;

    Ok(objects
        .into_iter()
        .map(|meta| ListedObject {
            location: meta.location.to_string(),
            size: meta.size,
            last_modified: meta.last_modified.into(),
        })
        .collect())
}

/// Extract the hash portion from a key like `.crab/shards/{hash}`.
fn extract_hash_from_key(key: &str) -> String {
    key.rsplit('/').next().unwrap_or("").to_string()
}

/// Filter objects by age unless the operator explicitly bypassed grace.
fn filter_by_grace(
    objects: Vec<ListedObject>,
    cutoff: SystemTime,
    force: bool,
) -> Vec<ListedObject> {
    if force {
        return objects;
    }
    objects
        .into_iter()
        .filter(|obj| obj.last_modified < cutoff)
        .collect()
}

/// Hashes extracted from a batch of shards.
struct ShardHashes {
    xorb_hashes: HashSet<String>,
    file_hashes: HashSet<String>,
}

/// Download each referenced shard once and extract both xorb hashes and
/// file hashes in a single pass. Runs downloads in parallel with a
/// bounded concurrency budget. Previously GC downloaded each shard
/// twice (once per extraction pass) and serially. See findings
/// CR5-F3 and CR5-F4.
async fn extract_hashes_from_shards(
    store: &Store,
    shard_objects: &[ListedObject],
) -> Result<ShardHashes> {
    use futures_util::stream::{self, StreamExt};

    const SHARD_DOWNLOAD_CONCURRENCY: usize = 16;

    let per_shard = stream::iter(shard_objects.iter())
        .map(|obj| async move {
            let hash_hex = extract_hash_from_key(&obj.location);
            let path = ObjectPath::from(obj.location.as_str());

            let data = store
                .inner()
                .get(&path)
                .await
                .map_err(CrabError::Storage)?
                .bytes()
                .await
                .map_err(CrabError::Storage)?;

            let hash =
                MerkleHash::from_hex(&hash_hex).map_err(|error| CrabError::CorruptObject {
                    path: obj.location.clone(),
                    reason: format!("invalid referenced shard hash: {error}"),
                })?;
            let actual_hash = crab_xet::hash::compute_data_hash(&data);
            if actual_hash != hash {
                return Err(CrabError::CorruptObject {
                    path: obj.location.clone(),
                    reason: format!(
                        "referenced shard content hash is {}, expected {}",
                        actual_hash.hex(),
                        hash.hex()
                    ),
                });
            }

            let reader = ShardReader::from_bytes(data, hash);
            let shard_info =
                reader
                    .shard_info_public()
                    .map_err(|error| CrabError::CorruptObject {
                        path: obj.location.clone(),
                        reason: format!("failed to parse referenced shard: {error}"),
                    })?;

            let v1_bytes = reader.v1_data();

            // First pass: xorb blocks.
            let mut xorbs: HashSet<String> = HashSet::new();
            let mut cursor = std::io::Cursor::new(v1_bytes);
            let blocks = shard_info
                .read_all_xorb_blocks_full(&mut cursor)
                .map_err(|error| CrabError::CorruptObject {
                    path: obj.location.clone(),
                    reason: format!("failed to read referenced shard xorb blocks: {error}"),
                })?;
            for xorb_info in &blocks {
                xorbs.insert(xorb_info.metadata.xorb_hash.hex());
            }

            // Second pass: file info (sequential but no re-download).
            let mut files: HashSet<String> = HashSet::new();
            let mut cursor = std::io::Cursor::new(v1_bytes);
            let file_infos = shard_info
                .read_all_file_info_sections(&mut cursor)
                .map_err(|error| CrabError::CorruptObject {
                    path: obj.location.clone(),
                    reason: format!("failed to read referenced shard file info: {error}"),
                })?;
            for file_info in &file_infos {
                files.insert(file_info.metadata.file_hash.hex());
            }

            Ok((xorbs, files))
        })
        .buffer_unordered(SHARD_DOWNLOAD_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;

    let mut xorb_hashes = HashSet::new();
    let mut file_hashes = HashSet::new();
    for (x, f) in per_shard {
        xorb_hashes.extend(x);
        file_hashes.extend(f);
    }

    Ok(ShardHashes {
        xorb_hashes,
        file_hashes,
    })
}

/// Delete or report candidates depending on dry-run mode.
async fn delete_or_report(
    store: &Store,
    kind: &str,
    candidates: &[ListedObject],
    dry_run: bool,
    outcome: &mut BucketGcOutcome,
) -> Result<()> {
    for obj in candidates {
        let hash = extract_hash_from_key(&obj.location);
        if dry_run {
            info!(kind = %kind, hash = %hash, size = obj.size, "would delete (dry-run)");
        } else {
            let path = ObjectPath::from(obj.location.as_str());
            match store.delete(&path).await {
                Ok(()) => {
                    debug!(kind = %kind, hash = %hash, "deleted");
                }
                Err(CrabError::NotFound { .. }) => {
                    // Already gone — idempotent.
                    debug!(kind = %kind, hash = %hash, "already deleted");
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }

        match kind {
            "shards" => outcome.shards_deleted += 1,
            "xorbs" => outcome.xorbs_deleted += 1,
            "file-index" => outcome.file_index_deleted += 1,
            _ => {}
        }
        outcome.bytes_reclaimed += obj.size;
    }
    Ok(())
}

/// Deregister a repo from the ref-registry.
///
/// Loads the current ref-registry via CAS, removes the repo's entry,
/// and writes back. After deregistration, the next bucket-scope GC run
/// will clean up objects exclusively referenced by that repo.
pub async fn deregister_repo(store: &Store, repo_prefix: &str) -> Result<()> {
    let registry_path = format!("{GLOBAL_PREFIX}/ref-registry");
    let updated: RefRegistry =
        cas_update_default(store, &registry_path, |reg: &mut RefRegistry| {
            let had_entry = reg.repos.contains_key(repo_prefix);
            reg.deregister(repo_prefix);
            reg.generation += 1;
            if had_entry {
                info!(repo = %repo_prefix, generation = reg.generation, "deregistered repo");
            } else {
                warn!(repo = %repo_prefix, "repo not found in ref-registry");
            }
        })
        .await?;

    info!(
        generation = updated.generation,
        remaining_repos = updated.repos.len(),
        "ref-registry updated"
    );
    Ok(())
}

/// Rebuild the bucket ref-registry from every discoverable repo manifest.
///
/// This is the explicit administrative proof required before destructive
/// bucket GC. Any unreadable manifest or shard index aborts the repair; a
/// partial scan is never marked complete.
pub async fn repair_ref_registry(store: &Store) -> Result<(usize, usize)> {
    use futures_util::StreamExt;

    let mut manifests = store.inner().list(None);
    let mut repo_prefixes = Vec::new();
    while let Some(item) = manifests.next().await {
        let meta = item.map_err(CrabError::from)?;
        let location = meta.location.as_ref();
        let Some(repo_prefix) = location.strip_suffix("/manifest") else {
            continue;
        };
        if repo_prefix.is_empty() || repo_prefix.starts_with(".crab/") {
            continue;
        }
        repo_prefixes.push(repo_prefix.to_owned());
    }
    repo_prefixes.sort();
    repo_prefixes.dedup();

    let mut repos = std::collections::HashMap::with_capacity(repo_prefixes.len());
    let mut shard_count = 0usize;
    for repo_prefix in &repo_prefixes {
        let router = StoreLayout::new(store.clone(), repo_prefix.clone());
        let (manifest, _) = crate::metadata::manifest::read_manifest(store, &router).await?;
        let shards = if manifest.shard_index_hash.is_empty() {
            Vec::new()
        } else {
            crate::metadata::manifest::read_bulk_shard_list(
                store,
                &router,
                &manifest.shard_index_hash,
            )
            .await?
        };
        shard_count = shard_count.checked_add(shards.len()).ok_or_else(|| {
            CrabError::Internal("ref-registry repair shard count overflow".to_owned())
        })?;
        repos.insert(repo_prefix.clone(), shards);
    }

    let storage = store.clone().into_storage();
    let router = crab_storage::StoreLayout::new(storage.clone(), String::new());
    crab_metadata::ref_registry::repair_ref_registry_from_manifests(&storage, &router, repos)
        .await?;
    info!(
        repos = repo_prefixes.len(),
        shards = shard_count,
        "ref-registry manifest backfill complete"
    );
    Ok((repo_prefixes.len(), shard_count))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use object_store::memory::InMemory;
    use std::sync::Arc;

    use crab_metadata::ref_registry::ActiveActiveCoordinatorRegistration;

    fn memory_store() -> Store {
        Store::new(Arc::new(InMemory::new()))
    }

    #[test]
    fn extract_hash_from_key_works() {
        assert_eq!(extract_hash_from_key(".crab/shards/abc123"), "abc123");
        assert_eq!(extract_hash_from_key(".crab/xorbs/def456"), "def456");
        assert_eq!(extract_hash_from_key(""), "");
    }

    #[test]
    fn filter_by_grace_retains_recent() {
        let cutoff = SystemTime::now() - Duration::from_secs(3600);
        let recent = ListedObject {
            location: ".crab/shards/abc".to_string(),
            size: 100,
            last_modified: SystemTime::now(),
        };
        let result = filter_by_grace(vec![recent], cutoff, false);
        assert!(result.is_empty());
    }

    #[test]
    fn filter_by_grace_passes_old() {
        let cutoff = SystemTime::now() - Duration::from_secs(3600);
        let old = ListedObject {
            location: ".crab/shards/abc".to_string(),
            size: 100,
            last_modified: SystemTime::now() - Duration::from_secs(7200),
        };
        let result = filter_by_grace(vec![old], cutoff, false);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn filter_by_grace_force_includes_recent() {
        let cutoff = SystemTime::now() - Duration::from_secs(3600);
        let recent = ListedObject {
            location: ".crab/xorbs/recent".to_owned(),
            size: 100,
            last_modified: SystemTime::now(),
        };

        let result = filter_by_grace(vec![recent], cutoff, true);

        assert_eq!(result.len(), 1);
    }

    #[tokio::test]
    async fn load_registry_missing_without_force_errors() {
        let store = memory_store();
        let result = load_ref_registry(&store, false).await;
        assert!(matches!(result, Err(CrabError::NotFound { .. })));
    }

    #[tokio::test]
    async fn load_registry_missing_with_force_returns_empty() {
        let store = memory_store();
        let reg = load_ref_registry(&store, true).await.unwrap();
        assert!(reg.repos.is_empty());
        assert_eq!(reg.generation, 0);
        assert_eq!(reg.schema_version, 0);
        assert!(!reg.is_complete_for_destructive_gc());
    }

    #[tokio::test]
    async fn load_registry_valid_json() {
        let store = memory_store();
        let reg = RefRegistry {
            generation: 5,
            repos: [("org/models".to_string(), vec!["aaa".to_string()])]
                .into_iter()
                .collect(),
            ..RefRegistry::default()
        };
        let body = serde_json::to_vec(&reg).unwrap();
        let path = ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry"));
        store.put(&path, Bytes::from(body)).await.unwrap();

        let loaded = load_ref_registry(&store, false).await.unwrap();
        assert_eq!(loaded.generation, 5);
        assert_eq!(loaded.repos.len(), 1);
    }

    #[tokio::test]
    async fn deregister_creates_registry_if_missing() {
        let store = memory_store();
        deregister_repo(&store, "org/old-repo").await.unwrap();

        let path = ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry"));
        let (body, _) = store.get_with_etag(&path).await.unwrap();
        let reg: RefRegistry = serde_json::from_slice(&body).unwrap();
        assert_eq!(reg.generation, 1);
        assert!(reg.repos.is_empty());
    }

    #[tokio::test]
    async fn deregister_removes_existing_repo() {
        let store = memory_store();
        let reg = RefRegistry {
            generation: 3,
            repos: [
                ("org/models".to_string(), vec!["aaa".to_string()]),
                ("org/datasets".to_string(), vec!["bbb".to_string()]),
            ]
            .into_iter()
            .collect(),
            ..RefRegistry::default()
        };
        let body = serde_json::to_vec(&reg).unwrap();
        let path = ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry"));
        store.put(&path, Bytes::from(body)).await.unwrap();

        deregister_repo(&store, "org/models").await.unwrap();

        let (body, _) = store.get_with_etag(&path).await.unwrap();
        let updated: RefRegistry = serde_json::from_slice(&body).unwrap();
        assert_eq!(updated.generation, 4);
        assert_eq!(updated.repos.len(), 1);
        assert!(!updated.repos.contains_key("org/models"));
        assert!(updated.repos.contains_key("org/datasets"));
    }

    #[tokio::test]
    async fn registry_repair_discovers_manifests_and_marks_coverage_complete() {
        let store = memory_store();
        for repo in ["org/a", "org/b"] {
            let router = StoreLayout::new(store.clone(), repo.to_owned());
            let manifest = crate::metadata::manifest::Manifest::default_for_repo("refs/heads/main");
            crate::metadata::manifest::create_manifest(&store, &router, &manifest)
                .await
                .unwrap();
        }

        let (repos, shards) = repair_ref_registry(&store).await.unwrap();

        assert_eq!((repos, shards), (2, 0));
        let registry = load_ref_registry(&store, false).await.unwrap();
        assert!(registry.is_complete_for_destructive_gc());
        assert!(registry.complete_repos.contains("org/a"));
        assert!(registry.complete_repos.contains("org/b"));
    }

    #[tokio::test]
    async fn bucket_gc_roots_include_history_only_shards() {
        use crate::metadata::manifest::{
            BulkData, Manifest, compact_pack_index, compact_shard_index, create_manifest,
            read_manifest, upload_segmented_bulk, write_manifest_cas,
        };

        let store = memory_store();
        let router = StoreLayout::new(store.clone(), "org/models".to_owned());
        let historical_shard = "a".repeat(64);
        let (old_shard_hash, _, old_shard_write) =
            compact_shard_index(1, std::slice::from_ref(&historical_shard)).unwrap();
        let (old_pack_hash, _, old_pack_write) = compact_pack_index(1, &[]).unwrap();
        upload_segmented_bulk(
            &store,
            &router,
            &BulkData {
                shard_index: old_shard_write,
                pack_index: old_pack_write,
            },
        )
        .await
        .unwrap();
        let mut old = Manifest::default_for_repo("refs/heads/main");
        old.generation = 1;
        old.shard_index_hash = old_shard_hash;
        old.pack_index_hash = old_pack_hash;
        old.seal_git_validation();
        create_manifest(&store, &router, &old).await.unwrap();
        let (_, etag) = read_manifest(&store, &router).await.unwrap();

        let (new_shard_hash, _, new_shard_write) = compact_shard_index(2, &[]).unwrap();
        let (new_pack_hash, _, new_pack_write) = compact_pack_index(2, &[]).unwrap();
        upload_segmented_bulk(
            &store,
            &router,
            &BulkData {
                shard_index: new_shard_write,
                pack_index: new_pack_write,
            },
        )
        .await
        .unwrap();
        let mut current = old.clone();
        current.generation = 2;
        current.shard_index_hash = new_shard_hash;
        current.pack_index_hash = new_pack_hash;
        current.seal_git_validation();
        write_manifest_cas(&store, &router, &current, &etag)
            .await
            .unwrap();
        let mut registry = RefRegistry::default();
        registry.register("org/models", Vec::new());

        let historical = historical_referenced_shards(&store, &registry)
            .await
            .unwrap();

        assert_eq!(historical, [historical_shard].into_iter().collect());
    }

    #[tokio::test]
    async fn dry_run_bucket_gc_with_empty_store() {
        let store = memory_store();
        // Put an empty registry so GC can proceed.
        let reg = RefRegistry::default();
        let body = serde_json::to_vec(&reg).unwrap();
        let path = ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry"));
        store.put(&path, Bytes::from(body)).await.unwrap();

        let args = BucketGcArgs {
            bucket: "test-bucket".to_string(),
            dry_run: true,
            grace_period: Duration::from_secs(3600),
            force: false,
        };

        let protected = HashSet::new();
        let protected_repos = HashSet::new();
        let outcome = run_bucket_gc(&args, &store, &protected, &protected_repos)
            .await
            .unwrap();
        assert!(outcome.dry_run);
        assert_eq!(outcome.shards_deleted, 0);
        assert_eq!(outcome.xorbs_deleted, 0);
        assert_eq!(outcome.file_index_deleted, 0);
    }

    #[tokio::test]
    async fn destructive_bucket_gc_preserves_recent_xorb_without_force() {
        let store = memory_store();
        let mut registry = RefRegistry::default();
        registry.mark_coverage_complete();
        let registry_path = ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry"));
        store
            .put(
                &registry_path,
                Bytes::from(serde_json::to_vec(&registry).unwrap()),
            )
            .await
            .unwrap();
        let xorb_path = ObjectPath::from(format!("{GLOBAL_PREFIX}/xorbs/{}", "a".repeat(64)));
        store
            .put(&xorb_path, Bytes::from_static(b"recent orphan"))
            .await
            .unwrap();

        let outcome = run_bucket_gc(
            &BucketGcArgs {
                bucket: "test-bucket".to_owned(),
                dry_run: false,
                grace_period: Duration::from_secs(3600),
                force: false,
            },
            &store,
            &HashSet::new(),
            &HashSet::new(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.xorbs_deleted, 0);
        store.get_with_etag(&xorb_path).await.unwrap();
    }

    #[tokio::test]
    async fn destructive_bucket_gc_force_deletes_recent_xorb() {
        let store = memory_store();
        let mut registry = RefRegistry::default();
        registry.mark_coverage_complete();
        let registry_path = ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry"));
        store
            .put(
                &registry_path,
                Bytes::from(serde_json::to_vec(&registry).unwrap()),
            )
            .await
            .unwrap();
        let xorb_path = ObjectPath::from(format!("{GLOBAL_PREFIX}/xorbs/{}", "b".repeat(64)));
        let xorb = Bytes::from_static(b"recent orphan");
        store.put(&xorb_path, xorb.clone()).await.unwrap();

        let outcome = run_bucket_gc(
            &BucketGcArgs {
                bucket: "test-bucket".to_owned(),
                dry_run: false,
                grace_period: Duration::from_secs(3600),
                force: true,
            },
            &store,
            &HashSet::new(),
            &HashSet::new(),
        )
        .await
        .unwrap();

        assert_eq!(outcome.xorbs_deleted, 1);
        assert_eq!(outcome.bytes_reclaimed, xorb.len() as u64);
        assert!(matches!(
            store.get_with_etag(&xorb_path).await,
            Err(CrabError::NotFound { .. })
        ));
        let summary = outcome.to_summary();
        assert_eq!(summary.packs_deleted, 0);
        assert_eq!(summary.xorbs_deleted, 1);
        assert_eq!(summary.shards_deleted, 0);
        assert_eq!(summary.bytes_reclaimed, xorb.len() as u64);
        assert!(!summary.dry_run);
        assert!(!summary.cancelled);
        assert!(!summary.partial_enumeration);
    }

    #[tokio::test]
    async fn destructive_gc_aborts_when_referenced_shard_is_corrupt() {
        let store = memory_store();
        let corrupt_shard = Bytes::from_static(b"not a shard");
        let shard_hash = crab_xet::hash::compute_data_hash(&corrupt_shard).hex();
        store
            .put(
                &ObjectPath::from(format!("{GLOBAL_PREFIX}/shards/{shard_hash}")),
                corrupt_shard,
            )
            .await
            .unwrap();

        let mut registry = RefRegistry::default();
        registry.register("org/models", vec![shard_hash]);
        registry.mark_coverage_complete();
        store
            .put(
                &ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry")),
                Bytes::from(serde_json::to_vec(&registry).unwrap()),
            )
            .await
            .unwrap();

        let error = run_bucket_gc(
            &BucketGcArgs {
                bucket: "test-bucket".to_owned(),
                dry_run: false,
                grace_period: Duration::from_secs(3600),
                force: false,
            },
            &store,
            &HashSet::new(),
            &HashSet::new(),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, CrabError::CorruptObject { .. }));
    }

    #[tokio::test]
    async fn destructive_gc_aborts_when_registry_root_is_missing() {
        let store = memory_store();
        let mut registry = RefRegistry::default();
        registry.register("org/models", vec!["a".repeat(64)]);
        registry.mark_coverage_complete();
        store
            .put(
                &ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry")),
                Bytes::from(serde_json::to_vec(&registry).unwrap()),
            )
            .await
            .unwrap();

        let error = run_bucket_gc(
            &BucketGcArgs {
                bucket: "test-bucket".to_owned(),
                dry_run: false,
                grace_period: Duration::from_secs(3600),
                force: false,
            },
            &store,
            &HashSet::new(),
            &HashSet::new(),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, CrabError::CorruptObject { .. }));
    }

    #[tokio::test]
    async fn bucket_gc_destructive_run_requires_proof_for_registered_active_active_repos() {
        let store = memory_store();
        let mut reg = RefRegistry::default();
        reg.mark_coverage_complete();
        reg.register_active_active_coordinator(
            "org/models",
            ActiveActiveCoordinatorRegistration {
                provider: "dynamodb".to_owned(),
                url: "dynamodb://crab-coordinator".to_owned(),
                region: "us-east-1".to_owned(),
                failover_regions: vec!["us-west-2".to_owned()],
            },
        );
        let body = serde_json::to_vec(&reg).unwrap();
        let registry_path = ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry"));
        store.put(&registry_path, Bytes::from(body)).await.unwrap();
        let args = BucketGcArgs {
            bucket: "test-bucket".to_string(),
            dry_run: false,
            grace_period: Duration::from_secs(3600),
            force: false,
        };
        let protected = HashSet::new();
        let protected_repos = HashSet::new();

        let err = run_bucket_gc(&args, &store, &protected, &protected_repos)
            .await
            .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("org/models"));
    }

    #[tokio::test]
    async fn bucket_gc_destructive_run_accepts_all_repo_coordinator_proof() {
        let store = memory_store();
        let mut reg = RefRegistry::default();
        reg.mark_coverage_complete();
        reg.register_active_active_coordinator(
            "org/models",
            ActiveActiveCoordinatorRegistration {
                provider: "dynamodb".to_owned(),
                url: "dynamodb://crab-coordinator".to_owned(),
                region: "us-east-1".to_owned(),
                failover_regions: Vec::new(),
            },
        );
        let body = serde_json::to_vec(&reg).unwrap();
        let registry_path = ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry"));
        store.put(&registry_path, Bytes::from(body)).await.unwrap();
        let args = BucketGcArgs {
            bucket: "test-bucket".to_string(),
            dry_run: false,
            grace_period: Duration::from_secs(3600),
            force: false,
        };
        let protected = HashSet::new();
        let protected_repos = ["org/models".to_owned()].into_iter().collect();

        let outcome = run_bucket_gc(&args, &store, &protected, &protected_repos)
            .await
            .unwrap();

        assert_eq!(outcome.shards_deleted, 0);
        assert_eq!(outcome.xorbs_deleted, 0);
    }

    #[tokio::test]
    async fn bucket_gc_destructive_run_rejects_incomplete_legacy_registry() {
        let store = memory_store();
        let legacy = br#"{"generation":1,"repos":{"org/models":["shard-a"]}}"#;
        let registry_path = ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry"));
        store
            .put(&registry_path, Bytes::from_static(legacy))
            .await
            .unwrap();
        let args = BucketGcArgs {
            bucket: "test-bucket".to_string(),
            dry_run: false,
            grace_period: Duration::from_secs(3600),
            force: true,
        };

        let err = run_bucket_gc(&args, &store, &HashSet::new(), &HashSet::new())
            .await
            .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(err.to_string().contains("ref-registry"));
    }

    #[test]
    fn bucket_gc_excludes_coordinator_protected_shared_objects() {
        let now = SystemTime::now();
        let shard_objects = vec![
            ListedObject {
                location: ".crab/shards/protected".to_owned(),
                size: 10,
                last_modified: now,
            },
            ListedObject {
                location: ".crab/shards/free".to_owned(),
                size: 20,
                last_modified: now,
            },
        ];
        let xorb_objects = vec![
            ListedObject {
                location: ".crab/xorbs/protected".to_owned(),
                size: 30,
                last_modified: now,
            },
            ListedObject {
                location: ".crab/xorbs/free".to_owned(),
                size: 40,
                last_modified: now,
            },
        ];
        let referenced = HashSet::new();
        let protected: HashSet<String> = [".crab/shards/protected", ".crab/xorbs/protected"]
            .into_iter()
            .map(str::to_owned)
            .collect();

        let shard_partition = partition_shards_for_gc(shard_objects, &referenced, &protected);
        assert_eq!(shard_partition.protected_count, 1);
        assert_eq!(shard_partition.unreferenced.len(), 1);
        assert_eq!(
            shard_partition.unreferenced[0].location,
            ".crab/shards/free"
        );
        assert_eq!(shard_partition.referenced.len(), 1);
        assert_eq!(
            shard_partition.referenced[0].location,
            ".crab/shards/protected"
        );

        let xorb_partition = partition_xorbs_for_gc(xorb_objects, &referenced, &protected);
        assert_eq!(xorb_partition.protected_count, 1);
        assert_eq!(xorb_partition.unreferenced.len(), 1);
        assert_eq!(xorb_partition.unreferenced[0].location, ".crab/xorbs/free");
    }

    /// Regression: the bucket GC walker MUST NOT enumerate or delete
    /// objects under `.crab/workflow/**`.
    ///
    /// The workflow push path (task 4.8) ships a fresh object
    /// namespace (`workflow/stages/…`, `workflow/exp/…`) that the
    /// current walker is intentionally blind to. This test seeds
    /// representative objects in that namespace, runs the GC with
    /// force=true (so a missing workflow entry in the registry
    /// doesn't error out) and dry_run=true (so we observe what
    /// *would* be deleted without needing to backdate
    /// `last_modified`), and asserts the workflow objects remain
    /// intact regardless of registry contents.
    ///
    /// If someone later adds a generic `list_global_objects(store, "*")`
    /// call to the walker without filtering, this test fails
    /// loudly: the workflow objects would show up in
    /// `bytes_reclaimed` / deletion counters.
    #[tokio::test]
    async fn bucket_gc_does_not_touch_workflow_objects() {
        let store = memory_store();

        // Empty ref-registry — worst case for the walker: every
        // workflow object is unreferenced by construction.
        let reg = RefRegistry::default();
        let body = serde_json::to_vec(&reg).unwrap();
        let registry_path = ObjectPath::from(format!("{GLOBAL_PREFIX}/ref-registry"));
        store.put(&registry_path, Bytes::from(body)).await.unwrap();

        // Seed representative workflow objects. Synthetic bytes
        // are fine — the walker never inspects object content,
        // only keys, so the test exercises key-matching behavior.
        let stage_key = ObjectPath::from(format!("{GLOBAL_PREFIX}/workflow/stages/ab/abcdef.json"));
        let meta_key = ObjectPath::from(format!(
            "{GLOBAL_PREFIX}/workflow/exp/01931b9e-4b3c-7b2a-b9f0-0123456789ab/meta.json"
        ));
        let stage_refs_key = ObjectPath::from(format!(
            "{GLOBAL_PREFIX}/workflow/exp/01931b9e-4b3c-7b2a-b9f0-0123456789ab/stage-refs.json"
        ));
        store
            .put(&stage_key, Bytes::from_static(b"{\"stage\": \"payload\"}"))
            .await
            .unwrap();
        store
            .put(&meta_key, Bytes::from_static(b"{\"exp\": \"payload\"}"))
            .await
            .unwrap();
        store
            .put(&stage_refs_key, Bytes::from_static(b"[\"deadbeef\"]"))
            .await
            .unwrap();

        // Run GC in dry-run mode. Nothing should be deleted (dry
        // run), and regardless of what *would* be deleted, the
        // workflow keys must not appear in any deletion counter.
        let args = BucketGcArgs {
            bucket: "test-bucket".to_string(),
            dry_run: true,
            grace_period: Duration::from_secs(3600),
            force: true,
        };
        let protected = HashSet::new();
        let protected_repos = HashSet::new();
        let outcome = run_bucket_gc(&args, &store, &protected, &protected_repos)
            .await
            .unwrap();
        assert!(outcome.dry_run);
        assert_eq!(
            outcome.shards_deleted, 0,
            "workflow stage entries must not be counted as shard deletions",
        );
        assert_eq!(
            outcome.xorbs_deleted, 0,
            "workflow stage entries must not be counted as xorb deletions",
        );
        assert_eq!(
            outcome.file_index_deleted, 0,
            "workflow stage entries must not be counted as file-index deletions",
        );
        assert_eq!(outcome.bytes_reclaimed, 0);

        // All three workflow objects survive — the walker never
        // saw them.
        assert!(
            store.get_with_etag(&stage_key).await.is_ok(),
            "workflow stage entry was touched by bucket GC",
        );
        assert!(
            store.get_with_etag(&meta_key).await.is_ok(),
            "workflow experiment meta was touched by bucket GC",
        );
        assert!(
            store.get_with_etag(&stage_refs_key).await.is_ok(),
            "workflow experiment stage-refs blob was touched by bucket GC",
        );
    }
}
