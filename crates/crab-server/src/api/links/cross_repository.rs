use super::*;
use crate::{
    cells::{
        Address,
        organization::development as graph,
        repository::{
            collaboration::{DevelopmentEndpoint, GetDevelopmentEndpoint},
            issues::transfer::publication::{Destination, GetLocation, Location},
        },
    },
    repository::{Repository, settings},
};
use std::collections::BTreeSet;
use uuid::Uuid;

#[cfg(test)]
mod tests;

type Key = (String, String, u64, Uuid, u64);

async fn endpoint(
    server: &Server,
    repo: &Repository,
    subject: Subject,
) -> Result<Option<DevelopmentEndpoint>> {
    let cell = app::repository_cell(server, repo)?;
    app::query_output(
        cell.client
            .query::<GetDevelopmentEndpoint>(cell.target, None, subject)
            .await,
    )
}

fn address(repo: &Repository, number: u64) -> Destination {
    Destination {
        repository: repo.id,
        number,
    }
}

async fn pair(
    server: &Server,
    source: &Repository,
    target: &Repository,
    issue: u64,
    pull: u64,
) -> Result<graph::Pair> {
    let (issue, pull) = tokio::try_join!(
        endpoint(
            server,
            source,
            Subject {
                kind: SubjectKind::Issue,
                number: issue
            }
        ),
        endpoint(
            server,
            target,
            Subject {
                kind: SubjectKind::Pull,
                number: pull
            }
        ),
    )?;
    let issue = issue.ok_or(Error::NotFound)?;
    let pull = pull.ok_or(Error::NotFound)?;
    Ok(graph::Pair {
        issue: issue
            .issue_origin
            .unwrap_or_else(|| address(source, issue.work.subject.number)),
        pull: address(target, pull.work.subject.number),
    })
}

fn repositories(
    server: &Server,
    principal: &Principal,
    key: (String, String),
    target: Uuid,
) -> Result<(Uuid, Arc<Repository>, Arc<Repository>, Principal)> {
    let source = app::repository(server, principal, &key)?;
    let organization = source.organization.ok_or(Error::NotFound)?;
    let target = server
        .repositories
        .by_id(target)
        .filter(|target| target.organization == Some(organization))
        .ok_or(Error::NotFound)?;
    let reader = principal
        .related_read(source.id, target.id)
        .ok_or(Error::NotFound)?;
    Ok((organization, source, target, reader))
}

pub(super) async fn detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, issue, target, pull)): Path<Key>,
) -> Result<Json<graph::State>> {
    let (organization, source, target, reader) =
        repositories(&server, &principal, (owner, name), target)?;
    app::membership(&server, &reader, &target).await?;
    let pair = pair(
        &server,
        &source,
        &target,
        app::number(issue)?,
        app::number(pull)?,
    )
    .await?;
    let cell = server.cells().ok_or(Error::CellUnavailable)?;
    let state = app::query_output(
        cell.query::<graph::Get>(Address::Organization(organization), None, pair)
            .await,
    )?;
    app::membership(&server, &principal, &source).await?;
    app::membership(&server, &reader, &target).await?;
    Ok(Json(state))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Selection {
    active: bool,
    version: u64,
}

pub(super) async fn set(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, issue, target, pull)): Path<Key>,
    Json(input): Json<Selection>,
) -> Result<Json<graph::State>> {
    let (organization, source, target, reader) =
        repositories(&server, &principal, (owner, name), target)?;
    if !principal.can_write(source.id) {
        return Err(Error::LinkPermission);
    }
    app::account(&principal)?;
    let issue = app::number(issue)?;
    let pull = app::number(pull)?;
    if input.version > app::MAX_NUMBER {
        return Err(Error::Invalid("Invalid linked work version"));
    }
    let server = server.as_ref();
    let source = source.as_ref();
    settings::with_ownership_lease(server, source, |cancel| async move {
        if !app::membership(server, &principal, source)
            .await?
            .principal
            .can_write(source.id)
        {
            return Err(Error::LinkPermission);
        }
        if settings::load_lifecycle(server, source).await?.archived {
            return Err(Error::Archived);
        }
        app::membership(server, &reader, &target).await?;
        let pair = pair(server, source, &target, issue, pull).await?;
        let cell = server.cells().ok_or(Error::CellUnavailable)?;
        let organization = Address::Organization(organization);
        if input.active
            && source.id == target.id
            && !app::query_output(cell.query::<graph::Get>(organization, None, pair).await)?.active
        {
            return Err(Error::Invalid(
                "Use the pull request sidebar to link work in this repository",
            ));
        }
        // Issue retirement and changes to either repository's owning organization
        // take the same lease. Recheck endpoint grants after their snapshot reads.
        if !app::membership(server, &principal, source)
            .await?
            .principal
            .can_write(source.id)
        {
            return Err(Error::LinkPermission);
        }
        app::membership(server, &reader, &target).await?;
        if cancel.is_cancelled() {
            return Err(Error::MergeBusy);
        }
        let outcome = app::command_output(
            cell.command::<graph::Set>(
                organization,
                app::mutation_identity()?,
                graph::Edit {
                    pair,
                    active: input.active,
                    version: input.version,
                },
            )
            .await,
        )?;
        match outcome {
            graph::Decision::Applied(state) => Ok(Json(state)),
            graph::Decision::Conflict => Err(Error::Conflict),
            graph::Decision::Limit => Err(Error::Invalid(
                "An issue or pull request can have up to 10 cross-repository links",
            )),
        }
    })
    .await
}

