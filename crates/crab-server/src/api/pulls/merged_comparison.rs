use axum::{
    Extension, Router,
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    routing::get,
};
use crab_remote_git::{
    Commit, GitPath, OperationContext, OperationKind, Page, PageRequest, RemoteGitSnapshot,
};
use gix_hash::ObjectId;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use super::{git::PullGit, statuses::parse_oid, storage};
use crate::{
    api::{
        ApiError, decode_hex, display_path, encode_hex, query, read_admission,
        representation::{commit_json, content_json, decode_cursor, encode_cursor},
    },
    app::{self, Error, Result},
    auth::Principal,
    repository::Repository,
    server::Server,
};

type Address = (String, String, u64);
type CommitAddress = (String, String, u64, String);
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
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/merged-comparison/commits",
            get(commits),
        )
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/merged-comparison/commits/{oid}",
            get(commit),
        )
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/merged-comparison/commits/{oid}/file",
            get(commit_file),
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct History {
    cursor: Option<String>,
    limit: Option<usize>,
}

enum Read {
    Comparison(Option<GitPath>),
    History {
        limit: usize,
        cursor: Option<String>,
    },
    Commit(ObjectId, Option<GitPath>),
}

async fn changes(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(address): Path<Address>,
    Query(_): Query<NoParameters>,
) -> Response {
    read(server, principal, address, Read::Comparison(None)).await
}

async fn file(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(address): Path<Address>,
    Query(input): Query<File>,
) -> Response {
    match file_path(&input.path_hex) {
        Ok(path) => read(server, principal, address, Read::Comparison(Some(path))).await,
        Err(error) => error.into_response(),
    }
}

async fn commits(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(address): Path<Address>,
    Query(input): Query<History>,
) -> Response {
    let limit = input.limit.unwrap_or(50);
    if !(1..=200).contains(&limit) {
        return ApiError::Input("limit must be between 1 and 200").into_response();
    }
    read(
        server,
        principal,
        address,
        Read::History {
            limit,
            cursor: input.cursor,
        },
    )
    .await
}

async fn commit(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number, oid)): Path<CommitAddress>,
    Query(_): Query<NoParameters>,
) -> Response {
    match selected_oid(&oid) {
        Ok(oid) => {
            read(
                server,
                principal,
                (owner, name, number),
                Read::Commit(oid, None),
            )
            .await
        }
        Err(error) => error.into_response(),
    }
}

async fn commit_file(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number, oid)): Path<CommitAddress>,
    Query(input): Query<File>,
) -> Response {
    let oid = match selected_oid(&oid) {
        Ok(oid) => oid,
        Err(error) => return error.into_response(),
    };
    match file_path(&input.path_hex) {
        Ok(path) => {
            read(
                server,
                principal,
                (owner, name, number),
                Read::Commit(oid, Some(path)),
            )
            .await
        }
        Err(error) => error.into_response(),
    }
}

fn selected_oid(value: &str) -> std::result::Result<ObjectId, ApiError> {
    parse_oid(value).map_err(|_| ApiError::Input("Choose an exact Git commit ID"))
}

fn file_path(hex: &str) -> std::result::Result<GitPath, ApiError> {
    if hex.is_empty() || hex.len() > 8192 {
        return Err(ApiError::Input("Choose a changed file path"));
    }
    Ok(GitPath::new(decode_hex(hex)?)?)
}

