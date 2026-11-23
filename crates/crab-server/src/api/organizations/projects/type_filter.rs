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
    let Some(filter) = input.issue_type.as_deref() else {
        shared::validate_cursor(input.after.is_some(), None, input.type_cursor.as_deref())?;
        return Ok(None);
    };
    let filter = Selection::parse(filter).map_err(Error::Invalid)?;
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
    let mut grouped = BTreeMap::<Uuid, BTreeMap<u64, Uuid>>::new();
    for row in candidates.items {
        grouped
            .entry(row.repository)
            .or_default()
            .insert(row.number, row.item);
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
                    .numbers
                    .iter()
                    .map(|number| {
                        work.get(number)
                            .copied()
                            .ok_or(Error::CellContract("Unexpected project issue type match"))
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
            items.extend(matched);
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
            items,
        },
        cursor,
        types,
        sources,
    }))
}

impl FilteredItems {
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
