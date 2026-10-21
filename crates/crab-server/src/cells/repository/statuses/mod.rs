use super::*;

pub(crate) struct CreateCommitStatus;

impl Mutation for CreateCommitStatus {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 11;
    type Input = CreateCommitStatusInput;
    type Output = CreateCommitStatusOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
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
                return Ok(MutationResult::Rejected(
                    CreateCommitStatusOutcome::RequestConflict,
                ));
            }
            let status = status_from_row(&row[1..12])?;
            return Ok(MutationResult::Success(CreateCommitStatusOutcome::Created(
                Box::new(status),
            )));
        }

        if !status_context_available(context, &input.oid, &input.context)? {
            return Ok(MutationResult::Rejected(
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
            return Ok(MutationResult::Rejected(
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
        Ok(MutationResult::Success(CreateCommitStatusOutcome::Created(
            Box::new(status),
        )))
    }
}

pub(crate) struct ListCommitStatuses;

impl Read for ListCommitStatuses {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 7;
    type Input = String;
    type Output = CommitStatusCatalog;

    fn execute(
        context: &mut Transaction<'_>,
        oid: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        validate_status_fields(&oid, "status", 0, None, None)?;
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT number, request_id, author_issuer, author_subject, author_name, oid, context, state, description, target_url, created_at_ms, context_key FROM repository_commit_statuses WHERE oid = ? ORDER BY context_key, number DESC",
                vec![SqlValue::Text(oid)],
            )],
        })?;
        if result[0].rows.len() > MAX_STATUS_SUBMISSIONS as usize {
            return Err(crate::cells::Error::Invalid(
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
            return Err(crate::cells::Error::Invalid(
                "repository commit status contexts exceed their bound",
            ));
        }
        Ok(CommitStatusCatalog { statuses })
    }
}

pub(crate) struct GetCommitStatusSubmission;

impl Read for GetCommitStatusSubmission {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 8;
    type Input = CommitStatusSubmissionKey;
    type Output = Option<CommitStatusRecord>;

    fn execute(
        context: &mut Transaction<'_>,
        key: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
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
    context: &Transaction<'_>,
    oid: &str,
    status_context: &str,
) -> crate::cells::Result<bool> {
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

fn status_submission_digest(input: &CreateCommitStatusInput) -> blake3::Hash {
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

#[cfg(test)]
mod tests;
