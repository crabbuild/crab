use crate::cells::application::{SqlBatch, SqlResultSet, SqlStatement, SqlValue};
use crate::cells::records::{RecordDecoder, RecordEncoder, StoredRecord};

use crate::cells::application::{Mutation, MutationResult, Read, RepositoryTransaction};

#[cfg(test)]
pub(crate) use checks::CheckRunPage;
pub(crate) use checks::{
    CheckAnnotationRecord, CheckOutputRecord, CheckReportInput, CheckRunDetail, CheckRunKey,
    CheckRunRecord, CheckStepRecord, CheckSubmissionKey, CreateCheckRun, CreateCheckRunInput,
    CreateCheckRunOutcome, GetCheckCreateSubmission, GetCheckRun, GetCheckUpdateSubmission,
    ListCheckRuns, ListCheckRunsInput, UpdateCheckRun, UpdateCheckRunInput, UpdateCheckRunOutcome,
};
pub(crate) use operations::{
    CreateCommitStatus, CreateLabel, DeleteLabel, GetCommitStatusSubmission, ListComments,
    ListCommitStatuses, ListIssues, ListLabels, UpdateComment, UpdateIssue, UpdateLabel,
};
pub(crate) use pull_review_threads::{
    CreatePullReviewReply, CreatePullReviewReplyInput, CreatePullReviewReplyOutcome,
    CreatePullReviewThread, CreatePullReviewThreadInput, CreatePullReviewThreadOutcome,
    GetPullReviewReply, GetPullReviewReplySubmission, GetPullReviewThread,
    GetPullReviewThreadSubmission, ListPullReviewReplies, ListPullReviewThreads,
    PullReviewReplyKey, PullReviewReplyListInput, PullReviewReplyRecord,
    PullReviewReplySubmissionKey, PullReviewThreadKey, PullReviewThreadListInput,
    PullReviewThreadRecord, PullReviewThreadSide, PullReviewThreadSubmissionKey,
    UpdatePullReviewReply, UpdatePullReviewReplyInput, UpdatePullReviewReplyOutcome,
    UpdatePullReviewThread, UpdatePullReviewThreadInput, UpdatePullReviewThreadOutcome,
};
pub(crate) use pulls::{
    CreatePull, CreatePullComment, CreatePullCommentInput, CreatePullCommentOutcome,
    CreatePullInput, CreatePullOutcome, CreatePullReview, CreatePullReviewInput,
    CreatePullReviewOutcome, GetPull, GetPullComment, GetPullMergeSubmission, GetPullReview,
    GetPullReviewSubmission, GetPullSubmission, ListPullComments, ListPullReviews, ListPulls,
    MergeMethod, PullChildKey, PullChildListInput, PullCommentRecord, PullListInput, PullMerge,
    PullMergeTransition, PullRecord, PullReviewRecord, PullState, PullSubmissionKey,
    ReservePullMerge, ReservePullMergeInput, ReservePullMergeOutcome, ReviewState,
    TransitionPullMerge, TransitionPullMergeInput, TransitionPullMergeOutcome, UpdatePull,
    UpdatePullComment, UpdatePullCommentInput, UpdatePullCommentOutcome, UpdatePullInput,
    UpdatePullOutcome, UpdatePullReview, UpdatePullReviewInput, UpdatePullReviewOutcome,
};
#[cfg(test)]
pub(crate) use releases::ReleaseAssetRecord;
pub(crate) use releases::{
    AttachReleaseAsset, AttachReleaseAssetInput, AttachReleaseAssetOutcome,
    CompleteReleasePublication, CompleteReleasePublicationInput, CompleteReleasePublicationOutcome,
    CreateRelease, CreateReleaseInput, CreateReleaseOutcome, DeleteRelease, DeleteReleaseAsset,
    DeleteReleaseAssetInput, DeleteReleaseAssetOutcome, DeleteReleaseInput, DeleteReleaseOutcome,
    GetRelease, GetReleaseByTag, GetReleaseSubmission, ListReleases, ReleaseAssetReservation,
    ReleaseListInput, ReleasePublication, ReleaseRecord, ReserveReleaseAsset,
    ReserveReleaseAssetInput, ReserveReleaseAssetOutcome, UpdateRelease, UpdateReleaseInput,
    UpdateReleaseOutcome,
};
pub(crate) use settings::{
    BranchProtectionRecord, BranchProtectionSettings, GetBranchProtections, GetRepositoryLifecycle,
    ReplaceBranchProtections, ReplaceBranchProtectionsInput, ReplaceBranchProtectionsOutcome,
    ReplaceRepositoryLifecycle, ReplaceRepositoryLifecycleInput, ReplaceRepositoryLifecycleOutcome,
    RepositoryLifecycleRecord,
};

