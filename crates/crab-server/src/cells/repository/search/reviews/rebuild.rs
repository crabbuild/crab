use crate::cells::{
    Error, Result,
    application::Transaction,
    repository::{
        RepositoryAuthor, integer, pulls::review_requests, result_text, result_u64_from_row,
    },
};
use serde::{Deserialize, Serialize};

const CURSOR: &str = "discussion-search/v1/review-source-cursor";
#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Cursor {
    Reviews { pull: u64, number: u64 },
    Requests { key: String },
}

pub(in crate::cells::repository::search) fn reset(context: &Transaction<'_>) -> Result<()> {
    context.delete_document(CURSOR)?;
    Ok(())
}
pub(in crate::cells::repository::search) fn clear(context: &Transaction<'_>) -> Result<bool> {
    let rows = context.query(
        "SELECT pull, identity FROM discussion_index_reviewers ORDER BY pull, identity LIMIT 8",
        &[],
    )?;
    for row in &rows.rows {
        context.query(
            "DELETE FROM discussion_index_reviewers WHERE pull = ? AND identity = ?",
            row,
        )?;
    }
    Ok(rows.rows.len() < 8)
}
pub(in crate::cells::repository::search) fn reviews(context: &Transaction<'_>) -> Result<bool> {
    let (pull, number) = match context.document(CURSOR)? {
        None => (0, 0),
        Some(Cursor::Reviews { pull, number }) => (pull, number),
        _ => return Err(Error::Invalid("Review source cursor is invalid")),
    };
    let rows = context.query("SELECT pull_number, number, author_issuer, author_subject, author_name FROM repository_pull_reviews WHERE (pull_number, number) > (?1, ?2) ORDER BY pull_number, number LIMIT 8", &[integer(pull)?, integer(number)?])?;
    for row in &rows.rows {
        let pull = result_u64_from_row(row, 0)?;
        let number = result_u64_from_row(row, 1)?;
        let author = RepositoryAuthor {
            issuer: result_text(row, 2)?,
            subject: result_text(row, 3)?,
            name: result_text(row, 4)?,
        };
        super::reviewed(context, pull, &author)?;
        context.put_document(CURSOR, &Cursor::Reviews { pull, number })?;
    }
    if rows.rows.len() < 8 {
        reset(context)?;
        return Ok(true);
    }
    Ok(false)
}
pub(in crate::cells::repository::search) fn requests(context: &Transaction<'_>) -> Result<bool> {
    let before = match context.document(CURSOR)? {
        None => None,
        Some(Cursor::Requests { key }) => Some(key),
        _ => return Err(Error::Invalid("Review request source cursor is invalid")),
    };
    let page = review_requests::index_page(context, before.as_deref())?;
    for (pull, request) in page.items {
        super::requested(
            context,
            pull,
            &request.reviewer,
            request.state == review_requests::State::Requested,
        )?;
    }
    match page.next {
        Some(key) => {
            context.put_document(CURSOR, &Cursor::Requests { key })?;
            Ok(false)
        }
        None => {
            reset(context)?;
            Ok(true)
        }
    }
}
