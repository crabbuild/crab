use super::*;
use crate::cells::repository::result_u64_from_row;

pub(super) fn insert(context: &Transaction<'_>, key: &Key, body: &str) -> Result<()> {
    let parameters = [
        SqlValue::Integer(key.source.code()),
        SqlValue::Integer(i64::from(!matches!(key.source, Source::IssueComment))),
        integer(key.discussion)?,
        integer(key.thread)?,
        integer(key.number)?,
        SqlValue::Text(body.to_lowercase()),
    ];
    context.query("INSERT INTO discussion_index_comment_text(source, kind, discussion, thread, number, body) VALUES (?, ?, ?, ?, ?, ?)", &parameters)?;
    Ok(())
}

pub(super) fn edit(context: &Transaction<'_>, key: &Key, body: &str) -> Result<()> {
    let parameters = [
        SqlValue::Text(body.to_lowercase()),
        SqlValue::Integer(key.source.code()),
        integer(key.discussion)?,
        integer(key.thread)?,
        integer(key.number)?,
    ];
    let result = context.query("UPDATE discussion_index_comment_text SET body = ? WHERE source = ? AND discussion = ? AND thread = ? AND number = ?", &parameters)?;
    if result.rows_affected != 1 {
        return Err(Error::Invalid("Contribution text projection is missing"));
    }
    Ok(())
}

pub(in crate::cells::repository::search) fn flush(context: &Transaction<'_>) -> Result<()> {
    let rows = context.query(
        "SELECT id FROM discussion_index_comment_text_dirty ORDER BY id LIMIT 129",
        &[],
    )?;
    if rows.rows.len() > super::super::index::MAX_DIRTY {
        return Err(Error::Invalid(
            "Contribution mutation exceeds its index work limit",
        ));
    }
    for row in &rows.rows {
        let parameters = [integer(result_u64_from_row(row, 0)?)?];
        // Delete before inserting the current body. A deleted highest rowid can
        // be reused in this transaction; its old tokens must not survive reuse.
        context.query(
            "DELETE FROM discussion_comment_fts WHERE rowid = ?",
            &parameters,
        )?;
        context.query("INSERT INTO discussion_comment_fts(rowid, body) SELECT id, body FROM discussion_index_comment_text WHERE id = ?", &parameters)?;
        context.query(
            "DELETE FROM discussion_index_comment_text_dirty WHERE id = ?",
            &parameters,
        )?;
    }
    Ok(())
}
