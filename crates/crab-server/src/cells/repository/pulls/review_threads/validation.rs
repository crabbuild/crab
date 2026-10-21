use super::*;

pub(super) fn same_author(left: &RepositoryAuthor, right: &RepositoryAuthor) -> bool {
    left.issuer == right.issuer && left.subject == right.subject
}

pub(super) fn validate_create_thread(
    input: &CreatePullReviewThreadInput,
) -> crate::cells::Result<()> {
    validate_number(input.pull)?;
    validate_author(&input.author)?;
    validate_body(&input.body, true)?;
    validate_oid(&input.base_oid)?;
    validate_oid(&input.head_oid)?;
    validate_path(&input.path)?;
    validate_oid_option(&input.old_blob_oid)?;
    validate_oid_option(&input.new_blob_oid)?;
    validate_lines(input.start_line, input.end_line)?;
    validate_suggested_text(input.suggested_text.as_deref())?;
    match input.side {
        PullReviewThreadSide::Old => {
            if input.old_blob_oid.is_none() || input.suggested_text.is_some() {
                return Err(crate::cells::Error::Invalid(
                    "old pull review thread anchor is invalid",
                ));
            }
        }
        PullReviewThreadSide::New => {
            if input.new_blob_oid.is_none() {
                return Err(crate::cells::Error::Invalid(
                    "new pull review thread anchor is invalid",
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn validate_thread_key(key: &PullReviewThreadKey) -> crate::cells::Result<()> {
    validate_number(key.pull)?;
    validate_number(key.number)
}

pub(super) fn validate_reply_key(key: &PullReviewReplyKey) -> crate::cells::Result<()> {
    validate_number(key.pull)?;
    validate_number(key.thread)?;
    validate_number(key.number)
}

pub(super) fn validate_thread_list(input: &PullReviewThreadListInput) -> crate::cells::Result<()> {
    validate_number(input.pull)?;
    validate_list(input.before, input.limit)?;
    if input
        .path
        .as_ref()
        .is_some_and(|path| validate_path(path).is_err())
    {
        return Err(crate::cells::Error::Invalid(
            "repository pull review thread path is invalid",
        ));
    }
    if input.comparison_base_oid.is_some() != input.comparison_head_oid.is_some()
        || (input.outdated.is_some()
            && (input.comparison_base_oid.is_none() || input.comparison_head_oid.is_none()))
    {
        return Err(crate::cells::Error::Invalid(
            "repository pull review thread comparison is incomplete",
        ));
    }
    if let Some(base) = &input.comparison_base_oid {
        validate_oid(base)?;
    }
    if let Some(head) = &input.comparison_head_oid {
        validate_oid(head)?;
    }
    Ok(())
}

pub(super) fn validate_reply_list(input: &PullReviewReplyListInput) -> crate::cells::Result<()> {
    validate_number(input.pull)?;
    validate_number(input.thread)?;
    validate_list(input.before, input.limit)
}

fn validate_path(path: &[u8]) -> crate::cells::Result<()> {
    if path.is_empty() || path.len() > 1024 || path.contains(&0) {
        return Err(crate::cells::Error::Invalid(
            "repository pull review thread path is invalid",
        ));
    }
    Ok(())
}

fn validate_oid(value: &str) -> crate::cells::Result<()> {
    if value.len() != 40
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || value.bytes().all(|byte| byte == b'0')
    {
        return Err(crate::cells::Error::Invalid(
            "repository pull review thread object ID is invalid",
        ));
    }
    Ok(())
}

fn validate_oid_option(value: &Option<String>) -> crate::cells::Result<()> {
    if let Some(value) = value {
        validate_oid(value)?;
    }
    Ok(())
}

fn validate_lines(start_line: u64, end_line: u64) -> crate::cells::Result<()> {
    validate_number(start_line)?;
    validate_number(end_line)?;
    if end_line < start_line || end_line - start_line >= 200 {
        return Err(crate::cells::Error::Invalid(
            "repository pull review thread line range is invalid",
        ));
    }
    Ok(())
}

pub(super) fn validate_suggested_text(value: Option<&str>) -> crate::cells::Result<()> {
    if value.is_some_and(|value| value.len() > MAX_SUGGESTED_TEXT_BYTES || value.contains('\0')) {
        return Err(crate::cells::Error::Invalid(
            "repository pull review suggestion is invalid",
        ));
    }
    Ok(())
}

pub(super) fn validate_thread_record(record: &PullReviewThreadRecord) -> crate::cells::Result<()> {
    validate_number(record.pull)?;
    validate_number(record.number)?;
    validate_author(&record.author)?;
    validate_body(&record.body, true)?;
    validate_suggested_text(record.suggested_text.as_deref())?;
    validate_oid(&record.base_oid)?;
    validate_oid(&record.head_oid)?;
    validate_path(&record.path)?;
    validate_oid_option(&record.old_blob_oid)?;
    validate_oid_option(&record.new_blob_oid)?;
    validate_lines(record.start_line, record.end_line)?;
    if record.side == PullReviewThreadSide::Old
        && (record.old_blob_oid.is_none() || record.suggested_text.is_some())
    {
        return Err(crate::cells::Error::Invalid(
            "old pull review thread record is invalid",
        ));
    }
    if record.side == PullReviewThreadSide::New && record.new_blob_oid.is_none() {
        return Err(crate::cells::Error::Invalid(
            "new pull review thread record is invalid",
        ));
    }
    validate_number(record.version)?;
    if record.updated_at_ms < record.created_at_ms {
        return Err(crate::cells::Error::Invalid(
            "repository pull review thread timestamp is invalid",
        ));
    }
    if let Some(author) = &record.resolved_by {
        validate_author(author)?;
    }
    Ok(())
}

pub(super) fn next_version(value: u64) -> crate::cells::Result<u64> {
    value
        .checked_add(1)
        .filter(|value| *value < MAX_NUMBER)
        .ok_or(crate::cells::Error::Invalid(
            "repository pull review version is exhausted",
        ))
}
