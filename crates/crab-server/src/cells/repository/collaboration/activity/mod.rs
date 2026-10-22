use serde::{Deserialize, Serialize};

use super::super::{
    Read, RepositoryAuthor, Transaction, timestamp, validate_author, validate_number,
    validate_title,
};
use super::{Page, Subject, Target};

const ROOT: &str = "collaboration/v1/activity";
const SEQUENCE: &str = "collaboration/v1/activity/sequence";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Action {
    Created,
    Drafted,
    ReadyForReview,
    ReviewRequested {
        reviewer: String,
    },
    ReviewRequestCancelled {
        reviewer: String,
    },
    Updated,
    Closed,
    IssueClosed {
        reason: crate::cells::repository::issues::lifecycle::Reason,
    },
    IssueCloseReasonChanged {
        from: Option<crate::cells::repository::issues::lifecycle::Reason>,
        to: crate::cells::repository::issues::lifecycle::Reason,
    },
    Reopened,
    Approved,
    ChangesRequested,
    Commented,
    Merged,
    Resolved,
    Unresolved,
    MilestoneAssigned {
        number: u64,
        title: String,
    },
    MilestoneRemoved {
        number: u64,
        title: String,
    },
    IssueLinked {
        number: u64,
        title: String,
    },
    IssueUnlinked {
        number: u64,
        title: String,
    },
}

impl Action {
    pub(super) fn related_subject(&self) -> Option<Subject> {
        match self {
            Self::IssueLinked { number, .. } | Self::IssueUnlinked { number, .. } => {
                Some(Subject {
                    kind: super::SubjectKind::Issue,
                    number: *number,
                })
            }
            _ => None,
        }
    }

