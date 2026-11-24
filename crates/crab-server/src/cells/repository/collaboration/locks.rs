use serde::{Deserialize, Serialize};

use super::{
    Subject, SubjectKind, Target,
    activity::{self, Action},
};
use crate::cells::{
    Error, Result,
    application::{Mutation, MutationResult, Read, Transaction},
    repository::{
        CLASS, MAX_NUMBER, RepositoryAuthor, advance_revision, search::index, settings, timestamp,
        validate_author, validate_number,
    },
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Reason {
    OffTopic,
    TooHeated,
    Resolved,
    Spam,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Duration {
    Hour,
    Day,
    Week,
}

impl Duration {
    fn millis(self) -> u64 {
        3_600_000
            * match self {
                Self::Hour => 1,
                Self::Day => 24,
                Self::Week => 168,
            }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Deadline {
    duration: Duration,
    at: u64,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Lock {
    pub version: u64,
    pub locked: bool,
    pub reason: Option<Reason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deadline: Option<Deadline>,
}

impl Lock {
    pub(crate) fn expires_at_ms(self) -> Option<u64> {
        self.deadline.map(|deadline| deadline.at)
    }

    fn effective_at(self, now_ms: u64) -> Self {
        if self
            .expires_at_ms()
            .is_some_and(|deadline| deadline <= now_ms)
        {
            return Self {
                version: self.version,
                ..Self::default()
            };
        }
        self
    }
}

const ROOT: &str = "collaboration/v1/locks/";

fn key(subject: Subject) -> String {
    format!("{ROOT}{}", subject.key())
}

pub(in crate::cells::repository) fn page(
    context: &Transaction<'_>,
    kind: SubjectKind,
    after: u64,
    limit: usize,
) -> Result<Vec<(Subject, Lock)>> {
    let namespace = match kind {
        SubjectKind::Issue => "issue",
        SubjectKind::Pull => "pull",
    };
    let prefix = format!("{ROOT}{namespace}/");
    let after = (after != 0).then(|| {
        key(Subject {
            kind,
            number: after,
        })
    });
    context
        .documents_after::<Lock>(&prefix, after.as_deref(), limit)?
        .into_iter()
        .map(|(path, lock)| {
            let number = path
                .strip_prefix(&prefix)
                .filter(|number| number.len() == 16)
                .and_then(|number| u64::from_str_radix(number, 16).ok())
                .ok_or(Error::Invalid("conversation lock address is invalid"))?;
            validate_number(number)?;
            Ok((Subject { kind, number }, lock))
        })
        .collect()
}

pub(in crate::cells::repository) fn load(
    context: &Transaction<'_>,
    subject: Subject,
) -> Result<Lock> {
    Ok(context.document(&key(subject))?.unwrap_or_default())
}

pub(in crate::cells::repository) fn blocked(
    context: &Transaction<'_>,
    target: Target,
    can_post_locked: bool,
) -> Result<bool> {
    Ok(!can_post_locked
        && load(context, target.subject())?
            .effective_at(timestamp(context.now_ms())?)
            .locked)
}

pub(crate) struct Get;
impl Read for Get {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 311;
    type Input = Subject;
    type Output = Option<(Lock, bool)>;

    fn execute(context: &mut Transaction<'_>, subject: Subject) -> Result<Self::Output> {
        if subject.closed(context)?.is_none() {
            return Ok(None);
        }
        Ok(Some((
            load(context, subject)?.effective_at(timestamp(context.now_ms())?),
            settings::archived(context)?,
        )))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Change {
    Lock {
        reason: Option<Reason>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration: Option<Duration>,
    },
    Unlock,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    pub subject: Subject,
    pub actor: RepositoryAuthor,
    pub version: u64,
    pub change: Change,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) enum Outcome {
    Saved(Lock, bool),
    Archived,
    NotFound,
    Conflict,
}

pub(crate) struct Set;
impl Mutation for Set {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 311;
    type Input = Input;
    type Output = Outcome;

    fn execute(context: &mut Transaction<'_>, input: Input) -> Result<MutationResult<Outcome>> {
        validate_author(&input.actor)?;
        if input.subject.closed(context)?.is_none() {
            return Ok(MutationResult::Rejected(Outcome::NotFound));
        }
        let current = load(context, input.subject)?;
        let now_ms = timestamp(context.now_ms())?;
        let effective = current.effective_at(now_ms);
        let (locked, reason, duration) = match input.change {
            Change::Lock { reason, duration } => (true, reason, duration),
            Change::Unlock => (false, None, None),
        };
        let matches = |lock: Lock| {
            lock.locked == locked
                && lock.reason == reason
                && lock.deadline.map(|deadline| deadline.duration) == duration
        };
        let archived = settings::archived(context)?;
        // Replay acknowledges the original deadline even after it expired. A fresh
        // request at the current version can lock the conversation again.
        if (input.version == current.version && matches(effective))
            || (input.version.checked_add(1) == Some(current.version) && matches(current))
        {
            return Ok(MutationResult::Success(Outcome::Saved(effective, archived)));
        }
        if archived {
            return Ok(MutationResult::Rejected(Outcome::Archived));
        }
        if input.version != current.version {
            return Ok(MutationResult::Rejected(Outcome::Conflict));
        }
        let version = current
            .version
            .checked_add(1)
            .filter(|version| *version < MAX_NUMBER)
            .ok_or(Error::Invalid("conversation lock version is exhausted"))?;
        let deadline = duration
            .map(|duration| {
                let at = now_ms
                    .checked_add(duration.millis())
                    .filter(|at| *at <= MAX_NUMBER)
                    .ok_or(Error::Invalid(
                        "conversation lock deadline is outside its range",
                    ))?;
                Ok::<_, Error>(Deadline { duration, at })
            })
            .transpose()?;
        let saved = Lock {
            version,
            locked,
            reason,
            deadline,
        };
        context.put_document(&key(input.subject), &saved)?;
        index::locks::project(context, input.subject, saved)?;
        index::invalidate_cursors(context)?;
        let target = match input.subject.kind {
            SubjectKind::Issue => Target::Issue {
                number: input.subject.number,
            },
            SubjectKind::Pull => Target::Pull {
                number: input.subject.number,
            },
        };
        let action = if locked {
            Action::Locked {
                reason,
                expires_at_ms: saved.expires_at_ms(),
            }
        } else {
            Action::Unlocked
        };
        activity::append(context, target, &input.actor, action)?;
        advance_revision(context)?;
        Ok(MutationResult::Success(Outcome::Saved(saved, false)))
    }
}
