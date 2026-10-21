use super::*;

pub(super) fn require_epoch(
    context: &RepositoryTransaction<'_>,
    epoch_id: u64,
    source: &SourceIdentity,
) -> crate::cells::Result<()> {
    if epoch_id == 0 {
        return Err(crate::cells::Error::Invalid("projection epoch is missing"));
    }
    let result = context.sql(&SqlBatch {
        statements: vec![statement(
            "SELECT source_token, state FROM git_projection_epochs WHERE epoch_id = ?",
            vec![integer(epoch_id)?],
        )],
    })?;
    let Some(row) = result[0].rows.first() else {
        return Err(crate::cells::Error::Invalid("projection epoch is missing"));
    };
    if result_text(row, 0)? != source.source_token || result_text(row, 1)? != "building" {
        return Err(crate::cells::Error::Invalid(
            "projection epoch identity or state differs",
        ));
    }
    Ok(())
}

pub(super) fn validate_source(source: &SourceIdentity) -> crate::cells::Result<()> {
    if source.source_token.is_empty()
        || source.source_token.len() > 128
        || source.manifest_etag.len() > 1024
        || source.journal_state_digest.len() > 128
        || source.pack_index_hash.len() > 128
        || source.git_validation_digest.len() > 128
        || source.head_ref.len() > 1024
    {
        return Err(crate::cells::Error::Invalid(
            "projection source identity is out of bounds",
        ));
    }
    Ok(())
}

pub(super) fn decode_payload<T: for<'de> Deserialize<'de>>(
    payload: &[u8],
) -> crate::cells::Result<T> {
    serde_json::from_slice(payload)
        .map_err(|_| crate::cells::Error::Invalid("projection batch payload is invalid"))
}

pub(super) fn statement(sql: &str, parameters: Vec<SqlValue>) -> SqlStatement {
    SqlStatement {
        sql: sql.to_owned(),
        parameters,
    }
}

pub(super) fn execute_statements(
    context: &RepositoryTransaction<'_>,
    mut statements: Vec<SqlStatement>,
) -> crate::cells::Result<()> {
    while !statements.is_empty() {
        let count = statements.len().min(MAX_SQL_STATEMENTS);
        let tail = statements.split_off(count);
        context.sql(&SqlBatch { statements })?;
        statements = tail;
    }
    Ok(())
}

pub(super) fn integer(value: u64) -> crate::cells::Result<SqlValue> {
    Ok(SqlValue::Integer(i64::try_from(value).map_err(|_| {
        crate::cells::Error::Invalid("projection integer exceeds SQLite range")
    })?))
}

pub(super) fn text(value: &str) -> SqlValue {
    SqlValue::Text(value.to_owned())
}

pub(super) fn optional_text_value(value: Option<&str>) -> SqlValue {
    value.map_or(SqlValue::Null, text)
}

pub(super) fn result_u64(
    sets: &[SqlResultSet],
    set: usize,
    column: usize,
) -> crate::cells::Result<u64> {
    match sets
        .get(set)
        .and_then(|set| set.rows.first())
        .and_then(|row| row.get(column))
    {
        Some(SqlValue::Integer(value)) => u64::try_from(*value)
            .map_err(|_| crate::cells::Error::Invalid("projection integer is negative")),
        _ => Err(crate::cells::Error::Invalid(
            "projection query returned invalid integer",
        )),
    }
}

pub(super) fn result_text(row: &[SqlValue], column: usize) -> crate::cells::Result<String> {
    match row.get(column) {
        Some(SqlValue::Text(value)) => Ok(value.clone()),
        _ => Err(crate::cells::Error::Invalid(
            "projection query returned invalid text",
        )),
    }
}

pub(super) fn result_i64(row: &[SqlValue], column: usize) -> crate::cells::Result<i64> {
    match row.get(column) {
        Some(SqlValue::Integer(value)) => Ok(*value),
        _ => Err(crate::cells::Error::Invalid(
            "projection query returned invalid integer",
        )),
    }
}

pub(super) fn blob(row: &[SqlValue], column: usize) -> crate::cells::Result<Vec<u8>> {
    match row.get(column) {
        Some(SqlValue::Blob(value)) => Ok(value.clone()),
        _ => Err(crate::cells::Error::Invalid(
            "projection query returned invalid blob",
        )),
    }
}

pub(super) fn optional_u64(row: &[SqlValue], column: usize) -> crate::cells::Result<Option<u64>> {
    match row.get(column) {
        Some(SqlValue::Null) => Ok(None),
        Some(SqlValue::Integer(value)) => u64::try_from(*value)
            .map(Some)
            .map_err(|_| crate::cells::Error::Invalid("projection integer is negative")),
        _ => Err(crate::cells::Error::Invalid(
            "projection query returned invalid optional integer",
        )),
    }
}

pub(super) fn optional_text(
    row: &[SqlValue],
    column: usize,
) -> crate::cells::Result<Option<String>> {
    match row.get(column) {
        Some(SqlValue::Null) => Ok(None),
        Some(SqlValue::Text(value)) => Ok(Some(value.clone())),
        _ => Err(crate::cells::Error::Invalid(
            "projection query returned invalid optional text",
        )),
    }
}
