use std::future::Future;

use crab_metadata::ref_journal::list_active_transactions;
use crab_metadata::{error::MetadataError, manifest_store, manifests::Manifest};
use crab_storage::{StorageError, Store, StoreLayout};
use crab_types::replication::ReplicaConfig;
use crab_xet::{
    shard_parse::{MAX_SHARD_SIZE_BYTES, extract_chunk_entries_streaming},
    xorb::format::MerkleHash,
};
use object_store::path::Path as ObjectPath;

use crate::{ReadError, Result};

const READ_ROUTING_POLICY_KEY: &str = "read.routing.policy";

/// Default time that replica readiness may trust cached probe results.
pub const DEFAULT_READINESS_CACHE_TTL_MS: u64 = 300_000;

/// Operator policy for selecting the read source for replica-aware operations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ReadRoutingPolicy {
    #[default]
    PreferLocal,
    PreferPrimary,
    ReadDisabled,
    ReplicaName(String),
}

impl ReadRoutingPolicy {
    /// Parse an operator-provided replica read policy.
    pub fn parse(value: &str) -> Result<Self> {
        let trimmed = value.trim();
        if trimmed.eq_ignore_ascii_case("prefer-local")
            || trimmed.eq_ignore_ascii_case("local")
            || trimmed.eq_ignore_ascii_case("auto")
        {
            return Ok(Self::PreferLocal);
        }
        if trimmed.eq_ignore_ascii_case("prefer-primary") || trimmed.eq_ignore_ascii_case("primary")
        {
            return Ok(Self::PreferPrimary);
        }
        if trimmed.eq_ignore_ascii_case("read-disabled") || trimmed.eq_ignore_ascii_case("disabled")
        {
            return Ok(Self::ReadDisabled);
        }
        if let Some(name) = trimmed
            .strip_prefix("replica:")
            .or_else(|| trimmed.strip_prefix("replica-name:"))
        {
            if name.is_empty() {
                return Err(ReadError::Configuration {
                    key: READ_ROUTING_POLICY_KEY.into(),
                    origin: "replica read policy requires a replica name after replica:".into(),
                });
            }
            return Ok(Self::ReplicaName(name.to_owned()));
        }

        Err(ReadError::Configuration {
            key: READ_ROUTING_POLICY_KEY.into(),
            origin: format!(
                "unsupported replica read policy {trimmed}; expected prefer-local, prefer-primary, read-disabled, or replica:<name>"
            ),
        })
    }
}

/// Replica candidate considered by read-source policy selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadReplicaCandidate<T> {
    name: String,
    read_enabled: bool,
    target: T,
}

impl<T> ReadReplicaCandidate<T> {
    /// Creates a replica candidate with the name and read-enabled flag used by
    /// operator policy.
    pub fn new(name: impl Into<String>, read_enabled: bool, target: T) -> Self {
        Self {
            name: name.into(),
            read_enabled,
            target,
        }
    }
}

impl ReadReplicaCandidate<ReplicaConfig> {
    /// Creates a read candidate whose target is the owned persisted replica config.
    #[must_use]
    pub fn from_replica_config(replica: ReplicaConfig) -> Self {
        let name = replica.name.clone();
        let read_enabled = replica.read;
        Self::new(name, read_enabled, replica)
    }
}

impl<'a> ReadReplicaCandidate<&'a ReplicaConfig> {
    /// Creates a read candidate whose target is a borrowed persisted replica config.
    #[must_use]
    pub fn from_replica_config_ref(replica: &'a ReplicaConfig) -> Self {
        Self::new(replica.name.clone(), replica.read, replica)
    }
}

/// Selects the replicas that a read operation should consider for the policy.
pub fn select_read_replicas<T, I>(replicas: I, policy: &ReadRoutingPolicy) -> Result<Vec<T>>
where
    I: IntoIterator<Item = ReadReplicaCandidate<T>>,
{
    let replicas = replicas.into_iter().collect::<Vec<_>>();
    match policy {
        ReadRoutingPolicy::PreferLocal => Ok(replicas
            .into_iter()
            .filter(|replica| replica.read_enabled)
            .map(|replica| replica.target)
            .collect()),
        ReadRoutingPolicy::ReplicaName(name) => {
            let Some(replica) = replicas.into_iter().find(|replica| replica.name == *name) else {
                return Err(ReadError::Configuration {
                    key: "replication.replicas".into(),
                    origin: format!("replica {name} is not configured"),
                });
            };
            if !replica.read_enabled {
                return Err(ReadError::Configuration {
                    key: "replication.replicas.read".into(),
                    origin: format!(
                        "replica {name} is disabled for read routing; enable it before forcing reads to that replica"
                    ),
                });
            }
            Ok(vec![replica.target])
        }
        ReadRoutingPolicy::PreferPrimary | ReadRoutingPolicy::ReadDisabled => Ok(Vec::new()),
    }
}

