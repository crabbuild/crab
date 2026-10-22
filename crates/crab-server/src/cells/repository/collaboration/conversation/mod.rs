//! Ordered discussion history projected from canonical contributions and events.

use super::{
    Subject, SubjectKind, Target,
    activity::{self, Action, Event},
};
use crate::cells::{
    Error, Result,
    application::{Read, SqlValue, Transaction},
    repository::{
        self, CommentKey, CommentRecord, GetComment, GetPullComment, GetPullReview, PullChildKey,
        PullCommentRecord, PullReviewRecord, RepositoryAuthor, integer, result_u64_from_row,
        validate_number,
    },
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

mod read;
mod rebuild;
pub(crate) use read::{ListConversation, Position, Query};
pub(in crate::cells::repository) use rebuild::{reset, step};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Source {
    Comment,
    Review,
    Activity,
}
impl Source {
    const fn code(self) -> i64 {
        match self {
            Self::Comment => 0,
            Self::Review => 1,
            Self::Activity => 2,
        }
    }
    fn from_code(code: u64) -> Result<Self> {
        match code {
            0 => Ok(Self::Comment),
            1 => Ok(Self::Review),
            2 => Ok(Self::Activity),
            _ => Err(Error::Invalid("conversation source is invalid")),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Key {
    pub source: Source,
    pub number: u64,
}
impl Key {
    fn validate(self, subject: Subject) -> Result<()> {
        validate_number(self.number)?;
        if self.source == Source::Review && subject.kind != SubjectKind::Pull {
            return Err(Error::Invalid(
                "issue conversations cannot contain pull reviews",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Cursor {
    pub subject: Subject,
    pub generation: Uuid,
    key: Key,
    created_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum Entry {
    IssueComment(CommentRecord),
    PullComment(PullCommentRecord),
    Review(PullReviewRecord),
    Activity(Event),
}
impl Entry {
    pub(crate) fn author(&self) -> &RepositoryAuthor {
        match self {
            Self::IssueComment(c) => &c.author,
            Self::PullComment(c) => &c.author,
            Self::Review(r) => &r.author,
            Self::Activity(e) => &e.actor,
        }
    }
    pub(crate) fn target(&self) -> Target {
        match self {
            Self::IssueComment(c) => Target::IssueComment {
                issue: c.issue,
                number: c.number,
            },
            Self::PullComment(c) => Target::PullComment {
                pull: c.pull,
                number: c.number,
            },
            Self::Review(r) => Target::PullReview {
                pull: r.pull,
                number: r.number,
            },
            Self::Activity(e) => e.target,
        }
    }
    fn key(&self) -> Key {
        match self {
            Self::IssueComment(c) => Key {
                source: Source::Comment,
                number: c.number,
            },
            Self::PullComment(c) => Key {
                source: Source::Comment,
                number: c.number,
            },
            Self::Review(r) => Key {
                source: Source::Review,
                number: r.number,
            },
            Self::Activity(e) => Key {
                source: Source::Activity,
                number: e.number,
            },
        }
    }
    fn created_at_ms(&self) -> u64 {
        match self {
            Self::IssueComment(c) => c.created_at_ms,
            Self::PullComment(c) => c.created_at_ms,
            Self::Review(r) => r.created_at_ms,
            Self::Activity(e) => e.created_at,
        }
    }
}

pub(in crate::cells::repository) fn install(context: &Transaction<'_>) -> Result<()> {
    context.install_schema(include_str!("schema.sql"))
}
fn kind(subject: Subject) -> i64 {
    i64::from(subject.kind == SubjectKind::Pull)
}
fn insert(context: &Transaction<'_>, subject: Subject, key: Key, created_at_ms: u64) -> Result<()> {
    validate_number(subject.number)?;
    key.validate(subject)?;
    if created_at_ms > repository::MAX_NUMBER {
        return Err(Error::Invalid("conversation timestamp is invalid"));
    }
    context.query("INSERT INTO discussion_conversation(kind, discussion, source, number, created_at_ms) VALUES (?, ?, ?, ?, ?)", &[SqlValue::Integer(kind(subject)),integer(subject.number)?,SqlValue::Integer(key.source.code()),integer(key.number)?,integer(created_at_ms)?])?;
    Ok(())
}

pub(super) fn project_event(context: &Transaction<'_>, event: &Event) -> Result<()> {
    // Contributions already own one editable card. Their activity snapshots and
    // the opening event must not duplicate that card or the root description.
    if matches!(
        event.target,
        Target::IssueComment { .. } | Target::PullComment { .. } | Target::PullReview { .. }
    ) || event.action == Action::Created
        && matches!(event.target, Target::Issue { .. } | Target::Pull { .. })
    {
        return Ok(());
    }
    let subject = event.target.subject();
    let key = Key {
        source: Source::Activity,
        number: event.number,
    };
    if subject.closed(context)?.is_some() {
        insert(context, subject, key, event.created_at)?;
    }
    if let Some(related) = event.action.related_subject()
        && related.closed(context)?.is_some()
    {
        insert(context, related, key, event.created_at)?;
    }
    Ok(())
}

fn load(context: &mut Transaction<'_>, subject: Subject, key: Key) -> Result<Entry> {
    let entry = match (subject.kind, key.source) {
        (SubjectKind::Issue, Source::Comment) => GetComment::execute(
            context,
            CommentKey {
                issue: subject.number,
                number: key.number,
            },
        )?
        .map(Entry::IssueComment),
        (SubjectKind::Pull, Source::Comment) => GetPullComment::execute(
            context,
            PullChildKey {
                pull: subject.number,
                number: key.number,
            },
        )?
        .map(Entry::PullComment),
        (SubjectKind::Pull, Source::Review) => GetPullReview::execute(
            context,
            PullChildKey {
                pull: subject.number,
                number: key.number,
            },
        )?
        .map(Entry::Review),
        (_, Source::Activity) => {
            activity::event(context, key.number, Some(subject))?.map(Entry::Activity)
        }
        _ => None,
    };
    entry.ok_or(Error::Invalid(
        "conversation index has no canonical contribution",
    ))
}

#[cfg(test)]
pub(in crate::cells::repository) mod tests;
