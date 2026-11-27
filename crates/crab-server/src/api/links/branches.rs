#![expect(
    clippy::result_large_err,
    reason = "handlers and their helpers return rejections as ready axum responses"
)]

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::{
    cells::{RepositoryCell, repository::collaboration::links::branches as native},
    repository::{Repository, development_branches::promotion, settings},
};
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use uuid::Uuid;

#[cfg(test)]
mod tests;

#[derive(Serialize)]
struct Address<'a> {
    id: Uuid,
    owner: &'a str,
    name: &'a str,
}

#[derive(Serialize)]
struct Item<'a> {
    repository: Address<'a>,
    reference: &'a str,
    exists: bool,
}

pub(super) async fn view(
    server: &Server,
    principal: &Principal,
    source: &Repository,
    cell: &RepositoryCell,
    issue: u64,
) -> Result<Value> {
    let view = app::query_output(
        cell.client
            .query::<native::Get>(&cell.target, None, issue)
            .await,
    )?
    .ok_or(Error::NotFound)?;
    let mut grouped = BTreeMap::<_, BTreeSet<_>>::new();
    for branch in view.state.branches {
        grouped
            .entry(branch.repository)
            .or_default()
            .insert(branch.reference);
    }
    let mut visible = Vec::new();
    for (id, references) in grouped {
        let Some(repo) = server.repositories.by_id(id) else {
            continue;
        };
        let Some(reader) = principal.related_read(source.id, id) else {
            continue;
        };
        if !can_read(server, &reader, &repo).await? {
            continue;
        }
        let current = repo.ref_oids(&references).await?;
        visible.push((repo, reader, references, current));
    }
    let mut items = Vec::new();
    for (repo, reader, references, current) in &visible {
        if !can_read(server, reader, repo).await? {
            continue;
        }
        for reference in references {
            items.push(Item {
                repository: Address {
                    id: repo.id,
                    owner: &repo.config.owner,
                    name: &repo.config.name,
                },
                reference,
                exists: current.contains_key(reference),
            });
        }
    }
    let grant = app::membership(server, principal, source).await?;
    let can_manage = grant.principal.can_write(source.id);
    let mut pending = None;
    if can_manage
        && let Some(request) = view.pending
        && app::account(&grant.principal).is_ok_and(|actor| actor == request.actor)
        && let Some(target) = server.repositories.by_id(request.branch.repository)
        && let Some(reader) = principal.related_read(source.id, target.id)
        && can_read(server, &reader, &target).await?
    {
        pending = Some(json!({
            "request_id": request.id,
            "repository": target.id,
            "name": request.branch.reference.trim_start_matches("refs/heads/"),
            "source_oid": request.commit,
        }));
    }
    Ok(json!({
        "items": items,
        "version": view.state.version,
        "can_manage": can_manage,
        "pending": pending,
    }))
}

