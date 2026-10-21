use super::*;

const PROGRESS: &str = "directory/migration/v2";
// 100 maximum-size v1 rows stay within the native SQL result byte limit.
// Larger batches could leave valid directories unable to make progress.
const BATCH: usize = 100;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Progress {
    after: Option<Uuid>,
    copied: u64,
}

/// Shared Directory preparation state reported by private management.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum DirectoryState {
    Outdated { format: u64, required_format: u64 },
    Migrating { copied: u64 },
    Ready { format: u64, revision: u64 },
}

pub(crate) struct GetDirectoryState;
impl Read for GetDirectoryState {
    const CLASS: &'static str = super::CLASS;
    const ID: u32 = 9;
    type Input = ();
    type Output = DirectoryState;
    fn execute(context: &mut Transaction<'_>, _: ()) -> Result<Self::Output> {
        state(context)
    }
}
fn state(context: &Transaction<'_>) -> Result<DirectoryState> {
    let current = format(context)?;
    if current != 2 {
        return Ok(match context.document::<Progress>(PROGRESS)? {
            Some(progress) => DirectoryState::Migrating {
                copied: progress.copied,
            },
            None => DirectoryState::Outdated {
                format: current,
                required_format: 2,
            },
        });
    }
    let revision = context.query(
        "SELECT app_revision FROM directory_identity WHERE singleton = 1",
        &[],
    )?;
    let Some([SqlValue::Integer(revision)]) = revision.rows.first().map(Vec::as_slice) else {
        return Err(Error::Invalid("directory revision is missing"));
    };
    Ok(DirectoryState::Ready {
        format: 2,
        revision: u64::try_from(*revision)
            .map_err(|_| Error::Invalid("directory revision is negative"))?,
    })
}

pub(crate) struct MigrateDirectory;
impl Mutation for MigrateDirectory {
    const CLASS: &'static str = super::CLASS;
    const ID: u32 = 10;
    type Input = ();
    type Output = DirectoryState;
    fn execute(context: &mut Transaction<'_>, _: ()) -> Result<MutationResult<Self::Output>> {
        match format(context)? {
            2 => return Ok(MutationResult::Success(state(context)?)),
            1 => (),
            _ => return Err(Error::Invalid("unsupported directory migration source")),
        }
        let mut progress = match context.document::<Progress>(PROGRESS)? {
            Some(progress) => progress,
            None => {
                // Renaming the source tables also stops old peer code from writing
                // during the bounded copy. Only this migration reads their shape.
                context.query("ALTER TABLE actors RENAME TO directory_v1_actors", &[])?;
                context.query(
                    "ALTER TABLE actor_submissions RENAME TO directory_v1_submissions",
                    &[],
                )?;
                context.install_schema(include_str!("../accounts/schema.sql"))?;
                context.install_schema(include_str!("../profiles/schema.sql"))?;
                Progress {
                    after: None,
                    copied: 0,
                }
            }
        };
        let columns = "id, issuer, subject, handle, name, version, created_at_ms, updated_at_ms";
        let rows = match progress.after {
            Some(after) => context.query(&format!("SELECT {columns} FROM directory_v1_actors WHERE id > ? ORDER BY id LIMIT {BATCH}"), &[SqlValue::Blob(after.as_bytes().to_vec())])?,
            None => context.query(&format!("SELECT {columns} FROM directory_v1_actors ORDER BY id LIMIT {BATCH}"), &[])?,
        };
        for row in &rows.rows {
            let [
                SqlValue::Blob(id),
                SqlValue::Text(issuer),
                SqlValue::Text(subject),
                SqlValue::Text(handle),
                SqlValue::Text(name),
                SqlValue::Integer(version),
                SqlValue::Integer(created),
                SqlValue::Integer(updated),
            ] = row.as_slice()
            else {
                return Err(Error::Invalid(
                    "directory migration source record is invalid",
                ));
            };
            let id = Uuid::from_slice(id).map_err(Error::sdk)?;
            if id.is_nil() {
                return Err(Error::Invalid("directory migration actor ID is nil"));
            }
            IdentityKey {
                issuer: issuer.clone(),
                subject: subject.clone(),
            }
            .validate()?;
            validate_profile(handle, name).map_err(Error::Invalid)?;
            context.query(
                "INSERT INTO accounts(id, issuer, subject, created_at_ms) VALUES (?, ?, ?, ?)",
                &[
                    SqlValue::Blob(id.as_bytes().to_vec()),
                    SqlValue::Text(issuer.clone()),
                    SqlValue::Text(subject.clone()),
                    SqlValue::Integer(*created),
                ],
            )?;
            context.query("INSERT INTO profiles(id, handle, name, version, created_at_ms, updated_at_ms) VALUES (?, ?, ?, ?, ?, ?)", &[
                SqlValue::Blob(id.as_bytes().to_vec()), SqlValue::Text(handle.clone()), SqlValue::Text(name.clone()),
                SqlValue::Integer(*version), SqlValue::Integer(*created), SqlValue::Integer(*updated),
            ])?;
            context.query("INSERT INTO profile_submissions(request_id, digest, actor_id) SELECT request_id, digest, actor_id FROM directory_v1_submissions WHERE actor_id = ?", &[SqlValue::Blob(id.as_bytes().to_vec())])?;
            progress.after = Some(id);
            progress.copied += 1;
        }
        if rows.rows.len() == BATCH {
            context.put_document(PROGRESS, &progress)?;
            advance(context)?;
            return Ok(MutationResult::Success(state(context)?));
        }
        // The legacy keys, copied profiles, receipts and format switch share one
        // native transaction. There is no serving reader for the retired tables.
        context.query("DROP TABLE directory_v1_submissions", &[])?;
        context.query("DROP TABLE directory_v1_actors", &[])?;
        context.query(
            "ALTER TABLE directory_identity RENAME TO directory_v1_identity",
            &[],
        )?;
        context.install_schema(include_str!("../identity.sql"))?;
        context.query("UPDATE directory_identity SET app_revision = (SELECT app_revision + 1 FROM directory_v1_identity WHERE singleton = 1) WHERE singleton = 1", &[])?;
        context.query("DROP TABLE directory_v1_identity", &[])?;
        context.delete_document(PROGRESS)?;
        Ok(MutationResult::Success(state(context)?))
    }
}

#[cfg(test)]
pub(super) mod tests;
