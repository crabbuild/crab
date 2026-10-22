use super::{
    Subject, SubjectKind, Target,
    activity::{Action, Event, ListActivity, Query},
};
use crate::cells::repository::{advance_revision, integer, result_u64_from_row};
use crate::cells::{
    Error, Result,
    application::{MutationResult, Read, Transaction},
};
use serde::{Deserialize, Serialize};

fn key(kind: SubjectKind) -> &'static str {
    match kind {
        SubjectKind::Issue => "repository/issues/lifecycle/v1/preparation",
        SubjectKind::Pull => "repository/pulls/lifecycle/v1/preparation",
    }
}
fn schema(kind: SubjectKind) -> &'static str {
    match kind {
        SubjectKind::Issue => include_str!("../../issues/lifecycle/schema.sql"),
        SubjectKind::Pull => include_str!("../../pulls/lifecycle/schema.sql"),
    }
}
const FRAME: usize = 8;
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Progress {
    after: u64,
    current: Option<u64>,
    before: Option<u64>,
    version: u64,
    copied: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    format: u8,
    progress: Option<Progress>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
/// Readiness of canonical discussion closure metadata.
pub enum LifecycleStatus {
    Outdated,
    Migrating { version: u64, copied: u64 },
    Ready,
}
fn stored(context: &Transaction<'_>, kind: SubjectKind) -> Result<Option<Stored>> {
    let value = context.document::<Stored>(key(kind))?;
    if value.as_ref().is_some_and(|value| value.format != 1) {
        return Err(Error::Invalid("unsupported lifecycle format"));
    }
    if let Some(progress) = value.as_ref().and_then(|value| value.progress.as_ref()) {
        let maximum = crate::cells::repository::MAX_NUMBER;
        if progress.version == 0
            || progress.version > maximum
            || progress.after > maximum
            || progress.copied > maximum
            || progress.copied > progress.after
            || progress
                .current
                .is_some_and(|current| current <= progress.after || current > maximum)
            || progress
                .before
                .is_some_and(|before| before == 0 || before > maximum)
            || progress.current.is_none() && progress.before.is_some()
        {
            return Err(Error::Invalid("invalid lifecycle preparation progress"));
        }
    }
    Ok(value)
}
pub(in crate::cells::repository) fn state(
    context: &Transaction<'_>,
    kind: SubjectKind,
) -> Result<LifecycleStatus> {
    Ok(match stored(context, kind)? {
        None => LifecycleStatus::Outdated,
        Some(Stored { progress: None, .. }) => LifecycleStatus::Ready,
        Some(Stored {
            progress: Some(p), ..
        }) => LifecycleStatus::Migrating {
            version: p.version,
            copied: p.copied,
        },
    })
}
pub(in crate::cells::repository) fn initialize(
    context: &Transaction<'_>,
    kind: SubjectKind,
) -> Result<()> {
    context.install_schema(schema(kind))?;
    context.put_document(
        key(kind),
        &Stored {
            format: 1,
            progress: None,
        },
    )
}
pub(in crate::cells::repository) fn ensure_ready(
    context: &Transaction<'_>,
    kind: SubjectKind,
) -> Result<()> {
    match state(context, kind)? {
        LifecycleStatus::Ready => Ok(()),
        _ => Err(Error::SchemaUnavailable),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrepareLifecycleInput {
    pub version: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum PrepareLifecycleOutcome {
    Applied(LifecycleStatus),
    Conflict(LifecycleStatus),
}

pub(in crate::cells::repository) fn prepare(
    context: &mut Transaction<'_>,
    kind: SubjectKind,
    version: u64,
    recover: fn(&Transaction<'_>, u64, Option<&Event>) -> Result<()>,
) -> Result<MutationResult<PrepareLifecycleOutcome>> {
    let old = stored(context, kind)?;
    if old.as_ref().is_some_and(|value| value.progress.is_none()) {
        return Ok(MutationResult::Success(PrepareLifecycleOutcome::Applied(
            LifecycleStatus::Ready,
        )));
    }
    let mut p = old
        .as_ref()
        .and_then(|value| value.progress.clone())
        .unwrap_or_default();
    if p.version != version {
        return Ok(MutationResult::Rejected(PrepareLifecycleOutcome::Conflict(
            state(context, kind)?,
        )));
    }
    if old.is_none() {
        context.install_schema(schema(kind))?;
    }
    for _ in 0..FRAME {
        let number = if let Some(current) = p.current {
            Some(current)
        } else {
            let sql = match kind {
                SubjectKind::Issue => {
                    "SELECT number FROM repository_issues WHERE number>? ORDER BY number LIMIT 1"
                }
                SubjectKind::Pull => {
                    "SELECT number FROM repository_pulls WHERE number>? ORDER BY number LIMIT 1"
                }
            };
            context
                .query(sql, &[integer(p.after)?])?
                .rows
                .first()
                .map(|row| result_u64_from_row(row, 0))
                .transpose()?
        };
        let Some(number) = number else {
            context.put_document(
                key(kind),
                &Stored {
                    format: 1,
                    progress: None,
                },
            )?;
            advance_revision(context)?;
            return Ok(MutationResult::Success(PrepareLifecycleOutcome::Applied(
                LifecycleStatus::Ready,
            )));
        };
        p.current = Some(number);
        let target = match kind {
            SubjectKind::Issue => Target::Issue { number },
            SubjectKind::Pull => Target::Pull { number },
        };
        let page = ListActivity::execute(
            context,
            Query {
                subject: Some(Subject { kind, number }),
                before: p.before,
                limit: 1,
            },
        )?
        .ok_or(Error::Invalid("lifecycle activity source is missing"))?;
        let event = page.items.into_iter().find(|event| {
            event.target == target
                && matches!(
                    event.action,
                    Action::Closed | Action::Reopened | Action::IssueClosed { .. } | Action::Merged
                )
        });
        if event.is_none() && page.next.is_some() {
            p.before = page.next;
            continue;
        }
        // The canonical state and this latest transition share one native snapshot.
        // Missing historical events remain unknown; edit/reservation times are not proof.
        recover(context, number, event.as_ref())?;
        p.after = number;
        p.current = None;
        p.before = None;
        p.copied = p
            .copied
            .checked_add(1)
            .ok_or(Error::Invalid("lifecycle preparation count limit reached"))?;
    }
    p.version = p
        .version
        .checked_add(1)
        .filter(|v| *v <= crate::cells::repository::MAX_NUMBER)
        .ok_or(Error::Invalid(
            "lifecycle preparation version limit reached",
        ))?;
    context.put_document(
        key(kind),
        &Stored {
            format: 1,
            progress: Some(p),
        },
    )?;
    advance_revision(context)?;
    Ok(MutationResult::Success(PrepareLifecycleOutcome::Applied(
        state(context, kind)?,
    )))
}
