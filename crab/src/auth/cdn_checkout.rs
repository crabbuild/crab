//! Direct CDN authorization for the SDK's checkout subprocesses.

use std::io::Read as _;
use std::path::Path;
use std::sync::Arc;

use crab_auth::CredentialProvider as _;
use crab_auth::cdn_checkout::{CdnCheckoutBinding, CdnCheckoutConsumer, MAX_BINDING_BYTES};
use crab_auth::crab_auth_client::CdnViewerDeployment;
use crab_git::CrabUrl;
use crab_remote::local::DIRECT_CDN_CHECKOUT_BINDING_ENV;
use crab_storage::cdn::{
    CdnAccess, CdnConsumer, CdnPolicy, CdnProfile, CdnRoute, CdnTransport, CdnVisibility,
};
use tokio_util::sync::CancellationToken;

use super::RepositoryStore;
use crate::core::config::Config;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::storage::store::Store;

pub(crate) struct Checkout {
    binding: CdnCheckoutBinding,
    deployment: CdnViewerDeployment,
}

impl Checkout {
    pub(crate) fn load() -> Result<Option<Self>> {
        let Some(path) =
            std::env::var_os(DIRECT_CDN_CHECKOUT_BINDING_ENV).filter(|path| !path.is_empty())
        else {
            return Ok(None);
        };
        read_binding(Path::new(&path), &std::env::current_dir()?).map(Some)
    }

    pub(crate) fn source_url(&self) -> CrabUrl {
        CrabUrl {
            bucket: self.deployment.bucket.clone(),
            repo_path: self.deployment.source_repo.clone(),
        }
    }

    pub(crate) async fn open(
        self,
        config: &Config,
        url: &CrabUrl,
        cancel: &CancellationToken,
    ) -> Result<RepositoryStore> {
        check_cancelled(cancel)?;
        if url.bucket != self.deployment.bucket || url.repo_path != self.deployment.source_repo {
            return Err(invalid("CDN checkout source repository changed"));
        }
        if self.binding.require_configured_source {
            let configured = Config::sdk_worktree_content_source(&self.binding.worktree)?;
            let expected = format!("crab://{}/{}", url.bucket, url.repo_path);
            if configured.as_deref() != Some(expected.as_str()) {
                return Err(invalid("CDN worktree content source changed"));
            }
        }
        let operation = "hydrate";
        let deployment = self.deployment;
        let consumer = match self.binding.consumer {
            CdnCheckoutConsumer::Sdk => CdnConsumer::Sdk,
            CdnCheckoutConsumer::Desktop => CdnConsumer::Desktop,
        };
        let store = if let Some(issuer) = self.binding.issuer {
            let viewer_enabled = deployment.storage_provider().is_some_and(|provider| {
                CdnPolicy {
                    provider,
                    consumer,
                    profile: CdnProfile::Direct,
                    visibility: CdnVisibility::Private,
                }
                .enabled()
            });
            let provider = Arc::new(
                crab_auth::create_crab_auth_provider(issuer.config)?
                    .with_cdn_checkout(deployment.clone(), issuer.principal)?
                    .with_viewer_grants(viewer_enabled),
            );
            let resolution = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(CrabError::Cancelled),
                result = provider.resolve(&url.bucket, &url.repo_path, operation) => result?,
            };
            let store = crab_auth_store::cdn::build_direct_cdn_store(
                provider,
                url.bucket.clone(),
                url.repo_path.clone(),
                operation.to_owned(),
                resolution,
                config.operation_timeout,
                consumer,
            )?;
            verify_target(
                &self.binding.target_identity,
                self.binding.scope.as_ref(),
                &deployment,
                &store,
            )?;
            store
        } else {
            let mut origin = config.clone();
            origin.cdn.clear();
            let store = super::build_store(&origin, url.clone(), operation, cancel)
                .await?
                .into_storage();
            verify_target(&self.binding.target_identity, None, &deployment, &store)?;
            let (Some(publication), Some(export)) =
                (&self.binding.publication, &self.binding.export_origin)
            else {
                return Err(invalid(
                    "public CDN checkout requires an approved publication receipt",
                ));
            };
            let route = CdnRoute::new(
                self.binding.target_identity,
                url.repo_path.clone(),
                None,
                CdnAccess::Public,
                CdnTransport::new(&deployment.base_url)?,
                config.operation_timeout,
            )
            .for_consumer(consumer, CdnProfile::Direct)
            .with_publication(
                &store,
                publication,
                export,
                Some(&crab_auth_store::azure_cdn::AzureCdnAuthority),
            )
            .await?;
            store.with_cdn_route(route)?
        };
        let repository_prefix = store
            .storage_scope()
            .map_or_else(|| url.repo_path.clone(), |scope| scope.repo_prefix.clone());
        let store = Store::from_storage(store);
        let remote = format!("crab://{}/{}", url.bucket, url.repo_path);
        let capsule_root = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(CrabError::Cancelled),
            result = super::open_repository_root(&store, &repository_prefix, &remote) => result?,
        };
        if tracing::enabled!(target: "crab::cdn::checkout_target", tracing::Level::DEBUG) {
            use crate::core::cdn::qualification_digest;
            let cache_root = crab_cache::default_cache_root();
            let global_prefix = store
                .storage_scope()
                .map_or(crab_storage::GLOBAL_PREFIX, |scope| {
                    scope.global_prefix.as_str()
                });
            let approval = deployment.approval_sha256().unwrap_or_default();
            tracing::debug!(
                target: "crab::cdn::checkout_target",
                parent: None,
                origin_sha256 = %qualification_digest(&deployment.origin),
                prefix_sha256 = %qualification_digest(format!("{repository_prefix}/")),
                global_prefix_sha256 = %qualification_digest(global_prefix),
                cache_root_sha256 = %qualification_digest(cache_root.as_os_str().as_encoded_bytes()),
                binding_sha256 = store.as_storage().cdn_binding_sha256().as_deref().unwrap_or_default(),
                approval_sha256 = approval,
                local_cache_only = config.cache.service_url.is_none(),
                "CDN child captured read target"
            );
        }
        Ok(RepositoryStore {
            store,
            repository_prefix,
            capsule_root,
        })
    }
}

