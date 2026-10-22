use super::*;
use crate::cells::application::{Mutation, MutationResult, Read};
use crate::cells::repository::collaboration::{
    SubjectKind,
    activity::{Action, Event},
    lifecycle::{self as preparation, LifecycleStatus},
};

pub(in crate::cells::repository) fn initialize(context: &Transaction<'_>) -> Result<()> {
    preparation::initialize(context, SubjectKind::Issue)
}
pub(in crate::cells::repository) fn ensure_ready(context: &Transaction<'_>) -> Result<()> {
    preparation::ensure_ready(context, SubjectKind::Issue)
}
pub(crate) struct GetIssueLifecycleStatus;
impl Read for GetIssueLifecycleStatus {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 290;
    type Input = ();
    type Output = LifecycleStatus;
    fn execute(context: &mut Transaction<'_>, (): ()) -> Result<Self::Output> {
        preparation::state(context, SubjectKind::Issue)
    }
}
pub(crate) struct PrepareIssueLifecycle;
impl Mutation for PrepareIssueLifecycle {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 290;
    type Input = preparation::PrepareLifecycleInput;
    type Output = preparation::PrepareLifecycleOutcome;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> Result<MutationResult<Self::Output>> {
        preparation::prepare(context, SubjectKind::Issue, input.version, recover)
    }
}
fn recover(context: &Transaction<'_>, issue: u64, event: Option<&Event>) -> Result<()> {
    let rows = context.query(
        "SELECT state FROM repository_issues WHERE number=?",
        &[integer(issue)?],
    )?;
    let row = rows
        .rows
        .first()
        .ok_or(Error::Invalid("issue lifecycle source is missing"))?;
    let closed = match result_u64_from_row(row, 0)? {
        0 => false,
        1 => true,
        _ => return Err(Error::Invalid("issue lifecycle state is invalid")),
    };
    let mut value = Lifecycle::default();
    if let Some(event) = event {
        match event.action {
            Action::Closed if closed => value.closed_at_ms = Some(event.created_at),
            Action::IssueClosed { reason } if closed => {
                value.state_reason = Some(reason);
                value.closed_at_ms = Some(event.created_at);
            }
            Action::Reopened if !closed => value.state_reason = Some(Reason::Reopened),
            _ => (),
        }
    }
    value.validate(closed)?;
    store(context, issue, &value)
}
