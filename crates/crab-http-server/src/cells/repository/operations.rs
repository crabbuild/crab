use super::*;

pub(crate) struct UpdateIssue;

impl Command for UpdateIssue {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 3;
    const CODEC_VERSION: u32 = 1;
    type Input = UpdateIssueInput;
    type Output = UpdateIssueOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        validate_number(input.number)?;
        validate_number(input.version)?;
        validate_author(&input.actor)?;
        if input.title.is_none()
            && input.body.is_none()
            && input.state.is_none()
            && input.label_ids.is_none()
            && input.assignee_subjects.is_none()
        {
            return Err(crab_cell_runtime::Error::Command(
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
            return Err(crab_cell_runtime::Error::Command(
                "repository issue state is invalid",
            ));
        }
        if let Some(labels) = input.label_ids.as_deref() {
            validate_label_ids(labels)?;
        }
        if let Some(assignees) = input.assignee_subjects.as_deref() {
            validate_assignees(assignees)?;
        }

        let current = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT number, author_issuer, author_subject, author_name, title, body, state, label_ids, assignee_subjects, version, created_at_ms, updated_at_ms FROM repository_issues WHERE number = ?",
                vec![integer(input.number)?],
            )],
        })?;
        let Some(row) = current[0].rows.first() else {
            return Ok(CommandResult::Rejected(UpdateIssueOutcome::NotFound));
        };
        let mut issue = issue_from_row(row)?;
        let author_change = input.title.is_some() || input.body.is_some() || input.state.is_some();
        if author_change && !same_author(&issue.author, &input.actor) {
            return Ok(CommandResult::Rejected(UpdateIssueOutcome::Forbidden));
        }
        if input.label_ids.is_some() && !input.can_manage_metadata {
            return Ok(CommandResult::Rejected(UpdateIssueOutcome::LabelForbidden));
        }
        if input.assignee_subjects.is_some() && !input.can_manage_metadata {
            return Ok(CommandResult::Rejected(
                UpdateIssueOutcome::AssigneeForbidden,
            ));
        }
        if issue.version != input.version {
            return Ok(CommandResult::Rejected(UpdateIssueOutcome::Conflict));
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
        if let Some(value) = input.assignee_subjects {
            issue.assignee_subjects = value;
        }
        issue.version = issue
            .version
            .checked_add(1)
            .filter(|version| *version <= MAX_NUMBER)
            .ok_or(crab_cell_runtime::Error::Command(
                "repository issue version is exhausted",
            ))?;
        issue.updated_at_ms = timestamp(context.now_ms())?;
        let updated = context.sql(&SqlBatch {
            statements: vec![statement(
                "UPDATE repository_issues SET title = ?, body = ?, state = ?, label_ids = ?, assignee_subjects = ?, version = ?, updated_at_ms = ? WHERE number = ? AND version = ?",
                vec![
                    SqlValue::Text(issue.title.clone()),
                    SqlValue::Text(issue.body.clone()),
                    SqlValue::Integer(i64::from(issue.state)),
                    SqlValue::Blob(encode_label_ids(&issue.label_ids)?),
                    SqlValue::Blob(encode_assignees(&issue.assignee_subjects)?),
                    integer(issue.version)?,
                    integer(issue.updated_at_ms)?,
                    integer(issue.number)?,
                    integer(input.version)?,
                ],
            )],
        })?;
        if updated[0].rows_affected != 1 {
            return Ok(CommandResult::Rejected(UpdateIssueOutcome::Conflict));
        }
        advance_revision(context)?;
        Ok(CommandResult::Success(UpdateIssueOutcome::Updated(
            Box::new(issue),
        )))
    }
}

pub(crate) struct UpdateComment;

impl Command for UpdateComment {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 4;
    const CODEC_VERSION: u32 = 1;
    type Input = UpdateCommentInput;
    type Output = UpdateCommentOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
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
            return Ok(CommandResult::Rejected(UpdateCommentOutcome::NotFound));
        };
        let mut comment = comment_from_row(row)?;
        if !same_author(&comment.author, &input.actor) {
            return Ok(CommandResult::Rejected(UpdateCommentOutcome::Forbidden));
        }
        if comment.version != input.version {
            return Ok(CommandResult::Rejected(UpdateCommentOutcome::Conflict));
        }
        comment.body = input.body;
        comment.version = comment
            .version
            .checked_add(1)
            .filter(|version| *version <= MAX_NUMBER)
            .ok_or(crab_cell_runtime::Error::Command(
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
            return Ok(CommandResult::Rejected(UpdateCommentOutcome::Conflict));
        }
        advance_revision(context)?;
        Ok(CommandResult::Success(UpdateCommentOutcome::Updated(
            comment,
        )))
    }
}

pub(crate) struct ListIssues;

impl Query for ListIssues {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 3;
    const CODEC_VERSION: u32 = 1;
    type Input = ListIssuesInput;
    type Output = IssuePage;

    fn execute(
        context: &mut QueryContext<'_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
        validate_list(input.before, input.limit)?;
        if input.state > 2 {
            return Err(crab_cell_runtime::Error::Command(
                "repository issue filter is invalid",
            ));
        }
        validate_query(input.query.as_deref())?;
        let sequence = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT last FROM repository_sequences WHERE kind = 'issue'",
                vec![],
            )],
        })?;
        let last = result_u64(&sequence, 0, 0)?;
        let maximum = input.before.map_or(last, |before| (before - 1).min(last));
        let mut cursor = maximum;
        let mut scanned = 0_u64;
        let mut items = Vec::new();
        while cursor > 0 && scanned < MAX_LIST_SCAN && items.len() < usize::from(input.limit) {
            let count = 8_u64.min(MAX_LIST_SCAN - scanned).min(cursor);
            let bottom = cursor - count + 1;
            let result = context.sql(&SqlBatch {
                statements: vec![statement(
                    "SELECT number, author_issuer, author_subject, author_name, title, body, state, label_ids, assignee_subjects, version, created_at_ms, updated_at_ms FROM repository_issues WHERE number BETWEEN ? AND ? ORDER BY number DESC",
                    vec![integer(bottom)?, integer(cursor)?],
                )],
            })?;
            let mut rows = result[0].rows.iter().peekable();
            for number in (bottom..=cursor).rev() {
                let matches = rows
                    .peek()
                    .is_some_and(|row| result_u64_from_row(row, 0).ok() == Some(number));
                let row = matches.then(|| rows.next()).flatten();
                scanned += 1;
                cursor -= 1;
                let Some(row) = row else {
                    continue;
                };
                let issue = issue_from_row(row)?;
                if (input.state == 2 || input.state == issue.state)
                    && matches_query(input.query.as_deref(), &issue)
                {
                    items.push(IssueSummary::from(issue));
                    if items.len() == usize::from(input.limit) {
                        break;
                    }
                }
            }
        }
        Ok(IssuePage {
            items,
            next: (cursor > 0).then_some(cursor + 1),
        })
    }
}

pub(crate) struct ListComments;

impl Query for ListComments {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 4;
    const CODEC_VERSION: u32 = 1;
    type Input = ListCommentsInput;
    type Output = CommentPage;

    fn execute(
        context: &mut QueryContext<'_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
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

impl From<IssueRecord> for IssueSummary {
    fn from(issue: IssueRecord) -> Self {
        Self {
            number: issue.number,
            author: issue.author,
            title: issue.title,
            state: issue.state,
            label_ids: issue.label_ids,
            assignee_subjects: issue.assignee_subjects,
            version: issue.version,
            created_at_ms: issue.created_at_ms,
            updated_at_ms: issue.updated_at_ms,
        }
    }
}
