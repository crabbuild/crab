use super::*;

pub(super) fn pull_state(
    context: &RepositoryTransaction<'_>,
    pull: u64,
) -> crate::cells::Result<Option<u64>> {
    let result = context.sql(&SqlBatch {
        statements: vec![statement(
            "SELECT state FROM repository_pulls WHERE number = ?",
            vec![integer(pull)?],
        )],
    })?;
    result[0]
        .rows
        .first()
        .map(|row| result_u64_from_row(row, 0))
        .transpose()
}

pub(super) fn next_number(
    context: &RepositoryTransaction<'_>,
    table: &str,
    pull: u64,
) -> crate::cells::Result<u64> {
    let insert = format!(
        "INSERT INTO {table}(pull_number, last) VALUES (?, 1) ON CONFLICT(pull_number) DO UPDATE SET last = last + 1 WHERE last < 9007199254740991"
    );
    let result = context.sql(&SqlBatch {
        statements: vec![
            statement(&insert, vec![integer(pull)?]),
            statement(
                &format!("SELECT last FROM {table} WHERE pull_number = ?"),
                vec![integer(pull)?],
            ),
        ],
    })?;
    if result[0].rows_affected != 1 {
        return Err(crate::cells::Error::Invalid(
            "repository pull review numbering is exhausted",
        ));
    }
    result_u64(&result, 1, 0)
}

pub(super) fn next_reply_number(
    context: &RepositoryTransaction<'_>,
    pull: u64,
    thread: u64,
) -> crate::cells::Result<u64> {
    let result = context.sql(&SqlBatch {
        statements: vec![
            statement(
                "INSERT INTO repository_pull_review_reply_sequences(pull_number, thread_number, last) VALUES (?, ?, 1) ON CONFLICT(pull_number, thread_number) DO UPDATE SET last = last + 1 WHERE last < 9007199254740991",
                vec![integer(pull)?, integer(thread)?],
            ),
            statement(
                "SELECT last FROM repository_pull_review_reply_sequences WHERE pull_number = ? AND thread_number = ?",
                vec![integer(pull)?, integer(thread)?],
            ),
        ],
    })?;
    if result[0].rows_affected != 1 {
        return Err(crate::cells::Error::Invalid(
            "repository pull review reply numbering is exhausted",
        ));
    }
    result_u64(&result, 1, 0)
}

pub(super) fn thread_digest(input: &CreatePullReviewThreadInput) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.repository.pull-review-thread.v1\0");
    hasher.update(&input.pull.to_be_bytes());
    hash_text(&mut hasher, &input.author.issuer);
    hash_text(&mut hasher, &input.author.subject);
    hash_text(&mut hasher, &input.body);
    match &input.suggested_text {
        Some(value) => {
            hasher.update(&[1]);
            hash_text(&mut hasher, value);
        }
        None => {
            hasher.update(&[0]);
        }
    }
    hash_text(&mut hasher, &input.base_oid);
    hash_text(&mut hasher, &input.head_oid);
    hasher.update(&(input.path.len() as u64).to_be_bytes());
    hasher.update(&input.path);
    for value in [&input.old_blob_oid, &input.new_blob_oid] {
        match value {
            Some(value) => {
                hasher.update(&[1]);
                hash_text(&mut hasher, value);
            }
            None => {
                hasher.update(&[0]);
            }
        }
    }
    hasher.update(&[input.side.code()]);
    hasher.update(&input.start_line.to_be_bytes());
    hasher.update(&input.end_line.to_be_bytes());
    hasher.finalize()
}

pub(super) fn reply_digest(input: &CreatePullReviewReplyInput) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.repository.pull-review-reply.v1\0");
    hasher.update(&input.pull.to_be_bytes());
    hasher.update(&input.thread.to_be_bytes());
    hash_text(&mut hasher, &input.author.issuer);
    hash_text(&mut hasher, &input.author.subject);
    hash_text(&mut hasher, &input.body);
    hasher.finalize()
}

