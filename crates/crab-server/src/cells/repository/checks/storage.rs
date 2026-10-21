use super::*;

pub(super) fn insert_check_version(
    context: &RepositoryTransaction<'_>,
    run: &CheckRunRecord,
    output: &CheckOutputRecord,
) -> crate::cells::Result<()> {
    context.sql(&SqlBatch {
        statements: vec![statement(
            "INSERT INTO repository_check_run_versions(run_number, version, create_request_id, author_issuer, author_subject, author_name, oid, name, status, conclusion, details_url, output_title, output, started_at_ms, completed_at_ms, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            vec![
                integer(run.number)?,
                integer(run.version)?,
                SqlValue::Blob(run.create_submission_id.to_vec()),
                SqlValue::Text(run.author.issuer.clone()),
                SqlValue::Text(run.author.subject.clone()),
                SqlValue::Text(run.author.name.clone()),
                SqlValue::Text(run.oid.clone()),
                SqlValue::Text(run.name.clone()),
                SqlValue::Integer(i64::from(run.status)),
                optional_integer(run.conclusion.map(u64::from))?,
                optional_text(run.details_url.clone()),
                SqlValue::Text(run.output_title.clone()),
                SqlValue::Blob(encode_check_output(output)?),
                optional_integer(run.started_at_ms)?,
                optional_integer(run.completed_at_ms)?,
                integer(run.created_at_ms)?,
                integer(run.updated_at_ms)?,
            ],
        )],
    })?;
    Ok(())
}

pub(super) fn load_check_version(
    context: &RepositoryTransaction<'_>,
    number: u64,
    version: u64,
) -> crate::cells::Result<CheckRunDetail> {
    let result = context.sql(&SqlBatch {
        statements: vec![statement(
            "SELECT run_number, create_request_id, author_issuer, author_subject, author_name, oid, name, status, conclusion, details_url, output_title, version, started_at_ms, completed_at_ms, created_at_ms, updated_at_ms, output FROM repository_check_run_versions WHERE run_number = ? AND version = ?",
            vec![integer(number)?, integer(version)?],
        )],
    })?;
    result[0]
        .rows
        .first()
        .map(|row| check_detail_from_row(row))
        .transpose()?
        .ok_or(crate::cells::Error::Invalid(
            "repository check submission has no result row",
        ))
}

pub(super) fn load_current_check(
    context: &RepositoryTransaction<'_>,
    oid: &str,
    number: u64,
) -> crate::cells::Result<Option<CheckRunDetail>> {
    let result = context.sql(&SqlBatch {
        statements: vec![statement(
            "SELECT run_number, create_request_id, author_issuer, author_subject, author_name, oid, name, status, conclusion, details_url, output_title, version, started_at_ms, completed_at_ms, created_at_ms, updated_at_ms, output FROM repository_check_run_versions WHERE oid = ? AND run_number = ? ORDER BY version DESC LIMIT 1",
            vec![SqlValue::Text(oid.to_owned()), integer(number)?],
        )],
    })?;
    result[0]
        .rows
        .first()
        .map(|row| check_detail_from_row(row))
        .transpose()
}

fn check_detail_from_row(row: &[SqlValue]) -> crate::cells::Result<CheckRunDetail> {
    let run = check_run_from_row(row)?;
    let output = decode_check_output(result_blob(row, 16)?)?;
    if output.title != run.output_title {
        return Err(crate::cells::Error::Invalid(
            "repository check output title does not match its run",
        ));
    }
    Ok(CheckRunDetail { run, output })
}

pub(super) fn check_run_from_row(row: &[SqlValue]) -> crate::cells::Result<CheckRunRecord> {
    let submission = <[u8; 16]>::try_from(result_blob(row, 1)?)
        .map_err(|_| crate::cells::Error::Invalid("repository check submission ID is invalid"))?;
    let status = u8::try_from(result_u64_from_row(row, 7)?)
        .map_err(|_| crate::cells::Error::Invalid("repository check status is invalid"))?;
    let conclusion = result_optional_u64(row, 8)?
        .map(u8::try_from)
        .transpose()
        .map_err(|_| crate::cells::Error::Invalid("repository check conclusion is invalid"))?;
    let run = CheckRunRecord {
        number: result_u64_from_row(row, 0)?,
        create_submission_id: submission,
        author: RepositoryAuthor {
            issuer: result_text(row, 2)?,
            subject: result_text(row, 3)?,
            name: result_text(row, 4)?,
        },
        oid: result_text(row, 5)?,
        name: result_text(row, 6)?,
        status,
        conclusion,
        details_url: result_optional_text(row, 9)?,
        output_title: result_text(row, 10)?,
        version: result_u64_from_row(row, 11)?,
        started_at_ms: result_optional_u64(row, 12)?,
        completed_at_ms: result_optional_u64(row, 13)?,
        created_at_ms: result_u64_from_row(row, 14)?,
        updated_at_ms: result_u64_from_row(row, 15)?,
    };
    validate_check_run(&run)?;
    Ok(run)
}

