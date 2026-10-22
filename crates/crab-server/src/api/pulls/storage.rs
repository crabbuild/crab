use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    app::{Error, Result},
    auth::Identity,
    cells::repository::{
        CreatePull, CreatePullComment, CreatePullCommentInput, CreatePullCommentOutcome,
        CreatePullInput, CreatePullOutcome, CreatePullReview, CreatePullReviewInput,
        CreatePullReviewOutcome, GetPull, GetPullComment, GetPullMergeSubmission, GetPullReview,
        GetPullReviewSubmission, GetPullSubmission, ListPullComments, ListPullReviews,
        PullChildKey, PullChildListInput, PullMergeTransition, PullRecord, PullSubmissionKey,
        ReservePullMerge, ReservePullMergeInput, ReservePullMergeOutcome, TransitionPullMerge,
        TransitionPullMergeInput, TransitionPullMergeOutcome, UpdatePull, UpdatePullComment,
        UpdatePullCommentInput, UpdatePullCommentOutcome, UpdatePullInput, UpdatePullOutcome,
        UpdatePullReview, UpdatePullReviewInput, UpdatePullReviewOutcome,
    },
    repository::Repository,
    server::Server,
};

pub(super) use crate::cells::repository::{MergeMethod, PullState, ReviewState};

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
    pub draft: bool,
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
    pub label_ids: Vec<u64>,
    pub assignees: Vec<crate::cells::repository::assignments::Assignee>,
    pub merge_pending: Option<PullMerge>,
    pub merge: Option<PullMerge>,
    pub review_decisions: Vec<PullReviewDecision>,
    pub version: u64,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct PullComment {
    pub number: u64,
    pub author: Identity,
    pub body: String,
    pub version: u64,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct PullReview {
    pub number: u64,
    pub author: Identity,
    pub body: String,
    pub state: ReviewState,
    pub commit_oid: String,
    pub version: u64,
    pub created_at: u64,
    pub updated_at: u64,
}

pub(super) struct NewPullRequest {
    pub draft: bool,
    pub author: Identity,
    pub request_id: String,
    pub title: String,
    pub body: String,
    pub base_ref: String,
    pub base_oid: String,
    pub head_ref: String,
    pub head_oid: String,
}

pub(super) struct PullEdit {
    pub actor: Identity,
    pub can_triage: bool,
    pub version: u64,
    pub title: Option<String>,
    pub body: Option<String>,
    pub state: Option<PullState>,
    pub label_ids: Option<Vec<u64>>,
    pub assignees: Option<Vec<crate::cells::repository::assignments::Assignee>>,
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

pub(super) struct PullDetail {
    pub pull: PullRequest,
    pub lifecycle: crate::cells::repository::PullLifecycle,
}
pub(super) async fn detail(
    server: &Server,
    repo: &Repository,
    number: u64,
) -> Result<Option<PullDetail>> {
    let routed = crate::app::repository_cell(server, repo)?;
    let detail = crate::app::query_output(
        routed
            .client
            .query::<crate::cells::repository::GetPullDetail>(&routed.target, None, number)
            .await,
    )?;
    Ok(detail.map(|value| PullDetail {
        pull: from_pull(value.pull),
        lifecycle: value.lifecycle,
    }))
}

pub(super) async fn pull(
    server: &Server,
    repo: &Repository,
    number: u64,
) -> Result<Option<PullRequest>> {
    let routed = crate::app::repository_cell(server, repo)?;
    crate::app::query_output(
        routed
            .client
            .query::<GetPull>(&routed.target, None, number)
            .await,
    )
    .map(|pull| pull.map(from_pull))
}

pub(super) async fn pull_submission(
    server: &Server,
    repo: &Repository,
    request_id: &str,
) -> Result<Option<PullRequest>> {
    let routed = crate::app::repository_cell(server, repo)?;
    crate::app::query_output(
        routed
            .client
            .query::<GetPullSubmission>(
                &routed.target,
                None,
                PullSubmissionKey {
                    pull: None,
                    submission_id: submission_id(request_id)?,
                },
            )
            .await,
    )
    .map(|pull| pull.map(from_pull))
}

pub(super) async fn create_pull(
    server: &Server,
    repo: &Repository,
    input: NewPullRequest,
) -> Result<PullRequest> {
    let routed = crate::app::repository_cell(server, repo)?;
    match crate::app::command_output(
        routed
            .client
            .command::<CreatePull>(
                &routed.target,
                crate::app::mutation_identity()?,
                CreatePullInput {
                    draft: input.draft,
                    submission_id: submission_id(&input.request_id)?,
                    author: crate::cells::repository::RepositoryAuthor::from(&input.author),
                    title: input.title,
                    body: input.body,
                    base_ref: input.base_ref,
                    base_oid: input.base_oid,
                    head_ref: input.head_ref,
                    head_oid: input.head_oid,
                },
            )
            .await,
    )? {
        CreatePullOutcome::Archived => Err(Error::Archived),
        CreatePullOutcome::Created(pull) => Ok(from_pull(*pull)),
        CreatePullOutcome::RequestConflict => Err(Error::RequestConflict),
    }
}

pub(super) async fn update_pull(
    server: &Server,
    repo: &Repository,
    number: u64,
    input: PullEdit,
) -> Result<PullRequest> {
    let routed = crate::app::repository_cell(server, repo)?;
    match crate::app::command_output(
        routed
            .client
            .command::<UpdatePull>(
                &routed.target,
                crate::app::mutation_identity()?,
                UpdatePullInput {
                    number,
                    actor: crate::cells::repository::RepositoryAuthor::from(&input.actor),
                    can_triage: input.can_triage,
                    version: input.version,
                    title: input.title,
                    body: input.body,
                    state: input.state,
                    label_ids: input.label_ids,
                    assignees: input.assignees,
                },
            )
            .await,
    )? {
        UpdatePullOutcome::Archived => Err(Error::Archived),
        UpdatePullOutcome::Updated(pull) => Ok(from_pull(*pull)),
        UpdatePullOutcome::NotFound => Err(Error::NotFound),
        UpdatePullOutcome::Forbidden => Err(Error::Forbidden),
        UpdatePullOutcome::MergePending => Err(Error::MergePending),
        UpdatePullOutcome::InvalidState => {
            Err(Error::Invalid("Merged pull requests cannot change state"))
        }
        UpdatePullOutcome::InvalidLabel => Err(Error::Invalid(
            "Label selection contains an unknown or deleted label",
        )),
        UpdatePullOutcome::Conflict => Err(Error::Conflict),
    }
}

pub(super) async fn comments(
    server: &Server,
    repo: &Repository,
    pull: u64,
    before: Option<u64>,
    limit: u8,
) -> Result<Option<(Vec<PullComment>, Option<u64>)>> {
    let routed = crate::app::repository_cell(server, repo)?;
    let page = crate::app::query_output(
        routed
            .client
            .query::<ListPullComments>(
                &routed.target,
                None,
                PullChildListInput {
                    pull,
                    before,
                    limit,
                },
            )
            .await,
    )?;
    Ok(page.map(|page| {
        (
            page.items.into_iter().map(from_comment).collect(),
            page.next,
        )
    }))
}

pub(super) async fn comment(
    server: &Server,
    repo: &Repository,
    pull: u64,
    number: u64,
) -> Result<Option<PullComment>> {
    let routed = crate::app::repository_cell(server, repo)?;
    crate::app::query_output(
        routed
            .client
            .query::<GetPullComment>(&routed.target, None, PullChildKey { pull, number })
            .await,
    )
    .map(|value| value.map(from_comment))
}

pub(super) async fn create_comment(
    server: &Server,
    repo: &Repository,
    pull: u64,
    author: Identity,
    request_id: String,
    body: String,
) -> Result<PullComment> {
    let routed = crate::app::repository_cell(server, repo)?;
    match crate::app::command_output(
        routed
            .client
            .command::<CreatePullComment>(
                &routed.target,
                crate::app::mutation_identity()?,
                CreatePullCommentInput {
                    pull,
                    submission_id: submission_id(&request_id)?,
                    author: crate::cells::repository::RepositoryAuthor::from(&author),
                    body,
                },
            )
            .await,
    )? {
        CreatePullCommentOutcome::Archived => Err(Error::Archived),
        CreatePullCommentOutcome::Created(comment) => Ok(from_comment(comment)),
        CreatePullCommentOutcome::PullNotFound => Err(Error::NotFound),
        CreatePullCommentOutcome::RequestConflict => Err(Error::RequestConflict),
    }
}

pub(super) async fn update_comment(
    server: &Server,
    repo: &Repository,
    pull: u64,
    number: u64,
    actor: Identity,
    version: u64,
    body: String,
) -> Result<PullComment> {
    let routed = crate::app::repository_cell(server, repo)?;
    match crate::app::command_output(
        routed
            .client
            .command::<UpdatePullComment>(
                &routed.target,
                crate::app::mutation_identity()?,
                UpdatePullCommentInput {
                    key: PullChildKey { pull, number },
                    actor: crate::cells::repository::RepositoryAuthor::from(&actor),
                    version,
                    body,
                },
            )
            .await,
    )? {
        UpdatePullCommentOutcome::Archived => Err(Error::Archived),
        UpdatePullCommentOutcome::Updated(comment) => Ok(from_comment(comment)),
        UpdatePullCommentOutcome::NotFound => Err(Error::NotFound),
        UpdatePullCommentOutcome::Forbidden => Err(Error::Forbidden),
        UpdatePullCommentOutcome::Conflict => Err(Error::Conflict),
    }
}

pub(super) async fn reviews(
    server: &Server,
    repo: &Repository,
    pull: u64,
    before: Option<u64>,
    limit: u8,
) -> Result<Option<(Vec<PullReview>, Option<u64>)>> {
    let routed = crate::app::repository_cell(server, repo)?;
    let page = crate::app::query_output(
        routed
            .client
            .query::<ListPullReviews>(
                &routed.target,
                None,
                PullChildListInput {
                    pull,
                    before,
                    limit,
                },
            )
            .await,
    )?;
    Ok(page.map(|page| (page.items.into_iter().map(from_review).collect(), page.next)))
}

pub(super) async fn review(
    server: &Server,
    repo: &Repository,
    pull: u64,
    number: u64,
) -> Result<Option<PullReview>> {
    let routed = crate::app::repository_cell(server, repo)?;
    crate::app::query_output(
        routed
            .client
            .query::<GetPullReview>(&routed.target, None, PullChildKey { pull, number })
            .await,
    )
    .map(|value| value.map(from_review))
}

pub(super) async fn review_submission(
    server: &Server,
    repo: &Repository,
    pull: u64,
    request_id: &str,
) -> Result<Option<PullReview>> {
    let routed = crate::app::repository_cell(server, repo)?;
    crate::app::query_output(
        routed
            .client
            .query::<GetPullReviewSubmission>(
                &routed.target,
                None,
                PullSubmissionKey {
                    pull: Some(pull),
                    submission_id: submission_id(request_id)?,
                },
            )
            .await,
    )
    .map(|value| value.map(from_review))
}

pub(super) async fn create_review(
    server: &Server,
    repo: &Repository,
    pull: u64,
    input: NewPullReview,
) -> Result<PullReview> {
    let routed = crate::app::repository_cell(server, repo)?;
    match crate::app::command_output(
        routed
            .client
            .command::<CreatePullReview>(
                &routed.target,
                crate::app::mutation_identity()?,
                CreatePullReviewInput {
                    pull,
                    submission_id: submission_id(&input.request_id)?,
                    author: crate::cells::repository::RepositoryAuthor::from(&input.author),
                    body: input.body,
                    state: input.state,
                    commit_oid: input.commit_oid,
                },
            )
            .await,
    )? {
        CreatePullReviewOutcome::Archived => Err(Error::Archived),
        CreatePullReviewOutcome::Created(review) => Ok(from_review(review)),
        CreatePullReviewOutcome::PullNotFound => Err(Error::NotFound),
        CreatePullReviewOutcome::PullClosed => {
            Err(Error::Invalid("Closed pull requests cannot be reviewed"))
        }
        CreatePullReviewOutcome::MergePending => Err(Error::MergePending),
        CreatePullReviewOutcome::OwnReview => Err(Error::OwnReview),
        CreatePullReviewOutcome::DecisionLimit => Err(Error::Invalid(
            "Pull requests support at most 96 review decisions",
        )),
        CreatePullReviewOutcome::RequestConflict => Err(Error::RequestConflict),
    }
}

pub(super) async fn update_review(
    server: &Server,
    repo: &Repository,
    pull: u64,
    number: u64,
    actor: Identity,
    version: u64,
    body: String,
) -> Result<PullReview> {
    let routed = crate::app::repository_cell(server, repo)?;
    match crate::app::command_output(
        routed
            .client
            .command::<UpdatePullReview>(
                &routed.target,
                crate::app::mutation_identity()?,
                UpdatePullReviewInput {
                    key: PullChildKey { pull, number },
                    actor: crate::cells::repository::RepositoryAuthor::from(&actor),
                    version,
                    body,
                },
            )
            .await,
    )? {
        UpdatePullReviewOutcome::Archived => Err(Error::Archived),
        UpdatePullReviewOutcome::Updated(review) => Ok(from_review(review)),
        UpdatePullReviewOutcome::NotFound => Err(Error::NotFound),
        UpdatePullReviewOutcome::Forbidden => Err(Error::Forbidden),
        UpdatePullReviewOutcome::Conflict => Err(Error::Conflict),
    }
}

pub(super) async fn recover_merge(
    server: &Server,
    repo: &Repository,
    input: &NewPullMerge,
    pull: u64,
) -> Result<Option<PullMerge>> {
    let routed = crate::app::repository_cell(server, repo)?;
    let merge = crate::app::query_output(
        routed
            .client
            .query::<GetPullMergeSubmission>(
                &routed.target,
                None,
                PullSubmissionKey {
                    pull: Some(pull),
                    submission_id: submission_id(&input.request_id)?,
                },
            )
            .await,
    )?;
    match merge {
        Some(record) if merge_matches_cell(&record, input) => Ok(Some(from_merge(record))),
        Some(_) => Err(Error::RequestConflict),
        None => Ok(None),
    }
}

pub(super) async fn reserve_merge(
    server: &Server,
    repo: &Repository,
    pull: u64,
    input: &NewPullMerge,
    commit_oid: gix_hash::ObjectId,
    created_at: u64,
) -> Result<PullMerge> {
    let routed = crate::app::repository_cell(server, repo)?;
    let merge = to_merge(input, commit_oid.to_string(), created_at)?;
    match crate::app::command_output(
        routed
            .client
            .command::<ReservePullMerge>(
                &routed.target,
                crate::app::mutation_identity()?,
                ReservePullMergeInput { pull, merge },
            )
            .await,
    )? {
        ReservePullMergeOutcome::Draft => Err(Error::DraftPull),
        ReservePullMergeOutcome::Archived => Err(Error::Archived),
        ReservePullMergeOutcome::Reserved(merge) => Ok(from_merge(*merge)),
        ReservePullMergeOutcome::PullNotFound => Err(Error::NotFound),
        ReservePullMergeOutcome::RequestConflict => Err(Error::RequestConflict),
    }
}

pub(super) async fn begin_merge(
    server: &Server,
    repo: &Repository,
    pull: u64,
    record: &PullMerge,
) -> Result<PullRequest> {
    transition_merge(server, repo, pull, record, PullMergeTransition::Begin).await
}
pub(super) async fn abort_merge(
    server: &Server,
    repo: &Repository,
    pull: u64,
    record: &PullMerge,
) -> Result<()> {
    transition_merge(server, repo, pull, record, PullMergeTransition::Abort)
        .await
        .map(drop)
}
pub(super) async fn complete_merge(
    server: &Server,
    repo: &Repository,
    pull: u64,
    record: &PullMerge,
) -> Result<PullRequest> {
    transition_merge(server, repo, pull, record, PullMergeTransition::Complete).await
}

async fn transition_merge(
    server: &Server,
    repo: &Repository,
    pull: u64,
    record: &PullMerge,
    transition: PullMergeTransition,
) -> Result<PullRequest> {
    let routed = crate::app::repository_cell(server, repo)?;
    match crate::app::command_output(
        routed
            .client
            .command::<TransitionPullMerge>(
                &routed.target,
                crate::app::mutation_identity()?,
                TransitionPullMergeInput {
                    pull,
                    merge: cell_merge(record)?,
                    transition,
                },
            )
            .await,
    )? {
        TransitionPullMergeOutcome::Draft => Err(Error::DraftPull),
        TransitionPullMergeOutcome::Archived => Err(Error::Archived),
        TransitionPullMergeOutcome::Applied(pull) => Ok(from_pull(*pull)),
        TransitionPullMergeOutcome::NotFound => Err(Error::NotFound),
        TransitionPullMergeOutcome::Conflict => Err(Error::MergeConflict),
    }
}

fn merge_matches_cell(left: &crate::cells::repository::PullMerge, input: &NewPullMerge) -> bool {
    left.request_id == submission_id(&input.request_id).unwrap_or([0; 16])
        && left.method == input.method
        && left.pull_version == input.pull_version
        && left.base_oid == input.base_oid
        && left.head_oid == input.head_oid
        && left.message == input.message
}

pub(super) fn from_author(value: crate::cells::repository::RepositoryAuthor) -> Identity {
    Identity {
        issuer: value.issuer,
        subject: value.subject,
        name: value.name,
    }
}
fn from_merge(value: crate::cells::repository::PullMerge) -> PullMerge {
    PullMerge {
        request_id: Uuid::from_bytes(value.request_id).to_string(),
        author: from_author(value.author),
        method: value.method,
        pull_version: value.pull_version,
        base_oid: value.base_oid,
        head_oid: value.head_oid,
        commit_oid: value.commit_oid,
        message: value.message,
        created_at: value.created_at_ms,
    }
}
fn cell_merge(value: &PullMerge) -> Result<crate::cells::repository::PullMerge> {
    Ok(crate::cells::repository::PullMerge {
        request_id: submission_id(&value.request_id)?,
        author: crate::cells::repository::RepositoryAuthor::from(&value.author),
        method: value.method,
        pull_version: value.pull_version,
        base_oid: value.base_oid.clone(),
        head_oid: value.head_oid.clone(),
        commit_oid: value.commit_oid.clone(),
        message: value.message.clone(),
        created_at_ms: value.created_at,
    })
}
fn to_merge(
    input: &NewPullMerge,
    commit_oid: String,
    created_at_ms: u64,
) -> Result<crate::cells::repository::PullMerge> {
    Ok(crate::cells::repository::PullMerge {
        request_id: submission_id(&input.request_id)?,
        author: crate::cells::repository::RepositoryAuthor::from(&input.author),
        method: input.method,
        pull_version: input.pull_version,
        base_oid: input.base_oid.clone(),
        head_oid: input.head_oid.clone(),
        commit_oid,
        message: input.message.clone(),
        created_at_ms,
    })
}
pub(super) fn from_pull(value: PullRecord) -> PullRequest {
    PullRequest {
        draft: value.draft,
        number: value.number,
        request_id: Uuid::from_bytes(value.create_submission_id).to_string(),
        author: from_author(value.author),
        title: value.title,
        body: value.body,
        state: value.state,
        base_ref: value.base_ref,
        base_oid: value.base_oid,
        head_ref: value.head_ref,
        head_oid: value.head_oid,
        label_ids: value.label_ids,
        assignees: value.assignees,
        merge_pending: value.merge_pending.map(from_merge),
        merge: value.merge.map(from_merge),
        review_decisions: value
            .review_decisions
            .into_iter()
            .map(|decision| PullReviewDecision {
                review: decision.review,
                author: from_author(decision.author),
                state: decision.state,
                commit_oid: decision.commit_oid,
            })
            .collect(),
        version: value.version,
        created_at: value.created_at_ms,
        updated_at: value.updated_at_ms,
    }
}
pub(in crate::api) fn from_comment(
    value: crate::cells::repository::PullCommentRecord,
) -> PullComment {
    PullComment {
        number: value.number,
        author: from_author(value.author),
        body: value.body,
        version: value.version,
        created_at: value.created_at_ms,
        updated_at: value.updated_at_ms,
    }
}
pub(in crate::api) fn from_review(value: crate::cells::repository::PullReviewRecord) -> PullReview {
    PullReview {
        number: value.number,
        author: from_author(value.author),
        body: value.body,
        state: value.state,
        commit_oid: value.commit_oid,
        version: value.version,
        created_at: value.created_at_ms,
        updated_at: value.updated_at_ms,
    }
}

pub(super) fn submission_id(value: &str) -> Result<[u8; 16]> {
    Uuid::parse_str(value)
        .map(Uuid::into_bytes)
        .map_err(|_| Error::Invalid("Submission ID must be a UUID"))
}