pub(super) fn insert_thread_statement(
    record: &PullReviewThreadRecord,
) -> crate::cells::Result<SqlStatement> {
    Ok(statement(
        "INSERT INTO repository_pull_review_threads(pull_number, number, author_issuer, author_subject, author_name, body, suggested_text, base_oid, head_oid, path, old_blob_oid, new_blob_oid, side, start_line, end_line, resolved, resolved_by_issuer, resolved_by_subject, resolved_by_name, resolved_at_ms, version, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        vec![
            integer(record.pull)?,
            integer(record.number)?,
            SqlValue::Text(record.author.issuer.clone()),
            SqlValue::Text(record.author.subject.clone()),
            SqlValue::Text(record.author.name.clone()),
            SqlValue::Text(record.body.clone()),
            optional_text(&record.suggested_text),
            SqlValue::Text(record.base_oid.clone()),
            SqlValue::Text(record.head_oid.clone()),
            SqlValue::Blob(record.path.clone()),
            optional_text(&record.old_blob_oid),
            optional_text(&record.new_blob_oid),
            SqlValue::Integer(i64::from(record.side.code())),
            integer(record.start_line)?,
            integer(record.end_line)?,
            SqlValue::Integer(if record.resolved { 1 } else { 0 }),
            optional_author_field(&record.resolved_by, |author| &author.issuer),
            optional_author_field(&record.resolved_by, |author| &author.subject),
            optional_author_field(&record.resolved_by, |author| &author.name),
            optional_integer(record.resolved_at_ms)?,
            integer(record.version)?,
            integer(record.created_at_ms)?,
            integer(record.updated_at_ms)?,
        ],
    ))
}

pub(super) fn update_thread_statement(
    record: &PullReviewThreadRecord,
    old_version: u64,
) -> crate::cells::Result<SqlStatement> {
    let mut statement = insert_thread_statement(record)?;
    statement.sql = "UPDATE repository_pull_review_threads SET body = ?, suggested_text = ?, resolved = ?, resolved_by_issuer = ?, resolved_by_subject = ?, resolved_by_name = ?, resolved_at_ms = ?, version = ?, updated_at_ms = ? WHERE pull_number = ? AND number = ? AND version = ?".into();
    statement.parameters = vec![
        SqlValue::Text(record.body.clone()),
        optional_text(&record.suggested_text),
        SqlValue::Integer(if record.resolved { 1 } else { 0 }),
        optional_author_field(&record.resolved_by, |author| &author.issuer),
        optional_author_field(&record.resolved_by, |author| &author.subject),
        optional_author_field(&record.resolved_by, |author| &author.name),
        optional_integer(record.resolved_at_ms)?,
        integer(record.version)?,
        integer(record.updated_at_ms)?,
        integer(record.pull)?,
        integer(record.number)?,
        integer(old_version)?,
    ];
    Ok(statement)
}

pub(super) fn insert_reply_statement(
    record: &PullReviewReplyRecord,
) -> crate::cells::Result<SqlStatement> {
    Ok(statement(
        "INSERT INTO repository_pull_review_replies(pull_number, thread_number, number, author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        vec![
            integer(record.pull)?,
            integer(record.thread)?,
            integer(record.number)?,
            SqlValue::Text(record.author.issuer.clone()),
            SqlValue::Text(record.author.subject.clone()),
            SqlValue::Text(record.author.name.clone()),
            SqlValue::Text(record.body.clone()),
            integer(record.version)?,
            integer(record.created_at_ms)?,
            integer(record.updated_at_ms)?,
        ],
    ))
}

fn optional_text(value: &Option<String>) -> SqlValue {
    value
        .as_ref()
        .map_or(SqlValue::Null, |value| SqlValue::Text(value.clone()))
}

fn optional_integer(value: Option<u64>) -> crate::cells::Result<SqlValue> {
    value.map_or(Ok(SqlValue::Null), integer)
}

