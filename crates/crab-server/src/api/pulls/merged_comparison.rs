use axum::{
    Extension, Router,
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    routing::get,
};
use crab_remote_git::{GitPath, OperationContext, OperationKind, RemoteGitSnapshot};
use gix_hash::ObjectId;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use super::{git::PullGit, statuses::parse_oid, storage};
use crate::{
    api::{
        ApiError, decode_hex, display_path, encode_hex, query, read_admission,
        representation::content_json,
    },
    app::{self, Error, Result},
    auth::Principal,
    repository::Repository,
    server::Server,
};

type Address = (String, String, u64);
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/merged-comparison",
            get(changes),
        )
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/merged-comparison/file",
            get(file),
        )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    path_hex: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoParameters {}

async fn changes(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(address): Path<Address>,
    Query(_): Query<NoParameters>,
) -> Response {
    read(server, principal, address, None).await
}

async fn file(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(address): Path<Address>,
    Query(input): Query<File>,
) -> Response {
    if input.path_hex.is_empty() || input.path_hex.len() > 8192 {
        return ApiError::Input("Choose a changed file path").into_response();
    }
    let path =
        decode_hex(&input.path_hex).and_then(|bytes| GitPath::new(bytes).map_err(ApiError::from));
    match path {
        Ok(path) => read(server, principal, address, Some(path)).await,
        Err(error) => error.into_response(),
    }
}

async fn read(
    server: Arc<Server>,
    principal: Principal,
    (owner, name, number): Address,
    path: Option<GitPath>,
) -> Response {
    let Ok(admission) = read_admission::acquire_bulk(&server) else {
        return read_admission::busy();
    };
    let cancel = server.cancellation.child_token();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let result = async {
        let repo = app::repository(&server, &principal, &(owner, name))
            .map_err(IntoResponse::into_response)?;
        let number = app::number(number).map_err(IntoResponse::into_response)?;
        let pull = storage::pull(&server, &repo, number)
            .await
            .map_err(IntoResponse::into_response)?
            .ok_or_else(|| Error::NotFound.into_response())?;
        let capture = Capture::open(&server, &principal, &repo, &pull, &cancel)
            .await
            .map_err(IntoResponse::into_response)?;
        let value = capture.view(path, &cancel).await.map_err(remote_response)?;
        capture
            .recheck(&server, &principal, &repo)
            .await
            .map_err(IntoResponse::into_response)?;
        let bytes =
            serde_json::to_vec(&value).map_err(|error| ApiError::Json(error).into_response())?;
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Err(remote_response(crab_remote_git::Error::LimitExceeded {
                limit: "HTTP response bytes",
                actual: bytes.len() as u64,
                maximum: MAX_RESPONSE_BYTES as u64,
            }));
        }
        Ok(([("content-type", "application/json")], bytes).into_response())
    }
    .await;
    read_admission::retain(result.unwrap_or_else(|response| response), admission)
}

fn remote_response(error: crab_remote_git::Error) -> Response {
    if matches!(
        crate::api::revalidation::shared_cause(&error),
        crab_remote_git::Error::SnapshotUnavailable | crab_remote_git::Error::ObjectNotFound { .. }
    ) {
        return (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(json!({"error":{"code":"comparison_unavailable","message":"The Git objects for this saved comparison are no longer available."}})),
        ).into_response();
    }
    ApiError::Remote(error).into_response()
}

struct Capture {
    git: PullGit,
    fork: Option<(Arc<Repository>, Principal)>,
    base: ObjectId,
    head: ObjectId,
}

impl Capture {
    async fn open(
        server: &Server,
        principal: &Principal,
        repo: &Repository,
        pull: &storage::PullRequest,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let merge = pull.merge.as_ref().ok_or(Error::NotFound)?;
        let fork = pull
            .head_repository
            .map(|id| super::git::fork_repository(server, repo, id))
            .transpose()?;
        app::membership(server, principal, repo).await?;
        let fork = if let Some(fork) = fork {
            // The request grant is scoped to the upstream. Resolve the fork's
            // grant from the same session after verifying its relationship.
            let reader = principal
                .related_read(repo.id, fork.id)
                .ok_or(Error::NotFound)?;
            app::membership(server, &reader, &fork).await?;
            Some((fork, reader))
        } else {
            None
        };
        let base_git = repo.open_current(server, server.options, cancel);
        let (base_git, head_git) = if let Some((source, _)) = &fork {
            let (base, head) = tokio::try_join!(
                base_git,
                source.open_current(server, server.options, cancel)
            )?;
            (base, Some(head))
        } else {
            (base_git.await?, None)
        };
        // Only the completed merge owns the retained roots. A supplied
        // comparison SHA or a locator hit is never a read authority.
        Ok(Self {
            git: PullGit {
                base: base_git,
                fork: head_git,
            },
            fork,
            base: parse_oid(&merge.base_oid)?,
            head: parse_oid(&merge.head_oid)?,
        })
    }

