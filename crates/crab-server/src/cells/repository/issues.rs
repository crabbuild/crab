use super::*;

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