async fn read(
    server: Arc<Server>,
    principal: Principal,
    (owner, name, number): Address,
    request: Read,
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
        let value = match request {
            Read::History { limit, cursor } => {
                let source_id = capture
                    .fork
                    .as_ref()
                    .map_or(repo.id, |(source, _)| source.id);
                let cursor = cursor
                    .as_deref()
                    .map(|cursor| decode_cursor(&server.cursor_key, source_id, cursor))
                    .transpose()
                    .map_err(IntoResponse::into_response)?;
                let page = PageRequest::new(limit, cursor).map_err(remote_response)?;
                let history = capture
                    .history(&page, &cancel)
                    .await
                    .map_err(remote_response)?;
                let items = history.items.iter().map(commit_json).collect::<Vec<_>>();
                let next = history
                    .next
                    .map(|next| encode_cursor(&server.cursor_key, source_id, next));
                json!({"items": items, "next": next})
            }
            Read::Comparison(path) => capture
                .view(None, path, &cancel)
                .await
                .map_err(remote_response)?,
            Read::Commit(oid, path) => capture
                .view(Some(oid), path, &cancel)
                .await
                .map_err(remote_response)?,
        };
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

pub(super) fn remote_response(error: crab_remote_git::Error) -> Response {
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

pub(super) struct Capture {
    git: PullGit,
    fork: Option<(Arc<Repository>, Principal)>,
    base: ObjectId,
    head: ObjectId,
}

pub(super) struct Operations {
    pub(super) head: OperationContext,
    fork_base: Option<OperationContext>,
}

impl Operations {
    pub(super) fn base(&self) -> &OperationContext {
        self.fork_base.as_ref().unwrap_or(&self.head)
    }

    pub(super) async fn finish<T>(
        self,
        result: crab_remote_git::Result<T>,
    ) -> crab_remote_git::Result<T> {
        let result = match self.fork_base {
            Some(base) => base.finish(result).await,
            None => result,
        };
        self.head.finish(result).await
    }
}

impl Capture {
    pub(super) async fn open(
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

    pub(super) async fn recheck(
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

    pub(super) async fn operations(
        &self,
        kind: OperationKind,
        cancel: &CancellationToken,
    ) -> crab_remote_git::Result<Operations> {
        let head = self.git.head().operation(kind, cancel).await?;
        let fork_base = if self.fork.is_some() {
            match self.git.base.operation(kind, cancel).await {
                Ok(base) => Some(base),
                Err(error) => return head.finish(Err(error)).await,
            }
        } else {
            None
        };
        Ok(Operations { head, fork_base })
    }

    pub(super) async fn base_snapshot(
        &self,
        ops: &Operations,
    ) -> crab_remote_git::Result<RemoteGitSnapshot> {
        self.git.base.snapshot_retained(self.base, ops.base()).await
    }

    pub(super) async fn tips(
        &self,
        ops: &Operations,
    ) -> crab_remote_git::Result<(RemoteGitSnapshot, RemoteGitSnapshot)> {
        let base = self.base_snapshot(ops).await?;
        let head = self
            .git
            .head()
            .snapshot_retained(self.head, &ops.head)
            .await?;
        Ok((base, head))
    }

    pub(super) async fn ancestor(
        &self,
        base: RemoteGitSnapshot,
        head: &RemoteGitSnapshot,
        ops: &Operations,
    ) -> crab_remote_git::Result<RemoteGitSnapshot> {
        let ancestor = query::comparison_ancestor(head, &base, &ops.head, ops.base()).await?;
        if ops.fork_base.is_none() && ancestor == base.commit_oid() {
            return Ok(base);
        }
        self.git.head().snapshot_retained(ancestor, &ops.head).await
    }

    async fn history(
        &self,
        page: &PageRequest,
        cancel: &CancellationToken,
    ) -> crab_remote_git::Result<Page<Commit>> {
        let ops = self.operations(OperationKind::History, cancel).await?;
        let result = async {
            let (base, head) = self.tips(&ops).await?;
            head.ahead_history_across_captures(&base, page, &ops.head, ops.base())
                .await
        }
        .await;
        ops.finish(result).await
    }

    async fn view(
        &self,
        selected: Option<ObjectId>,
        path: Option<GitPath>,
        cancel: &CancellationToken,
    ) -> crab_remote_git::Result<Value> {
        let kind = if path.is_some() {
            OperationKind::Diff
        } else {
            OperationKind::Compare
        };
        let ops = self.operations(kind, cancel).await?;
        let value = async {
            let (base, head) = self.tips(&ops).await?;
            let (base, head, metadata) = if let Some(oid) = selected {
                let commit = head
                    .ahead_commit_across_captures(&base, oid, &ops.head, ops.base())
                    .await?
                    .ok_or(crab_remote_git::Error::PathNotFound)?;
                // Only the range walk may authorize a user-supplied commit.
                // Its verified first parent is the base of this one-commit diff.
                let source = self.git.head();
                let base = match commit.parents.first() {
                    Some(parent) => Some(source.snapshot_retained(*parent, &ops.head).await?),
                    None => None,
                };
                let view = source.snapshot_retained(commit.oid, &ops.head).await?;
                (base, view, Some(commit))
            } else {
                let base = self.ancestor(base, &head, &ops).await?;
                (Some(base), head, None)
            };
            if let Some(path) = path {
                let blobs =
                    query::comparison_blobs_for_snapshots(base.as_ref(), &head, &path, &ops.head)
                        .await?;
                if !blobs.changed || blobs.old.is_none() && blobs.new.is_none() {
                    return Err(crab_remote_git::Error::PathNotFound);
                }
                return Ok(json!({
                    "base": base.as_ref().map(|base| base.commit_oid().to_string()),
                    "commit": head.commit_oid().to_string(),
                    "path": display_path(path.as_bytes()),
                    "path_hex": encode_hex(path.as_bytes()),
                    "old": blobs.old.as_ref().map(content_json),
                    "new": blobs.new.as_ref().map(content_json),
                }));
            }
            let base_tip = selected.is_none().then(|| self.base.to_string());
            let mut value =
                query::comparison_changes(&head, base.as_ref(), base_tip, &ops.head).await?;
            if let Some(commit) = metadata {
                value["metadata"] = commit_json(&commit);
            }
            value["commit"] = json!(head.commit_oid().to_string());
            value["generation"] = json!(self.git.head().generation());
            Ok(value)
        }
        .await;
        ops.finish(value).await
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
    let ops = capture
        .operations(OperationKind::Compare, &cancel)
        .await
        .map_err(crate::Error::from)?;
    let snapshots = async {
        let (base, head) = capture.tips(&ops).await?;
        let base = capture.ancestor(base, &head, &ops).await?;
        Ok((base, head))
    }
    .await;
    let (base, head) = ops.finish(snapshots).await.map_err(crate::Error::from)?;
    capture.recheck(server, principal, repo).await?;
    Ok((base.commit_oid().to_string(), head.commit_oid().to_string()))
}
