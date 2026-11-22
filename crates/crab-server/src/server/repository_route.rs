use serde::Serialize;
use uuid::Uuid;

use super::Server;
use crate::{
    app::{self, Error, Result},
    auth::Principal,
};

#[derive(Serialize)]
pub(crate) struct Destination {
    pub id: Uuid,
    pub owner: String,
    pub name: String,
}

pub(crate) async fn renamed(
    server: &Server,
    principal: &Principal,
    owner: &str,
    name: &str,
    expected: Option<Uuid>,
) -> Result<Option<Destination>> {
    let Some(catalog) = server.catalog() else {
        return Ok(None);
    };
    let (document, _) = catalog
        .load_snapshot()
        .await
        .map_err(app::repository_access_error)?;
    let Some(record) = document.renamed_route(owner, name) else {
        return Ok(None);
    };
    if expected.is_some_and(|expected| expected != record.id) {
        return Ok(None);
    }
    let Some(active) = server.repositories.by_id(record.id) else {
        return Ok(None);
    };
    let snapshot = document
        .membership_snapshot(record)
        .map_err(app::repository_access_error)?;
    // Resolve both the address and grants from the same authoritative record.
    // A stale replica may still have the previous organization loaded.
    let config = record
        .runtime_config(catalog.root(), &active.config.default_branch)
        .map_err(app::repository_access_error)?;
    let current = app::membership_for_config(server, principal, &config, snapshot).await?;
    if !current.principal.can_read(record.id) {
        return Ok(None);
    }
    let destination = Destination {
        id: record.id,
        owner: record.owner.clone(),
        name: record.name.clone(),
    };
    if (active.config.owner != destination.owner || active.config.name != destination.name)
        && server.repositories.version() < document.version
    {
        // A replica may see the durable rename before its refresh timer. Install
        // the authoritative catalog before redirecting to the new address.
        server
            .install_catalog(document.as_ref().clone())
            .await
            .map_err(|error| Error::RepositorySetup(Box::new(error)))?;
        let Some(active) = server.repositories.by_id(destination.id) else {
            return Ok(None);
        };
        if active.config.owner != destination.owner || active.config.name != destination.name {
            return Err(Error::PageChanged);
        }
        app::membership(server, principal, &active).await?;
    }
    Ok(Some(destination))
}