    fn validate(&self, target: Target) -> crate::cells::Result<()> {
        let valid = match target {
            Target::Issue { .. } => matches!(
                self,
                Self::Created
                    | Self::Updated
                    | Self::Closed
                    | Self::Reopened
                    | Self::IssueClosed { .. }
                    | Self::IssueCloseReasonChanged { .. }
                    | Self::MilestoneAssigned { .. }
                    | Self::MilestoneRemoved { .. }
            ),
            Target::Pull { .. } => matches!(
                self,
                Self::Created
                    | Self::Updated
                    | Self::Closed
                    | Self::Reopened
                    | Self::Merged
                    | Self::Drafted
                    | Self::ReadyForReview
                    | Self::ReviewRequested { .. }
                    | Self::ReviewRequestCancelled { .. }
                    | Self::IssueLinked { .. }
                    | Self::IssueUnlinked { .. }
                    | Self::MilestoneAssigned { .. }
                    | Self::MilestoneRemoved { .. }
            ),
            Target::IssueComment { .. } | Target::PullComment { .. } | Target::PullReply { .. } => {
                matches!(self, Self::Created | Self::Updated)
            }
            Target::PullReview { .. } => matches!(
                self,
                Self::Approved | Self::ChangesRequested | Self::Commented | Self::Updated
            ),
            Target::PullThread { .. } => matches!(
                self,
                Self::Created | Self::Updated | Self::Resolved | Self::Unresolved
            ),
        };
        if let Self::MilestoneAssigned { number, title }
        | Self::MilestoneRemoved { number, title }
        | Self::IssueLinked { number, title }
        | Self::IssueUnlinked { number, title } = self
        {
            validate_number(*number)?;
            validate_title(title)?;
        }
        if let Self::ReviewRequested { reviewer } | Self::ReviewRequestCancelled { reviewer } = self
        {
            validate_title(reviewer)?;
        }
        if matches!(
            self,
            Self::IssueClosed {
                reason: super::super::issues::lifecycle::Reason::Reopened
            } | Self::IssueCloseReasonChanged {
                from: Some(super::super::issues::lifecycle::Reason::Reopened),
                ..
            } | Self::IssueCloseReasonChanged {
                to: super::super::issues::lifecycle::Reason::Reopened,
                ..
            }
        ) {
            return Err(crate::cells::Error::Invalid(
                "closure activity reason is invalid",
            ));
        }
        if !valid {
            return Err(crate::cells::Error::Invalid(
                "activity action does not match its target",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Event {
    pub number: u64,
    pub target: Target,
    pub title: String,
    pub actor: RepositoryAuthor,
    pub action: Action,
    pub created_at: u64,
}

fn events(subject: Option<Subject>) -> String {
    subject.map_or_else(
        || format!("{ROOT}/events/"),
        |subject| format!("{ROOT}/subjects/{}/events/", subject.key()),
    )
}

pub(in crate::cells::repository) fn append(
    context: &Transaction<'_>,
    target: Target,
    actor: &RepositoryAuthor,
    action: Action,
) -> crate::cells::Result<()> {
    validate_author(actor)?;
    action.validate(target)?;
    if !target.exists(context)? {
        return Err(crate::cells::Error::Invalid("activity target is missing"));
    }
    let subject = target.subject();
    let summary = subject
        .summary(context)?
        .ok_or(crate::cells::Error::Invalid("activity subject is missing"))?;
    let number = context
        .document::<u64>(SEQUENCE)?
        .map(|number| {
            validate_number(number)?;
            Ok::<_, crate::cells::Error>(number)
        })
        .transpose()?
        .unwrap_or(0);
    let number = super::next_version(number)?;
    let event = Event {
        number,
        target,
        title: summary.title,
        actor: actor.clone(),
        action,
        created_at: timestamp(context.now_ms())?,
    };
    // The caller's SQL write, event, indexes, and sequence share the native
    // command transaction. A rejected command cannot leave phantom activity.
    context.put_document(SEQUENCE, &number)?;
    context.put_document(&format!("{}{number:016x}", events(None)), &event)?;
    context.put_document(&format!("{}{number:016x}", events(Some(subject))), &true)?;
    if let Some(related) = event.action.related_subject() {
        if related.closed(context)?.is_none() {
            return Err(crate::cells::Error::Invalid(
                "related activity subject is missing",
            ));
        }
        // A relationship is one repository event visible from both subjects.
        context.put_document(&format!("{}{number:016x}", events(Some(related))), &true)?;
    }
    super::conversation::project_event(context, &event)?;
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Query {
    pub subject: Option<Subject>,
    pub before: Option<u64>,
    pub limit: u8,
}

pub(crate) struct ListActivity;
impl Read for ListActivity {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 220;
    type Input = Query;
    type Output = Option<Page<Event>>;

    fn execute(context: &mut Transaction<'_>, input: Query) -> crate::cells::Result<Self::Output> {
        if !(1..=50).contains(&input.limit) {
            return Err(crate::cells::Error::Invalid(
                "activity pages require 1–50 events",
            ));
        }
        if let Some(before) = input.before {
            validate_number(before)?;
        }
        if let Some(subject) = input.subject
            && subject.closed(context)?.is_none()
        {
            return Ok(None);
        }
        let prefix = events(input.subject);
        let before = input.before.map(|number| format!("{prefix}{number:016x}"));
        let limit = usize::from(input.limit);
        let mut items = if input.subject.is_some() {
            context
                .documents::<bool>(&prefix, before.as_deref(), limit + 1)?
                .into_iter()
                .map(|(key, marker)| {
                    if !marker {
                        return Err(crate::cells::Error::Invalid(
                            "activity index marker is invalid",
                        ));
                    }
                    let number = number(&key, &prefix)?;
                    let key = format!("{}{number:016x}", events(None));
                    let event = context
                        .document::<Event>(&key)?
                        .ok_or(crate::cells::Error::Invalid("activity index has no event"))?;
                    validate(event, number, input.subject)
                })
                .collect::<crate::cells::Result<Vec<_>>>()?
        } else {
            context
                .documents::<Event>(&prefix, before.as_deref(), limit + 1)?
                .into_iter()
                .map(|(key, event)| {
                    let number = number(&key, &prefix)?;
                    validate(event, number, None)
                })
                .collect::<crate::cells::Result<Vec<_>>>()?
        };
        let has_more = items.len() > limit;
        items.truncate(limit);
        let next = if has_more {
            items.last().map(|event| event.number)
        } else {
            None
        };
        Ok(Some(Page { items, next }))
    }
}

pub(super) fn event(
    context: &Transaction<'_>,
    number: u64,
    subject: Option<Subject>,
) -> crate::cells::Result<Option<Event>> {
    validate_number(number)?;
    context
        .document::<Event>(&format!("{}{number:016x}", events(None)))?
        .map(|event| validate(event, number, subject))
        .transpose()
}

fn number(key: &str, prefix: &str) -> crate::cells::Result<u64> {
    let suffix = key
        .strip_prefix(prefix)
        .filter(|suffix| suffix.len() == 16)
        .ok_or(crate::cells::Error::Invalid("activity key is invalid"))?;
    let number = u64::from_str_radix(suffix, 16)
        .map_err(|_| crate::cells::Error::Invalid("activity key is invalid"))?;
    validate_number(number)?;
    Ok(number)
}

fn validate(event: Event, number: u64, subject: Option<Subject>) -> crate::cells::Result<Event> {
    if event.number != number
        || subject.is_some_and(|subject| {
            event.target.subject() != subject && event.action.related_subject() != Some(subject)
        })
    {
        return Err(crate::cells::Error::Invalid(
            "activity event does not match its index",
        ));
    }
    validate_author(&event.actor)?;
    validate_title(&event.title)?;
    if event.created_at > super::super::MAX_NUMBER {
        return Err(crate::cells::Error::Invalid(
            "activity timestamp is invalid",
        ));
    }
    event.target.validate()?;
    event.action.validate(event.target)?;
    Ok(event)
}

#[cfg(test)]
mod tests;