/// Replica selected as ready for a read operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyReadReplica<T> {
    pub name: String,
    pub repo_prefix: String,
    pub target: T,
    pub primary_generation: Option<u64>,
    pub replica_generation: Option<u64>,
}

/// Replica that was considered but could not serve the read operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadReplicaFallback<T> {
    pub name: String,
    pub repo_prefix: String,
    pub target: T,
    pub primary_generation: Option<u64>,
    pub replica_generation: Option<u64>,
    pub reason: Option<String>,
}

/// Controls whether replica readiness may trust local cache state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadinessCheckOptions {
    pub bypass_cache: bool,
    pub cache_ttl_ms: u64,
    pub max_object_probes: Option<u64>,
}

impl Default for ReadinessCheckOptions {
    fn default() -> Self {
        Self {
            bypass_cache: false,
            cache_ttl_ms: DEFAULT_READINESS_CACHE_TTL_MS,
            max_object_probes: None,
        }
    }
}

impl ReadinessCheckOptions {
    #[must_use]
    pub fn deep() -> Self {
        Self {
            bypass_cache: true,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn sampled(max_object_probes: u64) -> Self {
        Self {
            bypass_cache: true,
            max_object_probes: Some(max_object_probes),
            ..Self::default()
        }
    }
}

/// Counts remote object reads and existence probes performed during a replica
/// readiness check.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadinessProbeStats {
    pub object_probe_count: u64,
    pub object_read_count: u64,
}

/// Object-level readiness proof for one replica against a primary manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadReplicaReadiness {
    pub primary_generation: u64,
    pub replica_generation: Option<u64>,
    pub ready: bool,
    pub lag_generations: Option<u64>,
    pub reason: Option<String>,
    pub stats: ReadinessProbeStats,
}

impl ReadReplicaReadiness {
    fn ready(primary_generation: u64, replica_generation: u64, stats: ReadinessProbeStats) -> Self {
        Self {
            primary_generation,
            replica_generation: Some(replica_generation),
            ready: true,
            lag_generations: Some(replica_generation.saturating_sub(primary_generation)),
            reason: None,
            stats,
        }
    }

    fn not_ready(
        primary_generation: u64,
        replica_generation: Option<u64>,
        reason: impl Into<String>,
        stats: ReadinessProbeStats,
    ) -> Self {
        Self {
            primary_generation,
            replica_generation,
            ready: false,
            lag_generations: replica_generation
                .map(|replica_generation| primary_generation.saturating_sub(replica_generation)),
            reason: Some(reason.into()),
            stats,
        }
    }
}

/// Checks whether a replica has a manifest and referenced immutable objects at
/// least as fresh as the primary manifest.
pub async fn check_read_replica_readiness(
    primary_store: &Store,
    primary_router: &StoreLayout<Store>,
    replica_store: &Store,
    replica_router: &StoreLayout<Store>,
    options: ReadinessCheckOptions,
) -> Result<ReadReplicaReadiness> {
    let mut stats = ReadinessProbeStats::default();
    // Capture journal visibility before the manifest, matching repository
    // snapshot ordering without loading the primary's pack and shard indexes.
    let primary_active = list_active_transactions(primary_store, primary_router).await?;
    let (primary_manifest, _) =
        manifest_store::read_manifest(primary_store, primary_router).await?;
    let primary_generation = primary_manifest.generation;

    let replica_manifest = match manifest_store::read_manifest(replica_store, replica_router).await
    {
        Ok((manifest, _)) => manifest,
        Err(error) => {
            return Ok(ReadReplicaReadiness::not_ready(
                primary_generation,
                None,
                format!("replica manifest unavailable: {error}"),
                stats,
            ));
        }
    };

    if !primary_active.is_empty() {
        return Ok(ReadReplicaReadiness::not_ready(
            primary_generation,
            Some(replica_manifest.generation),
            "primary has uncompacted ref transactions",
            stats,
        ));
    }

    if replica_manifest.generation < primary_generation {
        return Ok(ReadReplicaReadiness::not_ready(
            primary_generation,
            Some(replica_manifest.generation),
            "replica manifest is stale",
            stats,
        ));
    }

    if let Some(reason) = referenced_object_gap(
        replica_store,
        replica_router,
        &replica_manifest,
        &mut stats,
        options,
    )
    .await?
    {
        return Ok(ReadReplicaReadiness::not_ready(
            primary_generation,
            Some(replica_manifest.generation),
            reason,
            stats,
        ));
    }

    Ok(ReadReplicaReadiness::ready(
        primary_generation,
        replica_manifest.generation,
        stats,
    ))
}