fn validate_check_run(run: &CheckRunRecord) -> crate::cells::Result<()> {
    validate_number(run.number)?;
    validate_number(run.version)?;
    validate_author(&run.author)?;
    validate_oid(&run.oid)?;
    validate_plain(&run.name, 100, "repository check name is invalid")?;
    validate_plain(
        &run.output_title,
        200,
        "repository check output title is invalid",
    )?;
    validate_check_state(run.status, run.conclusion)?;
    validate_target_url(run.details_url.as_deref())?;
    if run.updated_at_ms < run.created_at_ms
        || run
            .started_at_ms
            .is_some_and(|value| value < run.created_at_ms)
        || run
            .completed_at_ms
            .is_some_and(|value| value < run.created_at_ms)
        || (run.status == 0 && run.started_at_ms.is_some())
        || (run.status != 0 && run.started_at_ms.is_none())
        || (run.status == 2) != run.completed_at_ms.is_some()
    {
        return Err(crate::cells::Error::Invalid(
            "repository check timestamps are invalid",
        ));
    }
    Ok(())
}

pub(super) fn validate_check_report(report: &CheckReportInput) -> crate::cells::Result<()> {
    validate_check_state(report.status, report.conclusion)?;
    validate_target_url(report.details_url.as_deref())?;
    validate_check_output(&report.output)
}

fn validate_check_output(output: &CheckOutputRecord) -> crate::cells::Result<()> {
    validate_plain(
        &output.title,
        200,
        "repository check output title is invalid",
    )?;
    validate_markdown(&output.summary, 32 * 1024, true)?;
    if let Some(text) = output.text.as_deref() {
        validate_markdown(text, 64 * 1024, false)?;
    }
    if output.steps.len() > MAX_CHECK_STEPS || output.annotations.len() > MAX_CHECK_ANNOTATIONS {
        return Err(crate::cells::Error::Invalid(
            "repository check output collection is too large",
        ));
    }
    for step in &output.steps {
        validate_plain(&step.name, 100, "repository check step name is invalid")?;
        validate_check_state(step.status, step.conclusion)?;
        if let Some(log) = step.log.as_deref() {
            validate_markdown(log, 64 * 1024, false)?;
        }
    }
    for annotation in &output.annotations {
        validate_plain(
            &annotation.path,
            1_024,
            "repository check annotation path is invalid",
        )?;
        if annotation.start_line == 0
            || annotation.end_line < annotation.start_line
            || annotation.end_line > MAX_NUMBER
            || annotation.level > 2
        {
            return Err(crate::cells::Error::Invalid(
                "repository check annotation is invalid",
            ));
        }
        if let Some(title) = annotation.title.as_deref() {
            validate_plain(title, 200, "repository check annotation title is invalid")?;
        }
        validate_markdown(&annotation.message, 4 * 1024, true)?;
    }
    encode_check_output(output).map(|_| ())
}

fn validate_check_state(status: u8, conclusion: Option<u8>) -> crate::cells::Result<()> {
    if status > 2
        || conclusion.is_some_and(|value| value > 6)
        || ((status == 2) != conclusion.is_some())
    {
        return Err(crate::cells::Error::Invalid(
            "repository check state is invalid",
        ));
    }
    Ok(())
}

pub(super) fn validate_oid(oid: &str) -> crate::cells::Result<()> {
    if oid.len() != 40
        || !oid
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || oid.bytes().all(|byte| byte == b'0')
    {
        return Err(crate::cells::Error::Invalid(
            "repository check commit is invalid",
        ));
    }
    Ok(())
}

pub(super) fn validate_plain(
    value: &str,
    maximum: usize,
    error: &'static str,
) -> crate::cells::Result<()> {
    if value.is_empty()
        || value.trim() != value
        || value.chars().count() > maximum
        || value.chars().any(char::is_control)
    {
        return Err(crate::cells::Error::Invalid(error));
    }
    Ok(())
}

