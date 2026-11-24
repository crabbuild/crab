use std::{cmp::Reverse, sync::Arc};

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    middleware,
    routing::{get, put},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[cfg(test)]
mod tests;

use super::repository_transfer::{
    Management, SourceAccess, account, management_access, organization_owner, source_repository,
};
use crate::{
    app::{self, Error, Result},
    auth::Principal,
    catalog::{
        CatalogDocument, CatalogError, CatalogRecord, MembershipActor,
        deletion::{Delete, RESTORE_WINDOW_MS, Restore},
    },
    repository::settings::{self, RulesetScope},
    server::Server,
};

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repositories/{repository}/deletion",
            get(status).delete(delete),
        )
        .route(
            "/api/organizations/{organization}/deleted-repositories",
            get(list),
        )
        .route(
            "/api/organizations/{organization}/deleted-repositories/{repository}",
            put(restore),
        )
        .layer(axum::extract::DefaultBodyLimit::max(4096))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

#[derive(Serialize)]
struct Status {
    version: u32,
    can_delete: bool,
    forks: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteInput {
    request_id: Uuid,
    version: u32,
    confirmation: String,
}

#[derive(Serialize)]
struct Deleted {
    organization: Uuid,
}

#[derive(Serialize)]
struct Entry {
    id: Uuid,
    owner: String,
    name: String,
    deletion_id: Uuid,
    deleted_at_ms: u64,
    expires_at_ms: u64,
    can_restore: bool,
    forks: usize,
}

#[derive(Serialize)]
struct Page {
    items: Vec<Entry>,
    next: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    before: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreInput {
    deletion_id: Uuid,
}

#[derive(Serialize)]
struct RepositoryView {
    id: Uuid,
    owner: String,
    name: String,
}

fn catalog_error(error: CatalogError) -> Error {
    match error {
        CatalogError::NameUnavailable => Error::RepositoryNameUnavailable,
        CatalogError::Conflict => Error::Conflict,
        error => app::repository_access_error(error),
    }
}

fn now() -> Result<u64> {
    u64::try_from(crate::cells::unix_now_ms().map_err(Error::Cell)?)
        .map_err(|_| Error::CellContract("The server clock is invalid"))
}

async fn access(
    server: &Server,
    document: &CatalogDocument,
    record: &CatalogRecord,
    actor: &MembershipActor,
) -> Result<SourceAccess> {
    if record.organization_id().is_none() {
        return Err(Error::NotFound);
    }
    let upstream = record
        .forked_from
        .and_then(|id| document.repository_by_id(id));
    let source = record.access_record(upstream).map_err(catalog_error)?;
    match management_access(server, source, actor, Management::Delete).await? {
        SourceAccess::None => Err(Error::NotFound),
        access => Ok(access),
    }
}

fn require_deletion(access: SourceAccess) -> Result<()> {
    if access == SourceAccess::Allowed {
        Ok(())
    } else {
        Err(Error::RepositoryDeletionRestricted)
    }
}

async fn status(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(repository): Path<Uuid>,
) -> Result<Json<Status>> {
    let actor = account(&principal)?;
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let (document, _) = catalog.load_snapshot().await.map_err(catalog_error)?;
    let record = document
        .repository_by_id(repository)
        .ok_or(Error::NotFound)?;
    let access = access(&server, &document, record, &actor).await?;
    Ok(Json(Status {
        version: record.name_version(),
        can_delete: access == SourceAccess::Allowed,
        forks: document
            .repositories
            .iter()
            .filter(|fork| fork.forked_from == Some(repository))
            .count(),
    }))
}

async fn delete(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(repository): Path<Uuid>,
    Json(input): Json<DeleteInput>,
) -> Result<Json<Deleted>> {
    let actor = account(&principal)?;
    if input.request_id.is_nil() {
        return Err(Error::Invalid("A deletion submission ID is required"));
    }
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let (document, _) = catalog.load_snapshot().await.map_err(catalog_error)?;
    if let Some(deleted) = document.deleted_repository(repository)
        && deleted.same_deletion(input.request_id, &actor)
    {
        let organization = deleted
            .repository
            .organization_id()
            .ok_or(Error::NotFound)?;
        server
            .install_catalog(document.as_ref().clone())
            .await
            .map_err(|error| Error::RepositorySetup(Box::new(error)))?;
        return Ok(Json(Deleted { organization }));
    }
    let record = document
        .repository_by_id(repository)
        .ok_or(Error::NotFound)?;
    let organization = record.organization_id().ok_or(Error::NotFound)?;
    require_deletion(access(&server, &document, record, &actor).await?)?;
    if input.confirmation != format!("{}/{}", record.owner, record.name) {
        return Err(Error::Invalid(
            "Enter the full repository name to confirm deletion",
        ));
    }
    if input.version != record.name_version() {
        return Err(Error::Conflict);
    }
    let source = source_repository(&server, &document, record).await?;
    let document = document.as_ref();
    let service = server.as_ref();
    let updated = settings::with_ownership_lease(service, &source, |cancel| async move {
        require_deletion(access(service, document, record, &actor).await?)?;
        // Block deletion while an issue publication still owns any destination
        // in this fork network. All of these repositories share the org lease.
        for record in &document.repositories {
            if record.id != repository && record.forked_from != Some(repository) {
                continue;
            }
            let repository = source_repository(service, document, record).await?;
            settings::require_no_issue_reservations(service, &repository).await?;
        }
        account(&principal)?;
        if cancel.is_cancelled() {
            return Err(Error::MergeBusy);
        }
        catalog
            .delete_repository(
                document.version,
                Delete {
                    repository,
                    organization,
                    operation: input.request_id,
                    expected_version: input.version,
                    actor,
                    now: now()?,
                },
            )
            .await
            .map_err(catalog_error)
    })
    .await?;
    server
        .install_catalog(updated)
        .await
        .map_err(|error| Error::RepositorySetup(Box::new(error)))?;
    Ok(Json(Deleted { organization }))
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(organization): Path<Uuid>,
    Query(query): Query<List>,
) -> Result<Json<Page>> {
    let actor = account(&principal)?;
    if !organization_owner(&server, organization, &actor).await? {
        return Err(Error::NotFound);
    }
    let before = query
        .before
        .as_deref()
        .map(|value| {
            value
                .split_once(':')
                .and_then(|(ms, id)| Some((ms.parse::<u64>().ok()?, Uuid::parse_str(id).ok()?)))
                .ok_or(Error::Invalid("Invalid deleted repository cursor"))
        })
        .transpose()?;
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let (document, _) = catalog.load_snapshot().await.map_err(catalog_error)?;
    let now = now()?;
    let mut rows = Vec::new();
    let mut children = std::collections::HashMap::<Uuid, usize>::new();
    for row in &document.deleted_repositories {
        if row.repository.organization_id() != Some(organization)
            || row.deleted_at_ms.saturating_add(RESTORE_WINDOW_MS) <= now
        {
            continue;
        }
        if row.root == row.repository.id {
            if before.is_none_or(|cursor| (row.deleted_at_ms, row.root) < cursor) {
                rows.push(row);
            }
        } else {
            *children.entry(row.root).or_default() += 1;
        }
    }
    const LIMIT: usize = 32;
    rows.sort_unstable_by_key(|row| Reverse((row.deleted_at_ms, row.root)));
    let next = (rows.len() > LIMIT)
        .then(|| format!("{}:{}", rows[LIMIT - 1].deleted_at_ms, rows[LIMIT - 1].root));
    let items = rows
        .into_iter()
        .take(LIMIT)
        .map(|row| Entry {
            id: row.repository.id,
            owner: row.repository.owner.clone(),
            name: row.repository.name.clone(),
            deletion_id: row.operation,
            deleted_at_ms: row.deleted_at_ms,
            expires_at_ms: row.deleted_at_ms.saturating_add(RESTORE_WINDOW_MS),
            can_restore: row.restorable(now),
            forks: children.get(&row.root).copied().unwrap_or(0),
        })
        .collect();
    Ok(Json(Page { items, next }))
}

async fn restore(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((organization, repository)): Path<(Uuid, Uuid)>,
    Json(input): Json<RestoreInput>,
) -> Result<Json<RepositoryView>> {
    let actor = account(&principal)?;
    if !organization_owner(&server, organization, &actor).await? {
        return Err(Error::NotFound);
    }
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let service = server.as_ref();
    let updated = settings::with_ruleset_lease(
        service,
        RulesetScope::Organization(organization),
        &service.cancellation,
        |cancel| async move {
            if !organization_owner(service, organization, &actor).await? {
                return Err(Error::NotFound);
            }
            account(&principal)?;
            if cancel.is_cancelled() {
                return Err(Error::MergeBusy);
            }
            catalog
                .restore_repository(Restore {
                    repository,
                    organization,
                    operation: input.deletion_id,
                    actor,
                    now: now()?,
                })
                .await
                .map_err(catalog_error)
        },
    )
    .await?;
    let restored = updated
        .repository_by_id(repository)
        .ok_or(Error::NotFound)?;
    let view = RepositoryView {
        id: restored.id,
        owner: restored.owner.clone(),
        name: restored.name.clone(),
    };
    server
        .install_catalog(updated)
        .await
        .map_err(|error| Error::RepositorySetup(Box::new(error)))?;
    Ok(Json(view))
}
