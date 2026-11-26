use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    middleware,
    routing::get,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::repository_transfer::{
    Management, SourceAccess, account, management_access, source_repository,
};
use crate::{
    app::{self, Error, Result},
    auth::Principal,
    catalog::{
        CatalogRecord, MembershipActor, RepositoryApplicationState,
        visibility::{Setting, Visibility},
    },
    repository::settings,
    server::Server,
};

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repositories/{repository}/visibility",
            get(read).put(write),
        )
        .layer(DefaultBodyLimit::max(4096))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    version: u64,
    value: Visibility,
    confirmation: String,
}

#[derive(Serialize)]
struct Status {
    setting: Setting,
    can_change: bool,
    private_forks: usize,
    public_forks: usize,
}

async fn access(
    server: &Server,
    record: &CatalogRecord,
    actor: &MembershipActor,
) -> Result<SourceAccess> {
    if record.forked_from.is_some()
        || (record.organization_id().is_none() && !record.is_personal_repository())
    {
        return Err(Error::NotFound);
    }
    match management_access(
        server,
        record,
        record.baseline(),
        actor,
        Management::Visibility,
    )
    .await?
    {
        SourceAccess::None => Err(Error::NotFound),
        access => Ok(access),
    }
}

async fn read(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(repository): Path<Uuid>,
) -> Result<Json<Status>> {
    let actor = account(&principal)?;
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let (document, _) = catalog
        .load_snapshot()
        .await
        .map_err(app::repository_access_error)?;
    let record = document
        .repository_by_id(repository)
        .ok_or(Error::NotFound)?;
    let allowed = access(&server, record, &actor).await?;
    let forks = document
        .repositories
        .iter()
        .filter(|fork| {
            fork.forked_from == Some(repository)
                && (!record.visibility.value.is_public()
                    || fork.application == RepositoryApplicationState::CellReady)
        })
        .count();
    let (private_forks, public_forks) = if record.visibility.value.is_public() {
        (0, forks)
    } else {
        (forks, 0)
    };
    Ok(Json(Status {
        setting: record.visibility,
        can_change: allowed == SourceAccess::Allowed,
        private_forks,
        public_forks,
    }))
}

async fn write(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(repository): Path<Uuid>,
    Json(input): Json<Input>,
) -> Result<Json<Setting>> {
    let actor = account(&principal)?;
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let (document, _) = catalog
        .load_snapshot()
        .await
        .map_err(app::repository_access_error)?;
    let record = document
        .repository_by_id(repository)
        .ok_or(Error::NotFound)?;
    if access(&server, record, &actor).await? != SourceAccess::Allowed {
        return Err(Error::RepositoryVisibilityRestricted);
    }
    let runtime = source_repository(&server, &document, record).await?;
    let runtime = runtime.as_ref();
    let service = server.as_ref();
    let next = settings::with_ownership_lease(service, runtime, |cancel| async move {
        let (document, _) = catalog
            .load_snapshot()
            .await
            .map_err(app::repository_access_error)?;
        let record = document
            .repository_by_id(repository)
            .ok_or(Error::NotFound)?;
        if record.organization_id() != runtime.organization
            || record.ownership_epoch != runtime.ownership_epoch
        {
            return Err(Error::Conflict);
        }
        if access(service, record, &actor).await? != SourceAccess::Allowed {
            return Err(Error::RepositoryVisibilityRestricted);
        }
        if input.confirmation != format!("{}/{}", record.owner, record.name) {
            return Err(Error::Invalid(
                "Enter the full repository name to confirm visibility",
            ));
        }
        if record.visibility.value != input.value
            && settings::load_lifecycle(service, runtime).await?.archived
        {
            return Err(Error::Archived);
        }
        account(&principal)?;
        if cancel.is_cancelled() {
            return Err(Error::MergeBusy);
        }
        let occurred_at = u64::try_from(crate::cells::unix_now_ms().map_err(Error::Cell)?)
            .map_err(|_| Error::CellContract("The server clock is invalid"))?
            / 1_000;
        catalog
            .change_visibility(
                document.version,
                repository,
                Setting {
                    version: input.version,
                    value: input.value,
                },
                actor,
                occurred_at,
            )
            .await
            .map_err(app::repository_access_error)
    })
    .await?;
    let setting = next
        .repository_by_id(repository)
        .ok_or(Error::NotFound)?
        .visibility;
    server
        .install_catalog(next)
        .await
        .map_err(|error| Error::RepositorySetup(Box::new(error)))?;
    Ok(Json(setting))
}

#[cfg(test)]
mod tests;
