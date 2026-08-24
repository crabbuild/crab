//! Partitioned ref registry for bucket-wide GC roots.
//!
//! Each repository owns a small CAS coordination record beneath
//! `.crab/ref-registry/records`. Its shard roots are independently CAS-sharded
//! across deterministic four-hex partitions, so push cost and contention do
//! not grow with the bucket or require rewriting one large repository root
//! set. A coverage marker is published only by exclusive repair.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

#[cfg(feature = "storage")]
use crate::error::{MetadataError, Result};
#[cfg(feature = "storage")]
use crab_storage::{Store, StoreLayout};
#[cfg(feature = "storage")]
use futures_util::TryStreamExt;
#[cfg(feature = "storage")]
use object_store::path::Path;

/// Coordinator metadata for a repo that uses active-active writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveActiveCoordinatorRegistration {
    pub provider: String,
    pub url: String,
    pub region: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failover_regions: Vec<String>,
}

/// Maps repo prefixes to their referenced shard hash sets.
///
/// Each repo's entry contains the complete list of shard hashes from its
/// current shard-list. The `generation` counter is bumped on every CAS
/// write, providing a total ordering of registry versions.
///
/// The workflow-related fields (`workflow_stage_hashes`,
/// `workflow_experiment_ids`) are `#[serde(default)]` so older on-disk
/// payloads written before the workflow layer shipped deserialize cleanly
/// into empty maps. Writers always emit the fields, even when empty; the
/// serde default handles readers that see an older payload.
pub const REF_REGISTRY_SCHEMA_VERSION: u32 = 1;
pub const REF_REGISTRY_RECORD_SCHEMA_VERSION: u32 = 2;
const REF_REGISTRY_ROOT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepoRefRecord {
    schema_version: u32,
    repo_prefix: String,
    generation: u64,
    complete: bool,
    workflow_stage_hashes: Vec<String>,
    workflow_experiment_ids: Vec<String>,
    active_active_coordinator: Option<ActiveActiveCoordinatorRegistration>,
}

