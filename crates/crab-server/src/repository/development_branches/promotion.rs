use std::sync::Arc;

use uuid::Uuid;

use super::{IssueBranchPublication, Repository, finish_committed};
use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::{
        Address,
        organization::development::{Pair, branches as index},
        repository::{
            collaboration::{
                GetDevelopmentEndpoint, Subject, SubjectKind,
                links::branches::{self, Branch, promotion::cross},
            },
            issues::transfer::publication::Destination,
        },
    },
    repository::{
        commit_closures::{current_issue, current_scope},
        settings,
    },
    server::Server,
};

async fn track(
    server: &Server,
    organization: Uuid,
    rows: Vec<(Destination, Branch)>,
    active: bool,
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let cell = server.cells().ok_or(Error::CellUnavailable)?;
    app::command_output(
        cell.command::<index::Track>(
            Address::Organization(organization),
            app::mutation_identity()?,
            index::TrackInput { rows, active },
        )
        .await,
    )
}

pub(crate) async fn track_branch(
    server: &Server,
    repo: &Repository,
    issue: u64,
    branch: &Branch,
    active: bool,
) -> Result<()> {
    let Some(organization) = repo.organization else {
        return Ok(());
    };
    let cell = app::repository_cell(server, repo)?;
    if !active
        && let Some(view) = app::query_output(
            cell.client
                .query::<branches::Get>(cell.target, None, issue)
                .await,
        )?
        && (view.state.branches.contains(branch)
            || view.pending.is_some_and(|job| job.branch == *branch))
    {
        return Ok(());
    }
    let subject = Subject {
        kind: SubjectKind::Issue,
        number: issue,
    };
    let view = app::query_output(
        cell.client
            .query::<GetDevelopmentEndpoint>(cell.target, None, subject)
            .await,
    )?
    .ok_or(Error::NotFound)?;
    let origin = view.issue_origin.unwrap_or(Destination {
        repository: repo.id,
        number: issue,
    });
    track(server, organization, vec![(origin, branch.clone())], active).await
}

async fn candidate(
    server: &Server,
    repo: &Repository,
    principal: &Principal,
    organization: Uuid,
    branch: &Branch,
    issue: Destination,
) -> Result<Option<cross::Candidate>> {
    let Some(current) = current_issue(server, issue).await? else {
        return Ok(None);
    };
    let Some(source) = server
        .repositories
        .by_id(current.repository)
        .filter(|source| source.id != repo.id && source.organization == Some(organization))
    else {
        return Ok(None);
    };
    let Some(reader) = principal.related_read(repo.id, source.id) else {
        return Ok(None);
    };
    match app::membership(server, &reader, &source).await {
        Ok(_) => {}
        Err(Error::NotFound) => return Ok(None),
        Err(error) => return Err(error),
    }
    if settings::load_lifecycle(server, &source).await?.archived {
        return Ok(None);
    }
    let cell = app::repository_cell(server, &source)?;
    let read = || {
        cell.client
            .query::<branches::Get>(cell.target, None, current.number)
    };
    let Some(mut view) = app::query_output(read().await)? else {
        return Ok(None);
    };
    if let Some(request) = view
        .pending
        .as_ref()
        .filter(|pending| pending.branch == *branch)
        .cloned()
        && let Some(target) = server.repositories.by_id(branch.repository)
    {
        let publication = IssueBranchPublication {
            source: Arc::clone(&source),
            request,
        };
        // The branch can be visible in Git while its creation reply is lost.
        // Only the exact reservation receipt can make its issue link eligible.
        finish_committed(server, &target, &publication)
            .await
            .map_err(|error| Error::PullBranch(Box::new(error)))?;
        let Some(settled) = app::query_output(read().await)? else {
            return Ok(None);
        };
        view = settled;
    }
    if !view.state.branches.contains(branch) {
        if view.pending.is_none_or(|job| job.branch != *branch) {
            track(server, organization, vec![(issue, branch.clone())], false).await?;
        }
        return Ok(None);
    }
    Ok(Some(cross::Candidate {
        issue,
        version: view.state.version,
    }))
}

