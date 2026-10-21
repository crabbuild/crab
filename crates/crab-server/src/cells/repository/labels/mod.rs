use super::*;

pub(crate) struct CreateLabel;

impl Mutation for CreateLabel {
    const ID: u32 = 8;
    type Input = CreateLabelInput;
    type Output = CreateLabelOutcome;

    fn execute(
        context: &mut RepositoryTransaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(CreateLabelOutcome::Archived));
        }
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
                return Ok(MutationResult::Rejected(
                    CreateLabelOutcome::RequestConflict,
                ));
            }
            let number = result_u64_from_row(row, 1)?;
            let current = context.sql(&SqlBatch {
                statements: vec![statement(
                    "SELECT number, name, color, description, version, created_at_ms, updated_at_ms, deleted_version FROM repository_labels WHERE number = ?",
                    vec![integer(number)?],
                )],
            })?;
            let label = current[0].rows.first().ok_or(crate::cells::Error::Invalid(
                "repository label submission has no label row",
            ))?;
            if !matches!(label.get(7), Some(SqlValue::Null)) {
                return Ok(MutationResult::Rejected(CreateLabelOutcome::NotFound));
            }
            return Ok(MutationResult::Success(CreateLabelOutcome::Created(
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
            return Ok(MutationResult::Rejected(CreateLabelOutcome::NameConflict));
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
            return Ok(MutationResult::Rejected(CreateLabelOutcome::LimitReached));
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
        Ok(MutationResult::Success(CreateLabelOutcome::Created(label)))
    }
}

pub(crate) struct UpdateLabel;

impl Mutation for UpdateLabel {
    const ID: u32 = 9;
    type Input = UpdateLabelInput;
    type Output = UpdateLabelOutcome;

    fn execute(
        context: &mut RepositoryTransaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(UpdateLabelOutcome::Archived));
        }
        if input.number == 0 || input.number > MAX_REPOSITORY_LABELS {
            return Err(crate::cells::Error::Invalid(
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
            return Ok(MutationResult::Rejected(UpdateLabelOutcome::NotFound));
        };
        let mut label = label_from_row(row)?;
        if label.version != input.version {
            return Ok(MutationResult::Rejected(UpdateLabelOutcome::Conflict));
        }
        let name_key = input.name.to_lowercase();
        let conflict = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT 1 FROM repository_labels WHERE name_key = ? AND number != ? AND deleted_version IS NULL",
                vec![SqlValue::Text(name_key.clone()), integer(input.number)?],
            )],
        })?;
        if !conflict[0].rows.is_empty() {
            return Ok(MutationResult::Rejected(UpdateLabelOutcome::NameConflict));
        }
        label.name = input.name;
        label.color = input.color;
        label.description = input.description;
        label.version = label
            .version
            .checked_add(1)
            .filter(|version| *version <= MAX_NUMBER)
            .ok_or(crate::cells::Error::Invalid(
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
            return Ok(MutationResult::Rejected(UpdateLabelOutcome::Conflict));
        }
        advance_revision(context)?;
        Ok(MutationResult::Success(UpdateLabelOutcome::Updated(label)))
    }
}

pub(crate) struct DeleteLabel;

impl Mutation for DeleteLabel {
    const ID: u32 = 10;
    type Input = DeleteLabelInput;
    type Output = DeleteLabelOutcome;

    fn execute(
        context: &mut RepositoryTransaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(DeleteLabelOutcome::Archived));
        }
        if input.number == 0 || input.number > MAX_REPOSITORY_LABELS {
            return Err(crate::cells::Error::Invalid(
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
            return Ok(MutationResult::Rejected(DeleteLabelOutcome::NotFound));
        };
        if let Some(SqlValue::Integer(deleted)) = row.get(1) {
            return if u64::try_from(*deleted).ok() == Some(input.version) {
                Ok(MutationResult::Success(DeleteLabelOutcome::Deleted))
            } else {
                Ok(MutationResult::Rejected(DeleteLabelOutcome::NotFound))
            };
        }
        if result_u64_from_row(row, 0)? != input.version {
            return Ok(MutationResult::Rejected(DeleteLabelOutcome::Conflict));
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
            return Ok(MutationResult::Rejected(DeleteLabelOutcome::Conflict));
        }
        advance_revision(context)?;
        Ok(MutationResult::Success(DeleteLabelOutcome::Deleted))
    }
}

pub(crate) struct ListLabels;

impl Read for ListLabels {
    const ID: u32 = 6;
    type Input = ();
    type Output = LabelCatalog;

    fn execute(
        context: &mut RepositoryTransaction<'_>,
        (): Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        let result = context.sql(&SqlBatch {
            statements: vec![statement(
                "SELECT number, name, color, description, version, created_at_ms, updated_at_ms FROM repository_labels WHERE deleted_version IS NULL ORDER BY name_key, number",
                vec![],
            )],
        })?;
        if result[0].rows.len() > MAX_REPOSITORY_LABELS as usize {
            return Err(crate::cells::Error::Invalid(
                "repository label catalog exceeds its bound",
            ));
        }
        let labels = result[0]
            .rows
            .iter()
            .map(|row| label_from_row(row))
            .collect::<crate::cells::Result<Vec<_>>>()?;
        Ok(LabelCatalog { labels })
    }
}

fn label_submission_digest(input: &CreateLabelInput) -> blake3::Hash {
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

#[cfg(test)]
mod tests;
