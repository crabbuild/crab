use std::collections::BTreeSet;

use crate::{
    app::{self, Error, Result},
    cells::{
        Address,
        organization::development::descriptions::{Project, Replace},
        repository::{
            collaboration::{
                GetDevelopmentEndpoint, Subject, SubjectKind,
                links::keywords::{
                    Qualified,
                    projection::{Acknowledge, Backfill, Pending, References, Snapshot, View},
                },
            },
            issues::transfer::publication::Destination,
        },
    },
    server::Server,
};

use super::{Repository, commit_closures::current_issue};

pub(crate) async fn snapshot(
    server: &Server,
    repo: &Repository,
    pull: u64,
) -> Result<Option<Snapshot>> {
    let cell = app::repository_cell(server, repo)?;
    app::query_output(cell.client.query::<View>(cell.target, None, pull).await)
}

pub(super) async fn resolve<T>(
    server: &Server,
    source: &Repository,
    targets: impl IntoIterator<Item = (Qualified, T)>,
) -> Result<Vec<(Destination, T)>> {
    let mut targets = targets.into_iter().peekable();
    if targets.peek().is_none() {
        return Ok(Vec::new());
    }
    let Some(organization) = source.organization else {
        return Ok(Vec::new());
    };
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let (document, _) = catalog
        .load_snapshot()
        .await
        .map_err(app::repository_access_error)?;
    let mut issues = Vec::new();
    let mut seen = BTreeSet::new();
    for (target, value) in targets {
        let record = document
            .repositories
            .iter()
            .find(|record| {
                record.owner.eq_ignore_ascii_case(&target.owner)
                    && record.name.eq_ignore_ascii_case(&target.repository)
            })
            .or_else(|| document.renamed_route(&target.owner, &target.repository));
        let Some(record) = record.filter(|record| record.organization_id() == Some(organization))
        else {
            continue;
        };
        if record.id == source.id {
            continue;
        }
        let original = Destination {
            repository: record.id,
            number: target.issue,
        };
        let mut origin = original;
        if let Some(current) = current_issue(server, original).await?
            && let Some(repo) = server
                .repositories
                .by_id(current.repository)
                .filter(|repo| repo.organization == Some(organization))
        {
            let cell = app::repository_cell(server, &repo)?;
            if let Some(view) = app::query_output(
                cell.client
                    .query::<GetDevelopmentEndpoint>(
                        cell.target,
                        None,
                        Subject {
                            kind: SubjectKind::Issue,
                            number: current.number,
                        },
                    )
                    .await,
            )? {
                origin = view.issue_origin.unwrap_or(current);
            }
        }
        if seen.insert((origin.repository, origin.number)) {
            issues.push((origin, value));
        }
    }
    Ok(issues)
}

pub(crate) async fn targets(
    server: &Server,
    repo: &Repository,
    references: References,
    require_default: bool,
) -> Result<Vec<Destination>> {
    match references {
        References::Published(issues) => Ok(issues),
        References::Live { base_ref, targets } => {
            if require_default && !targets.is_empty() && base_ref != repo.default_ref().await? {
                return Ok(Vec::new());
            }
            Ok(
                resolve(server, repo, targets.into_iter().map(|target| (target, ())))
                    .await?
                    .into_iter()
                    .map(|(target, ())| target)
                    .collect(),
            )
        }
    }
}

async fn synchronize(server: &Server, repo: &Repository, pull: u64) -> Result<()> {
    let Some(organization) = repo.organization else {
        return Ok(());
    };
    let Some(view) = snapshot(server, repo, pull).await? else {
        return Ok(());
    };
    let issues = targets(server, repo, view.references, false).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    app::command_output(
        cells
            .command::<Replace>(
                Address::Organization(organization),
                app::mutation_identity()?,
                Project {
                    pull: Destination {
                        repository: repo.id,
                        number: pull,
                    },
                    version: view.version,
                    issues,
                },
            )
            .await,
    )?;
    app::command_output(
        cells
            .command::<Acknowledge>(repo.id, app::mutation_identity()?, (pull, view.version))
            .await,
    )
}

pub(crate) async fn refresh(server: &Server, repo: &Repository, pull: u64) {
    if let Err(error) = synchronize(server, repo, pull).await {
        tracing::warn!(repository = %repo.id, pull, %error, "qualified issue reference projection will retry");
    }
}

pub(crate) async fn recover(server: &Server, repo: &Repository) -> Result<()> {
    if repo.organization.is_none() {
        return Ok(());
    }
    let cell = app::repository_cell(server, repo)?;
    let (ready, pending) =
        app::query_output(cell.client.query::<Pending>(cell.target, None, ()).await)?;
    if !ready {
        app::command_output(
            cell.client
                .command::<Backfill>(cell.target, app::mutation_identity()?, ())
                .await,
        )?;
    }
    for pull in pending {
        refresh(server, repo, pull).await;
    }
    Ok(())
}