impl Default for RepoRefRecord {
    fn default() -> Self {
        Self {
            schema_version: REF_REGISTRY_RECORD_SCHEMA_VERSION,
            repo_prefix: String::new(),
            generation: 0,
            complete: false,
            workflow_stage_hashes: Vec::new(),
            workflow_experiment_ids: Vec::new(),
            active_active_coordinator: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShardRootPartition {
    schema_version: u32,
    repo_prefix: String,
    partition: String,
    generation: u64,
    shard_hashes: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryCoverage {
    schema_version: u32,
    complete: bool,
}

/// Per-repository GC root proof for one shard hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepoShardRootStatus {
    pub generation: u64,
    pub complete: bool,
    pub rooted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefRegistry {
    /// Registry schema. Missing on legacy payloads and therefore decoded as 0.
    #[serde(default)]
    pub schema_version: u32,
    /// True only after a bucket-wide repair has enumerated every repo manifest.
    #[serde(default)]
    pub coverage_complete: bool,
    /// Repos whose entry is known to contain their complete current shard set.
    #[serde(default)]
    pub complete_repos: HashSet<String>,
    /// Monotonically increasing version counter, bumped on each CAS write.
    pub generation: u64,
    /// Map from repo prefix (e.g. `"org/models"`) to the list of shard
    /// hash hex strings referenced by that repo's shard-list.
    pub repos: HashMap<String, Vec<String>>,
    /// Workflow-referenced stage hashes keyed by repo prefix.
    ///
    /// Absent on registries written before the workflow layer shipped —
    /// `#[serde(default)]` makes older JSON deserialize to an empty map
    /// without a migration step.
    #[serde(default)]
    pub workflow_stage_hashes: HashMap<String, Vec<String>>,
    /// Live experiment IDs keyed by repo prefix. Entries survive the
    /// client-side `exp gc` retention pass; removing them here is what
    /// makes their backing stage entries eligible for remote deletion.
    #[serde(default)]
    pub workflow_experiment_ids: HashMap<String, Vec<String>>,
    /// Active-active coordinator metadata keyed by repo prefix.
    ///
    /// Bucket-scope GC uses this as the authoritative list of repos whose
    /// coordinator transaction history must be consulted before deleting shared
    /// `.crab/` objects. Missing on older registries means no active-active repo
    /// has registered a bucket-GC safety contract yet.
    #[serde(default)]
    pub active_active_coordinators: HashMap<String, ActiveActiveCoordinatorRegistration>,
}

impl Default for RefRegistry {
    fn default() -> Self {
        Self {
            schema_version: REF_REGISTRY_SCHEMA_VERSION,
            coverage_complete: false,
            complete_repos: HashSet::new(),
            generation: 0,
            repos: HashMap::new(),
            workflow_stage_hashes: HashMap::new(),
            workflow_experiment_ids: HashMap::new(),
            active_active_coordinators: HashMap::new(),
        }
    }
}

impl RefRegistry {
    /// Record that a repo references a set of shard hashes.
    ///
    /// Replaces the repo's entry entirely with the provided shard list.
    /// This is intentional — the caller passes the repo's complete
    /// shard-list, not a delta.
    pub fn register(&mut self, repo_prefix: &str, shard_hashes: Vec<String>) {
        self.schema_version = REF_REGISTRY_SCHEMA_VERSION;
        self.repos.insert(repo_prefix.to_owned(), shard_hashes);
        self.complete_repos.insert(repo_prefix.to_owned());
    }

    /// Union a complete candidate shard set into a repo entry.
    ///
    /// Ordinary push uses union semantics so concurrent pushes and a push
    /// that crashes before manifest CAS can only retain extra GC roots,
    /// never remove another writer's roots. Compaction owns exact replacement.
    pub fn register_union(&mut self, repo_prefix: &str, shard_hashes: Vec<String>) {
        self.schema_version = REF_REGISTRY_SCHEMA_VERSION;
        let entry = self.repos.entry(repo_prefix.to_owned()).or_default();
        entry.extend(shard_hashes);
        entry.sort();
        entry.dedup();
        self.complete_repos.insert(repo_prefix.to_owned());
    }

    /// Replace one committed shard set while preserving concurrent candidates.
    ///
    /// Compaction passes the exact source set pinned before manifest CAS and
    /// the exact replacement set committed by that CAS. Any other entries are
    /// retained because they may protect an in-flight writer from GC.
    pub fn reconcile_compaction(
        &mut self,
        repo_prefix: &str,
        source_shards: &HashSet<String>,
        replacement_shards: &[String],
    ) {
        self.schema_version = REF_REGISTRY_SCHEMA_VERSION;
        let entry = self.repos.entry(repo_prefix.to_owned()).or_default();
        entry.retain(|hash| !source_shards.contains(hash));
        entry.extend(replacement_shards.iter().cloned());
        entry.sort();
        entry.dedup();
        self.complete_repos.insert(repo_prefix.to_owned());
    }

    /// Mark bucket-wide repo discovery complete after a manifest repair scan.
    pub fn mark_coverage_complete(&mut self) {
        self.schema_version = REF_REGISTRY_SCHEMA_VERSION;
        self.coverage_complete = true;
    }

    /// Whether destructive bucket GC has proof that registry coverage is safe.
    #[must_use]
    pub fn is_complete_for_destructive_gc(&self) -> bool {
        self.schema_version == REF_REGISTRY_SCHEMA_VERSION
            && self.coverage_complete
            && self
                .repos
                .keys()
                .all(|repo| self.complete_repos.contains(repo))
    }

    /// Record the workflow-referenced stage hashes for a repo.
    ///
    /// Replace-entire-set semantics matching [`Self::register`]: the
    /// caller passes the repo's complete set of live stage hashes, not
    /// a delta. Removing a hash from this set is what makes its
    /// backing `workflow/stages/<ab>/<hex>.json` object eligible for
    /// remote GC once its grace period elapses.
    pub fn register_workflow_stages(&mut self, repo_prefix: &str, stage_hashes: Vec<String>) {
        self.workflow_stage_hashes
            .insert(repo_prefix.to_owned(), stage_hashes);
    }

    /// Record the live experiment IDs for a repo.
    ///
    /// Replace-entire-set semantics matching [`Self::register`]. The
    /// remote GC walker treats any experiment ID present here as a
    /// root when computing workflow reachability, keeping its
    /// `workflow/exp/<uuid>/{meta,stage-refs}.json` blobs alive.
    pub fn register_experiments(&mut self, repo_prefix: &str, exp_ids: Vec<String>) {
        self.workflow_experiment_ids
            .insert(repo_prefix.to_owned(), exp_ids);
    }

    /// Union workflow roots published by one writer into a repo entry.
    ///
    /// Workflow objects are uploaded before their experiment ref becomes
    /// visible. Union semantics ensure a concurrent push can only leave an
    /// extra GC root; it can never erase another experiment's checkpoint or
    /// stage protection. Exact removal is deliberately a separate,
    /// administrator-driven reconciliation operation.
    pub fn register_workflow_union(
        &mut self,
        repo_prefix: &str,
        stage_hashes: impl IntoIterator<Item = String>,
        exp_ids: impl IntoIterator<Item = String>,
    ) -> bool {
        self.schema_version = REF_REGISTRY_SCHEMA_VERSION;
        let mut changed = false;
        let stages = self
            .workflow_stage_hashes
            .entry(repo_prefix.to_owned())
            .or_default();
        for hash in stage_hashes {
            if !stages.contains(&hash) {
                stages.push(hash);
                changed = true;
            }
        }
        stages.sort();
        stages.dedup();

        let experiments = self
            .workflow_experiment_ids
            .entry(repo_prefix.to_owned())
            .or_default();
        for id in exp_ids {
            if !experiments.contains(&id) {
                experiments.push(id);
                changed = true;
            }
        }
        experiments.sort();
        experiments.dedup();
        changed
    }

    /// Record the coordinator that protects an active-active repo's writes.
    pub fn register_active_active_coordinator(
        &mut self,
        repo_prefix: &str,
        coordinator: ActiveActiveCoordinatorRegistration,
    ) {
        self.active_active_coordinators
            .insert(repo_prefix.to_owned(), coordinator);
    }

    /// Remove active-active coordinator metadata for a repo.
    pub fn deregister_active_active_coordinator(&mut self, repo_prefix: &str) {
        self.active_active_coordinators.remove(repo_prefix);
    }

    /// Remove a repo's workflow entries (stage hashes + experiment IDs).
    ///
    /// Leaves the shard-list entry in `repos` untouched — callers that
    /// want to clear the full repo use [`Self::deregister`] instead.
    /// Useful when a repo stops participating in the workflow layer
    /// (e.g. the user disables `[workflow] enabled`) but is still
    /// pushing plain git content.
    pub fn deregister_workflow(&mut self, repo_prefix: &str) {
        self.workflow_stage_hashes.remove(repo_prefix);
        self.workflow_experiment_ids.remove(repo_prefix);
    }

    /// Remove a repo's entry (e.g. for `crab gc --deregister`).
    ///
    /// Also clears any workflow-scoped state for that repo — once a
    /// repo is fully deregistered its workflow refs cannot be
    /// reachable from any other root.
    pub fn deregister(&mut self, repo_prefix: &str) {
        self.repos.remove(repo_prefix);
        self.complete_repos.remove(repo_prefix);
        self.workflow_stage_hashes.remove(repo_prefix);
        self.workflow_experiment_ids.remove(repo_prefix);
        self.active_active_coordinators.remove(repo_prefix);
    }

    /// Compute the union of all referenced shard hashes across all repos.
    pub fn all_referenced_shards(&self) -> HashSet<String> {
        self.repos
            .values()
            .flat_map(|hashes| hashes.iter().cloned())
            .collect()
    }

    /// Compute the union of all workflow-referenced stage hashes across
    /// all repos. Used by the remote GC walker to gate deletion of
    /// `workflow/stages/<ab>/<hex>.json` objects.
    pub fn all_referenced_workflow_stages(&self) -> HashSet<String> {
        self.workflow_stage_hashes
            .values()
            .flat_map(|hashes| hashes.iter().cloned())
            .collect()
    }

    /// Compute the union of all live experiment IDs across all repos.
    /// Used by the remote GC walker to gate deletion of
    /// `workflow/exp/<uuid>/…` blobs.
    pub fn all_referenced_experiments(&self) -> HashSet<String> {
        self.workflow_experiment_ids
            .values()
            .flat_map(|ids| ids.iter().cloned())
            .collect()
    }

    /// Active-active repos whose coordinator safety snapshots must be loaded.
    pub fn active_active_repos(&self) -> HashSet<String> {
        self.active_active_coordinators.keys().cloned().collect()
    }

    /// Active-active repos not covered by a caller-provided GC safety proof.
    pub fn active_active_repos_missing_gc_proof(
        &self,
        proven_repos: &HashSet<String>,
    ) -> Vec<String> {
        let mut missing = self
            .active_active_coordinators
            .keys()
            .filter(|repo| !proven_repos.contains(*repo))
            .cloned()
            .collect::<Vec<_>>();
        missing.sort();
        missing
    }
}

#[cfg(feature = "storage")]
fn registry_records_prefix(router: &StoreLayout<Store>) -> Path {
    Path::from(format!("{}/records", router.ref_registry_path()))
}

#[cfg(feature = "storage")]
fn registry_record_path(router: &StoreLayout<Store>, repo_prefix: &str) -> Path {
    let digest = blake3::hash(repo_prefix.as_bytes()).to_hex().to_string();
    Path::from(format!(
        "{}/{}/{}.json",
        registry_records_prefix(router),
        &digest[..2],
        digest
    ))
}

#[cfg(feature = "storage")]
fn registry_coverage_path(router: &StoreLayout<Store>) -> Path {
    Path::from(format!("{}/coverage.json", router.ref_registry_path()))
}

#[cfg(feature = "storage")]
fn registry_shard_roots_prefix(router: &StoreLayout<Store>) -> Path {
    Path::from(format!("{}/shard-roots", router.ref_registry_path()))
}

#[cfg(feature = "storage")]
fn repo_digest(repo_prefix: &str) -> String {
    blake3::hash(repo_prefix.as_bytes()).to_hex().to_string()
}

#[cfg(feature = "storage")]
fn shard_root_partition(shard_hash: &str) -> String {
    let digest = blake3::hash(shard_hash.as_bytes()).to_hex().to_string();
    digest[..4].to_owned()
}

#[cfg(feature = "storage")]
fn repo_shard_roots_prefix(router: &StoreLayout<Store>, repo_prefix: &str) -> Path {
    Path::from(format!(
        "{}/{}",
        registry_shard_roots_prefix(router),
        repo_digest(repo_prefix)
    ))
}

#[cfg(feature = "storage")]
fn shard_root_partition_path(
    router: &StoreLayout<Store>,
    repo_prefix: &str,
    partition: &str,
) -> Path {
    Path::from(format!(
        "{}/{}.json",
        repo_shard_roots_prefix(router, repo_prefix),
        partition
    ))
}

#[cfg(feature = "storage")]
fn normalize(values: &mut Vec<String>) {
    values.sort_unstable();
    values.dedup();
}

#[cfg(feature = "storage")]
fn validate_record(record: &RepoRefRecord, path: &Path) -> Result<()> {
    if record.schema_version != REF_REGISTRY_RECORD_SCHEMA_VERSION
        || record.repo_prefix.trim().is_empty()
        || registry_record_path_for_root(path, &record.repo_prefix) != *path
    {
        return Err(MetadataError::CorruptObject {
            path: path.to_string(),
            reason: "invalid partitioned ref-registry record identity".to_owned(),
        });
    }
    Ok(())
}

#[cfg(feature = "storage")]
fn validate_shard_root_partition(
    root: &ShardRootPartition,
    router: &StoreLayout<Store>,
    path: &Path,
) -> Result<()> {
    let valid_partition = root.partition.len() == 4
        && root
            .partition
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
    if root.schema_version != REF_REGISTRY_ROOT_SCHEMA_VERSION
        || root.repo_prefix.trim().is_empty()
        || !valid_partition
        || shard_root_partition_path(router, &root.repo_prefix, &root.partition) != *path
        || root
            .shard_hashes
            .windows(2)
            .any(|window| window[0] >= window[1])
        || root
            .shard_hashes
            .iter()
            .any(|hash| shard_root_partition(hash) != root.partition)
    {
        return Err(MetadataError::CorruptObject {
            path: path.to_string(),
            reason: "invalid partitioned ref-registry shard roots".to_owned(),
        });
    }
    Ok(())
}

#[cfg(feature = "storage")]
fn registry_record_path_for_root(record_path: &Path, repo_prefix: &str) -> Path {
    let path = record_path.as_ref();
    let root = path
        .split_once("/records/")
        .map(|(root, _)| root)
        .unwrap_or_default();
    let digest = blake3::hash(repo_prefix.as_bytes()).to_hex().to_string();
    Path::from(format!("{root}/records/{}/{}.json", &digest[..2], digest))
}

#[cfg(feature = "storage")]
async fn update_repo_record(
    store: &Store,
    router: &StoreLayout<Store>,
    mutate: impl Fn(&mut RepoRefRecord),
) -> Result<RepoRefRecord> {
    let repo_prefix = router.repo_prefix().to_owned();
    let path = registry_record_path(router, &repo_prefix);
    let record =
        crab_storage::cas::cas_update_default::<RepoRefRecord, _>(store, path.as_ref(), |record| {
            if record.repo_prefix.is_empty() {
                record.repo_prefix.clone_from(&repo_prefix);
            }
            if record.repo_prefix == repo_prefix {
                mutate(record);
            }
        })
        .await
        .map_err(MetadataError::from)?;
    validate_record(&record, &path)?;
    Ok(record)
}

#[cfg(feature = "storage")]
async fn union_shard_root_partition(
    store: &Store,
    router: &StoreLayout<Store>,
    repo_prefix: &str,
    partition: &str,
    shard_hashes: Vec<String>,
) -> Result<()> {
    let path = shard_root_partition_path(router, repo_prefix, partition);
    let repo_prefix = repo_prefix.to_owned();
    let partition = partition.to_owned();
    let root = crab_storage::cas::cas_update_default::<ShardRootPartition, _>(
        store,
        path.as_ref(),
        |root| {
            if root.repo_prefix.is_empty() {
                root.repo_prefix.clone_from(&repo_prefix);
                root.partition.clone_from(&partition);
            }
            if root.repo_prefix == repo_prefix && root.partition == partition {
                root.schema_version = REF_REGISTRY_ROOT_SCHEMA_VERSION;
                root.shard_hashes.extend(shard_hashes.clone());
                normalize(&mut root.shard_hashes);
                root.generation = root.generation.saturating_add(1);
            }
        },
    )
    .await
    .map_err(MetadataError::from)?;
    validate_shard_root_partition(&root, router, &path)
}

#[cfg(feature = "storage")]
async fn replace_repo_shard_roots(
    store: &Store,
    router: &StoreLayout<Store>,
    repo_prefix: &str,
    shard_hashes: Vec<String>,
) -> Result<()> {
    let mut partitions = HashMap::<String, Vec<String>>::new();
    for hash in shard_hashes {
        partitions
            .entry(shard_root_partition(&hash))
            .or_default()
            .push(hash);
    }
    let mut expected = HashSet::with_capacity(partitions.len());
    for (partition, mut hashes) in partitions {
        normalize(&mut hashes);
        let path = shard_root_partition_path(router, repo_prefix, &partition);
        expected.insert(path.clone());
        let repo_prefix = repo_prefix.to_owned();
        let partition = partition.to_owned();
        let root = crab_storage::cas::cas_update_default::<ShardRootPartition, _>(
            store,
            path.as_ref(),
            |root| {
                root.schema_version = REF_REGISTRY_ROOT_SCHEMA_VERSION;
                root.repo_prefix.clone_from(&repo_prefix);
                root.partition.clone_from(&partition);
                root.shard_hashes.clone_from(&hashes);
                root.generation = root.generation.saturating_add(1);
            },
        )
        .await
        .map_err(MetadataError::from)?;
        validate_shard_root_partition(&root, router, &path)?;
    }
    let prefix = repo_shard_roots_prefix(router, repo_prefix);
    let mut roots = store.inner().list(Some(&prefix));
    while let Some(meta) = roots
        .try_next()
        .await
        .map_err(|source| MetadataError::Storage {
            source: crab_storage::StorageError::ObjectStore { source },
        })?
    {
        if !expected.contains(&meta.location) {
            store.delete(&meta.location).await?;
        }
    }
    Ok(())
}

/// Loads registry coordination records without materializing shard roots.
#[cfg(feature = "storage")]
pub async fn load_ref_registry_summary(
    store: &Store,
    router: &StoreLayout<Store>,
) -> Result<RefRegistry> {
    let coverage_path = registry_coverage_path(router);
    let coverage = match store.get_with_etag(&coverage_path).await {
        Ok((body, _)) => serde_json::from_slice::<RegistryCoverage>(&body).map_err(|error| {
            MetadataError::CorruptObject {
                path: coverage_path.to_string(),
                reason: format!("invalid JSON: {error}"),
            }
        })?,
        Err(crab_storage::StorageError::NotFound { .. }) => RegistryCoverage::default(),
        Err(error) => return Err(MetadataError::from(error)),
    };
    if coverage.schema_version != 0 && coverage.schema_version != REF_REGISTRY_RECORD_SCHEMA_VERSION
    {
        return Err(MetadataError::CorruptObject {
            path: coverage_path.to_string(),
            reason: "unsupported partitioned ref-registry coverage schema".to_owned(),
        });
    }

    let prefix = registry_records_prefix(router);
    let mut records = store.inner().list(Some(&prefix));
    let mut registry = RefRegistry {
        coverage_complete: coverage.complete,
        ..RefRegistry::default()
    };
    while let Some(meta) = records
        .try_next()
        .await
        .map_err(|source| MetadataError::Storage {
            source: crab_storage::StorageError::ObjectStore { source },
        })?
    {
        let (body, _) = store.get_with_etag(&meta.location).await?;
        let mut record: RepoRefRecord =
            serde_json::from_slice(&body).map_err(|error| MetadataError::CorruptObject {
                path: meta.location.to_string(),
                reason: format!("invalid JSON: {error}"),
            })?;
        validate_record(&record, &meta.location)?;
        normalize(&mut record.workflow_stage_hashes);
        normalize(&mut record.workflow_experiment_ids);
        let repo = record.repo_prefix.clone();
        if registry.repos.insert(repo.clone(), Vec::new()).is_some() {
            return Err(MetadataError::CorruptObject {
                path: meta.location.to_string(),
                reason: format!("duplicate partitioned ref-registry repo {repo}"),
            });
        }
        if record.complete {
            registry.complete_repos.insert(repo.clone());
        }
        if !record.workflow_stage_hashes.is_empty() {
            registry
                .workflow_stage_hashes
                .insert(repo.clone(), record.workflow_stage_hashes);
        }
        if !record.workflow_experiment_ids.is_empty() {
            registry
                .workflow_experiment_ids
                .insert(repo.clone(), record.workflow_experiment_ids);
        }
        if let Some(coordinator) = record.active_active_coordinator {
            registry
                .active_active_coordinators
                .insert(repo, coordinator);
        }
        registry.generation = registry.generation.saturating_add(record.generation);
    }

    Ok(registry)
}

/// Loads one repository record and one shard-root partition.
///
/// This keeps push-side receipt validation independent of bucket and
/// repository cardinality. `None` means the repository has no registry
/// record; a missing shard partition is a valid `rooted = false` result.
#[cfg(feature = "storage")]
pub async fn load_repo_shard_root_status(
    store: &Store,
    router: &StoreLayout<Store>,
    repo_prefix: &str,
    shard_hash: &str,
) -> Result<Option<RepoShardRootStatus>> {
    let record_path = registry_record_path(router, repo_prefix);
    let (record_body, _) = match store.get_with_etag(&record_path).await {
        Ok(value) => value,
        Err(crab_storage::StorageError::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(MetadataError::from(error)),
    };
    let record: RepoRefRecord =
        serde_json::from_slice(&record_body).map_err(|error| MetadataError::CorruptObject {
            path: record_path.to_string(),
            reason: format!("invalid JSON: {error}"),
        })?;
    validate_record(&record, &record_path)?;

    let partition = shard_root_partition(shard_hash);
    let root_path = shard_root_partition_path(router, repo_prefix, &partition);
    let rooted = match store.get_with_etag(&root_path).await {
        Ok((body, _)) => {
            let root: ShardRootPartition =
                serde_json::from_slice(&body).map_err(|error| MetadataError::CorruptObject {
                    path: root_path.to_string(),
                    reason: format!("invalid JSON: {error}"),
                })?;
            validate_shard_root_partition(&root, router, &root_path)?;
            root.shard_hashes
                .binary_search_by(|candidate| candidate.as_str().cmp(shard_hash))
                .is_ok()
        }
        Err(crab_storage::StorageError::NotFound { .. }) => false,
        Err(error) => return Err(MetadataError::from(error)),
    };
    Ok(Some(RepoShardRootStatus {
        generation: record.generation,
        complete: record.complete,
        rooted,
    }))
}

/// Streams validated shard roots one at a time from bounded partition bodies.
#[cfg(feature = "storage")]
pub async fn visit_ref_registry_shard_roots<F, Fut, E>(
    store: &Store,
    router: &StoreLayout<Store>,
    mut visit: F,
) -> std::result::Result<u64, E>
where
    F: FnMut(String, String) -> Fut,
    Fut: std::future::Future<Output = std::result::Result<(), E>>,
    E: From<MetadataError>,
{
    let root_prefix = registry_shard_roots_prefix(router);
    let mut roots = store.inner().list(Some(&root_prefix));
    let mut generation = 0u64;
    while let Some(meta) = roots
        .try_next()
        .await
        .map_err(|source| MetadataError::Storage {
            source: crab_storage::StorageError::ObjectStore { source },
        })?
    {
        let (body, _) = store
            .get_with_etag(&meta.location)
            .await
            .map_err(MetadataError::from)?;
        let root: ShardRootPartition =
            serde_json::from_slice(&body).map_err(|error| MetadataError::CorruptObject {
                path: meta.location.to_string(),
                reason: format!("invalid JSON: {error}"),
            })?;
        validate_shard_root_partition(&root, router, &meta.location).map_err(E::from)?;
        generation = generation.saturating_add(root.generation);
        for hash in root.shard_hashes {
            visit(root.repo_prefix.clone(), hash).await?;
        }
    }
    Ok(generation)
}

/// Loads the partitioned bucket registry into its aggregate compatibility view.
#[cfg(feature = "storage")]
pub async fn load_ref_registry(store: &Store, router: &StoreLayout<Store>) -> Result<RefRegistry> {
    let mut registry = load_ref_registry_summary(store, router).await?;
    let root_generation = visit_ref_registry_shard_roots(store, router, |repo, hash| {
        let result = registry
            .repos
            .get_mut(&repo)
            .ok_or_else(|| MetadataError::CorruptObject {
                path: registry_shard_roots_prefix(router).to_string(),
                reason: format!("shard-root partition has no repo record for {repo}"),
            })
            .map(|shards| shards.push(hash));
        std::future::ready(result)
    })
    .await?;
    registry.generation = registry.generation.saturating_add(root_generation);
    for shards in registry.repos.values_mut() {
        normalize(shards);
    }
    Ok(registry)
}

/// CAS-union a repo's base-plus-candidate shard set before manifest publish.
#[cfg(feature = "storage")]
pub async fn union_register_repo_shards(
    store: &Store,
    router: &StoreLayout<Store>,
    shard_hashes: Vec<String>,
) -> Result<u64> {
    let repo_prefix = router.repo_prefix().to_owned();
    let mut partitions = HashMap::<String, Vec<String>>::new();
    for hash in shard_hashes {
        partitions
            .entry(shard_root_partition(&hash))
            .or_default()
            .push(hash);
    }
    for (partition, hashes) in partitions {
        union_shard_root_partition(store, router, &repo_prefix, &partition, hashes).await?;
    }
    update_repo_record(store, router, |record| {
        record.schema_version = REF_REGISTRY_RECORD_SCHEMA_VERSION;
        record.complete = true;
        // Shard partitions commit first. Advancing this generation makes a
        // root-identity seal observe the complete union as one publication.
        record.generation = record.generation.saturating_add(1);
    })
    .await
    .map(|record| record.generation)
}

/// Remove only the source shards replaced by a committed compaction.
///
/// Candidate roots added by concurrent writers remain registered. This may
/// retain an extra root after a failed writer, but it cannot make live data
/// collectible.
#[cfg(feature = "storage")]
pub async fn reconcile_compacted_repo_shards(
    store: &Store,
    router: &StoreLayout<Store>,
    source_shards: HashSet<String>,
    replacement_shards: Vec<String>,
) -> Result<u64> {
    let registry_path = router.ref_registry_path();
    let repo_prefix = router.repo_prefix().to_owned();
    crab_storage::cas::cas_update_default::<RefRegistry, _>(
        store,
        registry_path.as_ref(),
        |registry| {
            let before = registry
                .repos
                .get(&repo_prefix)
                .cloned()
                .unwrap_or_default();
            let was_complete = registry.complete_repos.contains(&repo_prefix);
            registry.reconcile_compaction(&repo_prefix, &source_shards, &replacement_shards);
            if registry.repos.get(&repo_prefix) != Some(&before) || !was_complete {
                registry.generation += 1;
            }
        },
    )
    .await
    .map(|registry| registry.generation)
    .map_err(MetadataError::from)
}

/// Publish conservative workflow GC roots after immutable experiment
/// objects have been uploaded and before the experiment ref is made visible.
///
/// This helper intentionally uses union semantics. A failed or interrupted
/// push may leave an extra root, but it cannot remove another writer's root;
/// later reconciliation may remove stale roots once remote experiment refs
/// have been enumerated.
#[cfg(feature = "storage")]
pub async fn union_register_workflow_roots(
    store: &Store,
    router: &StoreLayout<Store>,
    stage_hashes: Vec<String>,
    exp_ids: Vec<String>,
) -> Result<u64> {
    update_repo_record(store, router, |record| {
        let before_stages = record.workflow_stage_hashes.clone();
        let before_experiments = record.workflow_experiment_ids.clone();
        record.schema_version = REF_REGISTRY_RECORD_SCHEMA_VERSION;
        record.workflow_stage_hashes.extend(stage_hashes.clone());
        record.workflow_experiment_ids.extend(exp_ids.clone());
        normalize(&mut record.workflow_stage_hashes);
        normalize(&mut record.workflow_experiment_ids);
        if record.workflow_stage_hashes != before_stages
            || record.workflow_experiment_ids != before_experiments
        {
            record.generation = record.generation.saturating_add(1);
        }
    })
    .await
    .map(|record| record.generation)
}

/// Exactly rebuilds repo shard records and publishes complete bucket coverage.
///
/// The caller must hold the exclusive bucket GC fence for the entire manifest
/// scan and this commit. That boundary makes removal of stale roots safe.
#[cfg(feature = "storage")]
pub async fn repair_ref_registry_from_manifests(
    store: &Store,
    router: &StoreLayout<Store>,
    repos: HashMap<String, Vec<String>>,
) -> Result<()> {
    let mut expected_paths = HashSet::with_capacity(repos.len());
    let mut expected_root_prefixes = HashSet::with_capacity(repos.len());
    for (repo_prefix, shard_hashes) in repos {
        let repo_router = StoreLayout::with_global_prefix(
            store.clone(),
            repo_prefix.clone(),
            router.global_prefix().to_owned(),
        );
        let path = registry_record_path(&repo_router, &repo_prefix);
        expected_paths.insert(path.clone());
        expected_root_prefixes.insert(repo_shard_roots_prefix(&repo_router, &repo_prefix));
        replace_repo_shard_roots(store, &repo_router, &repo_prefix, shard_hashes).await?;
        update_repo_record(store, &repo_router, |record| {
            record.schema_version = REF_REGISTRY_RECORD_SCHEMA_VERSION;
            record.complete = true;
            record.generation = record.generation.saturating_add(1);
        })
        .await?;
    }

    for meta in store.list_prefix(&registry_records_prefix(router)).await? {
        if !expected_paths.contains(&meta.location) {
            let (body, _) = store.get_with_etag(&meta.location).await?;
            let record: RepoRefRecord =
                serde_json::from_slice(&body).map_err(|error| MetadataError::CorruptObject {
                    path: meta.location.to_string(),
                    reason: format!("invalid JSON: {error}"),
                })?;
            validate_record(&record, &meta.location)?;
            let prefix = repo_shard_roots_prefix(router, &record.repo_prefix);
            for root in store.list_prefix(&prefix).await? {
                store.delete(&root.location).await?;
            }
            store.delete(&meta.location).await?;
        }
    }
    let mut roots = store
        .inner()
        .list(Some(&registry_shard_roots_prefix(router)));
    while let Some(meta) = roots
        .try_next()
        .await
        .map_err(|source| MetadataError::Storage {
            source: crab_storage::StorageError::ObjectStore { source },
        })?
    {
        if !expected_root_prefixes
            .iter()
            .any(|prefix| meta.location.as_ref().starts_with(prefix.as_ref()))
        {
            store.delete(&meta.location).await?;
        }
    }
    let coverage = RegistryCoverage {
        schema_version: REF_REGISTRY_RECORD_SCHEMA_VERSION,
        complete: true,
    };
    let path = registry_coverage_path(router);
    let bytes = bytes::Bytes::from(serde_json::to_vec(&coverage).map_err(|error| {
        MetadataError::CorruptObject {
            path: path.to_string(),
            reason: format!("cannot encode registry coverage: {error}"),
        }
    })?);
    match store.create_strict(&path, bytes.clone()).await {
        Ok(()) => Ok(()),
        Err(crab_storage::StorageError::StateConflict { .. }) => {
            let (_, etag) = store.get_with_etag(&path).await?;
            store.update(&path, bytes, etag).await?;
            Ok(())
        }
        Err(error) => Err(MetadataError::from(error)),
    }
}

/// Registers an active-active coordinator in the bucket ref-registry.
#[cfg(feature = "storage")]
pub async fn register_active_active_coordinator_for_repo(
    store: &Store,
    router: &StoreLayout<Store>,
    registration: ActiveActiveCoordinatorRegistration,
) -> Result<()> {
    update_repo_record(store, router, |record| {
        if record.active_active_coordinator.as_ref() != Some(&registration) {
            record.active_active_coordinator = Some(registration.clone());
            record.generation = record.generation.saturating_add(1);
        }
    })
    .await
    .map(|_| ())
}

/// Removes one repository's partitioned registry record.
#[cfg(feature = "storage")]
pub async fn deregister_repo(
    store: &Store,
    router: &StoreLayout<Store>,
    repo_prefix: &str,
) -> Result<bool> {
    let roots = repo_shard_roots_prefix(router, repo_prefix);
    for root in store.list_prefix(&roots).await? {
        store.delete(&root.location).await?;
    }
    let path = registry_record_path(router, repo_prefix);
    match store.delete(&path).await {
        Ok(()) => Ok(true),
        Err(crab_storage::StorageError::NotFound { .. }) => Ok(false),
        Err(error) => Err(MetadataError::from(error)),
    }
}

/// Replaces workflow roots for one repository after exact reconciliation.
#[cfg(feature = "storage")]
pub async fn register_workflow_roots_exact(
    store: &Store,
    router: &StoreLayout<Store>,
    mut stage_hashes: Vec<String>,
    mut exp_ids: Vec<String>,
) -> Result<u64> {
    normalize(&mut stage_hashes);
    normalize(&mut exp_ids);
    update_repo_record(store, router, |record| {
        if record.workflow_stage_hashes != stage_hashes || record.workflow_experiment_ids != exp_ids
        {
            record.workflow_stage_hashes.clone_from(&stage_hashes);
            record.workflow_experiment_ids.clone_from(&exp_ids);
            record.generation = record.generation.saturating_add(1);
        }
    })
    .await
    .map(|record| record.generation)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn default_registry_is_empty() {
        let reg = RefRegistry::default();
        assert_eq!(reg.schema_version, REF_REGISTRY_SCHEMA_VERSION);
        assert!(!reg.coverage_complete);
        assert_eq!(reg.generation, 0);
        assert!(reg.repos.is_empty());
        assert!(reg.all_referenced_shards().is_empty());
    }

    #[test]
    fn register_adds_repo_entry() {
        let mut reg = RefRegistry::default();
        reg.register("org/models", vec!["aaa".into(), "bbb".into()]);

        assert_eq!(reg.repos.len(), 1);
        assert_eq!(
            reg.repos["org/models"],
            vec!["aaa".to_string(), "bbb".to_string()]
        );
    }

    #[test]
    fn register_replaces_existing_entry() {
        let mut reg = RefRegistry::default();
        reg.register("org/models", vec!["aaa".into()]);
        reg.register("org/models", vec!["bbb".into(), "ccc".into()]);

        assert_eq!(reg.repos.len(), 1);
        assert_eq!(
            reg.repos["org/models"],
            vec!["bbb".to_string(), "ccc".to_string()]
        );
    }

    #[test]
    fn ordinary_push_union_never_removes_concurrent_roots() {
        let mut reg = RefRegistry::default();
        reg.register_union("org/models", vec!["base".into(), "writer-a".into()]);
        reg.register_union("org/models", vec!["base".into(), "writer-b".into()]);

        assert_eq!(
            reg.repos["org/models"],
            vec![
                "base".to_owned(),
                "writer-a".to_owned(),
                "writer-b".to_owned()
            ]
        );
        assert!(reg.complete_repos.contains("org/models"));
    }

    #[test]
    fn destructive_gc_requires_explicit_bucket_coverage() {
        let mut reg = RefRegistry::default();
        reg.register("org/models", vec!["shard".into()]);
        assert!(!reg.is_complete_for_destructive_gc());

        reg.mark_coverage_complete();
        assert!(reg.is_complete_for_destructive_gc());
    }

    #[test]
    fn deregister_removes_repo_entry() {
        let mut reg = RefRegistry::default();
        reg.register("org/models", vec!["aaa".into()]);
        reg.register("org/datasets", vec!["bbb".into()]);

        reg.deregister("org/models");

        assert_eq!(reg.repos.len(), 1);
        assert!(!reg.repos.contains_key("org/models"));
        assert!(reg.repos.contains_key("org/datasets"));
    }

    #[test]
    fn deregister_nonexistent_repo_is_noop() {
        let mut reg = RefRegistry::default();
        reg.register("org/models", vec!["aaa".into()]);

        reg.deregister("org/nonexistent");

        assert_eq!(reg.repos.len(), 1);
        assert!(reg.repos.contains_key("org/models"));
    }

    #[test]
    fn all_referenced_shards_returns_union() {
        let mut reg = RefRegistry::default();
        reg.register("org/models", vec!["aaa".into(), "bbb".into()]);
        reg.register("org/datasets", vec!["bbb".into(), "ccc".into()]);

        let all = reg.all_referenced_shards();

        assert_eq!(all.len(), 3);
        assert!(all.contains("aaa"));
        assert!(all.contains("bbb"));
        assert!(all.contains("ccc"));
    }

    #[test]
    fn all_referenced_shards_empty_after_full_deregister() {
        let mut reg = RefRegistry::default();
        reg.register("org/models", vec!["aaa".into()]);
        reg.deregister("org/models");

        assert!(reg.all_referenced_shards().is_empty());
    }

    #[test]
    fn serialize_deserialize_round_trip() {
        let mut reg = RefRegistry::default();
        reg.generation = 42;
        reg.register("org/models", vec!["abc123".into(), "def456".into()]);
        reg.register("org/datasets", vec!["abc123".into(), "fed987".into()]);

        let json = serde_json::to_string(&reg).unwrap();
        let parsed: RefRegistry = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.generation, 42);
        assert_eq!(parsed.repos.len(), 2);
        assert_eq!(parsed.repos["org/models"], reg.repos["org/models"]);
        assert_eq!(parsed.repos["org/datasets"], reg.repos["org/datasets"]);
    }

    #[test]
    fn deregister_exclusive_shards_removed_from_union() {
        let mut reg = RefRegistry::default();
        reg.register("repo-a", vec!["shared".into(), "only-a".into()]);
        reg.register("repo-b", vec!["shared".into(), "only-b".into()]);

        reg.deregister("repo-a");

        let all = reg.all_referenced_shards();
        assert!(all.contains("shared"));
        assert!(all.contains("only-b"));
        assert!(!all.contains("only-a"));
    }

    // Workflow extension behavior.

    #[test]
    fn register_workflow_stages_replaces_entire_set() {
        let mut reg = RefRegistry::default();
        reg.register_workflow_stages("org/models", vec!["aaa".into(), "bbb".into()]);
        reg.register_workflow_stages("org/models", vec!["ccc".into()]);

        assert_eq!(reg.workflow_stage_hashes.len(), 1);
        assert_eq!(
            reg.workflow_stage_hashes["org/models"],
            vec!["ccc".to_string()]
        );
    }

    #[test]
    fn register_experiments_replaces_entire_set() {
        let mut reg = RefRegistry::default();
        reg.register_experiments("org/models", vec!["exp-1".into(), "exp-2".into()]);
        reg.register_experiments("org/models", vec!["exp-3".into()]);

        assert_eq!(reg.workflow_experiment_ids.len(), 1);
        assert_eq!(
            reg.workflow_experiment_ids["org/models"],
            vec!["exp-3".to_string()]
        );
    }

    #[test]
    fn workflow_union_preserves_concurrent_roots_and_is_idempotent() {
        let mut reg = RefRegistry::default();
        assert!(reg.register_workflow_union(
            "org/models",
            vec!["stage-b".to_owned(), "stage-a".to_owned()],
            vec!["exp-2".to_owned()],
        ));
        assert!(reg.register_workflow_union(
            "org/models",
            vec!["stage-c".to_owned(), "stage-a".to_owned()],
            vec!["exp-1".to_owned(), "exp-2".to_owned()],
        ));
        assert!(!reg.register_workflow_union(
            "org/models",
            vec!["stage-a".to_owned()],
            vec!["exp-1".to_owned()],
        ));
        assert_eq!(
            reg.workflow_stage_hashes["org/models"],
            vec!["stage-a", "stage-b", "stage-c"]
        );
        assert_eq!(
            reg.workflow_experiment_ids["org/models"],
            vec!["exp-1", "exp-2"]
        );
    }

    #[test]
    fn all_referenced_workflow_stages_returns_union() {
        let mut reg = RefRegistry::default();
        reg.register_workflow_stages("org/a", vec!["sh1".into(), "sh2".into()]);
        reg.register_workflow_stages("org/b", vec!["sh2".into(), "sh3".into()]);

        let all = reg.all_referenced_workflow_stages();
        assert_eq!(all.len(), 3);
        assert!(all.contains("sh1"));
        assert!(all.contains("sh2"));
        assert!(all.contains("sh3"));
    }

    #[test]
    fn all_referenced_experiments_returns_union() {
        let mut reg = RefRegistry::default();
        reg.register_experiments("org/a", vec!["exp-1".into(), "exp-2".into()]);
        reg.register_experiments("org/b", vec!["exp-2".into(), "exp-3".into()]);

        let all = reg.all_referenced_experiments();
        assert_eq!(all.len(), 3);
        assert!(all.contains("exp-1"));
        assert!(all.contains("exp-2"));
        assert!(all.contains("exp-3"));
    }

    #[test]
    fn deregister_workflow_clears_only_workflow_state() {
        let mut reg = RefRegistry::default();
        reg.register("org/models", vec!["shard-1".into()]);
        reg.register_workflow_stages("org/models", vec!["stage-1".into()]);
        reg.register_experiments("org/models", vec!["exp-1".into()]);

        reg.deregister_workflow("org/models");

        // Shard list survives — only workflow fields cleared.
        assert!(reg.repos.contains_key("org/models"));
        assert!(!reg.workflow_stage_hashes.contains_key("org/models"));
        assert!(!reg.workflow_experiment_ids.contains_key("org/models"));
    }

    #[test]
    fn deregister_also_clears_workflow_state() {
        let mut reg = RefRegistry::default();
        reg.register("org/models", vec!["shard-1".into()]);
        reg.register_workflow_stages("org/models", vec!["stage-1".into()]);
        reg.register_experiments("org/models", vec!["exp-1".into()]);
        reg.register_active_active_coordinator(
            "org/models",
            ActiveActiveCoordinatorRegistration {
                provider: "dynamodb".into(),
                url: "dynamodb://crab-coordinator".into(),
                region: "us-east-1".into(),
                failover_regions: vec!["us-west-2".into()],
            },
        );

        reg.deregister("org/models");

        assert!(!reg.repos.contains_key("org/models"));
        assert!(!reg.workflow_stage_hashes.contains_key("org/models"));
        assert!(!reg.workflow_experiment_ids.contains_key("org/models"));
        assert!(!reg.active_active_coordinators.contains_key("org/models"));
    }

    #[test]
    fn active_active_coordinator_registration_round_trips() {
        let mut reg = RefRegistry::default();
        reg.register_active_active_coordinator(
            "org/models",
            ActiveActiveCoordinatorRegistration {
                provider: "spanner".into(),
                url: "spanner://crab-coordinator".into(),
                region: "nam3".into(),
                failover_regions: vec!["eur3".into()],
            },
        );

        let json = serde_json::to_string(&reg).unwrap();
        let parsed: RefRegistry = serde_json::from_str(&json).unwrap();

        assert_eq!(
            parsed.active_active_coordinators["org/models"].url,
            "spanner://crab-coordinator"
        );
        assert!(parsed.active_active_repos().contains("org/models"));
    }

    #[test]
    fn active_active_missing_gc_proof_is_sorted() {
        let mut reg = RefRegistry::default();
        for repo in ["org/z", "org/a"] {
            reg.register_active_active_coordinator(
                repo,
                ActiveActiveCoordinatorRegistration {
                    provider: "cosmosdb".into(),
                    url: "cosmosdb://crab-coordinator".into(),
                    region: "eastus".into(),
                    failover_regions: Vec::new(),
                },
            );
        }
        let proven = ["org/z".to_owned()].into_iter().collect();

        assert_eq!(
            reg.active_active_repos_missing_gc_proof(&proven),
            vec!["org/a".to_owned()]
        );
    }

    #[cfg(feature = "storage")]
    #[tokio::test]
    async fn register_active_active_coordinator_persists_to_ref_registry() {
        use std::sync::Arc;

        use object_store::ObjectStore;
        use object_store::memory::InMemory;

        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "org/models".to_owned());
        let registration = ActiveActiveCoordinatorRegistration {
            provider: "dynamodb".into(),
            url: "dynamodb://crab-coordinator".into(),
            region: "us-east-1".into(),
            failover_regions: vec!["us-west-2".into()],
        };

        register_active_active_coordinator_for_repo(&store, &router, registration.clone())
            .await
            .unwrap();

        let registry = load_ref_registry(&store, &router).await.unwrap();
        assert_eq!(registry.generation, 1);
        assert_eq!(
            registry.active_active_coordinators["org/models"],
            registration
        );
    }

    #[cfg(feature = "storage")]
    #[tokio::test]
    async fn workflow_union_persists_conservative_roots() {
        use std::sync::Arc;

        use object_store::ObjectStore;
        use object_store::memory::InMemory;

        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "org/models".to_owned());
        let first = union_register_workflow_roots(
            &store,
            &router,
            vec!["stage-b".to_owned(), "stage-a".to_owned()],
            vec!["exp-1".to_owned()],
        )
        .await
        .unwrap();
        let second = union_register_workflow_roots(
            &store,
            &router,
            vec!["stage-c".to_owned()],
            vec!["exp-2".to_owned()],
        )
        .await
        .unwrap();
        assert!(second > first);

        let registry = load_ref_registry(&store, &router).await.unwrap();
        assert_eq!(
            registry.workflow_stage_hashes["org/models"],
            vec!["stage-a", "stage-b", "stage-c"]
        );
        assert_eq!(
            registry.workflow_experiment_ids["org/models"],
            vec!["exp-1", "exp-2"]
        );
    }

    #[cfg(feature = "storage")]
    #[tokio::test]
    async fn targeted_shard_root_status_reads_only_the_repo_partition() {
        use std::sync::Arc;

        use object_store::ObjectStore;
        use object_store::memory::InMemory;

        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "org/models".to_owned());
        let generation = union_register_repo_shards(
            &store,
            &router,
            vec!["root-a".to_owned(), "root-b".to_owned()],
        )
        .await
        .unwrap();

        assert_eq!(
            load_repo_shard_root_status(&store, &router, "org/models", "root-b")
                .await
                .unwrap(),
            Some(RepoShardRootStatus {
                generation,
                complete: true,
                rooted: true,
            })
        );
        assert_eq!(
            load_repo_shard_root_status(&store, &router, "org/models", "missing")
                .await
                .unwrap(),
            Some(RepoShardRootStatus {
                generation,
                complete: true,
                rooted: false,
            })
        );
        assert_eq!(
            load_repo_shard_root_status(&store, &router, "org/absent", "root-b")
                .await
                .unwrap(),
            None
        );
    }

    #[cfg(feature = "storage")]
    #[tokio::test]
    async fn manifest_repair_replaces_stale_roots_exactly() {
        use std::sync::Arc;

        use object_store::ObjectStore;
        use object_store::memory::InMemory;

        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "org/models".to_owned());
        union_register_repo_shards(&store, &router, vec!["candidate".to_owned()])
            .await
            .unwrap();

        repair_ref_registry_from_manifests(
            &store,
            &router,
            HashMap::from([("org/models".to_owned(), vec!["base".to_owned()])]),
        )
        .await
        .unwrap();

        let registry = load_ref_registry(&store, &router).await.unwrap();
        assert_eq!(registry.repos["org/models"], vec!["base".to_owned()]);
        assert!(registry.is_complete_for_destructive_gc());
    }