pub(crate) async fn capture(
    server: &Server,
    repo: &Repository,
    principal: &Principal,
    branch: Branch,
) -> Result<Option<cross::Batch>> {
    let Some(organization) = repo.organization else {
        return Ok(None);
    };
    let cell = server.cells().ok_or(Error::CellUnavailable)?;
    let mut after = None;
    let mut issues = Vec::new();
    while issues.len() < cross::LIMIT {
        let (page, more) = app::query_output(
            cell.query::<index::Candidates>(
                Address::Organization(organization),
                None,
                (branch.clone(), after),
            )
            .await,
        )?;
        after = page.last().copied();
        for issue in page {
            if let Some(issue) =
                candidate(server, repo, principal, organization, &branch, issue).await?
            {
                issues.push(issue);
                if issues.len() == cross::LIMIT {
                    break;
                }
            }
        }
        if !more || after.is_none() {
            break;
        }
    }
    Ok((!issues.is_empty()).then_some(cross::Batch {
        organization,
        branch,
        issues,
    }))
}

pub(crate) async fn finish(server: &Server, repo: &Repository, pull: u64) -> Result<()> {
    if repo.organization.is_none() {
        return Ok(());
    }
    let cell = app::repository_cell(server, repo)?;
    let Some(batch) = app::query_output(
        cell.client
            .query::<cross::Get>(cell.target, None, pull)
            .await,
    )?
    else {
        return Ok(());
    };
    if current_scope(server, repo, batch.organization).await? {
        let organization = Address::Organization(batch.organization);
        let graph = server.cells().ok_or(Error::CellUnavailable)?;
        for candidate in batch.issues {
            let Some(current) = current_issue(server, candidate.issue).await? else {
                // Issue transfer can retire the old record before publishing the new one.
                return Err(Error::MergeBusy);
            };
            let Some(source) = server.repositories.by_id(current.repository) else {
                continue;
            };
            if !current_scope(server, &source, batch.organization).await?
                || settings::load_lifecycle(server, &source).await?.archived
            {
                continue;
            }
            let cell = app::repository_cell(server, &source)?;
            let view = app::query_output(
                cell.client
                    .query::<branches::Get>(cell.target, None, current.number)
                    .await,
            )?;
            if !view.is_some_and(|view| {
                view.state.version == candidate.version
                    && view.state.branches.contains(&batch.branch)
            }) {
                continue;
            }
            let pair = Pair {
                issue: candidate.issue,
                pull: Destination {
                    repository: repo.id,
                    number: pull,
                },
            };
            if !app::command_output(
                graph
                    .command::<index::Promote>(organization, app::mutation_identity()?, pair)
                    .await,
            )? {
                continue;
            }
            let decision = app::command_output(
                cell.client
                    .command::<branches::Set>(
                        cell.target,
                        app::mutation_identity()?,
                        branches::Edit {
                            issue: current.number,
                            branch: batch.branch.clone(),
                            active: false,
                            version: candidate.version,
                        },
                    )
                    .await,
            )?;
            if !matches!(decision, branches::Decision::Applied(_)) {
                return Err(Error::MergeBusy);
            }
            track(
                server,
                batch.organization,
                vec![(candidate.issue, batch.branch.clone())],
                false,
            )
            .await?;
        }
    }
    app::command_output(
        cell.client
            .command::<cross::Complete>(cell.target, app::mutation_identity()?, pull)
            .await,
    )
}

pub(crate) async fn refresh(server: &Server, repo: &Repository, pull: u64) {
    if let Err(error) = finish(server, repo, pull).await {
        tracing::warn!(repository = %repo.id, pull, %error, "development branch promotion will retry");
    }
}

pub(crate) async fn recover(server: &Server, repo: &Repository) -> Result<()> {
    let Some(organization) = repo.organization else {
        return Ok(());
    };
    let cell = app::repository_cell(server, repo)?;
    let page = app::query_output(
        cell.client
            .query::<cross::Backfill>(cell.target, None, organization)
            .await,
    )?;
    let pending = app::query_output(
        cell.client
            .query::<cross::Pending>(cell.target, None, ())
            .await,
    )?;
    if page.is_none() && pending.is_empty() {
        return Ok(());
    }
    settings::with_ownership_lease(server, repo, |cancel| async move {
        if cancel.is_cancelled() {
            return Err(Error::MergeBusy);
        }
        if page.is_some()
            && current_scope(server, repo, organization).await?
            && let Some(page) = app::query_output(
                cell.client
                    .query::<cross::Backfill>(cell.target, None, organization)
                    .await,
            )?
        {
            track(server, organization, page.branches, true).await?;
            app::command_output(
                cell.client
                    .command::<cross::Advance>(
                        cell.target,
                        app::mutation_identity()?,
                        (organization, page.from, page.to),
                    )
                    .await,
            )?;
        }
        for pull in pending {
            refresh(server, repo, pull).await;
        }
        Ok(())
    })
    .await
}