fn verify_target(
    target: &[u8; 32],
    scope: Option<&crab_types::storage::StorageScope>,
    deployment: &CdnViewerDeployment,
    store: &crab_storage::Store,
) -> Result<()> {
    if store.native_cdn_origin() != Some(deployment.origin.as_str())
        || store.target_identity() != Some(target)
        || store.storage_scope() != scope
    {
        return Err(invalid("CDN checkout authorization changed target or view"));
    }
    Ok(())
}

fn read_binding(path: &Path, worktree: &Path) -> Result<Checkout> {
    if !path.is_absolute() || !path.symlink_metadata()?.is_file() {
        return Err(invalid(
            "CDN checkout binding must be an absolute regular file",
        ));
    }
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(invalid("CDN checkout binding must be private"));
        }
    }
    if !metadata.is_file() || metadata.len() > MAX_BINDING_BYTES as u64 {
        return Err(invalid("invalid CDN checkout binding file"));
    }
    let mut body = Vec::new();
    file.take(MAX_BINDING_BYTES as u64 + 1)
        .read_to_end(&mut body)?;
    if body.len() > MAX_BINDING_BYTES {
        return Err(invalid("CDN checkout binding is too large"));
    }
    let binding: CdnCheckoutBinding =
        serde_json::from_slice(&body).map_err(|_| invalid("invalid CDN checkout binding"))?;
    if !binding.worktree.is_absolute()
        || std::fs::canonicalize(&binding.worktree)? != std::fs::canonicalize(worktree)?
    {
        return Err(invalid(
            "CDN checkout binding did not match the working directory",
        ));
    }
    let deployment = binding.deployment()?;
    Ok(Checkout {
        binding,
        deployment,
    })
}

fn invalid(message: &'static str) -> CrabError {
    CrabError::Configuration {
        key: DIRECT_CDN_CHECKOUT_BINDING_ENV.into(),
        origin: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Seek as _, Write as _};

    use crab_types::storage::StorageScope;
    use object_store::memory::InMemory;

    use super::*;

    #[tokio::test]
    async fn binding_is_limited_to_its_worktree_and_rejects_extra_fields() {
        let scratch = tempfile::tempdir().unwrap();
        let worktree = scratch.path().join("checkout");
        std::fs::create_dir(&worktree).unwrap();
        let binding = CdnCheckoutBinding {
            worktree: worktree.clone(),
            require_configured_source: true,
            origin: "s3://origin-bucket".into(),
            export_origin: Some("s3://export-bucket".into()),
            publication: Some(crab_storage::cdn::CdnPublicationFile {
                path: scratch.path().join("publication.json"),
                sha256: "a".repeat(64),
            }),
            approval_sha256: None,
            source_repo: "team/models".into(),
            base_url: "https://edge.example.test".into(),
            target_identity: [1; 32],
            consumer: CdnCheckoutConsumer::Sdk,
            scope: None,
            issuer: None,
        };
        let mut file = tempfile::NamedTempFile::new_in(scratch.path()).unwrap();
        serde_json::to_writer(&mut file, &binding).unwrap();
        let accepted = read_binding(file.path(), &worktree).unwrap();
        assert_eq!(accepted.source_url().repo_path, "team/models");
        assert!(read_binding(file.path(), scratch.path()).is_err());
        assert!(
            std::process::Command::new("git")
                .current_dir(&worktree)
                .args(["init", "--quiet", "--template="])
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(
            worktree.join("crab.toml"),
            "version = 1\n[remote]\nurl = 'crab://origin-bucket/team/sibling'\n",
        )
        .unwrap();
        let source = accepted.source_url();
        let result = accepted
            .open(&Config::default(), &source, &CancellationToken::new())
            .await;
        assert!(
            matches!(result, Err(CrabError::Configuration { origin, .. })
            if origin == "CDN worktree content source changed")
        );

        let mut unexpected = serde_json::to_value(binding).unwrap();
        unexpected["viewer_token"] = serde_json::Value::String("unused".into());
        file.rewind().unwrap();
        file.as_file().set_len(0).unwrap();
        file.write_all(&serde_json::to_vec(&unexpected).unwrap())
            .unwrap();
        assert!(read_binding(file.path(), &worktree).is_err());
    }

    #[test]
    fn checkout_grant_must_match_captured_physical_view() {
        let deployment = CdnViewerDeployment::new(
            "s3://origin-bucket",
            "team/models",
            "https://edge.example.test",
        )
        .unwrap();
        let scope = StorageScope {
            repo_prefix: "views/one".into(),
            global_prefix: "views/shared".into(),
            source_repo: "team/models".into(),
            scope_hash: "principal-one".into(),
        };
        let target = [1; 32];
        let store = crab_storage::Store::new(Arc::new(InMemory::new()))
            .with_native_cdn_origin(Some(deployment.origin.clone()))
            .with_target_identity(target)
            .with_storage_scope(scope.clone());
        assert!(verify_target(&target, Some(&scope), &deployment, &store).is_ok());
        assert!(verify_target(&target, None, &deployment, &store).is_err());
        assert!(verify_target(&[2; 32], Some(&scope), &deployment, &store).is_err());
    }
}
