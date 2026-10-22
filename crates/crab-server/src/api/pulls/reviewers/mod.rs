use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    api::{members::roster::Roster, profiles::attribution::Authors},
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::{
        review_requests,
        reviewers::{self, ListReviewers},
    },
    server::Server,
};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new().route(
        "/api/repos/{owner}/{name}/pulls/{number}/reviewers",
        get(list),
    )
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Parameters {
    after: Option<String>,
    limit: Option<u8>,
}

fn review_view(review: &reviewers::Review, head: Option<&str>) -> Value {
    let state = super::reviews::state(review.state, review.dismissed);
    json!({
        "number": review.number,
        "state": state,
        "commit_oid": review.commit_oid,
        "current": head.map(|head| head == review.commit_oid),
    })
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
        return Err(Error::Invalid("Reviewer pages require 1–50 items"));
    }
    if let Some(after) = &params.after {
        super::review_requests::valid_id(after)?;
    }
    let cell = app::repository_cell(&server, &repo)?;
    let page = app::query_output(
        cell.client
            .query::<ListReviewers>(
                &cell.target,
                None,
                reviewers::Query {
                    pull: app::number(number)?,
                    after: params.after,
                    limit,
                },
            )
            .await,
    )?
    .ok_or(Error::NotFound)?;
    let head = if let Some(head) = page.merged_head {
        Some(head)
    } else if page.items.iter().all(|reviewer| reviewer.review.is_none()) {
        None
    } else {
        repo.ref_oid(&page.head_ref).await?
    };
    let actors: Vec<_> = page
        .items
        .iter()
        .flat_map(|reviewer| {
            std::iter::once(&reviewer.author).chain(
                reviewer
                    .request
                    .as_ref()
                    .map(|request| &request.requested_by),
            )
        })
        .collect();
    let authors = Authors::load(&server, &principal, actors.into_iter()).await?;
    let roster = Roster::load(&server, &principal, &repo).await?;
    let author_id = roster.candidate(&page.author).map(|candidate| candidate.id);
    let candidates: Vec<_> = roster
        .items
        .iter()
        .filter(|candidate| !principal.is_local() && Some(candidate.id) != author_id)
        .collect();
    let items: Vec<_> = page
        .items
        .iter()
        .map(|reviewer| {
            let author = authors.view(&reviewer.author);
            let candidate_id = roster
                .candidate(&reviewer.author)
                .filter(|candidate| !roster.principal.is_local() && Some(candidate.id) != author_id)
                .map(|candidate| candidate.id);
            let request = reviewer.request.as_ref().map(|request| {
                let id = review_requests::reviewer_id(&request.reviewer);
                let requested_by = authors.view(&request.requested_by);
                json!({
                    "id": id,
                    "state": request.state,
                    "requested_by": requested_by,
                    "requested_at": request.requested_at,
                    "updated_at": request.updated_at,
                })
            });
            let review = reviewer
                .review
                .as_ref()
                .map(|review| review_view(review, head.as_deref()));
            let decision = reviewer
                .decision
                .as_ref()
                .map(|review| review_view(review, head.as_deref()));
            json!({
                "id": reviewer.id,
                "reviewer": author,
                "candidate_id": candidate_id,
                "request": request,
                "review": review,
                "decision": decision,
            })
        })
        .collect();
    let can_manage = page.open && !page.merge_pending && roster.principal.can_triage(repo.id);
    Ok(Json(
        json!({"items":items,"next":page.next,"version":page.version,"can_manage":can_manage,"candidates":candidates,"head_oid":head}),
    ))
}

#[cfg(test)]
mod tests;
