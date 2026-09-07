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
    Merged,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum ReviewState {
    Commented,
    Approved,
    ChangesRequested,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum MergeMethod {
    FastForward,
    MergeCommit,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PullMerge {
    pub request_id: String,
    pub author: Identity,
    pub method: MergeMethod,
    pub pull_version: u64,
    pub base_oid: String,
    pub head_oid: String,
    pub commit_oid: String,
    #[serde(default)]
    pub message: String,
    pub created_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PullReviewDecision {
    pub review: u64,
    pub author: Identity,
    pub state: ReviewState,
    pub commit_oid: String,
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
    #[serde(default)]
    pub label_ids: Vec<u64>,
    #[serde(default)]
    pub assignee_subjects: Vec<String>,
    #[serde(default)]
    pub merge_pending: Option<PullMerge>,
    #[serde(default)]
    pub merge: Option<PullMerge>,
    #[serde(default)]
    pub review_decisions: Vec<PullReviewDecision>,
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

pub(super) struct NewPullMerge {
    pub author: Identity,
    pub request_id: String,
    pub method: MergeMethod,
    pub pull_version: u64,
    pub base_oid: String,
    pub head_oid: String,
    pub message: String,
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

fn merge_request_path(pull: u64, request_id: &str) -> String {
    format!("{ROOT}/{pull:016}/merge/requests/{request_id}.json")
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
                    label_ids: vec![],
                    assignee_subjects: vec![],
                    merge_pending: None,
                    merge: None,
                    review_decisions: vec![],
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

pub(super) async fn record_review_decision(
    repo: &Repository,
    number: u64,
    review: &PullReview,
) -> Result<()> {
    if review.state == ReviewState::Commented {
        return Ok(());
    }
    let path = pull_path(number);
    for _ in 0..10 {
        let (mut pull, etag) = app_storage::read::<PullRequest>(repo, &path)
            .await?
            .ok_or(Error::NotFound)?;
        if pull.review_decisions.iter().any(|current| {
            app_storage::same_author(&current.author, &review.author)
                && current.review >= review.number
        }) {
            return Ok(());
        }
        if pull.state != PullState::Open {
            return Err(Error::Invalid("Closed pull requests cannot be reviewed"));
        }
        if pull.merge_pending.is_some() {
            return Err(Error::MergePending);
        }
        let decision = PullReviewDecision {
            review: review.number,
            author: review.author.clone(),
            state: review.state,
            commit_oid: review.commit_oid.clone(),
        };
        match pull
            .review_decisions
            .iter()
            .position(|current| app_storage::same_author(&current.author, &review.author))
        {
            Some(index) => pull.review_decisions[index] = decision,
            None if pull.review_decisions.len() < 1024 => pull.review_decisions.push(decision),
            None => {
                return Err(Error::Invalid(
                    "Pull requests support at most 1,024 review decisions",
                ));
            }
        }
        // Merge admission compares this version before claiming the pull. A
        // concurrent review decision must either precede or follow that claim.
        pull.version = pull
            .version
            .checked_add(1)
            .filter(|value| *value < app_storage::MAX_NUMBER)
            .ok_or(Error::Conflict)?;
        pull.updated_at = app_storage::now()?;
        match app_storage::update(repo, &path, &pull, etag).await {
            Ok(()) => return Ok(()),
            Err(Error::Storage(crab_storage::StorageError::StateConflict { .. })) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(Error::Conflict)
}

pub(super) fn merge_matches(left: &PullMerge, input: &NewPullMerge) -> bool {
    app_storage::same_author(&left.author, &input.author)
        && left.request_id == input.request_id
        && left.method == input.method
        && left.pull_version == input.pull_version
        && left.base_oid == input.base_oid
        && left.head_oid == input.head_oid
        && left.message == input.message
}

pub(super) async fn recover_merge(
    repo: &Repository,
    pull: u64,
    input: &NewPullMerge,
) -> Result<Option<PullMerge>> {
    let path = merge_request_path(pull, &input.request_id);
    let Some((record, _)) = app_storage::read::<PullMerge>(repo, &path).await? else {
        return Ok(None);
    };
    if !merge_matches(&record, input) {
        return Err(Error::RequestConflict);
    }
    Ok(Some(record))
}

pub(super) async fn reserve_merge(
    repo: &Repository,
    pull: u64,
    input: &NewPullMerge,
    commit_oid: gix_hash::ObjectId,
    created_at: u64,
) -> Result<PullMerge> {
    let path = merge_request_path(pull, &input.request_id);
    let proposed = PullMerge {
        request_id: input.request_id.clone(),
        author: input.author.clone(),
        method: input.method,
        pull_version: input.pull_version,
        base_oid: input.base_oid.clone(),
        head_oid: input.head_oid.clone(),
        commit_oid: commit_oid.to_string(),
        message: input.message.clone(),
        created_at,
    };
    let record = app_storage::create_or_read(repo, &path, proposed).await?;
    if !merge_matches(&record, input) {
        return Err(Error::RequestConflict);
    }
    Ok(record)
}

pub(super) async fn complete_merge(
    repo: &Repository,
    number: u64,
    record: &PullMerge,
) -> Result<PullRequest> {
    let path = pull_path(number);
    for _ in 0..10 {
        let (mut pull, etag) = app_storage::read::<PullRequest>(repo, &path)
            .await?
            .ok_or(Error::NotFound)?;
        if let Some(existing) = &pull.merge {
            if existing.request_id == record.request_id {
                return Ok(pull);
            }
            return Err(Error::MergeConflict);
        }
        if pull
            .merge_pending
            .as_ref()
            .is_none_or(|pending| pending.request_id != record.request_id)
        {
            return Err(Error::MergeConflict);
        }
        pull.state = PullState::Merged;
        pull.merge_pending = None;
        pull.merge = Some(record.clone());
        pull.version = pull
            .version
            .checked_add(1)
            .filter(|value| *value < app_storage::MAX_NUMBER)
            .ok_or(Error::Conflict)?;
        pull.updated_at = app_storage::now()?;
        match app_storage::update(repo, &path, &pull, etag).await {
            Ok(()) => return Ok(pull),
            Err(Error::Storage(crab_storage::StorageError::StateConflict { .. })) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(Error::Conflict)
}

pub(super) async fn begin_merge(
    repo: &Repository,
    number: u64,
    record: &PullMerge,
) -> Result<PullRequest> {
    let path = pull_path(number);
    for _ in 0..10 {
        let (mut pull, etag) = app_storage::read::<PullRequest>(repo, &path)
            .await?
            .ok_or(Error::NotFound)?;
        if let Some(existing) = &pull.merge {
            if existing.request_id == record.request_id {
                return Ok(pull);
            }
            return Err(Error::MergeConflict);
        }
        if let Some(pending) = &pull.merge_pending {
            if pending.request_id == record.request_id {
                return Ok(pull);
            }
            return Err(Error::MergeConflict);
        }
        if pull.state != PullState::Open || pull.version != record.pull_version {
            return Err(Error::MergeConflict);
        }
        pull.merge_pending = Some(record.clone());
        pull.version = pull
            .version
            .checked_add(1)
            .filter(|value| *value < app_storage::MAX_NUMBER)
            .ok_or(Error::Conflict)?;
        pull.updated_at = app_storage::now()?;
        match app_storage::update(repo, &path, &pull, etag).await {
            Ok(()) => return Ok(pull),
            Err(Error::Storage(crab_storage::StorageError::StateConflict { .. })) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(Error::Conflict)
}

pub(super) async fn abort_merge(repo: &Repository, number: u64, record: &PullMerge) -> Result<()> {
    let path = pull_path(number);
    for _ in 0..10 {
        let (mut pull, etag) = app_storage::read::<PullRequest>(repo, &path)
            .await?
            .ok_or(Error::NotFound)?;
        if pull.merge.is_some() {
            return Ok(());
        }
        if pull
            .merge_pending
            .as_ref()
            .is_none_or(|pending| pending.request_id != record.request_id)
        {
            return Ok(());
        }
        pull.merge_pending = None;
        pull.version = pull
            .version
            .checked_add(1)
            .filter(|value| *value < app_storage::MAX_NUMBER)
            .ok_or(Error::Conflict)?;
        pull.updated_at = app_storage::now()?;
        match app_storage::update(repo, &path, &pull, etag).await {
            Ok(()) => return Ok(()),
            Err(Error::Storage(crab_storage::StorageError::StateConflict { .. })) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(Error::Conflict)
}
