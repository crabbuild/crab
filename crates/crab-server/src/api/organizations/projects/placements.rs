use super::*;
use crate::{
    api::projects::type_filter as filter,
    cells::repository::{
        collaboration::SubjectKind,
        issues::transfer::publication::{Destination, GetLocations, Location},
        projects::organization_issue::GetCreatedIssue,
    },
};
use futures_util::{StreamExt, TryStreamExt, stream};
use std::collections::BTreeSet;

struct Pending {
    source: projects::WorkSource,
    seen: BTreeSet<(Uuid, u64)>,
}

pub(super) struct Placement {
    pub item: Uuid,
    pub subject: Subject,
    pub status: String,
    pub repository: Arc<Repository>,
}

#[derive(Clone)]
struct Observation {
    repository: Arc<Repository>,
    revision: u64,
}

pub(super) struct Locations {
    pub project_version: u64,
    pub rows: Vec<Placement>,
    observations: BTreeMap<Uuid, Observation>,
}

pub(super) async fn resolve(
    server: &Server,
    principal: &Principal,
    organization: Uuid,
    input: projects::SourcesQuery,
) -> Result<Locations> {
    let page = server
        .cells()
        .ok_or(Error::CellUnavailable)?
        .query::<projects::Sources>(Address::Organization(organization), None, input)
        .await
        .map_err(read_error)?
        .output
        .ok_or(Error::NotFound)?;
    let mut pending = BTreeMap::<Uuid, Vec<Pending>>::new();
    for source in page.items {
        pending.entry(source.repository).or_default().push(Pending {
            seen: BTreeSet::from([(source.repository, source.subject.number)]),
            source,
        });
    }
    let mut result = Locations {
        project_version: page.project_version,
        rows: Vec::new(),
        observations: BTreeMap::new(),
    };
    while !pending.is_empty() {
        // Resolve one hop per round, batching all numbers at the same owner.
        // Most work is present at its stored address and needs only one batch.
        let batches: Vec<_> = stream::iter(std::mem::take(&mut pending))
            .map(|(id, entries)| async move { batch(server, principal, id, entries).await })
            .buffer_unordered(8)
            .try_filter_map(futures_util::future::ok)
            .try_collect()
            .await?;
        for (repo, revision, entries) in batches {
            let observed = result
                .observations
                .entry(repo.id)
                .or_insert_with(|| Observation {
                    repository: Arc::clone(&repo),
                    revision,
                });
            if observed.revision != revision
                || observed.repository.ownership_epoch != repo.ownership_epoch
            {
                return Err(filter::stale());
            }
            for (mut entry, location) in entries {
                match location {
                    Location::Present => result.rows.push(Placement {
                        item: entry.source.item,
                        subject: entry.source.subject,
                        status: entry.source.status,
                        repository: Arc::clone(&repo),
                    }),
                    Location::Moved { destination } => {
                        if !entry
                            .seen
                            .insert((destination.repository, destination.number))
                        {
                            return Err(Error::CellContract(
                                "Issue transfer redirects form a cycle",
                            ));
                        }
                        entry.source.subject.number = destination.number;
                        pending
                            .entry(destination.repository)
                            .or_default()
                            .push(entry);
                    }
                    Location::Missing => {}
                }
            }
        }
    }
    result.rows.sort_unstable_by_key(|row| row.item);
    Ok(result)
}

