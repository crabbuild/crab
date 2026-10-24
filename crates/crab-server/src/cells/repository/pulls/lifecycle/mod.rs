use super::*;
use crate::cells::repository::collaboration::{
    SubjectKind,
    activity::{Action, Event},
    lifecycle::{self as preparation, LifecycleStatus},
};

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullLifecycle {
    pub closed_at_ms: Option<u64>,
    pub merged_at_ms: Option<u64>,
}
impl PullLifecycle {
    fn validate(&self, state: PullState) -> crate::cells::Result<()> {
        let invalid = match state {
            PullState::Open => self.closed_at_ms.is_some() || self.merged_at_ms.is_some(),
            PullState::Closed => self.merged_at_ms.is_some(),
            PullState::Merged => self.closed_at_ms != self.merged_at_ms,
        };
        if invalid {
            return Err(crate::cells::Error::Invalid(
                "pull state and lifecycle metadata disagree",
            ));
        }
        Ok(())
    }
}
pub(in crate::cells::repository) fn initialize(
    context: &Transaction<'_>,
) -> crate::cells::Result<()> {
    preparation::initialize(context, SubjectKind::Pull)
}
pub(in crate::cells::repository) fn ensure_ready(
    context: &Transaction<'_>,
) -> crate::cells::Result<()> {
    preparation::ensure_ready(context, SubjectKind::Pull)
}
fn store(
    context: &Transaction<'_>,
    number: u64,
    value: &PullLifecycle,
) -> crate::cells::Result<()> {
    let closed = value
        .closed_at_ms
        .map(integer)
        .transpose()?
        .unwrap_or(SqlValue::Null);
    let merged = value
        .merged_at_ms
        .map(integer)
        .transpose()?
        .unwrap_or(SqlValue::Null);
    context.query(
        "INSERT INTO repository_pull_lifecycle(pull,closed_at_ms,merged_at_ms) VALUES (?,?,?) ON CONFLICT(pull) DO UPDATE SET closed_at_ms=excluded.closed_at_ms,merged_at_ms=excluded.merged_at_ms",
        &[integer(number)?, closed, merged],
    )?;
    Ok(())
}
pub(in crate::cells::repository) fn create(
    context: &Transaction<'_>,
    number: u64,
) -> crate::cells::Result<()> {
    ensure_ready(context)?;
    store(context, number, &PullLifecycle::default())
}
pub(in crate::cells::repository) fn get(
    context: &Transaction<'_>,
    number: u64,
    state: PullState,
) -> crate::cells::Result<PullLifecycle> {
    ensure_ready(context)?;
    let rows = context.query(
        "SELECT closed_at_ms,merged_at_ms FROM repository_pull_lifecycle WHERE pull=?",
        &[integer(number)?],
    )?;
    let row = rows.rows.first().ok_or(crate::cells::Error::Invalid(
        "pull lifecycle record is missing",
    ))?;
    let value = PullLifecycle {
        closed_at_ms: if row[0] == SqlValue::Null {
            None
        } else {
            Some(result_u64_from_row(row, 0)?)
        },
        merged_at_ms: if row[1] == SqlValue::Null {
            None
        } else {
            Some(result_u64_from_row(row, 1)?)
        },
    };
    value.validate(state)?;
    Ok(value)
}
pub(in crate::cells::repository) fn change(
    context: &Transaction<'_>,
    number: u64,
    was: PullState,
    state: PullState,
    at: u64,
) -> crate::cells::Result<()> {
    let old = get(context, number, was)?;
    let next = match state {
        PullState::Open => PullLifecycle::default(),
        PullState::Closed if was != state => PullLifecycle {
            closed_at_ms: Some(at),
            merged_at_ms: None,
        },
        PullState::Merged if was != state => PullLifecycle {
            closed_at_ms: Some(at),
            merged_at_ms: Some(at),
        },
        _ => old.clone(),
    };
    next.validate(state)?;
    if old != next {
        store(context, number, &next)?;
    }
    Ok(())
}

pub(crate) struct GetPullLifecycleStatus;
impl Read for GetPullLifecycleStatus {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 322;
    type Input = ();
    type Output = LifecycleStatus;
    fn execute(context: &mut Transaction<'_>, (): ()) -> crate::cells::Result<Self::Output> {
        preparation::state(context, SubjectKind::Pull)
    }
}
pub(crate) struct PreparePullLifecycle;
impl Mutation for PreparePullLifecycle {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 322;
    type Input = preparation::PrepareLifecycleInput;
    type Output = preparation::PrepareLifecycleOutcome;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        preparation::prepare(context, SubjectKind::Pull, input.version, recover)
    }
}
fn recover(
    context: &Transaction<'_>,
    number: u64,
    event: Option<&Event>,
) -> crate::cells::Result<()> {
    let rows = context.query(
        "SELECT state FROM repository_pulls WHERE number=?",
        &[integer(number)?],
    )?;
    let row = rows.rows.first().ok_or(crate::cells::Error::Invalid(
        "pull lifecycle source is missing",
    ))?;
    let state = PullState::from_code(result_u64_from_row(row, 0)?)?;
    let at = event
        .filter(|event| {
            matches!(
                (&event.action, state),
                (Action::Closed, PullState::Closed) | (Action::Merged, PullState::Merged)
            )
        })
        .map(|event| event.created_at);
    let value = PullLifecycle {
        closed_at_ms: at,
        merged_at_ms: if state == PullState::Merged { at } else { None },
    };
    value.validate(state)?;
    store(context, number, &value)
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullDetail {
    pub pull: PullRecord,
    pub lifecycle: PullLifecycle,
}
pub(crate) struct GetPullDetail;
impl Read for GetPullDetail {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 323;
    type Input = u64;
    type Output = Option<PullDetail>;
    fn execute(context: &mut Transaction<'_>, number: u64) -> crate::cells::Result<Self::Output> {
        ensure_ready(context)?;
        let Some(pull) = GetPull::execute(context, number)? else {
            return Ok(None);
        };
        let lifecycle = get(context, number, pull.state)?;
        Ok(Some(PullDetail { pull, lifecycle }))
    }
}
#[cfg(test)]
pub(in crate::cells::repository) mod tests;
