use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::storage;
use crate::cells::repository::review_dismissals::Detail;
use crate::{
    api::{profiles::attribution::Authors, reactions},
    app::{self, Error, Result},
    auth::{Identity, Principal},
    cells::repository::collaboration::Target,
    server::Server,
};
use storage::{NewPullReview, PullRequest, PullState, ReviewState};
mod dismissals;

const MAX_NUMBER: u64 = 9_007_199_254_740_991;

fn same_author(left: &Identity, right: &Identity) -> bool {
    left.issuer == right.issuer && left.subject == right.subject
}

fn configured_protected(repo: &crate::repository::Repository, base_ref: &str) -> bool {
    base_ref.strip_prefix("refs/heads/").is_some_and(|branch| {
        repo.config
            .protected_branches
            .iter()
            .any(|rule| rule.branch == branch)
    })
}

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .merge(dismissals::routes())
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/reviews",
            get(list).post(create),
        )
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/reviews/{review}",
            get(detail).patch(edit),
        )
}

pub(super) fn state(review: ReviewState, dismissed: bool) -> &'static str {
    if dismissed {
        return "dismissed";
    }
    match review {
        ReviewState::Approved => "approved",
        ReviewState::ChangesRequested => "changes_requested",
        ReviewState::Commented => "commented",
    }
}

pub(in crate::api) async fn ruleset_requires_admin(
    server: &Server,
    repo: &crate::repository::Repository,
    principal: &Principal,
    review: Option<&Detail>,
) -> Result<bool> {
    let Some(review) = review.filter(|review| review.dismissible) else {
        return Ok(false);
    };
    if !principal.can_write(repo.id) || principal.can_admin(repo.id) {
        return Ok(false);
    }
    let policy = crate::repository::settings::load_ref_policy(server, repo).await?;
    let default_ref = policy.default_ref(repo).await?;
    Ok(policy
        .active()?
        .matches(&review.base_ref, &default_ref)
        .any(|(_, rule)| rule.requires_pull()))
}

pub(in crate::api) fn review_view(
    authors: &Authors,
    detail: &Detail,
    actor: &Identity,
    current_head: Option<&str>,
    reactions: &reactions::View,
    principal: &Principal,
    repo: &crate::repository::Repository,
    ruleset_protected: bool,
) -> Value {
    let review = &detail.review;
    let state = state(review.state, detail.dismissal.is_some());
    let admin_required = ruleset_protected
        || detail
            .admin_required
            .unwrap_or_else(|| configured_protected(repo, &detail.base_ref));
    json!({
        "number": review.number,
        "author": authors.view(&review.author),
        "body": review.body,
        "reactions": reactions,
        "state": state,
        "dismissal": detail.dismissal.as_ref().map(|value| json!({
            "author": authors.view(&value.author),
            "body": value.body,
            "created_at": value.created_at_ms,
        })),
        "can_dismiss": detail.dismissible && principal.can_write(repo.id) && (!admin_required || principal.can_admin(repo.id)),
        "commit_oid": review.commit_oid,
        "current": current_head.map(|oid| oid == review.commit_oid),
        "version": review.version,
        "created_at": review.created_at_ms,
        "updated_at": review.updated_at_ms,
        "can_edit": review.author.issuer==actor.issuer && review.author.subject==actor.subject,
    })
}