async fn batch(
    server: &Server,
    principal: &Principal,
    id: Uuid,
    entries: Vec<Pending>,
) -> Result<Option<(Arc<Repository>, u64, Vec<(Pending, Location)>)>> {
    let Some(repo) = accessible(server, principal, id).await? else {
        return Ok(None);
    };
    let cell = app::repository_cell(server, &repo)?;
    let numbers: Vec<_> = entries
        .iter()
        .filter_map(|entry| {
            (entry.source.subject.kind == SubjectKind::Issue).then_some(entry.source.subject.number)
        })
        .collect();
    let count = numbers.len();
    let observed = cell
        .client
        .query::<GetLocations>(&cell.target, None, numbers)
        .await
        .map_err(Error::Cell)?;
    if observed.output.len() != count {
        return Err(Error::CellContract(
            "Project issue location count differs from its request",
        ));
    }
    let mut locations = observed.output.into_iter();
    let ready = entries
        .into_iter()
        .map(|entry| {
            let location = if entry.source.subject.kind == SubjectKind::Issue {
                locations
                    .next()
                    .ok_or(Error::CellContract("Project issue location is missing"))?
            } else {
                // Pulls retain their original repository and number.
                Location::Present
            };
            Ok((entry, location))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some((repo, observed.receipt.revision, ready)))
}

impl Locations {
    pub(super) fn stamp(&self) -> Vec<(Uuid, u32, u64)> {
        self.observations
            .values()
            .map(|value| {
                (
                    value.repository.id,
                    value.repository.ownership_epoch,
                    value.revision,
                )
            })
            .collect()
    }

    pub(super) async fn verify(&self, server: &Server, principal: &Principal) -> Result<()> {
        let observations = self.observations.values().cloned().collect::<Vec<_>>();
        stream::iter(observations)
            .map(|value| async move {
                let repo = &value.repository;
                let cell = app::repository_cell(server, repo)?;
                let observed = cell
                    .client
                    .query::<GetLocations>(&cell.target, None, Vec::new())
                    .await
                    .map_err(Error::Cell)?;
                if observed.receipt.revision != value.revision {
                    return Err(filter::stale());
                }
                match app::membership(server, principal, repo).await {
                    Ok(_) => Ok(()),
                    Err(Error::NotFound) => Err(filter::stale()),
                    Err(error) => Err(error),
                }
            })
            .buffer_unordered(8)
            .try_for_each(|()| async { Ok(()) })
            .await
    }
}

pub(super) async fn check_attachment(
    server: &Server,
    organization: Uuid,
    project: projects::ProjectKey,
    candidate: Destination,
) -> Result<Option<projects::AttachmentCheck>> {
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let Some(sources) = cells
        .query::<projects::GetAttachmentSources>(Address::Organization(organization), None, project)
        .await
        .map_err(read_error)?
        .output
    else {
        return Ok(None);
    };
    let result = |already_linked| {
        Some(projects::AttachmentCheck {
            project_version: sources.project_version,
            already_linked,
        })
    };

    if sources.issues.contains(&candidate) {
        return Ok(result(true));
    }

    // A lost Project reply can leave the issue published with only a pending
    // placement. Sample both forms in one Organization snapshot and use each
    // source's durable publication receipt before comparing issue addresses.
    let created: Vec<_> = stream::iter(sources.pending)
        .map(|(repository, request)| {
            let cells = &cells;
            async move {
                let number = app::query_output(
                    cells
                        .query::<GetCreatedIssue>(repository, None, (organization, request))
                        .await,
                )?;
                Ok::<_, Error>(number.map(|number| Destination { repository, number }))
            }
        })
        .buffer_unordered(8)
        .try_filter_map(futures_util::future::ok)
        .try_collect()
        .await?;
    let mut addresses: Vec<_> = sources.issues.into_iter().chain(created).collect();
    let mut visited = BTreeSet::new();
    while !addresses.is_empty() {
        let mut by_repository = BTreeMap::<Uuid, Vec<u64>>::new();
        for address in addresses {
            if address == candidate {
                return Ok(result(true));
            }
            if visited.insert((address.repository, address.number)) {
                by_repository
                    .entry(address.repository)
                    .or_default()
                    .push(address.number);
            }
        }
        // This is Project-write admission. The manager may attach the live
        // candidate, but no historical address or restricted metadata leaves
        // the coordinator. Old Repository UUIDs still own their move receipts.
        let batches: Vec<Vec<Location>> = stream::iter(by_repository)
            .map(|(repository, numbers)| {
                let cells = &cells;
                async move {
                    let expected = numbers.len();
                    let located = app::query_output(
                        cells.query::<GetLocations>(repository, None, numbers).await,
                    )?;
                    if located.len() != expected {
                        return Err(Error::CellContract(
                            "Project issue location count differs from its request",
                        ));
                    }
                    Ok(located)
                }
            })
            .buffer_unordered(8)
            .try_collect()
            .await?;
        addresses = batches
            .into_iter()
            .flatten()
            .filter_map(|located| match located {
                Location::Moved { destination } => Some(destination),
                _ => None,
            })
            .collect();
    }
    Ok(result(false))
}
