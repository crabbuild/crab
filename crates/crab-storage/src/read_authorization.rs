use std::sync::Arc;

use object_store::path::Path;

use crate::{Result, StorageError, Store};

/// Revalidate a captured online grant before reading data without provider I/O.
#[async_trait::async_trait]
pub trait ReadAuthorization: Send + Sync {
    /// Renew expiring authority and reject any change to the captured view.
    async fn check(&self) -> Result<()>;
}

#[derive(Clone)]
pub(crate) struct BoundReadAuthorization {
    provider: Arc<dyn ReadAuthorization>,
    cache_namespace: [u8; 32],
    repo_prefix: Path,
    global_prefix: Path,
}

impl Store {
    /// Bind a resolved physical target to the authorization of its provider backend.
    pub fn with_read_authorization(
        mut self,
        authorization: Arc<dyn ReadAuthorization>,
        repository_prefix: &str,
    ) -> Result<Self> {
        let target = self
            .target_identity()
            .ok_or_else(|| StorageError::Forbidden {
                path: "online read authority requires a physical storage target".into(),
            })?;
        let scope = self.storage_scope();
        let repo_prefix = scope.map_or(repository_prefix, |view| view.repo_prefix.as_str());
        let global_prefix = scope.map_or(crate::GLOBAL_PREFIX, |view| view.global_prefix.as_str());
        let mut hash = blake3::Hasher::new_derive_key("crab online read cache v1");
        hash.update(target);
        for component in [
            repo_prefix,
            global_prefix,
            scope.map_or(repository_prefix, |view| view.source_repo.as_str()),
            scope.map_or("", |view| view.scope_hash.as_str()),
        ] {
            hash.update(&(component.len() as u64).to_be_bytes());
            hash.update(component.as_bytes());
        }
        self.read_authorization = Some(BoundReadAuthorization {
            provider: authorization,
            cache_namespace: *hash.finalize().as_bytes(),
            repo_prefix: Path::from(repo_prefix),
            global_prefix: Path::from(global_prefix),
        });
        Ok(self)
    }

    /// Payload namespace for an authorized physical target and view.
    #[must_use]
    pub fn authorized_cache_namespace(&self) -> Option<&[u8; 32]> {
        self.read_authorization
            .as_ref()
            .map(|bound| &bound.cache_namespace)
    }

    /// Whether reads are tied to a refreshable grant for this view.
    #[must_use]
    pub fn has_bound_read_authority(&self) -> bool {
        self.read_authorization.is_some()
    }

    /// Check online authority independently of a physical read or cache hit.
    pub async fn check_read_authority(&self) -> Result<()> {
        if let Some(authorization) = &self.read_authorization {
            authorization
                .provider
                .check()
                .await
                .map_err(|source| StorageError::ReadRejected {
                    source: Box::new(source),
                })?;
        }
        Ok(())
    }

    /// Authorize one cache key in the captured storage view before lookup.
    pub async fn check_cached_read(&self, path: &Path) -> Result<()> {
        if let Some(bound) = &self.read_authorization
            && !path.prefix_matches(&bound.repo_prefix)
            && !path.prefix_matches(&bound.global_prefix)
        {
            return Err(StorageError::ReadRejected {
                source: Box::new(StorageError::Forbidden {
                    path: path.to_string(),
                }),
            });
        }
        self.check_read_authority().await
    }
}