    async fn recheck(
        &self,
        server: &Server,
        principal: &Principal,
        repo: &Repository,
    ) -> Result<()> {
        app::membership(server, principal, repo).await?;
        if let Some((fork, reader)) = &self.fork {
            let current = super::git::fork_repository(server, repo, fork.id)?;
            if current.ownership_epoch != fork.ownership_epoch
                || current.identity.placement_generation() != fork.identity.placement_generation()
            {
                return Err(Error::NotFound);
            }
            app::membership(server, reader, &current).await?;
        }
        Ok(())
    }

    async fn snapshots(
        &self,
        operation: &OperationContext,
        cancel: &CancellationToken,
    ) -> crab_remote_git::Result<(RemoteGitSnapshot, RemoteGitSnapshot)> {
        let source = self.git.head();
        let head = source.snapshot_retained(self.head, operation).await?;
        let ancestor = if self.fork.is_some() {
            let base_operation = self
                .git
                .base
                .operation(OperationKind::Compare, cancel)
                .await?;
            let result = async {
                let base = self
                    .git
                    .base
                    .snapshot_retained(self.base, &base_operation)
                    .await?;
                query::comparison_ancestor(&head, &base, operation, &base_operation).await
            }
            .await;
            base_operation.finish(result).await?
        } else {
            let base = source.snapshot_retained(self.base, operation).await?;
            let ancestor = query::comparison_ancestor(&head, &base, operation, operation).await?;
            if ancestor == base.commit_oid() {
                return Ok((base, head));
            }
            ancestor
        };
        let base = source.snapshot_retained(ancestor, operation).await?;
        Ok((base, head))
    }

    async fn view(
        &self,
        path: Option<GitPath>,
        cancel: &CancellationToken,
    ) -> crab_remote_git::Result<Value> {
        let kind = if path.is_some() {
            OperationKind::Diff
        } else {
            OperationKind::Compare
        };
        let operation = self.git.head().operation(kind, cancel).await?;
        let value = async {
            let (base, head) = self.snapshots(&operation, cancel).await?;
            if let Some(path) = path {
                let blobs =
                    query::comparison_blobs_for_snapshots(Some(&base), &head, &path, &operation)
                        .await?;
                if !blobs.changed || blobs.old.is_none() && blobs.new.is_none() {
                    return Err(crab_remote_git::Error::PathNotFound);
                }
                return Ok(json!({
                    "base": base.commit_oid().to_string(),
                    "path": display_path(path.as_bytes()),
                    "path_hex": encode_hex(path.as_bytes()),
                    "old": blobs.old.as_ref().map(content_json),
                    "new": blobs.new.as_ref().map(content_json),
                }));
            }
            let changes = head.compare(&base, &operation).await?;
            let mut value = query::comparison_json(changes, Some(self.base.to_string()));
            value["commit"] = json!(self.head.to_string());
            value["generation"] = json!(self.git.head().generation());
            Ok(value)
        }
        .await;
        operation.finish(value).await
    }
}

pub(super) async fn anchors(
    server: &Server,
    principal: &Principal,
    repo: &Repository,
    pull: &storage::PullRequest,
) -> Result<(String, String)> {
    let cancel = server.cancellation.child_token();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let capture = Capture::open(server, principal, repo, pull, &cancel).await?;
    let operation = capture
        .git
        .head()
        .operation(OperationKind::Compare, &cancel)
        .await
        .map_err(crate::Error::from)?;
    let snapshots = capture.snapshots(&operation, &cancel).await;
    let (base, head) = operation
        .finish(snapshots)
        .await
        .map_err(crate::Error::from)?;
    capture.recheck(server, principal, repo).await?;
    Ok((base.commit_oid().to_string(), head.commit_oid().to_string()))
}
