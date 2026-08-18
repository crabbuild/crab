//! Ref registry: maps repo prefixes to their referenced global shard sets.
//!
//! The ref-registry is a JSON manifest at `.crab/ref-registry` tracking
//! which repos reference which global shards. It enables safe garbage
//! collection without scanning every repo's shard-list on every GC run.
//!
//! Updated via CAS after each successful push. The push pipeline reads the
//! current shard-list for the repo and writes the full set to the registry
//! (not just the delta — the entry is the complete shard-list for that repo).

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

#[cfg(feature = "storage")]
use crate::error::{MetadataError, Result};
#[cfg(feature = "storage")]
use crab_storage::{Store, StoreLayout};

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

/// CAS-union a repo's base-plus-candidate shard set before manifest publish.
#[cfg(feature = "storage")]
pub async fn union_register_repo_shards(
    store: &Store,
    router: &StoreLayout<Store>,
    shard_hashes: Vec<String>,
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
            registry.register_union(&repo_prefix, shard_hashes.clone());
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
    let registry_path = router.ref_registry_path();
    let repo_prefix = router.repo_prefix().to_owned();
    crab_storage::cas::cas_update_default::<RefRegistry, _>(
        store,
        registry_path.as_ref(),
        |registry| {
            let schema_was_current = registry.schema_version == REF_REGISTRY_SCHEMA_VERSION;
            let changed = registry.register_workflow_union(
                &repo_prefix,
                stage_hashes.clone(),
                exp_ids.clone(),
            );
            if changed || !schema_was_current {
                registry.generation += 1;
            }
        },
    )
    .await
    .map(|registry| registry.generation)
    .map_err(MetadataError::from)
}

/// Union repo shard entries from a bucket-wide manifest scan and mark coverage complete.
///
/// Repair cannot replace the registry wholesale: an ordinary push registers
/// candidate shards before manifest CAS, so clearing entries after the scan
/// could erase that concurrent writer's only GC protection. Extra roots are
/// safe and compaction remains the owner of exact replacement.
#[cfg(feature = "storage")]
pub async fn repair_ref_registry_from_manifests(
    store: &Store,
    router: &StoreLayout<Store>,
    repos: HashMap<String, Vec<String>>,
) -> Result<()> {
    let registry_path = router.ref_registry_path();
    crab_storage::cas::cas_update_default::<RefRegistry, _>(
        store,
        registry_path.as_ref(),
        |registry| {
            for (repo, shards) in &repos {
                registry.register_union(repo, shards.clone());
            }
            registry.mark_coverage_complete();
            registry.generation += 1;
        },
    )
    .await
    .map(|_| ())
    .map_err(MetadataError::from)
}

/// Registers an active-active coordinator in the bucket ref-registry.
#[cfg(feature = "storage")]
pub async fn register_active_active_coordinator_for_repo(
    store: &Store,
    router: &StoreLayout<Store>,
    registration: ActiveActiveCoordinatorRegistration,
) -> Result<()> {
    let registry_path = router.ref_registry_path();
    let repo_prefix = router.repo_prefix().to_owned();
    crab_storage::cas::cas_update_default::<RefRegistry, _>(
        store,
        registry_path.as_ref(),
        |registry| {
            if registry.active_active_coordinators.get(&repo_prefix) != Some(&registration) {
                registry.register_active_active_coordinator(&repo_prefix, registration.clone());
                registry.generation += 1;
            }
        },
    )
    .await
    .map(|_| ())
    .map_err(MetadataError::from)
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

        let (body, _) = store
            .get_with_etag(&router.ref_registry_path())
            .await
            .unwrap();
        let registry: RefRegistry = serde_json::from_slice(&body).unwrap();
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

        let (body, _) = store
            .get_with_etag(&router.ref_registry_path())
            .await
            .unwrap();
        let registry: RefRegistry = serde_json::from_slice(&body).unwrap();
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
    async fn manifest_repair_preserves_concurrent_pre_cas_roots() {
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

        let (body, _) = store
            .get_with_etag(&router.ref_registry_path())
            .await
            .unwrap();
        let registry: RefRegistry = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            registry.repos["org/models"],
            vec!["base".to_owned(), "candidate".to_owned()]
        );
        assert!(registry.is_complete_for_destructive_gc());
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
