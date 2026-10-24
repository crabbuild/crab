use super::*;

fn optional_record<T: StoredRecord>(value: &Option<T>) -> crate::cells::Result<SqlValue> {
    match value {
        Some(value) => {
            let mut encoder = RecordEncoder::new(512 * 1024).map_err(|_| {
                crate::cells::Error::Invalid("repository release publication is too large")
            })?;
            value.encode(&mut encoder).map_err(|_| {
                crate::cells::Error::Invalid("repository release publication is too large")
            })?;
            Ok(SqlValue::Blob(encoder.finish()))
        }
        None => Ok(SqlValue::Null),
    }
}

fn decode_optional_record<T: StoredRecord>(value: &SqlValue) -> crate::cells::Result<Option<T>> {
    match value {
        SqlValue::Null => Ok(None),
        SqlValue::Blob(bytes) => {
            let mut decoder = RecordDecoder::new(bytes, 512 * 1024).map_err(|_| {
                crate::cells::Error::Invalid("repository release publication is invalid")
            })?;
            let value = T::decode(&mut decoder).map_err(|_| {
                crate::cells::Error::Invalid("repository release publication is invalid")
            })?;
            decoder.finish().map_err(|_| {
                crate::cells::Error::Invalid("repository release publication is invalid")
            })?;
            Ok(Some(value))
        }
        _ => Err(crate::cells::Error::Invalid(
            "repository release publication is invalid",
        )),
    }
}

pub(super) fn insert_release(release: &ReleaseRecord) -> crate::cells::Result<SqlStatement> {
    Ok(statement(
        "INSERT INTO repository_releases(number, create_request_id, author_issuer, author_subject, author_name, tag_name, tag_oid, target_oid, title, body, prerelease, draft, publication_pending, version, created_at_ms, published_at_ms, updated_at_ms, deleted) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        release_values(release)?,
    ))
}

fn release_values(release: &ReleaseRecord) -> crate::cells::Result<Vec<SqlValue>> {
    Ok(vec![
        integer(release.number)?,
        SqlValue::Blob(release.create_submission_id.to_vec()),
        SqlValue::Text(release.author.issuer.clone()),
        SqlValue::Text(release.author.subject.clone()),
        SqlValue::Text(release.author.name.clone()),
        SqlValue::Text(release.tag_name.clone()),
        release
            .tag_oid
            .clone()
            .map_or(SqlValue::Null, SqlValue::Text),
        SqlValue::Text(release.target_oid.clone()),
        SqlValue::Text(release.title.clone()),
        SqlValue::Text(release.body.clone()),
        SqlValue::Integer(i64::from(release.prerelease)),
        SqlValue::Integer(i64::from(release.draft)),
        optional_record(&release.publication_pending)?,
        integer(release.version)?,
        integer(release.created_at_ms)?,
        release
            .published_at_ms
            .map(integer)
            .transpose()?
            .unwrap_or(SqlValue::Null),
        integer(release.updated_at_ms)?,
        SqlValue::Integer(i64::from(release.deleted)),
    ])
}

pub(super) fn update_release_statement(
    release: &ReleaseRecord,
    expected_version: u64,
) -> crate::cells::Result<SqlStatement> {
    Ok(statement(
        "UPDATE repository_releases SET tag_oid = ?, title = ?, body = ?, prerelease = ?, draft = ?, publication_pending = ?, version = ?, published_at_ms = ?, updated_at_ms = ?, deleted = ? WHERE number = ? AND version = ?",
        vec![
            release
                .tag_oid
                .clone()
                .map_or(SqlValue::Null, SqlValue::Text),
            SqlValue::Text(release.title.clone()),
            SqlValue::Text(release.body.clone()),
            SqlValue::Integer(i64::from(release.prerelease)),
            SqlValue::Integer(i64::from(release.draft)),
            optional_record(&release.publication_pending)?,
            integer(release.version)?,
            release
                .published_at_ms
                .map(integer)
                .transpose()?
                .unwrap_or(SqlValue::Null),
            integer(release.updated_at_ms)?,
            SqlValue::Integer(i64::from(release.deleted)),
            integer(release.number)?,
            integer(expected_version)?,
        ],
    ))
}

pub(super) fn update_release_row(
    context: &Transaction<'_>,
    release: &ReleaseRecord,
    expected_version: u64,
) -> crate::cells::Result<()> {
    let result = context.sql(&SqlBatch {
        statements: vec![update_release_statement(release, expected_version)?],
    })?;
    if result[0].rows_affected != 1 {
        return Err(crate::cells::Error::Invalid(
            "repository release update lost its transaction",
        ));
    }
    Ok(())
}

fn release_select() -> &'static str {
    "SELECT number, create_request_id, author_issuer, author_subject, author_name, tag_name, tag_oid, target_oid, title, body, prerelease, draft, publication_pending, version, created_at_ms, published_at_ms, updated_at_ms, deleted FROM repository_releases WHERE number = ?"
}