    #[cfg(feature = "storage")]
    #[tokio::test]
    async fn repo_updates_do_not_rewrite_unrelated_registry_state() {
        use std::sync::Arc;

        use object_store::ObjectStore;
        use object_store::memory::InMemory;

        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        for index in 0..256 {
            let router = StoreLayout::new(store.clone(), format!("org/repo-{index:04}"));
            union_register_repo_shards(&store, &router, vec![format!("shard-{index:04}")])
                .await
                .unwrap();
        }
        let unrelated = StoreLayout::new(store.clone(), "org/repo-0000".to_owned());
        let unrelated_path = registry_record_path(&unrelated, unrelated.repo_prefix());
        let (unrelated_before, _) = store.get_with_etag(&unrelated_path).await.unwrap();
        let target = StoreLayout::new(store.clone(), "org/target".to_owned());
        union_register_repo_shards(&store, &target, vec!["first".to_owned()])
            .await
            .unwrap();
        let path = registry_record_path(&target, target.repo_prefix());
        let (before, _) = store.get_with_etag(&path).await.unwrap();

        union_register_repo_shards(&store, &target, vec!["second".to_owned()])
            .await
            .unwrap();
        let (after, _) = store.get_with_etag(&path).await.unwrap();
        let (unrelated_after, _) = store.get_with_etag(&unrelated_path).await.unwrap();
        let root_objects = store
            .list_prefix(&repo_shard_roots_prefix(&target, target.repo_prefix()))
            .await
            .unwrap();

        assert!(before.len() < 1024);
        assert!(after.len() < 1024);
        assert_eq!(unrelated_before, unrelated_after);
        assert_eq!(root_objects.len(), 2);
        assert!(root_objects.iter().all(|object| object.size < 1024));
        assert!(matches!(
            store.get_with_etag(&target.ref_registry_path()).await,
            Err(crab_storage::StorageError::NotFound { .. })
        ));
    }

