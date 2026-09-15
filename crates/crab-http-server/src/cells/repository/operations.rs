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
            if !labels.is_empty() {
                let statements = labels
                    .iter()
                    .map(|label| {
                        Ok(statement(
                            "SELECT 1 FROM repository_labels WHERE number = ? AND deleted_version IS NULL",
                            vec![integer(*label)?],
                        ))
                    })
                    .collect::<crab_cell_runtime::Result<Vec<_>>>()?;
                let existing = context.sql(&SqlBatch { statements })?;
                if existing.iter().any(|result| result.rows.is_empty()) {
                    return Ok(CommandResult::Rejected(UpdateIssueOutcome::LabelInvalid));
                }
            }
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

pub(crate) struct CreateLabel;

impl Command for CreateLabel {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 8;
    const CODEC_VERSION: u32 = 1;
    type Input = CreateLabelInput;
    type Output = CreateLabelOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        validate_author(&input.author)?;
        validate_label_fields(&input.name, &input.color, input.description.as_deref())?;
        let payload_digest = label_submission_digest(&input);
        let reservation = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT payload_digest, label_number FROM repository_label_submissions WHERE request_id = ?",
                vec![SqlValue::Blob(input.submission_id.to_vec())],
            )],
        })?;
        if let Some(row) = reservation[0].rows.first() {
            if result_blob(row, 0)? != payload_digest.as_bytes() {
                return Ok(CommandResult::Rejected(CreateLabelOutcome::RequestConflict));
            }
            let number = result_u64_from_row(row, 1)?;
            let current = context.sql(&SqlBatch {
                statements: vec![statement(
                    "SELECT number, name, color, description, version, created_at_ms, updated_at_ms, deleted_version FROM repository_labels WHERE number = ?",
                    vec![integer(number)?],
                )],
            })?;
            let label = current[0]
                .rows
                .first()
                .ok_or(crab_cell_runtime::Error::Command(
                    "repository label submission has no label row",
                ))?;
            if !matches!(label.get(7), Some(SqlValue::Null)) {
                return Ok(CommandResult::Rejected(CreateLabelOutcome::NotFound));
            }
            return Ok(CommandResult::Success(CreateLabelOutcome::Created(
                label_from_row(label)?,
            )));
        }

        let name_key = input.name.to_lowercase();
        let conflict = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT 1 FROM repository_labels WHERE name_key = ? AND deleted_version IS NULL",
                vec![SqlValue::Text(name_key.clone())],
            )],
        })?;
        if !conflict[0].rows.is_empty() {
            return Ok(CommandResult::Rejected(CreateLabelOutcome::NameConflict));
        }
        let sequence = context.sql(&SqlBatch {
            statements: vec![
                statement(
                    "UPDATE repository_sequences SET last = last + 1 WHERE kind = 'label' AND last < 500",
                    vec![],
                ),
                statement(
                    "SELECT last FROM repository_sequences WHERE kind = 'label'",
                    vec![],
                ),
            ],
        })?;
        if sequence[0].rows_affected != 1 {
            return Ok(CommandResult::Rejected(CreateLabelOutcome::LimitReached));
        }
        let now = timestamp(context.now_ms())?;
        let label = LabelRecord {
            number: result_u64(&sequence, 1, 0)?,
            name: input.name,
            color: input.color,
            description: input.description,
            version: 1,
            created_at_ms: now,
            updated_at_ms: now,
        };
        context.sql(&SqlBatch {
            statements: vec![
                statement(
                    "INSERT INTO repository_label_submissions(request_id, payload_digest, label_number) VALUES (?, ?, ?)",
                    vec![
                        SqlValue::Blob(input.submission_id.to_vec()),
                        SqlValue::Blob(payload_digest.as_bytes().to_vec()),
                        integer(label.number)?,
                    ],
                ),
                statement(
                    "INSERT INTO repository_labels(number, name_key, name, color, description, version, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                    vec![
                        integer(label.number)?,
                        SqlValue::Text(name_key),
                        SqlValue::Text(label.name.clone()),
                        SqlValue::Text(label.color.clone()),
                        label
                            .description
                            .clone()
                            .map_or(SqlValue::Null, SqlValue::Text),
                        integer(label.version)?,
                        integer(label.created_at_ms)?,
                        integer(label.updated_at_ms)?,
                    ],
                ),
            ],
        })?;
        advance_revision(context)?;
        Ok(CommandResult::Success(CreateLabelOutcome::Created(label)))
    }
}

