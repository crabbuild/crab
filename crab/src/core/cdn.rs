//! Operator-owned CDN bindings for direct repositories.

use std::collections::BTreeSet;
use std::time::Duration;

use crab_auth::crab_auth_client::CdnViewerDeployment;
use crab_storage::{
    StorageProviderKind, Store,
    cdn::{CdnAccess, CdnConsumer, CdnProfile, CdnPublicationFile, CdnRoute, CdnTransport},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::core::config::{AuthConfig, AuthProvider};
use crate::core::error::{CrabError, Result};

const QUALIFICATION_TARGET_ENV: &str = "CRAB_INTERNAL_CDN_QUALIFICATION_TARGET";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QualificationTarget {
    pub(crate) origin: String,
    pub(crate) repository_prefix: String,
    #[serde(default)]
    pub(crate) read: Option<QualificationRead>,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct QualificationRead {
    binding_sha256: String,
    cache_root_sha256: String,
    public_export_sha256: String,
}

impl QualificationRead {
    fn verify(&self, store: &Store, cache_root: &std::path::Path) -> Result<()> {
        let root = cache_root.to_str().ok_or_else(|| CrabError::AuthFailed {
            path: "the CDN qualification cache root needs a UTF-8 path".into(),
        })?;
        if self.binding_sha256 != store.cdn_binding_sha256().unwrap_or_default()
            || self.cache_root_sha256 != qualification_digest(root)
        {
            return Err(CrabError::AuthFailed {
                path: "the selected CDN binding or cache root differs from the approved qualification action".into(),
            });
        }
        if self.public_export_sha256 != store.cdn_public_export_sha256().unwrap_or_default() {
            return Err(CrabError::AuthFailed {
                path: "the selected CDN public export approval differs from the approved qualification action".into(),
            });
        }
        Ok(())
    }
}

impl QualificationTarget {
    pub(crate) fn verify(&self, store: &Store, prefix: &str) -> Result<()> {
        let effective = store
            .storage_scope()
            .map_or(prefix, |scope| scope.repo_prefix.as_str());
        self.require_exact(store.native_cdn_origin(), effective)?;
        if store
            .read_prefix_target_identity(&object_store::path::Path::from(effective))
            .is_none()
        {
            return Err(CrabError::AuthFailed {
                path: "the approved CDN qualification prefix does not have one native transport"
                    .into(),
            });
        }
        Ok(())
    }

    fn require_exact(&self, origin: Option<&str>, prefix: &str) -> Result<()> {
        if origin != Some(self.origin.as_str()) || prefix != self.repository_prefix {
            return Err(CrabError::AuthFailed {
                path: "the resolved native origin or repository prefix differs from the approved CDN qualification fixture"
                    .to_owned(),
            });
        }
        Ok(())
    }
}

pub(crate) fn qualification_target() -> Result<Option<QualificationTarget>> {
    let Some(value) = std::env::var_os(QUALIFICATION_TARGET_ENV) else {
        return Ok(None);
    };
    value
        .to_str()
        .filter(|value| value.len() <= 1024)
        .and_then(|value| serde_json::from_str::<QualificationTarget>(value).ok())
        .filter(|target| !target.origin.is_empty() && !target.repository_prefix.is_empty())
        .map(Some)
        .ok_or_else(|| CrabError::Configuration {
            key: QUALIFICATION_TARGET_ENV.to_owned(),
            origin: "an approved native origin and repository prefix are required".to_owned(),
        })
}

pub(crate) fn qualification_digest(bytes: impl AsRef<[u8]>) -> String {
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    crab_git::lfs_pointer::hex_encode(&digest)
}

pub(crate) fn verify_qualification_cache(cache: &crate::core::config::CacheConfig) -> Result<()> {
    if cache.service_url.is_some() && qualification_target()?.is_some() {
        return Err(CrabError::AuthFailed {
            path: "the CDN qualification workload requires an isolated local cache".into(),
        });
    }
    Ok(())
}

pub(crate) fn verify_qualification_read(store: &Store) -> Result<()> {
    if let Some(read) = qualification_target()?.and_then(|target| target.read) {
        read.verify(store, &crab_cache::default_cache_root())?;
        tracing::debug!(
            target: "crab::cdn::qualification_read",
            parent: None,
            binding_sha256 = read.binding_sha256.as_str(),
            cache_root_sha256 = read.cache_root_sha256.as_str(),
            public_export_sha256 = read.public_export_sha256.as_str(),
            "CDN qualification verified read configuration"
        );
    }
    Ok(())
}

pub(crate) fn reject_managed_qualification() -> Result<()> {
    if qualification_target()?.is_some() {
        return Err(CrabError::AuthFailed {
            path: "the CDN qualification workload requires a direct repository".into(),
        });
    }
    Ok(())
}

pub(crate) fn verify_qualification_target(store: &Store, prefix: &str) -> Result<()> {
    if let Some(expected) = qualification_target()? {
        expected.verify(store, prefix)?;
        let origin_digest = qualification_digest(&expected.origin);
        let prefix_digest = qualification_digest(format!("{}/", expected.repository_prefix));
        // The guard may restrict an ordinary release build. Only the compiled
        // release policy and Cargo feature can enable edge delivery.
        tracing::debug!(
            target: "crab::cdn::qualification",
            parent: None,
            source_sha = env!("CRAB_BUILD_SOURCE_SHA"),
            target_triple = env!("CRAB_BUILD_TARGET"),
            cargo_profile = env!("CRAB_BUILD_PROFILE"),
            cargo_features = env!("CRAB_BUILD_FEATURES"),
            qualification_feature = crab_storage::cdn::qualification_enabled(),
            release_policy_sha256 = %crab_storage::cdn::release_policy_sha256(),
            origin_sha256 = origin_digest.as_str(),
            prefix_sha256 = prefix_digest.as_str(),
            "CDN qualification verified native target"
        );
    }
    Ok(())
}

/// Optional edge endpoint for one native bucket and physical repository.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CdnBinding {
    origin: String,
    export_origin: Option<String>,
    publication: Option<CdnPublicationFile>,
    approval_sha256: Option<String>,
    repo_prefix: String,
    base_url: String,
    #[serde(default)]
    access: Access,
    #[serde(skip)]
    trusted_issuer: Option<AuthConfig>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Access {
    #[default]
    Private,
    Public,
}

impl CdnBinding {
    pub(crate) fn bind_trusted_issuer(entries: &mut [Self], auth: &AuthConfig) {
        if auth.provider == AuthProvider::CrabAuth {
            for binding in entries
                .iter_mut()
                .filter(|binding| binding.access == Access::Private)
            {
                binding.trusted_issuer = Some(auth.clone());
            }
        }
    }

    fn origin_target(&self) -> Option<(StorageProviderKind, &str)> {
        let (provider, bucket) = if let Some(bucket) = self.origin.strip_prefix("s3://") {
            (StorageProviderKind::S3, bucket)
        } else if let Some(bucket) = self.origin.strip_prefix("gs://") {
            (StorageProviderKind::Gcs, bucket)
        } else {
            let (account, container) = self
                .origin
                .strip_prefix("https://")?
                .split_once(".blob.core.windows.net/")?;
            if !(3..=24).contains(&account.len())
                || !account
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            {
                return None;
            }
            (StorageProviderKind::Azure, container)
        };
        (!bucket.is_empty()
            && bucket.bytes().all(|b| {
                b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'-'
                    || (provider != StorageProviderKind::Azure && matches!(b, b'.' | b'_'))
            }))
        .then_some((provider, bucket))
    }

    pub(crate) fn validate_all(entries: &mut [Self]) -> Result<()> {
        let mut seen = BTreeSet::new();
        let mut viewer_routes = BTreeSet::new();
        for entry in entries {
            let deployment =
                CdnViewerDeployment::new(&entry.origin, &entry.repo_prefix, &entry.base_url)
                    .and_then(|deployment| {
                        deployment.with_approval(entry.approval_sha256.as_deref())
                    })
                    .map_err(|_| {
                        config_error("invalid native origin, repository prefix or HTTPS base")
                    })?;
            match (entry.access, entry.export_origin.as_deref()) {
                (Access::Public, Some(export)) if entry.approval_sha256.is_none() => {
                    deployment.validate_public_export(export)?;
                }
                (Access::Private, None) => {
                    if entry.publication.is_some() {
                        return Err(config_error(
                            "only public CDN bindings can use a publication receipt",
                        ));
                    }
                }
                _ => return Err(config_error("public CDN requires export_origin")),
            }
            entry.base_url = deployment.base_url;
            if !seen.insert((&entry.origin, &entry.repo_prefix)) {
                return Err(config_error("duplicate origin and repository prefix"));
            }
            if !viewer_routes.insert(format!("{}/{}", entry.base_url, entry.repo_prefix)) {
                return Err(config_error("CDN viewer path is bound more than once"));
            }
        }
        Ok(())
    }

    pub(crate) fn private_request<'a>(
        entries: &'a [Self],
        bucket: &str,
        prefix: &str,
    ) -> Result<Option<(Vec<CdnViewerDeployment>, &'a AuthConfig)>> {
        let candidates = entries.iter().filter(|binding| {
            binding.access == Access::Private
                && binding.repo_prefix == prefix
                && binding
                    .origin_target()
                    .is_some_and(|(_, target)| target == bucket)
        });
        let Some(binding) = candidates.clone().next() else {
            return Ok(None);
        };
        let issuer = binding.trusted_issuer.as_ref().ok_or_else(|| {
            config_error("private CDN bindings require Crab Auth in user configuration")
        })?;
        let deployments = candidates
            .map(|binding| binding.private_deployment(prefix))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Some((deployments, issuer)))
    }

    pub(crate) async fn attach_public(
        entries: &[Self],
        store: Store,
        prefix: &str,
        timeout: Duration,
        consumer: CdnConsumer,
    ) -> Result<Store> {
        let Some(binding) = Self::for_selected_origin(entries, &store, prefix, Access::Public)?
        else {
            return Ok(store);
        };
        if store.storage_scope().is_some() {
            return Err(config_error(
                "public CDN cannot serve a filtered repository view",
            ));
        }
        let Some(publication) = &binding.publication else {
            return Ok(store);
        };
        let export = binding
            .export_origin
            .as_deref()
            .ok_or_else(|| config_error("public CDN requires export_origin"))?;
        let target = store
            .target_identity()
            .copied()
            .ok_or_else(|| config_error("CDN requires a resolved physical target"))?;
        let route = CdnRoute::new(
            target,
            prefix.to_owned(),
            None,
            CdnAccess::Public,
            CdnTransport::new(&binding.base_url)?,
            timeout,
        )
        .for_consumer(consumer, CdnProfile::Direct)
        .with_publication(
            &store,
            publication,
            export,
            Some(&crab_auth_store::azure_cdn::AzureCdnAuthority),
        )
        .await?;
        Ok(store.with_cdn_route(route)?)
    }

    pub(crate) fn private_request_for_selected_origin<'a>(
        entries: &'a [Self],
        store: &Store,
        prefix: &str,
    ) -> Result<Option<(CdnViewerDeployment, &'a AuthConfig)>> {
        let Some(binding) = Self::for_selected_origin(entries, store, prefix, Access::Private)?
        else {
            return Ok(None);
        };
        let issuer = binding.trusted_issuer.as_ref().ok_or_else(|| {
            config_error("private CDN bindings require Crab Auth in user configuration")
        })?;
        Ok(Some((binding.private_deployment(prefix)?, issuer)))
    }

    fn private_deployment(&self, prefix: &str) -> crab_auth::error::Result<CdnViewerDeployment> {
        CdnViewerDeployment::new(&self.origin, prefix, &self.base_url)?
            .with_approval(self.approval_sha256.as_deref())
    }

    fn for_selected_origin<'a>(
        entries: &'a [Self],
        store: &Store,
        prefix: &str,
        access: Access,
    ) -> Result<Option<&'a Self>> {
        let bucket = store.bucket_identity();
        for candidate in entries.iter().filter(|entry| {
            entry.access == access
                && entry.repo_prefix == prefix
                && entry.origin_target() == Some((bucket.cloud, bucket.container.as_str()))
        }) {
            let origin = store
                .native_cdn_origin()
                .ok_or_else(|| config_error("CDN binding requires a supported native origin"))?;
            if origin == candidate.origin {
                return Ok(Some(candidate));
            }
        }
        Ok(None)
    }
}

