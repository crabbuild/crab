use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    middleware,
    routing::{get, put},
};
use uuid::Uuid;

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    catalog::{
        CatalogDocument, CatalogError, CatalogStore, attributes::Attributes,
        description::Description, forking::Forking, topics::Topics,
    },
    repository::{Repository, settings},
    server::Server,
};

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repositories/{repository}/description",
            get(read).put(write),
        )
        .route(
            "/api/repositories/{repository}/topics",
            get(read_topics).put(write_topics),
        )
        .route(
            "/api/repositories/{repository}/attributes",
            get(read_attributes).put(write_attributes),
        )
        .route("/api/repositories/{repository}/forking", put(write_forking))
        .layer(DefaultBodyLimit::max(4096))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

async fn authorized(
    server: &Server,
    principal: &Principal,
    repository: Uuid,
    allowed: fn(&Principal, Uuid) -> bool,
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
    if !allowed(&grant.principal, repository) {
        return Err(Error::NotFound);
    }
    Ok((catalog, document, active))
}

async fn read(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(repository): Path<Uuid>,
) -> Result<Json<Description>> {
    let (_, document, _) =
        authorized(&server, &principal, repository, Principal::can_maintain).await?;
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
    let (catalog, document, active) =
        authorized(&server, &principal, repository, Principal::can_maintain).await?;
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

async fn read_topics(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(repository): Path<Uuid>,
) -> Result<Json<Topics>> {
    let (_, document, _) = authorized(&server, &principal, repository, Principal::can_read).await?;
    let record = document
        .repository_by_id(repository)
        .ok_or(Error::NotFound)?;
    Ok(Json(record.topics.clone()))
}

async fn write_topics(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(repository): Path<Uuid>,
    Json(mut input): Json<Topics>,
) -> Result<Json<Topics>> {
    let (catalog, document, active) =
        authorized(&server, &principal, repository, Principal::can_maintain).await?;
    input.normalize().map_err(app::repository_access_error)?;
    let record = document
        .repository_by_id(repository)
        .ok_or(Error::NotFound)?;
    if input.names != record.topics.names
        && settings::load_lifecycle(&server, &active).await?.archived
    {
        return Err(Error::Archived);
    }
    let published = catalog
        .change_topics(document.version, repository, input)
        .await
        .map_err(|error| match error {
            CatalogError::Conflict => Error::RepositoryTopicsChanged,
            error => app::repository_access_error(error),
        })?;
    let result = published
        .repository_by_id(repository)
        .ok_or(Error::NotFound)?
        .topics
        .clone();
    server
        .install_catalog(published)
        .await
        .map_err(|error| Error::RepositorySetup(Box::new(error)))?;
    Ok(Json(result))
}

async fn read_attributes(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(repository): Path<Uuid>,
) -> Result<Json<Attributes>> {
    let (_, document, _) = authorized(&server, &principal, repository, Principal::can_read).await?;
    let record = document
        .repository_by_id(repository)
        .ok_or(Error::NotFound)?;
    Ok(Json(record.attributes.clone()))
}

async fn write_attributes(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(repository): Path<Uuid>,
    Json(mut input): Json<Attributes>,
) -> Result<Json<Attributes>> {
    let (catalog, document, active) =
        authorized(&server, &principal, repository, Principal::can_maintain).await?;
    input.normalize().map_err(app::repository_access_error)?;
    let record = document
        .repository_by_id(repository)
        .ok_or(Error::NotFound)?;
    let current = &record.attributes;
    if (input.homepage != current.homepage
        || input.license != current.license
        || input.is_template != current.is_template)
        && settings::load_lifecycle(&server, &active).await?.archived
    {
        return Err(Error::Archived);
    }
    let published = catalog
        .change_attributes(document.version, repository, input)
        .await
        .map_err(|error| match error {
            CatalogError::Conflict => Error::RepositoryAttributesChanged,
            error => app::repository_access_error(error),
        })?;
    let result = published
        .repository_by_id(repository)
        .ok_or(Error::NotFound)?
        .attributes
        .clone();
    server
        .install_catalog(published)
        .await
        .map_err(|error| Error::RepositorySetup(Box::new(error)))?;
    Ok(Json(result))
}

async fn write_forking(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(repository): Path<Uuid>,
    Json(input): Json<Forking>,
) -> Result<Json<Forking>> {
    let (_, _, repo) = authorized(&server, &principal, repository, Principal::can_admin).await?;
    let organization = repo
        .organization
        .filter(|_| repo.forked_from.is_none())
        .ok_or(Error::NotFound)?;
    let actor = super::organizations::actor(&server, &principal).await?;
    let service = server.as_ref();
    let published = settings::with_ownership_lease(service, &repo, |cancel| async move {
        let (catalog, document, active) =
            authorized(service, &principal, repository, Principal::can_admin).await?;
        // The lease must cover the repository's current organization even
        // if a transfer committed after the first authorization.
        if active.organization != Some(organization) {
            return Err(Error::Conflict);
        }
        let cells = service.cells().ok_or(Error::CellUnavailable)?;
        let grant =
            crate::repository::access::organization_grant(&cells, organization, actor, false)
                .await?;
        if !grant.private_forks {
            return Err(Error::RepositoryForkingRestricted);
        }
        if settings::load_lifecycle(service, &active).await?.archived {
            return Err(Error::Archived);
        }
        if cancel.is_cancelled() {
            return Err(Error::MergeBusy);
        }
        app::account(&principal)?;
        catalog
            .change_forking(document.version, repository, input)
            .await
            .map_err(|error| match error {
                CatalogError::Conflict => Error::Conflict,
                error => app::repository_access_error(error),
            })
    })
    .await?;
    let result = published
        .repository_by_id(repository)
        .ok_or(Error::NotFound)?
        .forking;
    server
        .install_catalog(published)
        .await
        .map_err(|error| Error::RepositorySetup(Box::new(error)))?;
    Ok(Json(result))
}

#[cfg(test)]
mod tests;