async fn current_issue(server: &Server, mut subject: Destination) -> Result<Option<Destination>> {
    let mut seen = BTreeSet::new();
    loop {
        if !seen.insert((subject.repository, subject.number)) {
            return Err(Error::CellContract("Issue transfer redirects form a cycle"));
        }
        let Some(repo) = server.repositories.by_id(subject.repository) else {
            return Ok(None);
        };
        let cell = app::repository_cell(server, &repo)?;
        let location = app::query_output(
            cell.client
                .query::<GetLocation>(cell.target, None, subject.number)
                .await,
        )?;
        match location {
            Location::Present => return Ok(Some(subject)),
            Location::Moved { destination } => subject = destination,
            Location::Missing => return Ok(None),
        }
    }
}

pub(super) async fn list(
    server: &Server,
    principal: &Principal,
    repository: &Repository,
    subject: Subject,
    organization: Uuid,
) -> Result<Vec<Value>> {
    let current = address(repository, subject.number);
    let (side, other_kind) = match subject.kind {
        SubjectKind::Issue => {
            let here = endpoint(server, repository, subject)
                .await?
                .ok_or(Error::NotFound)?;
            (
                graph::Side::Issue(here.issue_origin.unwrap_or(current)),
                SubjectKind::Pull,
            )
        }
        SubjectKind::Pull => (graph::Side::Pull(current), SubjectKind::Issue),
    };
    let cell = server.cells().ok_or(Error::CellUnavailable)?;
    let links = app::query_output(
        cell.query::<graph::List>(Address::Organization(organization), None, side)
            .await,
    )?;
    let mut items = Vec::new();
    for link in links {
        let other = match other_kind {
            SubjectKind::Pull => link.pair.pull,
            SubjectKind::Issue => {
                let Some(current) = current_issue(server, link.pair.issue).await? else {
                    continue;
                };
                current
            }
        };
        let Some(repo) = server
            .repositories
            .by_id(other.repository)
            .filter(|repo| repo.organization == Some(organization))
        else {
            continue;
        };
        let Some(reader) = principal.related_read(repository.id, repo.id) else {
            continue;
        };
        if membership(server, &reader, &repo).await?.is_none() {
            continue;
        }
        let Some(view) = endpoint(
            server,
            &repo,
            Subject {
                kind: other_kind,
                number: other.number,
            },
        )
        .await?
        else {
            continue;
        };
        if other_kind == SubjectKind::Issue && view.issue_origin.unwrap_or(other) != link.pair.issue
        {
            continue;
        }
        let can_unlink = subject.kind == SubjectKind::Pull
            && !settings::load_lifecycle(server, &repo).await?.archived;
        let Some(grant) = membership(server, &reader, &repo).await? else {
            continue;
        };
        items.push(json!({
            "work":view.work,
            "repository":{"id":repo.id,"owner":repo.config.owner,"name":repo.config.name},
            "version":link.version,
            "can_unlink":can_unlink && grant.principal.can_write(repo.id),
        }));
    }
    app::membership(server, principal, repository).await?;
    Ok(items)
}

async fn membership(
    server: &Server,
    principal: &Principal,
    repo: &Repository,
) -> Result<Option<app::Membership>> {
    match app::membership(server, principal, repo).await {
        Ok(grant) => Ok(Some(grant)),
        Err(Error::NotFound) => Ok(None),
        Err(error) => Err(error),
    }
}
