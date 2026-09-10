use crate::DirectStoreOptions;

/// Builder for a client with explicitly selected storage.
#[derive(Default)]
pub struct ClientBuilder {
    pub(crate) store: Option<DirectStoreOptions>,
    #[cfg(feature = "content")]
    pub(crate) cache: Option<crate::ContentCache>,
    #[cfg(feature = "local")]
    pub(crate) local_tools: Option<crate::LocalTools>,
    #[cfg(feature = "local")]
    pub(crate) local_execution_policy: crate::LocalExecutionPolicy,
    #[cfg(feature = "managed")]
    pub(crate) managed: Option<crate::ManagedOptions>,
}

impl ClientBuilder {
    /// Select the storage configuration consumed by this client.
    #[must_use]
    pub fn direct_store(mut self, options: DirectStoreOptions) -> Self {
        self.store = Some(options);
        self
    }

    /// Select explicit disk placement and retention budget for Crab reconstruction.
    #[cfg(feature = "content")]
    #[must_use]
    pub fn content_cache(mut self, cache: crate::ContentCache) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Configure exact tools and execution policy for local repository workflows.
    #[cfg(feature = "local")]
    #[must_use]
    pub fn local(mut self, options: crate::local::Options) -> Self {
        self.local_tools = Some(options.tools);
        self.local_execution_policy = options.execution_policy;
        self
    }

    /// Select managed-service profile, token-cache and authority resolution.
    #[cfg(feature = "managed")]
    #[must_use]
    pub fn managed(mut self, options: crate::ManagedOptions) -> Self {
        self.managed = Some(options);
        self
    }
}