const MAX_NUMBER: u64 = 9_007_199_254_740_991;
const MAX_LIST_ITEMS: usize = 50;
const MAX_LIST_SCAN: u64 = 200;
const MAX_LABELS: usize = 20;
const MAX_REPOSITORY_LABELS: u64 = 500;
const MAX_STATUS_CONTEXTS: u64 = 128;
const MAX_STATUS_SUBMISSIONS: u64 = 1_000;
const MAX_ASSIGNEES: usize = 10;
const MAX_LIST_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryAuthor {
    pub issuer: String,
    pub subject: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CommitStatusRecord {
    pub number: u64,
    pub submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub oid: String,
    pub context: String,
    pub state: u8,
    pub description: Option<String>,
    pub target_url: Option<String>,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CreateCommitStatusInput {
    pub submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub oid: String,
    pub context: String,
    pub state: u8,
    pub description: Option<String>,
    pub target_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum CreateCommitStatusOutcome {
    Created(Box<CommitStatusRecord>),
    RequestConflict,
    ContextLimit,
    SubmissionLimit,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CommitStatusSubmissionKey {
    pub oid: String,
    pub submission_id: [u8; 16],
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CommitStatusCatalog {
    pub statuses: Vec<CommitStatusRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct LabelRecord {
    pub number: u64,
    pub name: String,
    pub color: String,
    pub description: Option<String>,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct LabelCatalog {
    pub labels: Vec<LabelRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CreateLabelInput {
    pub submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub name: String,
    pub color: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum CreateLabelOutcome {
    Archived,
    Created(LabelRecord),
    RequestConflict,
    NameConflict,
    NotFound,
    LimitReached,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct UpdateLabelInput {
    pub number: u64,
    pub version: u64,
    pub name: String,
    pub color: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum UpdateLabelOutcome {
    Archived,
    Updated(LabelRecord),
    NotFound,
    NameConflict,
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct DeleteLabelInput {
    pub number: u64,
    pub version: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum DeleteLabelOutcome {
    Archived,
    Deleted,
    NotFound,
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CreateIssueInput {
    pub submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub title: String,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum CreateIssueOutcome {
    Archived,
    Created(Box<IssueRecord>),
    RequestConflict,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct IssueRecord {
    pub number: u64,
    pub author: RepositoryAuthor,
    pub title: String,
    pub body: String,
    pub state: u8,
    pub label_ids: Vec<u64>,
    pub assignee_subjects: Vec<String>,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CreateCommentInput {
    pub submission_id: [u8; 16],
    pub issue: u64,
    pub author: RepositoryAuthor,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CommentRecord {
    pub issue: u64,
    pub number: u64,
    pub author: RepositoryAuthor,
    pub body: String,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum CreateCommentOutcome {
    Archived,
    Created(CommentRecord),
    IssueNotFound,
    RequestConflict,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CommentKey {
    pub issue: u64,
    pub number: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct UpdateIssueInput {
    pub number: u64,
    pub actor: RepositoryAuthor,
    pub can_manage_metadata: bool,
    pub version: u64,
    pub title: Option<String>,
    pub body: Option<String>,
    pub state: Option<u8>,
    pub label_ids: Option<Vec<u64>>,
    pub assignee_subjects: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum UpdateIssueOutcome {
    Archived,
    Updated(Box<IssueRecord>),
    NotFound,
    Forbidden,
    LabelForbidden,
    LabelInvalid,
    AssigneeForbidden,
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct UpdateCommentInput {
    pub key: CommentKey,
    pub actor: RepositoryAuthor,
    pub version: u64,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum UpdateCommentOutcome {
    Archived,
    Updated(CommentRecord),
    NotFound,
    Forbidden,
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ListIssuesInput {
    pub before: Option<u64>,
    pub limit: u8,
    pub state: u8,
    pub query: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct IssueSummary {
    pub number: u64,
    pub author: RepositoryAuthor,
    pub title: String,
    pub state: u8,
    pub label_ids: Vec<u64>,
    pub assignee_subjects: Vec<String>,
    pub version: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct IssuePage {
    pub items: Vec<IssueSummary>,
    pub next: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ListCommentsInput {
    pub issue: u64,
    pub before: Option<u64>,
    pub limit: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum CommentPage {
    Found {
        items: Vec<CommentRecord>,
        next: Option<u64>,
    },
    IssueNotFound,
}

pub(crate) struct CreateIssue;

impl Mutation for CreateIssue {
    const ID: u32 = 1;
    type Input = CreateIssueInput;
    type Output = CreateIssueOutcome;

    fn execute(
        context: &mut RepositoryTransaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(CreateIssueOutcome::Archived));
        }
        validate_author(&input.author)?;
        validate_title(&input.title)?;
        validate_body(&input.body, false)?;
        let payload_digest = issue_submission_digest(&input);
        let reservation = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT payload_digest, issue_number FROM repository_issue_submissions WHERE request_id = ?",
                vec![SqlValue::Blob(input.submission_id.to_vec())],
            )],
        })?;
        if let Some(row) = reservation[0].rows.first() {
            if result_blob(row, 0)? != payload_digest.as_bytes() {
                return Ok(MutationResult::Rejected(
                    CreateIssueOutcome::RequestConflict,
                ));
            }
            let number = result_u64_from_row(row, 1)?;
            let current = context.sql(&SqlBatch {
                statements: vec![statement(
                    "SELECT number, author_issuer, author_subject, author_name, title, body, state, label_ids, assignee_subjects, version, created_at_ms, updated_at_ms FROM repository_issues WHERE number = ?",
                    vec![integer(number)?],
                )],
            })?;
            let issue = current[0].rows.first().ok_or(crate::cells::Error::Invalid(
                "repository issue submission has no issue row",
            ))?;
            return Ok(MutationResult::Success(CreateIssueOutcome::Created(
                Box::new(issue_from_row(issue)?),
            )));
        }
        let now = timestamp(context.now_ms())?;
        let sequence = context.sql(&SqlBatch {
            statements: vec![
                statement(
                    "UPDATE repository_sequences SET last = last + 1 WHERE kind = 'issue' AND last < 9007199254740991",
                    vec![],
                ),
                statement(
                    "SELECT last FROM repository_sequences WHERE kind = 'issue'",
                    vec![],
                ),
            ],
        })?;
        if sequence[0].rows_affected != 1 {
            return Err(crate::cells::Error::Invalid(
                "repository issue numbering is exhausted",
            ));
        }
        let number = result_u64(&sequence, 1, 0)?;
        let record = IssueRecord {
            number,
            author: input.author,
            title: input.title,
            body: input.body,
            state: 0,
            label_ids: vec![],
            assignee_subjects: vec![],
            version: 1,
            created_at_ms: now,
            updated_at_ms: now,
        };
        context.sql(&SqlBatch {
            statements: vec![statement(
                "INSERT INTO repository_issue_submissions(request_id, payload_digest, issue_number) VALUES (?, ?, ?)",
                vec![
                    SqlValue::Blob(input.submission_id.to_vec()),
                    SqlValue::Blob(payload_digest.as_bytes().to_vec()),
                    integer(record.number)?,
                ],
            )],
        })?;
        insert_issue(context, &record)?;
        advance_revision(context)?;
        Ok(MutationResult::Success(CreateIssueOutcome::Created(
            Box::new(record),
        )))
    }
}

pub(crate) struct CreateComment;

impl Mutation for CreateComment {
    const ID: u32 = 2;
    type Input = CreateCommentInput;
    type Output = CreateCommentOutcome;

    fn execute(
        context: &mut RepositoryTransaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(CreateCommentOutcome::Archived));
        }
        validate_number(input.issue)?;
        validate_author(&input.author)?;
        validate_body(&input.body, true)?;
        let exists = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT 1 FROM repository_issues WHERE number = ?",
                vec![integer(input.issue)?],
            )],
        })?;
        if exists[0].rows.is_empty() {
            return Ok(MutationResult::Rejected(
                CreateCommentOutcome::IssueNotFound,
            ));
        }
        let payload_digest = comment_submission_digest(&input);
        let reservation = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT payload_digest, comment_number FROM repository_comment_submissions WHERE issue_number = ? AND request_id = ?",
                vec![integer(input.issue)?, SqlValue::Blob(input.submission_id.to_vec())],
            )],
        })?;
        if let Some(row) = reservation[0].rows.first() {
            if result_blob(row, 0)? != payload_digest.as_bytes() {
                return Ok(MutationResult::Rejected(
                    CreateCommentOutcome::RequestConflict,
                ));
            }
            let number = result_u64_from_row(row, 1)?;
            let current = context.sql(&SqlBatch {
                statements: vec![statement(
                    "SELECT issue_number, number, author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms FROM repository_issue_comments WHERE issue_number = ? AND number = ?",
                    vec![integer(input.issue)?, integer(number)?],
                )],
            })?;
            let comment = current[0].rows.first().ok_or(crate::cells::Error::Invalid(
                "repository comment submission has no comment row",
            ))?;
            return Ok(MutationResult::Success(CreateCommentOutcome::Created(
                comment_from_row(comment)?,
            )));
        }
        let now = timestamp(context.now_ms())?;
        let sequence = context.sql(&SqlBatch {
            statements: vec![
                statement(
                    "INSERT INTO repository_comment_sequences(issue_number, last) VALUES (?, 1) ON CONFLICT(issue_number) DO UPDATE SET last = last + 1 WHERE last < 9007199254740991",
                    vec![integer(input.issue)?],
                ),
                statement(
                    "SELECT last FROM repository_comment_sequences WHERE issue_number = ?",
                    vec![integer(input.issue)?],
                ),
            ],
        })?;
        if sequence[0].rows_affected != 1 {
            return Err(crate::cells::Error::Invalid(
                "repository comment numbering is exhausted",
            ));
        }
        let number = result_u64(&sequence, 1, 0)?;
        let record = CommentRecord {
            issue: input.issue,
            number,
            author: input.author,
            body: input.body,
            version: 1,
            created_at_ms: now,
            updated_at_ms: now,
        };
        context.sql(&SqlBatch {
            statements: vec![statement(
                "INSERT INTO repository_comment_submissions(issue_number, request_id, payload_digest, comment_number) VALUES (?, ?, ?, ?)",
                vec![
                    integer(record.issue)?,
                    SqlValue::Blob(input.submission_id.to_vec()),
                    SqlValue::Blob(payload_digest.as_bytes().to_vec()),
                    integer(record.number)?,
                ],
            )],
        })?;
        insert_comment(context, &record)?;
        advance_revision(context)?;
        Ok(MutationResult::Success(CreateCommentOutcome::Created(
            record,
        )))
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct IssueDetail {
    pub(crate) issue: Option<IssueRecord>,
    pub(crate) labels: LabelCatalog,
}

pub(crate) struct GetIssueDetail;

impl Read for GetIssueDetail {
    const ID: u32 = 36;
    type Input = u64;
    type Output = IssueDetail;

    fn execute(
        context: &mut RepositoryTransaction<'_>,
        number: u64,
    ) -> crate::cells::Result<IssueDetail> {
        // Detail metadata shares the issue's observation point and routing
        // decision, so one HTTP request consumes exactly one reader turn.
        Ok(IssueDetail {
            issue: GetIssue::execute(context, number)?,
            labels: ListLabels::execute(context, ())?,
        })
    }
}

pub(crate) struct GetIssue;

impl Read for GetIssue {
    const ID: u32 = 1;
    type Input = u64;
    type Output = Option<IssueRecord>;

    fn execute(
        context: &mut RepositoryTransaction<'_>,
        number: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        validate_number(number)?;
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT number, author_issuer, author_subject, author_name, title, body, state, label_ids, assignee_subjects, version, created_at_ms, updated_at_ms FROM repository_issues WHERE number = ?",
                vec![integer(number)?],
            )],
        })?;
        result[0]
            .rows
            .first()
            .map(|row| issue_from_row(row.as_slice()))
            .transpose()
    }
}

pub(crate) struct GetComment;

impl Read for GetComment {
    const ID: u32 = 2;
    type Input = CommentKey;
    type Output = Option<CommentRecord>;

    fn execute(
        context: &mut RepositoryTransaction<'_>,
        key: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        validate_number(key.issue)?;
        validate_number(key.number)?;
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT issue_number, number, author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms FROM repository_issue_comments WHERE issue_number = ? AND number = ?",
                vec![integer(key.issue)?, integer(key.number)?],
            )],
        })?;
        result[0]
            .rows
            .first()
            .map(|row| comment_from_row(row.as_slice()))
            .transpose()
    }
}

mod checks;
mod checks_codec;
mod operations;
pub(crate) mod projection;
mod pull_review_threads;
mod pulls;
mod releases;
mod settings;
mod settings_codec;

pub(super) fn dispatch_mutation(
    context: &mut RepositoryTransaction<'_>,
    id: u32,
    input: serde_json::Value,
) -> crate::cells::Result<MutationResult<serde_json::Value>> {
    match id {
        <CreateIssue as Mutation>::ID => invoke_mutation::<CreateIssue>(context, input),
        <CreateComment as Mutation>::ID => invoke_mutation::<CreateComment>(context, input),
        <UpdateIssue as Mutation>::ID => invoke_mutation::<UpdateIssue>(context, input),
        <UpdateComment as Mutation>::ID => invoke_mutation::<UpdateComment>(context, input),
        <CreateLabel as Mutation>::ID => invoke_mutation::<CreateLabel>(context, input),
        <UpdateLabel as Mutation>::ID => invoke_mutation::<UpdateLabel>(context, input),
        <DeleteLabel as Mutation>::ID => invoke_mutation::<DeleteLabel>(context, input),
        <CreateCommitStatus as Mutation>::ID => {
            invoke_mutation::<CreateCommitStatus>(context, input)
        }
        <CreateCheckRun as Mutation>::ID => invoke_mutation::<CreateCheckRun>(context, input),
        <UpdateCheckRun as Mutation>::ID => invoke_mutation::<UpdateCheckRun>(context, input),
        <ReplaceBranchProtections as Mutation>::ID => {
            invoke_mutation::<ReplaceBranchProtections>(context, input)
        }
        <ReplaceRepositoryLifecycle as Mutation>::ID => {
            invoke_mutation::<ReplaceRepositoryLifecycle>(context, input)
        }
        <CreatePull as Mutation>::ID => invoke_mutation::<CreatePull>(context, input),
        <UpdatePull as Mutation>::ID => invoke_mutation::<UpdatePull>(context, input),
        <CreatePullComment as Mutation>::ID => invoke_mutation::<CreatePullComment>(context, input),
        <UpdatePullComment as Mutation>::ID => invoke_mutation::<UpdatePullComment>(context, input),
        <CreatePullReview as Mutation>::ID => invoke_mutation::<CreatePullReview>(context, input),
        <UpdatePullReview as Mutation>::ID => invoke_mutation::<UpdatePullReview>(context, input),
        <CreatePullReviewThread as Mutation>::ID => {
            invoke_mutation::<CreatePullReviewThread>(context, input)
        }
        <UpdatePullReviewThread as Mutation>::ID => {
            invoke_mutation::<UpdatePullReviewThread>(context, input)
        }
        <CreatePullReviewReply as Mutation>::ID => {
            invoke_mutation::<CreatePullReviewReply>(context, input)
        }
        <UpdatePullReviewReply as Mutation>::ID => {
            invoke_mutation::<UpdatePullReviewReply>(context, input)
        }
        <ReservePullMerge as Mutation>::ID => invoke_mutation::<ReservePullMerge>(context, input),
        <TransitionPullMerge as Mutation>::ID => {
            invoke_mutation::<TransitionPullMerge>(context, input)
        }
        <CreateRelease as Mutation>::ID => invoke_mutation::<CreateRelease>(context, input),
        <UpdateRelease as Mutation>::ID => invoke_mutation::<UpdateRelease>(context, input),
        <CompleteReleasePublication as Mutation>::ID => {
            invoke_mutation::<CompleteReleasePublication>(context, input)
        }
        <DeleteRelease as Mutation>::ID => invoke_mutation::<DeleteRelease>(context, input),
        <ReserveReleaseAsset as Mutation>::ID => {
            invoke_mutation::<ReserveReleaseAsset>(context, input)
        }
        <AttachReleaseAsset as Mutation>::ID => {
            invoke_mutation::<AttachReleaseAsset>(context, input)
        }
        <DeleteReleaseAsset as Mutation>::ID => {
            invoke_mutation::<DeleteReleaseAsset>(context, input)
        }
        <projection::ApplyProjectionBatch as Mutation>::ID => {
            invoke_mutation::<projection::ApplyProjectionBatch>(context, input)
        }
        _ => Err(crate::cells::Error::Invalid("unknown repository operation")),
    }
}

pub(super) fn dispatch_read(
    context: &mut RepositoryTransaction<'_>,
    id: u32,
    input: serde_json::Value,
) -> crate::cells::Result<serde_json::Value> {
    match id {
        <GetIssue as Read>::ID => invoke_read::<GetIssue>(context, input),
        <GetIssueDetail as Read>::ID => invoke_read::<GetIssueDetail>(context, input),
        <GetComment as Read>::ID => invoke_read::<GetComment>(context, input),
        <ListIssues as Read>::ID => invoke_read::<ListIssues>(context, input),
        <ListComments as Read>::ID => invoke_read::<ListComments>(context, input),
        <ListLabels as Read>::ID => invoke_read::<ListLabels>(context, input),
        <ListCommitStatuses as Read>::ID => invoke_read::<ListCommitStatuses>(context, input),
        <GetCommitStatusSubmission as Read>::ID => {
            invoke_read::<GetCommitStatusSubmission>(context, input)
        }
        <ListCheckRuns as Read>::ID => invoke_read::<ListCheckRuns>(context, input),
        <GetCheckRun as Read>::ID => invoke_read::<GetCheckRun>(context, input),
        <GetCheckCreateSubmission as Read>::ID => {
            invoke_read::<GetCheckCreateSubmission>(context, input)
        }
        <GetCheckUpdateSubmission as Read>::ID => {
            invoke_read::<GetCheckUpdateSubmission>(context, input)
        }
        <GetBranchProtections as Read>::ID => invoke_read::<GetBranchProtections>(context, input),
        <GetRepositoryLifecycle as Read>::ID => {
            invoke_read::<GetRepositoryLifecycle>(context, input)
        }
        <GetPull as Read>::ID => invoke_read::<GetPull>(context, input),
        <GetPullSubmission as Read>::ID => invoke_read::<GetPullSubmission>(context, input),
        <ListPulls as Read>::ID => invoke_read::<ListPulls>(context, input),
        <GetPullComment as Read>::ID => invoke_read::<GetPullComment>(context, input),
        <ListPullComments as Read>::ID => invoke_read::<ListPullComments>(context, input),
        <GetPullReview as Read>::ID => invoke_read::<GetPullReview>(context, input),
        <ListPullReviews as Read>::ID => invoke_read::<ListPullReviews>(context, input),
        <GetPullReviewSubmission as Read>::ID => {
            invoke_read::<GetPullReviewSubmission>(context, input)
        }
        <GetPullReviewThread as Read>::ID => invoke_read::<GetPullReviewThread>(context, input),
        <ListPullReviewThreads as Read>::ID => invoke_read::<ListPullReviewThreads>(context, input),
        <GetPullReviewThreadSubmission as Read>::ID => {
            invoke_read::<GetPullReviewThreadSubmission>(context, input)
        }
        <GetPullReviewReply as Read>::ID => invoke_read::<GetPullReviewReply>(context, input),
        <ListPullReviewReplies as Read>::ID => invoke_read::<ListPullReviewReplies>(context, input),
        <GetPullReviewReplySubmission as Read>::ID => {
            invoke_read::<GetPullReviewReplySubmission>(context, input)
        }
        <GetPullMergeSubmission as Read>::ID => {
            invoke_read::<GetPullMergeSubmission>(context, input)
        }
        <GetRelease as Read>::ID => invoke_read::<GetRelease>(context, input),
        <GetReleaseByTag as Read>::ID => invoke_read::<GetReleaseByTag>(context, input),
        <GetReleaseSubmission as Read>::ID => invoke_read::<GetReleaseSubmission>(context, input),
        <ListReleases as Read>::ID => invoke_read::<ListReleases>(context, input),
        <projection::GetProjectionState as Read>::ID => {
            invoke_read::<projection::GetProjectionState>(context, input)
        }
        <projection::GetProjectionAttribution as Read>::ID => {
            invoke_read::<projection::GetProjectionAttribution>(context, input)
        }
        _ => Err(crate::cells::Error::Invalid("unknown repository operation")),
    }
}

fn invoke_mutation<T: Mutation>(
    context: &mut RepositoryTransaction<'_>,
    input: serde_json::Value,
) -> crate::cells::Result<MutationResult<serde_json::Value>> {
    let input = serde_json::from_value(input)?;
    Ok(match T::execute(context, input)? {
        MutationResult::Success(value) => MutationResult::Success(serde_json::to_value(value)?),
        MutationResult::Rejected(value) => MutationResult::Rejected(serde_json::to_value(value)?),
    })
}

fn invoke_read<T: Read>(
    context: &mut RepositoryTransaction<'_>,
    input: serde_json::Value,
) -> crate::cells::Result<serde_json::Value> {
    Ok(serde_json::to_value(T::execute(
        context,
        serde_json::from_value(input)?,
    )?)?)
}

fn statement(sql: &str, parameters: Vec<SqlValue>) -> SqlStatement {
    SqlStatement {
        sql: sql.to_owned(),
        parameters,
    }
}

fn insert_issue(
    context: &RepositoryTransaction<'_>,
    record: &IssueRecord,
) -> crate::cells::Result<()> {
    context.sql(&SqlBatch {
        statements: vec![statement(
            "INSERT INTO repository_issues(number, author_issuer, author_subject, author_name, title, body, state, label_ids, assignee_subjects, version, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            vec![
                integer(record.number)?,
                SqlValue::Text(record.author.issuer.clone()),
                SqlValue::Text(record.author.subject.clone()),
                SqlValue::Text(record.author.name.clone()),
                SqlValue::Text(record.title.clone()),
                SqlValue::Text(record.body.clone()),
                SqlValue::Integer(i64::from(record.state)),
                SqlValue::Blob(encode_label_ids(&record.label_ids)?),
                SqlValue::Blob(encode_assignees(&record.assignee_subjects)?),
                integer(record.version)?,
                integer(record.created_at_ms)?,
                integer(record.updated_at_ms)?,
            ],
        )],
    })?;
    Ok(())
}

fn insert_comment(
    context: &RepositoryTransaction<'_>,
    record: &CommentRecord,
) -> crate::cells::Result<()> {
    context.sql(&SqlBatch {
        statements: vec![statement(
            "INSERT INTO repository_issue_comments(issue_number, number, author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            vec![
                integer(record.issue)?,
                integer(record.number)?,
                SqlValue::Text(record.author.issuer.clone()),
                SqlValue::Text(record.author.subject.clone()),
                SqlValue::Text(record.author.name.clone()),
                SqlValue::Text(record.body.clone()),
                integer(record.version)?,
                integer(record.created_at_ms)?,
                integer(record.updated_at_ms)?,
            ],
        )],
    })?;
    Ok(())
}

pub(super) fn issue_submission_digest(input: &CreateIssueInput) -> blake3::Hash {
    // Display names are snapshots; stable issuer/subject identity preserves the
    // retry identity when a user changes their presentation name.
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.repository.issue-submission.v1\0");
    hash_text(&mut hasher, &input.author.issuer);
    hash_text(&mut hasher, &input.author.subject);
    hash_text(&mut hasher, &input.title);
    hash_text(&mut hasher, &input.body);
    hasher.finalize()
}

pub(super) fn comment_submission_digest(input: &CreateCommentInput) -> blake3::Hash {
    // Keep this identity rule aligned with issue submissions.
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.repository.comment-submission.v1\0");
    hasher.update(&input.issue.to_be_bytes());
    hash_text(&mut hasher, &input.author.issuer);
    hash_text(&mut hasher, &input.author.subject);
    hash_text(&mut hasher, &input.body);
    hasher.finalize()
}

pub(super) fn label_submission_digest(input: &CreateLabelInput) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.repository.label-submission.v1\0");
    hash_text(&mut hasher, &input.author.issuer);
    hash_text(&mut hasher, &input.author.subject);
    hash_text(&mut hasher, &input.name);
    hash_text(&mut hasher, &input.color);
    match &input.description {
        Some(description) => {
            hasher.update(&[1]);
            hash_text(&mut hasher, description);
        }
        None => {
            hasher.update(&[0]);
        }
    }
    hasher.finalize()
}

pub(super) fn status_submission_digest(input: &CreateCommitStatusInput) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.repository.commit-status-submission.v1\0");
    hash_text(&mut hasher, &input.author.issuer);
    hash_text(&mut hasher, &input.author.subject);
    hash_text(&mut hasher, &input.oid);
    hash_text(&mut hasher, &input.context);
    hasher.update(&[input.state]);
    hash_optional_text(&mut hasher, input.description.as_deref());
    hash_optional_text(&mut hasher, input.target_url.as_deref());
    hasher.finalize()
}

fn hash_optional_text(hasher: &mut blake3::Hasher, value: Option<&str>) {
    match value {
        Some(value) => {
            hasher.update(&[1]);
            hash_text(hasher, value);
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

fn hash_text(hasher: &mut blake3::Hasher, value: &str) {
    hasher.update(&(value.len() as u64).to_be_bytes());
    hasher.update(value.as_bytes());
}

fn integer(value: u64) -> crate::cells::Result<SqlValue> {
    Ok(SqlValue::Integer(i64::try_from(value).map_err(|_| {
        crate::cells::Error::Invalid("repository integer exceeds SQLite range")
    })?))
}

fn advance_revision(context: &RepositoryTransaction<'_>) -> crate::cells::Result<()> {
    let result = context.sql(&SqlBatch {
        statements: vec![statement(
            "UPDATE repository_identity SET app_revision = app_revision + 1 WHERE singleton = 1 AND app_revision < 9007199254740991",
            vec![],
        )],
    })?;
    if result[0].rows_affected != 1 {
        return Err(crate::cells::Error::Invalid(
            "repository identity is missing or its revision is exhausted",
        ));
    }
    Ok(())
}

fn result_u64(sets: &[SqlResultSet], set: usize, column: usize) -> crate::cells::Result<u64> {
    match sets
        .get(set)
        .and_then(|set| set.rows.first())
        .and_then(|row| row.get(column))
    {
        Some(SqlValue::Integer(value)) => u64::try_from(*value)
            .map_err(|_| crate::cells::Error::Invalid("repository result is negative")),
        _ => Err(crate::cells::Error::Invalid(
            "repository query returned an invalid integer",
        )),
    }
}

fn result_text(row: &[SqlValue], column: usize) -> crate::cells::Result<String> {
    match row.get(column) {
        Some(SqlValue::Text(value)) => Ok(value.clone()),
        _ => Err(crate::cells::Error::Invalid(
            "repository query returned invalid text",
        )),
    }
}

fn result_blob(row: &[SqlValue], column: usize) -> crate::cells::Result<&[u8]> {
    match row.get(column) {
        Some(SqlValue::Blob(value)) => Ok(value),
        _ => Err(crate::cells::Error::Invalid(
            "repository query returned invalid bytes",
        )),
    }
}

fn result_optional_text(row: &[SqlValue], column: usize) -> crate::cells::Result<Option<String>> {
    match row.get(column) {
        Some(SqlValue::Null) => Ok(None),
        Some(SqlValue::Text(value)) => Ok(Some(value.clone())),
        _ => Err(crate::cells::Error::Invalid(
            "repository query returned invalid optional text",
        )),
    }
}

fn label_from_row(row: &[SqlValue]) -> crate::cells::Result<LabelRecord> {
    let record = LabelRecord {
        number: result_u64_from_row(row, 0)?,
        name: result_text(row, 1)?,
        color: result_text(row, 2)?,
        description: result_optional_text(row, 3)?,
        version: result_u64_from_row(row, 4)?,
        created_at_ms: result_u64_from_row(row, 5)?,
        updated_at_ms: result_u64_from_row(row, 6)?,
    };
    validate_label(&record)?;
    Ok(record)
}

fn status_from_row(row: &[SqlValue]) -> crate::cells::Result<CommitStatusRecord> {
    let submission = result_blob(row, 1)?;
    let submission_id = <[u8; 16]>::try_from(submission)
        .map_err(|_| crate::cells::Error::Invalid("repository status submission ID is invalid"))?;
    let state = u8::try_from(result_u64_from_row(row, 7)?)
        .map_err(|_| crate::cells::Error::Invalid("repository status state is invalid"))?;
    let record = CommitStatusRecord {
        number: result_u64_from_row(row, 0)?,
        submission_id,
        author: RepositoryAuthor {
            issuer: result_text(row, 2)?,
            subject: result_text(row, 3)?,
            name: result_text(row, 4)?,
        },
        oid: result_text(row, 5)?,
        context: result_text(row, 6)?,
        state,
        description: result_optional_text(row, 8)?,
        target_url: result_optional_text(row, 9)?,
        created_at_ms: result_u64_from_row(row, 10)?,
    };
    validate_status_record(&record)?;
    Ok(record)
}

fn issue_from_row(row: &[SqlValue]) -> crate::cells::Result<IssueRecord> {
    let state = u8::try_from(result_u64_from_row(row, 6)?)
        .map_err(|_| crate::cells::Error::Invalid("invalid issue state"))?;
    if state > 1 {
        return Err(crate::cells::Error::Invalid("invalid issue state"));
    }
    let record = IssueRecord {
        number: result_u64_from_row(row, 0)?,
        author: RepositoryAuthor {
            issuer: result_text(row, 1)?,
            subject: result_text(row, 2)?,
            name: result_text(row, 3)?,
        },
        title: result_text(row, 4)?,
        body: result_text(row, 5)?,
        state,
        label_ids: decode_label_ids(result_blob(row, 7)?)?,
        assignee_subjects: decode_assignees(result_blob(row, 8)?)?,
        version: result_u64_from_row(row, 9)?,
        created_at_ms: result_u64_from_row(row, 10)?,
        updated_at_ms: result_u64_from_row(row, 11)?,
    };
    validate_issue(&record)?;
    Ok(record)
}

fn comment_from_row(row: &[SqlValue]) -> crate::cells::Result<CommentRecord> {
    let record = CommentRecord {
        issue: result_u64_from_row(row, 0)?,
        number: result_u64_from_row(row, 1)?,
        author: RepositoryAuthor {
            issuer: result_text(row, 2)?,
            subject: result_text(row, 3)?,
            name: result_text(row, 4)?,
        },
        body: result_text(row, 5)?,
        version: result_u64_from_row(row, 6)?,
        created_at_ms: result_u64_from_row(row, 7)?,
        updated_at_ms: result_u64_from_row(row, 8)?,
    };
    validate_comment(&record)?;
    Ok(record)
}

fn result_u64_from_row(row: &[SqlValue], column: usize) -> crate::cells::Result<u64> {
    match row.get(column) {
        Some(SqlValue::Integer(value)) => u64::try_from(*value)
            .map_err(|_| crate::cells::Error::Invalid("repository result is negative")),
        _ => Err(crate::cells::Error::Invalid(
            "repository query returned an invalid integer",
        )),
    }
}

fn timestamp(now_ms: i64) -> crate::cells::Result<u64> {
    u64::try_from(now_ms)
        .map_err(|_| crate::cells::Error::Invalid("repository timestamp is negative"))
}

fn validate_number(number: u64) -> crate::cells::Result<()> {
    if number == 0 || number > MAX_NUMBER {
        return Err(crate::cells::Error::Invalid("repository number is invalid"));
    }
    Ok(())
}

fn validate_author(author: &RepositoryAuthor) -> crate::cells::Result<()> {
    for (value, maximum) in [
        (&author.issuer, 512),
        (&author.subject, 512),
        (&author.name, 160),
    ] {
        if value.is_empty()
            || value.chars().count() > maximum
            || value.chars().any(char::is_control)
        {
            return Err(crate::cells::Error::Invalid(
                "repository author identity is invalid",
            ));
        }
    }
    Ok(())
}

fn validate_title(title: &str) -> crate::cells::Result<()> {
    if title.trim() != title
        || title.is_empty()
        || title.chars().count() > 256
        || title.chars().any(char::is_control)
    {
        return Err(crate::cells::Error::Invalid(
            "repository issue title is invalid",
        ));
    }
    Ok(())
}

fn validate_body(body: &str, required: bool) -> crate::cells::Result<()> {
    if (required && body.trim().is_empty()) || body.len() > 64 * 1024 || body.contains('\0') {
        return Err(crate::cells::Error::Invalid(
            "repository discussion body is invalid",
        ));
    }
    Ok(())
}

fn validate_label_fields(
    name: &str,
    color: &str,
    description: Option<&str>,
) -> crate::cells::Result<()> {
    if name.is_empty()
        || name.trim() != name
        || name.chars().count() > 50
        || name.chars().any(char::is_control)
        || color.len() != 6
        || !color.bytes().all(|byte| byte.is_ascii_hexdigit())
        || color.to_ascii_lowercase() != color
        || description.is_some_and(|value| {
            value.is_empty()
                || value.trim() != value
                || value.chars().count() > 100
                || value.chars().any(char::is_control)
        })
    {
        return Err(crate::cells::Error::Invalid(
            "repository label fields are invalid",
        ));
    }
    Ok(())
}

fn validate_status_fields(
    oid: &str,
    context: &str,
    state: u8,
    description: Option<&str>,
    target_url: Option<&str>,
) -> crate::cells::Result<()> {
    if oid.len() != 40
        || !oid
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || oid.bytes().all(|byte| byte == b'0')
        || context.is_empty()
        || context.trim() != context
        || context.chars().count() > 100
        || context.chars().any(char::is_control)
        || state > 3
        || description
            .is_some_and(|value| value.chars().count() > 140 || value.chars().any(char::is_control))
        || target_url
            .is_some_and(|value| value.len() > 2_048 || value.chars().any(char::is_control))
    {
        return Err(crate::cells::Error::Invalid(
            "repository commit status is invalid",
        ));
    }
    if let Some(value) = target_url {
        let url = url::Url::parse(value).map_err(|_| {
            crate::cells::Error::Invalid("repository commit status target is invalid")
        })?;
        crate::config::validate_identity_url(&url, true).map_err(|_| {
            crate::cells::Error::Invalid("repository commit status target is invalid")
        })?;
    }
    Ok(())
}

fn validate_status_record(record: &CommitStatusRecord) -> crate::cells::Result<()> {
    if record.number == 0 || record.number > MAX_STATUS_SUBMISSIONS {
        return Err(crate::cells::Error::Invalid(
            "repository commit status number is invalid",
        ));
    }
    validate_author(&record.author)?;
    validate_status_fields(
        &record.oid,
        &record.context,
        record.state,
        record.description.as_deref(),
        record.target_url.as_deref(),
    )
}

fn validate_label(record: &LabelRecord) -> crate::cells::Result<()> {
    if record.number == 0 || record.number > MAX_REPOSITORY_LABELS {
        return Err(crate::cells::Error::Invalid(
            "repository label number is invalid",
        ));
    }
    validate_label_fields(&record.name, &record.color, record.description.as_deref())?;
    validate_number(record.version)?;
    if record.updated_at_ms < record.created_at_ms {
        return Err(crate::cells::Error::Invalid(
            "repository label row is invalid",
        ));
    }
    Ok(())
}

fn validate_issue(record: &IssueRecord) -> crate::cells::Result<()> {
    validate_number(record.number)?;
    validate_author(&record.author)?;
    validate_title(&record.title)?;
    validate_body(&record.body, false)?;
    validate_label_ids(&record.label_ids)?;
    validate_assignees(&record.assignee_subjects)?;
    validate_number(record.version)?;
    if record.state > 1 || record.updated_at_ms < record.created_at_ms {
        return Err(crate::cells::Error::Invalid(
            "repository issue row is invalid",
        ));
    }
    Ok(())
}

fn validate_label_ids(labels: &[u64]) -> crate::cells::Result<()> {
    if labels.len() > MAX_LABELS
        || labels.contains(&0)
        || labels.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(crate::cells::Error::Invalid(
            "repository issue labels are invalid",
        ));
    }
    Ok(())
}

fn validate_assignees(assignees: &[String]) -> crate::cells::Result<()> {
    if assignees.len() > MAX_ASSIGNEES
        || assignees.iter().any(|subject| {
            subject.is_empty()
                || subject.chars().count() > 512
                || subject.chars().any(char::is_control)
        })
        || assignees.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(crate::cells::Error::Invalid(
            "repository issue assignees are invalid",
        ));
    }
    Ok(())
}

fn validate_list(before: Option<u64>, limit: u8) -> crate::cells::Result<()> {
    if limit == 0
        || usize::from(limit) > MAX_LIST_ITEMS
        || before.is_some_and(|value| value == 0 || value > MAX_NUMBER)
    {
        return Err(crate::cells::Error::Invalid(
            "repository list bounds are invalid",
        ));
    }
    Ok(())
}

fn validate_query(query: Option<&str>) -> crate::cells::Result<()> {
    if query.is_some_and(|query| {
        query.is_empty()
            || query.trim() != query
            || query.chars().count() > 256
            || query.chars().any(char::is_control)
            || query.to_lowercase() != query
    }) {
        return Err(crate::cells::Error::Invalid(
            "repository search query is invalid",
        ));
    }
    Ok(())
}

fn matches_query(query: Option<&str>, issue: &IssueRecord) -> bool {
    query.is_none_or(|query| {
        [&issue.title, &issue.body, &issue.author.name]
            .iter()
            .any(|value| value.to_lowercase().contains(query))
    })
}

fn same_author(left: &RepositoryAuthor, right: &RepositoryAuthor) -> bool {
    left.issuer == right.issuer && left.subject == right.subject
}

fn encode_label_ids(labels: &[u64]) -> crate::cells::Result<Vec<u8>> {
    let mut encoder = RecordEncoder::new(16 * 1024)
        .map_err(|_| crate::cells::Error::Invalid("repository label encoding failed"))?;
    encoder
        .write_count(labels.len())
        .map_err(|_| crate::cells::Error::Invalid("repository label encoding failed"))?;
    for label in labels {
        encoder
            .write_u64(*label)
            .map_err(|_| crate::cells::Error::Invalid("repository label encoding failed"))?;
    }
    Ok(encoder.finish())
}

fn decode_label_ids(bytes: &[u8]) -> crate::cells::Result<Vec<u64>> {
    let mut decoder = RecordDecoder::new(bytes, 16 * 1024)
        .map_err(|_| crate::cells::Error::Invalid("repository label encoding is invalid"))?;
    let count = decoder
        .read_count()
        .map_err(|_| crate::cells::Error::Invalid("repository label encoding is invalid"))?;
    if count > MAX_LABELS {
        return Err(crate::cells::Error::Invalid(
            "repository label encoding is invalid",
        ));
    }
    let mut labels = Vec::with_capacity(count);
    for _ in 0..count {
        labels.push(
            decoder.read_u64().map_err(|_| {
                crate::cells::Error::Invalid("repository label encoding is invalid")
            })?,
        );
    }
    decoder
        .finish()
        .map_err(|_| crate::cells::Error::Invalid("repository label encoding is invalid"))?;
    validate_label_ids(&labels)?;
    Ok(labels)
}

fn encode_assignees(assignees: &[String]) -> crate::cells::Result<Vec<u8>> {
    let mut encoder = RecordEncoder::new(16 * 1024)
        .map_err(|_| crate::cells::Error::Invalid("repository assignee encoding failed"))?;
    encoder
        .write_count(assignees.len())
        .map_err(|_| crate::cells::Error::Invalid("repository assignee encoding failed"))?;
    for assignee in assignees {
        encoder
            .write_text(assignee)
            .map_err(|_| crate::cells::Error::Invalid("repository assignee encoding failed"))?;
    }
    Ok(encoder.finish())
}

fn decode_assignees(bytes: &[u8]) -> crate::cells::Result<Vec<String>> {
    let mut decoder = RecordDecoder::new(bytes, 16 * 1024)
        .map_err(|_| crate::cells::Error::Invalid("repository assignee encoding is invalid"))?;
    let count = decoder
        .read_count()
        .map_err(|_| crate::cells::Error::Invalid("repository assignee encoding is invalid"))?;
    if count > MAX_ASSIGNEES {
        return Err(crate::cells::Error::Invalid(
            "repository assignee encoding is invalid",
        ));
    }
    let mut assignees = Vec::with_capacity(count);
    for _ in 0..count {
        assignees.push(
            decoder
                .read_text()
                .map_err(|_| {
                    crate::cells::Error::Invalid("repository assignee encoding is invalid")
                })?
                .to_owned(),
        );
    }
    decoder
        .finish()
        .map_err(|_| crate::cells::Error::Invalid("repository assignee encoding is invalid"))?;
    validate_assignees(&assignees)?;
    Ok(assignees)
}

fn encoded_size<T: serde::Serialize>(value: &T) -> crate::cells::Result<usize> {
    Ok(serde_json::to_vec(value)?.len())
}

fn validate_comment(record: &CommentRecord) -> crate::cells::Result<()> {
    validate_number(record.issue)?;
    validate_number(record.number)?;
    validate_author(&record.author)?;
    validate_body(&record.body, true)?;
    validate_number(record.version)?;
    if record.updated_at_ms < record.created_at_ms {
        return Err(crate::cells::Error::Invalid(
            "repository comment row is invalid",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
