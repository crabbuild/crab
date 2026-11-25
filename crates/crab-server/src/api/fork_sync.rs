use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::get,
};
use crab_git::receive_plan::{ReceivePlanError, RefUpdate};
use gix_hash::ObjectId;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    app,
    auth::Principal,
    git::receive::{ReceiveError, synchronize_fork},
    server::Server,
};

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/fork-sync",
            get(preview).post(update),
        )
        .layer(axum::extract::DefaultBodyLimit::max(4096))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Preview {
    branch: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    branch: String,
    fork_oid: Option<String>,
    upstream_oid: String,
    request_id: Option<Uuid>,
}

fn verify_branch(branch: &str) -> app::Result<()> {
    branch
        .strip_prefix("refs/heads/")
        .and_then(|name| super::branches::branch_ref(name).ok())
        .filter(|name| name == branch)
        .map(drop)
        .ok_or(app::Error::Invalid("Select a branch to synchronize"))
}

async fn preview(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(input): Query<Preview>,
) -> app::Result<Json<Value>> {
    verify_branch(&input.branch)?;
    let fork = app::repository(&server, &principal, &key)?;
    let source = fork.fork_source(&server).ok_or(app::Error::NotFound)?;
    app::require_related_read(&server, &principal, &fork, &source).await?;
    let _permit =
        super::read_admission::acquire_bulk(&server).map_err(|_| app::Error::MergeBusy)?;
    let (fork_refs, upstream_refs) = tokio::try_join!(capture(&fork), capture(&source))?;
    let fork_oid = fork_refs.refs().get(&input.branch);
    let upstream_oid = upstream_refs.refs().get(&input.branch);
    let state = match (fork_oid, upstream_oid) {
        (None, None) => return Err(app::Error::NotFound),
        (Some(_), None) => "upstream_missing",
        (None, Some(_)) => "fork_missing",
        (Some(fork_oid), Some(upstream_oid)) if upstream_oid == fork_oid => "up_to_date",
        (Some(fork_oid), Some(upstream_oid)) => {
            if upstream_refs
                .git_visibility_index()
                .await
                .map_err(crate::Error::from)?
                .contains_hex_in_ref(&input.branch, fork_oid)
            {
                "behind"
            } else {
                if fork_refs
                    .git_visibility_index()
                    .await
                    .map_err(crate::Error::from)?
                    .contains_hex_in_ref(&input.branch, upstream_oid)
                {
                    "ahead"
                } else {
                    "diverged"
                }
            }
        }
    };
    app::require_related_read(&server, &principal, &fork, &source).await?;
    Ok(Json(
        json!({"state":state,"branch":input.branch,"fork_oid":fork_oid,"upstream_oid":upstream_oid}),
    ))
}

async fn capture(
    repo: &crate::repository::Repository,
) -> app::Result<crab_read::capsule_protocol::CapsuleRepositoryView> {
    use crab_read::capsule_protocol::{CapsuleReadLimits, open_git_view_from_root};
    let root = crab_metadata::capsule_protocol::load_root(&repo.layout)
        .await
        .map_err(|source| crate::Error::Settings {
            source: Box::new(source),
        })?;
    Ok(open_git_view_from_root(
        &repo.layout,
        root,
        CapsuleReadLimits {
            max_capsule_bytes: 2 * 1024 * 1024 * 1024,
            max_frontier_bytes: 2 * 1024 * 1024 * 1024,
        },
    )
    .await
    .map_err(crate::Error::from)?)
}

async fn update(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Json(input): Json<Update>,
) -> app::Result<Response> {
    verify_branch(&input.branch)?;
    let oid = |value: &str| {
        ObjectId::from_hex(value.as_bytes())
            .ok()
            .filter(|oid| oid.as_bytes().len() == 20 && !oid.is_null())
            .ok_or(app::Error::Invalid(
                "Review the current branch tips before syncing",
            ))
    };
    let fork = app::repository(&server, &principal, &key)?;
    let old = input.fork_oid.as_deref().map(oid).transpose()?;
    let new = oid(&input.upstream_oid)?;
    let request_id = match (old, input.request_id) {
        (None, Some(id)) if !id.is_nil() => Some(id),
        (Some(_), None) => None,
        _ => {
            return Err(app::Error::Invalid(
                "A branch copy requires a request ID and no fork tip",
            ));
        }
    };
    let update = RefUpdate {
        name: input.branch.clone(),
        old,
        new: Some(new),
    };
    let result = synchronize_fork(server, principal, fork, update, request_id).await;
    Ok(match result {
        Ok(()) => Json(json!({"branch":input.branch,"commit":new.to_string()})).into_response(),
        Err(error) => sync_error(error),
    })
}

fn sync_error(error: ReceiveError) -> Response {
    let (status, code, message) = match &error {
        ReceiveError::NotFound => return app::Error::NotFound.into_response(),
        ReceiveError::Archived => return app::Error::Archived.into_response(),
        ReceiveError::RefRule(message) => return app::Error::RefRule(message).into_response(),
        ReceiveError::Forbidden => (
            StatusCode::FORBIDDEN,
            "forbidden",
            "Write access is required to sync a fork",
        ),
        ReceiveError::Protected => (
            StatusCode::FORBIDDEN,
            "protected_branch",
            "This branch requires a pull request",
        ),
        ReceiveError::BranchChanged
        | ReceiveError::Graph(ReceivePlanError::Stale { .. })
        | ReceiveError::Write(crab_write::WriteError::RefChanged { .. }) => (
            StatusCode::CONFLICT,
            "branch_changed",
            "A branch changed. Review the current tips before syncing",
        ),
        ReceiveError::Graph(ReceivePlanError::NonFastForward { .. }) => (
            StatusCode::CONFLICT,
            "fork_diverged",
            "This branch has its own commits. Merge upstream changes in Git",
        ),
        ReceiveError::Graph(ReceivePlanError::Namespace(_))
        | ReceiveError::Write(crab_write::WriteError::Namespace(_)) => (
            StatusCode::CONFLICT,
            "branch_name_conflict",
            "The upstream branch name conflicts with a branch in this fork",
        ),
        ReceiveError::Busy
        | ReceiveError::Coordination(crab_coordination::CoordinationError::PushLockHeld {
            ..
        }) => (
            StatusCode::TOO_MANY_REQUESTS,
            "busy",
            "Git transfers are busy. Try again shortly",
        ),
        ReceiveError::Locked => (
            StatusCode::CONFLICT,
            "locked",
            "A changed file is locked by another user",
        ),
        _ => match error.staging_status() {
            Some((status, message)) => (status, "staging", message),
            None => {
                tracing::error!(?error, "fork synchronization failed");
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "fork_sync_failed",
                    "Could not sync the fork. Check the branch before retrying",
                )
            }
        },
    };
    (
        status,
        Json(json!({"error":{"code":code,"message":message}})),
    )
        .into_response()
}
