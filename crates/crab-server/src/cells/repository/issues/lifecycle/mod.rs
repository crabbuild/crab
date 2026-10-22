use crate::cells::repository::{integer, result_u64_from_row};
use crate::cells::{
    Error, Result,
    application::{SqlValue, Transaction},
};
use serde::{Deserialize, Serialize};

mod preparation;
pub(crate) use preparation::{GetIssueLifecycleStatus, PrepareIssueLifecycle};
pub(in crate::cells::repository) use preparation::{ensure_ready, initialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Reason {
    Completed,
    NotPlanned,
    Reopened,
}
impl Reason {
    pub(in crate::cells::repository) const fn name(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::NotPlanned => "not_planned",
            Self::Reopened => "reopened",
        }
    }
    pub(in crate::cells::repository) fn parse(value: &str) -> Result<Self> {
        match value {
            "completed" => Ok(Self::Completed),
            "not_planned" => Ok(Self::NotPlanned),
            "reopened" => Ok(Self::Reopened),
            _ => Err(Error::Invalid("issue state reason is invalid")),
        }
    }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Lifecycle {
    pub state_reason: Option<Reason>,
    pub closed_at_ms: Option<u64>,
}
impl Lifecycle {
    fn validate(&self, closed: bool) -> Result<()> {
        if !closed
            && (self.closed_at_ms.is_some()
                || self
                    .state_reason
                    .is_some_and(|reason| reason != Reason::Reopened))
            || closed && self.state_reason == Some(Reason::Reopened)
        {
            return Err(Error::Invalid(
                "issue state and lifecycle metadata disagree",
            ));
        }
        Ok(())
    }
}
pub(in crate::cells::repository) fn create(context: &Transaction<'_>, issue: u64) -> Result<()> {
    ensure_ready(context)?;
    store(context, issue, &Lifecycle::default())
}
fn store(context: &Transaction<'_>, issue: u64, value: &Lifecycle) -> Result<()> {
    let reason = value
        .state_reason
        .map_or(SqlValue::Null, |value| SqlValue::Text(value.name().into()));
    let closed = value
        .closed_at_ms
        .map(integer)
        .transpose()?
        .unwrap_or(SqlValue::Null);
    context.query("INSERT INTO repository_issue_lifecycle(issue, state_reason, closed_at_ms) VALUES (?, ?, ?) ON CONFLICT(issue) DO UPDATE SET state_reason=excluded.state_reason, closed_at_ms=excluded.closed_at_ms",&[integer(issue)?,reason,closed])?;
    Ok(())
}
pub(in crate::cells::repository) fn get(
    context: &Transaction<'_>,
    issue: u64,
    closed: bool,
) -> Result<Lifecycle> {
    ensure_ready(context)?;
    let rows = context.query(
        "SELECT state_reason, closed_at_ms FROM repository_issue_lifecycle WHERE issue=?",
        &[integer(issue)?],
    )?;
    let Some([reason, closed_at]) = rows.rows.first().map(Vec::as_slice) else {
        return Err(Error::Invalid("issue lifecycle record is missing"));
    };
    let state_reason = match reason {
        SqlValue::Null => None,
        SqlValue::Text(value) => Some(Reason::parse(value)?),
        _ => return Err(Error::Invalid("issue state reason is invalid")),
    };
    let closed_at_ms = if matches!(closed_at, SqlValue::Null) {
        None
    } else {
        Some(result_u64_from_row(std::slice::from_ref(closed_at), 0)?)
    };
    let value = Lifecycle {
        state_reason,
        closed_at_ms,
    };
    value.validate(closed)?;
    Ok(value)
}
// Invalid caller transitions are rejected separately from corrupt stored data.
pub(in crate::cells::repository) fn change(
    context: &Transaction<'_>,
    issue: u64,
    was_closed: bool,
    closed: bool,
    requested: Option<Reason>,
    now: u64,
) -> Result<Option<(Lifecycle, Lifecycle)>> {
    let old = get(context, issue, was_closed)?;
    let mut next = old.clone();
    if closed {
        if requested == Some(Reason::Reopened) {
            return Ok(None);
        }
        if !was_closed {
            next.state_reason = Some(requested.unwrap_or(Reason::Completed));
            next.closed_at_ms = Some(now);
        } else if let Some(reason) = requested {
            next.state_reason = Some(reason);
        }
    } else {
        if requested.is_some_and(|reason| reason != Reason::Reopened) {
            return Ok(None);
        }
        if was_closed {
            next.state_reason = Some(Reason::Reopened);
            next.closed_at_ms = None;
        } else if requested.is_some() && next.state_reason != requested {
            return Ok(None);
        }
    }
    next.validate(closed)?;
    if next != old {
        store(context, issue, &next)?;
    }
    Ok(Some((old, next)))
}

#[cfg(test)]
pub(in crate::cells::repository) mod tests;