fn validate_markdown(value: &str, maximum: usize, required: bool) -> crate::cells::Result<()> {
    if value.len() > maximum || value.contains('\0') || (required && value.trim().is_empty()) {
        return Err(crate::cells::Error::Invalid(
            "repository check markdown is invalid",
        ));
    }
    Ok(())
}

fn validate_target_url(value: Option<&str>) -> crate::cells::Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.len() > 2_048 || value.chars().any(char::is_control) {
        return Err(crate::cells::Error::Invalid(
            "repository check target is invalid",
        ));
    }
    let url = url::Url::parse(value)
        .map_err(|_| crate::cells::Error::Invalid("repository check target is invalid"))?;
    crate::config::validate_identity_url(&url, true)
        .map_err(|_| crate::cells::Error::Invalid("repository check target is invalid"))
}

fn encode_check_output(output: &CheckOutputRecord) -> crate::cells::Result<Vec<u8>> {
    let mut encoder = RecordEncoder::new(MAX_CHECK_OUTPUT_BYTES)
        .map_err(|_| crate::cells::Error::Invalid("repository check output is too large"))?;
    output
        .encode(&mut encoder)
        .map_err(|_| crate::cells::Error::Invalid("repository check output is too large"))?;
    Ok(encoder.finish())
}

fn decode_check_output(bytes: &[u8]) -> crate::cells::Result<CheckOutputRecord> {
    let mut decoder = RecordDecoder::new(bytes, MAX_CHECK_OUTPUT_BYTES)
        .map_err(|_| crate::cells::Error::Invalid("repository check output is invalid"))?;
    let output = CheckOutputRecord::decode(&mut decoder)
        .map_err(|_| crate::cells::Error::Invalid("repository check output is invalid"))?;
    decoder
        .finish()
        .map_err(|_| crate::cells::Error::Invalid("repository check output is invalid"))?;
    validate_check_output(&output)?;
    Ok(output)
}

pub(super) fn create_submission_digest(
    input: &CreateCheckRunInput,
) -> crate::cells::Result<blake3::Hash> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.repository.check-create-submission.v1\0");
    hash_text(&mut hasher, &input.author.issuer);
    hash_text(&mut hasher, &input.author.subject);
    hash_text(&mut hasher, &input.oid);
    hash_text(&mut hasher, &input.name);
    hash_report(&mut hasher, &input.report)?;
    Ok(hasher.finalize())
}

pub(super) fn update_submission_digest(
    input: &UpdateCheckRunInput,
) -> crate::cells::Result<blake3::Hash> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.repository.check-update-submission.v1\0");
    hash_text(&mut hasher, &input.actor.issuer);
    hash_text(&mut hasher, &input.actor.subject);
    hash_text(&mut hasher, &input.oid);
    hasher.update(&input.number.to_be_bytes());
    hasher.update(&input.version.to_be_bytes());
    hash_report(&mut hasher, &input.report)?;
    Ok(hasher.finalize())
}

fn hash_report(hasher: &mut blake3::Hasher, report: &CheckReportInput) -> crate::cells::Result<()> {
    hasher.update(&[report.status]);
    match report.conclusion {
        Some(value) => hasher.update(&[1, value]),
        None => hasher.update(&[0]),
    };
    hash_optional_text(hasher, report.details_url.as_deref());
    let output = encode_check_output(&report.output)?;
    hasher.update(&(output.len() as u64).to_be_bytes());
    hasher.update(&output);
    Ok(())
}

fn optional_integer(value: Option<u64>) -> crate::cells::Result<SqlValue> {
    value.map_or(Ok(SqlValue::Null), integer)
}

fn optional_text(value: Option<String>) -> SqlValue {
    value.map_or(SqlValue::Null, SqlValue::Text)
}

fn result_optional_u64(row: &[SqlValue], column: usize) -> crate::cells::Result<Option<u64>> {
    match row.get(column) {
        Some(SqlValue::Null) => Ok(None),
        Some(SqlValue::Integer(value)) => u64::try_from(*value)
            .map(Some)
            .map_err(|_| crate::cells::Error::Invalid("repository result is negative")),
        _ => Err(crate::cells::Error::Invalid(
            "repository query returned an invalid optional integer",
        )),
    }
}