fn optional_author_field(
    value: &Option<RepositoryAuthor>,
    field: impl Fn(&RepositoryAuthor) -> &String,
) -> SqlValue {
    value.as_ref().map_or(SqlValue::Null, |author| {
        SqlValue::Text(field(author).clone())
    })
}

pub(super) fn load_thread(
    context: &RepositoryTransaction<'_>,
    key: PullReviewThreadKey,
) -> crate::cells::Result<Option<PullReviewThreadRecord>> {
    validate_thread_key(&key)?;
    let result = context.sql(&SqlBatch {
        statements: vec![thread_select_statement(key.pull, key.number)?],
    })?;
    result[0]
        .rows
        .first()
        .map(|row| thread_from_row(row))
        .transpose()
}

pub(super) fn load_thread_query(
    context: &RepositoryTransaction<'_>,
    key: PullReviewThreadKey,
) -> crate::cells::Result<Option<PullReviewThreadRecord>> {
    validate_thread_key(&key)?;
    let result = context.sql(&SqlBatch {
        statements: vec![thread_select_statement(key.pull, key.number)?],
    })?;
    result[0]
        .rows
        .first()
        .map(|row| thread_from_row(row))
        .transpose()
}

fn thread_select_statement(pull: u64, number: u64) -> crate::cells::Result<SqlStatement> {
    Ok(statement(
        "SELECT pull_number, number, author_issuer, author_subject, author_name, body, suggested_text, base_oid, head_oid, path, old_blob_oid, new_blob_oid, side, start_line, end_line, resolved, resolved_by_issuer, resolved_by_subject, resolved_by_name, resolved_at_ms, version, created_at_ms, updated_at_ms FROM repository_pull_review_threads WHERE pull_number = ? AND number = ?",
        vec![integer(pull)?, integer(number)?],
    ))
}

pub(super) fn thread_from_row(row: &[SqlValue]) -> crate::cells::Result<PullReviewThreadRecord> {
    let resolved = result_u64_from_row(row, 15)?;
    let resolved_by = optional_author(row, 16)?;
    let resolved_at_ms = result_optional_u64(row, 19)?;
    if (resolved == 0) != resolved_by.is_none() || (resolved == 0) != resolved_at_ms.is_none() {
        return Err(crate::cells::Error::Invalid(
            "repository pull review thread resolution is invalid",
        ));
    }
    let record = PullReviewThreadRecord {
        pull: result_u64_from_row(row, 0)?,
        number: result_u64_from_row(row, 1)?,
        author: author_from_row(row, 2)?,
        body: result_text(row, 5)?,
        suggested_text: result_optional_text(row, 6)?,
        base_oid: result_text(row, 7)?,
        head_oid: result_text(row, 8)?,
        path: result_blob(row, 9)?.to_vec(),
        old_blob_oid: result_optional_text(row, 10)?,
        new_blob_oid: result_optional_text(row, 11)?,
        side: PullReviewThreadSide::from_code(result_u64_from_row(row, 12)?)?,
        start_line: result_u64_from_row(row, 13)?,
        end_line: result_u64_from_row(row, 14)?,
        resolved: resolved != 0,
        resolved_by,
        resolved_at_ms,
        version: result_u64_from_row(row, 20)?,
        created_at_ms: result_u64_from_row(row, 21)?,
        updated_at_ms: result_u64_from_row(row, 22)?,
    };
    validate_thread_record(&record)?;
    Ok(record)
}

pub(super) fn load_reply(
    context: &RepositoryTransaction<'_>,
    key: PullReviewReplyKey,
) -> crate::cells::Result<Option<PullReviewReplyRecord>> {
    validate_reply_key(&key)?;
    let result = context.sql(&SqlBatch {
        statements: vec![reply_select_statement(&key)?],
    })?;
    result[0]
        .rows
        .first()
        .map(|row| reply_from_row(row))
        .transpose()
}

