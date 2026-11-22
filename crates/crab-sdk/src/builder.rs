use crate::DirectStoreOptions;

/// Builder for a client with explicitly selected storage.
#[derive(Default)]
pub struct ClientBuilder {
    pub(crate) store: Option<DirectStoreOptions>,
    #[cfg(feature = "files")]
    pub(crate) cache: Option<crate::ContentCache>,
    #[cfg(feature = "local")]
    pub(crate) local_tools: Option<crate::LocalTools>,
    #[cfg(feature = "local")]
    pub(crate) local_execution_policy: crate::LocalExecutionPolicy,
    #[cfg(feature = "service")]
    pub(crate) managed: Option<crate::ManagedOptions>,
    #[cfg(feature = "service")]
    pub(crate) direct_cdn: Option<crate::storage::DirectCdnOptions>,
    #[cfg(feature = "service")]
    pub(crate) desktop_browsing: bool,
}

impl ClientBuilder {
    /// Select the storage configuration consumed by this client.
    #[must_use]
    pub fn direct_store(mut self, options: DirectStoreOptions) -> Self {
        self.store = Some(options);
        self
    }

    /// Select an explicit CDN export for native direct reads of one repository.
    #[cfg(feature = "service")]
    #[must_use]
    pub fn direct_cdn(mut self, options: crate::storage::DirectCdnOptions) -> Self {
        self.direct_cdn = Some(options);
        self
    }

    /// Use the desktop browse qualification scope for this native sidecar client.
    #[cfg(feature = "service")]
    #[must_use]
    pub fn for_desktop_browsing(mut self) -> Self {
        self.desktop_browsing = true;
        self
    }

    /// Select explicit disk placement and retention budget for Crab reconstruction.
    #[cfg(feature = "files")]
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
    #[cfg(feature = "service")]
    #[must_use]
    pub fn managed(mut self, options: crate::ManagedOptions) -> Self {
        self.managed = Some(options);
        self
    }
}
