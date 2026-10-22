use std::sync::Arc;

use crate::{
    api::{members::roster::Roster, profiles::attribution::Authors},
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::review_requests::{
        self, GetReviewRequest, Key, ListReviewRequests, Outcome, Selection, SetReviewRequest,
    },
    server::Server,
};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new().route(
        "/api/repos/{owner}/{name}/pulls/{number}/review-requests",
        get(list).put(set),
    )
}

fn valid_id(id: &str) -> Result<()> {
    if id.len() != 64
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Invalid("Invalid review request ID or cursor"));
    }
    Ok(())
}

fn view(request: &review_requests::Request, roster: &Roster, authors: &Authors) -> Value {
    let member = roster.candidate(&request.reviewer);
    json!({
        "id":review_requests::reviewer_id(&request.reviewer),
        "reviewer":authors.view(&request.reviewer),
        "candidate_id":member.filter(|_| !roster.principal.is_local()).map(|member| member.id),
        "state":request.state,
        "requested_by":authors.view(&request.requested_by),
        "requested_at":request.requested_at,
        "updated_at":request.updated_at,
    })
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Parameters {
    before: Option<String>,
    limit: Option<u8>,
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Query(params): Query<Parameters>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let limit = params.limit.unwrap_or(30);
    if !(1..=50).contains(&limit) {
        return Err(Error::Invalid("Review request pages require 1–50 items"));
    }
    if let Some(before) = &params.before {
        valid_id(before)?;
    }
    let cell = app::repository_cell(&server, &repo)?;
    let page = app::query_output(
        cell.client
            .query::<ListReviewRequests>(
                &cell.target,
                None,
                review_requests::Query {
                    pull: app::number(number)?,
                    before: params.before,
                    limit,
                },
            )
            .await,
    )?
    .ok_or(Error::NotFound)?;
    let actors: Vec<_> = page
        .items
        .iter()
        .flat_map(|request| [&request.reviewer, &request.requested_by])
        .collect();
    let authors = Authors::load(&server, &principal, actors.into_iter()).await?;
    let roster = Roster::load(&server, &principal, &repo).await?;
    let can_manage = page.open && !page.merge_pending && roster.principal.can_triage(repo.id);
    let items: Vec<_> = page
        .items
        .iter()
        .map(|request| view(request, &roster, &authors))
        .collect();
    let author_id = roster.candidate(&page.author).map(|candidate| candidate.id);
    let candidates: Vec<_> = roster
        .items
        .iter()
        .filter(|candidate| !principal.is_local() && Some(candidate.id) != author_id)
        .collect();
    Ok(Json(
        json!({"items":items,"next":page.next,"version":page.version,"can_manage":can_manage,"candidates":candidates}),
    ))
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Change {
    Request { actor_id: uuid::Uuid },
    Cancel { id: String },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    version: u64,
    change: Change,
}

async fn set(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    input: std::result::Result<Json<Input>, JsonRejection>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let actor = app::actor(&principal)?;
    let Json(input) = input?;
    let number = app::number(number)?;
    let version = app::number(input.version)?;
    let cell = app::repository_cell(&server, &repo)?;
    let (reviewer, requested, current) = match input.change {
        Change::Request { actor_id } => {
            if principal.is_local() {
                return Err(Error::Invalid(
                    "Review requests require members from the configured identity provider",
                ));
            }
            let roster = Roster::load(&server, &principal, &repo).await?;
            let reviewer = roster
                .author(actor_id)
                .ok_or(Error::Invalid(
                    "Reviewer must be a current repository member",
                ))?
                .clone();
            (reviewer, true, roster.principal)
        }
        Change::Cancel { id } => {
            valid_id(&id)?;
            let request = app::query_output(
                cell.client
                    .query::<GetReviewRequest>(&cell.target, None, Key { pull: number, id })
                    .await,
            )?
            .ok_or(Error::NotFound)?;
            // Cancel by the stored qualified identity, including after membership
            // removal or provider changes; names and current subjects cannot retarget it.
            let current = app::membership(&server, &principal, &repo).await?.principal;
            (request.reviewer, false, current)
        }
    };
    if !current.can_triage(repo.id) {
        return Err(Error::Forbidden);
    }
    let outcome = app::command_output(
        cell.client
            .command::<SetReviewRequest>(
                &cell.target,
                app::mutation_identity()?,
                Selection {
                    pull: number,
                    version,
                    actor: (&actor).into(),
                    can_manage: true,
                    reviewer,
                    requested,
                },
            )
            .await,
    )?;
    let request = match outcome {
        Outcome::Applied(request) => request,
        Outcome::Archived => return Err(Error::Archived),
        Outcome::NotFound => return Err(Error::NotFound),
        Outcome::Forbidden => return Err(Error::Forbidden),
        Outcome::OwnReview => {
            return Err(Error::Invalid(
                "Pull request authors cannot be requested as reviewers",
            ));
        }
        Outcome::Closed => {
            return Err(Error::Invalid(
                "Only open pull requests can request reviews",
            ));
        }
        Outcome::MergePending => return Err(Error::MergePending),
        Outcome::Conflict => return Err(Error::Conflict),
    };
    let authors = Authors::load(
        &server,
        &principal,
        [&request.reviewer, &request.requested_by].into_iter(),
    )
    .await?;
    let roster = Roster::load(&server, &principal, &repo).await?;
    Ok(Json(view(&request, &roster, &authors)))
}

#[cfg(test)]
mod tests;
