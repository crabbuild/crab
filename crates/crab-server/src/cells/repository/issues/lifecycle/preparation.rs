use super::*;
use crate::cells::application::{Mutation, MutationResult, Read};
use crate::cells::repository::{
    advance_revision,
    collaboration::{
        Subject, SubjectKind, Target,
        activity::{Action, ListActivity, Query},
    },
};

const KEY: &str = "repository/issues/lifecycle/v1/preparation";
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
/// Readiness of canonical issue closure metadata.
pub enum IssueLifecycleStatus {
    Outdated,
    Migrating { version: u64, copied: u64 },
    Ready,
}
fn stored(context: &Transaction<'_>) -> Result<Option<Stored>> {
    let value = context.document::<Stored>(KEY)?;
    if value.as_ref().is_some_and(|value| value.format != 1) {
        return Err(Error::Invalid("unsupported issue lifecycle format"));
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
            return Err(Error::Invalid(
                "invalid issue lifecycle preparation progress",
            ));
        }
    }
    Ok(value)
}
fn state(context: &Transaction<'_>) -> Result<IssueLifecycleStatus> {
    Ok(match stored(context)? {
        None => IssueLifecycleStatus::Outdated,
        Some(Stored { progress: None, .. }) => IssueLifecycleStatus::Ready,
        Some(Stored {
            progress: Some(p), ..
        }) => IssueLifecycleStatus::Migrating {
            version: p.version,
            copied: p.copied,
        },
    })
}
pub(in crate::cells::repository) fn initialize(context: &Transaction<'_>) -> Result<()> {
    context.install_schema(include_str!("schema.sql"))?;
    context.put_document(
        KEY,
        &Stored {
            format: 1,
            progress: None,
        },
    )
}
pub(in crate::cells::repository) fn ensure_ready(context: &Transaction<'_>) -> Result<()> {
    match state(context)? {
        IssueLifecycleStatus::Ready => Ok(()),
        _ => Err(Error::SchemaUnavailable),
    }
}
pub(crate) struct GetIssueLifecycleStatus;
impl Read for GetIssueLifecycleStatus {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 290;
    type Input = ();
    type Output = IssueLifecycleStatus;
    fn execute(context: &mut Transaction<'_>, (): ()) -> Result<Self::Output> {
        state(context)
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrepareIssueLifecycleInput {
    pub version: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum PrepareIssueLifecycleOutcome {
    Applied(IssueLifecycleStatus),
    Conflict(IssueLifecycleStatus),
}
pub(crate) struct PrepareIssueLifecycle;
impl Mutation for PrepareIssueLifecycle {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 290;
    type Input = PrepareIssueLifecycleInput;
    type Output = PrepareIssueLifecycleOutcome;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> Result<MutationResult<Self::Output>> {
        let old = stored(context)?;
        if old.as_ref().is_some_and(|value| value.progress.is_none()) {
            return Ok(MutationResult::Success(
                PrepareIssueLifecycleOutcome::Applied(IssueLifecycleStatus::Ready),
            ));
        }
        let mut p = old
            .as_ref()
            .and_then(|value| value.progress.clone())
            .unwrap_or_default();
        if p.version != input.version {
            return Ok(MutationResult::Rejected(
                PrepareIssueLifecycleOutcome::Conflict(state(context)?),
            ));
        }
        if old.is_none() {
            context.install_schema(include_str!("schema.sql"))?;
        }
        for _ in 0..FRAME {
            let issue = if let Some(issue) = p.current {
                Some(issue)
            } else {
                context.query("SELECT number FROM repository_issues WHERE number>? ORDER BY number LIMIT 1",&[integer(p.after)?])?.rows.first().map(|row|result_u64_from_row(row,0)).transpose()?
            };
            let Some(issue) = issue else {
                context.put_document(
                    KEY,
                    &Stored {
                        format: 1,
                        progress: None,
                    },
                )?;
                advance_revision(context)?;
                return Ok(MutationResult::Success(
                    PrepareIssueLifecycleOutcome::Applied(IssueLifecycleStatus::Ready),
                ));
            };
            p.current = Some(issue);
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
                _ => return Err(Error::Invalid("issue lifecycle source state is invalid")),
            };
            let page = ListActivity::execute(
                context,
                Query {
                    subject: Some(Subject {
                        kind: SubjectKind::Issue,
                        number: issue,
                    }),
                    before: p.before,
                    limit: 1,
                },
            )?
            .ok_or(Error::Invalid("issue activity source is missing"))?;
            let event = page.items.into_iter().find(|event| {
                event.target == Target::Issue { number: issue }
                    && matches!(
                        event.action,
                        Action::Closed | Action::Reopened | Action::IssueClosed { .. }
                    )
            });
            let mut value = Lifecycle::default();
            if let Some(event) = &event {
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
            if event.is_none() && page.next.is_some() {
                p.before = page.next;
                continue;
            }
            // Historical records can lack closure history. Preserve unknown
            // metadata rather than turn an edit time into a fabricated close.
            value.validate(closed)?;
            store(context, issue, &value)?;
            p.after = issue;
            p.current = None;
            p.before = None;
            p.copied = p
                .copied
                .checked_add(1)
                .ok_or(Error::Invalid("issue preparation count limit reached"))?;
        }
        p.version = p
            .version
            .checked_add(1)
            .filter(|v| *v <= crate::cells::repository::MAX_NUMBER)
            .ok_or(Error::Invalid("issue preparation version limit reached"))?;
        context.put_document(
            KEY,
            &Stored {
                format: 1,
                progress: Some(p),
            },
        )?;
        advance_revision(context)?;
        Ok(MutationResult::Success(
            PrepareIssueLifecycleOutcome::Applied(state(context)?),
        ))
    }
}
