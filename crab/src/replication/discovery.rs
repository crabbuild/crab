use bytes::Bytes;
use serde::{Deserialize, Serialize};

use super::{ReplicaConfig, ReplicationConfig, validate_replica_url_provider};
use crate::core::config::Config;
use crate::core::error::{CrabError, Result};
use crate::git::url::CrabUrl;
use crate::storage::StoreLayout;
use crate::storage::store::Store;

const REPLICA_DISCOVERY_VERSION: u32 = 1;
const MAX_REPLICA_DISCOVERY_BYTES: u64 = 64 * 1024;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct ReplicaDiscoveryDocument {
    version: u32,
    primary: String,
    replicas: Vec<ReplicaConfig>,
}

/// Publish the read-routing subset needed to bootstrap a fresh clone.
pub(crate) async fn publish(
    store: &Store,
    layout: &StoreLayout,
    primary: &str,
    replication: &ReplicationConfig,
) -> Result<()> {
    let document = ReplicaDiscoveryDocument {
        version: REPLICA_DISCOVERY_VERSION,
        primary: canonical_primary(primary)?,
        replicas: replication.replicas.clone(),
    };
    for replica in &document.replicas {
        validate_replica_url_provider(replica.provider, &replica.url)?;
    }

    let path = layout.replica_discovery_path();
    let body = serde_json::to_vec(&document).map_err(|error| CrabError::CorruptObject {
        path: path.to_string(),
        reason: format!("failed to serialize replica discovery: {error}"),
    })?;
    if body.len() as u64 > MAX_REPLICA_DISCOVERY_BYTES {
        return Err(CrabError::Configuration {
            key: "replication.replicas".into(),
            origin: format!(
                "replica discovery exceeds the {MAX_REPLICA_DISCOVERY_BYTES}-byte limit"
            ),
        });
    }
    store.put_overwrite(&path, Bytes::from(body)).await
}

/// Load replica routing from the authoritative primary before Git fetches config.
pub(crate) async fn load(
    store: &Store,
    layout: &StoreLayout,
    expected_primary: &str,
) -> Result<Option<ReplicationConfig>> {
    let path = layout.replica_discovery_path();
    let (body, _) = match store
        .get_with_etag_bounded(&path, MAX_REPLICA_DISCOVERY_BYTES)
        .await
    {
        Ok(result) => result,
        Err(CrabError::NotFound { path: missing }) if missing == path.as_ref() => return Ok(None),
        Err(error) => return Err(error),
    };
    let document: ReplicaDiscoveryDocument =
        serde_json::from_slice(&body).map_err(|error| CrabError::CorruptObject {
            path: path.to_string(),
            reason: format!("invalid replica discovery document: {error}"),
        })?;
    if document.version != REPLICA_DISCOVERY_VERSION {
        return Err(CrabError::IncompatibleFormat {
            required: format!("replica discovery v{REPLICA_DISCOVERY_VERSION}"),
            found: format!("replica discovery v{}", document.version),
        });
    }

    let expected_primary = canonical_primary(expected_primary)?;
    let discovered_primary =
        canonical_primary(&document.primary).map_err(|error| CrabError::CorruptObject {
            path: path.to_string(),
            reason: format!("invalid discovery primary: {error}"),
        })?;
    if discovered_primary != expected_primary {
        return Err(CrabError::CorruptObject {
            path: path.to_string(),
            reason: format!(
                "discovery primary {discovered_primary} does not match requested {expected_primary}"
            ),
        });
    }
    for replica in &document.replicas {
        validate_replica_url_provider(replica.provider, &replica.url).map_err(|error| {
            CrabError::CorruptObject {
                path: path.to_string(),
                reason: format!("invalid replica {}: {error}", replica.name),
            }
        })?;
    }

    Ok(Some(ReplicationConfig {
        primary: Some(discovered_primary),
        replicas: document.replicas,
        ..ReplicationConfig::default()
    }))
}

/// Apply discovered routing only when no explicit local routing is available.
pub(crate) fn apply_if_unconfigured(
    config: &mut Config,
    discovered: Option<ReplicationConfig>,
) -> bool {
    if config.replication.is_some() {
        return false;
    }
    let Some(discovered) = discovered else {
        return false;
    };
    config.replication = Some(discovered);
    true
}

fn canonical_primary(primary: &str) -> Result<String> {
    let parsed = CrabUrl::parse(primary)?;
    Ok(format!("crab://{}/{}", parsed.bucket, parsed.repo_path))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use object_store::memory::InMemory;

    use super::*;
    use crate::replication::{ReplicationProviderKind, ReplicationRpo};

    fn store_and_layout() -> (Store, StoreLayout) {
        let store = Store::new(Arc::new(InMemory::new()));
        let layout = StoreLayout::new(store.clone(), "org/repo".to_owned());
        (store, layout)
    }

    fn replication() -> ReplicationConfig {
        ReplicationConfig {
            primary: Some("crab://primary/org/repo".to_owned()),
            replicas: vec![ReplicaConfig {
                name: "west".to_owned(),
                provider: ReplicationProviderKind::S3,
                url: "crab://replica/replicated/org/repo".to_owned(),
                region: "us-west-2".to_owned(),
                backfill: true,
                read: true,
                rpo: ReplicationRpo::Standard,
            }],
            ..ReplicationConfig::default()
        }
    }

    #[tokio::test]
    async fn published_discovery_round_trips_read_routing() {
        let (store, layout) = store_and_layout();
        let expected = replication();

        publish(
            &store,
            &layout,
            expected.primary.as_deref().unwrap(),
            &expected,
        )
        .await
        .unwrap();
        let loaded = load(&store, &layout, "crab://primary/org/repo/")
            .await
            .unwrap()
            .unwrap();

        assert_eq!(loaded, expected);
    }

    #[tokio::test]
    async fn missing_discovery_keeps_primary_only_routing() {
        let (store, layout) = store_and_layout();

        let loaded = load(&store, &layout, "crab://primary/org/repo")
            .await
            .unwrap();

        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn discovery_is_bound_to_requested_primary() {
        let (store, layout) = store_and_layout();
        let replication = replication();
        publish(
            &store,
            &layout,
            replication.primary.as_deref().unwrap(),
            &replication,
        )
        .await
        .unwrap();

        let error = load(&store, &layout, "crab://other/org/repo")
            .await
            .unwrap_err();

        assert!(matches!(error, CrabError::CorruptObject { .. }));
    }

    #[tokio::test]
    async fn unsupported_discovery_version_fails_closed() {
        let (store, layout) = store_and_layout();
        let path = layout.replica_discovery_path();
        store
            .put_overwrite(
                &path,
                Bytes::from_static(
                    br#"{"version":2,"primary":"crab://primary/org/repo","replicas":[]}"#,
                ),
            )
            .await
            .unwrap();

        let error = load(&store, &layout, "crab://primary/org/repo")
            .await
            .unwrap_err();

        assert!(matches!(error, CrabError::IncompatibleFormat { .. }));
    }

    #[test]
    fn discovery_populates_unconfigured_session() {
        let mut config = Config::default();
        let expected = replication();

        let changed = apply_if_unconfigured(&mut config, Some(expected.clone()));

        assert!(changed);
        assert_eq!(config.replication, Some(expected));
    }

    #[test]
    fn explicit_local_replication_wins_over_discovery() {
        let mut config = Config {
            replication: Some(ReplicationConfig::default()),
            ..Config::default()
        };

        let changed = apply_if_unconfigured(&mut config, Some(replication()));

        assert!(!changed);
        assert_eq!(config.replication, Some(ReplicationConfig::default()));
    }
}
