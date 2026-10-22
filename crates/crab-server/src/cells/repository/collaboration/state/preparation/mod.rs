use super::{Resolution, Subject, SubjectKind, Submission, load_comment};
use crate::cells::repository::collaboration::lifecycle::{
    LifecycleStatus, PrepareLifecycleOutcome,
};
use crate::cells::repository::{CLASS, MAX_NUMBER, validate_number};
use crate::cells::{
    Error, Result,
    application::{Mutation, MutationResult, Read, Transaction},
};
use serde::{Deserialize, Serialize};

const FRAME: usize = 8;
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum Preparation {
    Ready,
    Migrating {
        before: String,
        version: u64,
        copied: u64,
    },
}
pub(super) fn key(kind: SubjectKind) -> &'static str {
    match kind {
        SubjectKind::Issue => "repository/issues/state-submissions/v2/preparation",
        SubjectKind::Pull => "repository/pulls/state-submissions/v2/preparation",
    }
}
pub(super) fn prefix(kind: SubjectKind) -> &'static str {
    match kind {
        SubjectKind::Issue => "collaboration/v1/state-submissions/issue/",
        SubjectKind::Pull => "collaboration/v1/state-submissions/pull/",
    }
}
fn stored(context: &Transaction<'_>, kind: SubjectKind) -> Result<Option<Preparation>> {
    let value = context.document::<Preparation>(key(kind))?;
    if let Some(Preparation::Migrating {
        before,
        version,
        copied,
    }) = &value
    {
        if *version == 0 || *version > MAX_NUMBER || *copied > MAX_NUMBER {
            return Err(Error::Invalid(
                "state submission preparation progress is invalid",
            ));
        }
        subject(before, kind)?;
    }
    Ok(value)
}
fn status(context: &Transaction<'_>, kind: SubjectKind) -> Result<LifecycleStatus> {
    Ok(match stored(context, kind)? {
        None => LifecycleStatus::Outdated,
        Some(Preparation::Ready) => LifecycleStatus::Ready,
        Some(Preparation::Migrating {
            version, copied, ..
        }) => LifecycleStatus::Migrating { version, copied },
    })
}
pub(super) fn initialize(context: &Transaction<'_>, kind: SubjectKind) -> Result<()> {
    context.put_document(key(kind), &Preparation::Ready)
}
pub(super) fn ensure_ready(context: &Transaction<'_>, kind: SubjectKind) -> Result<()> {
    if status(context, kind)? != LifecycleStatus::Ready {
        return Err(Error::SchemaUnavailable);
    }
    Ok(())
}
pub(crate) struct GetPreparation;
impl Read for GetPreparation {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 301;
    type Input = SubjectKind;
    type Output = LifecycleStatus;
    fn execute(context: &mut Transaction<'_>, kind: SubjectKind) -> Result<LifecycleStatus> {
        status(context, kind)
    }
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    pub kind: SubjectKind,
    pub version: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoricalSubmission {
    digest: String,
    comment: Option<u64>,
}
pub(crate) struct Prepare;
impl Mutation for Prepare {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 301;
    type Input = Input;
    type Output = PrepareLifecycleOutcome;
    fn execute(
        context: &mut Transaction<'_>,
        input: Input,
    ) -> Result<MutationResult<Self::Output>> {
        let old = stored(context, input.kind)?;
        let (before, version, copied) = match &old {
            Some(Preparation::Ready) => {
                return Ok(MutationResult::Success(PrepareLifecycleOutcome::Applied(
                    LifecycleStatus::Ready,
                )));
            }
            Some(Preparation::Migrating {
                before,
                version,
                copied,
            }) => (Some(before.as_str()), *version, *copied),
            None => (None, 0, 0),
        };
        if input.version != version {
            return Ok(MutationResult::Rejected(PrepareLifecycleOutcome::Conflict(
                status(context, input.kind)?,
            )));
        }
        let rows = context.documents::<HistoricalSubmission>(prefix(input.kind), before, FRAME)?;
        for (record, value) in &rows {
            if value.digest.len() != 64
                || !value
                    .digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(Error::Invalid(
                    "historical state submission digest is invalid",
                ));
            }
            if let Some(number) = value.comment {
                validate_number(number)?;
            }
            let subject = subject(record, input.kind)?;
            load_comment(context, subject, value.comment)?;
            // Only the bookkeeping representation changes. The original digest
            // and canonical comment reference remain the authority for replay.
            context.put_document(
                record,
                &Submission {
                    digest: value.digest.clone(),
                    resolution: Resolution::Applied {
                        comment: value.comment,
                    },
                },
            )?;
        }
        let new = if rows.len() < FRAME {
            Preparation::Ready
        } else {
            let before = rows
                .last()
                .ok_or(Error::Invalid("state submission frame is empty"))?
                .0
                .clone();
            let version =
                version
                    .checked_add(1)
                    .filter(|v| *v <= MAX_NUMBER)
                    .ok_or(Error::Invalid(
                        "state submission preparation version limit reached",
                    ))?;
            let copied = copied
                .checked_add(FRAME as u64)
                .filter(|v| *v <= MAX_NUMBER)
                .ok_or(Error::Invalid(
                    "state submission preparation count limit reached",
                ))?;
            Preparation::Migrating {
                before,
                version,
                copied,
            }
        };
        context.put_document(key(input.kind), &new)?;
        Ok(MutationResult::Success(PrepareLifecycleOutcome::Applied(
            status(context, input.kind)?,
        )))
    }
}
fn subject(key: &str, kind: SubjectKind) -> Result<Subject> {
    let (number, id) = key
        .strip_prefix(prefix(kind))
        .and_then(|tail| tail.split_once('/'))
        .ok_or(Error::Invalid("historical state submission key is invalid"))?;
    let parsed = u64::from_str_radix(number, 16)
        .map_err(|_| Error::Invalid("historical state submission number is invalid"))?;
    validate_number(parsed)?;
    let id = uuid::Uuid::parse_str(id)
        .map_err(|_| Error::Invalid("historical state submission ID is invalid"))?;
    let subject = Subject {
        kind,
        number: parsed,
    };
    let expected = format!("collaboration/v1/state-submissions/{}/{id}", subject.key());
    if expected != key {
        return Err(Error::Invalid(
            "historical state submission key is not canonical",
        ));
    }
    Ok(subject)
}
