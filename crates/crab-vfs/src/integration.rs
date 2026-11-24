//! Product integration boundaries supplied by the Crab CLI.

use std::sync::Arc;

use crate::{Result, StoreLayout};

/// Fully configured read path for a mounted Crab repository.
#[derive(Clone)]
pub struct MountReadContext {
    pub store_layout: StoreLayout,
    pub hydrator: Arc<crab_read::ShardHydrator>,
    pub catalog: Arc<dyn MountCatalogCapture>,
}

/// Captures a new immutable catalog while sharing the configured read resources.
#[async_trait::async_trait]
pub trait MountCatalogCapture: Send + Sync {
    async fn capture(&self) -> Result<Arc<crab_read::ShardHydrator>>;

    /// Verify a mount-owned Git revision before publishing a filesystem snapshot.
    fn validate_git_head(&self, _reference: &str, _oid: &str, _read_only: bool) -> Result<()> {
        Ok(())
    }

    /// Drain retained captures after backend requests and hydration workers stop.
    async fn shutdown(&self) -> Result<()>;
}

/// Resolves credentials, replica routing, and hydration configuration for a mount source.
#[async_trait::async_trait]
pub trait MountReadResolver: Send + Sync {
    async fn resolve(&self, remote: &str) -> Result<Option<MountReadContext>>;
}

/// Resolver used by embedders that only mount ordinary Git content.
#[derive(Debug, Default)]
pub struct NoopMountReadResolver;

#[async_trait::async_trait]
impl MountReadResolver for NoopMountReadResolver {
    async fn resolve(&self, _remote: &str) -> Result<Option<MountReadContext>> {
        Ok(None)
    }
}
