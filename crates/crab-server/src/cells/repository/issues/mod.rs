use super::*;
pub(crate) mod lifecycle;
pub(crate) mod pins;
mod reads;
use crate::cells::repository::collaboration::{
    Target,
    activity::{self, Action},
};
pub(crate) use reads::{GetComment, GetIssue, GetIssueDetail, ListComments};

pub(crate) struct CreateIssue;

impl Mutation for CreateIssue {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 1;
    type Input = CreateIssueInput;
    type Output = CreateIssueOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(CreateIssueOutcome::Archived));
        }
        lifecycle::ensure_ready(context)?;
        validate_author(&input.author)?;
        validate_title(&input.title)?;
        validate_body(&input.body, false)?;
        input.metadata.selection.validate()?;
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
                    "SELECT number, author_issuer, author_subject, author_name, title, body, state, label_ids, assignees, version, created_at_ms, updated_at_ms FROM repository_issues WHERE number = ?",
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
        use crate::cells::repository::collaboration::{Subject, SubjectKind, creation};
        match input.metadata.resolve(
            context,
            SubjectKind::Issue,
            input.submission_id,
            payload_digest,
            None,
        )? {
            creation::Resolution::Ready => {}
            creation::Resolution::Rejected(reason) => {
                return Ok(MutationResult::Success(
                    CreateIssueOutcome::MetadataRejected(reason),
                ));
            }
            creation::Resolution::Conflict => {
                return Ok(MutationResult::Rejected(
                    CreateIssueOutcome::RequestConflict,
                ));
            }
        }
        let milestone = input.metadata.selection.milestone;
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
            label_ids: input.metadata.selection.label_ids,
            assignees: input.metadata.eligible,
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
        lifecycle::create(context, record.number)?;
        advance_revision(context)?;
        let target = Target::Issue {
            number: record.number,
        };
        activity::append(context, target, &record.author, Action::Created)?;
        creation::assign_milestone(
            context,
            Subject {
                kind: SubjectKind::Issue,
                number: record.number,
            },
            &record.author,
            milestone,
        )?;
        Ok(MutationResult::Success(CreateIssueOutcome::Created(
            Box::new(record),
        )))
    }
}

pub(crate) struct CreateComment;

impl Mutation for CreateComment {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 2;
    type Input = CreateCommentInput;
    type Output = CreateCommentOutcome;

    fn execute(
        context: &mut Transaction<'_>,
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
        let record = append_comment(context, input.issue, &input.author, &input.body)?;
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

        Ok(MutationResult::Success(CreateCommentOutcome::Created(
            record,
        )))
    }
}

