use super::*;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS repository_issue_type_index (
    issue INTEGER PRIMARY KEY REFERENCES repository_issues(number) ON DELETE CASCADE,
    organization BLOB NOT NULL CHECK(length(organization) = 16),
    ownership_epoch INTEGER NOT NULL CHECK(ownership_epoch >= 0),
    type_id BLOB NOT NULL CHECK(length(type_id) = 16)
) STRICT;
CREATE INDEX IF NOT EXISTS repository_issue_type_lookup
    ON repository_issue_type_index(organization, ownership_epoch, type_id, issue);";

pub(in crate::cells::repository) fn install(context: &Transaction<'_>) -> crate::cells::Result<()> {
    context.install_schema(SCHEMA)
}

pub(super) fn project(
    context: &Transaction<'_>,
    number: u64,
    selection: Option<Selection>,
) -> crate::cells::Result<()> {
    if let Some(selection) = selection {
        context.query(
            "INSERT INTO repository_issue_type_index(issue, organization, ownership_epoch, type_id)
             VALUES (?, ?, ?, ?) ON CONFLICT(issue) DO UPDATE SET
             organization = excluded.organization, ownership_epoch = excluded.ownership_epoch,
             type_id = excluded.type_id",
            &[
                integer(number)?,
                SqlValue::Blob(selection.organization.as_bytes().to_vec()),
                SqlValue::Integer(i64::from(selection.ownership_epoch)),
                SqlValue::Blob(selection.id.as_bytes().to_vec()),
            ],
        )?;
    } else {
        context.query(
            "DELETE FROM repository_issue_type_index WHERE issue = ?",
            &[integer(number)?],
        )?;
    }
    Ok(())
}

pub(in crate::cells::repository) fn backfill(
    context: &Transaction<'_>,
    after: &mut u64,
    limit: usize,
) -> crate::cells::Result<bool> {
    let cursor = (*after != 0).then(|| key(*after));
    // Only issues with a stored assignment can contribute an index row.
    // The prepared index remains unavailable until every page commits.
    let rows = context.documents_after::<Assignment>(PREFIX, cursor.as_deref(), limit)?;
    for (key, assignment) in &rows {
        let number = key
            .strip_prefix(PREFIX)
            .and_then(|key| u64::from_str_radix(key, 16).ok())
            .ok_or(crate::cells::Error::Invalid("Issue type key is invalid"))?;
        project(context, number, assignment.selection)?;
        *after = number;
    }
    Ok(rows.len() < limit)
}
