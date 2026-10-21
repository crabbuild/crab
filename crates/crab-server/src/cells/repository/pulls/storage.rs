use super::*;

pub(super) fn insert_pull_statement(record: &PullRecord) -> crate::cells::Result<SqlStatement> {
    Ok(statement(
        "INSERT INTO repository_pulls(number, create_request_id, author_issuer, author_subject, author_name, title, body, state, base_ref, base_oid, head_ref, head_oid, label_ids, assignee_subjects, pending_merge, completed_merge, version, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        vec![
            integer(record.number)?,
            SqlValue::Blob(record.create_submission_id.to_vec()),
            SqlValue::Text(record.author.issuer.clone()),
            SqlValue::Text(record.author.subject.clone()),
            SqlValue::Text(record.author.name.clone()),
            SqlValue::Text(record.title.clone()),
            SqlValue::Text(record.body.clone()),
            SqlValue::Integer(i64::from(record.state.code())),
            SqlValue::Text(record.base_ref.clone()),
            SqlValue::Text(record.base_oid.clone()),
            SqlValue::Text(record.head_ref.clone()),
            SqlValue::Text(record.head_oid.clone()),
            SqlValue::Blob(encode_label_ids(&record.label_ids)?),
            SqlValue::Blob(encode_assignees(&record.assignee_subjects)?),
            optional_record(&record.merge_pending)?,
            optional_record(&record.merge)?,
            integer(record.version)?,
            integer(record.created_at_ms)?,
            integer(record.updated_at_ms)?,
        ],
    ))
}

pub(super) fn update_pull_row(
    context: &RepositoryTransaction<'_>,
    record: &PullRecord,
    expected: u64,
) -> crate::cells::Result<()> {
    let result = context.sql(&SqlBatch { statements: vec![statement("UPDATE repository_pulls SET title = ?, body = ?, state = ?, label_ids = ?, assignee_subjects = ?, pending_merge = ?, completed_merge = ?, version = ?, updated_at_ms = ? WHERE number = ? AND version = ?", vec![SqlValue::Text(record.title.clone()), SqlValue::Text(record.body.clone()), SqlValue::Integer(i64::from(record.state.code())), SqlValue::Blob(encode_label_ids(&record.label_ids)?), SqlValue::Blob(encode_assignees(&record.assignee_subjects)?), optional_record(&record.merge_pending)?, optional_record(&record.merge)?, integer(record.version)?, integer(record.updated_at_ms)?, integer(record.number)?, integer(expected)?])] })?;
    if result[0].rows_affected != 1 {
        return Err(crate::cells::Error::Invalid(
            "repository pull update lost its transaction",
        ));
    }
    Ok(())
}

fn optional_record<T: StoredRecord>(value: &Option<T>) -> crate::cells::Result<SqlValue> {
    match value {
        Some(value) => {
            let mut encoder = RecordEncoder::new(128 * 1024)
                .map_err(|_| crate::cells::Error::Invalid("repository pull value is too large"))?;
            value
                .encode(&mut encoder)
                .map_err(|_| crate::cells::Error::Invalid("repository pull value is too large"))?;
            Ok(SqlValue::Blob(encoder.finish()))
        }
        None => Ok(SqlValue::Null),
    }
}

fn decode_optional_record<T: StoredRecord>(value: &SqlValue) -> crate::cells::Result<Option<T>> {
    match value {
        SqlValue::Null => Ok(None),
        SqlValue::Blob(bytes) => {
            let mut decoder = RecordDecoder::new(bytes, 128 * 1024)
                .map_err(|_| crate::cells::Error::Invalid("repository pull value is invalid"))?;
            let value = T::decode(&mut decoder)
                .map_err(|_| crate::cells::Error::Invalid("repository pull value is invalid"))?;
            decoder
                .finish()
                .map_err(|_| crate::cells::Error::Invalid("repository pull value is invalid"))?;
            Ok(Some(value))
        }
        _ => Err(crate::cells::Error::Invalid(
            "repository pull value is invalid",
        )),
    }
}

