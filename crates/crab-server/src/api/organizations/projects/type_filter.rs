use super::*;
use crate::{
    api::projects::type_filter as shared,
    cells::{
        organization::issue_types::Schema,
        repository::{
            collaboration::SubjectKind,
            issues::issue_type::search::{Filter as Selection, Match, MatchInput},
            search::index::{GetSearchIndexStatus, SearchIndexStatus},
        },
    },
};
use futures_util::{StreamExt, TryStreamExt, stream};

pub(super) struct FilteredItems {
    pub matched: projects::MatchedItems,
    pub cursor: String,
    pub types: Option<Schema>,
    pub issue_type_counts: Option<BTreeMap<String, u64>>,
    sources: Vec<Source>,
    locations: placements::Locations,
    tracks_types: bool,
}

#[derive(Clone)]
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
    actor: projects::AccessActor,
    input: &Filter,
) -> Result<Option<FilteredItems>> {
    let group_by_type = input.group_by == Some(source::grouping::GroupBy::IssueType);
    let tracks_types = input.issue_type.is_some() || group_by_type;
    if !tracks_types && input.repository.is_none() {
        shared::validate_cursor(input.after.is_some(), None, input.type_cursor.as_deref())?;
        return Ok(None);
    }
    let filter = input
        .issue_type
        .as_deref()
        .map(Selection::parse)
        .transpose()
        .map_err(Error::Invalid)?;
    let locations = placements::resolve(
        server,
        principal,
        organization,
        projects::SourcesQuery {
            actor,
            project,
            item: input.item,
            include_pulls: input.repository.is_some(),
        },
    )
    .await?;
    let scoped = locations
        .rows
        .iter()
        .filter(|row| input.repository.is_none_or(|id| row.repository.id == id));
    let mut items = if input.repository.is_some() && filter.is_none() {
        scoped.clone().map(|row| row.item).collect()
    } else {
        Vec::new()
    };
    let mut grouped = BTreeMap::<_, (_, BTreeMap<_, Vec<_>>)>::new();
    if tracks_types {
        for row in scoped.filter(|row| {
            row.subject.kind == SubjectKind::Issue
                && row.repository.organization == Some(organization)
        }) {
            let matches_status = input
                .status
                .as_ref()
                .is_none_or(|status| status == &row.status);
            grouped
                .entry(row.repository.id)
                .or_insert_with(|| (Arc::clone(&row.repository), BTreeMap::new()))
                .1
                .entry(row.subject.number)
                .or_default()
                .push((row.item, matches_status));
        }
    }
    let types = if grouped.is_empty() {
        None
    } else {
        Some(issue_types::active(server, Some(organization)).await?)
    };
    let mut sources = Vec::new();
    let mut issue_type_counts = group_by_type.then(BTreeMap::new);
    if let Some(types) = &types {
        let matches: Vec<_> = stream::iter(grouped.into_values())
            .map(|(repo, work)| async move {
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
                let mut matched = Vec::new();
                for issue in found.issues {
                    let rows = work
                        .get(&issue.number)
                        .ok_or(Error::CellContract("Unexpected project issue type match"))?;
                    for &(item, matches_status) in rows {
                        let group = (group_by_type && matches_status).then(|| {
                            crate::cells::repository::issues::issue_type::group(
                                true,
                                issue.issue_type,
                            )
                        });
                        matched.push((item, group));
                    }
                }
                Ok::<_, Error>((
                    Source {
                        repo,
                        generation: found.generation,
                        revision: found.revision,
                    },
                    matched,
                ))
            })
            .buffer_unordered(8)
            .try_collect()
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
    sources.sort_unstable_by_key(|source| source.repo.id);
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
        "organization-project-source/1",
        organization,
        project,
        actor,
        locations.project_version,
        filter,
        types.as_ref().map(|schema| schema.version),
        input.repository,
        input.item,
        &input.status,
        input.group_by,
        stamps,
        locations.stamp(),
    ))?;
    shared::validate_cursor(
        input.after.is_some(),
        Some(&cursor),
        input.type_cursor.as_deref(),
    )?;
    Ok(Some(FilteredItems {
        matched: projects::MatchedItems {
            project_version: locations.project_version,
            items: (filter.is_some() || input.repository.is_some()).then_some(items),
        },
        cursor,
        types,
        issue_type_counts,
        sources,
        locations,
        tracks_types,
    }))
}

impl FilteredItems {
    pub(super) fn contains_source(&self, repo: &Repository) -> bool {
        !self.tracks_types
            || self.sources.iter().any(|source| {
                source.repo.id == repo.id && source.repo.ownership_epoch == repo.ownership_epoch
            })
    }

    pub(super) async fn verify(
        &self,
        server: &Server,
        principal: &Principal,
        organization: Uuid,
    ) -> Result<()> {
        self.locations.verify(server, principal).await?;
        if let Some(types) = &self.types
            && issue_types::active(server, Some(organization))
                .await?
                .version
                != types.version
        {
            return Err(shared::stale());
        }
        stream::iter(self.sources.clone())
            .map(|source| async move {
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
            })
            .buffer_unordered(8)
            .try_for_each(|()| async { Ok(()) })
            .await?;
        Ok(())
    }
}