pub(super) fn load_release(
    context: &Transaction<'_>,
    number: u64,
) -> crate::cells::Result<Option<ReleaseRecord>> {
    let result = context.sql(&SqlBatch { statements: vec![
        statement(release_select(), vec![integer(number)?]),
        statement("SELECT request_id, name, content_type, size, digest, uploader_issuer, uploader_subject, uploader_name, created_at_ms FROM repository_release_assets WHERE release_number = ? ORDER BY name", vec![integer(number)?]),
    ] })?;
    result[0]
        .rows
        .first()
        .map(|row| release_from_rows(row, &result[1].rows))
        .transpose()
}

pub(super) fn load_release_query(
    context: &Transaction<'_>,
    number: u64,
) -> crate::cells::Result<Option<ReleaseRecord>> {
    let result = context.sql(&SqlBatch { statements: vec![
        statement(release_select(), vec![integer(number)?]),
        statement("SELECT request_id, name, content_type, size, digest, uploader_issuer, uploader_subject, uploader_name, created_at_ms FROM repository_release_assets WHERE release_number = ? ORDER BY name", vec![integer(number)?]),
    ] })?;
    result[0]
        .rows
        .first()
        .map(|row| release_from_rows(row, &result[1].rows))
        .transpose()
}

fn release_from_rows(
    row: &[SqlValue],
    asset_rows: &[Vec<SqlValue>],
) -> crate::cells::Result<ReleaseRecord> {
    let submission = <[u8; 16]>::try_from(result_blob(row, 1)?)
        .map_err(|_| crate::cells::Error::Invalid("repository release submission ID is invalid"))?;
    let assets = asset_rows
        .iter()
        .map(|row| asset_from_row(row))
        .collect::<crate::cells::Result<Vec<_>>>()?;
    Ok(ReleaseRecord {
        number: result_u64_from_row(row, 0)?,
        create_submission_id: submission,
        author: author_from_row(row, 2)?,
        tag_name: result_text(row, 5)?,
        tag_oid: optional_text(row, 6)?,
        target_oid: result_text(row, 7)?,
        title: result_text(row, 8)?,
        body: result_text(row, 9)?,
        prerelease: result_bool(row, 10)?,
        draft: result_bool(row, 11)?,
        publication_pending: decode_optional_record(&row[12])?,
        version: result_u64_from_row(row, 13)?,
        created_at_ms: result_u64_from_row(row, 14)?,
        published_at_ms: optional_u64(row, 15)?,
        updated_at_ms: result_u64_from_row(row, 16)?,
        deleted: result_bool(row, 17)?,
        assets,
    })
}

fn asset_from_row(row: &[SqlValue]) -> crate::cells::Result<ReleaseAssetRecord> {
    Ok(ReleaseAssetRecord {
        request_id: <[u8; 16]>::try_from(result_blob(row, 0)?)
            .map_err(|_| crate::cells::Error::Invalid("repository release asset ID is invalid"))?,
        name: result_text(row, 1)?,
        content_type: result_text(row, 2)?,
        size: result_u64_from_row(row, 3)?,
        digest: result_text(row, 4)?,
        uploader: author_from_row(row, 5)?,
        created_at_ms: result_u64_from_row(row, 8)?,
    })
}

fn author_from_row(row: &[SqlValue], start: usize) -> crate::cells::Result<RepositoryAuthor> {
    Ok(RepositoryAuthor {
        issuer: result_text(row, start)?,
        subject: result_text(row, start + 1)?,
        name: result_text(row, start + 2)?,
    })
}

fn optional_text(row: &[SqlValue], index: usize) -> crate::cells::Result<Option<String>> {
    match row.get(index) {
        Some(SqlValue::Null) => Ok(None),
        Some(SqlValue::Text(value)) => Ok(Some(value.clone())),
        _ => Err(crate::cells::Error::Invalid(
            "repository release row is invalid",
        )),
    }
}

fn optional_u64(row: &[SqlValue], index: usize) -> crate::cells::Result<Option<u64>> {
    match row.get(index) {
        Some(SqlValue::Null) => Ok(None),
        Some(SqlValue::Integer(value)) => u64::try_from(*value)
            .map(Some)
            .map_err(|_| crate::cells::Error::Invalid("repository release row is invalid")),
        _ => Err(crate::cells::Error::Invalid(
            "repository release row is invalid",
        )),
    }
}

fn result_bool(row: &[SqlValue], index: usize) -> crate::cells::Result<bool> {
    match result_u64_from_row(row, index)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(crate::cells::Error::Invalid(
            "repository release row is invalid",
        )),
    }
}

pub(super) fn bounded_release_page(
    items: Vec<ReleaseRecord>,
) -> crate::cells::Result<(Vec<ReleaseRecord>, bool)> {
    let total = items.len();
    let mut used =
        4 + br#"{"items":["#.len() + br#"],"next":"#.len() + MAX_NUMBER.to_string().len() + 1;
    let mut kept = Vec::with_capacity(total);
    for item in items {
        let encoded = serde_json::to_vec(&item)
            .map_err(|_| crate::cells::Error::Invalid("repository release row is invalid"))?;
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
            "repository release exceeds the page limit",
        ));
    }
    let limited = kept.len() < total;
    Ok((kept, limited))
}
