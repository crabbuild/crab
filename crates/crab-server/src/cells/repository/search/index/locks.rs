use super::*;
use crate::cells::repository::collaboration::locks::{self, Lock};

pub(super) fn install(context: &Transaction<'_>) -> crate::cells::Result<()> {
    // Subjects keep their row identity when the disposable search document is
    // replaced. A foreign key to that document would drop locks on ordinary edits.
    context.install_schema(
        "CREATE TABLE IF NOT EXISTS discussion_index_locks (
             document INTEGER PRIMARY KEY,
             expires_at_ms INTEGER CHECK (expires_at_ms > 0)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS discussion_index_lock_expiry
             ON discussion_index_locks(expires_at_ms) WHERE expires_at_ms IS NOT NULL;",
    )
}

pub(in crate::cells::repository) fn project(
    context: &Transaction<'_>,
    subject: Subject,
    lock: Lock,
) -> crate::cells::Result<()> {
    let document = integer(rowid(subject))?;
    if lock.locked {
        let expiry = lock
            .expires_at_ms()
            .map(integer)
            .transpose()?
            .unwrap_or(SqlValue::Null);
        context.query(
            "INSERT INTO discussion_index_locks(document, expires_at_ms) VALUES (?, ?) ON CONFLICT(document) DO UPDATE SET expires_at_ms = excluded.expires_at_ms",
            &[document, expiry],
        )?;
    } else {
        context.query(
            "DELETE FROM discussion_index_locks WHERE document = ?",
            &[document],
        )?;
    }
    Ok(())
}

pub(super) fn step(
    context: &Transaction<'_>,
    build: &mut SearchIndexProgress,
    kind: SubjectKind,
) -> crate::cells::Result<bool> {
    let page = locks::page(context, kind, build.after, BATCH)?;
    for (subject, lock) in &page {
        project(context, *subject, *lock)?;
        build.after = subject.number;
    }
    Ok(page.len() < BATCH)
}

pub(super) fn next_expiry(context: &Transaction<'_>) -> crate::cells::Result<Option<u64>> {
    let rows = context.query(
        "SELECT expires_at_ms FROM discussion_index_locks WHERE expires_at_ms > ? ORDER BY expires_at_ms LIMIT 1",
        &[SqlValue::Integer(context.now_ms())],
    )?;
    rows.rows
        .first()
        .map(|row| crate::cells::repository::result_u64_from_row(row, 0))
        .transpose()
}