// Callers own validation, permissions and submission reservations; both plain
// comments and atomic state actions use this canonical contribution writer.
pub(in crate::cells::repository) fn append_comment(
    context: &Transaction<'_>,
    issue: u64,
    author: &RepositoryAuthor,
    body: &str,
) -> crate::cells::Result<CommentRecord> {
    let now = timestamp(context.now_ms())?;
    let sequence = context.sql(&SqlBatch {
        statements: vec![
            statement(
                "INSERT INTO repository_comment_sequences(issue_number, last) VALUES (?, 1) ON CONFLICT(issue_number) DO UPDATE SET last = last + 1 WHERE last < 9007199254740991",
                vec![integer(issue)?],
            ),
            statement(
                "SELECT last FROM repository_comment_sequences WHERE issue_number = ?",
                vec![integer(issue)?],
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
        issue,
        number,
        author: author.clone(),
        body: body.to_owned(),
        version: 1,
        created_at_ms: now,
        updated_at_ms: now,
    };
    insert_comment(context, &record)?;
    advance_revision(context)?;
    let target = Target::IssueComment {
        issue: record.issue,
        number: record.number,
    };
    search::contributions::project(context, target, &record.author, &record.body)?;
    activity::append(context, target, &record.author, Action::Created)?;
    Ok(record)
}

pub(crate) struct UpdateIssue;

impl Mutation for UpdateIssue {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 3;
    type Input = UpdateIssueInput;
    type Output = UpdateIssueOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(UpdateIssueOutcome::Archived));
        }
        lifecycle::ensure_ready(context)?;
        validate_number(input.number)?;
        validate_number(input.version)?;
        validate_author(&input.actor)?;
        if input.title.is_none()
            && input.body.is_none()
            && input.state.is_none()
            && input.state_reason.is_none()
            && input.label_ids.is_none()
            && input.assignees.is_none()
        {
            return Err(crate::cells::Error::Invalid(
                "repository issue update has no changes",
            ));
        }
        if let Some(title) = input.title.as_deref() {
            validate_title(title)?;
        }
        if let Some(body) = input.body.as_deref() {
            validate_body(body, false)?;
        }
        if input.state.is_some_and(|state| state > 1) {
            return Err(crate::cells::Error::Invalid(
                "repository issue state is invalid",
            ));
        }
        if let Some(labels) = input.label_ids.as_deref() {
            validate_label_ids(labels)?;
            if !labels.is_empty() {
                let statements = labels
                    .iter()
                    .map(|label| {
                        Ok(statement(
                            "SELECT 1 FROM repository_labels WHERE number = ? AND deleted_version IS NULL",
                            vec![integer(*label)?],
                        ))
                    })
                    .collect::<crate::cells::Result<Vec<_>>>()?;
                let existing = context.sql(&SqlBatch { statements })?;
                if existing.iter().any(|result| result.rows.is_empty()) {
                    return Ok(MutationResult::Rejected(UpdateIssueOutcome::LabelInvalid));
                }
            }
        }
        if let Some(assignees) = input.assignees.as_deref() {
            assignments::validate(assignees)?;
        }

        let current = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT number, author_issuer, author_subject, author_name, title, body, state, label_ids, assignees, version, created_at_ms, updated_at_ms FROM repository_issues WHERE number = ?",
                vec![integer(input.number)?],
            )],
        })?;
        let Some(row) = current[0].rows.first() else {
            return Ok(MutationResult::Rejected(UpdateIssueOutcome::NotFound));
        };
        let mut issue = issue_from_row(row)?;
        let was_closed = issue.state != 0;
        let edited = input
            .title
            .as_ref()
            .is_some_and(|value| value != &issue.title)
            || input
                .body
                .as_ref()
                .is_some_and(|value| value != &issue.body)
            || input
                .label_ids
                .as_ref()
                .is_some_and(|value| value != &issue.label_ids)
            || input
                .assignees
                .as_ref()
                .is_some_and(|value| value != &issue.assignees);
        let author = same_author(&issue.author, &input.actor);
        if (input.title.is_some() || input.body.is_some()) && !author
            || (input.state.is_some() || input.state_reason.is_some())
                && !(author || input.can_triage)
        {
            return Ok(MutationResult::Rejected(UpdateIssueOutcome::Forbidden));
        }
        if input.label_ids.is_some() && !input.can_triage {
            return Ok(MutationResult::Rejected(UpdateIssueOutcome::LabelForbidden));
        }
        if input.assignees.is_some() && !input.can_triage {
            return Ok(MutationResult::Rejected(
                UpdateIssueOutcome::AssigneeForbidden,
            ));
        }
        if issue.version != input.version {
            return Ok(MutationResult::Rejected(UpdateIssueOutcome::Conflict));
        }

        if let Some(value) = input.title {
            issue.title = value;
        }
        if let Some(value) = input.body {
            issue.body = value;
        }
        if let Some(value) = input.state {
            issue.state = value;
        }
        if let Some(value) = input.label_ids {
            issue.label_ids = value;
        }
        if let Some(value) = input.assignees {
            issue.assignees = value;
        }
        issue.version = issue
            .version
            .checked_add(1)
            .filter(|version| *version <= MAX_NUMBER)
            .ok_or(crate::cells::Error::Invalid(
                "repository issue version is exhausted",
            ))?;
        issue.updated_at_ms = timestamp(context.now_ms())?;
        let Some((previous_lifecycle, lifecycle)) = lifecycle::change(
            context,
            issue.number,
            was_closed,
            issue.state != 0,
            input.state_reason,
            issue.updated_at_ms,
        )?
        else {
            return Ok(MutationResult::Rejected(
                UpdateIssueOutcome::InvalidStateReason,
            ));
        };
        let updated = context.sql(&SqlBatch {
            statements: vec![statement(
                "UPDATE repository_issues SET title = ?, body = ?, state = ?, label_ids = ?, assignees = ?, version = ?, updated_at_ms = ? WHERE number = ? AND version = ?",
                vec![
                    SqlValue::Text(issue.title.clone()),
                    SqlValue::Text(issue.body.clone()),
                    SqlValue::Integer(i64::from(issue.state)),
                    SqlValue::Blob(encode_label_ids(&issue.label_ids)?),
                    SqlValue::Blob(assignments::encode(&issue.assignees)?),
                    integer(issue.version)?,
                    integer(issue.updated_at_ms)?,
                    integer(issue.number)?,
                    integer(input.version)?,
                ],
            )],
        })?;
        if updated[0].rows_affected != 1 {
            return Ok(MutationResult::Rejected(UpdateIssueOutcome::Conflict));
        }
        // Progress and the issue state must share the command transaction.
        let subject = collaboration::Subject {
            kind: collaboration::SubjectKind::Issue,
            number: issue.number,
        };
        collaboration::milestones::state_changed(context, subject, was_closed, issue.state != 0)?;
        advance_revision(context)?;
        let target = Target::Issue {
            number: issue.number,
        };
        if edited {
            activity::append(context, target, &input.actor, Action::Updated)?;
        }
        if was_closed != (issue.state != 0) {
            let action = if issue.state == 0 {
                Action::Reopened
            } else {
                Action::IssueClosed {
                    reason: lifecycle
                        .state_reason
                        .ok_or(crate::cells::Error::Invalid("new closure has no reason"))?,
                }
            };
            activity::append(context, target, &input.actor, action)?;
        }
        if was_closed
            && issue.state != 0
            && previous_lifecycle.state_reason != lifecycle.state_reason
        {
            activity::append(
                context,
                target,
                &input.actor,
                Action::IssueCloseReasonChanged {
                    from: previous_lifecycle.state_reason,
                    to: lifecycle.state_reason.ok_or(crate::cells::Error::Invalid(
                        "closure reason change is missing",
                    ))?,
                },
            )?;
        }
        Ok(MutationResult::Success(UpdateIssueOutcome::Updated(
            Box::new(issue),
        )))
    }
}