pub(super) fn load_reply_query(
    context: &RepositoryTransaction<'_>,
    key: PullReviewReplyKey,
) -> crate::cells::Result<Option<PullReviewReplyRecord>> {
    validate_reply_key(&key)?;
    let result = context.sql(&SqlBatch {
        statements: vec![reply_select_statement(&key)?],
    })?;
    result[0]
        .rows
        .first()
        .map(|row| reply_from_row(row))
        .transpose()
}

fn reply_select_statement(key: &PullReviewReplyKey) -> crate::cells::Result<SqlStatement> {
    Ok(statement(
        "SELECT pull_number, thread_number, number, author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms FROM repository_pull_review_replies WHERE pull_number = ? AND thread_number = ? AND number = ?",
        vec![
            integer(key.pull)?,
            integer(key.thread)?,
            integer(key.number)?,
        ],
    ))
}

pub(super) fn reply_from_row(row: &[SqlValue]) -> crate::cells::Result<PullReviewReplyRecord> {
    let record = PullReviewReplyRecord {
        pull: result_u64_from_row(row, 0)?,
        thread: result_u64_from_row(row, 1)?,
        number: result_u64_from_row(row, 2)?,
        author: author_from_row(row, 3)?,
        body: result_text(row, 6)?,
        version: result_u64_from_row(row, 7)?,
        created_at_ms: result_u64_from_row(row, 8)?,
        updated_at_ms: result_u64_from_row(row, 9)?,
    };
    validate_number(record.pull)?;
    validate_number(record.thread)?;
    validate_number(record.number)?;
    validate_author(&record.author)?;
    validate_body(&record.body, true)?;
    validate_number(record.version)?;
    if record.updated_at_ms < record.created_at_ms {
        return Err(crate::cells::Error::Invalid(
            "repository pull review reply timestamp is invalid",
        ));
    }
    Ok(record)
}

fn author_from_row(row: &[SqlValue], start: usize) -> crate::cells::Result<RepositoryAuthor> {
    Ok(RepositoryAuthor {
        issuer: result_text(row, start)?,
        subject: result_text(row, start + 1)?,
        name: result_text(row, start + 2)?,
    })
}

fn optional_author(
    row: &[SqlValue],
    start: usize,
) -> crate::cells::Result<Option<RepositoryAuthor>> {
    let issuer = result_optional_text(row, start)?;
    let subject = result_optional_text(row, start + 1)?;
    let name = result_optional_text(row, start + 2)?;
    match (issuer, subject, name) {
        (None, None, None) => Ok(None),
        (Some(issuer), Some(subject), Some(name)) => Ok(Some(RepositoryAuthor {
            issuer,
            subject,
            name,
        })),
        _ => Err(crate::cells::Error::Invalid(
            "repository pull review thread resolver is invalid",
        )),
    }
}

fn result_optional_u64(row: &[SqlValue], column: usize) -> crate::cells::Result<Option<u64>> {
    match row.get(column) {
        Some(SqlValue::Null) => Ok(None),
        Some(SqlValue::Integer(value)) => u64::try_from(*value)
            .map(Some)
            .map_err(|_| crate::cells::Error::Invalid("repository result is negative")),
        _ => Err(crate::cells::Error::Invalid(
            "repository query returned invalid optional integer",
        )),
    }
}

