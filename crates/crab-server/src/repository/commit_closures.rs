use crab_metadata::capsule_protocol::resolve_capsule_plan_receipt;
use std::collections::{BTreeMap, BTreeSet};

use super::development_references;
use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::{
        Address,
        organization::development as graph,
        repository::{
            RepositoryAuthor,
            collaboration::{
                activity::ExternalCommit,
                links::{
                    commits::{
                        CapturedIssue, CloseCross, Cross, CrossClosure, DeliverCross, Next,
                        OpenStateVersion, Publication, PublishedDescription, Reservation, Reserve,
                        Resolution, Resolve,
                    },
                    keywords::Qualified,
                },
            },
            issues::transfer::publication::{Destination, GetLocation, Location},
        },
    },
    repository::Repository,
    server::Server,
};
use uuid::Uuid;

pub(crate) async fn current_issue(
    server: &Server,
    mut subject: Destination,
) -> Result<Option<Destination>> {
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

pub(crate) async fn capture(
    server: &Server,
    repo: &Repository,
    principal: &Principal,
    pull: Option<u64>,
    commits: BTreeMap<Qualified, String>,
) -> Result<Option<CrossClosure>> {
    let Some(organization) = repo.organization else {
        return Ok(None);
    };
    if pull.is_none() && commits.is_empty() {
        return Ok(None);
    }
    let (links, described) = if let Some(pull) = pull {
        let cell = server.cells().ok_or(Error::CellUnavailable)?;
        let links = app::query_output(
            cell.query::<graph::List>(
                Address::Organization(organization),
                None,
                graph::Side::Pull(Destination {
                    repository: repo.id,
                    number: pull,
                }),
            )
            .await,
        )?;
        let described = match development_references::snapshot(server, repo, pull).await? {
            Some(view) => {
                development_references::targets(server, repo, view.references, true).await?
            }
            None => Vec::new(),
        };
        (links, described)
    } else {
        (Vec::new(), Vec::new())
    };
    let commits = development_references::resolve(server, repo, commits).await?;
    let mut issues = Vec::new();
    let mut history = Vec::new();
    let mut seen = BTreeSet::new();
    for (origin, oid) in links
        .into_iter()
        .filter(|link| link.closes_on_merge)
        .map(|link| link.pair.issue)
        .chain(described.iter().copied())
        .map(|origin| (origin, None))
        .chain(commits.into_iter().map(|(origin, oid)| (origin, Some(oid))))
    {
        if !seen.insert((origin.repository, origin.number)) {
            continue;
        }
        let Some(target) = current_issue(server, origin).await? else {
            continue;
        };
        let Some(issue_repository) = server
            .repositories
            .by_id(target.repository)
            .filter(|target| target.organization == Some(organization))
        else {
            continue;
        };
        if described.contains(&origin) {
            history.push(origin);
        }
        let Some(writer) = principal.related_issue_writer(repo.id, target.repository) else {
            continue;
        };
        match app::membership(server, &writer, &issue_repository).await {
            Ok(grant) if grant.principal.can_write(target.repository) => {}
            Ok(_) | Err(Error::NotFound) => continue,
            Err(error) => return Err(error),
        }
        let issue = app::repository_cell(server, &issue_repository)?;
        let Some(state_version) = app::query_output(
            issue
                .client
                .query::<OpenStateVersion>(issue.target, None, target.number)
                .await,
        )?
        else {
            continue;
        };
        issues.push(CapturedIssue {
            origin,
            state_version,
            external_commit: oid.map(|oid| ExternalCommit {
                repository: repo.id,
                oid,
            }),
        });
    }
    let description = pull
        .filter(|_| !history.is_empty())
        .map(|pull| PublishedDescription {
            pull,
            issues: history,
        });
    Ok(
        (!issues.is_empty() || description.is_some()).then_some(CrossClosure {
            organization,
            issues,
            description,
        }),
    )
}

pub(crate) async fn reserve(
    server: &Server,
    repo: &Repository,
    reservation: Reservation,
) -> Result<bool> {
    let cell = app::repository_cell(server, repo)?;
    app::command_output(
        cell.client
            .command::<Reserve>(cell.target, app::mutation_identity()?, reservation)
            .await,
    )
}

async fn reconcile(
    server: &Server,
    repo: &Repository,
    publication: Publication,
    due: Option<u64>,
) -> Result<()> {
    let receipt = resolve_capsule_plan_receipt(&repo.store, &repo.layout, &publication.plan)
        .await
        .map_err(|source| crate::Error::Settings {
            source: Box::new(source),
        })?;
    let cell = app::repository_cell(server, repo)?;
    let mut described_pull = None;
    let resolution = if let Some(receipt) = receipt {
        if !receipt.transaction().edits().iter().any(|edit| {
            edit.ref_name() == publication.reference
                && edit.expected_old() == publication.before.as_deref()
                && edit.new_oid() == Some(publication.after.as_str())
        }) {
            return Err(app::Error::CellContract(
                "issue closure receipt has a different ref edit",
            ));
        }
        if let Some((cross, actor)) = app::query_output(
            cell.client
                .query::<Cross>(cell.target, None, publication.plan.clone())
                .await,
        )? {
            described_pull = cross
                .description
                .as_ref()
                .map(|description| description.pull);
            if due.is_some() {
                // Live publication already holds this policy lease. A worker
                // recovering an older job must also fence ownership changes.
                super::settings::with_ownership_lease(server, repo, |cancel| async move {
                    if cancel.is_cancelled() {
                        return Err(Error::MergeBusy);
                    }
                    deliver_cross(server, repo, cross, actor).await
                })
                .await?;
            } else {
                deliver_cross(server, repo, cross, actor).await?;
            }
        }
        Resolution::Committed(publication.plan)
    } else if let Some(due) = due {
        Resolution::Uncommitted {
            plan: publication.plan,
            due,
        }
    } else {
        return Ok(());
    };
    app::command_output(
        cell.client
            .command::<Resolve>(cell.target, app::mutation_identity()?, resolution)
            .await,
    )?;
    if let Some(pull) = described_pull {
        development_references::refresh(server, repo, pull).await;
    }
    Ok(())
}

async fn current_scope(server: &Server, repo: &Repository, organization: Uuid) -> Result<bool> {
    if repo.organization != Some(organization) {
        return Ok(false);
    }
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    match catalog
        .membership(&repo.config.owner, &repo.config.name)
        .await
    {
        Ok(current) => Ok(current.repository_id == repo.id
            && current.ownership_epoch == repo.ownership_epoch
            && current
                .organization
                .is_some_and(|value| value.id == organization)),
        Err(crate::catalog::CatalogError::NotFound) => Ok(false),
        Err(error) => Err(Error::Membership(Box::new(error))),
    }
}

async fn deliver_cross(
    server: &Server,
    repo: &Repository,
    cross: CrossClosure,
    actor: RepositoryAuthor,
) -> Result<()> {
    if !current_scope(server, repo, cross.organization).await? {
        return Ok(());
    }
    for captured in cross.issues {
        // Retirement can precede destination publication. Hold the job until
        // the canonical issue is present, using the immutable transfer chain.
        let target = current_issue(server, captured.origin)
            .await?
            .ok_or(Error::MergeBusy)?;
        let repository = server
            .repositories
            .by_id(target.repository)
            .ok_or(Error::MergeBusy)?;
        if !current_scope(server, &repository, cross.organization).await? {
            continue;
        }
        let cell = app::repository_cell(server, &repository)?;
        if !app::command_output(
            cell.client
                .command::<CloseCross>(
                    cell.target,
                    app::mutation_identity()?,
                    DeliverCross {
                        target,
                        captured,
                        actor: actor.clone(),
                    },
                )
                .await,
        )? {
            return Err(Error::MergeBusy);
        }
    }
    Ok(())
}

pub(crate) async fn finish(server: &Server, repo: &Repository, publication: Publication) {
    if let Err(error) = reconcile(server, repo, publication, None).await {
        tracing::warn!(repository = %repo.id, %error, "issue closure will retry");
    }
}

pub(crate) async fn recover(server: &Server, repo: &Repository) -> Result<()> {
    let cell = app::repository_cell(server, repo)?;
    for _ in 0..8 {
        if server.cancellation.is_cancelled() {
            break;
        }
        let Some((due, publication)) =
            app::query_output(cell.client.query::<Next>(cell.target, None, ()).await)?
        else {
            break;
        };
        reconcile(server, repo, publication, Some(due)).await?;
    }
    Ok(())
}