pub(crate) struct UpdateLabel;

impl Command for UpdateLabel {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 9;
    const CODEC_VERSION: u32 = 1;
    type Input = UpdateLabelInput;
    type Output = UpdateLabelOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        if input.number == 0 || input.number > MAX_REPOSITORY_LABELS {
            return Err(crab_cell_runtime::Error::Command(
                "repository label number is invalid",
            ));
        }
        validate_number(input.version)?;
        validate_label_fields(&input.name, &input.color, input.description.as_deref())?;
        let current = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT number, name, color, description, version, created_at_ms, updated_at_ms FROM repository_labels WHERE number = ? AND deleted_version IS NULL",
                vec![integer(input.number)?],
            )],
        })?;
        let Some(row) = current[0].rows.first() else {
            return Ok(CommandResult::Rejected(UpdateLabelOutcome::NotFound));
        };
        let mut label = label_from_row(row)?;
        if label.version != input.version {
            return Ok(CommandResult::Rejected(UpdateLabelOutcome::Conflict));
        }
        let name_key = input.name.to_lowercase();
        let conflict = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT 1 FROM repository_labels WHERE name_key = ? AND number != ? AND deleted_version IS NULL",
                vec![SqlValue::Text(name_key.clone()), integer(input.number)?],
            )],
        })?;
        if !conflict[0].rows.is_empty() {
            return Ok(CommandResult::Rejected(UpdateLabelOutcome::NameConflict));
        }
        label.name = input.name;
        label.color = input.color;
        label.description = input.description;
        label.version = label
            .version
            .checked_add(1)
            .filter(|version| *version <= MAX_NUMBER)
            .ok_or(crab_cell_runtime::Error::Command(
                "repository label version is exhausted",
            ))?;
        label.updated_at_ms = timestamp(context.now_ms())?;
        let updated = context.sql(&SqlBatch {
            statements: vec![statement(
                "UPDATE repository_labels SET name_key = ?, name = ?, color = ?, description = ?, version = ?, updated_at_ms = ? WHERE number = ? AND version = ? AND deleted_version IS NULL",
                vec![
                    SqlValue::Text(name_key),
                    SqlValue::Text(label.name.clone()),
                    SqlValue::Text(label.color.clone()),
                    label
                        .description
                        .clone()
                        .map_or(SqlValue::Null, SqlValue::Text),
                    integer(label.version)?,
                    integer(label.updated_at_ms)?,
                    integer(label.number)?,
                    integer(input.version)?,
                ],
            )],
        })?;
        if updated[0].rows_affected != 1 {
            return Ok(CommandResult::Rejected(UpdateLabelOutcome::Conflict));
        }
        advance_revision(context)?;
        Ok(CommandResult::Success(UpdateLabelOutcome::Updated(label)))
    }
}

pub(crate) struct DeleteLabel;

impl Command for DeleteLabel {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 10;
    const CODEC_VERSION: u32 = 1;
    type Input = DeleteLabelInput;
    type Output = DeleteLabelOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        if input.number == 0 || input.number > MAX_REPOSITORY_LABELS {
            return Err(crab_cell_runtime::Error::Command(
                "repository label number is invalid",
            ));
        }
        validate_number(input.version)?;
        let current = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT version, deleted_version FROM repository_labels WHERE number = ?",
                vec![integer(input.number)?],
            )],
        })?;
        let Some(row) = current[0].rows.first() else {
            return Ok(CommandResult::Rejected(DeleteLabelOutcome::NotFound));
        };
        if let Some(SqlValue::Integer(deleted)) = row.get(1) {
            return if u64::try_from(*deleted).ok() == Some(input.version) {
                Ok(CommandResult::Success(DeleteLabelOutcome::Deleted))
            } else {
                Ok(CommandResult::Rejected(DeleteLabelOutcome::NotFound))
            };
        }
        if result_u64_from_row(row, 0)? != input.version {
            return Ok(CommandResult::Rejected(DeleteLabelOutcome::Conflict));
        }
        let deleted = context.sql(&SqlBatch {
            statements: vec![statement(
                "UPDATE repository_labels SET deleted_version = ? WHERE number = ? AND version = ? AND deleted_version IS NULL",
                vec![
                    integer(input.version)?,
                    integer(input.number)?,
                    integer(input.version)?,
                ],
            )],
        })?;
        if deleted[0].rows_affected != 1 {
            return Ok(CommandResult::Rejected(DeleteLabelOutcome::Conflict));
        }
        advance_revision(context)?;
        Ok(CommandResult::Success(DeleteLabelOutcome::Deleted))
    }
}

