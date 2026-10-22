use super::*;
use crate::cells::repository::collaboration::{
    Target,
    activity::{self, Action},
};

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
            assignees: vec![],
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
        let target = Target::Issue {
            number: record.number,
        };
        activity::append(context, target, &record.author, Action::Created)?;
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
        let target = Target::IssueComment {
            issue: record.issue,
            number: record.number,
        };
        search::contributions::project(context, target, &record.author, &record.body)?;
        activity::append(context, target, &record.author, Action::Created)?;
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
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 36;
    type Input = u64;
    type Output = IssueDetail;

    fn execute(context: &mut Transaction<'_>, number: u64) -> crate::cells::Result<IssueDetail> {
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
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 1;
    type Input = u64;
    type Output = Option<IssueRecord>;

    fn execute(
        context: &mut Transaction<'_>,
        number: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        validate_number(number)?;
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT number, author_issuer, author_subject, author_name, title, body, state, label_ids, assignees, version, created_at_ms, updated_at_ms FROM repository_issues WHERE number = ?",
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
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 2;
    type Input = CommentKey;
    type Output = Option<CommentRecord>;

    fn execute(
        context: &mut Transaction<'_>,
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
        validate_number(input.number)?;
        validate_number(input.version)?;
        validate_author(&input.actor)?;
        if input.title.is_none()
            && input.body.is_none()
            && input.state.is_none()
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
            || input.state.is_some() && !(author || input.can_triage)
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
                Action::Closed
            };
            activity::append(context, target, &input.actor, action)?;
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

impl IssueSummary {
    pub(in crate::cells::repository) fn load(
        context: &Transaction<'_>,
        number: u64,
        metrics: search::index::Metrics,
    ) -> crate::cells::Result<Self> {
        let result = context.sql(&SqlBatch { statements: vec![statement("SELECT number, author_issuer, author_subject, author_name, title, state, label_ids, assignees, version, created_at_ms, updated_at_ms FROM repository_issues WHERE number = ?", vec![integer(number)?])] })?;
        let row = result[0]
            .rows
            .first()
            .ok_or(crate::cells::Error::Invalid("indexed issue is missing"))?;
        Ok(IssueSummary {
            number: result_u64_from_row(row, 0)?,
            author: RepositoryAuthor {
                issuer: result_text(row, 1)?,
                subject: result_text(row, 2)?,
                name: result_text(row, 3)?,
            },
            title: result_text(row, 4)?,
            state: u8::try_from(result_u64_from_row(row, 5)?)
                .map_err(|_| crate::cells::Error::Invalid("indexed issue state is invalid"))?,
            label_ids: decode_label_ids(result_blob(row, 6)?)?,
            assignees: assignments::decode(result_blob(row, 7)?)?,
            version: result_u64_from_row(row, 8)?,
            created_at_ms: result_u64_from_row(row, 9)?,
            updated_at_ms: result_u64_from_row(row, 10)?,
            comment_count: metrics.comments,
            reaction_count: metrics.reactions,
        })
    }
}

pub(crate) struct ListComments;

impl Read for ListComments {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 4;
    type Input = ListCommentsInput;
    type Output = CommentPage;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        validate_number(input.issue)?;
        validate_list(input.before, input.limit)?;
        let exists = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT 1 FROM repository_issues WHERE number = ?",
                vec![integer(input.issue)?],
            )],
        })?;
        if exists[0].rows.is_empty() {
            return Ok(CommentPage::IssueNotFound);
        }
        let sequence = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT last FROM repository_comment_sequences WHERE issue_number = ?",
                vec![integer(input.issue)?],
            )],
        })?;
        let last = sequence[0]
            .rows
            .first()
            .map(|row| result_u64_from_row(row, 0))
            .transpose()?
            .unwrap_or(0);
        let maximum = input.before.map_or(last, |before| (before - 1).min(last));
        let mut cursor = maximum;
        let mut scanned = 0_u64;
        // Variant tag, collection count and trailing optional cursor are part of
        // the published result bound, not just the encoded comment bodies.
        let mut encoded_bytes = 5_usize;
        let mut items = Vec::new();
        while cursor > 0 && scanned < MAX_LIST_SCAN && items.len() < usize::from(input.limit) {
            let count = 8_u64.min(MAX_LIST_SCAN - scanned).min(cursor);
            let bottom = cursor - count + 1;
            let result = context.sql(&SqlBatch {
                statements: vec![statement(
                    "SELECT issue_number, number, author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms FROM repository_issue_comments WHERE issue_number = ? AND number BETWEEN ? AND ? ORDER BY number DESC",
                    vec![integer(input.issue)?, integer(bottom)?, integer(cursor)?],
                )],
            })?;
            let mut rows = result[0].rows.iter().peekable();
            let mut output_full = false;
            for number in (bottom..=cursor).rev() {
                let matches = rows
                    .peek()
                    .is_some_and(|row| result_u64_from_row(row, 1).ok() == Some(number));
                let row = matches.then(|| rows.next()).flatten();
                scanned += 1;
                cursor -= 1;
                let Some(row) = row else {
                    continue;
                };
                let comment = comment_from_row(row)?;
                let comment_bytes = encoded_size(&comment)?;
                if encoded_bytes
                    .checked_add(comment_bytes)
                    .and_then(|bytes| bytes.checked_add(9))
                    .is_none_or(|bytes| bytes > MAX_LIST_OUTPUT_BYTES)
                {
                    cursor += 1;
                    output_full = true;
                    break;
                }
                encoded_bytes += comment_bytes;
                items.push(comment);
                if items.len() == usize::from(input.limit) {
                    break;
                }
            }
            if output_full {
                break;
            }
        }
        Ok(CommentPage::Found {
            items,
            next: (cursor > 0).then_some(cursor + 1),
        })
    }
}
