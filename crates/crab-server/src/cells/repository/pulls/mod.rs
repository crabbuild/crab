use serde::{Deserialize, Serialize};

use super::*;

const MAX_REVIEW_DECISIONS: u64 = 96;
const MAX_PAGE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PullState {
    #[default]
    Open,
    Closed,
    Merged,
}

impl PullState {
    const fn code(self) -> u8 {
        match self {
            Self::Open => 0,
            Self::Closed => 1,
            Self::Merged => 2,
        }
    }

    fn from_code(code: u64) -> crate::cells::Result<Self> {
        match code {
            0 => Ok(Self::Open),
            1 => Ok(Self::Closed),
            2 => Ok(Self::Merged),
            _ => Err(crate::cells::Error::Invalid(
                "repository pull state is invalid",
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReviewState {
    Commented,
    Approved,
    ChangesRequested,
}

impl ReviewState {
    const fn code(self) -> u8 {
        match self {
            Self::Commented => 0,
            Self::Approved => 1,
            Self::ChangesRequested => 2,
        }
    }

    fn from_code(code: u64) -> crate::cells::Result<Self> {
        match code {
            0 => Ok(Self::Commented),
            1 => Ok(Self::Approved),
            2 => Ok(Self::ChangesRequested),
            _ => Err(crate::cells::Error::Invalid(
                "repository review state is invalid",
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MergeMethod {
    FastForward,
    MergeCommit,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullMerge {
    pub request_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub method: MergeMethod,
    pub pull_version: u64,
    pub base_oid: String,
    pub head_oid: String,
    pub commit_oid: String,
    pub message: String,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewDecision {
    pub review: u64,
    pub author: RepositoryAuthor,
    pub state: ReviewState,
    pub commit_oid: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullRecord {
    // Durable command receipts contain JSON replies. Pre-readiness replies
    // describe ordinary PRs and must still deserialize during retry recovery.
    #[serde(default)]
    pub draft: bool,
    pub number: u64,
    pub create_submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub title: String,
    pub body: String,
    pub state: PullState,
    pub base_ref: String,
    pub base_oid: String,
    pub head_ref: String,
    pub head_oid: String,
    pub label_ids: Vec<u64>,
    pub assignees: Vec<super::assignments::Assignee>,
    pub merge_pending: Option<PullMerge>,
    pub merge: Option<PullMerge>,
    pub review_decisions: Vec<PullReviewDecision>,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullSummary {
    pub draft: bool,
    pub number: u64,
    pub author: RepositoryAuthor,
    pub title: String,
    pub state: PullState,
    pub base_ref: String,
    pub head_ref: String,
    pub label_ids: Vec<u64>,
    pub assignees: Vec<super::assignments::Assignee>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub version: u64,
    pub closed_at_ms: Option<u64>,
    pub merged_at_ms: Option<u64>,
    pub comment_count: u64,
    pub reaction_count: u64,
}
impl super::search::index::Labelled for PullSummary {
    fn label_ids(&self) -> &[u64] {
        &self.label_ids
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullCommentRecord {
    pub pull: u64,
    pub number: u64,
    pub author: RepositoryAuthor,
    pub body: String,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewRecord {
    pub pull: u64,
    pub number: u64,
    pub author: RepositoryAuthor,
    pub body: String,
    pub state: ReviewState,
    pub commit_oid: String,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullChildKey {
    pub pull: u64,
    pub number: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullSubmissionKey {
    pub pull: Option<u64>,
    pub submission_id: [u8; 16],
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullChildListInput {
    pub pull: u64,
    pub before: Option<u64>,
    pub limit: u8,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullCommentPage {
    pub items: Vec<PullCommentRecord>,
    pub next: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullReviewPage {
    pub items: Vec<PullReviewRecord>,
    pub next: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreatePullInput {
    #[serde(default)]
    pub draft: bool,
    pub submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub title: String,
    pub body: String,
    pub base_ref: String,
    pub base_oid: String,
    pub head_ref: String,
    pub head_oid: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum CreatePullOutcome {
    Archived,
    Created(Box<PullRecord>),
    RequestConflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdatePullInput {
    pub number: u64,
    pub actor: RepositoryAuthor,
    pub can_triage: bool,
    pub version: u64,
    pub title: Option<String>,
    pub body: Option<String>,
    pub state: Option<PullState>,
    pub label_ids: Option<Vec<u64>>,
    pub assignees: Option<Vec<super::assignments::Assignee>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum UpdatePullOutcome {
    Archived,
    Updated(Box<PullRecord>),
    NotFound,
    Forbidden,
    MergePending,
    InvalidState,
    InvalidLabel,
    Conflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreatePullCommentInput {
    pub pull: u64,
    pub submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub body: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum CreatePullCommentOutcome {
    Archived,
    Created(PullCommentRecord),
    PullNotFound,
    RequestConflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdatePullCommentInput {
    pub key: PullChildKey,
    pub actor: RepositoryAuthor,
    pub version: u64,
    pub body: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum UpdatePullCommentOutcome {
    Archived,
    Updated(PullCommentRecord),
    NotFound,
    Forbidden,
    Conflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreatePullReviewInput {
    pub pull: u64,
    pub submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub body: String,
    pub state: ReviewState,
    pub commit_oid: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum CreatePullReviewOutcome {
    Archived,
    Created(PullReviewRecord),
    PullNotFound,
    PullClosed,
    MergePending,
    OwnReview,
    DecisionLimit,
    RequestConflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdatePullReviewInput {
    pub key: PullChildKey,
    pub actor: RepositoryAuthor,
    pub version: u64,
    pub body: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum UpdatePullReviewOutcome {
    Archived,
    Updated(PullReviewRecord),
    NotFound,
    Forbidden,
    Conflict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReservePullMergeInput {
    pub pull: u64,
    pub merge: PullMerge,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum ReservePullMergeOutcome {
    Draft,
    Archived,
    Reserved(Box<PullMerge>),
    PullNotFound,
    RequestConflict,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum PullMergeTransition {
    Begin,
    Abort,
    Complete,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TransitionPullMergeInput {
    pub pull: u64,
    pub merge: PullMerge,
    pub transition: PullMergeTransition,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum TransitionPullMergeOutcome {
    Draft,
    Archived,
    Applied(Box<PullRecord>),
    NotFound,
    Conflict,
}

mod operations;
pub(crate) use operations::{CreatePull, GetPull, GetPullSubmission, ListPulls, UpdatePull};

pub(in crate::cells::repository) mod comments;
pub(crate) use comments::{CreatePullComment, GetPullComment, ListPullComments, UpdatePullComment};

pub(in crate::cells::repository) mod lifecycle;
pub(crate) mod reviewers;
pub(crate) mod rollups;
pub(crate) use lifecycle::{
    GetPullDetail, GetPullLifecycleStatus, PreparePullLifecycle, PullLifecycle,
};

mod reviews;
pub(crate) use reviews::dismissals as review_dismissals;
pub(crate) use reviews::{
    CreatePullReview, GetPullReview, GetPullReviewSubmission, ListPullReviews, UpdatePullReview,
};

mod merge;
pub(crate) use merge::{GetPullMergeSubmission, ReservePullMerge, TransitionPullMerge};

mod storage;
use storage::{
    author_from_row, bounded_child_page, comment_from_row, decode_record, encode_record,
    insert_pull_statement, list_child_rows, load_comment, load_comment_query, load_pull,
    load_pull_query, load_review, load_review_query, next_child_number, next_version,
    pull_child_digest, review_from_row, update_pull_row,
};

use operations::validate_oid;

pub(super) mod review_threads;

#[cfg(test)]
mod tests;

pub(crate) mod review_requests;

pub(in crate::cells::repository) mod readiness;
pub(crate) use readiness::{
    PullReadiness, SetPullReadiness, SetPullReadinessInput, SetPullReadinessOutcome,
};