pub(crate) struct ListLabels;

impl Query for ListLabels {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 6;
    const CODEC_VERSION: u32 = 1;
    type Input = ();
    type Output = LabelCatalog;

    fn execute(
        context: &mut QueryContext<'_>,
        (): Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT number, name, color, description, version, created_at_ms, updated_at_ms FROM repository_labels WHERE deleted_version IS NULL ORDER BY name_key, number",
                vec![],
            )],
        })?;
        if result[0].rows.len() > MAX_REPOSITORY_LABELS as usize {
            return Err(crab_cell_runtime::Error::Command(
                "repository label catalog exceeds its bound",
            ));
        }
        let labels = result[0]
            .rows
            .iter()
            .map(|row| label_from_row(row))
            .collect::<crab_cell_runtime::Result<Vec<_>>>()?;
        Ok(LabelCatalog { labels })
    }
}

pub(crate) struct CreateCommitStatus;

impl Command for CreateCommitStatus {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 11;
    const CODEC_VERSION: u32 = 1;
    type Input = CreateCommitStatusInput;
    type Output = CreateCommitStatusOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        validate_author(&input.author)?;
        validate_status_fields(
            &input.oid,
            &input.context,
            input.state,
            input.description.as_deref(),
            input.target_url.as_deref(),
        )?;
        let payload_digest = status_submission_digest(&input);
        let existing = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT payload_digest, number, request_id, author_issuer, author_subject, author_name, oid, context, state, description, target_url, created_at_ms FROM repository_commit_statuses WHERE oid = ? AND request_id = ?",
                vec![
                    SqlValue::Text(input.oid.clone()),
                    SqlValue::Blob(input.submission_id.to_vec()),
                ],
            )],
        })?;
        if let Some(row) = existing[0].rows.first() {
            if result_blob(row, 0)? != payload_digest.as_bytes() {
                return Ok(CommandResult::Rejected(
                    CreateCommitStatusOutcome::RequestConflict,
                ));
            }
            let status = status_from_row(&row[1..12])?;
            return Ok(CommandResult::Success(CreateCommitStatusOutcome::Created(
                Box::new(status),
            )));
        }

        if !status_context_available(context, &input.oid, &input.context)? {
            return Ok(CommandResult::Rejected(
                CreateCommitStatusOutcome::ContextLimit,
            ));
        }
        let sequence = context.sql(&SqlBatch {
            statements: vec![
                statement(
                    "INSERT INTO repository_status_sequences(oid, last) VALUES (?, 1) ON CONFLICT(oid) DO UPDATE SET last = last + 1 WHERE last < 1000",
                    vec![SqlValue::Text(input.oid.clone())],
                ),
                statement(
                    "SELECT last FROM repository_status_sequences WHERE oid = ?",
                    vec![SqlValue::Text(input.oid.clone())],
                ),
            ],
        })?;
        if sequence[0].rows_affected != 1 {
            return Ok(CommandResult::Rejected(
                CreateCommitStatusOutcome::SubmissionLimit,
            ));
        }
        let status = CommitStatusRecord {
            number: result_u64(&sequence, 1, 0)?,
            submission_id: input.submission_id,
            author: input.author,
            oid: input.oid,
            context: input.context,
            state: input.state,
            description: input.description,
            target_url: input.target_url,
            created_at_ms: timestamp(context.now_ms())?,
        };
        let context_key = status.context.to_lowercase();
        context.sql(&SqlBatch {
            statements: vec![statement(
                "INSERT INTO repository_commit_statuses(oid, request_id, payload_digest, number, author_issuer, author_subject, author_name, context_key, context, state, description, target_url, created_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                vec![
                    SqlValue::Text(status.oid.clone()),
                    SqlValue::Blob(status.submission_id.to_vec()),
                    SqlValue::Blob(payload_digest.as_bytes().to_vec()),
                    integer(status.number)?,
                    SqlValue::Text(status.author.issuer.clone()),
                    SqlValue::Text(status.author.subject.clone()),
                    SqlValue::Text(status.author.name.clone()),
                    SqlValue::Text(context_key),
                    SqlValue::Text(status.context.clone()),
                    SqlValue::Integer(i64::from(status.state)),
                    status
                        .description
                        .clone()
                        .map_or(SqlValue::Null, SqlValue::Text),
                    status
                        .target_url
                        .clone()
                        .map_or(SqlValue::Null, SqlValue::Text),
                    integer(status.created_at_ms)?,
                ],
            )],
        })?;
        advance_revision(context)?;
        Ok(CommandResult::Success(CreateCommitStatusOutcome::Created(
            Box::new(status),
        )))
    }
}

