use std::sync::Arc;

use axum::{
    Extension, Json,
    extract::{Query, State},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{RepositoryLabel, may_read};
use crate::{
    api::catalog,
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::collaboration::{
        Subject, SubjectKind, SubjectSummary,
        watches::{
            DiscussionQuery, DiscussionSubscription, ListDiscussions, SubscriptionOrigin,
            SubscriptionReason, ThreadPreference,
        },
    },
    repository::Repository,
    server::Server,
};

pub(super) mod bulk;
mod timed;

const REPOSITORIES_PER_PAGE: usize = 4;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Parameters {
    repository: Option<Uuid>,
    kind: Option<SubjectKind>,
    reason: Option<SubscriptionReason>,
    sort: Option<Sort>,
    after: Option<String>,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Sort {
    Recent,
    Oldest,
}

#[derive(Serialize)]
struct Item {
    repository: RepositoryLabel,
    #[serde(flatten)]
    discussion: SubjectSummary,
    preference: ThreadPreference,
    origin: Option<SubscriptionOrigin>,
    muted: bool,
}

impl Item {
    fn new(repo: &Repository, item: DiscussionSubscription, muted: bool) -> Self {
        Self {
            repository: RepositoryLabel {
                id: repo.id,
                owner: repo.config.owner.clone(),
                name: repo.config.name.clone(),
            },
            discussion: item.discussion,
            preference: item.preference,
            origin: item.origin,
            muted,
        }
    }
}

#[derive(Serialize)]
pub(super) struct Page {
    items: Vec<Item>,
    next: Option<String>,
}

pub(super) async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Query(mut params): Query<Parameters>,
) -> Result<Json<Page>> {
    let actor = app::account(&principal)?;
    let repositories = visible_repositories(&server, &principal, params.repository).await?;
    if params.repository.is_some() && repositories.is_empty() {
        return Err(Error::NotFound);
    }
    let append_undated = params.sort.is_some() && params.reason.is_none();
    if let Some(sort) = params
        .sort
        .filter(|_| params.reason != Some(SubscriptionReason::Unknown))
    {
        if append_undated && let Some(after) = params.after.as_deref().and_then(undated_cursor) {
            params.after = after.map(str::to_owned);
        } else {
            let page = timed::list(
                &server,
                &principal,
                actor.clone(),
                &params,
                sort,
                &repositories,
            )
            .await?;
            if !page.items.is_empty() || !append_undated {
                return Ok(Json(page));
            }
            params.after = None;
        }
        params.reason = Some(SubscriptionReason::Unknown);
    }
    let after = params.after.as_deref().map(Cursor::parse).transpose()?;
    if let Some(after) = &after
        && (params.repository.is_some_and(|id| id != after.repository)
            || after
                .subject
                .is_some_and(|subject| params.kind.is_some_and(|kind| kind != subject.kind)))
    {
        return Err(Error::Invalid(
            "Subscription cursor does not match the filters",
        ));
    }
    let start = match &after {
        None => 0,
        Some(after) => {
            repositories
                .iter()
                .position(|repository| repository.id == after.repository)
                .ok_or(Error::PageChanged)?
                + usize::from(after.subject.is_none())
        }
    };
    let mut items = Vec::new();
    let mut next = None;
    let mut scanned = Vec::with_capacity(REPOSITORIES_PER_PAGE);
    for (index, repo) in repositories
        .iter()
        .enumerate()
        .skip(start)
        .take(REPOSITORIES_PER_PAGE)
    {
        let cell = app::repository_cell(&server, repo)?;
        let page = app::query_output(
            cell.client
                .query::<ListDiscussions>(
                    &cell.target,
                    None,
                    DiscussionQuery {
                        actor: actor.clone(),
                        kind: params.kind,
                        reason: params.reason,
                        after: after
                            .as_ref()
                            .filter(|after| after.repository == repo.id)
                            .and_then(|after| after.subject),
                    },
                )
                .await,
        )?;
        scanned.push(repo);
        items.extend(
            page.items
                .into_iter()
                .map(|item| Item::new(repo, item, page.muted)),
        );
        next = (page.next.is_some() || index + 1 < repositories.len()).then(|| {
            Cursor {
                repository: repo.id,
                subject: page.next,
            }
            .encode()
        });
        if page.next.is_some() {
            break;
        }
    }
    // Reads may finish after a grant is revoked. Only return titles and
    // continuation positions for repositories whose current grants still hold.
    for repo in scanned {
        if !may_read(&server, &principal, repo).await? {
            return Err(Error::PageChanged);
        }
    }
    if append_undated {
        next = next.map(|cursor| format!("undated:{cursor}"));
    }
    Ok(Json(Page { items, next }))
}

fn undated_cursor(value: &str) -> Option<Option<&str>> {
    if value == "undated" {
        Some(None)
    } else {
        value.strip_prefix("undated:").map(Some)
    }
}

async fn visible_repositories(
    server: &Server,
    principal: &Principal,
    repository: Option<Uuid>,
) -> Result<Vec<Arc<Repository>>> {
    Ok(catalog::visible(server, principal, None, None)
        .await?
        .into_iter()
        .map(|visible| visible.repository)
        .filter(|repo| repository.is_none_or(|id| id == repo.id))
        .collect())
}

struct Cursor {
    repository: Uuid,
    subject: Option<Subject>,
}

impl Cursor {
    fn parse(value: &str) -> Result<Self> {
        const INVALID: Error = Error::Invalid("Subscription cursor is invalid");
        let mut parts = value.split(':');
        let repository = parts
            .next()
            .and_then(|value| Uuid::parse_str(value).ok())
            .filter(|id| !id.is_nil())
            .ok_or(INVALID)?;
        let subject = match (parts.next(), parts.next(), parts.next()) {
            (None, None, None) => None,
            (Some(kind), Some(number), None) => Some(Subject {
                kind: match kind {
                    "issue" => SubjectKind::Issue,
                    "pull" => SubjectKind::Pull,
                    _ => return Err(INVALID),
                },
                number: number
                    .parse()
                    .ok()
                    .and_then(|number| app::number(number).ok())
                    .ok_or(INVALID)?,
            }),
            _ => return Err(INVALID),
        };
        Ok(Self {
            repository,
            subject,
        })
    }

    fn encode(self) -> String {
        match self.subject {
            None => self.repository.to_string(),
            Some(Subject { kind, number }) => {
                let kind = match kind {
                    SubjectKind::Issue => "issue",
                    SubjectKind::Pull => "pull",
                };
                format!("{}:{kind}:{number}", self.repository)
            }
        }
    }
}

#[cfg(test)]
mod tests;