async fn referenced_object_gap(
    store: &Store,
    router: &StoreLayout<Store>,
    manifest: &Manifest,
    stats: &mut ReadinessProbeStats,
    options: ReadinessCheckOptions,
) -> Result<Option<String>> {
    if !manifest.pack_index_hash.is_empty() {
        stats.object_read_count = stats.object_read_count.saturating_add(1);
        let packs =
            match manifest_store::read_bulk_pack_list(store, router, &manifest.pack_index_hash)
                .await
            {
                Ok(packs) => packs,
                Err(MetadataError::Storage {
                    source: StorageError::NotFound { .. },
                }) => return Ok(Some("pack index missing".to_owned())),
                Err(error) => return Err(error.into()),
            };
        for pack in packs {
            if readiness_probe_budget_exhausted(stats, options) {
                return Ok(None);
            }
            let pack_path = router.pack_path(&pack.pack_id);
            if let Some(reason) = missing_head(store, &pack_path, "pack", stats).await? {
                return Ok(Some(reason));
            }
            if readiness_probe_budget_exhausted(stats, options) {
                return Ok(None);
            }
            let meta_path = router.pack_metadata_path(&pack.pack_id);
            if let Some(reason) = missing_head(store, &meta_path, "pack metadata", stats).await? {
                return Ok(Some(reason));
            }
        }
    }

    if !manifest.shard_index_hash.is_empty() {
        stats.object_read_count = stats.object_read_count.saturating_add(1);
        let shards =
            match manifest_store::read_bulk_shard_list(store, router, &manifest.shard_index_hash)
                .await
            {
                Ok(shards) => shards,
                Err(MetadataError::Storage {
                    source: StorageError::NotFound { .. },
                }) => return Ok(Some("shard index missing".to_owned())),
                Err(error) => return Err(error.into()),
            };
        for shard in shards {
            if readiness_probe_budget_exhausted(stats, options) {
                return Ok(None);
            }
            let shard_hash = parse_merkle_hash(&shard, "shard")?;
            let shard_path = router.shard_path(&shard_hash);
            stats.object_read_count = stats.object_read_count.saturating_add(1);
            let shard_bytes = match store
                .get_with_etag_bounded(&shard_path, MAX_SHARD_SIZE_BYTES as u64)
                .await
            {
                Ok((bytes, _etag)) => bytes,
                Err(StorageError::NotFound { .. }) => {
                    return Ok(Some(format!("shard missing at {}", shard_path.as_ref())));
                }
                Err(error) => return Err(error.into()),
            };
            let mut xorb_hashes = Vec::new();
            for (_chunk_hash, xorb) in extract_chunk_entries_streaming(&shard_bytes) {
                if !xorb_hashes.contains(&xorb.xorb_hash) {
                    xorb_hashes.push(xorb.xorb_hash);
                }
            }
            for xorb_hash in xorb_hashes {
                if readiness_probe_budget_exhausted(stats, options) {
                    return Ok(None);
                }
                let xorb_path = router.xorb_path(&xorb_hash);
                if let Some(reason) = missing_head(store, &xorb_path, "xorb", stats).await? {
                    return Ok(Some(reason));
                }
            }
        }
    }

    Ok(None)
}

fn readiness_probe_budget_exhausted(
    stats: &ReadinessProbeStats,
    options: ReadinessCheckOptions,
) -> bool {
    options
        .max_object_probes
        .is_some_and(|max| stats.object_probe_count >= max)
}

fn parse_merkle_hash(value: &str, label: &str) -> Result<MerkleHash> {
    MerkleHash::from_hex(value).map_err(|error| ReadError::CorruptObject {
        path: label.to_owned(),
        reason: format!("invalid {label} hash {value}: {error}"),
    })
}

async fn missing_head(
    store: &Store,
    path: &ObjectPath,
    label: &str,
    stats: &mut ReadinessProbeStats,
) -> Result<Option<String>> {
    stats.object_probe_count = stats.object_probe_count.saturating_add(1);
    match store.head(path).await {
        Ok(_) => Ok(None),
        Err(StorageError::NotFound { .. }) => {
            Ok(Some(format!("{label} missing at {}", path.as_ref())))
        }
        Err(error) => Err(error.into()),
    }
}

/// Result of probing one replica candidate for a read operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadReplicaProbeResult<T, F> {
    Ready(ReadyReadReplica<T>),
    Fallback(ReadReplicaFallback<F>),
}