pub(crate) struct ListCommitStatuses;

impl Query for ListCommitStatuses {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 7;
    const CODEC_VERSION: u32 = 1;
    type Input = String;
    type Output = CommitStatusCatalog;

    fn execute(
        context: &mut QueryContext<'_>,
        oid: Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
        validate_status_fields(&oid, "status", 0, None, None)?;
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT number, request_id, author_issuer, author_subject, author_name, oid, context, state, description, target_url, created_at_ms, context_key FROM repository_commit_statuses WHERE oid = ? ORDER BY context_key, number DESC",
                vec![SqlValue::Text(oid)],
            )],
        })?;
        if result[0].rows.len() > MAX_STATUS_SUBMISSIONS as usize {
            return Err(crab_cell_runtime::Error::Command(
                "repository commit status catalog exceeds its bound",
            ));
        }
        let mut statuses = Vec::new();
        let mut previous = None;
        for row in &result[0].rows {
            let key = result_text(row, 11)?;
            if previous.as_deref() == Some(key.as_str()) {
                continue;
            }
            statuses.push(status_from_row(&row[..11])?);
            previous = Some(key);
        }
        if statuses.len() > MAX_STATUS_CONTEXTS as usize {
            return Err(crab_cell_runtime::Error::Command(
                "repository commit status contexts exceed their bound",
            ));
        }
        Ok(CommitStatusCatalog { statuses })
    }
}

pub(crate) struct GetCommitStatusSubmission;

impl Query for GetCommitStatusSubmission {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 8;
    const CODEC_VERSION: u32 = 1;
    type Input = CommitStatusSubmissionKey;
    type Output = Option<CommitStatusRecord>;

    fn execute(
        context: &mut QueryContext<'_>,
        key: Self::Input,
    ) -> crab_cell_runtime::Result<Self::Output> {
        validate_status_fields(&key.oid, "status", 0, None, None)?;
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT number, request_id, author_issuer, author_subject, author_name, oid, context, state, description, target_url, created_at_ms FROM repository_commit_statuses WHERE oid = ? AND request_id = ?",
                vec![
                    SqlValue::Text(key.oid),
                    SqlValue::Blob(key.submission_id.to_vec()),
                ],
            )],
        })?;
        result[0]
            .rows
            .first()
            .map(|row| status_from_row(row))
            .transpose()
    }
}

fn status_context_available(
    context: &CommandContext<'_, '_>,
    oid: &str,
    status_context: &str,
) -> crab_cell_runtime::Result<bool> {
    let context_key = status_context.to_lowercase();
    let result = context.sql(&SqlBatch {
        statements: vec![
            statement(
                "SELECT 1 FROM repository_commit_statuses WHERE oid = ? AND context_key = ? LIMIT 1",
                vec![
                    SqlValue::Text(oid.to_owned()),
                    SqlValue::Text(context_key),
                ],
            ),
            statement(
                "SELECT COUNT(DISTINCT context_key) FROM repository_commit_statuses WHERE oid = ?",
                vec![SqlValue::Text(oid.to_owned())],
            ),
        ],
    })?;
    Ok(!result[0].rows.is_empty() || result_u64(&result, 1, 0)? < MAX_STATUS_CONTEXTS)
}