fn pull_select() -> &'static str {
    "SELECT number, create_request_id, author_issuer, author_subject, author_name, title, body, state, base_ref, base_oid, head_ref, head_oid, label_ids, assignee_subjects, pending_merge, completed_merge, version, created_at_ms, updated_at_ms FROM repository_pulls WHERE number = ?"
}

pub(super) fn load_pull(
    context: &RepositoryTransaction<'_>,
    number: u64,
) -> crate::cells::Result<Option<PullRecord>> {
    let result = context.sql(&SqlBatch { statements: vec![statement(pull_select(), vec![integer(number)?]), statement("SELECT review_number, author_issuer, author_subject, author_name, state, commit_oid FROM repository_pull_review_decisions WHERE pull_number = ? ORDER BY review_number", vec![integer(number)?])] })?;
    result[0]
        .rows
        .first()
        .map(|row| pull_from_rows(context, row, &result[1].rows))
        .transpose()
}

pub(super) fn load_pull_query(
    context: &RepositoryTransaction<'_>,
    number: u64,
) -> crate::cells::Result<Option<PullRecord>> {
    let result = context.sql(&SqlBatch { statements: vec![statement(pull_select(), vec![integer(number)?]), statement("SELECT review_number, author_issuer, author_subject, author_name, state, commit_oid FROM repository_pull_review_decisions WHERE pull_number = ? ORDER BY review_number", vec![integer(number)?])] })?;
    result[0]
        .rows
        .first()
        .map(|row| pull_from_rows(context, row, &result[1].rows))
        .transpose()
}

fn pull_from_rows(
    context: &RepositoryTransaction<'_>,
    row: &[SqlValue],
    decisions: &[Vec<SqlValue>],
) -> crate::cells::Result<PullRecord> {
    let submission = <[u8; 16]>::try_from(result_blob(row, 1)?)
        .map_err(|_| crate::cells::Error::Invalid("repository pull submission ID is invalid"))?;
    let review_decisions = decisions
        .iter()
        .map(|decision| {
            Ok(PullReviewDecision {
                review: result_u64_from_row(decision, 0)?,
                author: author_from_row(decision, 1)?,
                state: ReviewState::from_code(result_u64_from_row(decision, 4)?)?,
                commit_oid: result_text(decision, 5)?,
            })
        })
        .collect::<crate::cells::Result<Vec<_>>>()?;
    let number = result_u64_from_row(row, 0)?;
    Ok(PullRecord {
        draft: readiness::load(context, number)? == PullReadiness::Draft,
        number,
        create_submission_id: submission,
        author: author_from_row(row, 2)?,
        title: result_text(row, 5)?,
        body: result_text(row, 6)?,
        state: PullState::from_code(result_u64_from_row(row, 7)?)?,
        base_ref: result_text(row, 8)?,
        base_oid: result_text(row, 9)?,
        head_ref: result_text(row, 10)?,
        head_oid: result_text(row, 11)?,
        label_ids: decode_label_ids(result_blob(row, 12)?)?,
        assignee_subjects: decode_assignees(result_blob(row, 13)?)?,
        merge_pending: decode_optional_record(&row[14])?,
        merge: decode_optional_record(&row[15])?,
        review_decisions,
        version: result_u64_from_row(row, 16)?,
        created_at_ms: result_u64_from_row(row, 17)?,
        updated_at_ms: result_u64_from_row(row, 18)?,
    })
}

pub(super) fn author_from_row(
    row: &[SqlValue],
    start: usize,
) -> crate::cells::Result<RepositoryAuthor> {
    Ok(RepositoryAuthor {
        issuer: result_text(row, start)?,
        subject: result_text(row, start + 1)?,
        name: result_text(row, start + 2)?,
    })
}

