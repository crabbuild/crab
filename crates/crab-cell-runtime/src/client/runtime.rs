//! In-process routing for Cells admitted after a client was created.

use super::*;
use crate::cell::actor::CellRuntime;
use crate::cell::catalog::CellCatalog;
use crate::control::authority::CellAuthority;
use crate::ltx::CellStorageLayout;

/// Selects a local owner before an invocation can be forwarded to a peer.
///
/// The product may acquire an idle, cataloged Cell through runtime admission.
/// Returning `None` delegates to the remote transport; errors stop dispatch.
pub trait LocalCellResolver: Send + Sync + 'static {
    /// Returns the authorized local owner, or `None` when routing must continue remotely.
    fn resolve(
        &self,
        target: CellTarget,
    ) -> Pin<Box<dyn Future<Output = Result<Option<CellHandle>>> + Send + 'static>>;
}

#[derive(Clone)]
pub(super) struct RuntimeCellTransport {
    registry: Arc<Registry>,
    resolver: Arc<dyn LocalCellResolver>,
    remote: Option<Arc<dyn CellTransport>>,
}

impl RuntimeCellTransport {
    pub(super) fn new(
        registry: Arc<Registry>,
        runtime: CellRuntime,
        layout: CellStorageLayout,
    ) -> Self {
        Self {
            registry,
            resolver: Arc::new(RuntimeLocalResolver { runtime, layout }),
            remote: None,
        }
    }

    pub(super) fn with_resolver(
        registry: Arc<Registry>,
        resolver: Arc<dyn LocalCellResolver>,
        remote: Arc<dyn CellTransport>,
    ) -> Self {
        Self {
            registry,
            resolver,
            remote: Some(remote),
        }
    }

    async fn owner(&self, target: &CellTarget) -> Result<Arc<dyn CellTransport>> {
        let local = self.resolver.resolve(target.clone()).await?;
        let Some(handle) = local else {
            return self
                .remote
                .clone()
                .ok_or(Error::Control("target Cell is not locally owned"));
        };
        Ok(Arc::new(LocalCellTransport {
            registry: self.registry.clone(),
            handles: Arc::new(HashMap::from([(handle.cell_id(), handle.clone())])),
            handle,
            telemetry: CellTelemetryHandle::default(),
        }))
    }
}

#[derive(Clone)]
pub(super) struct RuntimeLocalResolver {
    pub(super) runtime: CellRuntime,
    pub(super) layout: CellStorageLayout,
}

impl LocalCellResolver for RuntimeLocalResolver {
    fn resolve(
        &self,
        target: CellTarget,
    ) -> Pin<Box<dyn Future<Output = Result<Option<CellHandle>>> + Send + 'static>> {
        let resolver = self.clone();
        Box::pin(async move {
            let catalog = CellCatalog::new(resolver.layout.clone(), target.tenant())
                .lookup(target.cell_id())
                .await?
                .ok_or(Error::Control("target Cell is not cataloged"))?;
            let control = CellAuthority::new(resolver.layout)
                .load(target.cell_id())
                .await?
                .ok_or(Error::Control("target Cell has no authority record"))?;
            resolver.runtime.local_handle(catalog, &control).await
        })
    }
}

impl CellTransport for RuntimeCellTransport {
    fn describe(
        &self,
        target: CellTarget,
    ) -> Pin<Box<dyn Future<Output = Result<CellDescription>> + Send + 'static>> {
        let client = self.clone();
        Box::pin(async move { client.owner(&target).await?.describe(target).await })
    }

    fn command(
        &self,
        command: EncodedCommand,
    ) -> Pin<Box<dyn Future<Output = Result<StoredOutcome>> + Send + 'static>> {
        let client = self.clone();
        Box::pin(async move { client.owner(&command.target).await?.command(command).await })
    }

    fn query(
        &self,
        query: EncodedQuery,
    ) -> Pin<Box<dyn Future<Output = Result<EncodedObservation>> + Send + 'static>> {
        let client = self.clone();
        Box::pin(async move { client.owner(&query.target).await?.query(query).await })
    }

    fn resolve(
        &self,
        resolve: EncodedResolve,
    ) -> Pin<Box<dyn Future<Output = Result<Resolution>> + Send + 'static>> {
        let client = self.clone();
        Box::pin(async move { client.owner(&resolve.target).await?.resolve(resolve).await })
    }
}
