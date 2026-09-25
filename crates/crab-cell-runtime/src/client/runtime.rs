//! In-process routing for Cells admitted after a client was created.

use super::*;
use crate::cell::actor::CellRuntime;
use crate::cell::catalog::CellCatalog;
use crate::control::authority::CellAuthority;
use crate::ltx::CellStorageLayout;
use crate::peer::{PeerClientTransport, PeerPrincipal, PeerRoundTrip, PeerSigner};

#[derive(Clone)]
pub(super) struct RuntimeCellTransport {
    registry: Arc<Registry>,
    runtime: CellRuntime,
    layout: CellStorageLayout,
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
            runtime,
            layout,
            remote: None,
        }
    }

    pub(super) fn with_peer(
        registry: Arc<Registry>,
        runtime: CellRuntime,
        layout: CellStorageLayout,
        signer: Arc<PeerSigner>,
        principal: PeerPrincipal,
        round_trip: Arc<dyn PeerRoundTrip>,
    ) -> Self {
        Self {
            registry,
            runtime,
            layout,
            remote: Some(Arc::new(PeerClientTransport::new(
                signer, principal, round_trip,
            ))),
        }
    }

    async fn owner(&self, target: &CellTarget) -> Result<Arc<dyn CellTransport>> {
        let catalog = CellCatalog::new(self.layout.clone(), target.tenant())
            .lookup(target.cell_id())
            .await?
            .ok_or(Error::Control("target Cell is not cataloged"))?;
        let authority = CellAuthority::new(self.layout.clone());
        let control = authority
            .load(target.cell_id())
            .await?
            .ok_or(Error::Control("target Cell has no authority record"))?;
        let local = self.runtime.local_handle(catalog, &control).await?;
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