impl<T, F> ReadReplicaProbeResult<T, F> {
    /// Creates a ready probe result for a replica target.
    pub fn ready(
        name: impl Into<String>,
        repo_prefix: impl Into<String>,
        target: T,
        primary_generation: Option<u64>,
        replica_generation: Option<u64>,
    ) -> Self {
        Self::Ready(ReadyReadReplica {
            name: name.into(),
            repo_prefix: repo_prefix.into(),
            target,
            primary_generation,
            replica_generation,
        })
    }

    /// Creates a fallback probe result for a candidate that could not serve a read.
    pub fn fallback(
        name: impl Into<String>,
        repo_prefix: impl Into<String>,
        target: F,
        primary_generation: Option<u64>,
        replica_generation: Option<u64>,
        reason: Option<String>,
    ) -> Self {
        Self::Fallback(ReadReplicaFallback {
            name: name.into(),
            repo_prefix: repo_prefix.into(),
            target,
            primary_generation,
            replica_generation,
            reason,
        })
    }

    /// Converts object-level readiness into the read-selection probe shape.
    pub fn from_readiness(
        name: impl Into<String>,
        repo_prefix: impl Into<String>,
        ready_target: T,
        fallback_target: F,
        readiness: ReadReplicaReadiness,
    ) -> Self {
        let name = name.into();
        let repo_prefix = repo_prefix.into();
        if readiness.ready {
            Self::ready(
                name,
                repo_prefix,
                ready_target,
                Some(readiness.primary_generation),
                readiness.replica_generation,
            )
        } else {
            Self::fallback(
                name,
                repo_prefix,
                fallback_target,
                Some(readiness.primary_generation),
                readiness.replica_generation,
                readiness.reason,
            )
        }
    }
}

/// Result of readiness-aware read-replica selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadReplicaSelection<T, F> {
    Replica {
        selected: ReadyReadReplica<T>,
        fallbacks: Vec<ReadReplicaFallback<F>>,
    },
    Primary {
        fallbacks: Vec<ReadReplicaFallback<F>>,
    },
}

/// Final read-source choice after policy filtering and readiness probing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadStoreChoice<Primary, Replica, Fallback> {
    Primary {
        primary: Primary,
        fallbacks: Vec<ReadReplicaFallback<Fallback>>,
    },
    Replica {
        selected: ReadyReadReplica<Replica>,
        fallbacks: Vec<ReadReplicaFallback<Fallback>>,
    },
}

/// Selects the first ready read replica, falling back to primary otherwise.
pub async fn select_ready_read_replica<C, T, Fallback, I, Probe, ProbeFuture>(
    candidates: I,
    mut probe: Probe,
) -> ReadReplicaSelection<T, Fallback>
where
    I: IntoIterator<Item = C>,
    Probe: FnMut(C) -> ProbeFuture,
    ProbeFuture: Future<Output = ReadReplicaProbeResult<T, Fallback>>,
{
    let mut fallbacks = Vec::new();
    for candidate in candidates {
        match probe(candidate).await {
            ReadReplicaProbeResult::Ready(selected) => {
                return ReadReplicaSelection::Replica {
                    selected,
                    fallbacks,
                };
            }
            ReadReplicaProbeResult::Fallback(fallback) => fallbacks.push(fallback),
        }
    }
    ReadReplicaSelection::Primary { fallbacks }
}

/// Selects a read store target according to operator policy and replica readiness.
pub async fn select_read_store_choice<C, Primary, Replica, Fallback, I, Probe, ProbeFuture>(
    primary: Primary,
    replicas: I,
    policy: &ReadRoutingPolicy,
    probe: Probe,
) -> Result<ReadStoreChoice<Primary, Replica, Fallback>>
where
    I: IntoIterator<Item = ReadReplicaCandidate<C>>,
    Probe: FnMut(C) -> ProbeFuture,
    ProbeFuture: Future<Output = ReadReplicaProbeResult<Replica, Fallback>>,
{
    if matches!(
        policy,
        ReadRoutingPolicy::PreferPrimary | ReadRoutingPolicy::ReadDisabled
    ) {
        return Ok(ReadStoreChoice::Primary {
            primary,
            fallbacks: Vec::new(),
        });
    }

    let replicas = select_read_replicas(replicas, policy)?;
    let selection = select_ready_read_replica(replicas, probe).await;
    Ok(match selection {
        ReadReplicaSelection::Replica {
            selected,
            fallbacks,
        } => ReadStoreChoice::Replica {
            selected,
            fallbacks,
        },
        ReadReplicaSelection::Primary { fallbacks } => {
            ReadStoreChoice::Primary { primary, fallbacks }
        }
    })
}

/// Storage source selected for a read operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadSource {
    Primary,
    Replica { name: String },
}

