pub(in crate::cells) const CLASS: &str = "Repository";
pub(in crate::cells) const NAMESPACE: &str = "REPOSITORIES";

pub(super) use dispatch::{dispatch_mutation, dispatch_read};
pub(crate) use issues::{CreateComment, CreateIssue, GetComment, GetIssue, GetIssueDetail};
pub(crate) use models::{
    CommentKey, CommentPage, CommentRecord, CommitStatusCatalog, CommitStatusRecord,
    CommitStatusSubmissionKey, CreateCommentInput, CreateCommentOutcome, CreateCommitStatusInput,
    CreateCommitStatusOutcome, CreateIssueInput, CreateIssueOutcome, CreateLabelInput,
    CreateLabelOutcome, DeleteLabelInput, DeleteLabelOutcome, IssueRecord, IssueSummary,
    LabelCatalog, LabelRecord, ListCommentsInput, RepositoryAuthor, UpdateCommentInput,
    UpdateCommentOutcome, UpdateIssueInput, UpdateIssueOutcome, UpdateLabelInput,
    UpdateLabelOutcome,
};

use crate::cells::application::{SqlBatch, SqlResultSet, SqlStatement, SqlValue};
use crate::cells::records::{RecordDecoder, RecordEncoder, StoredRecord};

use crate::cells::application::{Mutation, MutationResult, Read, Transaction};

pub(crate) use checks::{
    CheckAnnotationRecord, CheckOutputRecord, CheckReportInput, CheckRunDetail, CheckRunKey,
    CheckRunRecord, CheckStepRecord, CheckSubmissionKey, CreateCheckRun, CreateCheckRunInput,
    CreateCheckRunOutcome, GetCheckCreateSubmission, GetCheckRun, GetCheckUpdateSubmission,
    ListCheckRuns, ListCheckRunsInput, UpdateCheckRun, UpdateCheckRunInput, UpdateCheckRunOutcome,
};
pub(crate) use issues::{ListComments, UpdateComment, UpdateIssue};
pub(crate) use labels::{CreateLabel, DeleteLabel, ListLabels, UpdateLabel};
pub(crate) use pulls::review_threads::{
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
    MergeMethod, PullChildKey, PullChildListInput, PullCommentRecord, PullMerge,
    PullMergeTransition, PullReadiness, PullRecord, PullReviewRecord, PullState, PullSubmissionKey,
    PullSummary, ReservePullMerge, ReservePullMergeInput, ReservePullMergeOutcome, ReviewState,
    SetPullReadiness, SetPullReadinessInput, SetPullReadinessOutcome, TransitionPullMerge,
    TransitionPullMergeInput, TransitionPullMergeOutcome, UpdatePull, UpdatePullComment,
    UpdatePullCommentInput, UpdatePullCommentOutcome, UpdatePullInput, UpdatePullOutcome,
    UpdatePullReview, UpdatePullReviewInput, UpdatePullReviewOutcome,
};
pub(crate) use pulls::{review_requests, reviewers};
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
pub(crate) use statuses::{CreateCommitStatus, GetCommitStatusSubmission, ListCommitStatuses};

const MAX_NUMBER: u64 = 9_007_199_254_740_991;
const MAX_LIST_ITEMS: usize = 50;
const MAX_LIST_SCAN: u64 = 200;
const MAX_LABELS: usize = 20;
const MAX_REPOSITORY_LABELS: u64 = 500;
const MAX_STATUS_CONTEXTS: u64 = 128;
const MAX_STATUS_SUBMISSIONS: u64 = 1_000;
const MAX_LIST_OUTPUT_BYTES: usize = 1024 * 1024;

pub(crate) mod assignments;
mod checks;
pub(crate) mod collaboration;
mod dispatch;
pub(crate) mod issues;
mod labels;
mod models;
pub(crate) mod projection;
mod pulls;
mod releases;
mod schema;
pub(crate) mod search;
mod settings;
mod statuses;
pub(in crate::cells) use schema::initialize;
pub(crate) use search::index::SearchInput;
pub(crate) use search::results::{DiscussionSummary, ListDiscussions};

fn statement(sql: &str, parameters: Vec<SqlValue>) -> SqlStatement {
    SqlStatement {
        sql: sql.to_owned(),
        parameters,
    }
}

fn insert_issue(context: &Transaction<'_>, record: &IssueRecord) -> crate::cells::Result<()> {
    context.sql(&SqlBatch {
        statements: vec![statement(
            "INSERT INTO repository_issues(number, author_issuer, author_subject, author_name, title, body, state, label_ids, assignees, version, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            vec![
                integer(record.number)?,
                SqlValue::Text(record.author.issuer.clone()),
                SqlValue::Text(record.author.subject.clone()),
                SqlValue::Text(record.author.name.clone()),
                SqlValue::Text(record.title.clone()),
                SqlValue::Text(record.body.clone()),
                SqlValue::Integer(i64::from(record.state)),
                SqlValue::Blob(encode_label_ids(&record.label_ids)?),
                SqlValue::Blob(assignments::encode(&record.assignees)?),
                integer(record.version)?,
                integer(record.created_at_ms)?,
                integer(record.updated_at_ms)?,
            ],
        )],
    })?;
    Ok(())
}

fn insert_comment(context: &Transaction<'_>, record: &CommentRecord) -> crate::cells::Result<()> {
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

fn advance_revision(context: &Transaction<'_>) -> crate::cells::Result<()> {
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
        assignees: assignments::decode(result_blob(row, 8)?)?,
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

fn validate_issue(record: &IssueRecord) -> crate::cells::Result<()> {
    validate_number(record.number)?;
    validate_author(&record.author)?;
    validate_title(&record.title)?;
    validate_body(&record.body, false)?;
    validate_label_ids(&record.label_ids)?;
    assignments::validate(&record.assignees)?;
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
