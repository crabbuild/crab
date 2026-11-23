use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    middleware,
    routing::get,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    catalog::{CatalogDocument, CatalogError, CatalogRecord, CatalogStore, Rename},
    repository::settings,
    server::Server,
};

#[derive(Serialize)]
struct Name {
    id: Uuid,
    owner: String,
    name: String,
    version: u32,
}

impl From<&CatalogRecord> for Name {
    fn from(record: &CatalogRecord) -> Self {
        Self {
            id: record.id,
            owner: record.owner.clone(),
            name: record.name.clone(),
            version: record.name_version(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    name: String,
    version: u32,
    request_id: Uuid,
}

#[derive(Debug, thiserror::Error)]
enum PublicationError {
    #[error(transparent)]
    Application(#[from] Error),
    #[error(transparent)]
    Lease(#[from] crab_remote::publication::Error),
}

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route("/api/repositories/{repository}/name", get(read).put(write))
        .layer(DefaultBodyLimit::max(1024))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

async fn authorized(
    server: &Server,
    principal: &Principal,
    repository: Uuid,
) -> Result<(CatalogStore, CatalogDocument)> {
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
    let membership = app::membership_snapshot(server, principal, &active, snapshot).await?;
    if !membership.principal.can_admin(repository) {
        return Err(Error::NotFound);
    }
    Ok((catalog, document))
}

async fn read(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(repository): Path<Uuid>,
) -> Result<Json<Name>> {
    let (_, document) = authorized(&server, &principal, repository).await?;
    Ok(Json(
        document
            .repository_by_id(repository)
            .ok_or(Error::NotFound)?
            .into(),
    ))
}

async fn write(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(repository): Path<Uuid>,
    Json(change): Json<Change>,
) -> Result<Json<Name>> {
    let (_, document) = authorized(&server, &principal, repository).await?;
    let organization = document
        .repository_by_id(repository)
        .ok_or(Error::NotFound)?
        .organization_id();
    let active = server
        .repositories
        .by_id(repository)
        .ok_or(Error::NotFound)?;
    let (send, reply) = tokio::sync::oneshot::channel();
    let tracker = server.receives.clone();
    tracker.spawn(async move {
        let scope = match organization {
            Some(id) => settings::RulesetScope::Organization(id),
            None => settings::RulesetScope::Repository(&active),
        };
        let result = settings::with_ruleset_lease(&server, scope, &server.cancellation, |cancel| {
            let server = server.as_ref();
            async move {
                commit(server, &principal, repository, organization, change, cancel)
                    .await
                    .map_err(PublicationError::Application)
            }
        })
        .await
        .map_err(|error| match error {
            PublicationError::Application(error) => error,
            PublicationError::Lease(error) => Error::RepositoryNamePublication(error),
        });
        let _ = send.send(result);
    });
    Ok(Json(reply.await.map_err(|_| {
        Error::RepositoryNamePublication(crab_remote::publication::Error::Cancelled)
    })??))
}

async fn commit(
    server: &Server,
    principal: &Principal,
    repository: Uuid,
    organization: Option<Uuid>,
    change: Change,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<Name> {
    let (catalog, document) = authorized(server, principal, repository).await?;
    let current = document
        .repository_by_id(repository)
        .ok_or(Error::NotFound)?;
    if current.organization_id() != organization {
        return Err(Error::PageChanged);
    }
    if change.name != current.name {
        if change.version != current.name_version() {
            return Err(Error::PageChanged);
        }
        let active = server
            .repositories
            .by_id(repository)
            .ok_or(Error::NotFound)?;
        if settings::load_lifecycle(server, &active).await?.archived {
            return Err(Error::Archived);
        }
        settings::check_rename(server, current, app::account(principal)?).await?;
    }
    // The policy and the catalog write use the same current source address.
    // Receipt replay does not change that address and survives later policies.
    if cancel.is_cancelled() {
        return Err(Error::RepositoryNamePublication(
            crab_remote::publication::Error::Cancelled,
        ));
    }
    let published = catalog
        .rename_repository(
            document.version,
            Rename {
                repository,
                expected_version: change.version,
                request: change.request_id,
                name: change.name,
            },
        )
        .await
        .map_err(|error| match error {
            CatalogError::NameUnavailable => Error::RepositoryNameUnavailable,
            CatalogError::Conflict => Error::PageChanged,
            error => app::repository_access_error(error),
        })?;
    let result = Name::from(
        published
            .repository_by_id(repository)
            .ok_or(Error::NotFound)?,
    );
    server
        .install_catalog(published)
        .await
        .map_err(|error| Error::RepositorySetup(Box::new(error)))?;
    Ok(result)
}

#[cfg(test)]
mod tests;