/// Concrete store/router pair that may become a selected read source.
pub struct ReadStoreTarget<Store, Router> {
    pub store: Store,
    pub router: Router,
}

impl<Store, Router> ReadStoreTarget<Store, Router> {
    /// Creates a source-neutral read target from an already built store/router pair.
    #[must_use]
    pub fn new(store: Store, router: Router) -> Self {
        Self { store, router }
    }

    /// Converts this target into a primary read selection.
    #[must_use]
    pub fn into_primary_selection(self) -> ReadStoreSelection<Store, Router> {
        ReadStoreSelection {
            store: self.store,
            router: self.router,
            source: ReadSource::Primary,
        }
    }

    /// Converts this target into a replica read selection.
    #[must_use]
    pub fn into_replica_selection(
        self,
        name: impl Into<String>,
    ) -> ReadStoreSelection<Store, Router> {
        ReadStoreSelection {
            store: self.store,
            router: self.router,
            source: ReadSource::Replica { name: name.into() },
        }
    }
}

/// Result of selecting a concrete store/router pair for a read operation.
pub struct ReadStoreSelection<Store, Router> {
    pub store: Store,
    pub router: Router,
    pub source: ReadSource,
}

impl<Store, Router> ReadStoreSelection<Store, Router> {
    /// Creates a primary read selection from an already built store/router pair.
    #[must_use]
    pub fn primary(store: Store, router: Router) -> Self {
        ReadStoreTarget::new(store, router).into_primary_selection()
    }

    /// Creates a replica read selection from an already built store/router pair.
    #[must_use]
    pub fn replica(store: Store, router: Router, name: impl Into<String>) -> Self {
        ReadStoreTarget::new(store, router).into_replica_selection(name)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use bytes::Bytes;
    use crab_metadata::{
        manifest_store::create_manifest,
        manifests::{Manifest, PackManifestEntry, compact_pack_index},
        ref_journal::{
            RefJournalEdit, RefJournalTransaction, commit_ref_transaction, read_ref_head,
        },
        segmented_store,
    };
    use object_store::memory::InMemory;

    use super::*;

    #[test]
    fn read_policy_parses_operator_values() {
        assert_eq!(
            ReadRoutingPolicy::parse("prefer-local").unwrap(),
            ReadRoutingPolicy::PreferLocal
        );
        assert_eq!(
            ReadRoutingPolicy::parse("primary").unwrap(),
            ReadRoutingPolicy::PreferPrimary
        );
        assert_eq!(
            ReadRoutingPolicy::parse("disabled").unwrap(),
            ReadRoutingPolicy::ReadDisabled
        );
        assert_eq!(
            ReadRoutingPolicy::parse("replica:west").unwrap(),
            ReadRoutingPolicy::ReplicaName("west".into())
        );
    }

    #[test]
    fn read_policy_rejects_empty_replica_name() {
        let error = ReadRoutingPolicy::parse("replica:").unwrap_err();

        assert!(error.to_string().contains("requires a replica name"));
    }

    fn replica_config(name: &str, read: bool) -> ReplicaConfig {
        ReplicaConfig {
            name: name.to_owned(),
            provider: crab_types::replication::ReplicationProviderKind::S3,
            url: format!("s3://bucket/{name}"),
            region: "us-west-2".to_owned(),
            backfill: false,
            read,
            rpo: crab_types::replication::ReplicationRpo::Standard,
        }
    }

    #[test]
    fn replica_config_candidate_derives_name_and_read_flag() {
        let west = replica_config("west", true);
        let disabled = replica_config("disabled", false);

        let selected = select_read_replicas(
            [
                ReadReplicaCandidate::from_replica_config(west),
                ReadReplicaCandidate::from_replica_config(disabled),
            ],
            &ReadRoutingPolicy::PreferLocal,
        )
        .unwrap();

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "west");
    }

    #[test]
    fn borrowed_replica_config_candidate_preserves_target_reference() {
        let west = replica_config("west", true);
        let east = replica_config("east", true);

        let selected = select_read_replicas(
            [
                ReadReplicaCandidate::from_replica_config_ref(&east),
                ReadReplicaCandidate::from_replica_config_ref(&west),
            ],
            &ReadRoutingPolicy::ReplicaName("west".to_owned()),
        )
        .unwrap();

        assert_eq!(selected, vec![&west]);
    }

    #[test]
    fn read_policy_keeps_enabled_replicas_in_order() {
        let selected = select_read_replicas(
            [
                ReadReplicaCandidate::new("east", true, "east-target"),
                ReadReplicaCandidate::new("disabled", false, "disabled-target"),
                ReadReplicaCandidate::new("west", true, "west-target"),
            ],
            &ReadRoutingPolicy::PreferLocal,
        )
        .unwrap();

        assert_eq!(selected, ["east-target", "west-target"]);
    }

