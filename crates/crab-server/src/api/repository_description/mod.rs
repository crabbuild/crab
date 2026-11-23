use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    middleware,
    routing::get,
};
use uuid::Uuid;

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    catalog::{CatalogDocument, CatalogError, CatalogStore, description::Description},
    repository::{Repository, settings},
    server::Server,
};

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repositories/{repository}/description",
            get(read).put(write),
        )
        .layer(DefaultBodyLimit::max(4096))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

async fn authorized(
    server: &Server,
    principal: &Principal,
    repository: Uuid,
) -> Result<(CatalogStore, CatalogDocument, Arc<Repository>)> {
    app::account(principal)?;
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let (document, _) = catalog.load().await.map_err(app::repository_access_error)?;
    let record = document
        .repository_by_id(repository)
        .ok_or(Error::NotFound)?;
    let active = server
        .repositories
        .by_id(repository)
        .ok_or(Error::NotFound)?;
    let snapshot = document
        .membership_snapshot(record)
        .map_err(app::repository_access_error)?;
    let grant = app::membership_snapshot(server, principal, &active, snapshot).await?;
    if !grant.principal.can_maintain(repository) {
        return Err(Error::NotFound);
    }
    Ok((catalog, document, active))
}

async fn read(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(repository): Path<Uuid>,
) -> Result<Json<Description>> {
    let (_, document, _) = authorized(&server, &principal, repository).await?;
    let record = document
        .repository_by_id(repository)
        .ok_or(Error::NotFound)?;
    Ok(Json(record.description()))
}

async fn write(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(repository): Path<Uuid>,
    Json(input): Json<Description>,
) -> Result<Json<Description>> {
    let (catalog, document, active) = authorized(&server, &principal, repository).await?;
    let record = document
        .repository_by_id(repository)
        .ok_or(Error::NotFound)?;
    if input.description != record.description().description
        && settings::load_lifecycle(&server, &active).await?.archived
    {
        return Err(Error::Archived);
    }
    let published = catalog
        .change_description(document.version, repository, input)
        .await
        .map_err(|error| match error {
            CatalogError::Conflict => Error::RepositoryDescriptionChanged,
            error => app::repository_access_error(error),
        })?;
    let result = published
        .repository_by_id(repository)
        .ok_or(Error::NotFound)?
        .description();
    server
        .install_catalog(published)
        .await
        .map_err(|error| Error::RepositorySetup(Box::new(error)))?;
    Ok(Json(result))
}

#[cfg(test)]
mod tests;
