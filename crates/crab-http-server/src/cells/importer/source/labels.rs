use rusqlite::params;

use super::super::legacy::DeletedLabel;
use super::{sqlite_error, to_i64};
use crate::cells::repository::{LabelRecord, RepositoryAuthor};

pub(super) type Catalog = (Vec<LabelRecord>, Vec<DeletedLabel>);

pub(super) fn insert_submission(
    transaction: &rusqlite::Transaction<'_>,
    request: [u8; 16],
    record: &LabelRecord,
    author: &RepositoryAuthor,
    digest: [u8; 32],
) -> crate::Result<()> {
    transaction
        .execute(
            "INSERT INTO repository_label_submissions(request_id, payload_digest, label_number, author_name, created_at_ms, initial_name, initial_color, initial_description) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                request.as_slice(),
                digest.as_slice(),
                to_i64(record.number)?,
                author.name,
                to_i64(record.created_at_ms)?,
                record.name,
                record.color,
                record.description,
            ],
        )
        .map_err(sqlite_error)?;
    Ok(())
}

pub(super) fn materialize(
    transaction: &rusqlite::Transaction<'_>,
    labels: Vec<LabelRecord>,
    deleted: Vec<DeletedLabel>,
) -> crate::Result<()> {
    for record in labels {
        let created_at: i64 = transaction
            .query_row(
                "SELECT created_at_ms FROM repository_label_submissions WHERE label_number = ?1",
                [to_i64(record.number)?],
                |row| row.get(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    crate::Error::Config("legacy label catalog has no matching reservation")
                }
                error => sqlite_error(error),
            })?;
        if u64::try_from(created_at) != Ok(record.created_at_ms) {
            return Err(crate::Error::Config(
                "legacy label catalog differs from its reservation",
            ));
        }
        transaction
            .execute(
                "INSERT INTO repository_labels(number, name_key, name, color, description, version, created_at_ms, updated_at_ms, deleted_version) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL)",
                params![
                    to_i64(record.number)?,
                    record.name.to_lowercase(),
                    record.name,
                    record.color,
                    record.description,
                    to_i64(record.version)?,
                    to_i64(record.created_at_ms)?,
                    to_i64(record.updated_at_ms)?,
                ],
            )
            .map_err(sqlite_error)?;
    }
    for record in deleted {
        let initial = transaction
            .query_row(
                "SELECT initial_name, initial_color, initial_description, created_at_ms FROM repository_label_submissions WHERE label_number = ?1",
                [to_i64(record.number)?],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => crate::Error::Config(
                    "legacy label tombstone has no matching reservation",
                ),
                error => sqlite_error(error),
            })?;
        transaction
            .execute(
                "INSERT INTO repository_labels(number, name_key, name, color, description, version, created_at_ms, updated_at_ms, deleted_version) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?6)",
                params![
                    to_i64(record.number)?,
                    initial.0.to_lowercase(),
                    initial.0,
                    initial.1,
                    initial.2,
                    to_i64(record.version)?,
                    initial.3,
                ],
            )
            .map_err(sqlite_error)?;
    }
    Ok(())
}

pub(super) fn validate_v1(record: &LabelRecord) -> crate::Result<()> {
    if record.number == 0
        || record.number > 500
        || record.name.is_empty()
        || record.name.trim() != record.name
        || record.name.chars().count() > 50
        || record.name.chars().any(char::is_control)
        || record.color.len() != 6
        || !record.color.bytes().all(|byte| byte.is_ascii_hexdigit())
        || record.color.to_ascii_lowercase() != record.color
        || record.description.as_ref().is_some_and(|value| {
            value.is_empty()
                || value.trim() != value
                || value.chars().count() > 100
                || value.chars().any(char::is_control)
        })
        || record.updated_at_ms < record.created_at_ms
    {
        return Err(crate::Error::Config(
            "legacy label row violates repository schema v1",
        ));
    }
    super::validate_number_v1(record.version)
}
