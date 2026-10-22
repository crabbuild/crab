use super::*;
use crate::cells::repository::{result_text, result_u64_from_row};

const CURSOR: &str = "discussion-search/v1/comment-source-cursor";

pub(in crate::cells::repository::search) fn reset(context: &Transaction<'_>) -> Result<()> {
    context.delete_document(CURSOR)?;
    Ok(())
}

pub(in crate::cells::repository::search) fn clear(context: &Transaction<'_>) -> Result<bool> {
    let rows = context.query("SELECT source, discussion, thread, number FROM discussion_index_commenters ORDER BY source, discussion, thread, number LIMIT 8", &[])?;
    for row in &rows.rows {
        context.query("DELETE FROM discussion_index_commenters WHERE source = ? AND discussion = ? AND thread = ? AND number = ?", row)?;
    }
    text::flush(context)?;
    Ok(rows.rows.len() < 8)
}

pub(in crate::cells::repository::search) fn rebuild(context: &Transaction<'_>) -> Result<bool> {
    let mut cursor = context.document(CURSOR)?.unwrap_or(Key {
        source: Source::IssueComment,
        discussion: 0,
        thread: 0,
        number: 0,
    });
    let table = cursor.source.table();
    let parent = if matches!(cursor.source, Source::IssueComment) {
        "issue_number"
    } else {
        "pull_number"
    };
    let (thread, keys, values) = if matches!(cursor.source, Source::Reply) {
        (
            "thread_number",
            format!("{parent}, thread_number, number"),
            vec![
                integer(cursor.discussion)?,
                integer(cursor.thread)?,
                integer(cursor.number)?,
            ],
        )
    } else {
        (
            "0",
            format!("{parent}, number"),
            vec![integer(cursor.discussion)?, integer(cursor.number)?],
        )
    };
    let placeholders = std::iter::repeat_n("?", values.len())
        .collect::<Vec<_>>()
        .join(",");
    // Seek the canonical composite primary key. OFFSET or a partially covered
    // tuple would make later frames scan all preceding comment history again.
    let sql = format!(
        "SELECT {parent}, {thread}, number, author_issuer, author_subject, body FROM {table} WHERE ({keys}) > ({placeholders}) ORDER BY {keys} LIMIT 8"
    );
    let rows = context.query(&sql, &values)?;
    for row in &rows.rows {
        cursor.discussion = result_u64_from_row(row, 0)?;
        cursor.thread = result_u64_from_row(row, 1)?;
        cursor.number = result_u64_from_row(row, 2)?;
        insert(
            context,
            &cursor,
            &result_text(row, 3)?,
            &result_text(row, 4)?,
            &result_text(row, 5)?,
        )?;
    }
    text::flush(context)?;
    if rows.rows.len() < 8 {
        let Some(next) = cursor.source.next() else {
            reset(context)?;
            return Ok(true);
        };
        cursor = Key {
            source: next,
            discussion: 0,
            thread: 0,
            number: 0,
        };
    }
    context.put_document(CURSOR, &cursor)?;
    Ok(false)
}
