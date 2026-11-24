pub(in crate::api) mod roster;
pub(super) mod team_access;

use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Serialize;
use serde_json::json;

use crate::{
    RepositoryGroup, RepositoryMember, app,
    auth::Principal,
    catalog::{AccessUpdate, CatalogDocument, CatalogError, CatalogStore, MembershipActor},
    server::Server,
};

pub(crate) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route("/api/repos/{owner}/{name}/members", get(list).put(replace))
        .merge(team_access::routes())
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024))
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(super) struct MembershipState {
    revision: u64,
    members: Vec<RepositoryMember>,
    groups: Vec<RepositoryGroup>,
    can_admin: bool,
    groups_supported: bool,
}

fn groups_supported(server: &Server) -> bool {
    server.auth.as_ref().is_none_or(|auth| auth.is_oidc())
}

fn error(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    (
        status,
        Json(json!({"error":{"code":code,"message":message}})),
    )
        .into_response()
}

fn boxed_error(status: StatusCode, code: &'static str, message: &'static str) -> Box<Response> {
    Box::new(error(status, code, message))
}

fn membership_error(failure: app::Error) -> Box<Response> {
    if matches!(failure, app::Error::NotFound) {
        boxed_error(
            StatusCode::NOT_FOUND,
            "repository_not_found",
            "Repository not found.",
        )
    } else {
        boxed_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "membership_unavailable",
            "Membership is temporarily unavailable.",
        )
    }
}

async fn authorized(
    server: &Server,
    principal: &Principal,
    owner: &str,
    name: &str,
) -> Result<(CatalogStore, CatalogDocument), Box<Response>> {
    if !(principal.is_local() || principal.is_user()) {
        return Err(boxed_error(
            StatusCode::NOT_FOUND,
            "repository_not_found",
            "Repository not found.",
        ));
    }
    let catalog = server.catalog().ok_or_else(|| {
        boxed_error(
            StatusCode::NOT_FOUND,
            "repository_not_found",
            "Repository not found.",
        )
    })?;
    let (document, _) = catalog.load().await.map_err(|_| {
        boxed_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "membership_unavailable",
            "Membership is temporarily unavailable.",
        )
    })?;
    let record = document
        .repositories
        .iter()
        .find(|record| {
            record.owner.eq_ignore_ascii_case(owner) && record.name.eq_ignore_ascii_case(name)
        })
        .ok_or_else(|| {
            boxed_error(
                StatusCode::NOT_FOUND,
                "repository_not_found",
                "Repository not found.",
            )
        })?;
    let active = server
        .repositories
        .get(&(record.owner.clone(), record.name.clone()))
        .ok_or_else(|| {
            boxed_error(
                StatusCode::NOT_FOUND,
                "repository_not_found",
                "Repository not found.",
            )
        })?;
    let current = app::membership_snapshot(
        server,
        principal,
        &active,
        document
            .membership_snapshot(record)
            .map_err(|error| membership_error(app::Error::Membership(Box::new(error))))?,
    )
    .await
    .map_err(membership_error)?;
    if !current.principal.can_admin(record.id) {
        return Err(boxed_error(
            StatusCode::NOT_FOUND,
            "repository_not_found",
            "Repository not found.",
        ));
    }
    Ok((catalog, document))
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    let (_catalog, document) = match authorized(&server, &principal, &owner, &name).await {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let record = document.repositories.iter().find(|record| {
        record.owner.eq_ignore_ascii_case(&owner) && record.name.eq_ignore_ascii_case(&name)
    });
    match record
        .map(|record| document.membership_snapshot(record))
        .transpose()
    {
        Ok(Some(snapshot)) => Json(MembershipState {
            revision: document.version,
            members: snapshot.members,
            groups: snapshot.groups,
            can_admin: true,
            groups_supported: groups_supported(&server),
        })
        .into_response(),
        Ok(None) => error(
            StatusCode::NOT_FOUND,
            "repository_not_found",
            "Repository not found.",
        ),
        Err(error) => *membership_error(app::Error::Membership(Box::new(error))),
    }
}