fn config_error(reason: &str) -> CrabError {
    CrabError::Configuration {
        key: "cdn".into(),
        origin: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdn_qualification_requires_the_exact_native_origin_and_prefix() {
        let prefix = "crab-cdn-qualification/approved";
        for origin in [
            "s3://qualification-bucket",
            "gs://qualification-bucket",
            "https://primaryacct.blob.core.windows.net/container",
        ] {
            let expected = QualificationTarget {
                origin: origin.into(),
                repository_prefix: prefix.into(),
                read: None,
            };
            assert!(expected.require_exact(Some(origin), prefix).is_ok());
            for (actual, path) in [
                (Some(origin), "crab-cdn-qualification/approved-neighbor"),
                (Some(origin), "crab-cdn-qualification/approved/view"),
                (
                    Some("https://otheracct.blob.core.windows.net/container"),
                    prefix,
                ),
                (None, prefix),
            ] {
                assert!(matches!(
                    expected.require_exact(actual, path),
                    Err(CrabError::AuthFailed { .. })
                ));
            }
        }
    }

    #[test]
    fn cdn_qualification_read_checks_viewer_state_and_cache_root() {
        use std::{path::Path, sync::Arc};

        use object_store::memory::InMemory;

        let cache = Path::new("scratch-cache");
        let native = Store::new(Arc::new(InMemory::new())).with_target_identity([3; 32]);
        let origin = QualificationRead {
            binding_sha256: String::new(),
            cache_root_sha256: qualification_digest("scratch-cache"),
            public_export_sha256: String::new(),
        };
        assert!(origin.verify(&native, cache).is_ok());
        let edge = native
            .clone()
            .with_cdn_route(CdnRoute::new(
                [3; 32],
                "crab-cdn-qualification/viewer".into(),
                None,
                CdnAccess::Public,
                CdnTransport::loopback_fixture(([127, 0, 0, 1], 9876).into()).unwrap(),
                Duration::from_secs(2),
            ))
            .unwrap();
        let expected = QualificationRead {
            binding_sha256: qualification_digest("public\nhttp://127.0.0.1:9876"),
            cache_root_sha256: origin.cache_root_sha256.clone(),
            public_export_sha256: String::new(),
        };
        assert!(expected.verify(&edge, cache).is_ok());
        assert!(matches!(
            origin.verify(&edge, cache),
            Err(CrabError::AuthFailed { .. })
        ));
        assert!(matches!(
            expected.verify(&native, cache),
            Err(CrabError::AuthFailed { .. })
        ));
        assert!(matches!(
            expected.verify(&edge, Path::new("ambient-cache")),
            Err(CrabError::AuthFailed { .. })
        ));
        let approved = QualificationRead {
            public_export_sha256: qualification_digest("approved public export"),
            ..expected
        };
        assert!(matches!(
            approved.verify(&edge, cache),
            Err(CrabError::AuthFailed { .. })
        ));
    }

    #[test]
    fn cdn_qualification_uses_the_effective_view_and_checks_for_routed_reads() {
        use std::sync::Arc;

        use object_store::memory::InMemory;

        let origin = "s3://qualification-bucket";
        let prefix = "crab-cdn-qualification/approved";
        let expected = QualificationTarget {
            origin: origin.into(),
            repository_prefix: prefix.into(),
            read: None,
        };
        let base = Store::new(Arc::new(InMemory::new()))
            .with_target_identity([1; 32])
            .with_native_cdn_origin(Some(origin.into()));
        assert!(expected.verify(&base, prefix).is_ok());

        let scoped = base.clone().with_storage_scope(crab_storage::StorageScope {
            repo_prefix: format!("{prefix}/view"),
            global_prefix: format!("{prefix}/view/.crab"),
            source_repo: prefix.into(),
            scope_hash: "a".repeat(64),
        });
        let routed =
            base.with_read_routes(vec![(format!("{prefix}/v2"), Arc::new(InMemory::new()))]);
        for store in [scoped, routed] {
            assert!(matches!(
                expected.verify(&store, prefix),
                Err(CrabError::AuthFailed { .. })
            ));
        }
    }

    #[test]
    fn private_mapping_needs_exact_source_prefix() {
        let mut entries = [toml::from_str::<CdnBinding>(
            r#"
            origin = "s3://models"
            repo_prefix = "org/demo"
            base_url = "https://downloads.example.com/"
        "#,
        )
        .unwrap()];
        CdnBinding::validate_all(&mut entries).unwrap();
        assert!(
            CdnBinding::private_request(&entries, "models", "org/demo2")
                .unwrap()
                .is_none()
        );
        assert!(
            CdnBinding::private_request(&entries, "other", "org/demo")
                .unwrap()
                .is_none()
        );
        assert!(CdnBinding::private_request(&entries, "models", "org/demo").is_err());
        let mut auth = AuthConfig {
            provider: AuthProvider::CrabAuth,
            auth_endpoint: Some("https://auth.example.com/v1/credentials".into()),
            ..AuthConfig::default()
        };
        CdnBinding::bind_trusted_issuer(&mut entries, &auth);
        auth.auth_endpoint = Some("https://repo-override.example.com/v1/credentials".into());
        let (request, issuer) = CdnBinding::private_request(&entries, "models", "org/demo")
            .unwrap()
            .unwrap();
        assert_eq!(request[0].base_url, "https://downloads.example.com");
        assert_eq!(
            issuer.auth_endpoint.as_deref(),
            Some("https://auth.example.com/v1/credentials")
        );
    }

    #[test]
    fn private_mapping_keeps_native_origins_distinct_for_shared_container_names() {
        let origins = [
            "s3://models",
            "gs://models",
            "https://accountone.blob.core.windows.net/models",
            "https://accounttwo.blob.core.windows.net/models",
        ];
        let mut entries = origins.into_iter().enumerate().map(|(index, origin)| {
            toml::from_str::<CdnBinding>(&format!(
                "origin = {origin:?}\nrepo_prefix = \"org/demo\"\nbase_url = \"https://edge{index}.example.com\""
            ))
            .unwrap()
        }).collect::<Vec<_>>();
        CdnBinding::validate_all(&mut entries).unwrap();
        CdnBinding::bind_trusted_issuer(
            &mut entries,
            &AuthConfig {
                provider: AuthProvider::CrabAuth,
                ..AuthConfig::default()
            },
        );
        let (approved, _) = CdnBinding::private_request(&entries, "models", "org/demo")
            .unwrap()
            .unwrap();
        assert_eq!(
            approved
                .iter()
                .map(|item| item.origin.as_str())
                .collect::<Vec<_>>(),
            origins
        );
        for (cloud, origin) in [
            (StorageProviderKind::S3, origins[0]),
            (StorageProviderKind::Gcs, origins[1]),
            (StorageProviderKind::Azure, origins[2]),
            (StorageProviderKind::Azure, origins[3]),
        ] {
            let store = Store::new(std::sync::Arc::new(object_store::memory::InMemory::new()))
                .with_bucket_identity(crab_storage::BucketIdentity::new(
                    cloud, "fixture", "models",
                ))
                .with_native_cdn_origin(Some(origin.to_owned()));
            let (deployment, _) =
                CdnBinding::private_request_for_selected_origin(&entries, &store, "org/demo")
                    .unwrap()
                    .unwrap();
            assert_eq!(deployment.origin, origin);
            assert!(
                CdnBinding::private_request_for_selected_origin(&entries, &store, "org/demo2")
                    .unwrap()
                    .is_none()
            );
            assert!(
                CdnBinding::private_request_for_selected_origin(
                    &entries,
                    &store.with_native_cdn_origin(None),
                    "org/demo",
                )
                .is_err()
            );
        }
    }

    #[test]
    fn different_origins_cannot_share_the_same_resulting_viewer_path() {
        let first = r#"
            origin = "s3://models"
            repo_prefix = "org/demo"
            base_url = "https://edge.example.com/"
        "#;
        let second = r#"
            origin = "gs://models"
            repo_prefix = "demo"
            base_url = "https://edge.example.com/org"
        "#;
        let mut entries = [first, second].map(|text| toml::from_str::<CdnBinding>(text).unwrap());
        let error = CdnBinding::validate_all(&mut entries).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("viewer path is bound more than once")
        );
    }

    #[tokio::test]
    async fn public_mapping_rejects_custom_endpoint_on_matching_bucket() {
        let mut entries = [toml::from_str::<CdnBinding>(
            r#"
            origin = "s3://models"
            export_origin = "s3://approved-export"
            repo_prefix = "org/demo"
            base_url = "https://downloads.example.com"
            access = "public"
        "#,
        )
        .unwrap()];
        CdnBinding::validate_all(&mut entries).unwrap();
        let store = Store::new(std::sync::Arc::new(object_store::memory::InMemory::new()))
            .with_bucket_identity(crab_storage::BucketIdentity::new(
                StorageProviderKind::S3,
                "models",
                "models",
            ))
            .with_target_identity([1; 32]);
        assert!(
            CdnBinding::attach_public(
                &entries,
                store,
                "org/demo",
                Duration::from_secs(10),
                CdnConsumer::Cli
            )
            .await
            .is_err()
        );
    }

    #[cfg(feature = "provider-cdn-qualification")]
    #[tokio::test]
    async fn public_binding_loads_the_operator_receipt_before_routing() {
        use object_store::{memory::InMemory, path::Path};

        let directory = tempfile::tempdir().unwrap();
        let origin = "s3://models";
        let prefix = "org/demo";
        let export = "s3://approved-export";
        let viewer = "https://downloads.example.com";
        let relative = format!("v2/pack-layers/aa/{}", "a".repeat(64));
        let approval = crab_storage::cdn::write_publication_fixture(
            directory.path().join("receipt.json"),
            origin,
            export,
            prefix,
            viewer,
            &relative,
            6,
        )
        .unwrap();
        let mut entries = [toml::from_str::<CdnBinding>(&format!(
            r#"
            origin = "{origin}"
            export_origin = "{export}"
            repo_prefix = "{prefix}"
            base_url = "{viewer}"
            access = "public"
            publication = {{ path = {path:?}, sha256 = {digest:?} }}
            "#,
            path = approval.path,
            digest = approval.sha256,
        ))
        .unwrap()];
        CdnBinding::validate_all(&mut entries).unwrap();
        let store = Store::new(std::sync::Arc::new(InMemory::new()))
            .with_bucket_identity(crab_storage::BucketIdentity::new(
                StorageProviderKind::S3,
                "models",
                "models",
            ))
            .with_native_cdn_origin(Some(origin.into()))
            .with_target_identity([1; 32]);
        let routed = CdnBinding::attach_public(
            &entries,
            store,
            prefix,
            Duration::from_secs(10),
            CdnConsumer::Cli,
        )
        .await
        .unwrap();
        assert!(
            routed
                .cdn_attempt(&Path::from(format!("{prefix}/{relative}")))
                .is_some()
        );
        let fresh = Path::from(format!("{prefix}/v2/pack-layers/bb/{}", "b".repeat(64)));
        assert!(routed.cdn_attempt(&fresh).is_none());
    }
}
