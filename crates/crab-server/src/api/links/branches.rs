use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::{
    cells::{RepositoryCell, repository::collaboration::links::branches as native},
    repository::Repository,
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
    let state = app::query_output(
        cell.client
            .query::<native::Get>(&cell.target, None, issue)
            .await,
    )?
    .ok_or(Error::NotFound)?;
    let mut grouped = BTreeMap::<_, BTreeSet<_>>::new();
    for branch in state.branches {
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
    Ok(json!({
        "items": items,
        "version": state.version,
        "can_manage": grant.principal.can_write(source.id),
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
    app::membership(&server, &reader, &target).await?;
    if !app::membership(&server, &principal, &source)
        .await?
        .principal
        .can_write(source.id)
    {
        return Err(Error::LinkPermission);
    }
    let cell = app::repository_cell(&server, &source)?;
    let outcome = app::command_output(
        cell.client
            .command::<native::Set>(
                &cell.target,
                app::mutation_identity()?,
                native::Edit {
                    issue,
                    branch: input.branch,
                    active: input.active,
                    version: input.version,
                },
            )
            .await,
    )?;
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
