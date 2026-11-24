use std::collections::{BTreeSet, HashMap};

use futures_util::{StreamExt, TryStreamExt, stream};
use uuid::Uuid;

use super::{Entry, NoticeTarget, may_read};
use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::{
        collaboration::SubjectKind,
        search::index::references::{FindReferences, ReferenceInput},
    },
    repository::Repository,
    server::Server,
};

pub(super) async fn readable_origins(
    server: &Server,
    principal: &Principal,
    origins: BTreeSet<Uuid>,
) -> Result<BTreeSet<Uuid>> {
    let found: Vec<_> = stream::iter(origins)
        .map(|id| async move {
            let Some(repo) = server.repositories.by_id(id) else {
                return Ok::<_, Error>(None);
            };
            Ok(may_read(server, principal, &repo).await?.then_some(id))
        })
        .buffer_unordered(8)
        .try_collect()
        .await?;
    Ok(found.into_iter().flatten().collect())
}

pub(super) async fn sources(
    server: &Server,
    principal: &Principal,
    repo: &Repository,
    entries: &[&Entry],
) -> Result<HashMap<u64, String>> {
    let transfers: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry.notice.target {
            NoticeTarget::IssueTransfer { origin, number } => Some((origin, number)),
            _ => None,
        })
        .collect();
    if transfers.is_empty() {
        return Ok(HashMap::new());
    }
    let origins = transfers.iter().map(|(origin, _)| *origin).collect();
    let visible = readable_origins(server, principal, origins).await?;
    let cell = app::repository_cell(server, repo)?;
    let sources: Vec<_> = stream::iter(transfers)
        .filter(|(origin, _)| std::future::ready(visible.contains(origin)))
        .map(|(origin, number)| {
            let cell = &cell;
            async move {
                let found = app::query_output(
                    cell.client
                        .query::<FindReferences>(
                            &cell.target,
                            None,
                            ReferenceInput {
                                search: number.to_string(),
                                exact: true,
                            },
                        )
                        .await,
                )?;
                Ok::<_, Error>(
                    found
                        .into_iter()
                        .find(|item| item.subject.kind == SubjectKind::Issue)
                        .map(|item| (origin, number, item.title)),
                )
            }
        })
        .buffer_unordered(6)
        .try_collect()
        .await?;
    // The target can take a separate storage round trip. Repeat source
    // authorization before disclosing an address or raising the unread badge.
    let visible = readable_origins(server, principal, visible).await?;
    Ok(sources
        .into_iter()
        .flatten()
        .filter_map(|(origin, number, title)| visible.contains(&origin).then_some((number, title)))
        .collect())
}
