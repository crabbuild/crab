use serde::{Deserialize, Serialize};

use super::{Assignee, Transaction, encode, validate};
use crate::cells::repository::{
    advance_revision,
    collaboration::{Subject, SubjectKind},
    integer, result_blob, result_u64_from_row,
};
use crate::cells::{
    Error, Result,
    application::{Mutation, MutationResult, Read, SqlValue},
    records::RecordDecoder,
};

const FORMAT: &str = "repository/assignments/format";
const PROGRESS: &str = "repository/assignments/migration";
const BATCH: usize = 8;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Progress {
    phase: SubjectKind,
    after: u64,
    copied: u64,
    version: u64,
}
impl Default for Progress {
    fn default() -> Self {
        Self {
            phase: SubjectKind::Issue,
            after: 0,
            copied: 0,
            version: 0,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum AssignmentState {
    Outdated,
    Migrating { version: u64, copied: u64 },
    Ready,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyAssignment {
    pub target: Subject,
    pub version: u64,
    pub subjects: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssignmentBatch {
    pub state: AssignmentState,
    pub records: Vec<LegacyAssignment>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssignmentMapping {
    pub subject: String,
    pub assignee: Assignee,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordMapping {
    pub target: Subject,
    pub version: u64,
    pub mappings: Vec<AssignmentMapping>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MigrateAssignmentsInput {
    pub version: u64,
    pub records: Vec<RecordMapping>,
}

pub(in crate::cells::repository) fn initialize(context: &Transaction<'_>) -> Result<()> {
    context.put_document(FORMAT, &2u8)
}
pub(in crate::cells::repository) fn ensure_ready(context: &Transaction<'_>) -> Result<()> {
    match context.document::<u8>(FORMAT)? {
        Some(2) => Ok(()),
        None => Err(Error::SchemaUnavailable),
        Some(_) => Err(Error::Invalid("Unsupported repository assignment format")),
    }
}
fn progress(context: &Transaction<'_>) -> Result<Option<Progress>> {
    context.document(PROGRESS)
}
fn state(context: &Transaction<'_>) -> Result<AssignmentState> {
    match context.document::<u8>(FORMAT)? {
        Some(2) => Ok(AssignmentState::Ready),
        Some(_) => Err(Error::Invalid("Unsupported repository assignment format")),
        None => Ok(match progress(context)? {
            Some(p) => AssignmentState::Migrating {
                version: p.version,
                copied: p.copied,
            },
            None => AssignmentState::Outdated,
        }),
    }
}
fn table(kind: SubjectKind) -> &'static str {
    match kind {
        SubjectKind::Issue => "repository_issues",
        SubjectKind::Pull => "repository_pulls",
    }
}
fn batch(context: &Transaction<'_>, after: Option<Subject>) -> Result<AssignmentBatch> {
    let state = state(context)?;
    if state == AssignmentState::Ready {
        return Ok(AssignmentBatch {
            state,
            records: vec![],
        });
    }
    let stored = progress(context)?;
    let column = if stored.is_some() {
        "assignees"
    } else {
        "assignee_subjects"
    };
    let mut position = stored.unwrap_or_default();
    if let Some(after) = after {
        super::super::validate_number(after.number)?;
        // Converted rows use the canonical codec. Inventory cannot revisit that
        // prefix through the private retired-subject decoder.
        let order = |kind| u8::from(kind == SubjectKind::Pull);
        if (order(after.kind), after.number) < (order(position.phase), position.after) {
            return Err(Error::Invalid(
                "Inventory cursor precedes the remaining assignment source",
            ));
        }
        position.phase = after.kind;
        position.after = after.number;
    }
    loop {
        let rows = context.query(&format!("SELECT number, version, {column} FROM {} WHERE number > ? ORDER BY number LIMIT {BATCH}", table(position.phase)), &[integer(position.after)?])?;
        if !rows.rows.is_empty() || position.phase == SubjectKind::Pull {
            let records = rows
                .rows
                .iter()
                .map(|row| {
                    Ok(LegacyAssignment {
                        target: Subject {
                            kind: position.phase,
                            number: result_u64_from_row(row, 0)?,
                        },
                        version: result_u64_from_row(row, 1)?,
                        subjects: legacy_decode(result_blob(row, 2)?)?,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            return Ok(AssignmentBatch { state, records });
        }
        position.phase = SubjectKind::Pull;
        position.after = 0;
    }
}
pub(crate) struct GetAssignmentBatch;
impl Read for GetAssignmentBatch {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 280;
    type Input = Option<Subject>;
    type Output = AssignmentBatch;
    fn execute(context: &mut Transaction<'_>, after: Self::Input) -> Result<Self::Output> {
        batch(context, after)
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum MigrateAssignmentsOutcome {
    Progress { state: AssignmentState },
    Conflict { state: AssignmentState },
}

pub(crate) struct MigrateAssignments;
impl Mutation for MigrateAssignments {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 280;
    type Input = MigrateAssignmentsInput;
    type Output = MigrateAssignmentsOutcome;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> Result<MutationResult<Self::Output>> {
        let batch = batch(context, None)?;
        if batch.state == AssignmentState::Ready {
            return Ok(MutationResult::Success(
                MigrateAssignmentsOutcome::Progress {
                    state: AssignmentState::Ready,
                },
            ));
        }
        let mut progress = progress(context)?.unwrap_or_default();
        if input.version != progress.version {
            return Ok(MutationResult::Rejected(
                MigrateAssignmentsOutcome::Conflict { state: batch.state },
            ));
        }
        let nonempty: Vec<_> = batch
            .records
            .iter()
            .filter(|record| !record.subjects.is_empty())
            .collect();
        if input.records.len() != nonempty.len() {
            return Err(Error::Invalid(
                "Every assigned source record requires an explicit identity mapping",
            ));
        }
        let mut converted = Vec::with_capacity(batch.records.len());
        for record in &batch.records {
            let mut assignees = vec![];
            if !record.subjects.is_empty() {
                let Some(mapped) = input.records.iter().find(|mapped| {
                    mapped.target == record.target && mapped.version == record.version
                }) else {
                    return Ok(MutationResult::Rejected(
                        MigrateAssignmentsOutcome::Conflict { state: batch.state },
                    ));
                };
                if mapped.mappings.len() != record.subjects.len() {
                    return Err(Error::Invalid(
                        "Assignment mapping must cover every source subject exactly once",
                    ));
                }
                let mut subjects: Vec<_> = mapped
                    .mappings
                    .iter()
                    .map(|mapped| &mapped.subject)
                    .collect();
                subjects.sort_unstable();
                if subjects != record.subjects.iter().collect::<Vec<_>>() {
                    return Err(Error::Invalid(
                        "Assignment mapping does not match the source subjects",
                    ));
                }
                assignees = mapped
                    .mappings
                    .iter()
                    .map(|mapped| mapped.assignee.clone())
                    .collect();
                assignees.sort_unstable_by_key(|assignee| assignee.id);
                validate(&assignees)?;
            }
            converted.push((record, encode(&assignees)?));
        }
        if progress.version == 0 {
            // Renaming blocks old application writes. Fleet peers must still drain
            // together: native transactions cannot fence a different application.
            for table in ["repository_issues", "repository_pulls"] {
                context.install_schema(&format!(
                    "ALTER TABLE {table} RENAME COLUMN assignee_subjects TO assignees;"
                ))?;
            }
        }
        for (record, bytes) in converted {
            context.query(
                &format!(
                    "UPDATE {} SET assignees = ? WHERE number = ? AND version = ?",
                    table(record.target.kind)
                ),
                &[
                    SqlValue::Blob(bytes),
                    integer(record.target.number)?,
                    integer(record.version)?,
                ],
            )?;
            // Format 4 is rebuilt explicitly after conversion. Retaining one dirty
            // marker per migrated row would turn final preparation into a large flush.
            context.query(
                "DELETE FROM discussion_index_dirty WHERE kind = ? AND number = ?",
                &[
                    SqlValue::Integer(i64::from(record.target.kind == SubjectKind::Pull)),
                    integer(record.target.number)?,
                ],
            )?;
            progress.phase = record.target.kind;
            progress.after = record.target.number;
            progress.copied += 1;
        }
        if batch.records.is_empty() {
            initialize(context)?;
            context.delete_document(PROGRESS)?;
            advance_revision(context)?;
        } else {
            progress.version = progress
                .version
                .checked_add(1)
                .ok_or(Error::Invalid("Assignment migration version exhausted"))?;
            context.put_document(PROGRESS, &progress)?;
        }
        Ok(MutationResult::Success(
            MigrateAssignmentsOutcome::Progress {
                state: state(context)?,
            },
        ))
    }
}
fn legacy_decode(bytes: &[u8]) -> Result<Vec<String>> {
    let mut decoder = RecordDecoder::new(bytes, 16 * 1024)
        .map_err(|_| Error::Invalid("Invalid legacy assignment encoding"))?;
    let count = decoder
        .read_count()
        .map_err(|_| Error::Invalid("Invalid legacy assignment count"))?;
    if count > 10 {
        return Err(Error::Invalid("Invalid legacy assignment count"));
    }
    let mut subjects = Vec::with_capacity(count);
    for _ in 0..count {
        subjects.push(
            decoder
                .read_text()
                .map_err(|_| Error::Invalid("Invalid legacy assignment subject"))?
                .to_owned(),
        );
    }
    decoder
        .finish()
        .map_err(|_| Error::Invalid("Invalid legacy assignment trailing bytes"))?;
    if subjects
        .iter()
        .any(|s| s.is_empty() || s.chars().count() > 512 || s.chars().any(char::is_control))
        || subjects.windows(2).any(|p| p[0] >= p[1])
    {
        return Err(Error::Invalid("Invalid legacy assignment subjects"));
    }
    Ok(subjects)
}

#[cfg(test)]
pub(in crate::cells::repository) mod tests;