    #[cfg(feature = "storage")]
    #[tokio::test]
    async fn compaction_reconciliation_preserves_concurrent_candidates() {
        use std::sync::Arc;

        use object_store::ObjectStore;
        use object_store::memory::InMemory;

        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "org/models".to_owned());
        union_register_repo_shards(
            &store,
            &router,
            vec!["source-a".to_owned(), "source-b".to_owned()],
        )
        .await
        .unwrap();
        union_register_repo_shards(&store, &router, vec!["concurrent".to_owned()])
            .await
            .unwrap();

        reconcile_compacted_repo_shards(
            &store,
            &router,
            HashSet::from(["source-a".to_owned(), "source-b".to_owned()]),
            vec!["replacement".to_owned()],
        )
        .await
        .unwrap();

        let (body, _) = store
            .get_with_etag(&router.ref_registry_path())
            .await
            .unwrap();
        let registry: RefRegistry = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            registry.repos["org/models"],
            vec!["concurrent".to_owned(), "replacement".to_owned()]
        );
    }

    #[test]
    fn workflow_round_trip_through_serde() {
        let mut reg = RefRegistry {
            generation: 7,
            ..RefRegistry::default()
        };
        reg.register("org/models", vec!["shard-a".into()]);
        reg.register_workflow_stages("org/models", vec!["stage-a".into(), "stage-b".into()]);
        reg.register_experiments("org/models", vec!["exp-a".into()]);

        let json = serde_json::to_string(&reg).unwrap();
        let parsed: RefRegistry = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.generation, 7);
        assert_eq!(parsed.repos["org/models"], vec!["shard-a".to_string()]);
        assert_eq!(
            parsed.workflow_stage_hashes["org/models"],
            vec!["stage-a".to_string(), "stage-b".to_string()]
        );
        assert_eq!(
            parsed.workflow_experiment_ids["org/models"],
            vec!["exp-a".to_string()]
        );
    }

    #[test]
    fn legacy_payload_without_workflow_fields_deserializes_empty() {
        // Payload shape written before the workflow layer shipped —
        // only `generation` + `repos`. The `#[serde(default)]`
        // attributes MUST make the two workflow maps default to
        // empty so older registries load cleanly without migration.
        let legacy = r#"{
            "generation": 3,
            "repos": {
                "org/models": ["sh1", "sh2"]
            }
        }"#;
        let parsed: RefRegistry = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.schema_version, 0);
        assert!(!parsed.coverage_complete);
        assert!(parsed.complete_repos.is_empty());
        assert_eq!(parsed.generation, 3);
        assert_eq!(parsed.repos.len(), 1);
        assert!(parsed.workflow_stage_hashes.is_empty());
        assert!(parsed.workflow_experiment_ids.is_empty());
        assert!(parsed.active_active_coordinators.is_empty());
        assert!(parsed.all_referenced_workflow_stages().is_empty());
        assert!(parsed.all_referenced_experiments().is_empty());
        assert!(parsed.active_active_repos().is_empty());
    }
}