pub(super) fn next_child_number(
    context: &RepositoryTransaction<'_>,
    table: &str,
    pull: u64,
) -> crate::cells::Result<u64> {
    let sql = format!(
        "INSERT INTO {table}(pull_number, last) VALUES (?, 1) ON CONFLICT(pull_number) DO UPDATE SET last = last + 1 WHERE last < 9007199254740991"
    );
    let select = format!("SELECT last FROM {table} WHERE pull_number = ?");
    let result = context.sql(&SqlBatch {
        statements: vec![
            statement(&sql, vec![integer(pull)?]),
            statement(&select, vec![integer(pull)?]),
        ],
    })?;
    if result[0].rows_affected != 1 {
        return Err(crate::cells::Error::Invalid(
            "repository pull child numbering is exhausted",
        ));
    }
    result_u64(&result, 1, 0)
}

pub(super) fn pull_child_digest(
    domain: &[u8],
    pull: u64,
    author: &RepositoryAuthor,
    body: &str,
    state: Option<u8>,
    oid: Option<&str>,
) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain);
    hasher.update(&pull.to_be_bytes());
    hash_text(&mut hasher, &author.issuer);
    hash_text(&mut hasher, &author.subject);
    hash_text(&mut hasher, body);
    if let Some(state) = state {
        hasher.update(&[state]);
    }
    if let Some(oid) = oid {
        hash_text(&mut hasher, oid);
    }
    hasher.finalize()
}
pub(super) fn next_version(value: u64) -> crate::cells::Result<u64> {
    value
        .checked_add(1)
        .filter(|value| *value < MAX_NUMBER)
        .ok_or(crate::cells::Error::Invalid(
            "repository pull version is exhausted",
        ))
}
pub(super) fn encode_record<T: StoredRecord>(value: &T) -> crate::cells::Result<Vec<u8>> {
    let mut encoder = RecordEncoder::new(128 * 1024)
        .map_err(|_| crate::cells::Error::Invalid("repository pull value is too large"))?;
    value
        .encode(&mut encoder)
        .map_err(|_| crate::cells::Error::Invalid("repository pull value is too large"))?;
    Ok(encoder.finish())
}
pub(super) fn decode_record<T: StoredRecord>(bytes: &[u8]) -> crate::cells::Result<T> {
    let mut decoder = RecordDecoder::new(bytes, 128 * 1024)
        .map_err(|_| crate::cells::Error::Invalid("repository pull value is invalid"))?;
    let value = T::decode(&mut decoder)
        .map_err(|_| crate::cells::Error::Invalid("repository pull value is invalid"))?;
    decoder
        .finish()
        .map_err(|_| crate::cells::Error::Invalid("repository pull value is invalid"))?;
    Ok(value)
}

