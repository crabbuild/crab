//! Product integration boundaries supplied by the Crab CLI.

use std::sync::Arc;

use crate::{Result, StoreLayout};

/// Fully configured read path for a mounted Crab repository.
#[derive(Clone)]
pub struct MountReadContext {
    pub store_layout: StoreLayout,
    pub hydrator: Arc<crab_read::ShardHydrator>,
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