    #[test]
    fn read_policy_can_force_named_replica() {
        let selected = select_read_replicas(
            [
                ReadReplicaCandidate::new("east", true, "east-target"),
                ReadReplicaCandidate::new("west", true, "west-target"),
            ],
            &ReadRoutingPolicy::ReplicaName("west".into()),
        )
        .unwrap();

        assert_eq!(selected, ["west-target"]);
    }

    #[test]
    fn read_policy_rejects_disabled_named_replica() {
        let error = select_read_replicas(
            [
                ReadReplicaCandidate::new("east", true, "east-target"),
                ReadReplicaCandidate::new("west", false, "west-target"),
            ],
            &ReadRoutingPolicy::ReplicaName("west".into()),
        )
        .unwrap_err();

        assert!(error.to_string().contains("disabled for read routing"));
    }

    #[test]
    fn primary_policies_skip_replicas() {
        for policy in [
            ReadRoutingPolicy::PreferPrimary,
            ReadRoutingPolicy::ReadDisabled,
        ] {
            let selected = select_read_replicas(
                [ReadReplicaCandidate::new("east", true, "east-target")],
                &policy,
            )
            .unwrap();

            assert!(selected.is_empty());
        }
    }

    #[tokio::test]
    async fn readiness_selection_picks_first_ready_candidate() {
        let selection = select_ready_read_replica(["east", "west"], |candidate| async move {
            if candidate == "west" {
                return ReadReplicaProbeResult::Ready(ReadyReadReplica {
                    name: candidate.to_owned(),
                    repo_prefix: "org/repo".to_owned(),
                    target: "west-target",
                    primary_generation: Some(7),
                    replica_generation: Some(7),
                });
            }
            ReadReplicaProbeResult::Fallback(ReadReplicaFallback {
                name: candidate.to_owned(),
                repo_prefix: "org/repo".to_owned(),
                target: candidate,
                primary_generation: Some(7),
                replica_generation: Some(6),
                reason: Some("replica behind primary".to_owned()),
            })
        })
        .await;

        match selection {
            ReadReplicaSelection::Replica {
                selected,
                fallbacks,
            } => {
                assert_eq!(selected.target, "west-target");
                assert_eq!(fallbacks.len(), 1);
                assert_eq!(fallbacks[0].target, "east");
            }
            ReadReplicaSelection::Primary { .. } => panic!("expected ready replica"),
        }
    }

    #[test]
    fn readiness_options_default_trusts_cache() {
        let options = ReadinessCheckOptions::default();

        assert!(!options.bypass_cache);
        assert_eq!(options.cache_ttl_ms, DEFAULT_READINESS_CACHE_TTL_MS);
        assert!(options.max_object_probes.is_none());
    }

    #[test]
    fn readiness_options_sampled_forces_deep_limited_probe() {
        let options = ReadinessCheckOptions::sampled(8);

        assert!(options.bypass_cache);
        assert_eq!(options.max_object_probes, Some(8));
    }

    #[tokio::test]
    async fn readiness_check_accepts_replica_after_pack_objects_arrive() {
        let (primary_store, primary_router) = memory_store_with_layout("org/repo");
        let (replica_store, replica_router) = memory_store_with_layout("org/repo");
        let pack_id = "a".repeat(64);
        let pack = test_pack_entry(&pack_id);
        let (pack_index_hash, _index, pack_write) =
            compact_pack_index(7, std::slice::from_ref(&pack)).expect("build pack index");
        let mut manifest = test_manifest(7);
        manifest.pack_index_hash = pack_index_hash;
        manifest.seal_git_validation();
        write_test_manifest(&primary_store, &primary_router, &manifest).await;
        write_test_manifest(&replica_store, &replica_router, &manifest).await;
        segmented_store::upload_write(&replica_store, &replica_router, &pack_write)
            .await
            .expect("upload pack index");
        replica_store
            .put(
                &replica_router.pack_path(&pack_id),
                Bytes::from_static(b"pack"),
            )
            .await
            .expect("upload pack object");
        replica_store
            .put(
                &replica_router.pack_metadata_path(&pack_id),
                Bytes::from_static(b"meta"),
            )
            .await
            .expect("upload pack metadata");

        let readiness = check_read_replica_readiness(
            &primary_store,
            &primary_router,
            &replica_store,
            &replica_router,
            ReadinessCheckOptions::deep(),
        )
        .await
        .expect("readiness check");

        assert!(readiness.ready);
        assert_eq!(readiness.primary_generation, 7);
        assert_eq!(readiness.replica_generation, Some(7));
        assert_eq!(readiness.stats.object_read_count, 1);
        assert_eq!(readiness.stats.object_probe_count, 2);
    }