async fn pull_and_head(
    server: &Server,
    repo: &crate::repository::Repository,
    id: u64,
) -> Result<(PullRequest, Option<String>)> {
    let pull = storage::pull(server, repo, id)
        .await?
        .ok_or(Error::NotFound)?;
    let head = super::review_head(
        server,
        repo,
        pull.head_repository,
        &pull.head_ref,
        pull.merge.as_ref().map(|merge| merge.head_oid.clone()),
    )
    .await?;
    Ok((pull, head))
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ListParameters {
    before: Option<u64>,
    limit: Option<usize>,
}

impl ListParameters {
    fn limit(&self) -> Result<usize> {
        let limit = self.limit.unwrap_or(30);
        if !(1..=50).contains(&limit)
            || self
                .before
                .is_some_and(|value| value == 0 || value > MAX_NUMBER)
        {
            return Err(Error::Invalid("Invalid review page"));
        }
        Ok(limit)
    }
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id)): Path<(String, String, u64)>,
    Query(params): Query<ListParameters>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let id = app::number(id)?;
    let actor = app::actor(&principal)?;
    let (_, head) = pull_and_head(&server, repo, id).await?;
    let limit = u8::try_from(params.limit()?).map_err(|_| Error::Invalid("Invalid review page"))?;
    let (reviews, next) = storage::reviews(&server, repo, id, params.before, limit)
        .await?
        .ok_or(Error::NotFound)?;
    let targets = reviews
        .iter()
        .map(|review| Target::PullReview {
            pull: id,
            number: review.review.number,
        })
        .collect();
    let reactions = reactions::read(&server, repo, &actor, targets).await?;
    let actors: Vec<_> = reviews
        .iter()
        .flat_map(|item| {
            std::iter::once(&item.review.author)
                .chain(item.dismissal.as_ref().map(|value| &value.author))
        })
        .collect();
    let authors = Authors::load(&server, &principal, actors.into_iter()).await?;
    let principal = app::membership(&server, &principal, repo).await?.principal;
    let ruleset_protected = ruleset_requires_admin(
        &server,
        repo,
        &principal,
        reviews.iter().find(|review| review.dismissible),
    )
    .await?;
    let items = reviews
        .iter()
        .zip(reactions)
        .map(|(review, reactions)| {
            review_view(
                &authors,
                review,
                &actor,
                head.as_deref(),
                &reactions,
                &principal,
                repo,
                ruleset_protected,
            )
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({"items":items,"next":next,"head_oid":head})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewReview {
    request_id: String,
    body: String,
    state: ReviewState,
    commit_oid: String,
    base_revision: Option<u64>,
}

fn validate_body(body: &str, state: ReviewState) -> Result<()> {
    app::body(body, state != ReviewState::Approved)
}

async fn create(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id)): Path<(String, String, u64)>,
    input: std::result::Result<Json<NewReview>, JsonRejection>,
) -> Result<impl IntoResponse> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let id = app::number(id)?;
    let Json(input) = input?;
    validate_body(&input.body, input.state)?;
    if input.commit_oid.len() != 40
        || input.commit_oid.bytes().all(|byte| byte == b'0')
        || !input
            .commit_oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Invalid(
            "Review requires an exact lowercase SHA-1 commit ID",
        ));
    }
    let actor = app::actor(&principal)?;
    let request_id = app::submission(&input.request_id)?;
    let replay = storage::review_submission(&server, repo, id, &request_id)
        .await?
        .is_some();
    let (pull, head) = pull_and_head(&server, repo, id).await?;
    if !replay {
        if pull.state != PullState::Open {
            return Err(Error::Invalid("Closed pull requests cannot be reviewed"));
        }
        let current_head = head.as_deref().ok_or(Error::Invalid(
            "Pull request branches must exist before submitting a review",
        ))?;
        // A new review applies only to the commit inspected, never unseen code.
        // A replay is verified by the native ledger against its original payload;
        // the current review body may already have been edited.
        if input.commit_oid != current_head {
            return Err(Error::Conflict);
        }
        if input.state != ReviewState::Commented && same_author(&pull.author, &actor) {
            return Err(Error::OwnReview);
        }
    }
    let review = storage::create_review(
        &server,
        repo,
        &principal,
        id,
        NewPullReview {
            author: actor.clone(),
            request_id,
            body: input.body,
            state: input.state,
            commit_oid: input.commit_oid,
            base_revision: input.base_revision,
        },
    )
    .await?;
    let reaction = reactions::one(
        &server,
        repo,
        &actor,
        Target::PullReview {
            pull: id,
            number: review.number,
        },
    )
    .await?;
    let review = storage::review(&server, repo, id, review.number)
        .await?
        .ok_or(Error::NotFound)?;
    let authors = Authors::load(
        &server,
        &principal,
        std::iter::once(&review.review.author)
            .chain(review.dismissal.as_ref().map(|value| &value.author)),
    )
    .await?;
    let principal = app::membership(&server, &principal, repo).await?.principal;
    let ruleset_protected =
        ruleset_requires_admin(&server, repo, &principal, Some(&review)).await?;
    Ok((
        StatusCode::CREATED,
        Json(review_view(
            &authors,
            &review,
            &actor,
            head.as_deref(),
            &reaction,
            &principal,
            repo,
            ruleset_protected,
        )),
    ))
}

async fn detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id, review)): Path<(String, String, u64, u64)>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let id = app::number(id)?;
    let actor = app::actor(&principal)?;
    let (_, head) = pull_and_head(&server, repo, id).await?;
    let review = storage::review(&server, repo, id, app::number(review)?)
        .await?
        .ok_or(Error::NotFound)?;
    let reaction = reactions::one(
        &server,
        repo,
        &actor,
        Target::PullReview {
            pull: id,
            number: review.review.number,
        },
    )
    .await?;
    let authors = Authors::load(
        &server,
        &principal,
        std::iter::once(&review.review.author)
            .chain(review.dismissal.as_ref().map(|value| &value.author)),
    )
    .await?;
    let principal = app::membership(&server, &principal, repo).await?.principal;
    let ruleset_protected =
        ruleset_requires_admin(&server, repo, &principal, Some(&review)).await?;
    Ok(Json(review_view(
        &authors,
        &review,
        &actor,
        head.as_deref(),
        &reaction,
        &principal,
        repo,
        ruleset_protected,
    )))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewEdit {
    version: u64,
    body: String,
}

async fn edit(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, id, review)): Path<(String, String, u64, u64)>,
    input: std::result::Result<Json<ReviewEdit>, JsonRejection>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let id = app::number(id)?;
    let Json(input) = input?;
    let actor = app::actor(&principal)?;
    let review_number = app::number(review)?;
    let existing = storage::review(&server, repo, id, review_number)
        .await?
        .ok_or(Error::NotFound)?;
    validate_body(&input.body, existing.review.state)?;
    let review = storage::update_review(
        &server,
        repo,
        id,
        review_number,
        actor.clone(),
        input.version,
        input.body,
    )
    .await?;
    let (_, head) = pull_and_head(&server, repo, id).await?;
    let reaction = reactions::one(
        &server,
        repo,
        &actor,
        Target::PullReview {
            pull: id,
            number: review.number,
        },
    )
    .await?;
    let review = storage::review(&server, repo, id, review.number)
        .await?
        .ok_or(Error::NotFound)?;
    let authors = Authors::load(
        &server,
        &principal,
        std::iter::once(&review.review.author)
            .chain(review.dismissal.as_ref().map(|value| &value.author)),
    )
    .await?;
    let principal = app::membership(&server, &principal, repo).await?.principal;
    let ruleset_protected =
        ruleset_requires_admin(&server, repo, &principal, Some(&review)).await?;
    Ok(Json(review_view(
        &authors,
        &review,
        &actor,
        head.as_deref(),
        &reaction,
        &principal,
        repo,
        ruleset_protected,
    )))
}

#[cfg(test)]
mod tests;
