use super::*;
use uuid::Uuid;

pub(in crate::cells) fn initialize(
    context: &Transaction<'_>,
    repository: Uuid,
    create: bool,
) -> crate::cells::Result<()> {
    let tables = context.query(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'repository_identity'",
        &[],
    )?;
    if tables.rows.is_empty() {
        // Serving must not recreate a Cell whose catalog already says ready.
        // Only explicit initialization of an unpublished repository creates it.
        if !create {
            return Err(crate::cells::Error::Invalid(
                "repository Cell has not been initialized",
            ));
        }
        context.install_schema(include_str!("../schema.sql"))?;
        search::index::initialize(context, repository)?;
        context.query(
            "INSERT INTO repository_identity(singleton, repository_uuid) VALUES (1, ?)",
            &[SqlValue::Blob(repository.as_bytes().to_vec())],
        )?;
    }
    let identity = context.query(
        "SELECT repository_uuid FROM repository_identity WHERE singleton = 1",
        &[],
    )?;
    if identity.rows.as_slice() != [vec![SqlValue::Blob(repository.as_bytes().to_vec())]] {
        return Err(crate::cells::Error::Invalid(
            "repository Cell identity does not match its catalog",
        ));
    }
    Ok(())
}
