use std::{cmp::Ordering, collections::HashMap, sync::Arc};

use futures_util::{StreamExt, TryStreamExt, stream};
use uuid::Uuid;

use super::{Cursor, Item, Page, Parameters, Sort, visible_repositories};
use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::{
        directory::IdentityKey,
        repository::collaboration::{
            Subject, SubjectKind,
            watches::{ListTimedDiscussions, TIMED_PAGE, TimeBound, TimedQuery},
        },
    },
    repository::Repository,
    server::Server,
};

pub(super) async fn list(
    server: &Server,
    principal: &Principal,
    actor: IdentityKey,
    params: &Parameters,
    sort: Sort,
    repositories: &[Arc<Repository>],
) -> Result<Page> {
    let after = params
        .after
        .as_deref()
        .map(|value| TimedCursor::parse(value, sort))
        .transpose()?;
    if let Some(cursor) = &after {
        if params.kind.is_some_and(|kind| kind != cursor.subject.kind)
            || params.repository.is_some_and(|id| id != cursor.repository)
        {
            return Err(Error::Invalid(
                "Subscription cursor does not match the filters",
            ));
        }
        if !repositories.iter().any(|repo| repo.id == cursor.repository) {
            return Err(Error::PageChanged);
        }
    }
    // An exact account-wide sort reads a bounded top page from every
    // authorized Repository Cell. The global cutoff is applied in each Cell.
    let candidates = stream::iter(repositories.iter().cloned())
        .map(|repo| {
            let actor = actor.clone();
            let bound = after.as_ref().map(|cursor| cursor.bound(repo.id));
            async move {
                let cell = app::repository_cell(server, &repo)?;
                let page = app::query_output(
                    cell.client
                        .query::<ListTimedDiscussions>(
                            &cell.target,
                            None,
                            TimedQuery {
                                actor,
                                kind: params.kind,
                                reason: params.reason,
                                oldest: sort == Sort::Oldest,
                                bound,
                            },
                        )
                        .await,
                )?;
                page.items
                    .into_iter()
                    .map(|item| {
                        let origin = item
                            .origin
                            .ok_or(Error::CellContract("Subscription date is missing"))?;
                        let cursor = TimedCursor {
                            date: origin.subscribed_at_ms,
                            repository: repo.id,
                            subject: item.discussion.subject,
                        };
                        Ok((cursor, Item::new(&repo, item, page.muted)))
                    })
                    .collect::<Result<Vec<_>>>()
            }
        })
        .buffer_unordered(8)
        .try_fold(Vec::new(), |mut candidates, mut page| async move {
            candidates.append(&mut page);
            candidates.sort_unstable_by(|(a, _), (b, _)| match sort {
                Sort::Oldest => a.key().cmp(&b.key()),
                Sort::Recent => b.key().cmp(&a.key()),
            });
            candidates.truncate(TIMED_PAGE + 1);
            Ok(candidates)
        })
        .await?;

    let visible: HashMap<_, _> = visible_repositories(server, principal, params.repository)
        .await?
        .into_iter()
        .map(|repo| (repo.id, repo))
        .collect();
    if candidates.iter().any(|(_, item)| {
        visible.get(&item.repository.id).is_none_or(|repo| {
            repo.config.owner != item.repository.owner || repo.config.name != item.repository.name
        })
    }) {
        return Err(Error::PageChanged);
    }
    let next = if candidates.len() > TIMED_PAGE {
        Some(candidates[TIMED_PAGE - 1].0.encode(sort))
    } else if params.reason.is_none() && !candidates.is_empty() {
        Some("undated".into())
    } else {
        None
    };
    let items = candidates
        .into_iter()
        .take(TIMED_PAGE)
        .map(|(_, item)| item)
        .collect();
    Ok(Page { items, next })
}

struct TimedCursor {
    date: u64,
    repository: Uuid,
    subject: Subject,
}

impl TimedCursor {
    fn key(&self) -> (u64, Uuid, bool, u64) {
        (
            self.date,
            self.repository,
            self.subject.kind == SubjectKind::Pull,
            self.subject.number,
        )
    }

    fn parse(value: &str, sort: Sort) -> Result<Self> {
        const INVALID: Error = Error::Invalid("Subscription date cursor is invalid");
        let (direction, value) = value.split_once(':').ok_or(INVALID)?;
        if direction != Self::direction(sort) {
            return Err(INVALID);
        }
        let (date, value) = value.split_once(':').ok_or(INVALID)?;
        let date = date
            .parse()
            .ok()
            .filter(|date| *date <= app::MAX_NUMBER)
            .ok_or(INVALID)?;
        let Cursor {
            repository,
            subject,
        } = Cursor::parse(value)?;
        Ok(Self {
            date,
            repository,
            subject: subject.ok_or(INVALID)?,
        })
    }

    fn encode(&self, sort: Sort) -> String {
        format!(
            "{}:{}:{}",
            Self::direction(sort),
            self.date,
            Cursor {
                repository: self.repository,
                subject: Some(self.subject),
            }
            .encode()
        )
    }

    fn direction(sort: Sort) -> &'static str {
        match sort {
            Sort::Recent => "recent",
            Sort::Oldest => "oldest",
        }
    }

    fn bound(&self, repo: Uuid) -> TimeBound {
        // At equal timestamps, the repository UUID precedes the typed
        // subject in the global key. Include or exclude the whole tie.
        match repo.cmp(&self.repository) {
            Ordering::Equal => TimeBound::Subject(self.date, self.subject),
            Ordering::Less => TimeBound::End(self.date),
            Ordering::Greater => TimeBound::Start(self.date),
        }
    }
}
