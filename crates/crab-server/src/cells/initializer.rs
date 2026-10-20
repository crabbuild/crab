use cellar::sdk::{NodeOutcome, ShutdownMode};
use tokio::net::TcpListener;
use uuid::Uuid;

use crate::{
    Config, Error, Result,
    catalog::{CatalogStore, RepositoryApplicationState},
};

pub(crate) async fn initialize_repository(config: &Config, repository: Uuid) -> Result<()> {
    let catalog = CatalogStore::from_config(config)?;
    let (document, _) = catalog.load().await?;
    let record = document
        .repositories
        .iter()
        .find(|record| record.id == repository)
        .ok_or(crate::catalog::CatalogError::NotFound)?;
    if record.application == RepositoryApplicationState::CellReady {
        return Ok(());
    }
    std::fs::create_dir_all(&config.cells.data_dir)?;
    let local = tempfile::Builder::new()
        .prefix("crab-cellar-init-")
        .tempdir_in(&config.cells.data_dir)?;
    let public = TcpListener::bind("127.0.0.1:0").await?;
    let internal = TcpListener::bind("127.0.0.1:0").await?;
    let address = internal.local_addr()?;
    let mut maintenance = config.clone();
    maintenance.cells.data_dir = local.path().to_owned();
    // The initializer has no lasting local cache or peer. Catalog readiness
    // must therefore be backed by object publication before it exits.
    maintenance.cells.durability = crate::CellDurabilityMode::Object;
    maintenance.cells.peer_advertise = url::Url::parse(&format!("https://{address}"))
        .map_err(|_| Error::Config("repository initialization address is invalid"))?;
    // Only a not-yet-published repository enters this short-lived node. Publish
    // readiness after its Cellar domain has flushed and joined, so serving
    // nodes cannot route new traffic to the initializer's loopback endpoint.
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|source| Error::Cell(super::Error::sdk(source)))?;
    let node_name = format!("crab-init-{}", Uuid::now_v7());
    let started = match super::runtime::start(
        &maintenance,
        public.local_addr()?,
        internal,
        node_name.clone(),
        client.clone(),
        client,
        1,
    )
    .await
    {
        Ok(started) => started,
        Err(error) => {
            // Failed startup cannot prove that all begun native work joined.
            let _retained = local.keep();
            return Err(error.into());
        }
    };
    let initialized: Result<()> = async {
        let cell = started.client.repository(repository)?;
        cell.client.initialize(repository).await?;
        Ok(())
    }
    .await;
    let stopped = match started.node.shutdown(ShutdownMode::Preserve).await {
        Ok(stopped) => stopped,
        Err(error) => {
            let _retained = local.keep();
            return Err(super::Error::sdk(error).into());
        }
    };
    let NodeOutcome::Stopped(report) = stopped else {
        let _retained = local.keep();
        return Err(Error::Config(
            "repository initialization lost its Cellar node",
        ));
    };
    if !report.ingress_drained
        || !report.application_work_drained
        || !report.cells_drained
        || report.drain_timed_out
    {
        // A failed drain may leave recovery evidence in this directory.
        let _retained = local.keep();
        return Err(Error::Config(
            "repository initialization did not complete its Cellar drain",
        ));
    }
    initialized?;
    wait_for_initializer_lease(config, &node_name).await?;
    catalog.mark_cell_ready(repository).await?;
    Ok(())
}

async fn wait_for_initializer_lease(config: &Config, node: &str) -> Result<()> {
    let bucket = super::runtime::bucket(config)?;
    let ownership = cellar::ownership_store::BucketOwnership::new(
        bucket.clone(),
        bucket,
        node.to_owned(),
        String::new(),
    );
    let domain = cellar::sdk::ExecutionDomain::new(tokio::runtime::Handle::current());
    // Preserve closes the initializer safely without requiring a successor.
    // Its last lease must expire before the catalog sends serving traffic to it.
    let settled = domain
        .run(async {
            tokio::time::timeout(std::time::Duration::from_secs(30), async {
                loop {
                    let lease = ownership
                        .read_node_lease(node)
                        .await
                        .map_err(super::Error::sdk)?;
                    let now = cellar::ownership_store::now_ms();
                    let remaining = lease.map_or(0, |lease| lease.expires_ms.saturating_sub(now));
                    if remaining == 0 {
                        return Ok::<(), super::Error>(());
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(remaining.min(250))).await;
                }
            })
            .await
            .map_err(super::Error::sdk)?
        })
        .await;
    domain.shutdown().await;
    settled.map_err(super::Error::sdk)??;
    Ok(())
}
