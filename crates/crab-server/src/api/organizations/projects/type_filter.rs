use super::*;
use crate::{
    api::projects::type_filter as shared,
    cells::{
        organization::issue_types::Schema,
        repository::{
            issues::issue_type::search::{Filter as Selection, Match, MatchInput},
            search::index::{GetSearchIndexStatus, SearchIndexStatus},
        },
    },
};

pub(super) struct FilteredItems {
    pub matched: projects::MatchedItems,
    pub cursor: String,
    pub types: Option<Schema>,
    pub issue_type_counts: Option<BTreeMap<String, u64>>,
    sources: Vec<Source>,
}

struct Source {
    repo: Arc<Repository>,
    generation: Uuid,
    revision: u64,
}

pub(super) async fn resolve(
    server: &Server,
    principal: &Principal,
    organization: Uuid,
    project: Uuid,
    actor: Uuid,
    input: &Filter,
) -> Result<Option<FilteredItems>> {
    let group_by_type = input.group_by == Some(source::grouping::GroupBy::IssueType);
    if input.issue_type.is_none() && !group_by_type {
        shared::validate_cursor(input.after.is_some(), None, input.type_cursor.as_deref())?;
        return Ok(None);
    }
    let filter = input
        .issue_type
        .as_deref()
        .map(Selection::parse)
        .transpose()
        .map_err(Error::Invalid)?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let candidates = cells
        .query::<projects::TypeSources>(
            Address::Organization(organization),
            None,
            projects::TypeSourcesQuery {
                actor,
                project,
                repository: input.repository,
                item: input.item,
            },
        )
        .await
        .map_err(read_error)?
        .output
        .ok_or(Error::NotFound)?;
    let mut grouped = BTreeMap::<Uuid, BTreeMap<u64, (Uuid, String)>>::new();
    for row in candidates.items {
        grouped
            .entry(row.repository)
            .or_default()
            .insert(row.number, (row.item, row.status));
    }
    // Project repository links cap this fan-out at 20. Each source work
    // lookup reads just the issue/type index and its snapshot stamp.
    let readable =
        futures_util::future::try_join_all(grouped.into_iter().map(|(id, work)| async move {
            let repo = accessible(server, principal, id).await?;
            Ok::<_, Error>(
                repo.filter(|repo| repo.organization == Some(organization))
                    .map(|repo| (repo, work)),
            )
        }))
        .await?;
    let readable = readable.into_iter().flatten().collect::<Vec<_>>();
    let types = if readable.is_empty() {
        None
    } else {
        Some(issue_types::active(server, Some(organization)).await?)
    };
    let mut items = Vec::new();
    let mut sources = Vec::new();
    let mut issue_type_counts = group_by_type.then(BTreeMap::new);
    if let Some(types) = &types {
        let matches = futures_util::future::try_join_all(readable.into_iter().map(
            |(repo, work)| async move {
                let cell = app::repository_cell(server, &repo)?;
                let scope = shared::scope(&repo, filter, types)?;
                let found = app::query_output(
                    cell.client
                        .query::<Match>(
                            &cell.target,
                            None,
                            MatchInput {
                                numbers: work.keys().copied().collect(),
                                scope,
                            },
                        )
                        .await,
                )?;
                let matched = found
                    .issues
                    .iter()
                    .map(|issue| {
                        let (item, status) = work
                            .get(&issue.number)
                            .ok_or(Error::CellContract("Unexpected project issue type match"))?;
                        let group = (group_by_type
                            && input
                                .status
                                .as_ref()
                                .is_none_or(|selected| selected == status))
                        .then(|| {
                            crate::cells::repository::issues::issue_type::group(
                                true,
                                issue.issue_type,
                            )
                        });
                        Ok((*item, group))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok::<_, Error>((
                    Source {
                        repo,
                        generation: found.generation,
                        revision: found.revision,
                    },
                    matched,
                ))
            },
        ))
        .await?;
        for (source, matched) in matches {
            sources.push(source);
            for (item, group) in matched {
                if filter.is_some() {
                    items.push(item);
                }
                if let (Some(counts), Some(group)) = (&mut issue_type_counts, group) {
                    *counts.entry(group).or_insert(0) += 1;
                }
            }
        }
    }
    let stamps = sources
        .iter()
        .map(|source| {
            (
                source.repo.id,
                source.repo.ownership_epoch,
                source.generation,
                source.revision,
            )
        })
        .collect::<Vec<_>>();
    let cursor = shared::cursor((
        "organization-project-type/1",
        organization,
        project,
        actor,
        candidates.project_version,
        filter,
        types.as_ref().map(|schema| schema.version),
        input.repository,
        input.item,
        &input.status,
        input.group_by,
        stamps,
    ))?;
    shared::validate_cursor(
        input.after.is_some(),
        Some(&cursor),
        input.type_cursor.as_deref(),
    )?;
    Ok(Some(FilteredItems {
        matched: projects::MatchedItems {
            project_version: candidates.project_version,
            items: filter.is_some().then_some(items),
        },
        cursor,
        types,
        issue_type_counts,
        sources,
    }))
}

impl FilteredItems {
    pub(super) fn contains_source(&self, repo: &Repository) -> bool {
        self.sources.iter().any(|source| {
            source.repo.id == repo.id && source.repo.ownership_epoch == repo.ownership_epoch
        })
    }

    pub(super) async fn verify(
        &self,
        server: &Server,
        principal: &Principal,
        organization: Uuid,
    ) -> Result<()> {
        if let Some(types) = &self.types
            && issue_types::active(server, Some(organization))
                .await?
                .version
                != types.version
        {
            return Err(shared::stale());
        }
        futures_util::future::try_join_all(self.sources.iter().map(|source| async move {
            let cell = app::repository_cell(server, &source.repo)?;
            let status = app::query_output(
                cell.client
                    .query::<GetSearchIndexStatus>(&cell.target, None, ())
                    .await,
            )?;
            if !matches!(status, SearchIndexStatus::Ready { generation, revision }
                if generation == source.generation && revision == source.revision)
            {
                return Err(shared::stale());
            }
            // Counts include all matching sources. Check even those absent
            // from this page after sampling their authoritative snapshot.
            match app::membership(server, principal, &source.repo).await {
                Ok(_) => Ok(()),
                Err(Error::NotFound) => Err(shared::stale()),
                Err(error) => Err(error),
            }
        }))
        .await?;
        Ok(())
    }
}
