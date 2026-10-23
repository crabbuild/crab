use super::{Kind, Saved};
use crate::cells::repository::{
    CLASS, MAX_NUMBER,
    collaboration::{
        Subject, SubjectKind, bulk,
        lifecycle::{LifecycleStatus, PrepareLifecycleOutcome},
        state,
    },
    issues::pins,
    validate_number,
};
use crate::cells::{
    Error, Result,
    application::{Mutation, MutationResult, Read, Transaction},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
pub(in crate::cells::repository) fn key(kind: Kind) -> &'static str {
    match kind {
        Kind::Issue => "repository/issues/state-submissions/v2/preparation",
        Kind::Pull => "repository/pulls/state-submissions/v2/preparation",
        Kind::Bulk => "repository/collaboration/bulk-submissions/v2/preparation",
        Kind::Pins => "repository/issues/pin-submissions/v2/preparation",
    }
}
fn stored(context: &Transaction<'_>, kind: Kind) -> Result<Option<Preparation>> {
    let value = context.document::<Preparation>(key(kind))?;
    if let Some(Preparation::Migrating {
        before,
        version,
        copied,
    }) = &value
    {
        if *version == 0 || *version > MAX_NUMBER || *copied > MAX_NUMBER {
            return Err(Error::Invalid("submission preparation progress is invalid"));
        }
        validate_key(before, kind)?;
    }
    Ok(value)
}
fn status(context: &Transaction<'_>, kind: Kind) -> Result<LifecycleStatus> {
    Ok(match stored(context, kind)? {
        None => LifecycleStatus::Outdated,
        Some(Preparation::Ready) => LifecycleStatus::Ready,
        Some(Preparation::Migrating {
            version, copied, ..
        }) => LifecycleStatus::Migrating { version, copied },
    })
}
pub(super) fn initialize(context: &Transaction<'_>, kind: Kind) -> Result<()> {
    context.put_document(key(kind), &Preparation::Ready)
}
pub(super) fn ensure_ready(context: &Transaction<'_>, kind: Kind) -> Result<()> {
    if status(context, kind)? != LifecycleStatus::Ready {
        return Err(Error::SchemaUnavailable);
    }
    Ok(())
}
pub(crate) struct GetPreparation;
impl Read for GetPreparation {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 301;
    type Input = Kind;
    type Output = LifecycleStatus;
    fn execute(context: &mut Transaction<'_>, kind: Kind) -> Result<LifecycleStatus> {
        status(context, kind)
    }
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    pub kind: Kind,
    pub version: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoricalState {
    digest: String,
    comment: Option<u64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoricalBulk {
    digest: String,
    count: u16,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoricalPins {
    digest: String,
    version: u64,
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
        let rows = context.documents::<Value>(input.kind.prefix(), before, FRAME)?;
        for (record, value) in &rows {
            let digest = value
                .get("digest")
                .and_then(Value::as_str)
                .ok_or(Error::Invalid("historical submission digest is missing"))?;
            if digest.len() != 64
                || !digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(Error::Invalid("historical submission digest is invalid"));
            }
            let saved = convert(context, input.kind, record, value.clone())?;
            // Only bookkeeping changes. Preserve the original identity/digest
            // and canonical result so replay cannot repeat a domain mutation.
            context.put_document(record, &saved)?;
        }
        let new = if rows.len() < FRAME {
            Preparation::Ready
        } else {
            let before = rows
                .last()
                .ok_or(Error::Invalid("submission preparation frame is empty"))?
                .0
                .clone();
            let version =
                version
                    .checked_add(1)
                    .filter(|v| *v <= MAX_NUMBER)
                    .ok_or(Error::Invalid(
                        "submission preparation version limit reached",
                    ))?;
            let copied = copied
                .checked_add(FRAME as u64)
                .filter(|v| *v <= MAX_NUMBER)
                .ok_or(Error::Invalid("submission preparation count limit reached"))?;
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
fn convert(context: &mut Transaction<'_>, kind: Kind, key: &str, value: Value) -> Result<Value> {
    Ok(match kind {
        Kind::Issue => convert_state(context, key, value, SubjectKind::Issue)?,
        Kind::Pull => convert_state(context, key, value, SubjectKind::Pull)?,
        Kind::Bulk => {
            validate_key(key, kind)?;
            let value: HistoricalBulk = serde_json::from_value(value)?;
            if !(1..=100).contains(&value.count) {
                return Err(Error::Invalid("historical bulk count is invalid"));
            }
            serde_json::to_value(Saved {
                digest: value.digest,
                resolution: bulk::Outcome::Applied(value.count),
            })?
        }
        Kind::Pins => {
            validate_key(key, kind)?;
            let value: HistoricalPins = serde_json::from_value(value)?;
            if value.version > MAX_NUMBER {
                return Err(Error::Invalid("historical pin version is invalid"));
            }
            serde_json::to_value(Saved {
                digest: value.digest,
                resolution: pins::Outcome::Applied(value.version),
            })?
        }
    })
}
fn convert_state(
    context: &mut Transaction<'_>,
    key: &str,
    value: Value,
    kind: SubjectKind,
) -> Result<Value> {
    let value: HistoricalState = serde_json::from_value(value)?;
    if let Some(number) = value.comment {
        validate_number(number)?;
    }
    let subject = subject(key, kind)?;
    state::load_comment(context, subject, value.comment)?;
    Ok(serde_json::to_value(Saved {
        digest: value.digest,
        resolution: state::Resolution::Applied {
            comment: value.comment,
        },
    })?)
}
fn validate_key(key: &str, kind: Kind) -> Result<()> {
    match kind {
        Kind::Issue => {
            subject(key, SubjectKind::Issue)?;
        }
        Kind::Pull => {
            subject(key, SubjectKind::Pull)?;
        }
        Kind::Bulk | Kind::Pins => {
            submission_id(
                key.strip_prefix(kind.prefix())
                    .ok_or(Error::Invalid("historical submission key is invalid"))?,
            )?;
        }
    }
    Ok(())
}
fn submission_id(value: &str) -> Result<uuid::Uuid> {
    let id = uuid::Uuid::parse_str(value)
        .map_err(|_| Error::Invalid("historical submission ID is invalid"))?;
    if id.to_string() != value {
        return Err(Error::Invalid("historical submission ID is not canonical"));
    }
    Ok(id)
}
fn subject(key: &str, kind: SubjectKind) -> Result<Subject> {
    let prefix = Kind::from(kind).prefix();
    let (number, id) = key
        .strip_prefix(prefix)
        .and_then(|tail| tail.split_once('/'))
        .ok_or(Error::Invalid("historical state submission key is invalid"))?;
    let number = u64::from_str_radix(number, 16)
        .map_err(|_| Error::Invalid("historical state submission number is invalid"))?;
    validate_number(number)?;
    let id = submission_id(id)?;
    if format!("{prefix}{number:016x}/{id}") != key {
        return Err(Error::Invalid(
            "historical state submission key is not canonical",
        ));
    }
    Ok(Subject { kind, number })
}

#[cfg(test)]
pub(in crate::cells::repository) mod tests;