pub(crate) struct UpdateComment;

impl Mutation for UpdateComment {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 4;
    type Input = UpdateCommentInput;
    type Output = UpdateCommentOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(UpdateCommentOutcome::Archived));
        }
        validate_number(input.key.issue)?;
        validate_number(input.key.number)?;
        validate_number(input.version)?;
        validate_author(&input.actor)?;
        validate_body(&input.body, true)?;
        let current = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT issue_number, number, author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms FROM repository_issue_comments WHERE issue_number = ? AND number = ?",
                vec![integer(input.key.issue)?, integer(input.key.number)?],
            )],
        })?;
        let Some(row) = current[0].rows.first() else {
            return Ok(MutationResult::Rejected(UpdateCommentOutcome::NotFound));
        };
        let mut comment = comment_from_row(row)?;
        if !same_author(&comment.author, &input.actor) {
            return Ok(MutationResult::Rejected(UpdateCommentOutcome::Forbidden));
        }
        if comment.version != input.version {
            return Ok(MutationResult::Rejected(UpdateCommentOutcome::Conflict));
        }
        let edited = comment.body != input.body;
        comment.body = input.body;
        comment.version = comment
            .version
            .checked_add(1)
            .filter(|version| *version <= MAX_NUMBER)
            .ok_or(crate::cells::Error::Invalid(
                "repository comment version is exhausted",
            ))?;
        comment.updated_at_ms = timestamp(context.now_ms())?;
        let updated = context.sql(&SqlBatch {
            statements: vec![statement(
                "UPDATE repository_issue_comments SET body = ?, version = ?, updated_at_ms = ? WHERE issue_number = ? AND number = ? AND version = ?",
                vec![
                    SqlValue::Text(comment.body.clone()),
                    integer(comment.version)?,
                    integer(comment.updated_at_ms)?,
                    integer(comment.issue)?,
                    integer(comment.number)?,
                    integer(input.version)?,
                ],
            )],
        })?;
        if updated[0].rows_affected != 1 {
            return Ok(MutationResult::Rejected(UpdateCommentOutcome::Conflict));
        }
        advance_revision(context)?;
        if edited {
            let target = Target::IssueComment {
                issue: comment.issue,
                number: comment.number,
            };
            search::contributions::edit(context, target, &comment.body)?;
            activity::append(context, target, &input.actor, Action::Updated)?;
        }
        Ok(MutationResult::Success(UpdateCommentOutcome::Updated(
            comment,
        )))
    }
}