async fn can_read(server: &Server, reader: &Principal, repo: &Repository) -> Result<bool> {
    match app::membership(server, reader, repo).await {
        Ok(_) => Ok(true),
        Err(Error::NotFound) => Ok(false),
        Err(error) => Err(error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Edit {
    branch: native::Branch,
    active: bool,
    version: u64,
}

pub(super) async fn set(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, issue)): Path<(String, String, u64)>,
    Json(input): Json<Edit>,
) -> Result<Json<Value>> {
    let source = app::repository(&server, &principal, &(owner, name))?;
    if !principal.can_write(source.id) {
        return Err(Error::LinkPermission);
    }
    app::actor(&principal)?;
    let issue = app::number(issue)?;
    if input.version > app::MAX_NUMBER {
        return Err(Error::Invalid("Invalid development branch version"));
    }
    input
        .branch
        .validate()
        .map_err(|_| Error::Invalid("Invalid development branch"))?;
    let target = server
        .repositories
        .by_id(input.branch.repository)
        .ok_or(Error::NotFound)?;
    let reader = principal
        .related_read(source.id, target.id)
        .ok_or(Error::NotFound)?;
    app::membership(&server, &reader, &target).await?;
    if input.active && target.ref_oid(&input.branch.reference).await?.is_none() {
        return Err(Error::NotFound);
    }
    let server = server.as_ref();
    let source = source.as_ref();
    let write = |cancel: tokio_util::sync::CancellationToken| async move {
        app::membership(server, &reader, &target).await?;
        if !app::membership(server, &principal, source)
            .await?
            .principal
            .can_write(source.id)
        {
            return Err(Error::LinkPermission);
        }
        if cancel.is_cancelled() {
            return Err(Error::MergeBusy);
        }
        let cell = app::repository_cell(server, source)?;
        let branch = input.branch;
        if input.active {
            promotion::track_branch(server, source, issue, &branch, true).await?;
        }
        let outcome = app::command_output(
            cell.client
                .command::<native::Set>(
                    &cell.target,
                    app::mutation_identity()?,
                    native::Edit {
                        issue,
                        branch: branch.clone(),
                        active: input.active,
                        version: input.version,
                    },
                )
                .await,
        )?;
        if !input.active && matches!(outcome, native::Decision::Applied(_)) {
            promotion::track_branch(server, source, issue, &branch, false).await?;
        }
        Ok(outcome)
    };
    let outcome = if source.organization.is_some() {
        settings::with_ownership_lease(server, source, write).await?
    } else {
        write(server.cancellation.clone()).await?
    };
    match outcome {
        native::Decision::Applied(version) => Ok(Json(json!({"version":version}))),
        native::Decision::Archived => Err(Error::Archived),
        native::Decision::Missing => Err(Error::NotFound),
        native::Decision::Conflict => Err(Error::Conflict),
        native::Decision::Limit => Err(Error::Invalid(
            "An issue can link up to 10 development branches",
        )),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NewBranch {
    request_id: Uuid,
    repository: Uuid,
    name: String,
    source_oid: String,
}

pub(super) async fn create(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, issue)): Path<(String, String, u64)>,
    Json(input): Json<NewBranch>,
) -> std::result::Result<(StatusCode, Json<Value>), Response> {
    use crate::{api::branches, git::receive::ReceiveError, repository::development_branches};
    let (source, target, writer, request) =
        admit_creation(&server, &principal, (owner, name), issue, input)
            .await
            .map_err(IntoResponse::into_response)?;
    let reference = request.branch.reference.clone();
    let commit = request.commit.clone();
    let status = development_branches::submit(server, writer, source, target, request)
        .await
        .map_err(|error| match error {
            ReceiveError::Settings(cause) => cause.into_response(),
            cause => branches::Error::from(cause).into_response(),
        })?;
    match status {
        native::creation::Status::Created => Ok((
            StatusCode::CREATED,
            Json(json!({
                "branch": reference, "commit": commit,
            })),
        )),
        native::creation::Status::Expired => Err(Error::Invalid(
            "This branch request ended. Reload the issue to start a new request.",
        )
        .into_response()),
        native::creation::Status::Pending => Err(Error::MergeBusy.into_response()),
    }
}

async fn admit_creation(
    server: &Server,
    principal: &Principal,
    key: (String, String),
    issue: u64,
    input: NewBranch,
) -> Result<(
    Arc<Repository>,
    Arc<Repository>,
    Principal,
    native::creation::Request,
)> {
    let source = app::repository(server, principal, &key)?;
    if !app::membership(server, principal, &source)
        .await?
        .principal
        .can_write(source.id)
    {
        return Err(Error::LinkPermission);
    }
    if input.request_id.is_nil() {
        return Err(Error::Invalid("Invalid development branch request ID"));
    }
    let target = server
        .repositories
        .by_id(input.repository)
        .ok_or(Error::NotFound)?;
    let reader = principal
        .related_read(source.id, target.id)
        .ok_or(Error::NotFound)?;
    let writer = app::membership(server, &reader, &target).await?.principal;
    if !writer.can_write(target.id) {
        return Err(Error::LinkPermission);
    }
    let reference = super::super::branches::branch_ref(&input.name).map_err(Error::Invalid)?;
    let mut request = native::creation::Request {
        id: input.request_id,
        issue: app::number(issue)?,
        branch: native::Branch {
            repository: target.id,
            reference,
        },
        commit: input.source_oid,
        actor: app::account(principal)?,
        target_epoch: target.ownership_epoch,
    };
    let oid = request
        .oid()
        .map_err(|_| Error::Invalid("Source must be a full SHA-1 commit ID"))?;
    request.commit = oid.to_string();
    Ok((source, target, writer, request))
}