pub(super) fn load_comment(
    context: &RepositoryTransaction<'_>,
    key: PullChildKey,
) -> crate::cells::Result<Option<PullCommentRecord>> {
    let result = context.sql(&SqlBatch { statements: vec![statement("SELECT pull_number, number, author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms FROM repository_pull_comments WHERE pull_number = ? AND number = ?", vec![integer(key.pull)?, integer(key.number)?])] })?;
    result[0]
        .rows
        .first()
        .map(|row| comment_from_row(row))
        .transpose()
}
pub(super) fn load_comment_query(
    context: &RepositoryTransaction<'_>,
    key: PullChildKey,
) -> crate::cells::Result<Option<PullCommentRecord>> {
    validate_number(key.pull)?;
    validate_number(key.number)?;
    let result = context.sql(&SqlBatch { statements: vec![statement("SELECT pull_number, number, author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms FROM repository_pull_comments WHERE pull_number = ? AND number = ?", vec![integer(key.pull)?, integer(key.number)?])] })?;
    result[0]
        .rows
        .first()
        .map(|row| comment_from_row(row))
        .transpose()
}
pub(super) fn comment_from_row(row: &[SqlValue]) -> crate::cells::Result<PullCommentRecord> {
    Ok(PullCommentRecord {
        pull: result_u64_from_row(row, 0)?,
        number: result_u64_from_row(row, 1)?,
        author: author_from_row(row, 2)?,
        body: result_text(row, 5)?,
        version: result_u64_from_row(row, 6)?,
        created_at_ms: result_u64_from_row(row, 7)?,
        updated_at_ms: result_u64_from_row(row, 8)?,
    })
}
pub(super) fn load_review(
    context: &RepositoryTransaction<'_>,
    key: PullChildKey,
) -> crate::cells::Result<Option<PullReviewRecord>> {
    let result = context.sql(&SqlBatch { statements: vec![statement("SELECT pull_number, number, author_issuer, author_subject, author_name, body, state, commit_oid, version, created_at_ms, updated_at_ms FROM repository_pull_reviews WHERE pull_number = ? AND number = ?", vec![integer(key.pull)?, integer(key.number)?])] })?;
    result[0]
        .rows
        .first()
        .map(|row| review_from_row(row))
        .transpose()
}
pub(super) fn load_review_query(
    context: &RepositoryTransaction<'_>,
    key: PullChildKey,
) -> crate::cells::Result<Option<PullReviewRecord>> {
    validate_number(key.pull)?;
    validate_number(key.number)?;
    let result = context.sql(&SqlBatch { statements: vec![statement("SELECT pull_number, number, author_issuer, author_subject, author_name, body, state, commit_oid, version, created_at_ms, updated_at_ms FROM repository_pull_reviews WHERE pull_number = ? AND number = ?", vec![integer(key.pull)?, integer(key.number)?])] })?;
    result[0]
        .rows
        .first()
        .map(|row| review_from_row(row))
        .transpose()
}
pub(super) fn review_from_row(row: &[SqlValue]) -> crate::cells::Result<PullReviewRecord> {
    Ok(PullReviewRecord {
        pull: result_u64_from_row(row, 0)?,
        number: result_u64_from_row(row, 1)?,
        author: author_from_row(row, 2)?,
        body: result_text(row, 5)?,
        state: ReviewState::from_code(result_u64_from_row(row, 6)?)?,
        commit_oid: result_text(row, 7)?,
        version: result_u64_from_row(row, 8)?,
        created_at_ms: result_u64_from_row(row, 9)?,
        updated_at_ms: result_u64_from_row(row, 10)?,
    })
}
pub(super) fn list_child_rows(
    context: &RepositoryTransaction<'_>,
    table: &str,
    input: &PullChildListInput,
    columns: &str,
) -> crate::cells::Result<Vec<Vec<SqlValue>>> {
    validate_number(input.pull)?;
    validate_list(input.before, input.limit)?;
    let upper = input.before.map_or(MAX_NUMBER, |value| value - 1);
    let sql = format!(
        "SELECT pull_number, number, {columns} FROM {table} WHERE pull_number = ? AND number <= ? ORDER BY number DESC LIMIT ?"
    );
    let result = context.sql(&SqlBatch {
        statements: vec![statement(
            &sql,
            vec![
                integer(input.pull)?,
                integer(upper)?,
                SqlValue::Integer(i64::from(input.limit)),
            ],
        )],
    })?;
    Ok(result[0].rows.clone())
}
pub(super) fn bounded_child_page<T: Serialize>(
    items: Vec<T>,
    requested: u8,
    number: impl Fn(&T) -> u64,
) -> crate::cells::Result<(Vec<T>, Option<u64>)> {
    let total = items.len();
    let mut used =
        4 + br#"{"items":["#.len() + br#"],"next":"#.len() + MAX_NUMBER.to_string().len() + 1;
    let mut kept = Vec::with_capacity(total);
    for item in items {
        let encoded = serde_json::to_vec(&item)
            .map_err(|_| crate::cells::Error::Invalid("repository pull child is invalid"))?;
        let separator = usize::from(!kept.is_empty());
        if used
            .checked_add(separator + encoded.len())
            .is_none_or(|size| size > MAX_PAGE_BYTES)
        {
            break;
        }
        used += separator + encoded.len();
        kept.push(item);
    }
    if kept.is_empty() && total != 0 {
        return Err(crate::cells::Error::Invalid(
            "repository pull child exceeds the page limit",
        ));
    }
    let next = (kept.len() < total || total == usize::from(requested))
        .then(|| kept.last().map(&number))
        .flatten();
    Ok((kept, next))
}
