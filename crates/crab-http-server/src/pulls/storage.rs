use serde::{Deserialize, Serialize};

use crate::{
    app::{Error, Result},
    app_storage,
    auth::Identity,
    server::Repository,
};

pub(super) const ROOT: &str = "app/v1/pulls";

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum PullState {
    #[default]
    Open,
    Closed,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum ReviewState {
    Commented,
    Approved,
    ChangesRequested,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PullRequest {
    pub number: u64,
    pub request_id: String,
    pub author: Identity,
    pub title: String,
    pub body: String,
    pub state: PullState,
    pub base_ref: String,
    pub base_oid: String,
    pub head_ref: String,
    pub head_oid: String,
    pub version: u64,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PullComment {
    pub number: u64,
    pub request_id: String,
    pub author: Identity,
    pub body: String,
    pub version: u64,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PullReview {
    pub number: u64,
    pub request_id: String,
    pub author: Identity,
    pub body: String,
    pub state: ReviewState,
    pub commit_oid: String,
    pub version: u64,
    pub created_at: u64,
    pub updated_at: u64,
}

pub(super) struct NewPullRequest {
    pub author: Identity,
    pub request_id: String,
    pub title: String,
    pub body: String,
    pub base_ref: String,
    pub base_oid: String,
    pub head_ref: String,
    pub head_oid: String,
}

pub(super) struct NewPullReview {
    pub author: Identity,
    pub request_id: String,
    pub body: String,
    pub state: ReviewState,
    pub commit_oid: String,
}

pub(super) fn pull_path(number: u64) -> String {
    format!("{ROOT}/{number:016}/pull.json")
}

pub(super) fn comments_root(number: u64) -> String {
    format!("{ROOT}/{number:016}/comments")
}

pub(super) fn comment_path(pull: u64, comment: u64) -> String {
    format!("{}/{comment:016}.json", comments_root(pull))
}

pub(super) fn reviews_root(number: u64) -> String {
    format!("{ROOT}/{number:016}/reviews")
}

pub(super) fn review_path(pull: u64, review: u64) -> String {
    format!("{}/{review:016}.json", reviews_root(pull))
}

pub(super) async fn recover_pull(
    repo: &Repository,
    author: &Identity,
    request_id: &str,
    title: &str,
    body: &str,
    base_ref: &str,
    head_ref: &str,
) -> Result<Option<PullRequest>> {
    let reservation = format!("{ROOT}/requests/{request_id}.json");
    let Some((pull, _)) = app_storage::read::<PullRequest>(repo, &reservation).await? else {
        return Ok(None);
    };
    if !app_storage::same_author(&pull.author, author)
        || pull.title != title
        || pull.body != body
        || pull.base_ref != base_ref
        || pull.head_ref != head_ref
    {
        return Err(Error::RequestConflict);
    }
    Ok(Some(
        app_storage::create_or_read(repo, &pull_path(pull.number), pull).await?,
    ))
}

pub(super) async fn create_pull(repo: &Repository, input: NewPullRequest) -> Result<PullRequest> {
    let reservation = format!("{ROOT}/requests/{}.json", input.request_id);
    let original = match app_storage::read::<PullRequest>(repo, &reservation).await? {
        Some((pull, _)) => pull,
        None => {
            let number = app_storage::reserve_number(repo, ROOT).await?;
            let timestamp = app_storage::now()?;
            app_storage::create_or_read(
                repo,
                &reservation,
                PullRequest {
                    number,
                    request_id: input.request_id.clone(),
                    author: input.author.clone(),
                    title: input.title.clone(),
                    body: input.body.clone(),
                    state: PullState::Open,
                    base_ref: input.base_ref.clone(),
                    base_oid: input.base_oid.clone(),
                    head_ref: input.head_ref.clone(),
                    head_oid: input.head_oid.clone(),
                    version: 1,
                    created_at: timestamp,
                    updated_at: timestamp,
                },
            )
            .await?
        }
    };
    if !app_storage::same_author(&original.author, &input.author)
        || original.title != input.title
        || original.body != input.body
        || original.base_ref != input.base_ref
        || original.head_ref != input.head_ref
    {
        return Err(Error::RequestConflict);
    }
    // The immutable reservation makes a retry converge after an uncertain response.
    let current = app_storage::create_or_read(repo, &pull_path(original.number), original).await?;
    if current.request_id != input.request_id {
        return Err(Error::Conflict);
    }
    Ok(current)
}

pub(super) async fn create_comment(
    repo: &Repository,
    pull: u64,
    author: Identity,
    request_id: String,
    body: String,
) -> Result<PullComment> {
    let root = comments_root(pull);
    let reservation = format!("{root}/requests/{request_id}.json");
    let original = match app_storage::read::<PullComment>(repo, &reservation).await? {
        Some((comment, _)) => comment,
        None => {
            let number = app_storage::reserve_number(repo, &root).await?;
            let timestamp = app_storage::now()?;
            app_storage::create_or_read(
                repo,
                &reservation,
                PullComment {
                    number,
                    request_id: request_id.clone(),
                    author: author.clone(),
                    body: body.clone(),
                    version: 1,
                    created_at: timestamp,
                    updated_at: timestamp,
                },
            )
            .await?
        }
    };
    if !app_storage::same_author(&original.author, &author) || original.body != body {
        return Err(Error::RequestConflict);
    }
    let current =
        app_storage::create_or_read(repo, &comment_path(pull, original.number), original).await?;
    if current.request_id != request_id {
        return Err(Error::Conflict);
    }
    Ok(current)
}

pub(super) async fn recover_review(
    repo: &Repository,
    pull: u64,
    author: &Identity,
    request_id: &str,
    body: &str,
    state: ReviewState,
) -> Result<Option<PullReview>> {
    let root = reviews_root(pull);
    let reservation = format!("{root}/requests/{request_id}.json");
    let Some((review, _)) = app_storage::read::<PullReview>(repo, &reservation).await? else {
        return Ok(None);
    };
    if !app_storage::same_author(&review.author, author)
        || review.body != body
        || review.state != state
    {
        return Err(Error::RequestConflict);
    }
    Ok(Some(
        app_storage::create_or_read(repo, &review_path(pull, review.number), review).await?,
    ))
}

pub(super) async fn create_review(
    repo: &Repository,
    pull: u64,
    input: NewPullReview,
) -> Result<PullReview> {
    let root = reviews_root(pull);
    let reservation = format!("{root}/requests/{}.json", input.request_id);
    let original = match app_storage::read::<PullReview>(repo, &reservation).await? {
        Some((review, _)) => review,
        None => {
            let number = app_storage::reserve_number(repo, &root).await?;
            let timestamp = app_storage::now()?;
            app_storage::create_or_read(
                repo,
                &reservation,
                PullReview {
                    number,
                    request_id: input.request_id.clone(),
                    author: input.author.clone(),
                    body: input.body.clone(),
                    state: input.state,
                    commit_oid: input.commit_oid.clone(),
                    version: 1,
                    created_at: timestamp,
                    updated_at: timestamp,
                },
            )
            .await?
        }
    };
    if !app_storage::same_author(&original.author, &input.author)
        || original.body != input.body
        || original.state != input.state
    {
        return Err(Error::RequestConflict);
    }
    let current =
        app_storage::create_or_read(repo, &review_path(pull, original.number), original).await?;
    if current.request_id != input.request_id {
        return Err(Error::Conflict);
    }
    Ok(current)
}
