use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    RepositoryMember,
    auth::Principal,
    catalog::{CatalogDocument, CatalogError, CatalogStore, MembershipActor},
    server::Server,
};

pub(crate) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route("/api/repos/{owner}/{name}/members", get(list).put(replace))
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024))
}

#[derive(Serialize)]
struct Output {
    revision: u64,
    members: Vec<RepositoryMember>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    expected_revision: u64,
    members: Vec<RepositoryMember>,
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

async fn authorized(
    server: &Server,
    principal: &Principal,
    owner: &str,
    name: &str,
) -> Result<(CatalogStore, CatalogDocument), Box<Response>> {
    if !matches!(principal, Principal::Local | Principal::User(_)) {
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
    let config = record
        .runtime_config(catalog.root(), &active.config.default_branch)
        .map_err(|_| {
            boxed_error(
                StatusCode::NOT_FOUND,
                "repository_not_found",
                "Repository not found.",
            )
        })?;
    if !principal.can_admin(&config) {
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
    let record = document.repositories.into_iter().find(|record| {
        record.owner.eq_ignore_ascii_case(&owner) && record.name.eq_ignore_ascii_case(&name)
    });
    match record {
        Some(record) => Json(Output {
            revision: document.version,
            members: record.members,
        })
        .into_response(),
        None => error(
            StatusCode::NOT_FOUND,
            "repository_not_found",
            "Repository not found.",
        ),
    }
}

async fn replace(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name)): Path<(String, String)>,
    input: Result<Json<Input>, JsonRejection>,
) -> Response {
    let (catalog, document) = match authorized(&server, &principal, &owner, &name).await {
        Ok(value) => value,
        Err(response) => return *response,
    };
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
    // Bind the authorization decision to exactly the caller's revision. Otherwise
    // a caller could name a newer revision after losing its administrator grant.
    if input.expected_revision != document.version {
        return error(
            StatusCode::CONFLICT,
            "membership_changed",
            "Membership changed. Reload members and try again.",
        );
    }
    let actor = match principal {
        Principal::Local => MembershipActor {
            issuer: "urn:crab:local".into(),
            subject: "operator".into(),
        },
        Principal::User(_) => match principal.identity() {
            Some(identity) => MembershipActor {
                issuer: identity.issuer,
                subject: identity.subject,
            },
            None => {
                return error(
                    StatusCode::NOT_FOUND,
                    "repository_not_found",
                    "Repository not found.",
                );
            }
        },
        _ => {
            return error(
                StatusCode::NOT_FOUND,
                "repository_not_found",
                "Repository not found.",
            );
        }
    };
    let old = document
        .repositories
        .iter()
        .find(|record| {
            record.owner.eq_ignore_ascii_case(&owner) && record.name.eq_ignore_ascii_case(&name)
        })
        .map(|record| record.members.clone());
    match catalog
        .replace_members(
            &owner,
            &name,
            input.expected_revision,
            input.members,
            actor,
            server.auth.is_some(),
        )
        .await
    {
        Ok(record) => Json(Output {
            revision: if old.as_ref() == Some(&record.members) {
                document.version
            } else {
                document.version + 1
            },
            members: record.members,
        })
        .into_response(),
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
