use std::sync::Arc;

use crate::{
    app::{self, Error, Result},
    auth::{Identity, Principal},
    cells::repository::{
        RepositoryAuthor,
        review_requests::{
            self, GetReviewRequest, Key, ListReviewRequests, Outcome, Selection, SetReviewRequest,
        },
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

fn view(
    request: &review_requests::Request,
    members: &[crate::RepositoryMember],
    actor: &Identity,
) -> Value {
    let member = members.iter().find(|member| {
        request.reviewer.issuer == actor.issuer && member.subject == request.reviewer.subject
    });
    json!({
        "id":review_requests::reviewer_id(&request.reviewer),
        "name":member.map_or(request.reviewer.name.as_str(),|member|member.name.as_str()),
        "available":member.is_some(),
        "state":request.state,
        "requested_by":request.requested_by.name,
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
    let actor = app::actor(&principal)?;
    let membership = app::membership(&server, &principal, &repo).await?;
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
    let is_author = page.author.issuer == actor.issuer && page.author.subject == actor.subject;
    let can_manage = page.open && !page.merge_pending && (membership.can_write || is_author);
    let items: Vec<_> = page
        .items
        .iter()
        .map(|request| view(request, &membership.members, &actor))
        .collect();
    let eligible: Vec<_> = membership
        .members
        .iter()
        .filter(|member| {
            page.author.issuer != actor.issuer || member.subject != page.author.subject
        })
        .map(|member| json!({"subject":member.subject,"name":member.name}))
        .collect();
    Ok(Json(
        json!({"items":items,"next":page.next,"version":page.version,"can_manage":can_manage,"eligible":eligible}),
    ))
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Change {
    Request { subject: String },
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
    let membership = app::membership(&server, &principal, &repo).await?;
    let Json(input) = input?;
    let number = app::number(number)?;
    let version = app::number(input.version)?;
    let cell = app::repository_cell(&server, &repo)?;
    let (reviewer, requested) = match input.change {
        Change::Request { subject } => {
            if matches!(principal, Principal::Local) {
                return Err(Error::Invalid(
                    "Review requests require members from the configured identity provider",
                ));
            }
            let member = membership
                .members
                .iter()
                .find(|member| member.subject == subject)
                .ok_or(Error::Invalid(
                    "Reviewer must be a current repository member",
                ))?;
            (
                RepositoryAuthor {
                    issuer: actor.issuer.clone(),
                    subject: member.subject.clone(),
                    name: member.name.clone(),
                },
                true,
            )
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
            (request.reviewer, false)
        }
    };
    let outcome = app::command_output(
        cell.client
            .command::<SetReviewRequest>(
                &cell.target,
                app::mutation_identity()?,
                Selection {
                    pull: number,
                    version,
                    actor: (&actor).into(),
                    can_manage: membership.can_write,
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
    Ok(Json(view(&request, &membership.members, &actor)))
}

#[cfg(test)]
mod tests;