pub(super) fn list_threads(
    context: &RepositoryTransaction<'_>,
    input: &PullReviewThreadListInput,
) -> crate::cells::Result<Vec<Vec<SqlValue>>> {
    let upper = input.before.map_or(MAX_NUMBER, |value| value - 1);
    let mut sql = String::from(
        "SELECT pull_number, number, author_issuer, author_subject, author_name, body, suggested_text, base_oid, head_oid, path, old_blob_oid, new_blob_oid, side, start_line, end_line, resolved, resolved_by_issuer, resolved_by_subject, resolved_by_name, resolved_at_ms, version, created_at_ms, updated_at_ms FROM repository_pull_review_threads WHERE pull_number = ? AND number <= ?",
    );
    let mut parameters = vec![integer(input.pull)?, integer(upper)?];
    if let Some(path) = &input.path {
        sql.push_str(" AND path = ?");
        parameters.push(SqlValue::Blob(path.clone()));
    }
    if let Some(resolved) = input.resolved {
        sql.push_str(" AND resolved = ?");
        parameters.push(SqlValue::Integer(i64::from(resolved)));
    }
    match input.outdated {
        Some(false) => {
            sql.push_str(" AND base_oid = ? AND head_oid = ?");
            parameters.push(SqlValue::Text(input.comparison_base_oid.clone().ok_or(
                crate::cells::Error::Invalid("thread comparison is incomplete"),
            )?));
            parameters.push(SqlValue::Text(input.comparison_head_oid.clone().ok_or(
                crate::cells::Error::Invalid("thread comparison is incomplete"),
            )?));
        }
        Some(true) => {
            sql.push_str(" AND NOT (base_oid = ? AND head_oid = ?)");
            parameters.push(SqlValue::Text(input.comparison_base_oid.clone().ok_or(
                crate::cells::Error::Invalid("thread comparison is incomplete"),
            )?));
            parameters.push(SqlValue::Text(input.comparison_head_oid.clone().ok_or(
                crate::cells::Error::Invalid("thread comparison is incomplete"),
            )?));
        }
        None => {}
    }
    sql.push_str(" ORDER BY number DESC LIMIT ?");
    parameters.push(SqlValue::Integer(i64::from(input.limit)));
    Ok(context.sql(&SqlBatch {
        statements: vec![statement(&sql, parameters)],
    })?[0]
        .rows
        .clone())
}

pub(super) fn list_replies(
    context: &RepositoryTransaction<'_>,
    input: &PullReviewReplyListInput,
) -> crate::cells::Result<Vec<Vec<SqlValue>>> {
    let upper = input.before.map_or(MAX_NUMBER, |value| value - 1);
    Ok(context.sql(&SqlBatch {
        statements: vec![statement(
            "SELECT pull_number, thread_number, number, author_issuer, author_subject, author_name, body, version, created_at_ms, updated_at_ms FROM repository_pull_review_replies WHERE pull_number = ? AND thread_number = ? AND number <= ? ORDER BY number DESC LIMIT ?",
            vec![
                integer(input.pull)?,
                integer(input.thread)?,
                integer(upper)?,
                SqlValue::Integer(i64::from(input.limit)),
            ],
        )],
    })?[0]
        .rows
        .clone())
}

pub(super) fn pull_exists_query(
    context: &RepositoryTransaction<'_>,
    pull: u64,
) -> crate::cells::Result<bool> {
    let result = context.sql(&SqlBatch {
        statements: vec![statement(
            "SELECT 1 FROM repository_pulls WHERE number = ?",
            vec![integer(pull)?],
        )],
    })?;
    Ok(!result[0].rows.is_empty())
}

pub(super) fn thread_exists_query(
    context: &RepositoryTransaction<'_>,
    pull: u64,
    thread: u64,
) -> crate::cells::Result<bool> {
    let result = context.sql(&SqlBatch {
        statements: vec![statement(
            "SELECT 1 FROM repository_pull_review_threads WHERE pull_number = ? AND number = ?",
            vec![integer(pull)?, integer(thread)?],
        )],
    })?;
    Ok(!result[0].rows.is_empty())
}

pub(super) fn bounded_page<T: Serialize>(
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
            .map_err(|_| crate::cells::Error::Invalid("repository pull review page is invalid"))?;
        let separator = usize::from(!kept.is_empty());
        if used
            .checked_add(separator + encoded.len())
            .is_none_or(|size| size > MAX_PAGE_WIRE_BYTES)
        {
            break;
        }
        used += separator + encoded.len();
        kept.push(item);
    }
    if kept.is_empty() && total != 0 {
        return Err(crate::cells::Error::Invalid(
            "repository pull review page exceeds the limit",
        ));
    }
    let next = (kept.len() < total || total == usize::from(requested))
        .then(|| kept.last().map(&number))
        .flatten();
    Ok((kept, next))
}