async fn replace(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name)): Path<(String, String)>,
    input: Result<Json<AccessUpdate>, JsonRejection>,
) -> Response {
    let (catalog, document) = match authorized(&server, &principal, &owner, &name).await {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let scope = document
        .repositories
        .iter()
        .find(|record| {
            record.owner.eq_ignore_ascii_case(&owner) && record.name.eq_ignore_ascii_case(&name)
        })
        .and_then(crate::catalog::CatalogRecord::organization_id);
    let Some(organization) = scope else {
        return replace_authorized(
            &server,
            &principal,
            &owner,
            &name,
            (catalog, document),
            input,
            None,
        )
        .await;
    };
    // Recheck the administrator and repository scope while holding the same
    // policy lease as Project writes that depend on these direct grants.
    match super::organizations::properties::policy_write(
        server,
        organization,
        move |server, _, cancel| async move {
            let (catalog, document) = match authorized(&server, &principal, &owner, &name).await {
                Ok(value) => value,
                Err(response) => return Ok(*response),
            };
            if !document.repositories.iter().any(|record| {
                record.owner.eq_ignore_ascii_case(&owner)
                    && record.name.eq_ignore_ascii_case(&name)
                    && record.organization_id() == Some(organization)
            }) {
                return Err(app::Error::PageChanged);
            }
            Ok(replace_authorized(
                &server,
                &principal,
                &owner,
                &name,
                (catalog, document),
                input,
                Some(&cancel),
            )
            .await)
        },
    )
    .await
    {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

async fn replace_authorized(
    server: &Server,
    principal: &Principal,
    owner: &str,
    name: &str,
    (catalog, document): (CatalogStore, CatalogDocument),
    input: Result<Json<AccessUpdate>, JsonRejection>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Response {
    if document.repositories.iter().any(|record| {
        record.owner.eq_ignore_ascii_case(owner)
            && record.name.eq_ignore_ascii_case(name)
            && record.forked_from.is_some()
    }) {
        return error(
            StatusCode::FORBIDDEN,
            "fork_access_inherited",
            "Manage fork access on the source repository.",
        );
    }
    let input = match input {
        Ok(Json(input)) => input,
        Err(_) => {
            return error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_membership",
                "Membership is invalid.",
            );
        }
    };
    if !input.groups.is_empty() && !groups_supported(server) {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "oidc_required",
            "SSO group grants require an OpenID Connect provider.",
        );
    }
    // Bind the authorization decision to exactly the caller's revision. Otherwise
    // a caller could name a newer revision after losing its administrator grant.
    if input.expected_revision != document.version {
        return error(
            StatusCode::CONFLICT,
            "membership_changed",
            "Membership changed. Reload members and try again.",
        );
    }
    let Some(identity) = principal.identity() else {
        return error(
            StatusCode::NOT_FOUND,
            "repository_not_found",
            "Repository not found.",
        );
    };
    let actor = MembershipActor {
        issuer: identity.issuer,
        subject: identity.subject,
    };

    let old = document
        .repositories
        .iter()
        .find(|record| {
            record.owner.eq_ignore_ascii_case(owner) && record.name.eq_ignore_ascii_case(name)
        })
        .map(|record| (record.members.clone(), record.groups.clone()));
    if cancel.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
        return app::Error::MergeBusy.into_response();
    }
    match catalog
        .replace_access_under_policy(owner, name, input, actor, server.auth.is_some())
        .await
    {
        Ok(record) => {
            let Some(active) = server
                .repositories
                .get(&(record.owner.clone(), record.name.clone()))
            else {
                return *membership_error(app::Error::NotFound);
            };
            let revision = if old.as_ref() == Some(&(record.members.clone(), record.groups.clone()))
            {
                document.version
            } else {
                document.version + 1
            };
            let can_admin = match app::membership_snapshot(
                server,
                principal,
                &active,
                record.access_snapshot(revision, &record),
            )
            .await
            {
                Ok(current) => current.principal.can_admin(record.id),
                Err(error) => return *membership_error(error),
            };
            Json(MembershipState {
                revision,
                members: record.members,
                groups: record.groups,
                can_admin,
                groups_supported: groups_supported(server),
            })
            .into_response()
        }
        Err(CatalogError::Conflict) => error(
            StatusCode::CONFLICT,
            "membership_changed",
            "Membership changed. Reload members and try again.",
        ),
        Err(CatalogError::NotFound) => error(
            StatusCode::NOT_FOUND,
            "repository_not_found",
            "Repository not found.",
        ),
        Err(CatalogError::Invalid("at least one administrator is required")) => error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "administrator_required",
            "At least one administrator is required.",
        ),
        Err(CatalogError::Invalid(_)) => error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_membership",
            "Membership is invalid.",
        ),
        Err(_) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "membership_unavailable",
            "Membership is temporarily unavailable.",
        ),
    }
}

#[cfg(test)]
mod tests;