    #[tokio::test]
    async fn readiness_check_reports_missing_referenced_pack() {
        let (primary_store, primary_router) = memory_store_with_layout("org/repo");
        let (replica_store, replica_router) = memory_store_with_layout("org/repo");
        let pack = test_pack_entry(&"b".repeat(64));
        let (pack_index_hash, _index, pack_write) =
            compact_pack_index(8, std::slice::from_ref(&pack)).expect("build pack index");
        let mut manifest = test_manifest(8);
        manifest.pack_index_hash = pack_index_hash;
        manifest.seal_git_validation();
        write_test_manifest(&primary_store, &primary_router, &manifest).await;
        write_test_manifest(&replica_store, &replica_router, &manifest).await;
        segmented_store::upload_write(&replica_store, &replica_router, &pack_write)
            .await
            .expect("upload pack index");

        let readiness = check_read_replica_readiness(
            &primary_store,
            &primary_router,
            &replica_store,
            &replica_router,
            ReadinessCheckOptions::deep(),
        )
        .await
        .expect("readiness check");

        assert!(!readiness.ready);
        assert_eq!(readiness.primary_generation, 8);
        assert_eq!(readiness.replica_generation, Some(8));
        assert!(
            readiness
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("pack missing"))
        );
        assert_eq!(readiness.stats.object_read_count, 1);
        assert_eq!(readiness.stats.object_probe_count, 1);
    }

    #[tokio::test]
    async fn readiness_check_rejects_replica_while_primary_journal_is_uncompacted() {
        let (primary_store, primary_router) = memory_store_with_layout("org/repo");
        let (replica_store, replica_router) = memory_store_with_layout("org/repo");
        let manifest = test_manifest(9);
        write_test_manifest(&primary_store, &primary_router, &manifest).await;
        write_test_manifest(&replica_store, &replica_router, &manifest).await;

        let ref_name = "refs/heads/main";
        let head = read_ref_head(&primary_store, &primary_router, ref_name)
            .await
            .expect("read ref head");
        let transaction = RefJournalTransaction::new(
            BTreeMap::from([(ref_name.to_owned(), head.visible_transaction.clone())]),
            vec![RefJournalEdit {
                ref_name: ref_name.to_owned(),
                old_oid: None,
                new_oid: Some("c".repeat(40)),
                peeled_oid: None,
                lock_holder: None,
                visibility_evidence_hash: None,
            }],
            None,
            Vec::new(),
            Vec::new(),
        )
        .expect("build transaction");
        commit_ref_transaction(&primary_store, &primary_router, &transaction, &[head])
            .await
            .expect("commit transaction");

        let readiness = check_read_replica_readiness(
            &primary_store,
            &primary_router,
            &replica_store,
            &replica_router,
            ReadinessCheckOptions::deep(),
        )
        .await
        .expect("readiness check");

        assert!(!readiness.ready);
        assert_eq!(readiness.primary_generation, 9);
        assert_eq!(readiness.replica_generation, Some(9));
        assert_eq!(
            readiness.reason.as_deref(),
            Some("primary has uncompacted ref transactions")
        );
    }

    #[test]
    fn probe_result_conversion_keeps_readiness_shape_owned() {
        let ready = ReadReplicaProbeResult::from_readiness(
            "west",
            "org/repo",
            "ready-target",
            "fallback-target",
            ReadReplicaReadiness::ready(9, 9, ReadinessProbeStats::default()),
        );

        match ready {
            ReadReplicaProbeResult::Ready(selected) => {
                assert_eq!(selected.name, "west");
                assert_eq!(selected.target, "ready-target");
                assert_eq!(selected.primary_generation, Some(9));
                assert_eq!(selected.replica_generation, Some(9));
            }
            ReadReplicaProbeResult::Fallback(_) => panic!("expected ready replica"),
        }

        let fallback = ReadReplicaProbeResult::from_readiness(
            "east",
            "org/repo",
            "ready-target",
            "fallback-target",
            ReadReplicaReadiness::not_ready(
                9,
                Some(8),
                "replica manifest is stale",
                ReadinessProbeStats::default(),
            ),
        );

        match fallback {
            ReadReplicaProbeResult::Fallback(fallback) => {
                assert_eq!(fallback.name, "east");
                assert_eq!(fallback.target, "fallback-target");
                assert_eq!(fallback.primary_generation, Some(9));
                assert_eq!(fallback.replica_generation, Some(8));
                assert_eq!(
                    fallback.reason.as_deref(),
                    Some("replica manifest is stale")
                );
            }
            ReadReplicaProbeResult::Ready(_) => panic!("expected fallback"),
        }
    }

    #[tokio::test]
    async fn readiness_selection_falls_back_to_primary() {
        let selection = select_ready_read_replica(["east", "west"], |candidate| async move {
            ReadReplicaProbeResult::<&'static str, _>::Fallback(ReadReplicaFallback {
                name: candidate.to_owned(),
                repo_prefix: "org/repo".to_owned(),
                target: candidate,
                primary_generation: Some(7),
                replica_generation: Some(6),
                reason: Some("replica behind primary".to_owned()),
            })
        })
        .await;

        match selection {
            ReadReplicaSelection::Primary { fallbacks } => {
                assert_eq!(fallbacks.len(), 2);
                assert_eq!(fallbacks[0].target, "east");
                assert_eq!(fallbacks[1].target, "west");
            }
            ReadReplicaSelection::Replica { .. } => panic!("expected primary fallback"),
        }
    }

    #[tokio::test]
    async fn read_store_choice_skips_probe_for_primary_policy() {
        let choice: ReadStoreChoice<&str, &str, &str> = select_read_store_choice(
            "primary-target",
            [ReadReplicaCandidate::new("east", true, "east-target")],
            &ReadRoutingPolicy::PreferPrimary,
            |_| async {
                panic!("primary policy must not probe replicas");
            },
        )
        .await
        .unwrap();

        match choice {
            ReadStoreChoice::Primary { primary, fallbacks } => {
                assert_eq!(primary, "primary-target");
                assert!(fallbacks.is_empty());
            }
            ReadStoreChoice::Replica { .. } => panic!("expected primary"),
        }
    }

    #[tokio::test]
    async fn read_store_choice_returns_ready_replica_with_fallbacks() {
        let choice = select_read_store_choice(
            "primary-target",
            [
                ReadReplicaCandidate::new("east", true, "east-target"),
                ReadReplicaCandidate::new("west", true, "west-target"),
            ],
            &ReadRoutingPolicy::PreferLocal,
            |candidate| async move {
                if candidate == "west-target" {
                    return ReadReplicaProbeResult::Ready(ReadyReadReplica {
                        name: "west".to_owned(),
                        repo_prefix: "org/repo".to_owned(),
                        target: candidate,
                        primary_generation: Some(2),
                        replica_generation: Some(2),
                    });
                }
                ReadReplicaProbeResult::Fallback(ReadReplicaFallback {
                    name: "east".to_owned(),
                    repo_prefix: "org/repo".to_owned(),
                    target: candidate,
                    primary_generation: Some(2),
                    replica_generation: Some(1),
                    reason: Some("replica behind primary".to_owned()),
                })
            },
        )
        .await
        .unwrap();

        match choice {
            ReadStoreChoice::Replica {
                selected,
                fallbacks,
            } => {
                assert_eq!(selected.target, "west-target");
                assert_eq!(fallbacks.len(), 1);
                assert_eq!(fallbacks[0].target, "east-target");
            }
            ReadStoreChoice::Primary { .. } => panic!("expected ready replica"),
        }
    }

    #[test]
    fn read_store_target_converts_to_primary_selection() {
        let selection = ReadStoreTarget::new("store", "router").into_primary_selection();

        assert_eq!(selection.store, "store");
        assert_eq!(selection.router, "router");
        assert_eq!(selection.source, ReadSource::Primary);
    }

    #[test]
    fn read_store_target_converts_to_replica_selection() {
        let selection = ReadStoreSelection::replica("store", "router", "west");

        assert_eq!(selection.store, "store");
        assert_eq!(selection.router, "router");
        assert_eq!(
            selection.source,
            ReadSource::Replica {
                name: "west".to_owned()
            }
        );
    }

    fn memory_store_with_layout(repo_prefix: &str) -> (Store, StoreLayout<Store>) {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), repo_prefix.to_owned());
        (store, router)
    }

    fn test_manifest(generation: u64) -> Manifest {
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = generation;
        manifest.seal_git_validation();
        manifest
    }

    async fn write_test_manifest(store: &Store, router: &StoreLayout<Store>, manifest: &Manifest) {
        create_manifest(store, router, manifest)
            .await
            .expect("write test manifest");
    }

    fn test_pack_entry(pack_id: &str) -> PackManifestEntry {
        PackManifestEntry {
            pack_id: pack_id.to_owned(),
            size: 42,
            content_hash: pack_id.to_owned(),
            ref_tips: vec!["b".repeat(40)],
            object_count: 1,
        }
    }
}
