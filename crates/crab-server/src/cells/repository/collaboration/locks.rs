use serde::{Deserialize, Serialize};

use super::{
    Subject, SubjectKind, Target,
    activity::{self, Action},
};
use crate::cells::{
    Error, Result,
    application::{Mutation, MutationResult, Read, Transaction},
    repository::{
        CLASS, MAX_NUMBER, RepositoryAuthor, advance_revision, settings, validate_author,
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

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Lock {
    pub version: u64,
    pub locked: bool,
    pub reason: Option<Reason>,
}

fn key(subject: Subject) -> String {
    format!("collaboration/v1/locks/{}", subject.key())
}

fn load(context: &Transaction<'_>, subject: Subject) -> Result<Lock> {
    Ok(context.document(&key(subject))?.unwrap_or_default())
}

pub(in crate::cells::repository) fn blocked(
    context: &Transaction<'_>,
    target: Target,
    can_post_locked: bool,
) -> Result<bool> {
    Ok(!can_post_locked && load(context, target.subject())?.locked)
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
            load(context, subject)?,
            settings::archived(context)?,
        )))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Change {
    Lock { reason: Option<Reason> },
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
        let (locked, reason) = match input.change {
            Change::Lock { reason } => (true, reason),
            Change::Unlock => (false, None),
        };
        let archived = settings::archived(context)?;
        // A repeated conditional change can acknowledge the same published state.
        if current.locked == locked
            && current.reason == reason
            && (input.version == current.version
                || input.version.checked_add(1) == Some(current.version))
        {
            return Ok(MutationResult::Success(Outcome::Saved(current, archived)));
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
        let saved = Lock {
            version,
            locked,
            reason,
        };
        context.put_document(&key(input.subject), &saved)?;
        let target = match input.subject.kind {
            SubjectKind::Issue => Target::Issue {
                number: input.subject.number,
            },
            SubjectKind::Pull => Target::Pull {
                number: input.subject.number,
            },
        };
        let action = if locked {
            Action::Locked { reason }
        } else {
            Action::Unlocked
        };
        activity::append(context, target, &input.actor, action)?;
        advance_revision(context)?;
        Ok(MutationResult::Success(Outcome::Saved(saved, false)))
    }
}
